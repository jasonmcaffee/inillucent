//! TLS through SChannel, the Windows implementation.
//!
//! Invariant: **the certificate is verified by Windows, not here.** SChannel is
//! given the server's name up front and asked to validate automatically, so the
//! chain is built against the machine's own trust store and the host name is
//! checked by the same code every other program on the machine uses. A failure
//! comes back as a `SEC_E_*` status from `InitializeSecurityContextW` and stops
//! the handshake, which is before either protocol writes an authentication
//! packet.
//!
//! When the URL names its own authority with `sslrootcert=`, validation is
//! taken over from SChannel instead: the peer chain is retrieved with
//! `QueryContextAttributesW`, built with `CertGetCertificateChain` against a
//! temporary store holding that authority, and the host name is checked with
//! `CertVerifyCertificateChainPolicy` under `CERT_CHAIN_POLICY_SSL`. Both
//! halves still happen inside Windows; what changes is which roots are
//! considered.
//!
//! ## The shape of the record layer
//!
//! SChannel does not read or write the socket. It converts between plaintext
//! and records and tells the caller what it needs, so every function here is a
//! loop around three states: a buffer of received bytes that may be an
//! incomplete record, a call that consumes what it can, and a socket read when
//! it could not. `SECBUFFER_EXTRA` is the part that has to be right - a single
//! `read` very often returns one and a half records, and the half has to be
//! kept for the next call rather than dropped.

// The one module in this crate that may name `unsafe`: it is the operating-system
// boundary and nothing else. See the note on `deny(unsafe_code)` in `lib.rs`.
#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::mem::size_of;
use std::net::TcpStream;
use std::ptr::{null, null_mut};

use inillucent_base::DbResult;

use windows_sys::Win32::Foundation::{
    CERT_E_CN_NO_MATCH, CERT_E_EXPIRED, CERT_E_UNTRUSTEDROOT, CRYPT_E_REVOCATION_OFFLINE,
    SEC_E_CERT_EXPIRED, SEC_E_ILLEGAL_MESSAGE, SEC_E_INCOMPLETE_MESSAGE, SEC_E_INVALID_TOKEN,
    SEC_E_OK, SEC_E_UNTRUSTED_ROOT, SEC_E_WRONG_PRINCIPAL, SEC_I_CONTEXT_EXPIRED,
    SEC_I_CONTINUE_NEEDED, SEC_I_RENEGOTIATE,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, DecryptMessage, DeleteSecurityContext, EncryptMessage,
    FreeContextBuffer, FreeCredentialsHandle, InitializeSecurityContextW, QueryContextAttributesW,
    SecBuffer, SecBufferDesc, SecPkgContext_StreamSizes, ISC_REQ_ALLOCATE_MEMORY,
    ISC_REQ_CONFIDENTIALITY, ISC_REQ_MANUAL_CRED_VALIDATION, ISC_REQ_REPLAY_DETECT,
    ISC_REQ_SEQUENCE_DETECT, ISC_REQ_STREAM, ISC_REQ_USE_SUPPLIED_CREDS, SCHANNEL_CRED,
    SCHANNEL_CRED_VERSION, SCH_CRED_AUTO_CRED_VALIDATION, SCH_CRED_MANUAL_CRED_VALIDATION,
    SCH_CRED_NO_DEFAULT_CREDS, SCH_USE_STRONG_CRYPTO, SECBUFFER_ALERT, SECBUFFER_DATA,
    SECBUFFER_EMPTY, SECBUFFER_EXTRA, SECBUFFER_STREAM_HEADER, SECBUFFER_STREAM_TRAILER,
    SECBUFFER_TOKEN, SECBUFFER_VERSION, SECPKG_ATTR_REMOTE_CERT_CONTEXT, SECPKG_ATTR_STREAM_SIZES,
    SECPKG_CRED_OUTBOUND, UNISP_NAME_W,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::Security::Cryptography::{
    CertAddEncodedCertificateToStore, CertCloseStore, CertCreateCertificateChainEngine,
    CertFreeCertificateChain, CertFreeCertificateChainEngine, CertFreeCertificateContext,
    CertGetCertificateChain, CertOpenStore, CertVerifyCertificateChainPolicy,
    HTTPSPolicyCallbackData, AUTHTYPE_SERVER, CERT_CHAIN_CONTEXT, CERT_CHAIN_ENGINE_CONFIG,
    CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG, CERT_CHAIN_PARA, CERT_CHAIN_POLICY_PARA,
    CERT_CHAIN_POLICY_SSL, CERT_CHAIN_POLICY_STATUS, CERT_CONTEXT, CERT_STORE_ADD_ALWAYS,
    CERT_STORE_CREATE_NEW_FLAG, CERT_STORE_PROV_MEMORY, X509_ASN_ENCODING,
};

/// How many bytes are read from the socket in one go during a handshake.
///
/// A TLS record is at most 16 KiB of plaintext plus its header and trailer, and
/// a handshake flight is several of them. This is one read's worth rather than
/// a ceiling: everything here loops until SChannel says it has enough.
const CHUNK: usize = 16 * 1024;

/// The largest handshake this client will accumulate before giving up.
///
/// A server that never completes a handshake would otherwise be able to make
/// this process grow a buffer for as long as it kept sending bytes.
const MAX_HANDSHAKE: usize = 256 * 1024;

/// Returns the name of the implementation, which is always present here.
pub fn available() -> Option<&'static str> {
    Some("SChannel")
}

/// One established TLS session over a socket.
pub struct Session {
    /// The socket the records travel on.
    socket: TcpStream,
    /// The credentials the context was built from.
    credentials: SecHandle,
    /// The security context itself.
    context: SecHandle,
    /// How large a header, a trailer and a message may be.
    sizes: SecPkgContext_StreamSizes,
    /// Bytes read from the socket that are not yet a whole record.
    incoming: Vec<u8>,
    /// Decrypted bytes not yet handed to the caller.
    plaintext: Vec<u8>,
    /// How far into `plaintext` the caller has read.
    at: usize,
    /// What the peer turned out to be, for the migration's report.
    peer: String,
    /// Whether the peer has closed the session.
    finished: bool,
}

// SAFETY: a `SecHandle` is two pointer-sized opaque values that SSPI
// dereferences only through its own calls, and every one of those calls is made
// from the thread that owns this `Session` - the migration runs on one thread
// and hands the session to nobody. The type is `!Send` only because it holds
// raw integers that Rust cannot reason about, not because SSPI has thread
// affinity for a context.
unsafe impl Send for Session {}

impl Session {
    /// Returns what the verified peer was, for the migration's report.
    pub fn description(&self) -> String {
        self.peer.clone()
    }

    /// Closes the session and releases what SSPI allocated.
    pub fn shutdown(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Session {
    /// Releases the security context and the credentials.
    fn drop(&mut self) {
        // SAFETY: both handles were produced by the calls whose companions
        // these are, neither has been released before - `Drop` runs once - and
        // neither is used afterwards.
        unsafe {
            DeleteSecurityContext(&self.context);
            FreeCredentialsHandle(&self.credentials);
        }
    }
}

/// Connects, handshakes and verifies.
///
/// @param socket - the connected socket
/// @param host - the name to check the certificate against
/// @param root - an extra certificate authority file, when the URL named one
pub fn connect(socket: TcpStream, host: &str, root: Option<&str>) -> DbResult<Session> {
    let extra_root = match root {
        None => None,
        Some(path) => Some(super::load_root(path)?),
    };
    let credentials = acquire(extra_root.is_some())?;
    // Both halves have to say manual, and finding that out cost an afternoon.
    // `SCH_CRED_MANUAL_CRED_VALIDATION` on the credential is not enough on its
    // own: SChannel still validates the chain inside
    // `InitializeSecurityContextW` and returns `SEC_E_UNTRUSTED_ROOT` before
    // this client ever sees the certificate, so a private authority named with
    // `sslrootcert=` could never be honoured. The context flag below is what
    // actually defers the decision.
    match handshake(socket, host, credentials, extra_root.as_deref()) {
        Ok(session) => Ok(session),
        Err(failure) => {
            // SAFETY: `credentials` came from `AcquireCredentialsHandleW` above
            // and the handshake did not take ownership of it on this path.
            unsafe { FreeCredentialsHandle(&credentials) };
            Err(failure)
        }
    }
}

/// Acquires outbound SChannel credentials.
///
/// **`SCH_USE_STRONG_CRYPTO` is not decoration.** Without it SChannel will
/// negotiate the algorithms Windows keeps for compatibility, and a migration
/// that negotiated RC4 would be encrypted in the sense that matters to nobody.
///
/// @param manual - whether this client will validate the chain itself, which it
///   does only when the URL named its own authority
fn acquire(manual: bool) -> DbResult<SecHandle> {
    let mut credentials = SecHandle {
        dwLower: 0,
        dwUpper: 0,
    };
    let validation = match manual {
        true => SCH_CRED_MANUAL_CRED_VALIDATION,
        false => SCH_CRED_AUTO_CRED_VALIDATION,
    };
    // SAFETY: `SCHANNEL_CRED` is a plain C structure of integers, unions and raw
    // pointers with no `Drop`, no reference and no niche, so every byte
    // pattern is a valid value of it. All zeroes is what the Win32 headers
    // themselves initialise one to before filling in `cbSize`, which is the
    // next statement.
    let mut description: SCHANNEL_CRED = unsafe { std::mem::zeroed() };
    description.dwVersion = SCHANNEL_CRED_VERSION;
    description.dwFlags = SCH_USE_STRONG_CRYPTO | SCH_CRED_NO_DEFAULT_CREDS | validation;
    // SAFETY: every pointer is null or points at a live local for the length of
    // the call, and `description` is a zeroed `SCHANNEL_CRED` with the two
    // fields SSPI reads for an outbound credential set.
    let status = unsafe {
        AcquireCredentialsHandleW(
            null(),
            UNISP_NAME_W,
            SECPKG_CRED_OUTBOUND,
            null_mut(),
            (&description as *const SCHANNEL_CRED).cast(),
            None,
            null_mut(),
            &mut credentials,
            null_mut(),
        )
    };
    if status != SEC_E_OK {
        return Err(super::unavailable(format!(
            "SChannel would not give this process outbound credentials (0x{status:08x})"
        )));
    }
    Ok(credentials)
}

/// Drives the handshake to completion and verifies the peer.
///
/// @param socket - the connected socket
/// @param host - the name to check against
/// @param credentials - the outbound credentials
/// @param extra_root - a DER authority to trust, when the URL named one
fn handshake(
    mut socket: TcpStream,
    host: &str,
    credentials: SecHandle,
    extra_root: Option<&[u8]>,
) -> DbResult<Session> {
    let mut target: Vec<u16> = host.encode_utf16().collect();
    target.push(0);
    let mut requested = ISC_REQ_SEQUENCE_DETECT
        | ISC_REQ_REPLAY_DETECT
        | ISC_REQ_CONFIDENTIALITY
        | ISC_REQ_ALLOCATE_MEMORY
        | ISC_REQ_STREAM
        | ISC_REQ_USE_SUPPLIED_CREDS;
    if extra_root.is_some() {
        // See `connect`: without this SChannel decides for itself and refuses
        // the private authority before `verify_peer` is reached.
        requested |= ISC_REQ_MANUAL_CRED_VALIDATION;
    }

    let mut context = SecHandle {
        dwLower: 0,
        dwUpper: 0,
    };
    let mut have_context = false;
    let mut received: Vec<u8> = Vec::new();
    let mut attributes: u32 = 0;

    loop {
        let out_buffer = SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: null_mut(),
        };
        let alert = SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_ALERT,
            pvBuffer: null_mut(),
        };
        let mut out_buffers = [out_buffer, alert];
        let mut out = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 2,
            pBuffers: out_buffers.as_mut_ptr(),
        };

        let mut in_buffers = [
            SecBuffer {
                cbBuffer: received.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: received.as_mut_ptr().cast(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: null_mut(),
            },
        ];
        let input = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 2,
            pBuffers: in_buffers.as_mut_ptr(),
        };

        // SAFETY: `credentials` is live, `context` is either uninitialised on
        // the first call (signalled by passing null for the old context) or the
        // one the previous call produced, `target` is a NUL-terminated UTF-16
        // buffer that outlives the call, and both descriptors point at arrays
        // that outlive it. `ISC_REQ_ALLOCATE_MEMORY` means SSPI owns the output
        // buffer, which is freed with `FreeContextBuffer` below.
        let status = unsafe {
            InitializeSecurityContextW(
                &credentials,
                match have_context {
                    true => &context,
                    false => null(),
                },
                target.as_ptr(),
                requested,
                0,
                0,
                match received.is_empty() {
                    true => null(),
                    false => &input,
                },
                0,
                &mut context,
                &mut out,
                &mut attributes,
                null_mut(),
            )
        };
        have_context = true;

        // Whatever SSPI produced goes to the server before anything else is
        // decided, including on a failure: an alert record is how the server is
        // told why this client stopped.
        let produced = out_buffers.first().copied().unwrap_or(SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: null_mut(),
        });
        if produced.cbBuffer > 0 && !produced.pvBuffer.is_null() {
            // SAFETY: SSPI allocated `pvBuffer` with `cbBuffer` valid bytes and
            // this is the only reader of it before `FreeContextBuffer`.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    produced.pvBuffer.cast::<u8>(),
                    produced.cbBuffer as usize,
                )
            };
            let written = socket.write_all(bytes);
            // SAFETY: the buffer came from SSPI under
            // `ISC_REQ_ALLOCATE_MEMORY`, so SSPI frees it, and nothing reads it
            // after this line.
            unsafe { FreeContextBuffer(produced.pvBuffer) };
            written.map_err(|error| {
                super::not_verified(host, format!("the handshake could not be written: {error}"))
            })?;
        }

        match status {
            SEC_E_OK => {
                // Anything the server sent past the end of the handshake is the
                // first application record and must be kept.
                let leftover = extra_of(&in_buffers, &received);
                let session = finish(
                    socket,
                    host,
                    credentials,
                    context,
                    leftover,
                    extra_root,
                    requested,
                )?;
                return Ok(session);
            }
            SEC_I_CONTINUE_NEEDED => {
                received = extra_of(&in_buffers, &received);
                read_more(&mut socket, &mut received, host)?;
            }
            SEC_E_INCOMPLETE_MESSAGE => {
                read_more(&mut socket, &mut received, host)?;
            }
            other => {
                // SAFETY: a context was produced by at least the first call and
                // is released exactly once here, on the only path that leaves
                // the loop without handing it to a `Session`.
                unsafe { DeleteSecurityContext(&context) };
                return Err(super::not_verified(host, describe(other)));
            }
        }
    }
}

/// Returns the bytes SChannel did not consume, as a fresh buffer.
///
/// **This is the part that has to be right.** A socket read returns whatever
/// arrived, which is regularly one and a bit records; the bit belongs to the
/// next call and dropping it corrupts the stream in a way that shows up as a
/// protocol error somewhere else entirely.
///
/// @param buffers - the input buffers the last call was given
/// @param received - the buffer they described
fn extra_of(buffers: &[SecBuffer; 2], received: &[u8]) -> Vec<u8> {
    for buffer in buffers.iter() {
        if buffer.BufferType == SECBUFFER_EXTRA && buffer.cbBuffer > 0 {
            let count = buffer.cbBuffer as usize;
            let from = received.len().saturating_sub(count);
            return received.get(from..).unwrap_or(&[]).to_vec();
        }
    }
    Vec::new()
}

/// Reads one chunk from the socket onto the end of a buffer.
///
/// @param socket - the socket
/// @param into - the buffer to extend
/// @param host - the name, for a failure message
fn read_more(socket: &mut TcpStream, into: &mut Vec<u8>, host: &str) -> DbResult<()> {
    if into.len() >= MAX_HANDSHAKE {
        return Err(super::not_verified(
            host,
            format!("the handshake passed {MAX_HANDSHAKE} bytes without completing"),
        ));
    }
    let mut scratch = vec![0u8; CHUNK];
    match socket.read(&mut scratch) {
        Ok(0) => Err(super::not_verified(
            host,
            "the server closed the connection during the TLS handshake, which is what a server \
             that does not speak TLS on this port does",
        )),
        Ok(read) => {
            into.extend_from_slice(scratch.get(..read).unwrap_or(&[]));
            Ok(())
        }
        Err(error) => Err(super::not_verified(
            host,
            format!("the handshake could not be read: {error}"),
        )),
    }
}

/// Completes a handshake into a session, verifying the peer first.
///
/// @param socket - the socket
/// @param host - the name checked against
/// @param credentials - the credentials, moved into the session
/// @param context - the established context, moved into the session
/// @param leftover - application bytes that arrived with the last flight
/// @param extra_root - a DER authority to trust, when the URL named one
/// @param requested - the context flags that were asked for
#[allow(clippy::too_many_arguments)]
fn finish(
    socket: TcpStream,
    host: &str,
    credentials: SecHandle,
    context: SecHandle,
    leftover: Vec<u8>,
    extra_root: Option<&[u8]>,
    requested: u32,
) -> DbResult<Session> {
    let _ = requested;
    // SAFETY: `SecPkgContext_StreamSizes` is a plain C structure of integers, unions and raw
    // pointers with no `Drop`, no reference and no niche, so every byte
    // pattern is a valid value of it. All zeroes is what the Win32 headers
    // themselves initialise one to before filling in `cbSize`, which is the
    // next statement.
    let mut sizes: SecPkgContext_StreamSizes = unsafe { std::mem::zeroed() };
    // SAFETY: the context is established, and `sizes` is a live local of the
    // type this attribute writes.
    let status = unsafe {
        QueryContextAttributesW(
            &context,
            SECPKG_ATTR_STREAM_SIZES,
            (&mut sizes as *mut SecPkgContext_StreamSizes).cast(),
        )
    };
    if status != SEC_E_OK {
        // SAFETY: released once, on a path that does not build a `Session`.
        unsafe { DeleteSecurityContext(&context) };
        return Err(super::not_verified(
            host,
            format!("SChannel would not report its record sizes (0x{status:08x})"),
        ));
    }

    let peer = match verify_peer(&context, host, extra_root) {
        Ok(peer) => peer,
        Err(failure) => {
            // SAFETY: released once, on a path that does not build a `Session`.
            unsafe { DeleteSecurityContext(&context) };
            return Err(failure);
        }
    };

    Ok(Session {
        socket,
        credentials,
        context,
        sizes,
        incoming: leftover,
        plaintext: Vec::new(),
        at: 0,
        peer,
        finished: false,
    })
}

/// Reads the peer certificate and returns what it is.
///
/// With no extra authority SChannel has already validated the chain and the
/// host name, so this only names the subject for the report. With one, the
/// credentials were acquired for manual validation and this is where the chain
/// is actually built and checked - against a temporary store holding that
/// authority, with `CERT_CHAIN_POLICY_SSL` doing the host name.
///
/// @param context - the established context
/// @param host - the name to check against
/// @param extra_root - a DER authority to trust, when the URL named one
fn verify_peer(context: &SecHandle, host: &str, extra_root: Option<&[u8]>) -> DbResult<String> {
    let mut certificate: *mut CERT_CONTEXT = null_mut();
    // SAFETY: the context is established and `certificate` is a live local the
    // attribute writes a pointer into. The pointer is a reference SSPI hands
    // over and is released with `CertFreeCertificateContext` below.
    let status = unsafe {
        QueryContextAttributesW(
            context,
            SECPKG_ATTR_REMOTE_CERT_CONTEXT,
            (&mut certificate as *mut *mut CERT_CONTEXT).cast(),
        )
    };
    if status != SEC_E_OK || certificate.is_null() {
        return Err(super::not_verified(
            host,
            "the server presented no certificate",
        ));
    }
    let outcome = match extra_root {
        None => Ok(format!("a certificate trusted by this machine, for {host}")),
        Some(root) => verify_against(certificate, host, root),
    };
    // SAFETY: `certificate` came from the query above and nothing reads it
    // after this line.
    unsafe { CertFreeCertificateContext(certificate) };
    outcome
}

/// Builds and checks the chain against one named authority.
///
/// @param certificate - the peer's certificate
/// @param host - the name to check against
/// @param root - the authority's DER bytes
fn verify_against(certificate: *mut CERT_CONTEXT, host: &str, root: &[u8]) -> DbResult<String> {
    // SAFETY: a memory store takes no provider argument and the flags are the
    // documented ones for a new in-process store.
    let store = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_MEMORY,
            0,
            0,
            CERT_STORE_CREATE_NEW_FLAG,
            null(),
        )
    };
    if store.is_null() {
        return Err(super::not_verified(
            host,
            "a temporary certificate store could not be made",
        ));
    }
    // SAFETY: `root` is a live slice for the length of the call and the store
    // copies what it is given.
    let added = unsafe {
        CertAddEncodedCertificateToStore(
            store,
            X509_ASN_ENCODING,
            root.as_ptr(),
            root.len() as u32,
            CERT_STORE_ADD_ALWAYS,
            null_mut(),
        )
    };
    if added == 0 {
        // SAFETY: the store was opened above and is closed exactly once.
        unsafe { CertCloseStore(store, 0) };
        return Err(super::not_verified(
            host,
            "the file named by sslrootcert is not a certificate this platform can read",
        ));
    }

    // **The store has to be the chain engine's *exclusive root*, not an extra
    // store to look in.** `hadditionalstore` supplies certificates for
    // *building* a chain and decides nothing about trust, so a chain built that
    // way to a private authority still comes back with
    // `CERT_TRUST_IS_UNTRUSTED_ROOT` and `sslrootcert=` could never work. An
    // engine with `hExclusiveRoot` set says "this, and only this, is a root",
    // which is exactly what naming an authority means - and it is stricter than
    // the machine store, not weaker: nothing else is trusted for this
    // connection.
    // SAFETY: `CERT_CHAIN_ENGINE_CONFIG` is a plain C structure of integers, unions and raw
    // pointers with no `Drop`, no reference and no niche, so every byte
    // pattern is a valid value of it. All zeroes is what the Win32 headers
    // themselves initialise one to before filling in `cbSize`, which is the
    // next statement.
    let mut configuration: CERT_CHAIN_ENGINE_CONFIG = unsafe { std::mem::zeroed() };
    configuration.cbSize = size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32;
    configuration.hExclusiveRoot = store;
    configuration.dwExclusiveFlags = CERT_CHAIN_EXCLUSIVE_ENABLE_CA_FLAG;
    let mut engine = 0;
    // SAFETY: `configuration` is a correctly sized zeroed structure whose one
    // store is live, and `engine` is a local the call writes.
    let made = unsafe { CertCreateCertificateChainEngine(&configuration, &mut engine) };
    if made == 0 {
        // SAFETY: the store was opened above and is closed exactly once here.
        unsafe { CertCloseStore(store, 0) };
        return Err(super::not_verified(
            host,
            "a certificate chain engine could not be made for the authority in sslrootcert",
        ));
    }

    let mut parameters: CERT_CHAIN_PARA = unsafe { std::mem::zeroed() };
    parameters.cbSize = size_of::<CERT_CHAIN_PARA>() as u32;
    let mut chain: *mut CERT_CHAIN_CONTEXT = null_mut();
    // SAFETY: the engine, the certificate and the store are live, `parameters`
    // is a correctly sized zeroed structure, and `chain` is a local the call
    // writes.
    let built = unsafe {
        CertGetCertificateChain(
            engine,
            certificate,
            null(),
            store,
            &parameters,
            0,
            null_mut(),
            &mut chain,
        )
    };
    // SAFETY: both were made above; the chain holds its own references to what
    // it needs, so releasing here is correct and happens once.
    unsafe {
        CertFreeCertificateChainEngine(engine);
        CertCloseStore(store, 0);
    }
    if built == 0 || chain.is_null() {
        return Err(super::not_verified(
            host,
            "no chain could be built from the server's certificate to the authority in \
             sslrootcert",
        ));
    }

    let mut name: Vec<u16> = host.encode_utf16().collect();
    name.push(0);
    // SAFETY: `HTTPSPolicyCallbackData` is a plain C structure of integers, unions and raw
    // pointers with no `Drop`, no reference and no niche, so every byte
    // pattern is a valid value of it. All zeroes is what the Win32 headers
    // themselves initialise one to before filling in `cbSize`, which is the
    // next statement.
    let mut https: HTTPSPolicyCallbackData = unsafe { std::mem::zeroed() };
    // The first field is a union of two names for the same `u32`, which is
    // what the header does; either arm writes the same bytes.
    https.Anonymous.cbStruct = size_of::<HTTPSPolicyCallbackData>() as u32;
    https.dwAuthType = AUTHTYPE_SERVER;
    https.pwszServerName = name.as_mut_ptr();
    // SAFETY: `CERT_CHAIN_POLICY_PARA` is a plain C structure of integers, unions and raw
    // pointers with no `Drop`, no reference and no niche, so every byte
    // pattern is a valid value of it. All zeroes is what the Win32 headers
    // themselves initialise one to before filling in `cbSize`, which is the
    // next statement.
    let mut policy: CERT_CHAIN_POLICY_PARA = unsafe { std::mem::zeroed() };
    policy.cbSize = size_of::<CERT_CHAIN_POLICY_PARA>() as u32;
    policy.pvExtraPolicyPara = (&mut https as *mut HTTPSPolicyCallbackData).cast();
    // SAFETY: `CERT_CHAIN_POLICY_STATUS` is a plain C structure of integers, unions and raw
    // pointers with no `Drop`, no reference and no niche, so every byte
    // pattern is a valid value of it. All zeroes is what the Win32 headers
    // themselves initialise one to before filling in `cbSize`, which is the
    // next statement.
    let mut result: CERT_CHAIN_POLICY_STATUS = unsafe { std::mem::zeroed() };
    result.cbSize = size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
    // SAFETY: the chain is live, and both structures are correctly sized
    // zeroed locals that outlive the call.
    let checked = unsafe {
        CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, chain, &policy, &mut result)
    };
    // SAFETY: the chain came from `CertGetCertificateChain` and is released
    // exactly once, with nothing reading it afterwards.
    unsafe { CertFreeCertificateChain(chain) };
    if checked == 0 {
        return Err(super::not_verified(
            host,
            "the chain policy could not be run",
        ));
    }
    if result.dwError != 0 {
        return Err(super::not_verified(host, describe(result.dwError as i32)));
    }
    Ok(format!(
        "a certificate chaining to the authority in sslrootcert, for {host}"
    ))
}

impl Read for Session {
    /// Hands back decrypted bytes, decrypting a record when there are none.
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.at < self.plaintext.len() {
                let available = self.plaintext.len().saturating_sub(self.at);
                let taking = available.min(output.len());
                let (from, to) = (self.at, self.at.saturating_add(taking));
                let source = self.plaintext.get(from..to).unwrap_or(&[]).to_vec();
                if let Some(slot) = output.get_mut(..taking) {
                    slot.copy_from_slice(&source);
                }
                self.at = to;
                return Ok(taking);
            }
            if self.finished {
                return Ok(0);
            }
            self.plaintext.clear();
            self.at = 0;
            self.decrypt_one()?;
        }
    }
}

impl Session {
    /// Decrypts one record, reading from the socket until there is a whole one.
    fn decrypt_one(&mut self) -> std::io::Result<()> {
        loop {
            if !self.incoming.is_empty() {
                let mut buffers = [
                    SecBuffer {
                        cbBuffer: self.incoming.len() as u32,
                        BufferType: SECBUFFER_DATA,
                        pvBuffer: self.incoming.as_mut_ptr().cast(),
                    },
                    empty(),
                    empty(),
                    empty(),
                ];
                let message = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: 4,
                    pBuffers: buffers.as_mut_ptr(),
                };
                // SAFETY: the context is established, and the descriptor points
                // at an array of live buffers whose first one describes
                // `self.incoming`, which is not moved for the length of the
                // call.
                let status = unsafe { DecryptMessage(&self.context, &message, 0, null_mut()) };
                match status {
                    SEC_E_OK | SEC_I_RENEGOTIATE => {
                        let mut plain = Vec::new();
                        let mut extra = Vec::new();
                        for buffer in buffers.iter() {
                            if buffer.pvBuffer.is_null() || buffer.cbBuffer == 0 {
                                continue;
                            }
                            // SAFETY: SChannel rewrote each buffer to point
                            // inside `self.incoming`, which is still alive and
                            // has not been resized.
                            let bytes = unsafe {
                                std::slice::from_raw_parts(
                                    buffer.pvBuffer.cast::<u8>(),
                                    buffer.cbBuffer as usize,
                                )
                            };
                            match buffer.BufferType {
                                SECBUFFER_DATA => plain.extend_from_slice(bytes),
                                SECBUFFER_EXTRA => extra.extend_from_slice(bytes),
                                _ => {}
                            }
                        }
                        self.incoming = extra;
                        self.plaintext = plain;
                        self.at = 0;
                        if status == SEC_I_RENEGOTIATE {
                            // A server asking to renegotiate mid-migration is
                            // refused rather than accommodated: renegotiation
                            // is where a verified session can become a
                            // differently verified one, and no server this
                            // client talks to needs it.
                            return Err(std::io::Error::other(
                                "the server asked to renegotiate the TLS session, which this \
                                 client does not do",
                            ));
                        }
                        return Ok(());
                    }
                    SEC_E_INCOMPLETE_MESSAGE => {}
                    SEC_I_CONTEXT_EXPIRED => {
                        self.finished = true;
                        self.plaintext.clear();
                        self.at = 0;
                        return Ok(());
                    }
                    other => {
                        return Err(std::io::Error::other(format!(
                            "a TLS record could not be decrypted: {}",
                            describe(other)
                        )))
                    }
                }
            }
            let mut scratch = vec![0u8; CHUNK];
            let read = self.socket.read(&mut scratch)?;
            if read == 0 {
                self.finished = true;
                return Ok(());
            }
            self.incoming
                .extend_from_slice(scratch.get(..read).unwrap_or(&[]));
        }
    }
}

impl Write for Session {
    /// Encrypts and writes, one record's worth at a time.
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let most = self.sizes.cbMaximumMessage as usize;
        if most == 0 {
            return Err(std::io::Error::other(
                "the TLS session reports no message size",
            ));
        }
        let taking = input.len().min(most);
        let header = self.sizes.cbHeader as usize;
        let trailer = self.sizes.cbTrailer as usize;
        let mut record = vec![0u8; header.saturating_add(taking).saturating_add(trailer)];
        if let (Some(slot), Some(source)) = (
            record.get_mut(header..header.saturating_add(taking)),
            input.get(..taking),
        ) {
            slot.copy_from_slice(source);
        }

        let base = record.as_mut_ptr();
        let mut buffers = [
            SecBuffer {
                cbBuffer: header as u32,
                BufferType: SECBUFFER_STREAM_HEADER,
                // SAFETY: `base` is the start of a live allocation of at least
                // `header + taking + trailer` bytes.
                pvBuffer: base.cast(),
            },
            SecBuffer {
                cbBuffer: taking as u32,
                BufferType: SECBUFFER_DATA,
                // SAFETY: within the same allocation, `header` bytes in.
                pvBuffer: unsafe { base.add(header) }.cast(),
            },
            SecBuffer {
                cbBuffer: trailer as u32,
                BufferType: SECBUFFER_STREAM_TRAILER,
                // SAFETY: within the same allocation, at its data's end.
                pvBuffer: unsafe { base.add(header.saturating_add(taking)) }.cast(),
            },
            empty(),
        ];
        let message = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 4,
            pBuffers: buffers.as_mut_ptr(),
        };
        // SAFETY: the context is established and every buffer points inside
        // `record`, which is not moved for the length of the call.
        let status = unsafe { EncryptMessage(&self.context, 0, &message, 0) };
        if status != SEC_E_OK {
            return Err(std::io::Error::other(format!(
                "a TLS record could not be encrypted: {}",
                describe(status)
            )));
        }
        // The trailer's length is written back by `EncryptMessage`, and a
        // record sent with the length that was asked for rather than the one
        // produced is a record the server rejects.
        let produced = buffers
            .iter()
            .take(3)
            .map(|buffer| buffer.cbBuffer as usize)
            .fold(0usize, |total, one| total.saturating_add(one));
        self.socket
            .write_all(record.get(..produced).unwrap_or(&record))?;
        Ok(taking)
    }

    /// Flushes the socket the records travel on.
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

/// Returns an empty buffer, which is what SSPI wants in an unused slot.
fn empty() -> SecBuffer {
    SecBuffer {
        cbBuffer: 0,
        BufferType: SECBUFFER_EMPTY,
        pvBuffer: null_mut(),
    }
}

/// Turns a security status into a sentence an operator can act on.
///
/// **The four that matter are named.** "0x80090325" is a number somebody has to
/// search for; "the certificate chains to a root this machine does not trust"
/// is the difference between a stale certificate and an attack, which is the
/// decision the person reading it has to make.
///
/// @param status - what SSPI or the chain policy reported
fn describe(status: i32) -> String {
    match status {
        SEC_E_UNTRUSTED_ROOT | CERT_E_UNTRUSTEDROOT => {
            "it chains to a root this machine does not trust. If the server uses a private \
             authority, name it with sslrootcert=<file>."
                .to_string()
        }
        SEC_E_WRONG_PRINCIPAL | CERT_E_CN_NO_MATCH => {
            "it was issued for a different name than the one this migration connected to. \
             Connect by the name on the certificate."
                .to_string()
        }
        SEC_E_CERT_EXPIRED | CERT_E_EXPIRED => "it has expired, or is not valid yet.".to_string(),
        CRYPT_E_REVOCATION_OFFLINE => {
            "whether it has been revoked could not be checked, because the revocation server \
             could not be reached."
                .to_string()
        }
        SEC_E_ILLEGAL_MESSAGE | SEC_E_INVALID_TOKEN => {
            "the server did not answer with TLS. A server that speaks the protocol in the clear \
             on this port answers a handshake with its own greeting, which is what this looks \
             like."
                .to_string()
        }
        other => format!("SChannel reported 0x{other:08x}"),
    }
}
