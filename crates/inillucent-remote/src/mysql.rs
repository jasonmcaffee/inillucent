//! MySQL and MariaDB, over the client/server protocol.
//!
//! The subset a reader needs: the version 10 handshake, two authentication
//! plugins, `COM_QUERY` and the text resultset. Everything is framed the same
//! way - a three-byte little-endian payload length, a one-byte sequence number,
//! then the payload - and a payload of exactly `0xffffff` bytes means the
//! message continues in the next packet, which is why [`Connection::packet`]
//! concatenates rather than returning one frame.
//!
//! **`caching_sha2_password` full authentication is refused, not faked.** Its
//! fast path - the server already holds the password in its cache, or the
//! account uses `mysql_native_password` - works over a plain socket and is what
//! this client speaks. The full path requires either TLS or an RSA public-key
//! exchange, and this client has neither. Pretending otherwise would surface as
//! "the server rejected my password", which is a wrong diagnosis of a correct
//! password, so the refusal names the two ways out instead.
//!
//! Invariant: **every number this client reads out of a packet is bounded
//! before it sizes anything.** A server can be hostile, buggy, or reached
//! through a proxy, so a length-encoded count is a number an attacker chooses:
//! the 256 MiB message cap bounds the packet and not the numbers inside it.

use std::collections::BTreeMap;
use std::time::Duration;

use inillucent_base::hash::sha256;
use inillucent_base::DbResult;
use inillucent_tree::datum::OwnedDatum;

use crate::auth::{sha1, xor};
use crate::source::{Kind, RemoteSource, SourceColumn, SourceTable};
use crate::stream::{protocol, will_not, Stream};
use crate::url::{ConnectionUrl, Transport};

/// The capabilities this client advertises.
mod capability {
    /// Understands the 4.1 password hashing.
    pub const LONG_PASSWORD: u32 = 0x0000_0001;
    /// Wants the wider column flags.
    pub const LONG_FLAG: u32 = 0x0000_0004;
    /// The handshake names a database.
    pub const CONNECT_WITH_DB: u32 = 0x0000_0008;
    /// The 4.1 protocol, which every server since 2004 speaks.
    pub const PROTOCOL_41: u32 = 0x0000_0200;
    /// Transactions, which the snapshot needs.
    pub const TRANSACTIONS: u32 = 0x0000_2000;
    /// The rest of the connection is TLS.
    pub const SSL: u32 = 0x0000_0800;
    /// The 4.1 authentication exchange.
    pub const SECURE_CONNECTION: u32 = 0x0000_8000;
    /// The handshake names its authentication plugin.
    pub const PLUGIN_AUTH: u32 = 0x0008_0000;
    /// The authentication response carries its own length.
    pub const PLUGIN_AUTH_LENENC: u32 = 0x0020_0000;
    /// A resultset ends with an OK packet rather than an EOF one.
    pub const DEPRECATE_EOF: u32 = 0x0100_0000;
}

/// The `utf8mb4_general_ci` collation id, so every string arrives as UTF-8.
const CHARSET_UTF8MB4: u8 = 45;

/// The collation id that means "these bytes are not text".
const CHARSET_BINARY: u16 = 63;

/// `COM_QUERY`.
const COM_QUERY: u8 = 0x03;

/// `COM_QUIT`.
const COM_QUIT: u8 = 0x01;

/// The most columns a result set may claim.
///
/// **MySQL's own hard limit, used here as a bound on a number the server
/// chooses (task-1932, H5).** `stream_query` read a length-encoded column count
/// out of the first packet of a result set and ran `Vec::with_capacity` on it,
/// and `lenenc_read` decodes up to `u64::MAX` - so a twelve-byte packet claiming
/// `u64::MAX` columns asked for an allocation that panics with capacity overflow
/// or exhausts memory, before the first row. The 256 MiB message cap in
/// `stream.rs` bounds the packet, not a number inside it.
///
/// A migration talks to a server somebody else runs, possibly through a proxy,
/// so the count is untrusted in exactly the way a database page is. Refusing
/// above MySQL's own limit costs a real server nothing and turns a hostile one
/// into a named `protocol` error.
const MAX_COLUMNS: u64 = 4096;

/// One column of a result set, as `ColumnDefinition41` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The column's name.
    pub name: String,
    /// The wire type code.
    pub type_code: u8,
    /// The collation, whose `63` means the bytes are binary rather than text.
    pub charset: u16,
    /// The column's flags, whose `0x20` means unsigned.
    pub flags: u16,
}

/// A whole result set, buffered.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    /// The columns, in order.
    pub fields: Vec<Field>,
    /// The rows, each value either absent (SQL NULL) or its bytes.
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

/// A connection to a MySQL or MariaDB server.
pub struct MysqlSource {
    /// The socket.
    stream: Stream,
    /// The next sequence number to write.
    sequence: u8,
    /// What the server calls itself.
    version: String,
    /// What the two sides agreed on.
    capabilities: u32,
    /// The URL, kept for the report - and it redacts its own password.
    url: ConnectionUrl,
    /// Whether a read snapshot is open.
    in_snapshot: bool,
}

/// What a streaming read hands each row to: the columns it is shaped by, and
/// the row's values, each absent for a SQL NULL.
pub type RowSink<'s> = dyn FnMut(&[Field], &[Option<Vec<u8>>]) -> DbResult<()> + 's;

impl MysqlSource {
    /// Connects, authenticates, and opens the read snapshot.
    ///
    /// @param url - where to connect and as who
    pub fn connect(url: &ConnectionUrl) -> DbResult<MysqlSource> {
        MysqlSource::connect_over(url, Transport::VerifiedTls)
    }

    /// Connects on a transport the caller's policy has already decided.
    ///
    /// **The upgrade is inside the handshake, not before it.** MySQL has no
    /// separate request message: the client reads the server's greeting, sends
    /// a *truncated* handshake response carrying nothing but the capability
    /// flags with `CLIENT_SSL` set, upgrades the socket, and then sends the
    /// real response - user name, database and password - over TLS. So the
    /// order still puts every credential after the certificate check, which is
    /// the property that matters, and the server's greeting is the only thing
    /// that crosses in the clear.
    ///
    /// @param url - where to connect and as who
    /// @param transport - what the policy in `ConnectionUrl::transport` decided
    pub fn connect_over(url: &ConnectionUrl, transport: Transport) -> DbResult<MysqlSource> {
        let stream = Stream::connect(&url.address(), Duration::from_secs(url.timeout_seconds()))?;
        MysqlSource::over_transport(stream, url, transport)
    }

    /// Wraps an already-connected socket and runs the handshake on it.
    ///
    /// The seam the protocol tests drive. It does everything
    /// [`MysqlSource::connect`] does except dial, snapshot included - a seam
    /// that stopped one step short would be testing a sequence no real run ever
    /// performs.
    ///
    /// @param stream - a connected socket
    /// @param url - the credentials to authenticate with
    pub fn over(stream: Stream, url: &ConnectionUrl) -> DbResult<MysqlSource> {
        MysqlSource::over_transport(stream, url, Transport::Plaintext)
    }

    /// The same, on a stated transport.
    ///
    /// @param stream - a connected socket
    /// @param url - the credentials to authenticate with
    /// @param transport - what the policy decided
    pub fn over_transport(
        stream: Stream,
        url: &ConnectionUrl,
        transport: Transport,
    ) -> DbResult<MysqlSource> {
        let mut source = MysqlSource {
            stream,
            sequence: 0,
            version: String::new(),
            capabilities: 0,
            url: url.clone(),
            in_snapshot: false,
        };
        source.handshake(transport)?;
        source.open_snapshot()?;
        Ok(source)
    }

    /// Returns what the peer proved it was, when the connection is encrypted.
    pub fn peer(&self) -> Option<&str> {
        self.stream.peer()
    }

    /// Reads the server's greeting and answers it.
    ///
    /// @param transport - whether to upgrade the socket before authenticating
    fn handshake(&mut self, transport: Transport) -> DbResult<()> {
        let greeting = self.packet()?;
        let hello = Greeting::decode(&greeting)?;
        self.version = hello.version.clone();

        let mut capabilities = capability::LONG_PASSWORD
            | capability::LONG_FLAG
            | capability::PROTOCOL_41
            | capability::TRANSACTIONS
            | capability::SECURE_CONNECTION
            | capability::CONNECT_WITH_DB
            | capability::PLUGIN_AUTH
            | capability::PLUGIN_AUTH_LENENC;
        // Only claimed when the server offers it: a client that asked for a
        // resultset terminator the server does not send would wait for one.
        if hello.capabilities & capability::DEPRECATE_EOF != 0 {
            capabilities |= capability::DEPRECATE_EOF;
        }
        if transport == Transport::VerifiedTls {
            if hello.capabilities & capability::SSL == 0 {
                return Err(will_not(format!(
                    "{} does not offer TLS, so this migration stopped before it sent a password. \
                     Turn TLS on at the server, or - only for a loopback address or a network you \
                     trust - write ssl-mode=disable in the URL and pass --insecure-plaintext.",
                    self.url.address()
                )));
            }
            capabilities |= capability::SSL;
        }
        self.capabilities = capabilities;

        let password = self.url.password.clone().or_else(|| {
            std::env::var("MYSQL_PWD")
                .ok()
                .filter(|password| !password.is_empty())
        });
        let response = auth_response(&hello.plugin, password.as_deref(), &hello.scramble)?;

        // The 32-byte prefix every handshake response begins with. Sent on its
        // own first when TLS was asked for: that truncated packet is how the
        // protocol says "everything after this is encrypted", and the user
        // name that would follow it is the first thing worth hiding.
        let mut prefix: Vec<u8> = Vec::new();
        prefix.extend_from_slice(&capabilities.to_le_bytes());
        // The largest packet this client will accept, which is the protocol's
        // own ceiling rather than a number of our own: a row wider than this is
        // split by the server, and `packet` reassembles it.
        prefix.extend_from_slice(&0x0100_0000u32.to_le_bytes());
        prefix.push(CHARSET_UTF8MB4);
        prefix.extend_from_slice(&[0u8; 23]);
        if transport == Transport::VerifiedTls {
            self.write_packet(&prefix)?;
            let host = self.url.host.clone();
            let root = self.url.root_certificate().map(str::to_string);
            self.stream.upgrade(&host, root.as_deref())?;
        }

        let mut body: Vec<u8> = prefix;
        body.extend_from_slice(self.url.user.as_bytes());
        body.push(0);
        body.extend_from_slice(&lenenc_int(response.len() as u64));
        body.extend_from_slice(&response);
        body.extend_from_slice(self.url.database.as_bytes());
        body.push(0);
        body.extend_from_slice(hello.plugin.as_bytes());
        body.push(0);
        self.write_packet(&body)?;

        loop {
            let reply = self.packet()?;
            match reply.first() {
                Some(0x00) => return Ok(()),
                Some(0xff) => return Err(decode_error(&reply)),
                // AuthMoreData, which for caching_sha2_password says whether
                // the fast path succeeded.
                Some(0x01) => {
                    match reply.get(1) {
                        Some(0x03) => continue,
                        Some(0x04) => return Err(will_not(
                            "the account uses caching_sha2_password and the server's cache does \
                             not hold it, so it is asking for the full RSA or TLS exchange this \
                             migration client does not speak. Connect once with the mysql client \
                             to prime the cache, or run the migration as an account created with \
                             IDENTIFIED WITH mysql_native_password.",
                        )),
                        other => {
                            return Err(protocol(format!(
                            "the server sent auth data this client does not understand: {other:?}"
                        )))
                        }
                    }
                }
                // AuthSwitchRequest: recompute with the plugin it names.
                Some(0xfe) => {
                    let mut at = 1usize;
                    let plugin = read_cstring(&reply, &mut at);
                    let scramble: Vec<u8> = reply
                        .get(at..)
                        .unwrap_or_default()
                        .iter()
                        .copied()
                        .take_while(|byte| *byte != 0)
                        .collect();
                    let switched = auth_response(&plugin, password.as_deref(), &scramble)?;
                    self.write_packet(&switched)?;
                }
                other => {
                    return Err(protocol(format!(
                        "unexpected handshake reply {other:?} from the server"
                    )))
                }
            }
        }
    }

    /// Opens the one snapshot every read happens inside.
    fn open_snapshot(&mut self) -> DbResult<()> {
        self.execute("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")?;
        // **The session's zone is pinned, so a `TIMESTAMP` does not depend on
        // where the migration was run from (task-1932, M10).** MySQL stores a
        // `TIMESTAMP` in UTC and renders it in the *session's* `time_zone`,
        // which defaults to the server's - so the same table migrated from a
        // laptop in Denver and from a server in UTC carried two different
        // strings for one instant. The destination's text then depended on who
        // ran the tool, and `RowDigest` hashes the carried bytes, so a resumed
        // migration that copied correctly failed its own verification.
        //
        // `+00:00` rather than `UTC`, because the named zones need the
        // `mysql.time_zone` tables loaded and an offset never does. The
        // PostgreSQL client pins the same three renderings in its startup
        // packet; see `postgres::start_up`.
        self.execute("SET SESSION time_zone = '+00:00'")?;
        // `WITH CONSISTENT SNAPSHOT` is InnoDB's, and it is what makes the read
        // as of one instant rather than as of whenever each table was reached.
        // A server whose tables are MyISAM answers it and ignores it, which is
        // a property of that engine rather than of this client.
        self.execute("START TRANSACTION WITH CONSISTENT SNAPSHOT")?;
        self.in_snapshot = true;
        Ok(())
    }

    /// Reads one whole message, reassembling a payload split across packets.
    fn packet(&mut self) -> DbResult<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let header = self.stream.read_exact(4)?;
            let length = usize::from(*header.first().unwrap_or(&0))
                | (usize::from(*header.get(1).unwrap_or(&0)) << 8)
                | (usize::from(*header.get(2).unwrap_or(&0)) << 16);
            self.sequence = header.get(3).unwrap_or(&0).wrapping_add(1);
            out.extend_from_slice(&self.stream.read_exact(length)?);
            if length < 0x00ff_ffff {
                return Ok(out);
            }
        }
    }

    /// Writes one message, splitting a payload the protocol cannot frame whole.
    ///
    /// @param body - the payload
    fn write_packet(&mut self, body: &[u8]) -> DbResult<()> {
        let mut at = 0usize;
        loop {
            let taking = body.len().saturating_sub(at).min(0x00ff_fffe);
            let chunk = body.get(at..at.saturating_add(taking)).unwrap_or(&[]);
            let mut framed = vec![
                (taking & 0xff) as u8,
                ((taking >> 8) & 0xff) as u8,
                ((taking >> 16) & 0xff) as u8,
                self.sequence,
            ];
            self.sequence = self.sequence.wrapping_add(1);
            framed.extend_from_slice(chunk);
            self.stream.write_all(&framed)?;
            at = at.saturating_add(taking);
            if at >= body.len() {
                return Ok(());
            }
        }
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
    /// @param sql - the query
    /// @param sink - what to do with each row
    pub fn stream_query(&mut self, sql: &str, sink: &mut RowSink<'_>) -> DbResult<u64> {
        // Every command starts a new sequence.
        self.sequence = 0;
        let mut body = vec![COM_QUERY];
        body.extend_from_slice(sql.as_bytes());
        self.write_packet(&body)?;

        let first = self.packet()?;
        match first.first() {
            Some(0x00) => return Ok(0),
            Some(0xff) => return Err(decode_error(&first)),
            Some(0xfb) => {
                return Err(will_not(
                    "the server asked for a LOCAL INFILE upload, which a read-only migration \
                     never sends",
                ))
            }
            _ => {}
        }
        let columns = column_count(&first)?;

        let mut fields = Vec::with_capacity(columns);
        for _ in 0..columns {
            fields.push(decode_column(&self.packet()?)?);
        }
        // A server without DEPRECATE_EOF sends an EOF packet between the column
        // definitions and the rows.
        if self.capabilities & capability::DEPRECATE_EOF == 0 {
            let terminator = self.packet()?;
            if terminator.first() != Some(&0xfe) {
                return Err(protocol(
                    "the column definitions were not followed by the EOF this server's \
                     capabilities promised",
                ));
            }
        }

        let mut rows = 0u64;
        loop {
            let packet = self.packet()?;
            match packet.first() {
                Some(0xff) => return Err(decode_error(&packet)),
                // An EOF or OK terminator: 0xfe with a payload short enough not
                // to be a row whose first column is 0xfe bytes long.
                Some(0xfe) if packet.len() < 9 => break,
                _ => {}
            }
            let row = decode_row(&packet, columns)?;
            rows = rows.saturating_add(1);
            sink(&fields, &row)?;
        }
        Ok(rows)
    }
}

impl RemoteSource for MysqlSource {
    /// Reads every base table of the connected database.
    fn describe(&mut self) -> DbResult<Vec<SourceTable>> {
        let columns = self.query(COLUMN_QUERY)?;
        let keys = self.query(KEY_QUERY)?;

        let mut key_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for at in 0..keys.rows.len() {
            key_of
                .entry(keys.text(at, 0))
                .or_default()
                .push(keys.text(at, 1));
        }

        let mut tables: Vec<SourceTable> = Vec::new();
        for at in 0..columns.rows.len() {
            let name = columns.text(at, 0);
            let declared = columns.text(at, 3);
            let column = SourceColumn {
                name: columns.text(at, 1),
                kind: kind_of(&columns.text(at, 2), &declared),
                declared,
                nullable: columns.text(at, 4).eq_ignore_ascii_case("yes"),
            };
            match tables.iter_mut().find(|table| table.name == name) {
                Some(table) => table.columns.push(column),
                None => tables.push(SourceTable {
                    target: name.clone(),
                    primary_key: key_of.get(&name).cloned().unwrap_or_default(),
                    schema: self.url.database.clone(),
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
            back_quoted(&table.name)
        ))?;
        result.text(0, 0).parse::<u64>().map_err(|_| {
            protocol(format!(
                "count(*) on {} did not answer a number",
                table.name
            ))
        })
    }

    /// Streams every row of a table.
    fn scan(
        &mut self,
        table: &SourceTable,
        sink: &mut dyn FnMut(&[OwnedDatum]) -> DbResult<()>,
    ) -> DbResult<u64> {
        let selected = table
            .columns
            .iter()
            .map(|column| back_quoted(&column.name))
            .collect::<Vec<String>>()
            .join(", ");
        let kinds: Vec<Kind> = table.columns.iter().map(|column| column.kind).collect();
        self.stream_query(
            &format!("SELECT {selected} FROM {}", back_quoted(&table.name)),
            &mut |_, row| {
                let mut values = Vec::with_capacity(row.len());
                for (at, value) in row.iter().enumerate() {
                    values.push(carry(
                        value.as_deref(),
                        *kinds.get(at).unwrap_or(&Kind::Text),
                    ));
                }
                sink(&values)
            },
        )
    }

    /// Returns what the server calls itself.
    fn server(&self) -> String {
        if self.version.to_ascii_lowercase().contains("mariadb") {
            format!("MariaDB {}", self.version)
        } else {
            format!("MySQL {}", self.version)
        }
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
        self.sequence = 0;
        let _ = self.write_packet(&[COM_QUIT]);
        self.stream.close();
    }
}

/// Every base table's columns, in ordinal order.
const COLUMN_QUERY: &str = "\
SELECT c.TABLE_NAME, c.COLUMN_NAME, c.DATA_TYPE, c.COLUMN_TYPE, c.IS_NULLABLE \
  FROM information_schema.COLUMNS c \
  JOIN information_schema.TABLES t \
    ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
 WHERE c.TABLE_SCHEMA = DATABASE() AND t.TABLE_TYPE = 'BASE TABLE' \
 ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION";

/// Each table's primary key, in key order.
const KEY_QUERY: &str = "\
SELECT TABLE_NAME, COLUMN_NAME \
  FROM information_schema.KEY_COLUMN_USAGE \
 WHERE TABLE_SCHEMA = DATABASE() AND CONSTRAINT_NAME = 'PRIMARY' \
 ORDER BY TABLE_NAME, ORDINAL_POSITION";

/// What exists in the source and does not come across.
const NOT_CARRIED_QUERY: &str = "\
SELECT 'view', TABLE_NAME FROM information_schema.TABLES \
 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'VIEW' \
UNION ALL \
SELECT 'trigger', TRIGGER_NAME FROM information_schema.TRIGGERS \
 WHERE TRIGGER_SCHEMA = DATABASE() \
UNION ALL \
SELECT LOWER(ROUTINE_TYPE), ROUTINE_NAME FROM information_schema.ROUTINES \
 WHERE ROUTINE_SCHEMA = DATABASE() \
 ORDER BY 1, 2";

/// The server's greeting, decoded.
#[derive(Clone, Debug)]
struct Greeting {
    /// The version string the server reports.
    version: String,
    /// What the server can do.
    capabilities: u32,
    /// The authentication challenge, both halves joined.
    scramble: Vec<u8>,
    /// Which authentication plugin the account uses.
    plugin: String,
}

impl Greeting {
    /// Decodes a version 10 handshake packet.
    ///
    /// @param body - the packet's payload
    fn decode(body: &[u8]) -> DbResult<Greeting> {
        if body.first() != Some(&10) {
            // A server that answers the connection with an error packet -
            // "Host is not allowed to connect" is the usual one - says so here
            // rather than in the login.
            if body.first() == Some(&0xff) {
                return Err(decode_error(body));
            }
            return Err(protocol(format!(
                "the server greeted with protocol version {:?}; this client speaks 10",
                body.first()
            )));
        }
        let mut at = 1usize;
        let version = read_cstring(body, &mut at);
        // connection id (4)
        at = at.saturating_add(4);
        let mut scramble: Vec<u8> = body
            .get(at..at.saturating_add(8))
            .ok_or_else(|| protocol("the greeting carried no challenge"))?
            .to_vec();
        // the 8 challenge bytes plus one filler
        at = at.saturating_add(9);
        let lower = le_u16(body, at);
        at = at.saturating_add(2);
        if body.len() <= at {
            return Err(protocol("the greeting ended before its capability flags"));
        }
        // character set (1), status flags (2)
        at = at.saturating_add(3);
        let upper = le_u16(body, at);
        at = at.saturating_add(2);
        let capabilities = u32::from(lower) | (u32::from(upper) << 16);
        let challenge_length = usize::from(*body.get(at).unwrap_or(&0));
        at = at.saturating_add(1);
        // reserved (10)
        at = at.saturating_add(10);
        if capabilities & capability::SECURE_CONNECTION != 0 {
            let rest = challenge_length.saturating_sub(8).max(13);
            let more = body
                .get(at..at.saturating_add(rest))
                .ok_or_else(|| protocol("the greeting's challenge is shorter than it declared"))?;
            // The declared length counts a trailing NUL that is not part of the
            // challenge; a client that kept it computes a wrong hash and is
            // told its password is wrong.
            scramble
                .extend_from_slice(more.get(..more.len().saturating_sub(1)).unwrap_or_default());
            at = at.saturating_add(rest);
        }
        let plugin = if capabilities & capability::PLUGIN_AUTH != 0 {
            read_cstring(body, &mut at)
        } else {
            "mysql_native_password".to_string()
        };
        Ok(Greeting {
            version,
            capabilities,
            scramble,
            plugin,
        })
    }
}

/// Returns the login response one authentication plugin expects.
///
/// @param plugin - the plugin the server named
/// @param password - the password, when the URL or the environment holds one
/// @param scramble - the server's challenge
pub fn auth_response(plugin: &str, password: Option<&str>, scramble: &[u8]) -> DbResult<Vec<u8>> {
    let Some(password) = password.filter(|password| !password.is_empty()) else {
        // An account with no password answers with an empty response, which is
        // what a `trust`-equivalent MySQL account expects.
        return Ok(Vec::new());
    };
    match plugin {
        "mysql_native_password" => {
            // SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))
            let first = sha1(password.as_bytes());
            let second = sha1(&first);
            let mut seed = scramble.to_vec();
            seed.extend_from_slice(&second);
            Ok(xor(&first, &sha1(&seed)))
        }
        "caching_sha2_password" => {
            // SHA256(password) XOR SHA256(SHA256(SHA256(password)) || scramble)
            let first = sha256(password.as_bytes());
            let second = sha256(&first);
            let mut seed = second.to_vec();
            seed.extend_from_slice(scramble);
            Ok(xor(&first, &sha256(&seed)))
        }
        "mysql_clear_password" => {
            let mut out = password.as_bytes().to_vec();
            out.push(0);
            Ok(out)
        }
        other => Err(will_not(format!(
            "the account authenticates with the {other} plugin, which this migration client does \
             not speak. It speaks mysql_native_password and caching_sha2_password's fast path."
        ))),
    }
}

/// Returns how a MySQL type is carried.
///
/// @param data_type - `information_schema.COLUMNS.DATA_TYPE`, already lower case
/// @param column_type - the full declaration, which is where `unsigned` lives
pub fn kind_of(data_type: &str, column_type: &str) -> Kind {
    let unsigned = column_type.to_ascii_lowercase().contains("unsigned");
    match data_type.to_ascii_lowercase().as_str() {
        // **An unsigned 64-bit integer does not fit a signed one**, and half of
        // its range would round through a double. Its digits are carried
        // instead, which is exact.
        "bigint" if unsigned => Kind::Decimal,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            Kind::Integer
        }
        "float" | "double" | "real" => Kind::Real,
        "decimal" | "numeric" | "dec" | "fixed" => Kind::Decimal,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit"
        | "geometry" | "point" | "linestring" | "polygon" => Kind::Blob,
        _ => Kind::Text,
    }
}

/// Converts one wire value into what the destination stores.
///
/// The text protocol renders every value as bytes, so a blob's "text" is the
/// blob - there is no decoding step, and adding one would corrupt it.
///
/// @param value - the value's bytes, or `None` for SQL NULL
/// @param kind - how this column is carried
pub fn carry(value: Option<&[u8]>, kind: Kind) -> OwnedDatum {
    let Some(bytes) = value else {
        return OwnedDatum::Null;
    };
    match kind {
        Kind::Integer | Kind::Boolean => match String::from_utf8_lossy(bytes).trim().parse::<i64>()
        {
            Ok(number) => OwnedDatum::Int(number),
            Err(_) => OwnedDatum::Text(bytes.to_vec()),
        },
        Kind::Real => match String::from_utf8_lossy(bytes).trim().parse::<f64>() {
            Ok(number) => OwnedDatum::Real(number),
            Err(_) => OwnedDatum::Text(bytes.to_vec()),
        },
        Kind::Decimal | Kind::Text => OwnedDatum::Text(bytes.to_vec()),
        Kind::Blob => OwnedDatum::Blob(bytes.to_vec()),
    }
}

/// Returns an identifier quoted for MySQL.
///
/// @param name - the identifier, which may contain a backtick
fn back_quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Decodes a `ColumnDefinition41` packet.
///
/// @param body - the packet's payload
fn decode_column(body: &[u8]) -> DbResult<Field> {
    let mut at = 0usize;
    // catalog, schema, table, org_table - four length-encoded strings this
    // reader has no use for, walked past rather than copied.
    for _ in 0..4 {
        skip_lenenc_string(body, &mut at);
    }
    let name = read_lenenc_string(body, &mut at)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    // org_name, then the fixed-length block.
    skip_lenenc_string(body, &mut at);
    // The block's own length, always 0x0c.
    let _ = lenenc_read(body, &mut at);
    let charset = le_u16(body, at);
    at = at.saturating_add(2);
    // column length (4)
    at = at.saturating_add(4);
    let type_code = *body
        .get(at)
        .ok_or_else(|| protocol("a column definition carried no type"))?;
    at = at.saturating_add(1);
    let flags = le_u16(body, at);
    Ok(Field {
        name,
        type_code,
        charset,
        flags,
    })
}

/// Decodes one text-protocol row.
///
/// @param body - the packet's payload
/// @param columns - how many values it must hold
fn decode_row(body: &[u8], columns: usize) -> DbResult<Vec<Option<Vec<u8>>>> {
    // **Checked here as well as at the call site.** `stream_query` bounds the
    // count it decodes, and this is a separate entry point that a later caller
    // could reach with a number from somewhere else; a bound that only one of
    // two doors carries is a bound somebody walks past (task-1932, H5).
    if columns as u64 > MAX_COLUMNS {
        return Err(protocol(format!(
            "a row claims {columns} values, past the {MAX_COLUMNS} columns a MySQL server can have"
        )));
    }
    let mut at = 0usize;
    let mut row = Vec::with_capacity(columns);
    for _ in 0..columns {
        match body.get(at) {
            Some(0xfb) => {
                row.push(None);
                at = at.saturating_add(1);
            }
            Some(_) => {
                let value = read_lenenc_string(body, &mut at)
                    .ok_or_else(|| protocol("a row value runs past the packet"))?;
                row.push(Some(value));
            }
            None => return Err(protocol("a row ended before all of its columns")),
        }
    }
    Ok(row)
}

/// Decodes an ERR packet into an error naming its code and SQL state.
///
/// @param body - the packet's payload
fn decode_error(body: &[u8]) -> inillucent_base::error::DbError {
    let code = le_u16(body, 1);
    let mut at = 3usize;
    let state = if body.get(at) == Some(&b'#') {
        let state = String::from_utf8_lossy(
            body.get(at.saturating_add(1)..at.saturating_add(6))
                .unwrap_or_default(),
        )
        .into_owned();
        at = at.saturating_add(6);
        state
    } else {
        String::new()
    };
    let message = String::from_utf8_lossy(body.get(at..).unwrap_or_default()).into_owned();
    if state.is_empty() {
        will_not(format!("mysql {code}: {message}"))
    } else {
        will_not(format!("mysql {code} ({state}): {message}"))
    }
}

/// Reads a NUL-terminated string, advancing past its terminator.
///
/// @param body - the packet
/// @param at - where to start, advanced past the string
fn read_cstring(body: &[u8], at: &mut usize) -> String {
    let start = *at;
    let mut end = start;
    while body.get(end).is_some_and(|byte| *byte != 0) {
        end = end.saturating_add(1);
    }
    let text = String::from_utf8_lossy(body.get(start..end).unwrap_or_default()).into_owned();
    *at = end.saturating_add(1);
    text
}

/// Reads a length-encoded integer, advancing past it.
///
/// @param body - the packet
/// @param at - where to start, advanced past the integer
fn lenenc_read(body: &[u8], at: &mut usize) -> Option<u64> {
    let first = *body.get(*at)?;
    *at = at.saturating_add(1);
    let width = match first {
        0xfc => 2usize,
        0xfd => 3,
        0xfe => 8,
        0xfb => return None,
        _ => return Some(u64::from(first)),
    };
    let bytes = body.get(*at..at.saturating_add(width))?;
    *at = at.saturating_add(width);
    let mut value = 0u64;
    for (index, byte) in bytes.iter().enumerate() {
        value |= u64::from(*byte) << (index.saturating_mul(8));
    }
    Some(value)
}

/// Returns how many columns a result set's first packet claims.
///
/// Separate from `stream_query` so the bound can be asserted without a live
/// server: the packet an attacker sends is twelve bytes, and a test that needed
/// a socket to show that would not be written.
///
/// @param first - the first packet of a result set
fn column_count(first: &[u8]) -> DbResult<usize> {
    let mut at = 0usize;
    let claimed = lenenc_read(first, &mut at)
        .ok_or_else(|| protocol("the result set did not begin with a column count"))?;
    if claimed > MAX_COLUMNS {
        return Err(protocol(format!(
            "the result set claims {claimed} columns, past the {MAX_COLUMNS} a MySQL server can have"
        )));
    }
    Ok(claimed as usize)
}

/// Reads a length-encoded string, advancing past it.
///
/// @param body - the packet
/// @param at - where to start, advanced past the string
fn read_lenenc_string(body: &[u8], at: &mut usize) -> Option<Vec<u8>> {
    let length = lenenc_read(body, at)? as usize;
    let bytes = body.get(*at..at.saturating_add(length))?.to_vec();
    *at = at.saturating_add(length);
    Some(bytes)
}

/// Advances past a length-encoded string without copying it.
///
/// @param body - the packet
/// @param at - where to start, advanced past the string
fn skip_lenenc_string(body: &[u8], at: &mut usize) {
    if let Some(length) = lenenc_read(body, at) {
        *at = at.saturating_add(length as usize);
    }
}

/// Encodes a length-encoded integer.
///
/// @param value - the number to encode
fn lenenc_int(value: u64) -> Vec<u8> {
    if value < 0xfb {
        vec![value as u8]
    } else if value <= 0xffff {
        let mut out = vec![0xfc];
        out.extend_from_slice(&(value as u16).to_le_bytes());
        out
    } else if value <= 0x00ff_ffff {
        let mut out = vec![0xfd];
        out.extend_from_slice((value as u32).to_le_bytes().get(..3).unwrap_or(&[0, 0, 0]));
        out
    } else {
        let mut out = vec![0xfe];
        out.extend_from_slice(&value.to_le_bytes());
        out
    }
}

/// Reads a little-endian `u16` at an offset, or zero past the end.
///
/// @param body - the packet
/// @param at - where the field starts
fn le_u16(body: &[u8], at: usize) -> u16 {
    u16::from(*body.get(at).unwrap_or(&0))
        | (u16::from(*body.get(at.saturating_add(1)).unwrap_or(&0)) << 8)
}

/// Returns whether a column's bytes are binary rather than text.
///
/// Kept public because it is the rule a reader has to know to tell a `TEXT`
/// column from a `BLOB` one when only a result set is available - the two share
/// every wire type code and differ only in this collation id.
///
/// @param field - the column definition
pub fn is_binary(field: &Field) -> bool {
    field.charset == CHARSET_BINARY
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H5 (task-1920): a column count the server chose is bounded before it
    /// sizes anything.
    ///
    /// **What it used to do.** `stream_query` decoded the length-encoded column
    /// count out of a result set's first packet and ran
    /// `Vec::with_capacity(columns)` on it. `lenenc_read` decodes up to
    /// `u64::MAX`, and the 256 MiB message cap in `stream.rs` bounds the packet
    /// rather than a number inside it - so a twelve-byte packet claiming
    /// `u64::MAX` columns panicked with capacity overflow or asked for an
    /// allocation that exhausted memory, before the first row of a migration.
    ///
    /// A migration talks to a server somebody else runs, possibly through a
    /// proxy, so this is a number an attacker chooses. The packets below are
    /// each exactly what such a server would send: one byte for a small count,
    /// `0xfc` and two, `0xfd` and three, `0xfe` and eight.
    #[test]
    fn a_column_count_above_the_servers_own_limit_is_refused() {
        // `0xfd` introduces a three-byte little-endian count.
        let mut ten_million = vec![0xfdu8];
        ten_million.extend_from_slice(&10_000_000u32.to_le_bytes()[..3]);
        let refused = column_count(&ten_million).expect_err("ten million columns is refused");
        assert!(
            refused.detail().unwrap_or_default().contains("10000000"),
            "the refusal must name the count it refused: {refused:?}"
        );

        // `0xfe` introduces an eight-byte one, which is where `u64::MAX` lives.
        let mut enormous = vec![0xfeu8];
        enormous.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(column_count(&enormous).is_err());

        // `0xfc` introduces two bytes, so 65,535 is the most a two-byte count
        // can claim - still past MySQL's own 4,096.
        let mut sixty_five_thousand = vec![0xfcu8];
        sixty_five_thousand.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(column_count(&sixty_five_thousand).is_err());

        // An ordinary result set is unaffected, which is the half a bound that
        // simply refused everything would break.
        assert_eq!(column_count(&[3u8]).expect("three columns"), 3);
        let mut four_thousand = vec![0xfcu8];
        four_thousand.extend_from_slice(&4_000u16.to_le_bytes());
        assert_eq!(column_count(&four_thousand).expect("four thousand"), 4_000);

        // And the row decoder carries the same bound, because it is a second
        // door into the same allocation.
        assert!(decode_row(&[], 10_000_000).is_err());
        assert!(decode_row(&[], 0).is_ok());
    }

    /// The `mysql_native_password` response, against the worked example every
    /// implementation of this protocol is checked with: the algorithm is
    /// `SHA1(pass) XOR SHA1(scramble || SHA1(SHA1(pass)))`, so recomputing it
    /// the long way here proves the short way above agrees.
    #[test]
    fn native_password_is_the_documented_three_hash_exchange() {
        let scramble: Vec<u8> = (1..=20u8).collect();
        let response =
            auth_response("mysql_native_password", Some("secret"), &scramble).expect("computes");
        assert_eq!(response.len(), 20);
        let first = sha1(b"secret");
        let second = sha1(&first);
        let mut seed = scramble.clone();
        seed.extend_from_slice(&second);
        let expected = xor(&first, &sha1(&seed));
        assert_eq!(response, expected);
        // And it is not the password, nor a plain hash of it.
        assert_ne!(response, first.to_vec());
        assert_ne!(response, b"secret".to_vec());
    }

    /// The `caching_sha2_password` fast-path response is 32 bytes and follows
    /// the documented XOR, which is a different shape from the SHA-1 one.
    #[test]
    fn caching_sha2_is_the_documented_sha256_exchange() {
        let scramble: Vec<u8> = (1..=20u8).collect();
        let response =
            auth_response("caching_sha2_password", Some("secret"), &scramble).expect("computes");
        assert_eq!(response.len(), 32);
        let first = sha256(b"secret");
        let second = sha256(&first);
        let mut seed = second.to_vec();
        seed.extend_from_slice(&scramble);
        assert_eq!(response, xor(&first, &sha256(&seed)));
    }

    /// An account with no password answers with nothing, which is what a
    /// password-less account expects - not an empty-string hash.
    #[test]
    fn a_passwordless_account_sends_an_empty_response() {
        assert!(auth_response("mysql_native_password", None, &[1, 2, 3])
            .expect("computes")
            .is_empty());
        assert!(auth_response("caching_sha2_password", Some(""), &[1, 2, 3])
            .expect("computes")
            .is_empty());
    }

    /// A plugin this client does not speak is refused by name rather than
    /// answered with bytes that will fail to authenticate.
    #[test]
    fn an_unknown_plugin_is_refused_by_name() {
        let error = auth_response("auth_gssapi_client", Some("x"), &[]).expect_err("refuses");
        assert!(
            error.message().contains("auth_gssapi_client"),
            "{}",
            error.message()
        );
    }

    /// The type map, asserted per declared type.
    #[test]
    fn the_type_map_carries_each_family_the_way_the_tdd_says() {
        assert_eq!(kind_of("int", "int(11)"), Kind::Integer);
        assert_eq!(kind_of("bigint", "bigint(20)"), Kind::Integer);
        // The one that would otherwise round: an unsigned 64-bit integer.
        assert_eq!(kind_of("bigint", "bigint(20) unsigned"), Kind::Decimal);
        assert_eq!(kind_of("double", "double"), Kind::Real);
        assert_eq!(kind_of("decimal", "decimal(38,10)"), Kind::Decimal);
        assert_eq!(kind_of("varbinary", "varbinary(64)"), Kind::Blob);
        assert_eq!(kind_of("longblob", "longblob"), Kind::Blob);
        for declared in ["varchar", "text", "json", "datetime", "enum", "set", "uuid"] {
            assert_eq!(kind_of(declared, declared), Kind::Text, "{declared}");
        }
    }

    /// Length-encoded integers round-trip across all four widths, which is the
    /// codec every string in this protocol is framed by.
    #[test]
    fn length_encoded_integers_round_trip() {
        for value in [
            0u64,
            1,
            250,
            251,
            0xff,
            0x1234,
            0xffff,
            0x12_3456,
            0x0100_0000,
        ] {
            let encoded = lenenc_int(value);
            let mut at = 0usize;
            assert_eq!(lenenc_read(&encoded, &mut at), Some(value), "{value}");
            assert_eq!(at, encoded.len(), "{value}");
        }
    }

    /// A row decodes its values, and a `0xfb` is a NULL rather than a
    /// one-byte string.
    #[test]
    fn a_text_row_decodes_its_values_and_its_nulls() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&lenenc_int(2));
        packet.extend_from_slice(b"42");
        packet.push(0xfb);
        packet.extend_from_slice(&lenenc_int(5));
        packet.extend_from_slice(b"hello");
        let row = decode_row(&packet, 3).expect("decodes");
        assert_eq!(
            row,
            vec![Some(b"42".to_vec()), None, Some(b"hello".to_vec())]
        );
    }

    /// A row that ends early is an error naming the problem rather than a
    /// shorter row that a migration would then store.
    #[test]
    fn a_truncated_row_is_an_error() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&lenenc_int(9));
        packet.extend_from_slice(b"short");
        assert!(decode_row(&packet, 1).is_err());
    }

    /// An ERR packet names its code and its SQL state.
    #[test]
    fn an_error_packet_names_its_code_and_state() {
        let mut packet = vec![0xff];
        packet.extend_from_slice(&1045u16.to_le_bytes());
        packet.push(b'#');
        packet.extend_from_slice(b"28000");
        packet.extend_from_slice(b"Access denied for user 'nobody'@'localhost'");
        let error = decode_error(&packet);
        assert!(error.message().contains("1045"), "{}", error.message());
        assert!(error.message().contains("28000"), "{}", error.message());
    }

    /// A greeting decodes into its version, capabilities, challenge and plugin
    /// - and the challenge is the two halves joined **without** the trailing
    /// NUL, which is the byte that silently breaks the hash if it is kept.
    #[test]
    fn a_version_10_greeting_decodes() {
        let capabilities: u32 =
            capability::PROTOCOL_41 | capability::SECURE_CONNECTION | capability::PLUGIN_AUTH;
        let mut packet = vec![10u8];
        packet.extend_from_slice(b"8.4.0\0");
        packet.extend_from_slice(&7u32.to_le_bytes());
        packet.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        packet.push(0);
        packet.extend_from_slice(&((capabilities & 0xffff) as u16).to_le_bytes());
        packet.push(CHARSET_UTF8MB4);
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet.extend_from_slice(&((capabilities >> 16) as u16).to_le_bytes());
        packet.push(21);
        packet.extend_from_slice(&[0u8; 10]);
        packet.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 0]);
        packet.extend_from_slice(b"caching_sha2_password\0");

        let greeting = Greeting::decode(&packet).expect("decodes");
        assert_eq!(greeting.version, "8.4.0");
        assert_eq!(greeting.plugin, "caching_sha2_password");
        assert_eq!(greeting.scramble, (1..=20u8).collect::<Vec<u8>>());
    }

    /// A server that answers the connection with an error packet - "Host is
    /// not allowed to connect" - is reported as that error rather than as an
    /// unreadable greeting.
    #[test]
    fn an_error_where_the_greeting_belongs_is_reported_as_the_error() {
        let mut packet = vec![0xff];
        packet.extend_from_slice(&1130u16.to_le_bytes());
        packet.extend_from_slice(b"Host '10.0.0.1' is not allowed to connect");
        let error = Greeting::decode(&packet).expect_err("refuses");
        assert!(error.message().contains("1130"), "{}", error.message());
    }

    /// An identifier holding a backtick is doubled rather than escaped.
    #[test]
    fn a_backtick_in_an_identifier_is_doubled() {
        assert_eq!(back_quoted("we`ird"), "`we``ird`");
    }

    /// A blob's bytes are carried as they arrive: the text protocol does not
    /// encode them, so decoding them would corrupt them.
    #[test]
    fn a_blob_is_carried_byte_for_byte() {
        let bytes = vec![0x00, 0xff, 0x5c, 0x78];
        assert_eq!(
            carry(Some(&bytes), Kind::Blob),
            OwnedDatum::Blob(bytes.clone())
        );
        assert_eq!(carry(None, Kind::Blob), OwnedDatum::Null);
    }
}

/// The door the fuzz targets come in by.
///
/// **A named entry point rather than a public decoder.** The three functions
/// below read bytes a network peer controls, and they are private because
/// nothing outside this module has any business calling them. The fuzz crate
/// lives outside the workspace - `cargo-fuzz` needs a nightly toolchain, which
/// is why - so it cannot reach a private item, and widening the decoders
/// themselves would put three parsers in this crate's public API to serve a
/// test. These wrappers answer whether the decode succeeded and nothing else.
pub mod fuzzing {
    /// Reads an untrusted greeting packet.
    ///
    /// @param body - the packet's bytes
    pub fn greeting(body: &[u8]) -> bool {
        super::Greeting::decode(body).is_ok()
    }

    /// Reads an untrusted column definition packet.
    ///
    /// @param body - the packet's bytes
    pub fn column(body: &[u8]) -> bool {
        super::decode_column(body).is_ok()
    }

    /// Reads an untrusted row packet.
    ///
    /// @param body - the packet's bytes
    /// @param columns - how many columns the row description promised
    pub fn row(body: &[u8], columns: usize) -> bool {
        super::decode_row(body, columns).is_ok()
    }
}

#[cfg(test)]
mod fuzz_seeded {
    /// How many inputs the seeded sweep below reads.
    const CASES: usize = 20_000;

    /// Returns the next value of a deterministic generator.
    ///
    /// @param state - the generator's state, advanced in place
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// None of the three packet decoders panics on arbitrary bytes.
    ///
    /// **The stable-toolchain twin of `fuzz/fuzz_targets/mysql.rs`.** The fuzz
    /// crate needs nightly, so nothing in CI ran it for the length of this
    /// project; this runs the same decoders over a deterministic twenty
    /// thousand inputs in the ordinary suite, which is what makes a regression
    /// here fail a pull request rather than a scheduled job nobody reads.
    #[test]
    fn the_packet_decoders_never_panic_on_arbitrary_bytes() {
        let mut state = 0x1932_0001_u64;
        let mut accepted = 0usize;
        for case in 0..CASES {
            let length = (next(&mut state) % 96) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| next(&mut state) as u8).collect();
            accepted += usize::from(super::fuzzing::greeting(&bytes));
            accepted += usize::from(super::fuzzing::column(&bytes));
            accepted += usize::from(super::fuzzing::row(&bytes, case % 8));
        }
        // Not an assertion about how many are valid - it is that the sweep ran
        // and the decoders answered, rather than the loop being optimised into
        // nothing by a future edit that drops the return value.
        assert!(accepted <= CASES * 3);
    }
}
