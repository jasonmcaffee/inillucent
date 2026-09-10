//! TLS through the system OpenSSL, the Unix implementation.
//!
//! Invariant: **the certificate is verified by OpenSSL, not here.** The context
//! is built with `SSL_VERIFY_PEER`, the default verify paths, and
//! `SSL_set1_host`, which is the call that makes OpenSSL check the host name as
//! part of the handshake rather than leaving it to the caller. `SSL_connect`
//! then fails before any application byte is written, which is before either
//! protocol sends a password.
//!
//! ## Why it is loaded at run time
//!
//! Linking `libssl` would make it a build dependency of a workspace whose
//! policy is that a production crate takes none, and would make a machine
//! without the development package unable to build the engine at all. Loading
//! it with `dlopen` keeps the dependency at exactly the two crates already on
//! the allow-list and moves "is TLS available" from build time to run time,
//! where it belongs: the answer is a property of the machine the migration runs
//! on, and it is reported as a refusal that names what to install.
//!
//! `SSL_set1_host` is OpenSSL 1.1.0 and later. Anything older is refused rather
//! than used without it: a session verified without a host name check is
//! verified against every certificate any trusted authority ever issued, which
//! is not a check.

// The one module in this crate that may name `unsafe`: it is the operating-system
// boundary and nothing else. See the note on `deny(unsafe_code)` in `lib.rs`.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_long, c_void, CString};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::sync::OnceLock;

use inillucent_base::DbResult;

/// The names the system TLS library goes by, newest first.
///
/// A version suffix rather than the bare `libssl.so`, because the bare name is
/// part of the *development* package and is missing on a machine that has the
/// library and not the headers - which is most machines.
const CANDIDATES: [&str; 6] = [
    "libssl.so.3",
    "libssl.so.1.1",
    "libssl.3.dylib",
    "libssl.1.1.dylib",
    "libssl.dylib",
    "libssl.so",
];

/// `SSL_VERIFY_PEER`, which makes a failed chain a failed handshake.
const SSL_VERIFY_PEER: c_int = 0x01;

/// `X509_V_OK`, the one result that is not a rejection.
const X509_V_OK: c_long = 0;

/// The entry points this client uses, resolved once.
struct Library {
    /// Builds a client method for the highest protocol both ends have.
    tls_client_method: unsafe extern "C" fn() -> *mut c_void,
    /// Makes a context.
    ssl_ctx_new: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    /// Releases a context.
    ssl_ctx_free: unsafe extern "C" fn(*mut c_void),
    /// Loads the platform's trust store.
    ssl_ctx_set_default_verify_paths: unsafe extern "C" fn(*mut c_void) -> c_int,
    /// Loads one named authority file instead of, or beside, the store.
    ssl_ctx_load_verify_locations:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    /// Turns peer verification on.
    ssl_ctx_set_verify: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void),
    /// Refuses everything below a named protocol version.
    ssl_ctx_ctrl: unsafe extern "C" fn(*mut c_void, c_int, c_long, *mut c_void) -> c_long,
    /// Makes a connection object.
    ssl_new: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    /// Releases a connection object.
    ssl_free: unsafe extern "C" fn(*mut c_void),
    /// Points a connection at a socket.
    ssl_set_fd: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    /// Names the host the certificate must be valid for.
    ssl_set1_host: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    /// Sends the host name in the handshake, which a virtual host needs.
    ssl_ctrl: unsafe extern "C" fn(*mut c_void, c_int, c_long, *mut c_void) -> c_long,
    /// Performs the handshake.
    ssl_connect: unsafe extern "C" fn(*mut c_void) -> c_int,
    /// Reads decrypted bytes.
    ssl_read: unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> c_int,
    /// Writes bytes to be encrypted.
    ssl_write: unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int,
    /// Closes the session politely.
    ssl_shutdown: unsafe extern "C" fn(*mut c_void) -> c_int,
    /// Reports why the handshake failed.
    ssl_get_verify_result: unsafe extern "C" fn(*mut c_void) -> c_long,
    /// Turns a verification result into a sentence.
    x509_verify_cert_error_string: unsafe extern "C" fn(c_long) -> *const c_char,
    /// Names the protocol version that was agreed.
    ssl_get_version: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    /// Which library this turned out to be.
    name: &'static str,
}

// SAFETY: every field is a function pointer into a shared library that stays
// loaded for the life of the process - `dlclose` is never called - and OpenSSL's
// own state is per-`SSL` rather than in these pointers.
unsafe impl Send for Library {}
// SAFETY: as above; the pointers are read-only after resolution.
unsafe impl Sync for Library {}

/// `SSL_CTRL_SET_MIN_PROTO_VERSION`, for refusing TLS 1.1 and below.
const SSL_CTRL_SET_MIN_PROTO_VERSION: c_int = 123;

/// `SSL_CTRL_SET_TLSEXT_HOSTNAME`, for sending the server name.
const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;

/// `TLSEXT_NAMETYPE_host_name`.
const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;

/// `TLS1_2_VERSION`. **The floor, and it is deliberate.** TLS 1.0 and 1.1 are
/// withdrawn, and a migration that negotiated one would be encrypted in a way
/// that satisfies a checklist and not an attacker.
const TLS1_2_VERSION: c_long = 0x0303;

/// The library, resolved on first use.
static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();

extern "C" {
    /// Opens a shared library.
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    /// Looks a symbol up in one.
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// `RTLD_NOW | RTLD_GLOBAL`, so a missing symbol fails here rather than later.
const RTLD_NOW_GLOBAL: c_int = 0x00002 | 0x00100;

/// Resolves one symbol, or returns `None`.
///
/// @param handle - the open library
/// @param name - the symbol
///
/// # Safety
///
/// `handle` must be a live library handle.
unsafe fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let named = CString::new(name).ok()?;
    let found = dlsym(handle, named.as_ptr());
    (!found.is_null()).then_some(found)
}

/// Loads the system TLS library and resolves what this client uses.
///
/// Every symbol is required. A partial resolution is treated as no library at
/// all, because the alternative is discovering the missing one during a
/// handshake, at which point the choice is between a crash and a silent
/// downgrade.
fn library() -> Option<&'static Library> {
    LIBRARY
        .get_or_init(|| {
            for name in CANDIDATES {
                let Ok(named) = CString::new(name) else {
                    continue;
                };
                // SAFETY: `named` is a live NUL-terminated string, and the
                // handle is never closed, so every pointer resolved from it
                // stays valid for the life of the process.
                let handle = unsafe { dlopen(named.as_ptr(), RTLD_NOW_GLOBAL) };
                if handle.is_null() {
                    continue;
                }
                // SAFETY: `handle` is a live library handle, and each cast is
                // from a resolved symbol to the signature OpenSSL publishes for
                // it. A wrong signature here is a defect this file owns; the
                // names are stable across 1.1 and 3.
                let resolved = unsafe {
                    Some(Library {
                        tls_client_method: std::mem::transmute(symbol(
                            handle,
                            "TLS_client_method",
                        )?),
                        ssl_ctx_new: std::mem::transmute(symbol(handle, "SSL_CTX_new")?),
                        ssl_ctx_free: std::mem::transmute(symbol(handle, "SSL_CTX_free")?),
                        ssl_ctx_set_default_verify_paths: std::mem::transmute(symbol(
                            handle,
                            "SSL_CTX_set_default_verify_paths",
                        )?),
                        ssl_ctx_load_verify_locations: std::mem::transmute(symbol(
                            handle,
                            "SSL_CTX_load_verify_locations",
                        )?),
                        ssl_ctx_set_verify: std::mem::transmute(symbol(
                            handle,
                            "SSL_CTX_set_verify",
                        )?),
                        ssl_ctx_ctrl: std::mem::transmute(symbol(handle, "SSL_CTX_ctrl")?),
                        ssl_new: std::mem::transmute(symbol(handle, "SSL_new")?),
                        ssl_free: std::mem::transmute(symbol(handle, "SSL_free")?),
                        ssl_set_fd: std::mem::transmute(symbol(handle, "SSL_set_fd")?),
                        // The host name check. Absent before OpenSSL 1.1.0, and
                        // its absence is why such a library is refused rather
                        // than used.
                        ssl_set1_host: std::mem::transmute(symbol(handle, "SSL_set1_host")?),
                        ssl_ctrl: std::mem::transmute(symbol(handle, "SSL_ctrl")?),
                        ssl_connect: std::mem::transmute(symbol(handle, "SSL_connect")?),
                        ssl_read: std::mem::transmute(symbol(handle, "SSL_read")?),
                        ssl_write: std::mem::transmute(symbol(handle, "SSL_write")?),
                        ssl_shutdown: std::mem::transmute(symbol(handle, "SSL_shutdown")?),
                        ssl_get_verify_result: std::mem::transmute(symbol(
                            handle,
                            "SSL_get_verify_result",
                        )?),
                        x509_verify_cert_error_string: std::mem::transmute(symbol(
                            handle,
                            "X509_verify_cert_error_string",
                        )?),
                        ssl_get_version: std::mem::transmute(symbol(handle, "SSL_get_version")?),
                        name,
                    })
                };
                if resolved.is_some() {
                    return resolved;
                }
            }
            None
        })
        .as_ref()
}

/// Returns the name of the TLS library this machine will use.
pub fn available() -> Option<&'static str> {
    library().map(|held| held.name)
}

/// One established TLS session over a socket.
pub struct Session {
    /// The socket, held so it is not closed under OpenSSL.
    socket: TcpStream,
    /// The connection object.
    ssl: *mut c_void,
    /// The context it was made from.
    context: *mut c_void,
    /// The resolved library.
    library: &'static Library,
    /// What the peer turned out to be, for the migration's report.
    peer: String,
}

// SAFETY: an `SSL` and its `SSL_CTX` are used from one thread here - the
// migration runs on one - and are released exactly once in `Drop`.
unsafe impl Send for Session {}

impl Session {
    /// Returns what the verified peer was, for the migration's report.
    pub fn description(&self) -> String {
        self.peer.clone()
    }

    /// Closes the session politely and then the socket.
    pub fn shutdown(&mut self) {
        // SAFETY: `ssl` is live until `Drop`, and a shutdown on an already
        // shut session is a documented no-op that returns a status.
        unsafe { (self.library.ssl_shutdown)(self.ssl) };
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Session {
    /// Releases the connection object and its context.
    fn drop(&mut self) {
        // SAFETY: both pointers came from the calls whose companions these are
        // and neither is used after this; `Drop` runs once.
        unsafe {
            (self.library.ssl_free)(self.ssl);
            (self.library.ssl_ctx_free)(self.context);
        }
    }
}

/// Connects, handshakes and verifies.
///
/// @param socket - the connected socket
/// @param host - the name to check the certificate against
/// @param root - an extra certificate authority file, when the URL named one
pub fn connect(socket: TcpStream, host: &str, root: Option<&str>) -> DbResult<Session> {
    let Some(library) = library() else {
        return Err(super::unavailable(format!(
            "none of {} could be loaded",
            CANDIDATES.join(", ")
        )));
    };
    let named = CString::new(host)
        .map_err(|_| super::not_verified(host, "the host name contains a NUL byte"))?;

    // SAFETY: `TLS_client_method` takes nothing and returns a static method
    // table, and `SSL_CTX_new` takes ownership of nothing.
    let context = unsafe { (library.ssl_ctx_new)((library.tls_client_method)()) };
    if context.is_null() {
        return Err(super::unavailable("a TLS context could not be made"));
    }

    let build = || -> DbResult<Session> {
        // SAFETY: `context` is live for the length of this closure, which runs
        // before the release below.
        unsafe {
            if (library.ssl_ctx_set_default_verify_paths)(context) != 1 && root.is_none() {
                return Err(super::unavailable(
                    "this machine's certificate authority store could not be read, so no \
                     certificate could be verified against it",
                ));
            }
            if let Some(path) = root {
                let file = CString::new(path).map_err(|_| {
                    super::not_verified(host, "the sslrootcert path contains a NUL byte")
                })?;
                if (library.ssl_ctx_load_verify_locations)(context, file.as_ptr(), std::ptr::null())
                    != 1
                {
                    return Err(super::not_verified(
                        host,
                        format!("the authority file {path} could not be read"),
                    ));
                }
            }
            (library.ssl_ctx_set_verify)(context, SSL_VERIFY_PEER, std::ptr::null_mut());
            (library.ssl_ctx_ctrl)(
                context,
                SSL_CTRL_SET_MIN_PROTO_VERSION,
                TLS1_2_VERSION,
                std::ptr::null_mut(),
            );
        }

        // SAFETY: `context` is live.
        let ssl = unsafe { (library.ssl_new)(context) };
        if ssl.is_null() {
            return Err(super::unavailable("a TLS connection could not be made"));
        }

        // SAFETY: `ssl` is live and the socket outlives it - the `Session`
        // below owns both, and every early return frees `ssl` before dropping
        // the socket.
        let prepared = unsafe {
            let fd = socket.as_raw_fd();
            if (library.ssl_set_fd)(ssl, fd) != 1 {
                return Err(super::unavailable("the socket could not be given to TLS"));
            }
            // **This is the host name check.** Without it OpenSSL verifies the
            // chain and nothing else, which accepts any certificate any trusted
            // authority ever issued for any name.
            if (library.ssl_set1_host)(ssl, named.as_ptr()) != 1 {
                return Err(super::not_verified(
                    host,
                    "the host name could not be given to the certificate check",
                ));
            }
            // The name in the handshake as well, so a server hosting several
            // names answers with the right certificate rather than its default.
            (library.ssl_ctrl)(
                ssl,
                SSL_CTRL_SET_TLSEXT_HOSTNAME,
                TLSEXT_NAMETYPE_HOST_NAME,
                named.as_ptr() as *mut c_void,
            );
            (library.ssl_connect)(ssl)
        };

        if prepared != 1 {
            // SAFETY: `ssl` is live and is released exactly once on this path.
            let reason = unsafe {
                let result = (library.ssl_get_verify_result)(ssl);
                let text = match result == X509_V_OK {
                    true => "the TLS handshake failed. A server speaking its protocol in the \
                             clear on this port answers a handshake with its own greeting, \
                             which is what this looks like."
                        .to_string(),
                    false => describe(library, result),
                };
                (library.ssl_free)(ssl);
                text
            };
            return Err(super::not_verified(host, reason));
        }

        // SAFETY: the handshake succeeded, so `ssl` is an established session.
        let (result, version) = unsafe {
            let result = (library.ssl_get_verify_result)(ssl);
            let version = (library.ssl_get_version)(ssl);
            let version = match version.is_null() {
                true => "TLS".to_string(),
                false => std::ffi::CStr::from_ptr(version)
                    .to_string_lossy()
                    .into_owned(),
            };
            (result, version)
        };
        if result != X509_V_OK {
            // SAFETY: released exactly once on this path.
            let reason = unsafe {
                let text = describe(library, result);
                (library.ssl_free)(ssl);
                text
            };
            return Err(super::not_verified(host, reason));
        }

        Ok(Session {
            socket,
            ssl,
            context,
            library,
            peer: format!("a certificate trusted by this machine, for {host}, over {version}"),
        })
    };

    match build() {
        Ok(session) => Ok(session),
        Err(failure) => {
            // SAFETY: `context` is live and is released exactly once on the
            // path where no `Session` took ownership of it.
            unsafe { (library.ssl_ctx_free)(context) };
            Err(failure)
        }
    }
}

/// Turns a verification result into a sentence an operator can act on.
///
/// @param library - the resolved library
/// @param result - what `SSL_get_verify_result` reported
///
/// # Safety
///
/// `library` must hold live function pointers.
unsafe fn describe(library: &Library, result: c_long) -> String {
    let text = (library.x509_verify_cert_error_string)(result);
    let said = match text.is_null() {
        true => format!("verification result {result}"),
        false => std::ffi::CStr::from_ptr(text)
            .to_string_lossy()
            .into_owned(),
    };
    format!(
        "{said}. If the server uses a private authority, name it with sslrootcert=<file>; if the \
         name is wrong, connect by the name on the certificate."
    )
}

impl Read for Session {
    /// Reads decrypted bytes.
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let want = output.len().min(c_int::MAX as usize) as c_int;
        // SAFETY: `ssl` is established and `output` is a live buffer of at
        // least `want` bytes for the length of the call.
        let read = unsafe { (self.library.ssl_read)(self.ssl, output.as_mut_ptr().cast(), want) };
        match read {
            count if count > 0 => Ok(count as usize),
            // Zero is a closed session, which the readers above treat as the
            // server having hung up - the same thing a plaintext socket does.
            0 => Ok(0),
            _ => Err(std::io::Error::other("the TLS session could not be read")),
        }
    }
}

impl Write for Session {
    /// Writes bytes to be encrypted.
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        let want = input.len().min(c_int::MAX as usize) as c_int;
        // SAFETY: `ssl` is established and `input` is a live buffer of at least
        // `want` bytes for the length of the call.
        let written = unsafe { (self.library.ssl_write)(self.ssl, input.as_ptr().cast(), want) };
        match written {
            count if count > 0 => Ok(count as usize),
            _ => Err(std::io::Error::other(
                "the TLS session could not be written",
            )),
        }
    }

    /// Nothing to flush: OpenSSL writes each record as it is produced.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
