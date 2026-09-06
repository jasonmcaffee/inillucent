//! A JSON POST to a process on this machine.
//!
//! Sixty lines of HTTP/1.1 rather than a client crate, because the only thing
//! this harness talks to over a socket is a `llama-server` on loopback: no TLS,
//! no redirects, no authentication, no proxy. The dependency policy asks that
//! every third-party crate be infrastructure the engine genuinely needs, and a
//! full HTTP client pulled in so a benchmark can reach 127.0.0.1 is not that.
//!
//! Both framings a `llama-server` reply can use are handled, because it picks
//! between them: `Content-Length` for a short body and chunked transfer encoding
//! for a long one, and an embedding response for a batch is long.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result};

/// POST a JSON body and return the response body as a string.
///
/// @param host - the host, e.g. `127.0.0.1`
/// @param port - the port the server listens on
/// @param path - the request path, e.g. `/v1/embeddings`
/// @param body - the request body, already JSON
/// @param timeout - how long to wait for the reply; an embedding batch on a busy
///   card is slow, and a short timeout here reads as a dead server
pub fn post_json(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<String> {
    let mut stream = TcpStream::connect((host, port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).context("reading the status line")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("the reply did not start with a status: {status_line:?}"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }

    let body = if chunked {
        read_chunked(&mut reader)?
    } else if let Some(n) = content_length {
        let mut buf = vec![0u8; n];
        reader.read_exact(&mut buf)?;
        buf
    } else {
        // No framing at all: the server said `Connection: close` and will end the
        // body by closing, which is what HTTP/1.0 did and what a few builds still
        // do for an error page.
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        buf
    };
    let body = String::from_utf8_lossy(&body).into_owned();

    anyhow::ensure!(
        (200..300).contains(&status),
        "{host}:{port}{path} answered {status}: {}",
        body.chars().take(600).collect::<String>()
    );
    Ok(body)
}

/// Read a chunked body: a hexadecimal length, the bytes, repeat until zero.
fn read_chunked(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let size_text = size_line.trim();
        if size_text.is_empty() {
            continue;
        }
        // A chunk size may carry extensions after a semicolon; nothing here uses
        // them, but they are legal and would otherwise fail the parse.
        let size_text = size_text.split(';').next().unwrap_or(size_text);
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("{size_text:?} is not a chunk size"))?;
        if size == 0 {
            break;
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        // The CRLF that follows every chunk.
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(out)
}

/// GET a path, for a health check.
/// @param host - the host
/// @param port - the port
/// @param path - the request path
/// @param timeout - how long to wait
pub fn get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<String> {
    let mut stream = TcpStream::connect((host, port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut buf = Vec::new();
    BufReader::new(stream).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Serve one canned reply on a loopback port, so the parsing can be tested
    /// without a model server.
    fn serve_once(reply: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                // Drain the request so the client's write does not block.
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                let mut length = 0usize;
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let trimmed = line.trim_end().to_string();
                    if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                    if trimmed.is_empty() {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let _ = socket.write_all(reply);
                let _ = socket.flush();
            }
        });
        port
    }

    #[test]
    fn a_content_length_body_is_read_whole() {
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"data\":[1,2,3]}\n",
        );
        let got =
            post_json("127.0.0.1", port, "/v1/embeddings", "{}", Duration::from_secs(5)).unwrap();
        assert_eq!(got, "{\"data\":[1,2,3]}\n");
    }

    /// The framing a real embedding batch comes back under, and the one a naive
    /// reader gets wrong: it would return the hexadecimal sizes as part of the
    /// JSON and the parse would fail somewhere far from here.
    #[test]
    fn a_chunked_body_is_reassembled() {
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              9\r\n{\"data\":[\r\n7\r\n1,2,3]}\r\n0\r\n\r\n",
        );
        let got =
            post_json("127.0.0.1", port, "/v1/embeddings", "{}", Duration::from_secs(5)).unwrap();
        assert_eq!(got, "{\"data\":[1,2,3]}");
    }

    /// A refusal has to arrive as an error carrying the server's own message.
    /// `llama-server` answers 500 when a batch exceeds its physical batch size,
    /// and that message is the only thing that says so.
    #[test]
    fn a_failure_status_becomes_an_error_carrying_the_body() {
        let port = serve_once(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 53\r\n\r\n\
              {\"error\":{\"message\":\"input is too large to process\"}}",
        );
        let err = post_json("127.0.0.1", port, "/v1/embeddings", "{}", Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("answered 500"), "{err}");
        assert!(err.contains("too large to process"), "{err}");
    }
}
