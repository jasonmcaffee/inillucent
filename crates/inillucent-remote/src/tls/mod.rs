//! Verified TLS for the two migration clients, through the platform's own
//! implementation.
//!
//! Invariant: **a session this module hands back has had the server's
//! certificate chain built to a trusted root and its host name checked, and a
//! failure of either stops the connection before a password is written.** The
//! order matters as much as the check: both protocols upgrade the socket
//! *before* their authentication exchange, so a certificate that does not
//! verify costs the operator a refusal rather than a credential.
//!
//! ## Why there is no TLS crate here
//!
//! `docs/dependency-policy.md` allows one category that fits this: the
//! operating-system boundary, which is what `libc` and `windows-sys` already
//! are. Every platform this ships on has an audited TLS implementation and a
//! trust store that somebody else keeps current, and reaching them is an FFI
//! call. The two alternatives were both worse:
//!
//! - **A TLS crate.** `rustls` is the obvious one and it is a large new
//!   production dependency with a cryptographic backend of its own, in a crate
//!   `inillucent-cli` links - and the policy's worked argument against the
//!   `postgres` crate turns on exactly that.
//! - **Writing TLS here.** Not a real option, and saying so is the point: a
//!   first-party X.509 chain builder and record layer would be a much larger
//!   security finding than the one this fixes.
//!
//! So Windows uses SChannel through SSPI and Unix uses the system OpenSSL,
//! loaded at run time. Neither implements any cryptography in this workspace;
//! both hand the certificate to the platform and act on its answer.
//!
//! ## What a missing implementation does
//!
//! Refuses, by name. A machine with no usable TLS gets a message saying so and
//! naming the two ways forward - install it, or say `sslmode=disable` and pass
//! `--insecure-plaintext` for a loopback or trusted endpoint. It does **not**
//! fall back to plaintext: falling back is the failure this whole change is
//! about, and a fallback nobody sees is worse than the refusal.

use std::net::TcpStream;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;

#[cfg(not(any(windows, unix)))]
mod platform {
    //! The fallback for a platform with neither backend, which refuses.

    use std::io::{Read, Write};

    use super::*;

    /// Reports that no TLS implementation is reachable here.
    pub fn available() -> Option<&'static str> {
        None
    }

    /// A session that cannot exist on this platform.
    pub struct Session;

    impl Session {
        /// Returns what the peer was, which never happens here.
        pub fn description(&self) -> String {
            String::new()
        }

        /// Shuts the session down, which never happens here.
        pub fn shutdown(&mut self) {}
    }

    impl Read for Session {
        /// Never reads, because a session is never built.
        fn read(&mut self, _output: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no TLS on this platform"))
        }
    }

    impl Write for Session {
        /// Never writes, because a session is never built.
        fn write(&mut self, _input: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no TLS on this platform"))
        }

        /// Never flushes, because a session is never built.
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Refuses, because this platform has no backend.
    pub fn connect(_socket: TcpStream, _host: &str, _root: Option<&str>) -> DbResult<Session> {
        Err(super::unavailable("this platform has no TLS backend"))
    }
}

pub use platform::Session;

/// Returns the name of the TLS implementation this machine will use.
///
/// `None` when there is none, which is what makes "no TLS here" a refusal a
/// caller can produce before it dials rather than a failure half way through a
/// handshake.
pub fn available() -> Option<&'static str> {
    platform::available()
}

/// Upgrades a connected socket to TLS, verifying the server.
///
/// @param socket - the connected socket, before any protocol bytes are written
/// @param host - the name to check the certificate against
/// @param root - a certificate authority file to trust in addition to the
///   platform's store, when the URL named one
pub fn connect(socket: TcpStream, host: &str, root: Option<&str>) -> DbResult<Session> {
    if host.is_empty() {
        return Err(refusal(
            "verified TLS needs a host name to check the certificate against, and this URL \
             names none",
        ));
    }
    platform::connect(socket, host, root)
}

/// Builds the refusal a machine with no usable TLS gets.
///
/// It names the two ways forward rather than only the problem, because the
/// operator hitting this is usually one flag away from a correct run and the
/// wrong flag away from an insecure one.
///
/// @param why - what was missing
pub(crate) fn unavailable(why: impl std::fmt::Display) -> inillucent_base::error::DbError {
    refusal(format!(
        "this migration asks for verified TLS and no TLS implementation is reachable here: {why}. \
         Install the platform's TLS library, or - only for a loopback address or a network you \
         trust - write sslmode=disable in the URL and pass --insecure-plaintext."
    ))
}

/// Reads a certificate authority file and returns its DER bytes.
///
/// PEM or DER, because an operator's authority file is whichever their tooling
/// produced and refusing one of the two would be a puzzle rather than a policy.
/// Only the first certificate in a PEM file is taken: an authority file with
/// several is a chain, and the root is what a chain is built *to*.
///
/// Windows only: SChannel is handed the certificate's bytes, while OpenSSL
/// on Unix is handed the path and reads the file itself.
///
/// @param path - the file the URL named
#[cfg(windows)]
pub(crate) fn load_root(path: &str) -> DbResult<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(|error| {
        refusal(format!(
            "sslrootcert names {path}, which could not be read: {error}"
        ))
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let Some(start) = text.find("-----BEGIN CERTIFICATE-----") else {
        // No PEM header, so it is already DER.
        return Ok(bytes);
    };
    let body = text.get(start..).unwrap_or_default();
    let Some(end) = body.find("-----END CERTIFICATE-----") else {
        return Err(refusal(format!(
            "sslrootcert names {path}, which begins a PEM certificate and never ends it"
        )));
    };
    let inner: String = body
        .get("-----BEGIN CERTIFICATE-----".len()..end)
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    crate::auth::base64_decode(&inner).ok_or_else(|| {
        refusal(format!(
            "sslrootcert names {path}, whose PEM body is not valid base64"
        ))
    })
}

/// Builds the refusal a failed certificate check gets.
///
/// **It names the host and the reason and nothing else.** A verification
/// failure is the message an operator reads while deciding whether they are
/// being attacked or have a stale certificate, and a message carrying the URL
/// would carry the password with it.
///
/// @param host - the name that was being checked
/// @param why - what the platform said was wrong
pub(crate) fn not_verified(
    host: &str,
    why: impl std::fmt::Display,
) -> inillucent_base::error::DbError {
    refusal(format!(
        "the TLS certificate {host} presented did not verify: {why}. Nothing was sent - the \
         connection stopped before this migration wrote a password. Fix the certificate, or name \
         its authority with sslrootcert=<file> if it is a private one."
    ))
}
