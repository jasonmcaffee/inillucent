//! Copy a running server's tables into an inillucent database, verify them, and
//! publish by rename.
//!
//! The procedure is the one `inillucent-migrate`'s SQLite path already runs,
//! minus the steps a *server* makes meaningless. It holds the same invariants,
//! and they are the reason to read this file rather than write the loop by
//! hand:
//!
//! 1. **The source is never written.** The whole read happens inside one
//!    read-only, repeatable-read transaction. There is no flag that changes
//!    this and no cleanup step that deletes anything.
//! 2. **One snapshot.** Reading table B after table A committed produces a
//!    destination describing a database that never existed. The snapshot opens
//!    before the catalog is read, so even the schema is as of one instant.
//! 3. **Staged, never written where an application looks.** The build goes to
//!    `.<name>.staging` beside the destination, so the publish is a
//!    same-filesystem rename and therefore atomic.
//! 4. **Verified on a fresh open.** The staged file is checkpointed and closed
//!    before a single row is checked, so what is verified is what a new process
//!    sees rather than what the writing pool saw.
//! 5. **Counts *and* digests.** A migration that moves the right number of rows
//!    and the wrong bytes passes a count check on its own.
//! 6. **Nothing unverified is published**, and a failure leaves the staging
//!    file and the report - because the thing a person needs after a failed
//!    migration is the evidence.
//!
//! ## What the verification proves, and what it does not
//!
//! The two digests are computed by two different readers over two different
//! things: the source's rows come off a socket through this crate's wire
//! client, and the destination's come off PAX leaves through the engine's own
//! scan. A disagreement therefore means the copy is wrong, and agreement means
//! every value that was read reached the destination unchanged.
//!
//! It does **not** prove the wire decoding was right, because the copy and the
//! digest share that reader. That oracle is a third engine - `psql` and the
//! `mysql` client themselves - and it lives in the live-server acceptance tests
//! rather than in this function, because the tool must not require a PostgreSQL
//! installation in order to migrate away from one.
//!
//! Invariant: **the copy is proved to match before it is published, and the
//! publish is a rename.** A digest is taken of what the server sent and of what
//! was written, and they are compared; only then does the finished file take
//! the name. A reader therefore sees the old database or the new one and never
//! a half-copied one.

use std::fmt;
use std::path::{Path, PathBuf};

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::hash::Sha256;
use inillucent_base::DbResult;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

use crate::mysql::MysqlSource;
use crate::postgres::PostgresSource;
use crate::source::{quoted, RemoteSource, SourceTable};
use crate::url::{ConnectionUrl, Scheme, Transport};

/// How many rows one destination transaction holds by default.
pub const DEFAULT_BATCH: u64 = 10_000;

/// One thing that was checked, and what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// Whether it holds.
    pub passed: bool,
    /// What was found, whether it holds or not.
    pub detail: String,
}

impl Check {
    /// Returns a check that holds.
    ///
    /// @param name - what was checked
    /// @param detail - what was found
    pub fn passed(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            passed: true,
            detail: detail.into(),
        }
    }

    /// Returns a check that does not hold.
    ///
    /// @param name - what was checked
    /// @param detail - what was found
    pub fn failed(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            passed: false,
            detail: detail.into(),
        }
    }

    /// Returns the check as one line of a report.
    pub fn line(&self) -> String {
        format!(
            "{} {} {}",
            if self.passed { "pass" } else { "FAIL" },
            self.name,
            self.detail
        )
    }
}

/// An order-independent digest of a set of rows, folded one row at a time.
///
/// **Why a multiset hash rather than a sorted list of row digests.** Both sides
/// are read with no `ORDER BY` - the destination's `SELECT *` may be answered
/// from any structure that covers it - so the digest has to be independent of
/// the order rows arrive in. Sorting them means holding every row of the table,
/// which is the one thing this migration is built not to do. Summing each row's
/// SHA-256 modulo 2^256 is order-independent and costs 32 bytes for a table of
/// any size.
///
/// It is a *sum* rather than an exclusive-or on purpose: under XOR a pair of
/// identical rows cancels, so `{A, A, B, B}` and `{C, C, D, D}` would digest
/// alike, and duplicate rows are exactly what a re-run of a broken copy
/// produces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowDigest {
    /// How many rows have been folded in.
    pub rows: u64,
    /// The running sum of each row's digest, little-endian.
    sum: [u8; 32],
}

impl RowDigest {
    /// Returns an empty digest.
    pub fn new() -> RowDigest {
        RowDigest {
            rows: 0,
            sum: [0u8; 32],
        }
    }

    /// Folds one row in.
    ///
    /// @param row - the row's values
    pub fn add(&mut self, row: &[OwnedDatum]) {
        let digest = inillucent_base::hash::sha256(&encode_row(row));
        let mut carry = 0u16;
        for index in 0..32usize {
            let left = u16::from(*self.sum.get(index).unwrap_or(&0));
            let right = u16::from(*digest.get(index).unwrap_or(&0));
            let total = left.saturating_add(right).saturating_add(carry);
            if let Some(slot) = self.sum.get_mut(index) {
                *slot = (total & 0xff) as u8;
            }
            carry = total >> 8;
        }
        self.rows = self.rows.saturating_add(1);
    }

    /// Returns the digest as hexadecimal, with the row count folded in.
    ///
    /// The count is part of the value rather than reported beside it, so a
    /// table that lost a row of all-zero values still digests differently.
    pub fn hex(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(&self.rows.to_le_bytes());
        hash.update(&self.sum);
        hash.hex()
    }
}

impl fmt::Display for RowDigest {
    /// Writes the digest the way a report shows it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

/// Returns one row as the bytes it is digested by.
///
/// Every value carries its type tag and every variable-length value its length,
/// so an integer `1` and the text `"1"` do not digest alike and two different
/// row shapes cannot produce the same byte stream.
///
/// @param row - the row's values
pub fn encode_row(row: &[OwnedDatum]) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len().saturating_mul(12));
    out.extend_from_slice(&(row.len() as u64).to_le_bytes());
    for value in row {
        match value {
            OwnedDatum::Null => out.push(0),
            OwnedDatum::Int(number) => {
                out.push(1);
                out.extend_from_slice(&number.to_le_bytes());
            }
            OwnedDatum::Real(number) => {
                out.push(2);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
            OwnedDatum::Text(bytes) => {
                out.push(3);
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            OwnedDatum::Blob(bytes) => {
                out.push(4);
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

/// What one table cost and what it carried.
#[derive(Clone, Debug)]
pub struct TableReport {
    /// The table as the source names it.
    pub source: String,
    /// The table as the destination names it.
    pub target: String,
    /// How many rows were copied.
    pub rows: u64,
    /// The digest folded while copying.
    pub digest: String,
}

/// What one migration did.
#[derive(Clone, Debug)]
pub struct Report {
    /// The source, with its password redacted.
    pub source: String,
    /// What the server calls itself.
    pub server: String,
    /// Where the database was published, when it was.
    pub destination: PathBuf,
    /// The staging file, which is left behind on failure.
    pub staged: PathBuf,
    /// One entry per table carried.
    pub tables: Vec<TableReport>,
    /// Every check and what it found.
    pub checks: Vec<Check>,
    /// What exists in the source and did not come across.
    pub not_carried: Vec<(String, String)>,
    /// How the connection was made: `verified-tls` or `plaintext`.
    ///
    /// **Recorded so the question is answerable from the artifact.** "Did that
    /// migration send its rows over an encrypted connection" was previously
    /// answerable only by asking whoever ran it, and the report is the thing
    /// that outlives them.
    pub transport: String,
    /// What the peer's certificate proved, when the connection was encrypted.
    pub peer: Option<String>,
}

impl Report {
    /// Reports whether every check passed.
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|check| check.passed)
    }

    /// Returns the checks that failed.
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|check| !check.passed).collect()
    }

    /// Returns how many rows were carried altogether.
    pub fn rows(&self) -> u64 {
        self.tables
            .iter()
            .fold(0u64, |total, table| total.saturating_add(table.rows))
    }

    /// Renders the report a migration writes beside its destination.
    ///
    /// **It carries the redacted URL and never the password**, which is the
    /// same rule the connection URL's own `Display` holds - and this is the
    /// file most likely to be attached to a bug report.
    pub fn markdown(&self) -> String {
        let mut out = format!(
            "# Migration report

Source: `{}`
Server: {}
Destination: `{}`
Transport: {}
",
            self.source,
            self.server,
            self.destination.display(),
            self.transport
        );
        if let Some(peer) = &self.peer {
            out.push_str(&format!("Peer: {peer}\n"));
        }
        out.push('\n');
        out.push_str("## Tables\n\n| source | destination | rows | digest |\n|---|---|---|---|\n");
        for table in &self.tables {
            out.push_str(&format!(
                "| {} | {} | {} | `{}` |\n",
                table.source, table.target, table.rows, table.digest
            ));
        }
        out.push_str("\n## Checks\n\n");
        for check in &self.checks {
            out.push_str(&format!("- {}\n", check.line()));
        }
        if !self.not_carried.is_empty() {
            out.push_str(
                "\n## Not carried\n\nThese exist in the source and are not in the destination.\n\n",
            );
            for (kind, name) in &self.not_carried {
                out.push_str(&format!("- {kind} `{name}`\n"));
            }
        }
        out
    }
}

/// What a migration was asked to do.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Where to read from.
    pub url: ConnectionUrl,
    /// Where the finished database should end up.
    pub destination: PathBuf,
    /// How many rows one destination transaction holds.
    pub batch: u64,
    /// Whether to write the report beside the destination.
    pub write_report: bool,
    /// Whether the operator asked to permit an unencrypted connection.
    ///
    /// **Two things have to agree before a password crosses a network in the
    /// clear**: this, and `sslmode=disable` in the URL. Neither alone is
    /// enough. See `ConnectionUrl::transport`, which is where the pair is
    /// resolved and where a loopback address is exempted from both.
    pub insecure_plaintext: bool,
    /// What this migration may spend, when the caller sets a ceiling.
    ///
    /// **`None` means "whatever is already armed", which is the right default
    /// and not the same as unbounded (task-1932, H11).** A migration started
    /// from `inillucent migrate` runs inside the budget `command::run` armed,
    /// so the deadline an MCP server set applies to it without anybody passing
    /// it here. This field is for the other direction: a caller with no budget
    /// of its own - the standalone `inillucent-migrate` binary, a test - that
    /// wants one for this migration alone.
    pub limits: Option<inillucent_base::budget::Limits>,
}

impl Plan {
    /// Returns a plan for one URL and one destination.
    ///
    /// @param url - the source
    /// @param destination - where the database should end up
    pub fn new(url: ConnectionUrl, destination: impl AsRef<Path>) -> Plan {
        Plan {
            url,
            destination: destination.as_ref().to_path_buf(),
            batch: DEFAULT_BATCH,
            write_report: true,
            insecure_plaintext: false,
            limits: None,
        }
    }

    /// Returns how this plan's connection will be made, or why it is refused.
    ///
    /// Asked before anything is dialled, so a refusal costs no socket and no
    /// staging file.
    pub fn transport(&self) -> DbResult<Transport> {
        self.url.transport(self.insecure_plaintext)
    }
}

/// Connects to the server a URL names, over verified TLS.
///
/// @param url - which server, and as who
pub fn connect(url: &ConnectionUrl) -> DbResult<Box<dyn RemoteSource>> {
    connect_over(url, Transport::VerifiedTls)
}

/// Connects on a transport the caller's policy has already decided.
///
/// @param url - which server, and as who
/// @param transport - what `ConnectionUrl::transport` decided
pub fn connect_over(url: &ConnectionUrl, transport: Transport) -> DbResult<Box<dyn RemoteSource>> {
    match url.scheme {
        Scheme::Postgres => Ok(Box::new(PostgresSource::connect_over(url, transport)?)),
        Scheme::Mysql => Ok(Box::new(MysqlSource::connect_over(url, transport)?)),
    }
}

/// Migrates a running server into a new database, verified, and publishes it.
///
/// Returns the report whether or not it passed: a migration that verified badly
/// is not an error to propagate, it is a result to read. What it never does is
/// publish one.
///
/// @param plan - what to migrate and where to put it
pub fn migrate(plan: &Plan) -> DbResult<Report> {
    // The transport is decided before the socket, so a refusal costs nothing
    // and - more to the point - so that a plan that would have gone in the
    // clear never reaches the dialling code at all.
    let transport = plan.transport()?;
    let mut source = connect_over(&plan.url, transport)?;
    let peer = source.peer();
    let outcome = run(plan, source.as_mut()).map(|mut report| {
        report.transport = transport.name().to_string();
        report.peer = peer;
        report
    });
    source.finish();
    outcome
}

/// Migrates from an already-connected source, which is the seam the tests drive.
///
/// @param plan - what to migrate and where to put it
/// @param source - a connected server
pub fn run(plan: &Plan, source: &mut dyn RemoteSource) -> DbResult<Report> {
    // A ceiling the caller asked for, armed for the length of this migration
    // and dropped afterwards, so an ambient budget is restored rather than
    // replaced. With no ceiling the caller's own budget stays in force.
    let _armed = plan.limits.clone().map(|limits| {
        inillucent_base::budget::arm(
            limits,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    });
    if plan.destination.exists() {
        return Err(misuse(format!(
            "{} already exists; a migration publishes by renaming and never overwrites",
            plan.destination.display()
        )));
    }
    let staged = staging_path(&plan.destination);
    if staged.exists() {
        return Err(misuse(format!(
            "{} is left over from an earlier migration. A remote migration is one pass - a server \
             changes underneath a resumed one - so move or remove that file and run this again.",
            staged.display()
        )));
    }

    let tables = source.describe()?;
    let not_carried = source.not_carried().unwrap_or_default();
    let server = source.server();

    let mut reports: Vec<TableReport> = Vec::new();
    let mut checks: Vec<Check> = Vec::new();

    // The build. A failure anywhere in here leaves the staging file where it
    // fell and never touches the destination.
    {
        let database = Database::open(&staged).map_err(|error| {
            corrupt(format!(
                "the staging database {} could not be opened: {}",
                staged.display(),
                error.message()
            ))
        })?;
        let connection = database.session();

        for table in &tables {
            connection
                .execute_batch(&table.create_sql())
                .map_err(|error| {
                    corrupt(format!(
                        "{} could not be created: {}",
                        table.target,
                        error.detail().unwrap_or_else(|| error.message())
                    ))
                })?;
        }

        for table in &tables {
            let digest = copy_table(&connection, source, table, plan.batch)?;
            reports.push(TableReport {
                source: table.qualified(),
                target: table.target.clone(),
                rows: digest.rows,
                digest: digest.hex(),
            });
        }

        // Checkpointed and closed before a single row is verified: the fold of
        // the log into the file is what makes the next open read a finished
        // database rather than replay its way to one.
        database.checkpoint().map_err(|error| {
            corrupt(format!(
                "{} could not be checkpointed: {}",
                staged.display(),
                error.message()
            ))
        })?;
    }

    // **Opened from the file**, in a pool that has never seen the copy.
    let opened = Database::open(&staged).map_err(|error| {
        corrupt(format!(
            "{} was built but could not be reopened: {}",
            staged.display(),
            error.message()
        ))
    })?;
    match opened.check() {
        Ok(()) => checks.push(Check::passed(
            "structure.integrity",
            "every tree walks in key order on a fresh open",
        )),
        Err(error) => checks.push(Check::failed(
            "structure.integrity",
            format!("a tree is not intact: {}", error.message()),
        )),
    }
    let connection = opened.session();
    for (table, report) in tables.iter().zip(reports.iter()) {
        // The count, asked of the **source** as a count rather than as a scan,
        // so the check is against a different query path on the server than the
        // one that produced the rows.
        match source.count(table) {
            Ok(counted) if counted == report.rows => checks.push(Check::passed(
                &format!("source.count.{}", table.target),
                format!("{counted} rows, counted separately from the scan"),
            )),
            Ok(counted) => checks.push(Check::failed(
                &format!("source.count.{}", table.target),
                format!(
                    "the source counts {counted} rows and the scan produced {}",
                    report.rows
                ),
            )),
            Err(error) => checks.push(Check::failed(
                &format!("source.count.{}", table.target),
                format!("the source could not be counted: {}", error.message()),
            )),
        }

        let read = digest_table(&connection, &table.target);
        match read {
            Ok(destination) => {
                if destination.rows == report.rows {
                    checks.push(Check::passed(
                        &format!("count.{}", table.target),
                        format!("{} rows", destination.rows),
                    ));
                } else {
                    checks.push(Check::failed(
                        &format!("count.{}", table.target),
                        format!(
                            "{} rows migrated, {} read from the source",
                            destination.rows, report.rows
                        ),
                    ));
                }
                let digest = destination.hex();
                if digest == report.digest {
                    checks.push(Check::passed(&format!("digest.{}", table.target), digest));
                } else {
                    checks.push(Check::failed(
                        &format!("digest.{}", table.target),
                        format!("{digest} migrated, {} read from the source", report.digest),
                    ));
                }
            }
            Err(error) => checks.push(Check::failed(
                &format!("table.{}", table.target),
                format!(
                    "could not be read from the migrated database: {}",
                    error.detail().unwrap_or_else(|| error.message())
                ),
            )),
        }
    }
    if tables.is_empty() {
        checks.push(Check::passed("tables", "the source holds none"));
    }
    // Ends the borrow of the staged database so it can be closed and renamed.
    let _ = connection;
    drop(opened);

    let mut report = Report {
        source: plan.url.to_string(),
        server,
        destination: plan.destination.clone(),
        staged: staged.clone(),
        tables: reports,
        checks,
        not_carried,
        // Filled in by `migrate`, which is the only caller that knows how the
        // connection was made. `run` takes an already-connected source, so a
        // transport it invented here would be a claim rather than a record.
        transport: String::new(),
        peer: None,
    };

    if report.passed() {
        publish(&staged, &plan.destination)?;
    }
    if plan.write_report {
        let path = with_suffix(&plan.destination, ".migration-report.md");
        std::fs::write(&path, report.markdown()).map_err(|error| {
            corrupt(format!("{} could not be written: {error}", path.display()))
        })?;
    }
    if !report.passed() {
        report.destination = PathBuf::new();
    }
    Ok(report)
}

/// How many rows one page of the verifying read holds.
///
/// **The verify used to read the whole table.** `connection.query("SELECT * FROM
/// t")` and this engine's statements materialise, so checking `chunk_embedding`
/// - 601,862 rows of 9,513-byte text - held every one of them at once: peak
/// resident 4.7 GB, scaling with the largest table rather than with anything the
/// operator chose. It completed on the machine it was measured on. On a smaller
/// one it would have failed nine minutes into a migration that had already done
/// all of its work, and the staged file would have been thrown away for a
/// shortage of memory rather than a difference in the data.
///
/// Two thousand rows is a page of about 19 MB at that row width and about 2 MB
/// at an ordinary one, and it is a page rather than a row because a statement
/// per row would pay a prepare, a descent and a plan lookup for each.
const VERIFY_PAGE_ROWS: usize = 2_000;

/// Folds every row of a destination table into a digest, one page at a time.
///
/// **Paged by rowid, which every destination table has.** `SourceTable::create_sql`
/// writes the primary key as a table constraint and never `WITHOUT ROWID`, so
/// `rowid` is present, unique and ordered on all of them - and paging by it
/// costs one descent per page rather than the rescan an `OFFSET` would.
///
/// The rowid is selected so the page can be walked and is **not** digested: the
/// source digest was folded over the source's own columns, so the destination's
/// have to be the same list. That is what `row.get(1..)` is for.
///
/// The digest does not depend on the order rows arrive in, which is what makes a
/// paged read produce the same value as a single one.
///
/// @param connection - the reopened staging database
/// @param table - the destination table's name
fn digest_table(
    connection: &inillucent_engine::connect::Connection<'_>,
    table: &str,
) -> DbResult<RowDigest> {
    let sql = format!(
        "SELECT rowid, * FROM {} WHERE rowid > ?1 ORDER BY rowid LIMIT {VERIFY_PAGE_ROWS}",
        quoted(table)
    );
    let mut statement = connection.prepare(&sql)?;
    let mut digest = RowDigest::new();
    let mut after = i64::MIN;
    loop {
        statement.reset();
        statement.bind_integer(1, after)?;
        let mut seen = 0usize;
        while statement.step()? {
            let row = statement.row();
            let Some(OwnedDatum::Int(rowid)) = row.first() else {
                return Err(corrupt(format!(
                    "{table} answered a row with no rowid, so the verify cannot page it"
                )));
            };
            after = *rowid;
            digest.add(row.get(1..).unwrap_or(&[]));
            seen = seen.saturating_add(1);
        }
        if seen < VERIFY_PAGE_ROWS {
            return Ok(digest);
        }
    }
}

/// Copies one table, folding each row into a digest as it is bound.
///
/// The source is read **once**: the same pass that binds a row into the
/// destination's prepared `INSERT` also folds it into the digest that the
/// destination is later checked against. Reading a live server twice would take
/// twice as long and, without the snapshot, would not even be the same table.
///
/// @param connection - the staging database
/// @param source - the connected server
/// @param table - which table
/// @param batch - how many rows one transaction holds
fn copy_table(
    connection: &inillucent_engine::connect::Connection<'_>,
    source: &mut dyn RemoteSource,
    table: &SourceTable,
    batch: u64,
) -> DbResult<RowDigest> {
    let mut statement = connection.prepare(&table.insert_sql()).map_err(|error| {
        corrupt(format!(
            "the insert for {} could not be prepared: {}",
            table.target,
            error.detail().unwrap_or_else(|| error.message())
        ))
    })?;
    let mut digest = RowDigest::new();
    let mut in_batch = 0u64;
    let batch = batch.max(1);
    connection.execute_batch("BEGIN")?;
    let copied = source.scan(table, &mut |row| {
        statement.clear_bindings();
        for (at, value) in row.iter().enumerate() {
            statement.bind((at as u32).saturating_add(1), value.clone())?;
        }
        statement.step()?;
        statement.reset();
        digest.add(row);
        in_batch = in_batch.saturating_add(1);
        if in_batch >= batch {
            connection.execute_batch("COMMIT")?;
            // **The request's budget is read once per batch, and it is the only
            // thing that can stop this (task-1932, H11).** A migration runs
            // outside the executor, so none of the engine's own checks are
            // reached: the sixty second deadline an MCP server arms never
            // fired, a cancellation had nothing to land on, and a copy of a
            // large table held the server for hours with the client unable to
            // do anything about it. Per batch rather than per row because the
            // commit beside it is already the expensive thing on this path, and
            // a batch is the unit the caller chose.
            inillucent_base::budget::check()?;
            connection.execute_batch("BEGIN")?;
            in_batch = 0;
        }
        Ok(())
    });
    match copied {
        Ok(_) => {
            connection.execute_batch("COMMIT")?;
            Ok(digest)
        }
        Err(error) => {
            // The rows already committed stay in the staging file, which is
            // never published and never cleaned up - it is the evidence.
            let _ = connection.execute_batch("COMMIT");
            Err(corrupt(format!(
                "{} could not be copied: {}",
                table.qualified(),
                error.detail().unwrap_or_else(|| error.message())
            )))
        }
    }
}

/// Moves a verified staging file into place, refusing to overwrite anything.
///
/// The log segments move first. A `.rdb` is a file plus the segments named
/// after it; renaming only the first would publish a database whose log was
/// still called by the staging name. The build is checkpointed before it is
/// verified, so in practice every segment here is empty - but "in practice
/// empty" is not a thing to publish on.
///
/// @param staged - the verified staging file
/// @param destination - where it belongs
fn publish(staged: &Path, destination: &Path) -> DbResult<()> {
    if destination.exists() {
        return Err(misuse(format!(
            "{} appeared while the migration was running; nothing was published",
            destination.display()
        )));
    }
    for segment in log_segments(staged) {
        let Some(name) = segment
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&file_name(staged)) else {
            continue;
        };
        let moved = destination.with_file_name(format!("{}{suffix}", file_name(destination)));
        std::fs::rename(&segment, &moved).map_err(|error| {
            corrupt(format!(
                "{} could not be published to {}: {error}",
                segment.display(),
                moved.display()
            ))
        })?;
    }
    std::fs::rename(staged, destination).map_err(|error| {
        corrupt(format!(
            "{} was verified but could not be published to {}: {error}",
            staged.display(),
            destination.display()
        ))
    })?;
    // The directory entry itself has to reach the disk, or a power loss can
    // leave a rename that only happened in a cache.
    if let Some(parent) = destination.parent() {
        if let Ok(handle) = std::fs::File::open(parent) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}

/// Returns a database file's own name, which its log segments are prefixed by.
///
/// @param database - the database file
fn file_name(database: &Path) -> String {
    database
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Returns every log segment sitting beside one database file.
///
/// @param database - the database file
fn log_segments(database: &Path) -> Vec<PathBuf> {
    let Some(directory) = database.parent() else {
        return Vec::new();
    };
    let prefix = format!("{}-wal.", file_name(database));
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(prefix.as_str())
        })
        .map(|entry| entry.path())
        .collect();
    out.sort();
    out
}

/// Returns the staging path a destination is built at.
///
/// Beside the destination rather than in a temporary directory, so the rename
/// that publishes it stays within one file system and is therefore atomic. A
/// rename across devices is a copy, and a copy is a window in which the
/// destination exists and is not yet whole.
///
/// @param destination - where the migration will publish
pub fn staging_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "migrated".to_string());
    let directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    directory.join(format!(".{name}.staging"))
}

/// Returns a path with a suffix appended to its file name.
///
/// @param path - the base path
/// @param suffix - what to append to its name
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = file_name(path);
    if name.is_empty() {
        name = "database".to_string();
    }
    name.push_str(suffix);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a row of one integer.
    fn row(value: i64) -> Vec<OwnedDatum> {
        vec![OwnedDatum::Int(value)]
    }

    /// The digest does not depend on the order rows arrive in, which is the
    /// property that lets the destination be read with an unordered `SELECT *`.
    #[test]
    fn the_digest_is_independent_of_row_order() {
        let mut forwards = RowDigest::new();
        let mut backwards = RowDigest::new();
        for value in 1..=50i64 {
            forwards.add(&row(value));
        }
        for value in (1..=50i64).rev() {
            backwards.add(&row(value));
        }
        assert_eq!(forwards.hex(), backwards.hex());
        assert_eq!(forwards.rows, 50);
    }

    /// **Duplicate rows do not cancel.** This is the case an exclusive-or would
    /// get wrong, and duplicated rows are exactly what a re-run of a broken
    /// copy produces.
    #[test]
    fn duplicate_rows_do_not_cancel() {
        let mut pairs = RowDigest::new();
        for value in [1i64, 1, 2, 2] {
            pairs.add(&row(value));
        }
        let mut others = RowDigest::new();
        for value in [3i64, 3, 4, 4] {
            others.add(&row(value));
        }
        assert_ne!(pairs.hex(), others.hex());
    }

    /// A row that lost a value, or whose value changed type, digests
    /// differently - so a migration that dropped a column cannot pass.
    #[test]
    fn a_type_change_or_a_missing_value_changes_the_digest() {
        let mut integer = RowDigest::new();
        integer.add(&[OwnedDatum::Int(1)]);
        let mut text = RowDigest::new();
        text.add(&[OwnedDatum::Text(b"1".to_vec())]);
        assert_ne!(integer.hex(), text.hex());

        let mut wide = RowDigest::new();
        wide.add(&[OwnedDatum::Int(1), OwnedDatum::Null]);
        assert_ne!(integer.hex(), wide.hex());
    }

    /// The row count is part of the digest, so a table that lost a row of
    /// zeroes still fails - a sum alone would not have moved.
    #[test]
    fn the_row_count_is_part_of_the_digest() {
        let mut one = RowDigest::new();
        one.add(&[OwnedDatum::Int(0)]);
        let mut two = RowDigest::new();
        two.add(&[OwnedDatum::Int(0)]);
        two.add(&[OwnedDatum::Int(0)]);
        assert_ne!(one.hex(), two.hex());
    }

    /// The staging file is beside the destination and is never the destination.
    #[test]
    fn the_staging_name_is_beside_the_destination_and_hidden() {
        let staged = staging_path(Path::new("/var/db/corpus.rdb"));
        assert_eq!(staged.parent(), Path::new("/var/db/corpus.rdb").parent());
        assert!(staged
            .file_name()
            .map(|name| name.to_string_lossy().starts_with('.'))
            .unwrap_or(false));
        assert_ne!(staged, PathBuf::from("/var/db/corpus.rdb"));
    }

    /// A report with no checks has not passed - the same rule the other
    /// migration path holds, so an empty run cannot read as a success.
    #[test]
    fn a_report_with_no_checks_has_not_passed() {
        let report = Report {
            source: "postgres://u:***@h:5432/d".to_string(),
            server: "PostgreSQL 17.2".to_string(),
            destination: PathBuf::new(),
            staged: PathBuf::new(),
            tables: Vec::new(),
            checks: Vec::new(),
            not_carried: Vec::new(),
            transport: "verified-tls".to_string(),
            peer: None,
        };
        assert!(!report.passed());
    }

    /// **The written report carries no password.** It is the file most likely
    /// to be attached to a bug report.
    #[test]
    fn the_report_never_carries_the_password() {
        let url = ConnectionUrl::parse("postgres://user:hunter2@db:5432/corpus").expect("parses");
        let report = Report {
            source: url.to_string(),
            server: "PostgreSQL 17.2".to_string(),
            destination: PathBuf::from("out.rdb"),
            staged: PathBuf::from(".out.rdb.staging"),
            tables: vec![TableReport {
                source: "public.note".to_string(),
                target: "note".to_string(),
                rows: 3,
                digest: "abc".to_string(),
            }],
            checks: vec![Check::passed("count.note", "3 rows")],
            not_carried: vec![("view".to_string(), "public.recent".to_string())],
            transport: "verified-tls".to_string(),
            peer: Some("a certificate trusted by this machine, for db".to_string()),
        };
        let markdown = report.markdown();
        assert!(!markdown.contains("hunter2"), "{markdown}");
        // **How the connection was made is in the artifact**, so "were those
        // rows encrypted in transit" is answerable without asking whoever ran
        // it. The report is the thing that outlives them.
        assert!(markdown.contains("Transport: verified-tls"), "{markdown}");
        assert!(markdown.contains("public.note"));
        assert!(markdown.contains("pass count.note"));
        assert!(markdown.contains("public.recent"));
    }
    /// A paged verifying read digests exactly what one whole-table read does.
    ///
    /// **What this is a regression test for.** The verify used to
    /// be `connection.query("SELECT * FROM t")`, and this engine's statements
    /// materialise - so checking `chunk_embedding`, 601,862 rows of 9,513-byte
    /// text, held 4.7 GB resident at once. The read is now paged by rowid, and
    /// the property that makes that legitimate is the one
    /// `the_digest_is_independent_of_row_order` pins.
    ///
    /// The table holds more than two full pages and its rowids have gaps, so
    /// the paging is exercised rather than only its first page, and a pager that
    /// assumed contiguous rowids or counted rows instead of following the key
    /// would stop early and be caught here.
    #[test]
    fn a_paged_read_digests_the_same_value_as_a_whole_one() {
        let database = inillucent_engine::connect::Database::open(":memory:")
            .expect("an in-memory database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER, body TEXT)")
            .expect("the schema is created");
        let rows = VERIFY_PAGE_ROWS.saturating_mul(3).saturating_add(37);
        connection.execute_batch("BEGIN").expect("begin");
        let mut insert = connection
            .prepare("INSERT INTO t (id, body) VALUES (?1, ?2)")
            .expect("the insert prepares");
        for nth in 0..rows {
            insert.reset();
            insert.bind_integer(1, nth as i64).expect("bind");
            insert
                .bind_text(2, &format!("row {nth} of {rows}"))
                .expect("bind");
            while insert.step().expect("step") {}
        }
        drop(insert);
        connection.execute_batch("COMMIT").expect("commit");
        // Gaps in the rowids, so a pager that assumed they were contiguous
        // would read the wrong rows.
        connection
            .execute_batch("DELETE FROM t WHERE id % 7 = 0")
            .expect("some rows are removed");

        let whole = connection
            .query("SELECT * FROM t")
            .expect("the whole table reads");
        let mut expected = RowDigest::new();
        for row in &whole {
            expected.add(row);
        }
        assert!(
            expected.rows > VERIFY_PAGE_ROWS as u64 * 2,
            "the fixture is only {} rows, which does not exercise paging",
            expected.rows
        );
        let paged = digest_table(&connection, "t").expect("the paged read runs");
        assert_eq!(paged.rows, expected.rows, "every row was read exactly once");
        assert_eq!(
            paged.hex(),
            expected.hex(),
            "the paged read digests what the whole read digests"
        );
    }
}
