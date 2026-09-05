//! A second process that takes locks on a database file on command.
//!
//! Invariant: the probe holds exactly the locks it has been told to hold and
//! reports the outcome of every request, so a test can assert on cross-process
//! conflicts rather than on timing.
//!
//! Same-process lock tests prove almost nothing on POSIX, where advisory locks
//! are held per process and a second handle in the same process is invisible to
//! the kernel. The only way to show that two *processes* exclude each other is
//! to start one, which is what this binary is for. It speaks a line protocol on
//! stdin and stdout so the parent never has to guess how long to wait:
//!
//! ```text
//! open <path>          -> ok | error <detail>
//! lock shared          -> ok | busy | error <detail>
//! lock reserved        -> ok | busy | error <detail>
//! lock pending         -> ok | busy | error <detail>
//! lock exclusive       -> ok | busy | error <detail>
//! unlock shared        -> ok | error <detail>
//! unlock none          -> ok | error <detail>
//! check-reserved       -> true | false | error <detail>
//! write <offset> <hex> -> ok | error <detail>
//! read <offset> <len>  -> hex <bytes> | error <detail>
//! shm-open             -> ok | error <detail>
//! shm-map              -> ok | error <detail>
//! shm-write <off> <hex>-> ok | error <detail>
//! shm-read <off> <len> -> hex <bytes> | error <detail>
//! close                -> ok
//! exit                 -> (no reply)
//! ```

use std::io::{BufRead, Write};

use std::sync::Arc;

use inillucent_vfs::contract::{FileLock, OpenOptions, SharedMemory, ShmRegion, Vfs, VfsFile};
use inillucent_vfs::error::VfsError;
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;

/// Everything the probe has open, so one command can close all of it.
#[derive(Default)]
struct Held {
    file: Option<Box<dyn VfsFile>>,
    shm: Option<Arc<dyn SharedMemory>>,
    region: Option<Arc<dyn ShmRegion>>,
}

/// The size of one wal-index region, which is what `shm-map` maps.
const REGION_SIZE: usize = 32_768;

/// Reads commands until stdin closes or `exit` arrives.
fn main() {
    let vfs = OsVfs::new();
    let mut held = Held::default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let command = line.trim();
        if command == "exit" {
            break;
        }
        let reply = dispatch(&vfs, &mut held, command);
        if writeln!(stdout, "{reply}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

/// Runs one command and returns the line to reply with.
fn dispatch(vfs: &OsVfs, held: &mut Held, command: &str) -> String {
    let file = &mut held.file;
    let mut parts = command.split_whitespace();
    match parts.next() {
        Some("open") => open(vfs, file, parts.next().unwrap_or_default()),
        Some("shm-open") => shm_open(&held.file, &mut held.shm),
        Some("shm-map") => shm_map(&mut held.shm, &mut held.region),
        Some("shm-write") => shm_write(&held.region, parts.next(), parts.next()),
        Some("shm-read") => shm_read(&held.region, parts.next(), parts.next()),
        Some("lock") => with_file(file, |handle| {
            level(parts.next().unwrap_or_default()).map(|level| handle.lock(level))
        }),
        Some("unlock") => with_file(file, |handle| {
            level(parts.next().unwrap_or_default()).map(|level| handle.unlock(level))
        }),
        Some("check-reserved") => match file.as_ref() {
            Some(handle) => match handle.check_reserved_lock() {
                Ok(held) => held.to_string(),
                Err(error) => describe(&error),
            },
            None => "error no file is open".to_string(),
        },
        Some("write") => write_bytes(file, parts.next(), parts.next()),
        Some("read") => read_bytes(file, parts.next(), parts.next()),
        Some("close") => {
            held.region = None;
            held.shm = None;
            held.file = None;
            "ok".to_string()
        }
        _ => format!("error unknown command {command}"),
    }
}

/// Opens the database file the rest of the commands act on.
fn open(vfs: &OsVfs, file: &mut Option<Box<dyn VfsFile>>, path: &str) -> String {
    if path.is_empty() {
        return "error open needs a path".to_string();
    }
    match vfs.open(&DbPath::from(path), OpenOptions::main_db()) {
        Ok(handle) => {
            *file = Some(handle);
            "ok".to_string()
        }
        Err(error) => describe(&error),
    }
}

/// Opens the shared memory beside the file the probe has open.
fn shm_open(file: &Option<Box<dyn VfsFile>>, shm: &mut Option<Arc<dyn SharedMemory>>) -> String {
    let Some(handle) = file.as_ref() else {
        return "error no file is open".to_string();
    };
    match handle.shared_memory() {
        Ok(Some(memory)) => {
            *shm = Some(memory);
            "ok".to_string()
        }
        Ok(None) => "error this file has no shared memory".to_string(),
        Err(error) => describe(&error),
    }
}

/// Opens the shared memory beside the open file and maps its first region.
fn shm_map(
    shm: &mut Option<Arc<dyn SharedMemory>>,
    region: &mut Option<Arc<dyn ShmRegion>>,
) -> String {
    let Some(memory) = shm.clone() else {
        return "error shm-map needs shm-open first".to_string();
    };
    match memory.map(0, REGION_SIZE, true) {
        Ok(Some(mapped)) => {
            *region = Some(mapped);
            "ok".to_string()
        }
        Ok(None) => "error region 0 does not exist".to_string(),
        Err(error) => describe(&error),
    }
}

/// Writes hex-encoded bytes into the mapped region.
fn shm_write(
    region: &Option<Arc<dyn ShmRegion>>,
    offset: Option<&str>,
    hex: Option<&str>,
) -> String {
    let Some(region) = region.as_ref() else {
        return "error no region is mapped".to_string();
    };
    let (Some(offset), Some(hex)) = (offset, hex) else {
        return "error shm-write needs an offset and hex bytes".to_string();
    };
    let Ok(offset) = offset.parse::<usize>() else {
        return "error shm-write offset is not a number".to_string();
    };
    let Some(bytes) = decode_hex(hex) else {
        return "error shm-write payload is not hex".to_string();
    };
    match region.write(offset, &bytes) {
        Ok(()) => "ok".to_string(),
        Err(error) => describe(&error),
    }
}

/// Reads bytes out of the mapped region and returns them hex-encoded.
fn shm_read(
    region: &Option<Arc<dyn ShmRegion>>,
    offset: Option<&str>,
    len: Option<&str>,
) -> String {
    let Some(region) = region.as_ref() else {
        return "error no region is mapped".to_string();
    };
    let (Some(offset), Some(len)) = (offset, len) else {
        return "error shm-read needs an offset and a length".to_string();
    };
    let (Ok(offset), Ok(len)) = (offset.parse::<usize>(), len.parse::<usize>()) else {
        return "error shm-read arguments are not numbers".to_string();
    };
    let mut buffer = vec![0u8; len];
    match region.read(offset, &mut buffer) {
        Ok(()) => format!("hex {}", encode_hex(&buffer)),
        Err(error) => describe(&error),
    }
}

/// Applies a lock or unlock to the open file and formats the outcome.
fn with_file<F>(file: &mut Option<Box<dyn VfsFile>>, action: F) -> String
where
    F: FnOnce(&dyn VfsFile) -> Option<Result<(), VfsError>>,
{
    let Some(handle) = file.as_ref() else {
        return "error no file is open".to_string();
    };
    match action(handle.as_ref()) {
        None => "error unknown lock level".to_string(),
        Some(Ok(())) => "ok".to_string(),
        Some(Err(error)) if error.code() == inillucent_base::error::PrimaryCode::Busy => {
            "busy".to_string()
        }
        Some(Err(error)) => describe(&error),
    }
}

/// Writes hex-encoded bytes at an offset.
fn write_bytes(
    file: &mut Option<Box<dyn VfsFile>>,
    offset: Option<&str>,
    hex: Option<&str>,
) -> String {
    let Some(handle) = file.as_ref() else {
        return "error no file is open".to_string();
    };
    let (Some(offset), Some(hex)) = (offset, hex) else {
        return "error write needs an offset and hex bytes".to_string();
    };
    let Ok(offset) = offset.parse::<u64>() else {
        return "error write offset is not a number".to_string();
    };
    let Some(bytes) = decode_hex(hex) else {
        return "error write payload is not hex".to_string();
    };
    match handle.write_all_at(offset, &bytes) {
        Ok(()) => "ok".to_string(),
        Err(error) => describe(&error),
    }
}

/// Reads bytes at an offset and returns them hex-encoded.
fn read_bytes(
    file: &mut Option<Box<dyn VfsFile>>,
    offset: Option<&str>,
    len: Option<&str>,
) -> String {
    let Some(handle) = file.as_ref() else {
        return "error no file is open".to_string();
    };
    let (Some(offset), Some(len)) = (offset, len) else {
        return "error read needs an offset and a length".to_string();
    };
    let (Ok(offset), Ok(len)) = (offset.parse::<u64>(), len.parse::<usize>()) else {
        return "error read arguments are not numbers".to_string();
    };
    let mut buffer = vec![0u8; len];
    match handle.read_exact_at(offset, &mut buffer) {
        Ok(()) => format!("hex {}", encode_hex(&buffer)),
        Err(error) => describe(&error),
    }
}

/// Parses a lock level name.
fn level(name: &str) -> Option<FileLock> {
    match name {
        "none" => Some(FileLock::None),
        "shared" => Some(FileLock::Shared),
        "reserved" => Some(FileLock::Reserved),
        "pending" => Some(FileLock::Pending),
        "exclusive" => Some(FileLock::Exclusive),
        _ => None,
    }
}

/// Formats an error as a reply line.
fn describe(error: &VfsError) -> String {
    format!("error {} {}", error.extended().value(), error.detail())
}

/// Decodes a hex string into bytes.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let pair = std::str::from_utf8(bytes.get(index..index + 2)?).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
        index += 2;
    }
    Some(out)
}

/// Encodes bytes as a lowercase hex string.
fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
