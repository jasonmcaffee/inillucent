//! PostgreSQL, over the version 3 frontend/backend protocol.
//!
//! The subset a reader needs, and no more: the startup packet, three
//! authentication methods, the simple query protocol in text format, and
//! `ErrorResponse` decoded into its own SQLSTATE.
//!
//! Three decisions worth defending, because each one is a place a different
//! choice would have looked reasonable:
//!
//! - **The simple query protocol, in text format.** The extended protocol buys
//!   parameter binding and binary results. A reader that streams whole tables
//!   binds nothing, and binary results would mean decoding a layout that varies
//!   by type and by server version. The text rendering is produced by the
//!   type's own output function - it is what `psql` prints - so it is exact for
//!   every type including the ones this engine has no equivalent for.
//! - **One repeatable-read snapshot for the whole read.** Reading table B after
//!   table A committed produces a destination that describes a database which
//!   never existed. The snapshot is opened before the catalog is read, so even
//!   the schema is as of one instant.
//! - **A cursor rather than a whole result set.** `FETCH 10000` bounds this
//!   process's memory to one batch no matter how large the table is.

use std::collections::BTreeMap;
use std::time::Duration;

use inillucent_base::DbResult;
use inillucent_tree::datum::OwnedDatum;

use crate::auth::{base64_decode, base64_encode, hmac_sha256, md5, pbkdf2_sha256, to_hex, xor};
use crate::source::{Kind, RemoteSource, SourceColumn, SourceTable};
use crate::stream::{be_i32, be_u16, be_u32, protocol, will_not, Stream};
use crate::url::{ConnectionUrl, Transport};
use inillucent_base::hash::sha256;

/// The protocol version this client speaks: 3.0, as `(3 << 16) | 0`.
const PROTOCOL_VERSION: u32 = 196_608;

/// The version number that means "this is an `SSLRequest`, not a startup".
///
/// `(1234 << 16) | 5679`, which is how the protocol carries a request that is
/// not a protocol version: a server that does not recognise it answers with a
/// single byte rather than closing, which is what makes the negotiation
/// possible at all.
const SSL_REQUEST_CODE: u32 = 80_877_103;

/// Asks the server for TLS and upgrades the socket when it agrees.
///
/// **A refusal is a refusal, not a fallback.** A server that answers `N` has
/// TLS turned off, and continuing in the clear would send this migration's
/// password over the network with nothing said about it - which is the whole
/// defect this replaces. The message names the two ways forward.
///
/// @param stream - the freshly connected socket, with nothing written on it
/// @param url - where the host name and any named authority come from
fn request_tls(stream: &mut Stream, url: &ConnectionUrl) -> DbResult<()> {
    let mut packet: Vec<u8> = Vec::new();
    packet.extend_from_slice(&8u32.to_be_bytes());
    packet.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&packet)?;
    match stream.read_u8()? {
        b'S' => stream.upgrade(&url.host, url.root_certificate()),
        b'N' => Err(will_not(format!(
            "{} refused a TLS connection, so this migration stopped before it sent a password. \
             Turn TLS on at the server (PostgreSQL's `ssl = on`), or - only for a loopback \
             address or a network you trust - write sslmode=disable in the URL and pass \
             --insecure-plaintext.",
            url.address()
        ))),
        b'E' => Err(will_not(format!(
            "{} answered the TLS request with an error, which is what a server older than 8.0 \
             does. This client requires TLS unless it is told otherwise.",
            url.address()
        ))),
        other => Err(protocol(format!(
            "{} answered a TLS request with 0x{other:02x}, which is not one of the two bytes the \
             protocol allows",
            url.address()
        ))),
    }
}

/// How many rows one `FETCH` asks for.
const FETCH_ROWS: usize = 10_000;

/// The cursor a table scan runs through.
const CURSOR: &str = "inillucent_migrate_cursor";

/// One column of a result set, as `RowDescription` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The column's name.
    pub name: String,
    /// The type's OID, which is what the value's text has to be read against.
    pub type_oid: u32,
}

/// A whole result set, buffered.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    /// The columns, in order.
    pub fields: Vec<Field>,
    /// The rows, each value either absent (SQL NULL) or its text bytes.
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
}

impl ResultSet {
    /// Returns one row's column as text, or the empty string.
    ///
    /// @param row - which row
    /// @param column - which column
    pub fn text(&self, row: usize, column: usize) -> String {
        self.rows
            .get(row)
            .and_then(|row| row.get(column))
            .and_then(|value| value.as_ref())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    }
}

/// A connection to a PostgreSQL server.
pub struct PostgresSource {
    /// The socket.
    stream: Stream,
    /// What the server calls itself, from its `server_version` parameter.
    version: String,
    /// The URL, kept for the report - and it redacts its own password.
    url: ConnectionUrl,
    /// Whether a read snapshot is open.
    in_snapshot: bool,
}

/// What a streaming read hands each row to: the columns it is shaped by, and
/// the row's values, each absent for a SQL NULL.
pub type RowSink<'s> = dyn FnMut(&[Field], &[Option<Vec<u8>>]) -> DbResult<()> + 's;

impl PostgresSource {
    /// Connects, authenticates, and opens the read snapshot.
    ///
    /// @param url - where to connect and as who
    pub fn connect(url: &ConnectionUrl) -> DbResult<PostgresSource> {
        PostgresSource::connect_over(url, Transport::VerifiedTls)
    }

    /// Connects on a transport the caller's policy has already decided.
    ///
    /// **The upgrade happens before the startup packet.** PostgreSQL's own
    /// `SSLRequest` is a whole message of its own, sent in the clear on a
    /// freshly opened socket, and everything that identifies the connection -
    /// the user name, the database, and then the password - comes after it. So
    /// a refused or unverifiable certificate costs a socket and nothing else.
    ///
    /// @param url - where to connect and as who
    /// @param transport - what the policy in `ConnectionUrl::transport` decided
    pub fn connect_over(url: &ConnectionUrl, transport: Transport) -> DbResult<PostgresSource> {
        let mut stream =
            Stream::connect(&url.address(), Duration::from_secs(url.timeout_seconds()))?;
        if transport == Transport::VerifiedTls {
            request_tls(&mut stream, url)?;
        }
        PostgresSource::over(stream, url)
    }

    /// Wraps an already-connected socket and runs the handshake on it.
    ///
    /// The seam the protocol tests drive: a recorded transcript is served over
    /// a loopback socket and handed here, so the client's own bytes are checked
    /// without a server installed. It does everything [`PostgresSource::connect`]
    /// does except dial, snapshot included - a seam that stopped one step short
    /// would be testing a sequence no real run ever performs.
    ///
    /// @param stream - a connected socket
    /// @param url - the credentials to authenticate with
    pub fn over(stream: Stream, url: &ConnectionUrl) -> DbResult<PostgresSource> {
        let mut source = PostgresSource {
            stream,
            version: String::new(),
            url: url.clone(),
            in_snapshot: false,
        };
        source.start_up()?;
        source.open_snapshot()?;
        Ok(source)
    }

    /// Returns what the peer proved it was, when the connection is encrypted.
    pub fn peer(&self) -> Option<&str> {
        self.stream.peer()
    }

    /// Sends the startup packet and answers whatever authentication is asked for.
    fn start_up(&mut self) -> DbResult<()> {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        for (name, value) in [
            ("user", self.url.user.as_str()),
            ("database", self.url.database.as_str()),
            ("client_encoding", "UTF8"),
            ("application_name", "inillucent-migrate"),
            // **Three renderings pinned, so the migration does not depend on
            // where it was run from.** `bytea_output` gives the decoder one
            // shape to read rather than two. `datestyle` fixes the day/month
            // order, which otherwise follows the server's locale. And
            // `timezone` is the one that was measured: a `timestamptz` is
            // rendered in the *session's* zone, so the same database migrated
            // from a laptop in Denver produced `2026-01-01 20:04:05-07` where
            // one migrated from a server in UTC produced
            // `2026-01-02 03:04:05+00` - the same instant, a different string,
            // and therefore a different digest and a destination whose text
            // depends on who ran the tool.
            (
                "options",
                "-c bytea_output=hex -c datestyle=ISO,YMD -c timezone=UTC",
            ),
        ] {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut packet = ((body.len().saturating_add(4)) as u32)
            .to_be_bytes()
            .to_vec();
        packet.extend_from_slice(&body);
        self.stream.write_all(&packet)?;
        self.authenticate()
    }

    /// Reads the authentication exchange through to `ReadyForQuery`.
    fn authenticate(&mut self) -> DbResult<()> {
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'R' => {
                    let code = be_u32(&body, 0)?;
                    match code {
                        // AuthenticationOk.
                        0 => continue,
                        // AuthenticationCleartextPassword.
                        3 => {
                            let password = self.password_or_refuse("cleartext password")?;
                            let mut message = password.into_bytes();
                            message.push(0);
                            self.send(b'p', &message)?;
                        }
                        // AuthenticationMD5Password.
                        5 => {
                            let salt = body
                                .get(4..8)
                                .ok_or_else(|| protocol("the md5 challenge carried no salt"))?
                                .to_vec();
                            let password = self.password_or_refuse("md5 password")?;
                            let inner =
                                to_hex(&md5(
                                    &[password.as_bytes(), self.url.user.as_bytes()].concat()
                                ));
                            let outer = to_hex(&md5(&[inner.as_bytes(), salt.as_slice()].concat()));
                            let mut message = format!("md5{outer}").into_bytes();
                            message.push(0);
                            self.send(b'p', &message)?;
                        }
                        // AuthenticationSASL.
                        10 => self.scram(&body)?,
                        11 | 12 => {
                            return Err(protocol(
                                "the server continued a SASL exchange this client did not start",
                            ))
                        }
                        other => {
                            return Err(will_not(format!(
                                "the server asked for authentication method {other}, which this \
                                 migration client does not implement. It speaks trust, cleartext, \
                                 md5 and scram-sha-256."
                            )))
                        }
                    }
                }
                b'S' => {
                    let mut fields = body.split(|byte| *byte == 0);
                    let name = fields.next().unwrap_or_default();
                    let value = fields.next().unwrap_or_default();
                    if name == b"server_version" {
                        self.version = String::from_utf8_lossy(value).into_owned();
                    }
                }
                b'K' | b'N' => continue,
                b'Z' => return Ok(()),
                b'E' => return Err(decode_error(&body)),
                other => {
                    return Err(protocol(format!(
                        "unexpected message '{}' during authentication",
                        other as char
                    )))
                }
            }
        }
    }

    /// Runs the SCRAM-SHA-256 exchange, RFC 5802 and RFC 7677.
    ///
    /// **The server's signature is verified.** Skipping that check is the
    /// difference between authenticating the server and merely being let in by
    /// it, and a client that skipped it would accept a man in the middle that
    /// answered anything at all in the final message.
    ///
    /// @param body - the `AuthenticationSASL` body, listing the mechanisms
    fn scram(&mut self, body: &[u8]) -> DbResult<()> {
        let mechanisms: Vec<String> = body
            .get(4..)
            .unwrap_or_default()
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        if !mechanisms.iter().any(|name| name == "SCRAM-SHA-256") {
            return Err(will_not(format!(
                "the server offered {} and this client speaks SCRAM-SHA-256",
                mechanisms.join(", ")
            )));
        }
        let password = self.password_or_refuse("scram-sha-256")?;
        let nonce = client_nonce();
        let client_first_bare = format!("n=,r={nonce}");
        let client_first = format!("n,,{client_first_bare}");

        let mut initial: Vec<u8> = b"SCRAM-SHA-256".to_vec();
        initial.push(0);
        initial.extend_from_slice(&(client_first.len() as u32).to_be_bytes());
        initial.extend_from_slice(client_first.as_bytes());
        self.send(b'p', &initial)?;

        let (tag, body) = self.read_message()?;
        if tag == b'E' {
            return Err(decode_error(&body));
        }
        if tag != b'R' || be_u32(&body, 0)? != 11 {
            return Err(protocol("the server did not continue the SASL exchange"));
        }
        let server_first = String::from_utf8_lossy(body.get(4..).unwrap_or_default()).into_owned();
        let attributes = scram_attributes(&server_first);
        let combined = attributes
            .get("r")
            .ok_or_else(|| protocol("the SASL challenge carried no nonce"))?
            .clone();
        if !combined.starts_with(&nonce) {
            return Err(protocol(
                "the server's nonce does not extend the one this client sent",
            ));
        }
        let salt = attributes
            .get("s")
            .and_then(|value| base64_decode(value))
            .ok_or_else(|| protocol("the SASL challenge carried no readable salt"))?;
        let iterations: u32 = attributes
            .get("i")
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| protocol("the SASL challenge carried no iteration count"))?;

        let client_final_bare = format!("c=biws,r={combined}");
        let auth_message = format!("{client_first_bare},{server_first},{client_final_bare}");
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof = xor(&client_key, &client_signature);
        let client_final = format!("{client_final_bare},p={}", base64_encode(&proof));
        self.send(b'p', client_final.as_bytes())?;

        let (tag, body) = self.read_message()?;
        if tag == b'E' {
            return Err(decode_error(&body));
        }
        if tag != b'R' || be_u32(&body, 0)? != 12 {
            return Err(protocol("the server did not finish the SASL exchange"));
        }
        let final_message = String::from_utf8_lossy(body.get(4..).unwrap_or_default()).into_owned();
        let signature = scram_attributes(&final_message)
            .get("v")
            .and_then(|value| base64_decode(value))
            .ok_or_else(|| protocol("the SASL result carried no server signature"))?;
        let server_key = hmac_sha256(&salted, b"Server Key");
        let expected = hmac_sha256(&server_key, auth_message.as_bytes());
        if signature != expected {
            return Err(protocol(
                "the server's SCRAM signature does not verify; this is not the server that holds \
                 this password",
            ));
        }
        Ok(())
    }

    /// Returns the password, or refuses with the method that needed one.
    ///
    /// `PGPASSWORD` is consulted because that is where `psql` looks, and a
    /// migration is usually run from the same shell as the `psql` that proved
    /// the credentials work.
    ///
    /// @param method - the authentication method asking, for the message
    fn password_or_refuse(&self, method: &str) -> DbResult<String> {
        if let Some(password) = &self.url.password {
            return Ok(password.clone());
        }
        if let Ok(password) = std::env::var("PGPASSWORD") {
            if !password.is_empty() {
                return Ok(password);
            }
        }
        Err(will_not(format!(
            "the server asked for {method} authentication and the connection URL carries no \
             password. Put it in the URL, or set PGPASSWORD."
        )))
    }

    /// Opens the one snapshot every read happens inside.
    fn open_snapshot(&mut self) -> DbResult<()> {
        self.execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")?;
        self.in_snapshot = true;
        Ok(())
    }

    /// Sends one frontend message.
    ///
    /// @param tag - the message's type byte
    /// @param body - the payload, without its length
    fn send(&mut self, tag: u8, body: &[u8]) -> DbResult<()> {
        let mut packet = vec![tag];
        packet.extend_from_slice(&((body.len().saturating_add(4)) as u32).to_be_bytes());
        packet.extend_from_slice(body);
        self.stream.write_all(&packet)
    }

    /// Reads one backend message: its tag and its body without the length.
    fn read_message(&mut self) -> DbResult<(u8, Vec<u8>)> {
        let tag = self.stream.read_u8()?;
        let header = self.stream.read_exact(4)?;
        let length = be_u32(&header, 0)? as usize;
        if length < 4 {
            return Err(protocol(format!(
                "the server sent a message of {length} bytes, which cannot hold its own length"
            )));
        }
        let body = self.stream.read_exact(length.saturating_sub(4))?;
        Ok((tag, body))
    }

    /// Runs a statement and discards its rows.
    ///
    /// @param sql - the statement
    pub fn execute(&mut self, sql: &str) -> DbResult<()> {
        self.query(sql).map(|_| ())
    }

    /// Runs one query and buffers its result.
    ///
    /// @param sql - the query
    pub fn query(&mut self, sql: &str) -> DbResult<ResultSet> {
        let mut out = ResultSet::default();
        self.stream_query(sql, &mut |fields, row| {
            if out.fields.is_empty() {
                out.fields = fields.to_vec();
            }
            out.rows.push(row.to_vec());
            Ok(())
        })?;
        Ok(out)
    }

    /// Runs one query and hands each row to a sink as it arrives.
    ///
    /// Returns how many rows there were. An `ErrorResponse` is decoded and
    /// **the stream is still drained to `ReadyForQuery`** before it is returned,
    /// because a connection left mid-message is a connection that cannot be
    /// used for the next table.
    ///
    /// @param sql - the query
    /// @param sink - what to do with each row
    pub fn stream_query(&mut self, sql: &str, sink: &mut RowSink<'_>) -> DbResult<u64> {
        let mut message = sql.as_bytes().to_vec();
        message.push(0);
        self.send(b'Q', &message)?;

        let mut fields: Vec<Field> = Vec::new();
        let mut rows = 0u64;
        let mut failure = None;
        let mut sink_failure = None;
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'T' => fields = decode_row_description(&body)?,
                b'D' => {
                    let row = decode_data_row(&body)?;
                    rows = rows.saturating_add(1);
                    if sink_failure.is_none() {
                        if let Err(error) = sink(&fields, &row) {
                            sink_failure = Some(error);
                        }
                    }
                }
                b'C' | b'I' | b'n' | b'S' | b'K' | b'N' => continue,
                b'E' => failure = Some(decode_error(&body)),
                b'Z' => break,
                other => {
                    return Err(protocol(format!(
                        "unexpected message '{}' while running a query",
                        other as char
                    )))
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if let Some(error) = sink_failure {
            return Err(error);
        }
        Ok(rows)
    }
}

impl RemoteSource for PostgresSource {
    /// Reads every ordinary table in every schema the application owns.
    fn describe(&mut self) -> DbResult<Vec<SourceTable>> {
        let columns = self.query(COLUMN_QUERY)?;
        let keys = self.query(KEY_QUERY)?;

        let mut key_of: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for at in 0..keys.rows.len() {
            key_of
                .entry((keys.text(at, 0), keys.text(at, 1)))
                .or_default()
                .push(keys.text(at, 2));
        }

        let mut tables: Vec<SourceTable> = Vec::new();
        for at in 0..columns.rows.len() {
            let schema = columns.text(at, 0);
            let name = columns.text(at, 1);
            let column = SourceColumn {
                name: columns.text(at, 2),
                declared: columns.text(at, 5),
                kind: kind_of(columns.text(at, 3).parse::<u32>().unwrap_or(0)),
                // `attnotnull` arrives as `t` or `f`.
                nullable: columns.text(at, 4) != "t",
            };
            match tables
                .iter_mut()
                .find(|table| table.schema == schema && table.name == name)
            {
                Some(table) => table.columns.push(column),
                None => tables.push(SourceTable {
                    target: target_name(&schema, &name),
                    primary_key: key_of
                        .get(&(schema.clone(), name.clone()))
                        .cloned()
                        .unwrap_or_default(),
                    schema,
                    name,
                    columns: vec![column],
                }),
            }
        }
        Ok(tables)
    }

    /// Counts a table's rows, as a count rather than as a scan.
    fn count(&mut self, table: &SourceTable) -> DbResult<u64> {
        let result = self.query(&format!(
            "SELECT count(*) FROM {}",
            qualified(&table.schema, &table.name)
        ))?;
        result.text(0, 0).parse::<u64>().map_err(|_| {
            protocol(format!(
                "count(*) on {} did not answer a number",
                table.name
            ))
        })
    }

    /// Streams every row of a table through a cursor.
    fn scan(
        &mut self,
        table: &SourceTable,
        sink: &mut dyn FnMut(&[OwnedDatum]) -> DbResult<()>,
    ) -> DbResult<u64> {
        let selected = table
            .columns
            .iter()
            .map(|column| quoted(&column.name))
            .collect::<Vec<String>>()
            .join(", ");
        let kinds: Vec<Kind> = table.columns.iter().map(|column| column.kind).collect();
        self.execute(&format!(
            "DECLARE {CURSOR} NO SCROLL CURSOR FOR SELECT {selected} FROM {}",
            qualified(&table.schema, &table.name)
        ))?;
        let mut total = 0u64;
        loop {
            let mut converted: DbResult<()> = Ok(());
            let fetched = self.stream_query(
                &format!("FETCH FORWARD {FETCH_ROWS} FROM {CURSOR}"),
                &mut |_, row| {
                    let mut values = Vec::with_capacity(row.len());
                    for (at, value) in row.iter().enumerate() {
                        values.push(carry(
                            value.as_deref(),
                            *kinds.get(at).unwrap_or(&Kind::Text),
                        )?);
                    }
                    sink(&values)
                },
            );
            let fetched = match fetched {
                Ok(fetched) => fetched,
                Err(error) => {
                    converted = Err(error);
                    0
                }
            };
            if let Err(error) = converted {
                let _ = self.execute(&format!("CLOSE {CURSOR}"));
                return Err(error);
            }
            total = total.saturating_add(fetched);
            if (fetched as usize) < FETCH_ROWS {
                break;
            }
        }
        self.execute(&format!("CLOSE {CURSOR}"))?;
        Ok(total)
    }

    /// Returns what the server calls itself.
    fn server(&self) -> String {
        format!("PostgreSQL {}", self.version)
    }

    /// Returns what the peer's certificate proved, when encrypted.
    fn peer(&self) -> Option<String> {
        self.stream.peer().map(str::to_string)
    }

    /// Returns the objects that exist and are not carried.
    fn not_carried(&mut self) -> DbResult<Vec<(String, String)>> {
        let result = self.query(NOT_CARRIED_QUERY)?;
        Ok((0..result.rows.len())
            .map(|at| (result.text(at, 0), result.text(at, 1)))
            .collect())
    }

    /// Commits the read snapshot and closes the connection.
    fn finish(&mut self) {
        if self.in_snapshot {
            let _ = self.execute("COMMIT");
            self.in_snapshot = false;
        }
        let _ = self.send(b'X', &[]);
        self.stream.close();
    }
}

/// Every ordinary table's columns, in ordinal order.
///
/// `pg_catalog` directly rather than `information_schema`, because the type OID
/// is what the value's text has to be read against and the view does not expose
/// it - it exposes a name, which would have to be mapped back.
const COLUMN_QUERY: &str = "\
SELECT n.nspname, c.relname, a.attname, a.atttypid::text, \
       CASE WHEN a.attnotnull THEN 't' ELSE 'f' END, \
       pg_catalog.format_type(a.atttypid, a.atttypmod) \
  FROM pg_catalog.pg_class c \
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
 WHERE c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped \
   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
   AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%' \
 ORDER BY n.nspname, c.relname, a.attnum";

/// Each table's primary key, in key order.
const KEY_QUERY: &str = "\
SELECT n.nspname, c.relname, a.attname \
  FROM pg_catalog.pg_index i \
  JOIN pg_catalog.pg_class c ON c.oid = i.indrelid \
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
  CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) \
  JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
 WHERE i.indisprimary AND c.relkind = 'r' \
   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
 ORDER BY n.nspname, c.relname, k.ord";

/// What exists in the source and does not come across.
const NOT_CARRIED_QUERY: &str = "\
SELECT CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
                      WHEN 'S' THEN 'sequence' WHEN 'f' THEN 'foreign table' \
                      WHEN 'p' THEN 'partitioned table' ELSE c.relkind::text END, \
       n.nspname || '.' || c.relname \
  FROM pg_catalog.pg_class c \
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
 WHERE c.relkind IN ('v', 'm', 'S', 'f', 'p') \
   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
UNION ALL \
SELECT 'trigger', n.nspname || '.' || c.relname || '.' || t.tgname \
  FROM pg_catalog.pg_trigger t \
  JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
 WHERE NOT t.tgisinternal AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
 ORDER BY 1, 2";

/// Returns an identifier quoted for PostgreSQL.
///
/// @param name - the identifier
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Returns a schema-qualified table name, quoted.
///
/// @param schema - the schema
/// @param name - the table
fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quoted(schema), quoted(name))
}

/// Returns what a source table is called in the destination.
///
/// This dialect has one namespace per database, so a schema other than `public`
/// is folded into the name rather than dropped - dropping it would collide two
/// tables into one, and a migration that silently merged `sales.order` into
/// `archive.order` would be the worst failure this tool has.
///
/// @param schema - the source schema
/// @param name - the source table
fn target_name(schema: &str, name: &str) -> String {
    if schema == "public" || schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}__{name}")
    }
}

/// Returns how a PostgreSQL type is carried.
///
/// Anything not named here is carried as the server's own text rendering, which
/// is exact for every type PostgreSQL has - arrays, ranges, composites, enums,
/// `inet`, `tsvector` and whatever an extension added.
///
/// @param oid - the type's OID, from `RowDescription`
pub fn kind_of(oid: u32) -> Kind {
    match oid {
        // bool
        16 => Kind::Boolean,
        // bytea
        17 => Kind::Blob,
        // int8, int2, int4, oid, xid, cid, regproc
        20 | 21 | 23 | 24 | 26 | 28 | 29 => Kind::Integer,
        // float4, float8
        700 | 701 => Kind::Real,
        // numeric, money
        790 | 1700 => Kind::Decimal,
        _ => Kind::Text,
    }
}

/// Converts one wire value into what the destination stores.
///
/// @param value - the text bytes, or `None` for SQL NULL
/// @param kind - how this column is carried
pub fn carry(value: Option<&[u8]>, kind: Kind) -> DbResult<OwnedDatum> {
    let Some(bytes) = value else {
        return Ok(OwnedDatum::Null);
    };
    let text = String::from_utf8_lossy(bytes);
    Ok(match kind {
        Kind::Integer => match text.trim().parse::<i64>() {
            Ok(number) => OwnedDatum::Int(number),
            // An integer that does not fit is carried as its digits rather than
            // as a rounded double: `oid` is unsigned 32-bit and fits, but an
            // extension type mapped here by mistake must not lose precision
            // quietly.
            Err(_) => OwnedDatum::Text(bytes.to_vec()),
        },
        Kind::Boolean => OwnedDatum::Int(i64::from(text.trim() == "t" || text.trim() == "true")),
        Kind::Real => match text.trim().parse::<f64>() {
            Ok(number) => OwnedDatum::Real(number),
            Err(_) => OwnedDatum::Text(bytes.to_vec()),
        },
        Kind::Decimal | Kind::Text => OwnedDatum::Text(bytes.to_vec()),
        Kind::Blob => OwnedDatum::Blob(decode_bytea(bytes)),
    })
}

/// Decodes `bytea` from either of the two text forms PostgreSQL emits.
///
/// The startup packet asks for `hex`, but a server whose configuration refuses
/// the option, or an older one, answers in the backslash-escape form - and a
/// reader that assumed one would carry the *characters* of the other as bytes.
///
/// @param bytes - the value's text
fn decode_bytea(bytes: &[u8]) -> Vec<u8> {
    if bytes.starts_with(b"\\x") {
        let digits = bytes.get(2..).unwrap_or_default();
        let mut out = Vec::with_capacity(digits.len() / 2);
        let mut at = 0usize;
        while at.saturating_add(1) < digits.len() {
            let high = (*digits.get(at).unwrap_or(&b'0') as char).to_digit(16);
            let low = (*digits.get(at.saturating_add(1)).unwrap_or(&b'0') as char).to_digit(16);
            match (high, low) {
                (Some(high), Some(low)) => out.push(((high << 4) | low) as u8),
                _ => return bytes.to_vec(),
            }
            at = at.saturating_add(2);
        }
        return out;
    }
    // The escape form: `\\` is one backslash, `\nnn` is one octal byte.
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes.get(at) == Some(&b'\\') {
            if bytes.get(at.saturating_add(1)) == Some(&b'\\') {
                out.push(b'\\');
                at = at.saturating_add(2);
                continue;
            }
            let octal = bytes
                .get(at.saturating_add(1)..at.saturating_add(4))
                .and_then(|digits| std::str::from_utf8(digits).ok())
                .and_then(|digits| u16::from_str_radix(digits, 8).ok());
            if let Some(byte) = octal {
                out.push((byte & 0xff) as u8);
                at = at.saturating_add(4);
                continue;
            }
        }
        out.push(*bytes.get(at).unwrap_or(&0));
        at = at.saturating_add(1);
    }
    out
}

/// Decodes a `RowDescription` into its fields.
///
/// @param body - the message body
fn decode_row_description(body: &[u8]) -> DbResult<Vec<Field>> {
    let count = be_u16(body, 0)? as usize;
    let mut fields = Vec::with_capacity(count);
    let mut at = 2usize;
    for _ in 0..count {
        let end = body
            .get(at..)
            .and_then(|rest| rest.iter().position(|byte| *byte == 0))
            .ok_or_else(|| protocol("a column name in RowDescription is not terminated"))?;
        let name =
            String::from_utf8_lossy(body.get(at..at.saturating_add(end)).unwrap_or_default())
                .into_owned();
        at = at.saturating_add(end).saturating_add(1);
        // table oid (4), column number (2), type oid (4), type size (2),
        // type modifier (4), format code (2).
        let type_oid = be_u32(body, at.saturating_add(6))?;
        at = at.saturating_add(18);
        fields.push(Field { name, type_oid });
    }
    Ok(fields)
}

/// Decodes a `DataRow` into one value per column.
///
/// @param body - the message body
fn decode_data_row(body: &[u8]) -> DbResult<Vec<Option<Vec<u8>>>> {
    let count = be_u16(body, 0)? as usize;
    let mut row = Vec::with_capacity(count);
    let mut at = 2usize;
    for _ in 0..count {
        let length = be_i32(body, at)?;
        at = at.saturating_add(4);
        if length < 0 {
            row.push(None);
            continue;
        }
        let length = length as usize;
        let value = body
            .get(at..at.saturating_add(length))
            .ok_or_else(|| protocol("a DataRow value runs past the message"))?
            .to_vec();
        at = at.saturating_add(length);
        row.push(Some(value));
    }
    Ok(row)
}

/// Decodes an `ErrorResponse` into an error naming its SQLSTATE.
///
/// The SQLSTATE is in the message on purpose: `28P01` is the same five
/// characters in every locale, and a caller that has to match on English is a
/// caller that breaks when the server's `lc_messages` changes.
///
/// @param body - the message body
fn decode_error(body: &[u8]) -> inillucent_base::error::DbError {
    let mut code = String::new();
    let mut message = String::new();
    let mut detail = String::new();
    for field in body.split(|byte| *byte == 0) {
        let Some(tag) = field.first() else { continue };
        let text = String::from_utf8_lossy(field.get(1..).unwrap_or_default()).into_owned();
        match tag {
            b'C' => code = text,
            b'M' => message = text,
            b'D' => detail = text,
            _ => {}
        }
    }
    let said = if detail.is_empty() {
        format!("postgres {code}: {message}")
    } else {
        format!("postgres {code}: {message} ({detail})")
    };
    will_not(said)
}

/// Splits a SCRAM message into its `key=value` attributes.
///
/// @param message - the whole message
fn scram_attributes(message: &str) -> BTreeMap<String, String> {
    message
        .split(',')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Returns a fresh client nonce.
///
/// Base64 of 18 bytes from the **operating system's** generator, not a seeded
/// one: SCRAM's replay protection and its mutual authentication both rest on
/// the server being unable to predict this string, and a deterministic
/// generator seeded from a clock is what somebody who knows roughly when the
/// migration ran can enumerate. The `+`, `/` and `=` base64 can produce are
/// folded away, because a SCRAM attribute is comma-separated and `=`-delimited.
fn client_nonce() -> String {
    let mut bytes = [0u8; 18];
    if inillucent_vfs::os::system_randomness(&mut bytes).is_err() {
        // The only way that fails is the platform's own generator failing, at
        // which point a nonce that is merely unlikely to repeat is better than
        // no exchange at all: the login still authenticates, it loses only the
        // guarantee that the server could not have predicted the string.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as u64)
            .unwrap_or(0);
        let mut generator = inillucent_base::rng::Rng::new(now ^ u64::from(std::process::id()));
        generator.fill(&mut bytes);
    }
    base64_encode(&bytes).replace(['+', '/', '='], "A")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The type map, asserted per OID rather than by exercising a server, so a
    /// change to it fails here rather than in somebody's migration.
    #[test]
    fn the_type_map_carries_each_family_the_way_the_tdd_says() {
        assert_eq!(kind_of(16), Kind::Boolean);
        assert_eq!(kind_of(17), Kind::Blob);
        for oid in [20u32, 21, 23, 26] {
            assert_eq!(kind_of(oid), Kind::Integer, "oid {oid}");
        }
        for oid in [700u32, 701] {
            assert_eq!(kind_of(oid), Kind::Real, "oid {oid}");
        }
        assert_eq!(kind_of(1700), Kind::Decimal);
        // text, varchar, uuid, json, jsonb, timestamptz, interval, an array,
        // and something no version of this client has heard of.
        for oid in [25u32, 1043, 2950, 114, 3802, 1184, 1186, 1007, 999_999] {
            assert_eq!(kind_of(oid), Kind::Text, "oid {oid}");
        }
    }

    /// Values are carried, not reinterpreted: a numeric keeps its digits, a
    /// null stays a null, and a boolean becomes this dialect's 0 or 1.
    #[test]
    fn values_are_carried_rather_than_reinterpreted() {
        assert_eq!(carry(None, Kind::Integer).unwrap(), OwnedDatum::Null);
        assert_eq!(
            carry(Some(b"42"), Kind::Integer).unwrap(),
            OwnedDatum::Int(42)
        );
        assert_eq!(
            carry(Some(b"t"), Kind::Boolean).unwrap(),
            OwnedDatum::Int(1)
        );
        assert_eq!(
            carry(Some(b"f"), Kind::Boolean).unwrap(),
            OwnedDatum::Int(0)
        );
        assert_eq!(
            carry(Some(b"1.5"), Kind::Real).unwrap(),
            OwnedDatum::Real(1.5)
        );
        // The one that matters: an exact decimal keeps every digit it had.
        assert_eq!(
            carry(Some(b"12345678901234567890.1234567890"), Kind::Decimal).unwrap(),
            OwnedDatum::Text(b"12345678901234567890.1234567890".to_vec())
        );
    }

    /// An integer too wide for this dialect is carried as its digits rather
    /// than rounded into a double.
    #[test]
    fn an_out_of_range_integer_keeps_its_digits() {
        assert_eq!(
            carry(Some(b"99999999999999999999"), Kind::Integer).unwrap(),
            OwnedDatum::Text(b"99999999999999999999".to_vec())
        );
    }

    /// Both `bytea` text forms decode to the same bytes.
    #[test]
    fn bytea_decodes_from_hex_and_from_escapes() {
        assert_eq!(decode_bytea(b"\\x00ff10"), vec![0x00, 0xff, 0x10]);
        assert_eq!(decode_bytea(b"abc"), b"abc".to_vec());
        assert_eq!(decode_bytea(b"a\\\\b"), b"a\\b".to_vec());
        assert_eq!(decode_bytea(b"\\000\\377"), vec![0x00, 0xff]);
    }

    /// A `RowDescription` and a `DataRow` decode into their parts, including a
    /// null, which is a length of -1 rather than a zero-length value.
    #[test]
    fn a_row_description_and_a_data_row_decode() {
        let mut description = Vec::new();
        description.extend_from_slice(&1u16.to_be_bytes());
        description.extend_from_slice(b"id\0");
        description.extend_from_slice(&0u32.to_be_bytes());
        description.extend_from_slice(&1u16.to_be_bytes());
        description.extend_from_slice(&23u32.to_be_bytes());
        description.extend_from_slice(&4u16.to_be_bytes());
        description.extend_from_slice(&0u32.to_be_bytes());
        description.extend_from_slice(&0u16.to_be_bytes());
        let fields = decode_row_description(&description).expect("decodes");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "id");
        assert_eq!(fields[0].type_oid, 23);

        let mut row = Vec::new();
        row.extend_from_slice(&2u16.to_be_bytes());
        row.extend_from_slice(&2i32.to_be_bytes());
        row.extend_from_slice(b"42");
        row.extend_from_slice(&(-1i32).to_be_bytes());
        let values = decode_data_row(&row).expect("decodes");
        assert_eq!(values, vec![Some(b"42".to_vec()), None]);
    }

    /// A truncated `DataRow` is an error naming the problem, not a panic and
    /// not a shorter row.
    #[test]
    fn a_truncated_data_row_is_an_error() {
        let mut row = Vec::new();
        row.extend_from_slice(&1u16.to_be_bytes());
        row.extend_from_slice(&9i32.to_be_bytes());
        row.extend_from_slice(b"short");
        assert!(decode_data_row(&row).is_err());
    }

    /// An `ErrorResponse` names its SQLSTATE, so a caller can act on the code
    /// rather than on the server's English.
    #[test]
    fn an_error_response_names_its_sqlstate() {
        let mut body = Vec::new();
        body.extend_from_slice(b"SFATAL\0");
        body.extend_from_slice(b"C28P01\0");
        body.extend_from_slice(b"Mpassword authentication failed for user \"nobody\"\0");
        body.push(0);
        let error = decode_error(&body);
        assert!(error.message().contains("28P01"), "{}", error.message());
    }

    /// A schema other than `public` is folded into the destination's name, so
    /// two same-named tables in two schemas cannot become one.
    #[test]
    fn a_non_public_schema_is_folded_into_the_target_name() {
        assert_eq!(target_name("public", "note"), "note");
        assert_eq!(target_name("sales", "order"), "sales__order");
        assert_ne!(
            target_name("sales", "order"),
            target_name("archive", "order")
        );
    }

    /// The client nonce carries no comma, which is the character that separates
    /// SCRAM attributes.
    #[test]
    fn the_client_nonce_cannot_split_a_scram_message() {
        for _ in 0..32 {
            let nonce = client_nonce();
            assert!(!nonce.contains(','), "{nonce}");
            assert!(!nonce.is_empty());
        }
    }

    /// The SCRAM computation, against RFC 7677's worked example. This is what
    /// says the exchange is SCRAM-SHA-256 and not something shaped like it.
    #[test]
    fn the_scram_proof_matches_rfc_7677() {
        let password = "pencil";
        let client_first_bare = "n=user,r=rOprNGfwEbeRWgbNEkqO";
        let server_first =
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let client_final_bare = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let salt = base64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").expect("decodes");
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, 4096);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let auth_message = format!("{client_first_bare},{server_first},{client_final_bare}");
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof = xor(&client_key, &client_signature);
        assert_eq!(
            base64_encode(&proof),
            "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        let server_key = hmac_sha256(&salted, b"Server Key");
        let signature = hmac_sha256(&server_key, auth_message.as_bytes());
        assert_eq!(
            base64_encode(&signature),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }

    /// A SCRAM message splits into its attributes, including one whose value
    /// holds an `=` of its own - a base64 salt always does.
    #[test]
    fn scram_attributes_survive_a_value_holding_an_equals() {
        let parsed = scram_attributes("r=abc,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096");
        assert_eq!(parsed.get("r").map(String::as_str), Some("abc"));
        assert_eq!(
            parsed.get("s").map(String::as_str),
            Some("W22ZaJ0SNY7soEsUEjb6gQ==")
        );
        assert_eq!(parsed.get("i").map(String::as_str), Some("4096"));
    }
}
