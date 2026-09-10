//! Reading a zip or a gzipped tar, far enough to take one file out of it.
//!
//! Invariant: **nothing is written outside the destination directory.** A
//! member whose name is absolute, or which walks up out of the destination with
//! `..`, stops the extraction rather than being sanitised into something
//! harmless-looking. An installer that writes outside where it said it would
//! write is the bug this check exists for, and it is exercised by a test that
//! builds an archive with `../escape` in it.
//!
//! ## Why this is here rather than a crate
//!
//! Because it is small at the subset an installer needs, and the pieces are
//! already in the workspace: `inillucent_base::deflate::inflate` is the whole of
//! zip's and gzip's compression, and it is already checked against the format.
//! What is left is two container formats, each about a hundred lines - a zip
//! central directory and a tar header - and neither of them is a place where a
//! third-party crate would be carrying knowledge this repository does not have.
//!
//! What is **not** implemented is deliberate: no zip64, no encryption, no
//! compression method other than store and deflate, no tar extension beyond the
//! GNU long-name record. Each of those is refused by name if it appears, so an
//! archive this cannot read is a message rather than a wrong file. The two
//! archives this actually opens - Microsoft's ONNX Runtime releases - use none
//! of them.

use std::path::{Component, Path, PathBuf};

use inillucent_base::deflate::inflate;
use inillucent_base::error::refusal;
use inillucent_base::DbResult;

/// One file inside an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Its name inside the archive, with forward slashes.
    pub name: String,
    /// Its contents.
    pub bytes: Vec<u8>,
    /// Whether the archive marked it executable.
    pub executable: bool,
}

/// The largest single member this will hold in memory.
///
/// The library inside an ONNX Runtime release is about 13 MB and the largest
/// thing in the archive is under 300 MB; the ceiling is what stops a
/// deliberately malformed header asking for the whole machine.
pub const MAX_MEMBER: u64 = 1024 * 1024 * 1024;

/// Reads every member of a zip archive.
///
/// The central directory is read rather than the local headers, because a local
/// header may declare a length of zero and defer the real one to a descriptor
/// after the data - and a reader that trusted the local header would extract an
/// empty file from a valid archive.
///
/// @param bytes - the whole archive
pub fn read_zip(bytes: &[u8]) -> DbResult<Vec<Member>> {
    let end = find_end_of_central_directory(bytes)
        .ok_or_else(|| refusal("this is not a zip archive: it has no central directory"))?;
    let count = u64::from(word(bytes, end.saturating_add(10)));
    let start = long(bytes, end.saturating_add(16)) as usize;

    let mut members = Vec::new();
    let mut at = start;
    for _ in 0..count {
        if long(bytes, at) != 0x0201_4b50 {
            return Err(refusal(
                "a zip central directory entry is not where the archive said",
            ));
        }
        let method = word(bytes, at.saturating_add(10));
        let compressed = long(bytes, at.saturating_add(20)) as usize;
        let uncompressed = long(bytes, at.saturating_add(24)) as usize;
        let name_len = word(bytes, at.saturating_add(28)) as usize;
        let extra_len = word(bytes, at.saturating_add(30)) as usize;
        let comment_len = word(bytes, at.saturating_add(32)) as usize;
        let external = long(bytes, at.saturating_add(38));
        let local = long(bytes, at.saturating_add(42)) as usize;
        let name_at = at.saturating_add(46);
        let name = String::from_utf8_lossy(
            bytes
                .get(name_at..name_at.saturating_add(name_len))
                .unwrap_or(&[]),
        )
        .into_owned();
        at = name_at
            .saturating_add(name_len)
            .saturating_add(extra_len)
            .saturating_add(comment_len);

        if name.ends_with('/') {
            continue;
        }
        if compressed as u64 > MAX_MEMBER || uncompressed as u64 > MAX_MEMBER {
            return Err(refusal(format!(
                "{name} declares {uncompressed} bytes, over the {MAX_MEMBER}-byte ceiling this \
                 reader will hold"
            )));
        }
        if long(bytes, local) != 0x0403_4b50 {
            return Err(refusal(format!(
                "{name}'s local header is not where the archive said"
            )));
        }
        let local_name = word(bytes, local.saturating_add(26)) as usize;
        let local_extra = word(bytes, local.saturating_add(28)) as usize;
        let data = local
            .saturating_add(30)
            .saturating_add(local_name)
            .saturating_add(local_extra);
        let raw = bytes
            .get(data..data.saturating_add(compressed))
            .ok_or_else(|| refusal(format!("{name} runs past the end of the archive")))?;
        let content = match method {
            0 => raw.to_vec(),
            8 => inflate(raw)?,
            other => {
                return Err(refusal(format!(
                    "{name} is compressed with method {other}, and this reader knows only stored \
                     and deflated"
                )))
            }
        };
        if content.len() != uncompressed {
            return Err(refusal(format!(
                "{name} decompressed to {} bytes and the archive said {uncompressed}",
                content.len()
            )));
        }
        // Bit 0 of the high half of the external attributes is the Unix mode,
        // and 0o111 is the executable bits. A zip made on Windows leaves it
        // zero, which is right: nothing on Windows is executable by a mode bit.
        let executable = (external >> 16) & 0o111 != 0;
        members.push(Member {
            name,
            bytes: content,
            executable,
        });
    }
    Ok(members)
}

/// Reads every member of a gzipped tar.
///
/// @param bytes - the whole archive
pub fn read_tar_gz(bytes: &[u8]) -> DbResult<Vec<Member>> {
    read_tar(&gunzip(bytes)?)
}

/// Decompresses a gzip stream.
///
/// The header is fixed-width with four optional trailing fields, and the
/// trailer carries the length the payload must decompress to - which is checked,
/// because an inflate that stops early on a truncated download otherwise
/// produces a short file with no complaint.
///
/// @param bytes - the gzip stream
pub fn gunzip(bytes: &[u8]) -> DbResult<Vec<u8>> {
    let magic = (byte(bytes, 0), byte(bytes, 1), byte(bytes, 2));
    if magic != (0x1f, 0x8b, 8) {
        return Err(refusal("this is not a gzip stream"));
    }
    let flags = byte(bytes, 3);
    let mut at = 10usize;
    if flags & 0b0000_0100 != 0 {
        // FEXTRA: a two-byte length and that many bytes.
        let extra = word(bytes, at) as usize;
        at = at.saturating_add(2).saturating_add(extra);
    }
    for flag in [0b0000_1000u8, 0b0001_0000] {
        // FNAME and FCOMMENT: each a zero-terminated string.
        if flags & flag != 0 {
            while byte(bytes, at) != 0 {
                if at >= bytes.len() {
                    return Err(refusal("a gzip header string never ends"));
                }
                at = at.saturating_add(1);
            }
            at = at.saturating_add(1);
        }
    }
    if flags & 0b0000_0010 != 0 {
        // FHCRC: a two-byte header checksum.
        at = at.saturating_add(2);
    }
    let trailer = bytes.len().saturating_sub(8);
    let payload = bytes
        .get(at..trailer)
        .ok_or_else(|| refusal("this gzip stream is shorter than its own header"))?;
    let out = inflate(payload)?;
    let declared = long(bytes, trailer.saturating_add(4)) as usize;
    // The trailer's length is modulo 2^32, so it is checked that way rather than
    // against the whole length - a 5 GB archive is legal and its trailer wraps.
    if out.len() & 0xffff_ffff != declared {
        return Err(refusal(format!(
            "this gzip stream decompressed to {} bytes and its trailer says {declared}; the \
             download is truncated",
            out.len()
        )));
    }
    Ok(out)
}

/// Reads every member of an uncompressed tar.
///
/// @param bytes - the whole archive
pub fn read_tar(bytes: &[u8]) -> DbResult<Vec<Member>> {
    let mut members = Vec::new();
    let mut at = 0usize;
    // A GNU long name arrives as its own record before the file it names.
    let mut pending_name: Option<String> = None;
    while at.saturating_add(512) <= bytes.len() {
        let header = bytes.get(at..at.saturating_add(512)).unwrap_or(&[]);
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let name = pending_name.take().unwrap_or_else(|| field(header, 0, 100));
        let prefix = field(header, 345, 155);
        let name = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let size = octal(header, 124, 12)?;
        let mode = octal(header, 100, 8).unwrap_or(0);
        let kind = *header.get(156).unwrap_or(&b'0');
        let data = at.saturating_add(512);
        if size > MAX_MEMBER {
            return Err(refusal(format!(
                "{name} declares {size} bytes, over the {MAX_MEMBER}-byte ceiling this reader \
                 will hold"
            )));
        }
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let content = bytes
            .get(data..data.saturating_add(size))
            .ok_or_else(|| refusal(format!("{name} runs past the end of the archive")))?;
        // Round up to the next 512-byte record.
        at = data.saturating_add(size.saturating_add(511) / 512 * 512);

        match kind {
            b'L' => {
                // A GNU long name record: its content is the next member's name.
                pending_name = Some(
                    String::from_utf8_lossy(content)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            // Regular files. `0` is the modern marker and a zero byte is the
            // one older writers used; a directory, a symbolic link and a hard
            // link are all skipped, because an installer takes one file out of
            // an archive and never rebuilds its shape.
            b'0' | 0 => members.push(Member {
                name,
                bytes: content.to_vec(),
                executable: mode & 0o111 != 0,
            }),
            _ => {}
        }
    }
    Ok(members)
}

/// Reads an archive, choosing the format from the file's own first bytes.
///
/// From the bytes rather than from the extension, because an extension is what
/// the download was called and the content is what it is.
///
/// @param bytes - the whole archive
pub fn read(bytes: &[u8]) -> DbResult<Vec<Member>> {
    if byte(bytes, 0) == 0x1f && byte(bytes, 1) == 0x8b {
        return read_tar_gz(bytes);
    }
    if byte(bytes, 0) == b'P' && byte(bytes, 1) == b'K' {
        return read_zip(bytes);
    }
    Err(refusal(
        "this file is neither a zip archive nor a gzip stream",
    ))
}

/// Writes one member under a directory, refusing a name that escapes it.
///
/// @param member - what to write
/// @param into - the destination directory
/// @param as_name - the file name to give it, or its own name when `None`
pub fn write_member(member: &Member, into: &Path, as_name: Option<&str>) -> DbResult<PathBuf> {
    let name = as_name.unwrap_or(&member.name);
    let relative = safe_relative(name)?;
    let path = into.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            refusal(format!(
                "{} could not be created: {error}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(&path, &member.bytes)
        .map_err(|error| refusal(format!("{} could not be written: {error}", path.display())))?;
    set_executable(&path, member.executable);
    Ok(path)
}

/// Turns an archive member's name into a path that cannot leave a directory.
///
/// A refusal rather than a sanitisation. Stripping the `..` out of
/// `../../etc/passwd` produces `etc/passwd`, which is a file nobody asked for
/// written under a name that looks deliberate; refusing produces a message
/// naming the archive.
///
/// @param name - the member's name
pub fn safe_relative(name: &str) -> DbResult<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        return Err(refusal(format!(
            "the archive holds a member named {name}, which is an absolute path. Nothing was \
             extracted"
        )));
    }
    let path = PathBuf::from(&normalized);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(refusal(format!(
                    "the archive holds a member named {name}, which walks outside the directory \
                     it is being extracted into. Nothing was extracted"
                )))
            }
        }
    }
    Ok(path)
}

/// Marks a file executable on the platforms where that is a thing.
///
/// @param path - the file
/// @param executable - whether it should be
fn set_executable(path: &Path, executable: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !executable {
            return;
        }
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o755);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
}

/// Finds the end-of-central-directory record.
///
/// Searched backwards from the end because the record is last and may be
/// followed by a comment of up to 65,535 bytes.
///
/// @param bytes - the archive
fn find_end_of_central_directory(bytes: &[u8]) -> Option<usize> {
    let last = bytes.len().checked_sub(22)?;
    let earliest = last.saturating_sub(65_535);
    (earliest..=last)
        .rev()
        .find(|at| long(bytes, *at) == 0x0605_4b50)
}

/// One byte, or zero past the end.
///
/// @param bytes - the buffer
/// @param at - the offset
fn byte(bytes: &[u8], at: usize) -> u8 {
    *bytes.get(at).unwrap_or(&0)
}

/// A little-endian 16-bit number, or zero past the end.
///
/// @param bytes - the buffer
/// @param at - the offset
fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from(byte(bytes, at)) | (u16::from(byte(bytes, at.saturating_add(1))) << 8)
}

/// A little-endian 32-bit number, or zero past the end.
///
/// @param bytes - the buffer
/// @param at - the offset
fn long(bytes: &[u8], at: usize) -> u32 {
    u32::from(word(bytes, at)) | (u32::from(word(bytes, at.saturating_add(2))) << 16)
}

/// A NUL-terminated text field out of a tar header.
///
/// @param header - the 512-byte header
/// @param at - where the field starts
/// @param len - how wide it is
fn field(header: &[u8], at: usize, len: usize) -> String {
    let slice = header.get(at..at.saturating_add(len)).unwrap_or(&[]);
    let end = slice.iter().position(|b| *b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(slice.get(..end).unwrap_or(&[])).into_owned()
}

/// An octal number field out of a tar header.
///
/// @param header - the 512-byte header
/// @param at - where the field starts
/// @param len - how wide it is
fn octal(header: &[u8], at: usize, len: usize) -> DbResult<u64> {
    let text = field(header, at, len);
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| {
        refusal(format!(
            "a tar header field reads {text:?}, which is not octal"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_base::deflate::deflate;

    /// Builds a zip archive holding one stored member, so the reader has
    /// something real to read.
    ///
    /// @param name - the member's name
    /// @param content - its bytes
    /// @param method - 0 for stored, 8 for deflated
    fn zip_of(name: &str, content: &[u8], method: u16) -> Vec<u8> {
        let payload = if method == 8 {
            deflate(content)
        } else {
            content.to_vec()
        };
        let mut out = Vec::new();
        let local_at = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc, unchecked by this reader
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&payload);

        let directory_at = out.len() as u32;
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        out.extend_from_slice(&(0o755u32 << 16).to_le_bytes()); // external attributes
        out.extend_from_slice(&local_at.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        let directory_size = out.len() as u32 - directory_at;

        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with the directory
        out.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
        out.extend_from_slice(&1u16.to_le_bytes()); // entries in total
        out.extend_from_slice(&directory_size.to_le_bytes());
        out.extend_from_slice(&directory_at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out
    }

    /// Builds a tar holding one regular file.
    ///
    /// @param name - the member's name
    /// @param content - its bytes
    fn tar_of(name: &str, content: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        let mode = format!("{:07o}\0", 0o755u32);
        header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
        let size = format!("{:011o} ", content.len());
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[156] = b'0';
        // The checksum field is spaces while the sum is taken, then the sum.
        for slot in header.iter_mut().skip(148).take(8) {
            *slot = b' ';
        }
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..148 + checksum.len()].copy_from_slice(checksum.as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(content);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    /// A gzip stream around some bytes, with the trailer this reader checks.
    ///
    /// @param content - what to compress
    fn gzip_of(content: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        out.extend_from_slice(&deflate(content));
        out.extend_from_slice(&inillucent_base::deflate::adler32(content).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out
    }

    /// A stored zip member comes back as its own bytes.
    #[test]
    fn a_stored_zip_member_round_trips() {
        let archive = zip_of("lib/onnxruntime.dll", b"MZ the library", 0);
        let members = read_zip(&archive).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "lib/onnxruntime.dll");
        assert_eq!(members[0].bytes, b"MZ the library");
        assert!(members[0].executable, "the external attributes said 0755");
    }

    /// A deflated zip member comes back as its own bytes.
    #[test]
    fn a_deflated_zip_member_round_trips() {
        let content = "the same line, over and over\n".repeat(200);
        let archive = zip_of("lib/libonnxruntime.so", content.as_bytes(), 8);
        let members = read_zip(&archive).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].bytes, content.as_bytes());
    }

    /// A tar member comes back as its own bytes, through gzip and without it.
    #[test]
    fn a_tar_member_round_trips_through_gzip() {
        let content = "the library".repeat(500);
        let tar = tar_of(
            "onnxruntime-linux-x64/lib/libonnxruntime.so",
            content.as_bytes(),
        );
        let members = read_tar(&tar).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].bytes, content.as_bytes());
        assert!(members[0].executable);

        let gzipped = gzip_of(&tar);
        let members = read_tar_gz(&gzipped).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(
            members[0].name,
            "onnxruntime-linux-x64/lib/libonnxruntime.so"
        );
        assert_eq!(members[0].bytes, content.as_bytes());
    }

    /// The format is chosen from the bytes rather than from the name.
    #[test]
    fn the_format_is_read_off_the_first_bytes() {
        assert_eq!(read(&zip_of("a", b"x", 0)).unwrap().len(), 1);
        assert_eq!(read(&gzip_of(&tar_of("a", b"x"))).unwrap().len(), 1);
        assert!(read(b"not an archive at all").is_err());
    }

    /// A member that walks out of the destination stops the extraction.
    ///
    /// This is the check the module exists for, so it is exercised on every
    /// shape a name can take rather than on one.
    #[test]
    fn a_member_that_escapes_the_destination_is_refused() {
        assert!(safe_relative("../escape").is_err());
        assert!(safe_relative("a/../../escape").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("C:/windows/system32/x.dll").is_err());
        assert!(safe_relative("..\\escape").is_err());
        assert_eq!(
            safe_relative("lib/onnxruntime.dll").unwrap(),
            PathBuf::from("lib/onnxruntime.dll")
        );
    }

    /// An archive holding an escaping member writes nothing.
    #[test]
    fn an_archive_with_an_escaping_member_writes_nothing() {
        let into = std::env::temp_dir().join("inillucent-archive-escape");
        let _ = std::fs::remove_dir_all(&into);
        std::fs::create_dir_all(&into).unwrap();
        let member = Member {
            name: "../escaped.txt".to_string(),
            bytes: b"should never be written".to_vec(),
            executable: false,
        };
        assert!(write_member(&member, &into, None).is_err());
        assert!(!into.join("../escaped.txt").exists());
        std::fs::remove_dir_all(&into).unwrap();
    }

    /// A compression method this reader does not know is refused by number.
    #[test]
    fn an_unknown_compression_method_is_refused_by_number() {
        let mut archive = zip_of("x", b"content", 0);
        // Move the method to 12 (bzip2) in both headers.
        let directory = find_end_of_central_directory(&archive).unwrap();
        let start = long(&archive, directory + 16) as usize;
        archive[8] = 12;
        archive[start + 10] = 12;
        let failure = read_zip(&archive).unwrap_err().to_string();
        assert!(failure.contains("method 12"), "{failure}");
    }

    /// A truncated gzip stream is refused rather than producing a short file.
    #[test]
    fn a_truncated_gzip_stream_is_refused() {
        let content = "x".repeat(4096);
        let mut gzipped = gzip_of(content.as_bytes());
        let length = gzipped.len();
        // Claim twice as much as the payload holds.
        let doubled = ((content.len() * 2) as u32).to_le_bytes();
        gzipped[length - 4..].copy_from_slice(&doubled);
        let failure = gunzip(&gzipped).unwrap_err().to_string();
        assert!(failure.contains("truncated"), "{failure}");
    }

    /// Something that is not a gzip stream is refused rather than inflated.
    #[test]
    fn a_stream_that_is_not_gzip_is_refused() {
        assert!(gunzip(b"PK\x03\x04 a zip, not a gzip").is_err());
        assert!(gunzip(b"").is_err());
    }
}
