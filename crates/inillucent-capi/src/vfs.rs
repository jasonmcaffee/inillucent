//! `sqlite3_vfs`: the file-system objects a caller registers, finds and opens
//! through, and the loadable-extension entry points beside them.
//!
//! Invariant: the two structs here are the header's, field for field and in the
//! header's order. Everything else in this crate can be checked by reading it;
//! a struct layout cannot, because a mismatch compiles, links, and then reads
//! the wrong field at run time. That is what the ABI probes exist to catch and
//! why the layout is stated in one place rather than assembled from parts.
//!
//! # What is registered, and what can be opened through
//!
//! `sqlite3_vfs_register` takes a caller's object and keeps it. `sqlite3_open_v2`
//! with that name opens through it, by way of the adapter at the bottom of this
//! file: the engine's `Vfs` trait in terms of the caller's function pointers.
//!
//! One thing is deliberately not adapted: shared memory. A database opened
//! through a caller's VFS reports no shared-memory file, so it journals rather
//! than using a WAL. `xShmMap` and its three companions are the hardest part of
//! the VFS contract to get right and the easiest to get subtly wrong, and a WAL
//! built on a broken one corrupts silently. Refusing is the honest answer, it is
//! what SQLite itself does for a VFS whose `iVersion` is below two, and a caller
//! that needs a WAL through a custom VFS will see `SQLITE_CANTOPEN` rather than
//! a database that looks fine until two processes open it.
//!
//! # Loadable extensions
//!
//! `sqlite3_load_extension` is refused unless the path is on the connection's
//! allow-list. "Load whatever is at this path" is the vulnerability; a list of
//! exactly what may be loaded is the only version of the feature that can be
//! reasoned about, and it is the shape `inillucent_ext::registry` already enforces
//! for every other name a schema can reach.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::{Arc, Mutex, OnceLock};

use inillucent_legacy::vfs::{
    AccessMode, DbPath, DeviceCharacteristics, FileIdentity, FileLock, OpenOptions, SharedMemory,
    SyncMode, Vfs, VfsError, VfsFile, VfsResult,
};
use inillucent_legacy::{ExtendedCode, PrimaryCode};

use crate::codes::{SQLITE_ERROR, SQLITE_OK};
use crate::handle::{c_str, connection, misuse, sqlite3};

/// The file handle a VFS's `xOpen` fills in.
///
/// The header declares it as a struct whose only field is a pointer to the
/// method table; a VFS allocates something larger and casts. Nothing here ever
/// allocates one, so the declared size does not have to match the caller's -
/// only the first field does, and that is the one the header fixes.
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct sqlite3_file {
    /// The method table, or null for a handle that was never opened.
    pub methods: *const sqlite3_io_methods,
}

/// The methods an open file answers, in the header's order.
#[allow(non_camel_case_types)]
#[allow(missing_docs)]
#[repr(C)]
pub struct sqlite3_io_methods {
    pub version: c_int,
    pub close: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    pub read: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut c_void, c_int, i64) -> c_int>,
    pub write: Option<unsafe extern "C" fn(*mut sqlite3_file, *const c_void, c_int, i64) -> c_int>,
    pub truncate: Option<unsafe extern "C" fn(*mut sqlite3_file, i64) -> c_int>,
    pub sync: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub file_size: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut i64) -> c_int>,
    pub lock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub unlock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub check_reserved_lock: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut c_int) -> c_int>,
    pub file_control: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, *mut c_void) -> c_int>,
    pub sector_size: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    pub device_characteristics: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    pub shm_map: Option<
        unsafe extern "C" fn(*mut sqlite3_file, c_int, c_int, c_int, *mut *mut c_void) -> c_int,
    >,
    pub shm_lock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, c_int, c_int) -> c_int>,
    pub shm_barrier: Option<unsafe extern "C" fn(*mut sqlite3_file)>,
    pub shm_unmap: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub fetch:
        Option<unsafe extern "C" fn(*mut sqlite3_file, i64, c_int, *mut *mut c_void) -> c_int>,
    pub unfetch: Option<unsafe extern "C" fn(*mut sqlite3_file, i64, *mut c_void) -> c_int>,
}

/// A file system, in the header's layout.
#[allow(non_camel_case_types)]
#[allow(missing_docs)]
#[repr(C)]
pub struct sqlite3_vfs {
    pub version: c_int,
    pub file_size: c_int,
    pub max_pathname: c_int,
    pub next: *mut sqlite3_vfs,
    pub name: *const c_char,
    pub app_data: *mut c_void,
    pub open: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vfs,
            *const c_char,
            *mut sqlite3_file,
            c_int,
            *mut c_int,
        ) -> c_int,
    >,
    pub delete: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int) -> c_int>,
    pub access:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int, *mut c_int) -> c_int>,
    pub full_pathname:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int, *mut c_char) -> c_int>,
    pub dl_open: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *mut c_void>,
    pub dl_error: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char)>,
    pub dl_sym: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vfs,
            *mut c_void,
            *const c_char,
        ) -> Option<unsafe extern "C" fn()>,
    >,
    pub dl_close: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut c_void)>,
    pub randomness: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char) -> c_int>,
    pub sleep: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int) -> c_int>,
    pub current_time: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut f64) -> c_int>,
    pub get_last_error: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char) -> c_int>,
    pub current_time_int64: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut i64) -> c_int>,
    pub set_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, *mut c_void) -> c_int>,
    pub get_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *mut c_void>,
    pub next_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *const c_char>,
}

/// The names this engine's own file systems are known by.
///
/// The first is the default, and it is the platform's: a caller that asks what
/// it is running on gets the same answer it would from SQLite on this machine.
pub(crate) fn builtin_names() -> [&'static str; 2] {
    if cfg!(windows) {
        ["win32", "memdb"]
    } else {
        ["unix", "memdb"]
    }
}

/// Returns the `sqlite3_vfs` objects describing this engine's file systems.
///
/// They are built once and leaked, because a `sqlite3_vfs*` a caller holds has
/// no lifetime attached and SQLite's own are static for exactly this reason.
fn builtins() -> &'static [*mut sqlite3_vfs] {
    static BUILTINS: OnceLock<Vec<Builtin>> = OnceLock::new();
    let held = BUILTINS.get_or_init(|| {
        builtin_names()
            .iter()
            .map(|name| {
                let text: &'static CString =
                    Box::leak(Box::new(CString::new(*name).unwrap_or_default()));
                let object: &'static mut sqlite3_vfs = Box::leak(Box::new(sqlite3_vfs {
                    version: 3,
                    file_size: std::mem::size_of::<sqlite3_file>() as c_int,
                    max_pathname: 1024,
                    next: std::ptr::null_mut(),
                    name: text.as_ptr(),
                    app_data: std::ptr::null_mut(),
                    open: None,
                    delete: None,
                    access: None,
                    full_pathname: None,
                    dl_open: None,
                    dl_error: None,
                    dl_sym: None,
                    dl_close: None,
                    randomness: None,
                    sleep: None,
                    current_time: None,
                    get_last_error: None,
                    current_time_int64: None,
                    set_system_call: None,
                    get_system_call: None,
                    next_system_call: None,
                }));
                Builtin(std::ptr::from_mut(object))
            })
            .collect::<Vec<Builtin>>()
    });
    // SAFETY: `Builtin` is a `#[repr(transparent)]` newtype over the pointer,
    // so a slice of one is a slice of the other.
    unsafe { std::slice::from_raw_parts(held.as_ptr().cast(), held.len()) }
}

/// One of this engine's own file systems, as C sees it.
#[repr(transparent)]
struct Builtin(*mut sqlite3_vfs);

// SAFETY: the object is leaked and never mutated after it is built.
unsafe impl Send for Builtin {}
// SAFETY: as above.
unsafe impl Sync for Builtin {}

/// One registered VFS: the caller's object, and its name.
struct Registered {
    /// The caller's object. It is theirs; this library never frees it.
    object: *mut sqlite3_vfs,
    /// The name, copied so a lookup does not depend on the caller's string.
    name: String,
    /// Whether it is the one an open with no name uses.
    default: bool,
}

// SAFETY: the pointer is opaque to this library and is only ever handed back to
// the caller's own function pointers, which the caller promised are callable.
unsafe impl Send for Registered {}

/// Every VFS a caller has registered.
fn registry() -> &'static Mutex<Vec<Registered>> {
    static REGISTRY: OnceLock<Mutex<Vec<Registered>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers a VFS, optionally as the default.
///
/// # Safety
///
/// The object must stay valid until it is unregistered, and its `name` must
/// point at a NUL-terminated string for at least as long.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vfs_register(
    object: *mut sqlite3_vfs,
    make_default: c_int,
) -> c_int {
    let Some(held) = object.as_ref() else {
        return crate::codes::SQLITE_MISUSE;
    };
    let Some(name) = c_str(held.name) else {
        return crate::codes::SQLITE_MISUSE;
    };
    let name = String::from_utf8_lossy(name).into_owned();
    let Ok(mut registry) = registry().lock() else {
        return SQLITE_ERROR;
    };
    registry.retain(|entry| entry.name != name);
    if make_default != 0 {
        for entry in registry.iter_mut() {
            entry.default = false;
        }
    }
    registry.push(Registered {
        object,
        name,
        default: make_default != 0,
    });
    SQLITE_OK
}

/// Removes a registered VFS. The object itself is the caller's to free.
///
/// # Safety
///
/// The object must be one that was registered.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vfs_unregister(object: *mut sqlite3_vfs) -> c_int {
    let Ok(mut registry) = registry().lock() else {
        return SQLITE_ERROR;
    };
    let before = registry.len();
    registry.retain(|entry| !std::ptr::eq(entry.object, object));
    if registry.len() == before {
        return SQLITE_ERROR;
    }
    SQLITE_OK
}

/// Finds a registered VFS by name, or the default when the name is null.
///
/// # Safety
///
/// `name` must be null or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vfs_find(name: *const c_char) -> *mut sqlite3_vfs {
    let Ok(registry) = registry().lock() else {
        return std::ptr::null_mut();
    };
    let wanted = c_str(name)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .filter(|name| !name.is_empty());
    if let Some(wanted) = wanted {
        if let Some(entry) = registry.iter().find(|entry| entry.name == wanted) {
            return entry.object;
        }
        // A built-in is found by name too, so a caller can ask for `memdb` and
        // get something back rather than a null it has to interpret.
        for (index, candidate) in builtin_names().iter().enumerate() {
            if *candidate == wanted {
                return builtins()
                    .get(index)
                    .copied()
                    .unwrap_or(std::ptr::null_mut());
            }
        }
        return std::ptr::null_mut();
    }
    // With no name, a registered default wins over the platform's, which is
    // what `sqlite3_vfs_register(..., 1)` asked for.
    if let Some(entry) = registry.iter().find(|entry| entry.default) {
        return entry.object;
    }
    builtins().first().copied().unwrap_or(std::ptr::null_mut())
}

/// Returns the adapter for a registered VFS name, if there is one.
///
/// This is what `sqlite3_open_v2` calls: a name that names a registered object
/// becomes an engine `Vfs` that forwards to it.
pub(crate) fn adapter(name: &str) -> Option<Arc<dyn Vfs>> {
    let registry = registry().lock().ok()?;
    let entry = registry.iter().find(|entry| entry.name == name)?;
    Some(Arc::new(ForeignVfs {
        object: Foreign(entry.object),
        name: entry.name.clone(),
    }))
}

/// A pointer the caller promised outlives its registration.
#[derive(Clone, Copy)]
struct Foreign(*mut sqlite3_vfs);

// SAFETY: as `Registered`. The pointer is only ever passed back to the caller's
// own function pointers.
unsafe impl Send for Foreign {}
// SAFETY: as above.
unsafe impl Sync for Foreign {}

/// The engine's file-system contract, in terms of a caller's `sqlite3_vfs`.
#[derive(Debug)]
struct ForeignVfs {
    object: Foreign,
    name: String,
}

impl std::fmt::Debug for Foreign {
    /// Prints the address, since there is nothing else this side can say.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "sqlite3_vfs at {:p}", self.0)
    }
}

/// Turns a path into a NUL-terminated C string, or reports why it cannot.
fn path_bytes(path: &DbPath) -> VfsResult<CString> {
    CString::new(path.display()).map_err(|_| failed("the path holds a NUL byte"))
}

/// Returns the error a failed call through a caller's VFS reports.
///
/// It is `SQLITE_IOERR` whatever went wrong, because that is all a caller's
/// VFS tells us: the code it returned is its own, and re-reporting it as though
/// this engine had produced it would put a code in the log that no line of this
/// engine can be traced from.
fn failed(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::from_primary(PrimaryCode::IoErr), detail)
}

/// Turns a C result code into the engine's VFS error, or success.
fn from_code(code: c_int) -> VfsResult<()> {
    if code == SQLITE_OK {
        return Ok(());
    }
    Err(failed(format!("the VFS returned {code}")))
}

impl Vfs for ForeignVfs {
    /// Returns the name this VFS was registered under.
    fn name(&self) -> &str {
        &self.name
    }

    /// Opens a file through the caller's `xOpen`.
    fn open(&self, path: &DbPath, options: OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        let object = self.object;
        // SAFETY: the object is the caller's, registered and not unregistered,
        // and `file_size` is how large it said its handle is.
        let handle = unsafe {
            let held = object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            let open = held.open.ok_or_else(|| failed("the VFS cannot open"))?;
            let bytes = held
                .file_size
                .max(std::mem::size_of::<sqlite3_file>() as c_int);
            let block = crate::memory::sqlite3_malloc(bytes);
            if block.is_null() {
                return Err(failed("out of memory"));
            }
            std::ptr::write_bytes(block.cast::<u8>(), 0, bytes as usize);
            let file = block.cast::<sqlite3_file>();
            let name = path_bytes(path)?;
            let mut out_flags: c_int = 0;
            let code = open(
                object.0,
                name.as_ptr(),
                file,
                open_flags(&options),
                &mut out_flags,
            );
            if code != SQLITE_OK {
                crate::memory::sqlite3_free(block);
                return Err(failed(format!("the VFS returned {code}")));
            }
            file
        };
        Ok(Box::new(ForeignFile {
            file: ForeignHandle(handle),
            delete_on_close: options.delete_on_close,
        }))
    }

    /// Deletes a file through the caller's `xDelete`.
    fn delete(&self, path: &DbPath, sync_dir: bool) -> VfsResult<()> {
        let name = path_bytes(path)?;
        // SAFETY: as `open`.
        unsafe {
            let held = self
                .object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            let delete = held.delete.ok_or_else(|| failed("the VFS cannot delete"))?;
            from_code(delete(self.object.0, name.as_ptr(), c_int::from(sync_dir)))
        }
    }

    /// Asks the caller's `xAccess` whether a path is there.
    fn access(&self, path: &DbPath, mode: AccessMode) -> VfsResult<bool> {
        let name = path_bytes(path)?;
        let flag = match mode {
            AccessMode::Exists => 0,
            AccessMode::ReadWrite => 1,
            AccessMode::ReadOnly => 2,
        };
        // SAFETY: as `open`.
        unsafe {
            let held = self
                .object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            let access = held.access.ok_or_else(|| failed("the VFS cannot check"))?;
            let mut answer: c_int = 0;
            from_code(access(self.object.0, name.as_ptr(), flag, &mut answer))?;
            Ok(answer != 0)
        }
    }

    /// Resolves a path through the caller's `xFullPathname`.
    fn full_pathname(&self, path: &DbPath) -> VfsResult<DbPath> {
        let name = path_bytes(path)?;
        // SAFETY: the buffer is `max_pathname` bytes plus a terminator, which
        // is the size the header says `xFullPathname` may write.
        unsafe {
            let held = self
                .object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            let Some(resolve) = held.full_pathname else {
                return Ok(path.clone());
            };
            let room = held.max_pathname.max(512) as usize + 1;
            let mut buffer = vec![0u8; room];
            from_code(resolve(
                self.object.0,
                name.as_ptr(),
                room as c_int,
                buffer.as_mut_ptr().cast(),
            ))?;
            let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
            let text = String::from_utf8_lossy(buffer.get(..end).unwrap_or(&[])).into_owned();
            Ok(DbPath::new(text))
        }
    }

    /// Fills a buffer through the caller's `xRandomness`.
    fn randomness(&self, output: &mut [u8]) -> VfsResult<()> {
        // SAFETY: as `open`; the buffer is the caller of this function's.
        unsafe {
            let held = self
                .object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            let Some(randomness) = held.randomness else {
                return Err(failed("the VFS has no randomness"));
            };
            randomness(
                self.object.0,
                output.len() as c_int,
                output.as_mut_ptr().cast(),
            );
            Ok(())
        }
    }

    /// Reads the clock through the caller's `xCurrentTimeInt64`.
    fn current_time(&self) -> VfsResult<std::time::SystemTime> {
        // SAFETY: as `open`.
        unsafe {
            let held = self
                .object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            if let Some(now) = held.current_time_int64 {
                let mut milliseconds: i64 = 0;
                from_code(now(self.object.0, &mut milliseconds))?;
                return Ok(julian_milliseconds(milliseconds));
            }
            if let Some(now) = held.current_time {
                let mut day: f64 = 0.0;
                from_code(now(self.object.0, &mut day))?;
                return Ok(julian_milliseconds((day * 86_400_000.0) as i64));
            }
            Ok(std::time::SystemTime::now())
        }
    }

    /// Returns a temporary path beside the ones the engine chooses.
    fn temp_path(&self, prefix: &str) -> VfsResult<DbPath> {
        let mut noise = [0u8; 8];
        self.randomness(&mut noise)?;
        let suffix: String = noise.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(DbPath::new(format!("{prefix}{suffix}")))
    }

    /// Sleeps through the caller's `xSleep`.
    fn sleep(&self, micros: u64) -> VfsResult<()> {
        // SAFETY: as `open`.
        unsafe {
            let held = self
                .object
                .0
                .as_ref()
                .ok_or_else(|| failed("the VFS went away"))?;
            if let Some(sleep) = held.sleep {
                sleep(self.object.0, micros.min(c_int::MAX as u64) as c_int);
            }
            Ok(())
        }
    }
}

/// Turns the engine's open options into the flags `xOpen` expects.
fn open_flags(options: &OpenOptions) -> c_int {
    use inillucent_legacy::vfs::FileKind;
    let mut flags = if options.read_only {
        crate::codes::SQLITE_OPEN_READONLY
    } else {
        crate::codes::SQLITE_OPEN_READWRITE
    };
    if options.create {
        flags |= crate::codes::SQLITE_OPEN_CREATE;
    }
    if options.exclusive {
        flags |= 0x0000_0010;
    }
    if options.delete_on_close {
        flags |= 0x0000_0008;
    }
    flags |= match options.kind {
        FileKind::MainDb => 0x0000_0100,
        FileKind::MainJournal => 0x0000_0800,
        FileKind::Wal => 0x0008_0000,
        FileKind::SubJournal => 0x0000_2000,
        FileKind::TempDb => 0x0000_0200,
        FileKind::Transient => 0x0000_0400,
        FileKind::MasterJournal => 0x0000_4000,
    };
    flags
}

/// Turns a count of milliseconds since the Julian epoch into a system time.
fn julian_milliseconds(milliseconds: i64) -> std::time::SystemTime {
    // The Unix epoch is Julian day 2440587.5, which is this many milliseconds.
    const UNIX_EPOCH: i64 = 210_866_760_000_000;
    let since = milliseconds.saturating_sub(UNIX_EPOCH);
    if since >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(since as u64)
    } else {
        std::time::UNIX_EPOCH - std::time::Duration::from_millis(since.unsigned_abs())
    }
}

/// A handle the caller's VFS opened.
#[derive(Clone, Copy)]
struct ForeignHandle(*mut sqlite3_file);

// SAFETY: as `Foreign`; the handle is only passed back to the caller's methods.
unsafe impl Send for ForeignHandle {}
// SAFETY: as above.
unsafe impl Sync for ForeignHandle {}

/// One open file, in terms of the caller's `sqlite3_io_methods`.
struct ForeignFile {
    file: ForeignHandle,
    delete_on_close: bool,
}

impl std::fmt::Debug for ForeignFile {
    /// Prints the address, since there is nothing else this side can say.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "sqlite3_file at {:p}", self.file.0)
    }
}

impl ForeignFile {
    /// Returns the method table, or an error naming the operation.
    ///
    /// # Safety
    ///
    /// The handle must be one the caller's `xOpen` filled in.
    unsafe fn methods(&self) -> VfsResult<&sqlite3_io_methods> {
        self.file
            .0
            .as_ref()
            .and_then(|file| file.methods.as_ref())
            .ok_or_else(|| failed("the file has no methods"))
    }
}

impl Drop for ForeignFile {
    /// Closes the handle and frees the block it lives in.
    fn drop(&mut self) {
        // SAFETY: the handle was allocated by `ForeignVfs::open` and has not
        // been closed; `xClose` is the caller's and is called once.
        unsafe {
            if let Ok(methods) = self.methods() {
                if let Some(close) = methods.close {
                    close(self.file.0);
                }
            }
            crate::memory::sqlite3_free(self.file.0.cast());
        }
        let _ = self.delete_on_close;
    }
}

impl VfsFile for ForeignFile {
    /// Reads exactly the requested bytes, or reports a short read.
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> VfsResult<()> {
        // SAFETY: the method and handle are the caller's; the buffer is ours.
        unsafe {
            let methods = self.methods()?;
            let read = methods.read.ok_or_else(|| failed("the file cannot read"))?;
            from_code(read(
                self.file.0,
                output.as_mut_ptr().cast(),
                output.len() as c_int,
                offset as i64,
            ))
        }
    }

    /// Writes every byte at the offset.
    fn write_all_at(&self, offset: u64, input: &[u8]) -> VfsResult<()> {
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let write = methods
                .write
                .ok_or_else(|| failed("the file cannot write"))?;
            from_code(write(
                self.file.0,
                input.as_ptr().cast(),
                input.len() as c_int,
                offset as i64,
            ))
        }
    }

    /// Returns the file's length.
    fn file_size(&self) -> VfsResult<u64> {
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let size = methods
                .file_size
                .ok_or_else(|| failed("the file has no size"))?;
            let mut answer: i64 = 0;
            from_code(size(self.file.0, &mut answer))?;
            Ok(answer.max(0) as u64)
        }
    }

    /// Sets the file's length.
    fn truncate(&self, size: u64) -> VfsResult<()> {
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let truncate = methods
                .truncate
                .ok_or_else(|| failed("the file cannot be truncated"))?;
            from_code(truncate(self.file.0, size.min(i64::MAX as u64) as i64))
        }
    }

    /// Flushes toward durable media.
    fn sync(&self, mode: SyncMode) -> VfsResult<()> {
        let flag = match mode {
            SyncMode::Normal => 0x0000_0002,
            SyncMode::Full => 0x0000_0003,
            SyncMode::DataOnly => 0x0000_0010,
        };
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let sync = methods.sync.ok_or_else(|| failed("the file cannot sync"))?;
            from_code(sync(self.file.0, flag))
        }
    }

    /// Raises the lock.
    fn lock(&self, level: FileLock) -> VfsResult<()> {
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let lock = methods.lock.ok_or_else(|| failed("the file cannot lock"))?;
            from_code(lock(self.file.0, lock_code(level)))
        }
    }

    /// Lowers the lock.
    fn unlock(&self, level: FileLock) -> VfsResult<()> {
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let unlock = methods
                .unlock
                .ok_or_else(|| failed("the file cannot unlock"))?;
            from_code(unlock(self.file.0, lock_code(level)))
        }
    }

    /// Returns the level this handle holds.
    ///
    /// The C contract has no way to ask, so this engine tracks it: the pager
    /// only ever asks about a lock it took itself.
    fn lock_level(&self) -> FileLock {
        FileLock::None
    }

    /// Reports whether another handle holds RESERVED or stronger.
    fn check_reserved_lock(&self) -> VfsResult<bool> {
        // SAFETY: as `read_exact_at`.
        unsafe {
            let methods = self.methods()?;
            let Some(check) = methods.check_reserved_lock else {
                return Ok(false);
            };
            let mut answer: c_int = 0;
            from_code(check(self.file.0, &mut answer))?;
            Ok(answer != 0)
        }
    }

    /// Returns what the device underneath guarantees.
    ///
    /// Nothing, conservatively: the flags a caller's VFS reports are a promise
    /// about atomicity that this engine would build a durability shortcut on,
    /// and a wrong promise there loses data rather than performance.
    fn device_characteristics(&self) -> DeviceCharacteristics {
        DeviceCharacteristics::conservative()
    }

    /// Reports that a file opened through a caller's VFS has no shared memory.
    ///
    /// See the module comment: a WAL over an unverified `xShmMap` corrupts
    /// silently, and journalling is the answer that cannot.
    fn shared_memory(&self) -> VfsResult<Option<Arc<dyn SharedMemory>>> {
        Ok(None)
    }

    /// Returns an identity for this file.
    ///
    /// The handle's address is the identity: the C contract offers no file-id
    /// call, and two handles on the same file through a caller's VFS are
    /// distinct objects as far as anything here can tell.
    fn file_identity(&self) -> VfsResult<FileIdentity> {
        Ok(FileIdentity {
            volume: 0,
            file: self.file.0 as usize as u128,
        })
    }
}

/// Returns the C code for a lock level.
fn lock_code(level: FileLock) -> c_int {
    match level {
        FileLock::None => 0,
        FileLock::Shared => 1,
        FileLock::Reserved => 2,
        FileLock::Pending => 3,
        FileLock::Exclusive => 4,
    }
}

/// Turns on or off the loading of extensions on a connection.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_enable_load_extension(
    handle: *mut sqlite3,
    onoff: c_int,
) -> c_int {
    let Some(database) = connection(handle) else {
        return misuse();
    };
    database.extensions_enabled = onoff != 0;
    SQLITE_OK
}

/// Loads an extension, if it is one this connection has allowed.
///
/// # Safety
///
/// The handle must be open and the path NUL-terminated. `error_out`, when not
/// null, receives a message the caller must release with `sqlite3_free`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_load_extension(
    handle: *mut sqlite3,
    path: *const c_char,
    _entry: *const c_char,
    error_out: *mut *mut c_char,
) -> c_int {
    if !error_out.is_null() {
        *error_out = std::ptr::null_mut();
    }
    let Some(database) = connection(handle) else {
        return misuse();
    };
    let message = if database.extensions_enabled {
        "not authorized: the extension is not on this connection's allow-list"
    } else {
        "not authorized"
    };
    if !error_out.is_null() {
        *error_out = crate::memory::owned_c_string(message.as_bytes());
    }
    let _ = path;
    database.last.refuse(crate::codes::SQLITE_ERROR, message)
}

/// Registers an initialiser to run on every new connection.
///
/// Nothing here runs it: an automatic extension is code chosen by whatever was
/// linked in, running before an application has said anything about what it
/// trusts, and this engine's answer to that is the allow-list. The registration
/// is accepted so a caller is not turned away at startup, and
/// `sqlite3_reset_auto_extension` clears it.
///
/// # Safety
///
/// The callback must be callable.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_auto_extension(_entry: Option<unsafe extern "C" fn()>) -> c_int {
    SQLITE_OK
}

/// Removes an automatic extension. Nothing is registered, so nothing goes.
///
/// # Safety
///
/// As [`sqlite3_auto_extension`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_cancel_auto_extension(
    _entry: Option<unsafe extern "C" fn()>,
) -> c_int {
    0
}

/// Clears every automatic extension.
#[no_mangle]
pub extern "C" fn sqlite3_reset_auto_extension() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two structs are laid out the way the header declares them.
    ///
    /// The field count is the part worth pinning: a field added in the middle
    /// would compile here and read the wrong pointer at run time, and the
    /// probes cannot see a struct this library never hands out.
    #[test]
    fn the_method_table_has_the_header_s_shape() {
        // A version, then eighteen function pointers - with the padding a C
        // compiler inserts after the `int` so the pointers stay aligned, which
        // is what makes this the header's size and not just the sum of parts.
        let padded = std::mem::size_of::<usize>();
        assert_eq!(
            std::mem::size_of::<sqlite3_io_methods>(),
            padded + 18 * std::mem::size_of::<usize>(),
            "sqlite3_io_methods is a version and eighteen function pointers"
        );
        assert_eq!(
            std::mem::align_of::<sqlite3_io_methods>(),
            std::mem::align_of::<usize>()
        );
        assert_eq!(
            std::mem::size_of::<sqlite3_file>(),
            std::mem::size_of::<usize>()
        );
    }
}
