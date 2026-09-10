//! The wire clients, driven against **recorded transcripts of real servers**,
//! with no server installed.
//!
//! `tests/fixtures/*.transcript` are byte-for-byte recordings of complete
//! migrations - handshake, catalog, cursor, counts, commit - taken through
//! `tools/record-wire-transcript.py` sitting between this client and a real
//! PostgreSQL 17.2 and a real MySQL 8.4.6. Each line is `S <hex>` or `C <hex>`:
//! what the server sent, and what the client sent.
//!
//! Two things that makes possible, and they are the reason this file exists
//! rather than a handful of hand-built packets:
//!
//! 1. **The suite runs on a clone with nothing installed.** The live suites
//!    (`live_postgres.rs`, `live_mysql.rs`) are the acceptance tests and they
//!    skip without a server. These do not skip.
//! 2. **The client's own bytes are asserted**, not just its tolerance of the
//!    server's. Every `C` record is concatenated and compared against what the
//!    client writes during the replay, so a startup packet with the wrong
//!    length, an `md5` response computed over the wrong salt, or a MySQL
//!    capability flag that quietly changed fails *here* - rather than at
//!    somebody's server, where it reads as "your password is wrong".
//!
//! That comparison is only meaningful because these three exchanges are
//! deterministic: the client's reply is a function of the server's challenge,
//! which comes out of the transcript. SCRAM is not - its client nonce is fresh
//! every time by design - so it is covered by a fake server further down that
//! *computes* the exchange rather than replaying one, and by `live_postgres.rs`
//! against a real `scram-sha-256` instance.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use inillucent_base::hash::sha256;
use inillucent_engine::connect::Database;
use inillucent_remote::auth::{base64_decode, base64_encode, hmac_sha256, pbkdf2_sha256, xor};
use inillucent_remote::migrate::{run, Plan};
use inillucent_remote::{ConnectionUrl, MysqlSource, PostgresSource, RemoteSource, Stream};
use inillucent_tree::datum::OwnedDatum;

/// A recorded exchange: what each side sent, in order.
struct Transcript {
    /// Everything the server sent, concatenated.
    server: Vec<u8>,
    /// Everything the client sent, concatenated.
    client: Vec<u8>,
}

/// Reads one transcript file.
///
/// @param name - the file's name under `tests/fixtures`
fn transcript(name: &str) -> Transcript {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
    let mut server = Vec::new();
    let mut client = Vec::new();
    for line in text.lines() {
        let Some((tag, hex)) = line.split_once(' ') else {
            continue;
        };
        let bytes = decode_hex(hex);
        match tag {
            "S" => server.extend_from_slice(&bytes),
            "C" => client.extend_from_slice(&bytes),
            _ => {}
        }
    }
    Transcript { server, client }
}

/// Returns the bytes a hexadecimal string spells.
///
/// @param text - the hex, with no separators
fn decode_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u32> = text
        .chars()
        .filter_map(|digit| digit.to_digit(16))
        .collect();
    digits
        .chunks_exact(2)
        .map(|pair| ((pair[0] << 4) | pair[1]) as u8)
        .collect()
}

/// Serves one recorded server side over loopback and collects the client's.
///
/// **The whole server side is written up front**, in one thread, while a second
/// drains what the client writes. Both protocols are strictly request and
/// response, and the client reads only what it needs, so a server that writes
/// ahead is indistinguishable from one that answers each request - and it
/// removes any need for this harness to understand where one message ends.
///
/// @param recorded - what the server said
fn serve(recorded: Vec<u8>) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds a loopback port");
    let address = listener.local_addr().expect("has an address").to_string();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((socket, _)) = listener.accept() else {
            let _ = sender.send(Vec::new());
            return;
        };
        let writing = socket.try_clone().expect("clones the socket");
        let writer = std::thread::spawn(move || {
            let mut writing = writing;
            let _ = writing.write_all(&recorded);
            let _ = writing.flush();
            // **Half-close when the recording runs out.** A real server that
            // has nothing more to say has hung up, and the difference matters:
            // a client waiting for a message that is not coming should see the
            // end of the stream and say so, not sit on the socket until a
            // timeout turns a protocol defect into "the host did not respond".
            let _ = writing.shutdown(std::net::Shutdown::Write);
        });
        let mut reading = socket;
        let mut said = Vec::new();
        let mut scratch = [0u8; 8192];
        while let Ok(read) = reading.read(&mut scratch) {
            if read == 0 {
                break;
            }
            said.extend_from_slice(scratch.get(..read).unwrap_or_default());
        }
        let _ = writer.join();
        let _ = sender.send(said);
    });
    (address, receiver)
}

/// Connects to a fake server with a read and write timeout on the socket.
///
/// **Every socket in this file is bounded.** A protocol mistake in the client -
/// waiting for a message the server was never going to send - is a *hang*, and
/// an unbounded hang in a test suite is a build that never finishes rather than
/// a test that fails. Ten seconds is far longer than a loopback exchange needs
/// and far shorter than a person's patience.
///
/// @param address - the fake server's `host:port`
fn dial(address: &str) -> Stream {
    let socket = TcpStream::connect(address).expect("connects to the fake server");
    let bound = std::time::Duration::from_secs(10);
    socket
        .set_read_timeout(Some(bound))
        .expect("sets a read timeout");
    socket
        .set_write_timeout(Some(bound))
        .expect("sets a write timeout");
    Stream::from_socket(socket)
}

/// Returns a scratch path nothing else is using.
///
/// @param name - what to call it
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inillucent-protocol-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    path
}

/// Removes a migration's output and its report.
///
/// @param destination - the published path
fn clean(destination: &Path) {
    let _ = std::fs::remove_file(destination);
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(destination.with_file_name(format!("{name}.migration-report.md")));
    let _ = std::fs::remove_file(destination.with_file_name(format!(".{name}.staging")));
}

/// Reports where two byte strings first differ, for a readable failure.
///
/// @param produced - what the client wrote during the replay
/// @param recorded - what it wrote when a real server accepted it
fn first_difference(produced: &[u8], recorded: &[u8]) -> String {
    for at in 0..produced.len().max(recorded.len()) {
        let (left, right) = (produced.get(at), recorded.get(at));
        if left != right {
            let window = at.saturating_sub(8);
            return format!(
                "byte {at}: replay sent {left:?}, the recording sent {right:?}\n  replay:    {}\n  recording: {}",
                produced
                    .get(window..(at + 8).min(produced.len()))
                    .map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
                    .unwrap_or_default(),
                recorded
                    .get(window..(at + 8).min(recorded.len()))
                    .map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
                    .unwrap_or_default(),
            );
        }
    }
    "no difference".to_string()
}

/// Replays one PostgreSQL transcript through a whole migration.
///
/// @param file - the transcript
/// @param url - the URL whose credentials answer its challenge
/// @param name - what to call the scratch database
fn replay_postgres(file: &str, url: &str, name: &str) -> inillucent_remote::Report {
    let recorded = transcript(file);
    let (address, said) = serve(recorded.server);
    let parsed = ConnectionUrl::parse(&url.replace("HOSTPORT", &address)).expect("parses");
    let mut source = PostgresSource::over(dial(&address), &parsed).expect("the handshake runs");
    let destination = scratch(name);
    let report = run(&Plan::new(parsed, &destination), &mut source).expect("the migration runs");
    source.finish();
    let produced = said.recv().unwrap_or_default();
    assert_eq!(
        produced,
        recorded.client,
        "the client's own bytes changed since the recording. {}",
        first_difference(&produced, &recorded.client)
    );
    clean(&destination);
    report
}

/// A whole migration over a recorded `trust` login, with no server installed.
#[test]
fn a_recorded_postgres_trust_migration_replays_byte_for_byte() {
    let report = replay_postgres(
        "postgres-trust.transcript",
        "postgres://postgres@HOSTPORT/inillucent_migrate_test",
        "pg-trust",
    );
    assert!(report.passed(), "{:?}", report.failures());
    assert_eq!(report.server, "PostgreSQL 17.2");
    assert_eq!(report.tables.len(), 6);
    assert_eq!(report.rows(), 13);
}

/// The same over a recorded `md5` login. **This is the one that pins the hash**:
/// the salt comes out of the transcript, so the response the client computes is
/// deterministic, and a wrong MD5 fails the byte comparison rather than looking
/// like somebody's password being wrong.
#[test]
fn a_recorded_postgres_md5_login_replays_byte_for_byte() {
    let report = replay_postgres(
        "postgres-md5.transcript",
        "postgres://md5user:md5-secret@HOSTPORT/inillucent_migrate_test",
        "pg-md5",
    );
    assert!(report.passed(), "{:?}", report.failures());
    assert_eq!(report.tables.len(), 6);
}

/// The values a replayed migration produces are the ones a live one produced,
/// read back out of the published file.
#[test]
fn a_replayed_migration_publishes_the_same_values_a_live_one_did() {
    let recorded = transcript("postgres-trust.transcript");
    let (address, said) = serve(recorded.server);
    let url = ConnectionUrl::parse(&format!(
        "postgres://postgres@{address}/inillucent_migrate_test"
    ))
    .expect("parses");
    let mut source = PostgresSource::over(dial(&address), &url).expect("the handshake runs");
    let destination = scratch("pg-values");
    let report = run(&Plan::new(url, &destination), &mut source).expect("the migration runs");
    source.finish();
    let _ = said.recv();
    assert!(report.passed(), "{:?}", report.failures());

    let database = Database::open(&destination).expect("the published database opens");
    let connection = database.connect();
    let rows = connection
        .query("SELECT price FROM note WHERE id = 1")
        .expect("the query runs");
    assert_eq!(
        rows.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Text(
            b"12345678901234567890.1234567890".to_vec()
        )),
        "the wide numeric keeps every digit it had"
    );
    let payload = connection
        .query("SELECT payload FROM note WHERE id = 2")
        .expect("the query runs");
    assert_eq!(
        payload.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Blob(vec![0xde, 0xad, 0xbe, 0xef])),
        "bytea arrives as bytes rather than as the text \\xdeadbeef"
    );
    let _ = connection;
    drop(database);
    clean(&destination);
}

/// A whole migration over a recorded `mysql_native_password` login. The
/// scramble comes out of the transcript, so this pins the SHA-1 exchange the
/// same way the md5 case pins MD5.
#[test]
fn a_recorded_mysql_native_password_migration_replays_byte_for_byte() {
    let recorded = transcript("mysql-native.transcript");
    let (address, said) = serve(recorded.server);
    let url = ConnectionUrl::parse(&format!(
        "mysql://nativeuser:native-secret@{address}/inillucent_migrate_test"
    ))
    .expect("parses");
    let mut source = MysqlSource::over(dial(&address), &url).expect("the handshake runs");
    let destination = scratch("my-native");
    let report = run(&Plan::new(url, &destination), &mut source).expect("the migration runs");
    source.finish();
    let produced = said.recv().unwrap_or_default();
    assert_eq!(
        produced,
        recorded.client,
        "the client's own bytes changed since the recording. {}",
        first_difference(&produced, &recorded.client)
    );
    assert!(report.passed(), "{:?}", report.failures());
    assert!(report.server.starts_with("MySQL 8.4"), "{}", report.server);
    assert_eq!(report.tables.len(), 4);
    assert_eq!(report.rows(), 10);
    clean(&destination);
}

/// An account with **no** password answers with an empty response rather than
/// with the hash of an empty string, and a real server accepted that - which is
/// what this recording is.
#[test]
fn a_recorded_mysql_passwordless_login_replays_byte_for_byte() {
    let recorded = transcript("mysql-empty-password.transcript");
    let (address, said) = serve(recorded.server);
    let url = ConnectionUrl::parse(&format!("mysql://root@{address}/inillucent_migrate_test"))
        .expect("parses");
    let mut source = MysqlSource::over(dial(&address), &url).expect("the handshake runs");
    let destination = scratch("my-root");
    let report = run(&Plan::new(url, &destination), &mut source).expect("the migration runs");
    source.finish();
    let produced = said.recv().unwrap_or_default();
    assert_eq!(
        produced,
        recorded.client,
        "the client's own bytes changed since the recording. {}",
        first_difference(&produced, &recorded.client)
    );
    assert!(report.passed(), "{:?}", report.failures());
    clean(&destination);
}

// ---------------------------------------------------------------------------
// Failures a real server will not produce on request.
//
// Each of these is a constructed exchange rather than a recording, because
// there is no way to ask PostgreSQL to sign its SASL result wrongly or to cut a
// row in half. They are the half of the protocol surface a recording cannot
// reach, and they are the half where an error rather than a hang or a panic is
// the whole property.
// ---------------------------------------------------------------------------

/// Frames one PostgreSQL backend message.
///
/// @param tag - the message's type byte
/// @param body - the payload, without its length
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// Builds an `AuthenticationSASL` message offering SCRAM-SHA-256.
fn sasl_offer() -> Vec<u8> {
    let mut body = 10u32.to_be_bytes().to_vec();
    body.extend_from_slice(b"SCRAM-SHA-256\0\0");
    message(b'R', &body)
}

/// Runs a SCRAM server against a client, live, and answers it.
///
/// It cannot be a recording: the client's nonce is fresh every run by design,
/// and the server's reply is computed from it. So this is a small SCRAM server,
/// and driving the client against it checks the halves a recording cannot -
/// that the client refuses a server whose final signature does not verify.
///
/// @param password - the password the fake server believes in
/// @param honest - whether to sign the final message correctly
fn scram_server(password: &str, honest: bool) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address").to_string();
    let password = password.to_string();
    let handle = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        let mut scratch = [0u8; 8192];
        // The startup packet, which needs no reply beyond the offer.
        let _ = socket.read(&mut scratch);
        let _ = socket.write_all(&sasl_offer());

        // The client's SASLInitialResponse: `SCRAM-SHA-256\0`, a length, then
        // `n,,n=,r=<nonce>`.
        let read = socket.read(&mut scratch).unwrap_or(0);
        let sent = String::from_utf8_lossy(scratch.get(..read).unwrap_or_default()).into_owned();
        let client_nonce = sent
            .rsplit_once("r=")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
        let client_first_bare = format!("n=,r={client_nonce}");

        let salt = b"0123456789abcdef";
        let combined = format!("{client_nonce}serverpart");
        let server_first = format!("r={combined},s={},i=4096", base64_encode(salt));
        let mut body = 11u32.to_be_bytes().to_vec();
        body.extend_from_slice(server_first.as_bytes());
        let _ = socket.write_all(&message(b'R', &body));

        // The client's proof, which this server does not need to check.
        let read = socket.read(&mut scratch).unwrap_or(0);
        let final_message =
            String::from_utf8_lossy(scratch.get(..read).unwrap_or_default()).into_owned();
        let client_final_bare = final_message
            .split_once("c=biws")
            .map(|(_, rest)| format!("c=biws{}", rest.split(",p=").next().unwrap_or_default()))
            .unwrap_or_default();

        let auth_message = format!("{client_first_bare},{server_first},{client_final_bare}");
        let salted = pbkdf2_sha256(password.as_bytes(), salt, 4096);
        let server_key = hmac_sha256(&salted, b"Server Key");
        let signature = if honest {
            hmac_sha256(&server_key, auth_message.as_bytes()).to_vec()
        } else {
            // A signature over the wrong message: what a server that does not
            // hold this password can produce, and what the client must refuse.
            hmac_sha256(&server_key, b"not the auth message").to_vec()
        };
        let mut body = 12u32.to_be_bytes().to_vec();
        body.extend_from_slice(format!("v={}", base64_encode(&signature)).as_bytes());
        let _ = socket.write_all(&message(b'R', &body));
        let _ = socket.write_all(&message(b'R', &0u32.to_be_bytes()));
        let _ = socket.write_all(&message(b'Z', b"I"));
        // **And then it keeps answering.** The client opens its read snapshot
        // straight after the login and commits it on the way out, so a fake
        // server that stopped at `AuthenticationOk` would leave the client
        // waiting for a `ReadyForQuery` that never comes - which is a hang
        // rather than a failure, and a hang is the worst shape a test can have.
        loop {
            match socket.read(&mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = socket.write_all(&message(b'C', b"OK\0"));
                    if socket.write_all(&message(b'Z', b"T")).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (address, handle)
}

/// The SCRAM exchange completes against a server that signs correctly. The
/// derivation is checked against RFC 7677's vector in the unit tests and
/// against a real `scram-sha-256` PostgreSQL in `live_postgres.rs`; this is the
/// exchange itself, over a socket.
#[test]
fn scram_completes_against_a_server_that_signs_correctly() {
    let (address, handle) = scram_server("pencil", true);
    let url =
        ConnectionUrl::parse(&format!("postgres://user:pencil@{address}/db")).expect("parses");
    let mut source = match PostgresSource::over(dial(&address), &url) {
        Ok(source) => source,
        Err(error) => panic!("the exchange should have completed: {error:?}"),
    };
    source.finish();
    let _ = handle.join();
}

/// **A server whose final signature does not verify is refused.** Skipping that
/// check is the difference between authenticating the server and merely being
/// let in by it, and the failure it prevents is silent: the migration would
/// otherwise run happily against whatever answered the socket.
#[test]
fn scram_refuses_a_server_whose_signature_does_not_verify() {
    let (address, handle) = scram_server("pencil", false);
    let url =
        ConnectionUrl::parse(&format!("postgres://user:pencil@{address}/db")).expect("parses");
    let error = match PostgresSource::over(dial(&address), &url) {
        Ok(_) => panic!("the client should have refused"),
        Err(error) => error,
    };
    let said = error.detail().unwrap_or_else(|| error.message());
    assert!(said.contains("does not verify"), "{said}");
    let _ = handle.join();
}

/// A server that sends an `ErrorResponse` instead of accepting the login is
/// reported with its SQLSTATE, so a caller matches on `28P01` rather than on
/// the server's English.
#[test]
fn a_login_error_carries_its_sqlstate_through_the_socket() {
    let mut recorded = Vec::new();
    let mut body = b"SFATAL\0".to_vec();
    body.extend_from_slice(b"C28P01\0");
    body.extend_from_slice(b"Mpassword authentication failed for user \"nobody\"\0");
    body.push(0);
    recorded.extend_from_slice(&message(b'E', &body));
    let (address, _said) = serve(recorded);
    let url = ConnectionUrl::parse(&format!("postgres://nobody:x@{address}/db")).expect("parses");
    let error = match PostgresSource::over(dial(&address), &url) {
        Ok(_) => panic!("the client should have refused"),
        Err(error) => error,
    };
    let said = error.detail().unwrap_or_else(|| error.message());
    assert!(said.contains("28P01"), "{said}");
}

/// **A stream that stops in the middle of a message is an error, not a hang and
/// not a shorter message.** A migration that quietly read half a row would
/// publish half a table.
#[test]
fn a_truncated_message_is_an_error_rather_than_a_short_read() {
    // An `AuthenticationOk` whose length promises 200 bytes and then ends.
    let mut recorded = vec![b'R'];
    recorded.extend_from_slice(&200u32.to_be_bytes());
    recorded.extend_from_slice(&[0u8; 8]);
    let (address, _said) = serve(recorded);
    let url = ConnectionUrl::parse(&format!("postgres://u:x@{address}/db")).expect("parses");
    let error = match PostgresSource::over(dial(&address), &url) {
        Ok(_) => panic!("the client should have refused"),
        Err(error) => error,
    };
    let said = error.detail().unwrap_or_else(|| error.message());
    assert!(
        said.contains("closed the connection"),
        "the failure should name the short read: {said}"
    );
}

/// MySQL's `caching_sha2_password` **full** authentication is refused by name,
/// with the two ways out, rather than surfacing as a wrong password.
#[test]
fn caching_sha2_full_authentication_is_refused_by_name() {
    let capabilities: u32 = 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
    let mut greeting = vec![10u8];
    greeting.extend_from_slice(b"8.4.6\0");
    greeting.extend_from_slice(&7u32.to_le_bytes());
    greeting.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    greeting.push(0);
    greeting.extend_from_slice(&((capabilities & 0xffff) as u16).to_le_bytes());
    greeting.push(45);
    greeting.extend_from_slice(&0u16.to_le_bytes());
    greeting.extend_from_slice(&((capabilities >> 16) as u16).to_le_bytes());
    greeting.push(21);
    greeting.extend_from_slice(&[0u8; 10]);
    greeting.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 0]);
    greeting.extend_from_slice(b"caching_sha2_password\0");

    let mut recorded = framed(&greeting, 0);
    // AuthMoreData saying the cache does not hold this account.
    recorded.extend_from_slice(&framed(&[0x01, 0x04], 2));
    let (address, _said) = serve(recorded);
    let url =
        ConnectionUrl::parse(&format!("mysql://someone:secret@{address}/db")).expect("parses");
    let error = match MysqlSource::over(dial(&address), &url) {
        Ok(_) => panic!("the client should have refused"),
        Err(error) => error,
    };
    let said = error.detail().unwrap_or_else(|| error.message());
    assert!(said.contains("prime the cache"), "{said}");
    assert!(said.contains("mysql_native_password"), "{said}");
}

/// Frames one MySQL packet.
///
/// @param body - the payload
/// @param sequence - the packet's sequence number
fn framed(body: &[u8], sequence: u8) -> Vec<u8> {
    let length = body.len();
    let mut out = vec![
        (length & 0xff) as u8,
        ((length >> 8) & 0xff) as u8,
        ((length >> 16) & 0xff) as u8,
        sequence,
    ];
    out.extend_from_slice(body);
    out
}

/// The SCRAM derivation this crate performs is the one RFC 7677 publishes, all
/// the way to the stored key - repeated here because `protocol.rs` is the file
/// somebody reads when a login stops working, and the derivation is the first
/// thing they will want to see checked.
#[test]
fn the_scram_derivation_matches_its_published_vector() {
    let salt = base64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").expect("decodes");
    let salted = pbkdf2_sha256(b"pencil", &salt, 4096);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256(&client_key);
    let auth_message = "n=user,r=rOprNGfwEbeRWgbNEkqO,\
                        r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
                        s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,\
                        c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
    assert_eq!(
        base64_encode(&xor(&client_key, &signature)),
        "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
    );
}

/// A real `scram-sha-256` PostgreSQL's own challenge, decoded.
///
/// `postgres-scram.transcript` is the one recording that cannot be replayed -
/// the client's nonce is fresh every run by design, so the server's reply to a
/// *different* nonce is not an answer to this one. It is kept anyway, and this
/// is what it is kept for: the mechanism the server offers, the salt and the
/// iteration count are read here out of **bytes PostgreSQL 17.2 actually sent**
/// rather than out of a message this test wrote for itself. A decoder that
/// agrees only with its own encoder agrees with nothing.
///
/// It walks the message framing to find them rather than searching the bytes
/// for `r=`, which is the first thing I tried and which matched a `r=` inside
/// an unrelated later message. Walking is also the stronger check: a stream
/// whose lengths did not line up would not reach the end.
#[test]
fn a_real_servers_scram_challenge_decodes() {
    let recorded = transcript("postgres-scram.transcript");
    let mut offered = false;
    let mut challenge = None;
    let mut at = 0usize;
    while at + 5 <= recorded.server.len() {
        let tag = recorded.server[at];
        let length = u32::from_be_bytes([
            recorded.server[at + 1],
            recorded.server[at + 2],
            recorded.server[at + 3],
            recorded.server[at + 4],
        ]) as usize;
        assert!(
            length >= 4,
            "a message cannot be shorter than its own length"
        );
        let body = match recorded.server.get(at + 5..at + 1 + length) {
            Some(body) => body,
            None => break,
        };
        at += 1 + length;
        if tag != b'R' {
            continue;
        }
        match u32::from_be_bytes([body[0], body[1], body[2], body[3]]) {
            // AuthenticationSASL, listing the mechanisms it will accept.
            10 => {
                offered = body.windows(13).any(|window| window == b"SCRAM-SHA-256");
            }
            // AuthenticationSASLContinue: `r=<nonce>,s=<salt>,i=<count>`.
            11 => {
                challenge = Some(String::from_utf8_lossy(&body[4..]).into_owned());
                break;
            }
            _ => {}
        }
    }

    assert!(offered, "the server offered SCRAM-SHA-256");
    let challenge = challenge.expect("the recording carries a SASL challenge");
    let attributes: Vec<(&str, &str)> = challenge
        .split(',')
        .filter_map(|part| part.split_once('='))
        .collect();

    let salt = attributes
        .iter()
        .find(|(key, _)| *key == "s")
        .map(|(_, value)| *value)
        .expect("the challenge names a salt");
    let decoded = base64_decode(salt).expect("the salt is base64");
    assert_eq!(decoded.len(), 16, "PostgreSQL's own salt is 16 bytes");

    let iterations: u32 = attributes
        .iter()
        .find(|(key, _)| *key == "i")
        .and_then(|(_, value)| value.parse().ok())
        .expect("the challenge names an iteration count");
    assert!(
        iterations >= 4096,
        "an iteration count below RFC 7677's floor would be the server's own problem,          but this recording should not carry one: {iterations}"
    );

    let nonce = attributes
        .iter()
        .find(|(key, _)| *key == "r")
        .map(|(_, value)| *value)
        .expect("the challenge extends a nonce");
    assert!(
        nonce.len() > 24,
        "the combined nonce is the client's plus the server's: {nonce}"
    );
}
