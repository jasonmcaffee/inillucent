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

/// A connected socket with a small read buffer.
pub struct Stream {
    /// The socket itself.
    socket: TcpStream,
    /// Bytes read from the socket and not yet handed out.
    buffer: Vec<u8>,
    /// How far into `buffer` the reader has got.
    at: usize,
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
            socket,
            buffer: Vec::new(),
            at: 0,
        })
    }

    /// Wraps an already-connected socket, which is what the tests hand it.
    ///
    /// @param socket - a connected stream
    pub fn from_socket(socket: TcpStream) -> Stream {
        Stream {
            socket,
            buffer: Vec::new(),
            at: 0,
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

    /// Shuts the socket down in both directions, ignoring a failure.
    ///
    /// A close that fails has nothing left to protect: the caller is finished
    /// with the connection either way, and the operating system will reclaim it.
    pub fn close(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
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
