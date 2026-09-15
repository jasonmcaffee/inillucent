//! Verified TLS, against a real TLS server.
//!
//! Invariant: **a migration to a host that is not a loopback address either
//! runs over a verified TLS session or does not run.** Three ways of getting
//! that wrong are covered here, and each of them is a way this client used to
//! behave:
//!
//! 1. **Plaintext by default.** A URL that said nothing about transport
//!    connected in the clear and sent a password. The policy cases assert the
//!    refusal and its message, and that a loopback address is the one exception.
//! 2. **A downgrade the server chose.** PostgreSQL answers an `SSLRequest`
//!    with one byte, and a client that fell back on `N` would be a client whose
//!    security is decided by a server setting nobody in the migration can see.
//!    The case here asserts the refusal **and that the server received nothing
//!    but the eight-byte request** - the startup packet, which carries the user
//!    name and the database, was never written.
//! 3. **A certificate nobody checked.** The two verification cases use a real
//!    TLS server with a certificate this client is made to build a chain for,
//!    and assert that an untrusted root and a name mismatch each stop the
//!    connection with the server having seen no credential.
//!
//! ## How the server is built
//!
//! `python` with its `ssl` module, driven from `tests/tls_server.py`. It is a
//! prerequisite this workspace cannot build, so a machine without it makes
//! these cases report that and return; `inillucent-testrun --strict` is what
//! counts a suite that could not run, so a green with no Python is not mistaken
//! for a green with one. The certificates are generated per run into
//! `_agent_output/`, because a certificate checked into a repository expires and
//! then fails a test for a reason that has nothing to do with the code.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use inillucent_remote::{ConnectionUrl, PostgresSource, Transport};

/// Says why a case did not run, and fails the case when the run is strict.
///
/// **The same helper `inillucent_compat::differential::skipping` is, written
/// here because the layering contract will not let a production crate depend
/// on the test harness (task-1932, H10).** Every skip message in the workspace
/// ends with `; skipping`, which is the one marker
/// `tests/inillucent-testing-tdd.md` §9 asks for and the one phrase
/// `inillucent-testrun`'s classifier matches. `INILLUCENT_STRICT`, which
/// `inillucent-testrun --strict` sets, turns the skip into a failure that names
/// the test - which matters most here, because this binary runs other tests,
/// so a skip of its own was invisible to `--strict` by both routes: a CI image
/// without Python's `ssl` module passed the TLS verification suite without
/// running any of it.
///
/// @param reason - what is missing, without the marker
fn skipping(reason: &str) {
    if std::env::var("INILLUCENT_STRICT").is_ok_and(|value| !value.is_empty()) {
        panic!("{reason}; skipping - and this run is strict, so a skip is a failure");
    }
    eprintln!("{reason}; skipping");
}

/// Where the generated certificates and the server's log go.
fn area() -> PathBuf {
    let path = workspace_root().join("_agent_output/tls");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the workspace root.
fn workspace_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

/// Returns the Python interpreter, when one is on the path.
fn python() -> Option<String> {
    for name in ["python", "python3", "py"] {
        let ok = Command::new(name)
            .args(["-c", "import ssl"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok {
            return Some(name.to_string());
        }
    }
    None
}

/// A fake PostgreSQL server that terminates TLS, and what it saw.
struct Server {
    /// The process.
    child: Child,
    /// The port it is listening on.
    port: u16,
}

impl Server {
    /// Starts one and waits for it to say which port it took.
    ///
    /// @param python - the interpreter
    /// @param mode - `accept`, `refuse` or `plain`, which the script reads
    /// @param certificate - the certificate file to present, when it presents one
    /// @param key - its private key
    fn start(python: &str, mode: &str, certificate: &Path, key: &Path) -> Option<Server> {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/tls_server.py");
        let mut child = Command::new(python)
            .arg(&script)
            .arg(mode)
            .arg(certificate)
            .arg(key)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let port: u16 = line.trim().strip_prefix("port ")?.parse().ok()?;
        // The reader is dropped, which leaves the pipe open in the child; the
        // script writes nothing else until it exits.
        Some(Server { child, port })
    }

    /// Stops the server.
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Returns everything the server recorded about what it received.
    fn saw(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut text);
        }
        text
    }
}

/// Generates a certificate authority and a server certificate under one name.
///
/// Returns the authority file, the certificate and the key. `None` when there
/// is no `openssl` to generate them with, which is a skip.
///
/// @param name - what the certificate is issued for, as a SAN
/// @param tag - a directory name, so two cases never share files
fn certificates(name: &str, tag: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let out = area().join(tag);
    let _ = std::fs::create_dir_all(&out);
    let ca_key = out.join("ca.key");
    let ca_certificate = out.join("ca.pem");
    let key = out.join("server.key");
    let certificate = out.join("server.pem");
    let request = out.join("server.csr");
    let extensions = out.join("server.ext");

    let subject_alternative = match name.parse::<std::net::IpAddr>() {
        Ok(_) => format!("IP:{name}"),
        Err(_) => format!("DNS:{name}"),
    };
    std::fs::write(
        &extensions,
        format!("subjectAltName={subject_alternative}\nbasicConstraints=CA:FALSE\n"),
    )
    .ok()?;

    let run = |arguments: &[&std::ffi::OsStr]| -> bool {
        Command::new("openssl")
            .args(arguments)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    let osstr = |text: &str| std::ffi::OsString::from(text);

    // The authority.
    if !run(&[
        osstr("req").as_ref(),
        osstr("-x509").as_ref(),
        osstr("-newkey").as_ref(),
        osstr("rsa:2048").as_ref(),
        osstr("-nodes").as_ref(),
        osstr("-keyout").as_ref(),
        ca_key.as_os_str(),
        osstr("-out").as_ref(),
        ca_certificate.as_os_str(),
        osstr("-days").as_ref(),
        osstr("2").as_ref(),
        osstr("-subj").as_ref(),
        osstr("/CN=inillucent test authority").as_ref(),
    ]) {
        return None;
    }
    // The server's key and request.
    if !run(&[
        osstr("req").as_ref(),
        osstr("-newkey").as_ref(),
        osstr("rsa:2048").as_ref(),
        osstr("-nodes").as_ref(),
        osstr("-keyout").as_ref(),
        key.as_os_str(),
        osstr("-out").as_ref(),
        request.as_os_str(),
        osstr("-subj").as_ref(),
        std::ffi::OsString::from(format!("/CN={name}")).as_ref(),
    ]) {
        return None;
    }
    // Signed by the authority, with the name in a SAN - which is the only
    // place a modern check looks.
    if !run(&[
        osstr("x509").as_ref(),
        osstr("-req").as_ref(),
        osstr("-in").as_ref(),
        request.as_os_str(),
        osstr("-CA").as_ref(),
        ca_certificate.as_os_str(),
        osstr("-CAkey").as_ref(),
        ca_key.as_os_str(),
        osstr("-CAcreateserial").as_ref(),
        osstr("-out").as_ref(),
        certificate.as_os_str(),
        osstr("-days").as_ref(),
        osstr("2").as_ref(),
        osstr("-extfile").as_ref(),
        extensions.as_os_str(),
    ]) {
        return None;
    }
    certificate
        .is_file()
        .then_some((ca_certificate, certificate, key))
}

/// Returns the sentence an engine failure carries, detail first.
///
/// @param error - the failure
fn said(error: inillucent_base::error::DbError) -> String {
    match error.detail() {
        Some(detail) => detail.to_string(),
        None => error.message().to_string(),
    }
}

/// A URL naming a loopback port, with whatever parameters a case wants.
///
/// @param port - the port the fake server took
/// @param parameters - the query string, without the `?`
fn url(port: u16, parameters: &str) -> ConnectionUrl {
    let text = match parameters.is_empty() {
        true => format!("postgres://tester:secret@127.0.0.1:{port}/corpus"),
        false => format!("postgres://tester:secret@127.0.0.1:{port}/corpus?{parameters}"),
    };
    ConnectionUrl::parse(&text).expect("the URL parses")
}

/// A server that refuses TLS does not get a plaintext connection instead.
///
/// **And the assertion is about what the server received**, not only about the
/// return value: a client that refused after writing its startup packet would
/// have leaked the user name and the database, and a check on the error message
/// alone could not tell the two apart.
#[test]
fn a_server_that_refuses_tls_gets_no_credentials() {
    let Some(python) = python() else {
        skipping("transport: no python with an ssl module");
        return;
    };
    let Some((_, certificate, key)) = certificates("127.0.0.1", "refuse") else {
        skipping("transport: no openssl to generate certificates");
        return;
    };
    let Some(mut server) = Server::start(&python, "refuse", &certificate, &key) else {
        skipping("transport: the fake server did not start");
        return;
    };
    let failure =
        PostgresSource::connect_over(&url(server.port, "sslmode=require"), Transport::VerifiedTls)
            .err()
            .map(said)
            .unwrap_or_else(|| "the connection succeeded".to_string());
    let saw = server.saw();
    server.stop();
    assert!(
        failure.contains("refused a TLS connection"),
        "the client did not refuse a server that turned TLS down: {failure}"
    );
    assert!(
        saw.contains("received 8 bytes"),
        "the client wrote more than the SSL request before giving up: {saw}"
    );
    assert!(
        !saw.contains("tester") && !saw.contains("corpus"),
        "the credentials reached a server that had refused TLS: {saw}"
    );
}

/// A certificate signed by an authority this machine does not trust is
/// refused, and the server sees no credential.
#[test]
fn an_untrusted_certificate_is_refused() {
    let Some(python) = python() else {
        skipping("transport: no python with an ssl module");
        return;
    };
    let Some((_, certificate, key)) = certificates("127.0.0.1", "untrusted") else {
        skipping("transport: no openssl to generate certificates");
        return;
    };
    let Some(mut server) = Server::start(&python, "accept", &certificate, &key) else {
        skipping("transport: the fake server did not start");
        return;
    };
    // No `sslrootcert`, so the chain is built against the machine's own store -
    // which has never heard of the authority this test just made.
    let failure =
        PostgresSource::connect_over(&url(server.port, "sslmode=require"), Transport::VerifiedTls)
            .err()
            .map(said)
            .unwrap_or_else(|| "the connection succeeded".to_string());
    let saw = server.saw();
    server.stop();
    assert!(
        failure.contains("did not verify"),
        "an untrusted certificate was accepted: {failure}"
    );
    assert!(
        !saw.contains("tester"),
        "the user name reached a server whose certificate did not verify: {saw}"
    );
}

/// A certificate that chains correctly and names the wrong host is refused.
///
/// This is the case the chain check alone cannot make: the authority is named
/// with `sslrootcert=`, so the chain builds and only the name is wrong. Without
/// a host name check this connection succeeds, which is what makes it the case
/// worth having.
#[test]
fn a_certificate_for_another_name_is_refused() {
    let Some(python) = python() else {
        skipping("transport: no python with an ssl module");
        return;
    };
    let Some((authority, certificate, key)) = certificates("wrong.example", "mismatch") else {
        skipping("transport: no openssl to generate certificates");
        return;
    };
    let Some(mut server) = Server::start(&python, "accept", &certificate, &key) else {
        skipping("transport: the fake server did not start");
        return;
    };
    let parameters = format!(
        "sslmode=require&sslrootcert={}",
        authority.to_string_lossy().replace('\\', "/")
    );
    let failure =
        PostgresSource::connect_over(&url(server.port, &parameters), Transport::VerifiedTls)
            .err()
            .map(said)
            .unwrap_or_else(|| "the connection succeeded".to_string());
    let saw = server.saw();
    server.stop();
    assert!(
        failure.contains("did not verify"),
        "a certificate issued for another name was accepted: {failure}"
    );
    // **For the right reason.** The authority is named, so the chain builds;
    // if this said "untrusted root" the case would be re-testing the previous
    // one and the host name check would be untested.
    assert!(
        failure.contains("different name"),
        "the certificate was refused, but not for its name: {failure}"
    );
    assert!(
        !saw.contains("tester"),
        "the user name reached a server presenting the wrong name: {saw}"
    );
}

/// A certificate that chains to a named authority **and** carries the right
/// name is accepted, and the session is encrypted.
///
/// The case that makes the three refusals above mean something: a client that
/// refused everything would pass all of them.
#[test]
fn a_certificate_that_verifies_is_accepted() {
    let Some(python) = python() else {
        skipping("transport: no python with an ssl module");
        return;
    };
    let Some((authority, certificate, key)) = certificates("127.0.0.1", "accepted") else {
        skipping("transport: no openssl to generate certificates");
        return;
    };
    let Some(mut server) = Server::start(&python, "accept", &certificate, &key) else {
        skipping("transport: the fake server did not start");
        return;
    };
    let parameters = format!(
        "sslmode=require&sslrootcert={}",
        authority.to_string_lossy().replace('\\', "/")
    );
    // The server hangs up after the handshake rather than speaking PostgreSQL,
    // so the connection fails *after* the TLS session is established. What is
    // being asserted is that it got that far: the failure is a protocol one,
    // not a verification one, and the server logged an encrypted handshake.
    let failure =
        PostgresSource::connect_over(&url(server.port, &parameters), Transport::VerifiedTls)
            .err()
            .map(said)
            .unwrap_or_default();
    let saw = server.saw();
    server.stop();
    assert!(
        !failure.contains("did not verify"),
        "a certificate that chains to the named authority and carries the right name was \
         refused: {failure}"
    );
    assert!(
        saw.contains("handshake ok"),
        "the TLS handshake did not complete against a certificate that should verify: \
         {saw}\nclient said: {failure}"
    );
}

/// A plaintext connection carries the whole migration when both halves of the
/// policy say so, which is what keeps the escape hatch usable.
///
/// It also pins the shape of the escape hatch: a server speaking plaintext
/// receives the startup packet, so this is the one case where the credentials
/// legitimately cross an unencrypted socket.
#[test]
fn an_explicit_plaintext_connection_still_works() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().expect("has an address").port();
    let handle = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return Vec::new();
        };
        let mut first = [0u8; 64];
        let read = socket.read(&mut first).unwrap_or(0);
        // An error response, so the client stops rather than waiting.
        let _ = socket.write_all(b"E\x00\x00\x00\x04");
        first.get(..read).unwrap_or(&[]).to_vec()
    });

    let text = format!("postgres://tester:secret@127.0.0.1:{port}/corpus?sslmode=disable");
    let url = ConnectionUrl::parse(&text).expect("parses");
    assert_eq!(
        url.transport(true).expect("permits"),
        Transport::Plaintext,
        "a loopback address with sslmode=disable is plaintext"
    );
    let _ = PostgresSource::connect_over(&url, Transport::Plaintext);
    let received = handle.join().unwrap_or_default();

    // The startup packet, not an SSL request: the first four bytes are a length
    // and the next four are the protocol version 3.0, which is 196608.
    assert!(received.len() >= 8, "nothing reached the server");
    let code = u32::from_be_bytes([
        *received.get(4).unwrap_or(&0),
        *received.get(5).unwrap_or(&0),
        *received.get(6).unwrap_or(&0),
        *received.get(7).unwrap_or(&0),
    ]);
    assert_eq!(
        code, 196_608,
        "a plaintext connection did not send the startup packet first"
    );
}

/// The whole `migrate` entry point refuses before it dials.
///
/// The refusal has to come from the plan rather than from the socket, because
/// a refusal after the connect has already told a server that somebody is
/// trying - and, more usefully, because a plan that is going to be refused
/// should cost nothing.
#[test]
fn a_plan_that_would_go_in_the_clear_is_refused_before_it_dials() {
    // A port nothing is listening on: if the refusal happened after the dial,
    // the message would be about a connection rather than about a policy.
    let url =
        ConnectionUrl::parse("postgres://tester:secret@db.example:5432/corpus?sslmode=disable")
            .expect("parses");
    let plan = inillucent_remote::Plan::new(url, area().join("never-written.rdb"));
    let refused = plan.transport().expect_err("refuses");
    assert!(
        refused.message().contains("--insecure-plaintext"),
        "{}",
        refused.message()
    );
    assert!(
        !area().join("never-written.rdb").exists(),
        "a refused plan created its destination"
    );
}

/// The refusal a machine with no TLS gets names what to do about it.
///
/// It cannot be provoked on a machine that *has* TLS, so what is checked is the
/// pair: either an implementation is reported, or the sentence a caller would
/// see names both ways forward.
#[test]
fn a_machine_with_no_tls_says_so_rather_than_falling_back() {
    match inillucent_remote::tls::available() {
        Some(name) => assert!(!name.is_empty(), "the TLS backend has no name"),
        None => {
            let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
            let port = listener.local_addr().expect("has an address").port();
            let socket = TcpStream::connect(("127.0.0.1", port)).expect("connects");
            let refused = match inillucent_remote::tls::connect(socket, "db.example", None) {
                Ok(_) => panic!("a machine that reports no TLS backend built a session"),
                Err(refused) => refused,
            };
            assert!(refused.message().contains("--insecure-plaintext"));
        }
    }
}

/// A path is only a path; this keeps the unused import honest.
#[allow(dead_code)]
fn unused(_: &Path) {}

/// A refusal names the host and carries no part of the URL that reached it.
///
/// **The one message an operator reads while deciding whether they are being
/// attacked (task-1962, T4).** It is also the message that gets pasted into a
/// ticket, a chat and a log aggregator, and the URL it came from carries the
/// password. `tls::not_verified` is written to name the host and the platform's
/// reason and nothing else; this is the assertion that it stays that way,
/// because a later change adding `{url}` to the format string would be a
/// credential leak that every other test in this file passes.
///
/// The host is asserted too. A message with everything redacted tells the
/// operator nothing, and passing this test by saying less would be the wrong
/// fix.
#[test]
fn a_refusal_names_the_host_and_not_the_password() {
    let Some(python) = python() else {
        skipping("transport: no python with an ssl module");
        return;
    };
    let Some((_, certificate, key)) = certificates("127.0.0.1", "redaction") else {
        skipping("transport: no openssl to generate certificates");
        return;
    };
    let Some(mut server) = Server::start(&python, "accept", &certificate, &key) else {
        skipping("transport: the fake server did not start");
        return;
    };
    // The same untrusted certificate the case above uses, so this fails for a
    // reason that is already understood and the only new question is the words.
    let error =
        PostgresSource::connect_over(&url(server.port, "sslmode=require"), Transport::VerifiedTls)
            .err()
            .expect("an untrusted certificate is refused");
    server.stop();

    // Every rendering a caller can reach: the detail, the message, and the
    // `Display` a caller that does neither will print.
    let renderings = [
        error.detail().unwrap_or_default().to_string(),
        error.message().to_string(),
        format!("{error}"),
    ];
    for rendering in &renderings {
        assert!(
            !rendering.contains("secret"),
            "the password reached the refusal: {rendering}"
        );
        assert!(
            !rendering.contains("tester"),
            "the user name reached the refusal: {rendering}"
        );
        assert!(
            !rendering.contains("postgres://"),
            "the URL reached the refusal, and it carries the password: {rendering}"
        );
    }
    let detail = said(error);
    assert!(
        detail.contains("127.0.0.1"),
        "the refusal should name the host whose certificate failed, or the          operator cannot tell which endpoint it was about: {detail}"
    );
    assert!(
        detail.contains("did not verify"),
        "and should say what went wrong: {detail}"
    );
}
