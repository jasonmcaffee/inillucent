//! The socket, and the bounded reading both protocols are written against.
//!
//! Two properties everything above this file depends on:
//!
//! - **Every length is checked before it is allocated.** Both protocols put a
//!   length in front of a payload, so a server - or something pretending to be
//!   one - can ask this process to reserve four gigabytes by writing four
//!   bytes. `read_exact` refuses anything over [`MAX_MESSAGE`] and names the
//!   number it was asked for.
//! - **A short read is an error, never a shorter message.** A stream that ends
//!   mid-row has to fail loudly: the alternative is a migration that publishes
//!   a table it only read half of.
//!
//! Invariant: **every length is checked before it is allocated, and every read
//! is bounded.** Both protocols put a length on the wire ahead of the bytes it
//! describes, which makes it a number the peer chooses; this file is the one
//! place that number turns into a buffer, so it is the one place the bound has
//! to hold.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use inillucent_base::error::{corrupt, refusal, DbError, PrimaryCode};
use inillucent_base::DbResult;

/// The largest single protocol message this client will hold.
///
/// 256 MiB, which is well above any row either server will send in one message
/// - MySQL's own packet ceiling is 16 MiB before it splits, and PostgreSQL's
/// `DataRow` is bounded by the row - and well below a length that would let a
/// bad four bytes exhaust the machine.
pub const MAX_MESSAGE: usize = 256 * 1024 * 1024;

/// What the bytes actually travel on.
///
/// **The upgrade happens in place, part way through the connection.** Both
/// protocols open in the clear, ask the server for TLS in their own way and
/// then continue on the same socket, so a `Stream` has to be able to become
/// encrypted without the reader above it knowing - and without a buffered byte
/// being left on the plaintext side, which is why [`Stream::upgrade`] refuses
/// while anything is buffered.
pub enum Transport {
    /// An unencrypted socket.
    Plain(TcpStream),
    /// A verified TLS session over one.
    Secure(Box<crate::tls::Session>),
    /// Neither, which is what a stream holds for the length of an upgrade and
    /// after one that failed.
    ///
    /// It exists because moving the socket out to hand it to the TLS layer
    /// leaves a hole, and a hole with a plausible-looking socket in it would be
    /// a stream that reads plaintext after a handshake this client refused. A
    /// stream in this state fails every read and write by name; the callers all
    /// abandon it, and this is what makes abandoning it safe.
    Broken,
}

impl Read for Transport {
    /// Reads from whichever transport this is.
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(socket) => socket.read(output),
            Transport::Secure(session) => session.read(output),
            Transport::Broken => Err(std::io::Error::other(
                "this connection was abandoned part way through a TLS upgrade",
            )),
        }
    }
}

impl Write for Transport {
    /// Writes to whichever transport this is.
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(socket) => socket.write(input),
            Transport::Secure(session) => session.write(input),
            Transport::Broken => Err(std::io::Error::other(
                "this connection was abandoned part way through a TLS upgrade",
            )),
        }
    }

    /// Flushes whichever transport this is.
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(socket) => socket.flush(),
            Transport::Secure(session) => session.flush(),
            Transport::Broken => Ok(()),
        }
    }
}

/// A connected socket with a small read buffer.
pub struct Stream {
    /// What the bytes travel on.
    socket: Transport,
    /// Bytes read from the socket and not yet handed out.
    buffer: Vec<u8>,
    /// How far into `buffer` the reader has got.
    at: usize,
    /// What the peer proved it was, when the session is encrypted.
    peer: Option<String>,
}

impl Stream {
    /// Dials a server and returns the connected stream.
    ///
    /// @param address - `host:port`, already bracketed if the host is IPv6
    /// @param timeout - how long to wait for the connection and for each read
    pub fn connect(address: &str, timeout: Duration) -> DbResult<Stream> {
        let resolved = address
            .to_socket_addrs()
            .map_err(|error| cannot_open(format!("{address} could not be resolved: {error}")))?
            .next()
            .ok_or_else(|| cannot_open(format!("{address} resolved to no address")))?;
        let socket = TcpStream::connect_timeout(&resolved, timeout)
            .map_err(|error| cannot_open(format!("{address} could not be reached: {error}")))?;
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|error| cannot_open(format!("{address}: {error}")))?;
        socket
            .set_write_timeout(Some(timeout))
            .map_err(|error| cannot_open(format!("{address}: {error}")))?;
        // Nagle would hold a small handshake packet waiting for company that is
        // never coming, because the next thing this client does is wait for the
        // server's reply to it.
        let _ = socket.set_nodelay(true);
        Ok(Stream {
            socket: Transport::Plain(socket),
            buffer: Vec::new(),
            at: 0,
            peer: None,
        })
    }

    /// Replaces the plaintext socket with a verified TLS session over it.
    ///
    /// **Refused while anything is buffered.** A byte read from the plaintext
    /// socket and held here would be a byte the TLS session never sees, and the
    /// record stream would be off by that much for the rest of the connection.
    /// Both protocols call this immediately after a one-byte or one-packet
    /// reply, with nothing outstanding, so the refusal is a guard rather than a
    /// limitation.
    ///
    /// @param host - the name to check the certificate against
    /// @param root - a certificate authority file, when the URL named one
    pub fn upgrade(&mut self, host: &str, root: Option<&str>) -> DbResult<()> {
        if self.at < self.buffer.len() {
            return Err(protocol(
                "the server sent bytes after agreeing to TLS and before the handshake, which \
                 this client will not read",
            ));
        }
        let taken = std::mem::replace(&mut self.socket, Transport::Broken);
        let socket = match taken {
            Transport::Plain(socket) => socket,
            other => {
                self.socket = other;
                return Err(protocol("this connection is already encrypted"));
            }
        };
        // A failure leaves the stream `Broken` on purpose. The socket is gone -
        // the TLS layer owns it and drops it - and putting anything else here
        // would be putting back a connection whose handshake this client
        // refused.
        let session = crate::tls::connect(socket, host, root)?;
        self.peer = Some(session.description());
        self.socket = Transport::Secure(Box::new(session));
        self.buffer.clear();
        self.at = 0;
        Ok(())
    }

    /// Returns what the peer proved it was, when this connection is encrypted.
    pub fn peer(&self) -> Option<&str> {
        self.peer.as_deref()
    }

    /// Reports whether this connection is encrypted.
    pub fn is_encrypted(&self) -> bool {
        matches!(self.socket, Transport::Secure(_))
    }

    /// Wraps an already-connected socket, which is what the tests hand it.
    ///
    /// @param socket - a connected stream
    pub fn from_socket(socket: TcpStream) -> Stream {
        Stream {
            socket: Transport::Plain(socket),
            buffer: Vec::new(),
            at: 0,
            peer: None,
        }
    }

    /// Writes a whole message and flushes it.
    ///
    /// @param bytes - the framed message
    pub fn write_all(&mut self, bytes: &[u8]) -> DbResult<()> {
        self.socket.write_all(bytes).map_err(|error| {
            cannot_open(format!("the connection could not be written: {error}"))
        })?;
        self.socket
            .flush()
            .map_err(|error| cannot_open(format!("the connection could not be flushed: {error}")))
    }

    /// Reads exactly `count` bytes, refusing an implausible length.
    ///
    /// @param count - how many bytes the message's own header promised
    pub fn read_exact(&mut self, count: usize) -> DbResult<Vec<u8>> {
        if count > MAX_MESSAGE {
            return Err(corrupt(format!(
                "the server announced a {count}-byte message, over the {MAX_MESSAGE}-byte ceiling \
                 this client will hold; the stream is not this protocol"
            )));
        }
        let mut out = vec![0u8; count];
        let mut filled = 0usize;
        while filled < count {
            // Anything already buffered first, then straight into the caller's
            // vector - there is no reason to copy a megabyte row twice.
            if self.at < self.buffer.len() {
                let available = self.buffer.len().saturating_sub(self.at);
                let taking = available.min(count.saturating_sub(filled));
                let (from, to) = (self.at, self.at.saturating_add(taking));
                let source = self.buffer.get(from..to).unwrap_or(&[]).to_vec();
                if let Some(slot) = out.get_mut(filled..filled.saturating_add(taking)) {
                    slot.copy_from_slice(&source);
                }
                self.at = to;
                filled = filled.saturating_add(taking);
                continue;
            }
            let Some(slot) = out.get_mut(filled..) else {
                break;
            };
            match self.socket.read(slot) {
                Ok(0) => {
                    return Err(corrupt(format!(
                        "the server closed the connection after {filled} of {count} bytes"
                    )))
                }
                Ok(read) => filled = filled.saturating_add(read),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(cannot_open(format!(
                        "the connection failed after {filled} of {count} bytes: {error}"
                    )))
                }
            }
        }
        Ok(out)
    }

    /// Reads one byte.
    pub fn read_u8(&mut self) -> DbResult<u8> {
        Ok(*self.read_exact(1)?.first().unwrap_or(&0))
    }

    /// Returns the next byte without consuming it.
    ///
    /// MySQL's result sets are told apart by their first byte, and a reader that
    /// consumed it to look would have to put it back.
    pub fn peek_u8(&mut self) -> DbResult<u8> {
        if self.at >= self.buffer.len() {
            let mut scratch = [0u8; 4096];
            let read = match self.socket.read(&mut scratch) {
                Ok(0) => {
                    return Err(corrupt(
                        "the server closed the connection where a message was expected",
                    ))
                }
                Ok(read) => read,
                Err(error) => {
                    return Err(cannot_open(format!("the connection failed: {error}")));
                }
            };
            self.buffer = scratch.get(..read).unwrap_or(&[]).to_vec();
            self.at = 0;
        }
        Ok(*self.buffer.get(self.at).unwrap_or(&0))
    }

    /// Reads whatever is available, up to the size of the caller's buffer.
    ///
    /// [`read_exact`](Self::read_exact) is the right shape for a protocol whose
    /// messages carry their own length, which is both of the ones this crate
    /// speaks. A body being streamed to a file is the other shape: the caller
    /// wants to write out whatever has arrived rather than wait for a count it
    /// was never told. Zero means the peer closed, which is a legitimate end of
    /// a body that had no length in front of it.
    ///
    /// @param out - where the bytes go
    pub fn read_some(&mut self, out: &mut [u8]) -> DbResult<usize> {
        if self.at < self.buffer.len() {
            let available = self.buffer.len().saturating_sub(self.at);
            let taking = available.min(out.len());
            let (from, to) = (self.at, self.at.saturating_add(taking));
            let source = self.buffer.get(from..to).unwrap_or(&[]).to_vec();
            if let Some(slot) = out.get_mut(..taking) {
                slot.copy_from_slice(&source);
            }
            self.at = to;
            return Ok(taking);
        }
        loop {
            return match self.socket.read(out) {
                Ok(read) => Ok(read),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => Err(cannot_open(format!("the connection failed: {error}"))),
            };
        }
    }

    /// Reads one line ending in CRLF, without the terminator.
    ///
    /// **Bounded**, because a status line or a header from something that is not
    /// a web server is a stream with no newline in it, and a reader with no
    /// ceiling would hold the whole of it. The bound is named in the refusal so
    /// a reader can tell "this is not HTTP" from "this header is unusually long".
    ///
    /// A bare LF is accepted as a terminator as well as CRLF: the specification
    /// asks for CRLF and a small number of real servers send LF, and refusing
    /// those would be strictness nobody benefits from.
    ///
    /// @param max - the longest line this will hold
    pub fn read_line(&mut self, max: usize) -> DbResult<String> {
        let mut line: Vec<u8> = Vec::new();
        loop {
            // Peeking is what fills the buffer, so the byte-at-a-time read below
            // is a buffer index rather than a socket call.
            self.peek_u8()?;
            let byte = self.read_u8()?;
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }
            if line.len() >= max {
                return Err(protocol(format!(
                    "a line ran past {max} bytes with no end to it; this is not an HTTP response"
                )));
            }
            line.push(byte);
        }
    }

    /// Shuts the socket down in both directions, ignoring a failure.
    ///
    /// A close that fails has nothing left to protect: the caller is finished
    /// with the connection either way, and the operating system will reclaim it.
    pub fn close(&mut self) {
        match &mut self.socket {
            Transport::Plain(socket) => {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            Transport::Secure(session) => session.shutdown(),
            Transport::Broken => {}
        }
    }
}

/// Builds the error a connection failure raises.
///
/// `CantOpen` rather than `Corrupt`: nothing on a disk is wrong, a socket did
/// not open or did not stay open.
///
/// @param detail - what happened, naming the address but never a password
pub fn cannot_open(detail: impl Into<String>) -> DbError {
    let detail = detail.into();
    DbError::primary(PrimaryCode::CantOpen)
        .with_message(detail.clone())
        .with_detail(detail)
}

/// Builds the error a protocol violation raises.
///
/// @param detail - what the server sent and what was expected
pub fn protocol(detail: impl Into<String>) -> DbError {
    corrupt(detail)
}

/// Builds the error a refusal raises, for something this client will not do.
///
/// @param said - the sentence a person reads
pub fn will_not(said: impl Into<String>) -> DbError {
    refusal(said)
}

/// Reads a big-endian `u32` out of a slice at an offset.
///
/// @param bytes - the message
/// @param at - where the field starts
pub fn be_u32(bytes: &[u8], at: usize) -> DbResult<u32> {
    let slice = bytes
        .get(at..at.saturating_add(4))
        .ok_or_else(|| protocol(format!("a 4-byte field at {at} runs past the message")))?;
    Ok(u32::from_be_bytes([
        *slice.first().unwrap_or(&0),
        *slice.get(1).unwrap_or(&0),
        *slice.get(2).unwrap_or(&0),
        *slice.get(3).unwrap_or(&0),
    ]))
}

/// Reads a big-endian `i32` out of a slice at an offset.
///
/// @param bytes - the message
/// @param at - where the field starts
pub fn be_i32(bytes: &[u8], at: usize) -> DbResult<i32> {
    Ok(be_u32(bytes, at)? as i32)
}

/// Reads a big-endian `u16` out of a slice at an offset.
///
/// @param bytes - the message
/// @param at - where the field starts
pub fn be_u16(bytes: &[u8], at: usize) -> DbResult<u16> {
    let slice = bytes
        .get(at..at.saturating_add(2))
        .ok_or_else(|| protocol(format!("a 2-byte field at {at} runs past the message")))?;
    Ok(u16::from_be_bytes([
        *slice.first().unwrap_or(&0),
        *slice.get(1).unwrap_or(&0),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bounds check is on the announced length, before any allocation, so
    /// a four-byte lie cannot cost a gigabyte.
    #[test]
    fn an_over_large_message_is_refused_by_its_announced_length() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("has an address");
        let handle = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let socket = TcpStream::connect(address).expect("connects");
        let mut stream = Stream::from_socket(socket);
        let error = stream
            .read_exact(MAX_MESSAGE.saturating_add(1))
            .expect_err("refuses");
        assert!(error.detail().unwrap_or_default().contains("ceiling"));
        let _ = handle.join();
    }

    /// Fields are read by bounds-checked accessors, so a truncated message is
    /// an error rather than a panic.
    #[test]
    fn a_field_past_the_end_of_a_message_is_an_error() {
        assert!(be_u32(&[0, 0, 0], 0).is_err());
        assert!(be_u16(&[0], 0).is_err());
        assert_eq!(be_u32(&[0, 0, 1, 0], 0).unwrap_or(0), 256);
        assert_eq!(be_i32(&[0xff, 0xff, 0xff, 0xff], 0).unwrap_or(0), -1);
    }
}
