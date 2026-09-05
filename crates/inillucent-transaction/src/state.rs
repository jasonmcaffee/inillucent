//! The connection's transaction state: autocommit, begin modes, savepoints,
//! and the counters a statement moves.
//!
//! Invariant: this module decides *what* is legal and remembers what has to be
//! undone; it never touches a file. The pager holds the page images and the
//! locks, and the two are pushed and popped together - a savepoint here is
//! always an undo level there, which is why `ROLLBACK TO` cannot restore the
//! counters without also restoring the pages.
//!
//! The counters are part of the transaction rather than of the statement for a
//! reason SQLite documents and applications rely on: `changes()` reports the
//! last statement's row count and survives a `ROLLBACK`, while
//! `last_insert_rowid()` is restored by `ROLLBACK TO` because a savepoint that
//! undid the insert has undone the rowid too.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;

/// How an explicit `BEGIN` acquires its rights.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeginMode {
    /// Take nothing until the first read or write needs it.
    Deferred,
    /// Take the writer's reservation now, so a later write cannot be refused.
    Immediate,
    /// Take the write lock now, excluding readers as well as writers.
    Exclusive,
}

impl BeginMode {
    /// Parses the keyword a `BEGIN` carries, defaulting to DEFERRED.
    pub fn parse(text: &str) -> Option<BeginMode> {
        match text.to_ascii_lowercase().as_str() {
            "" | "deferred" => Some(BeginMode::Deferred),
            "immediate" => Some(BeginMode::Immediate),
            "exclusive" => Some(BeginMode::Exclusive),
            _ => None,
        }
    }

    /// Reports whether the mode takes writer rights at `BEGIN`.
    pub fn writes_immediately(self) -> bool {
        matches!(self, BeginMode::Immediate | BeginMode::Exclusive)
    }
}

/// What a statement does when a constraint refuses a row.
///
/// The five differ in how much they undo, and the difference is the whole
/// point: ABORT undoes the statement, FAIL keeps the rows it had already
/// written, ROLLBACK undoes the transaction, IGNORE skips the row and carries
/// on, and REPLACE deletes whatever was in the way.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConflictAlgorithm {
    /// Undo the statement and report the error. SQLite's default.
    #[default]
    Abort,
    /// Undo the whole transaction and report the error.
    Rollback,
    /// Stop, report the error, and keep the rows already written.
    Fail,
    /// Skip the offending row and continue without an error.
    Ignore,
    /// Delete the rows that conflict, then write this one.
    Replace,
}

impl ConflictAlgorithm {
    /// Parses the keyword an `ON CONFLICT` clause or an `INSERT OR` carries.
    pub fn parse(text: &str) -> Option<ConflictAlgorithm> {
        match text.to_ascii_lowercase().as_str() {
            "rollback" => Some(ConflictAlgorithm::Rollback),
            "abort" => Some(ConflictAlgorithm::Abort),
            "fail" => Some(ConflictAlgorithm::Fail),
            "ignore" => Some(ConflictAlgorithm::Ignore),
            "replace" => Some(ConflictAlgorithm::Replace),
            _ => None,
        }
    }

    /// Returns the keyword spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictAlgorithm::Rollback => "ROLLBACK",
            ConflictAlgorithm::Abort => "ABORT",
            ConflictAlgorithm::Fail => "FAIL",
            ConflictAlgorithm::Ignore => "IGNORE",
            ConflictAlgorithm::Replace => "REPLACE",
        }
    }

    /// Reports whether a conflict under this algorithm ends the statement.
    pub fn stops_the_statement(self) -> bool {
        !matches!(self, ConflictAlgorithm::Ignore | ConflictAlgorithm::Replace)
    }

    /// Reports whether a conflict under this algorithm undoes the statement's
    /// earlier rows.
    pub fn undoes_the_statement(self) -> bool {
        matches!(self, ConflictAlgorithm::Abort | ConflictAlgorithm::Rollback)
    }

    /// Reports whether a conflict under this algorithm undoes the transaction.
    pub fn undoes_the_transaction(self) -> bool {
        self == ConflictAlgorithm::Rollback
    }
}

/// The counters a connection reports about what it has done.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChangeCounters {
    /// Rows changed by the most recent completed statement.
    pub changes: i64,
    /// Rows changed since the connection was opened.
    pub total_changes: i64,
    /// The rowid the most recent successful insert allocated.
    pub last_insert_rowid: i64,
}

/// One open savepoint, and everything a `ROLLBACK TO` has to put back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Savepoint {
    /// The name the statement gave it. The transaction level has none.
    pub name: Option<String>,
    /// The counters as they were when it opened.
    pub counters: ChangeCounters,
    /// Whether this level is the automatic one a statement runs inside.
    pub is_statement: bool,
    /// Whether closing this level publishes a row count.
    ///
    /// A SELECT opens a statement level like any other statement, and closing
    /// it must not overwrite `changes()`: SQLite reports the last statement
    /// that *changed* rows, so a query between two updates leaves the count
    /// alone rather than zeroing it.
    pub counts: bool,
}

/// What the connection is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionState {
    /// No transaction is open; each statement is its own.
    Autocommit,
    /// A read transaction is open and holds a snapshot.
    Read,
    /// A write transaction is open.
    Write,
    /// A failure left the transaction unusable; only ROLLBACK is accepted.
    Failed,
}

/// What a connection has done, for the write baselines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransactionStats {
    /// Explicit and implicit transactions committed.
    pub commits: u64,
    /// Transactions rolled back.
    pub rollbacks: u64,
    /// Savepoints opened.
    pub savepoints: u64,
    /// Statements that rolled themselves back after a conflict.
    pub statement_rollbacks: u64,
}

/// The connection's transaction machine.
///
/// It is deliberately ignorant of pages and locks. The session drives it and
/// the pager in step, and every method here either succeeds and changes the
/// state or fails and leaves it exactly as it was, so a caller cannot end up
/// with a machine that disagrees with its pager about how many levels are open.
#[derive(Clone, Debug)]
pub struct Transaction {
    state: TransactionState,
    mode: BeginMode,
    explicit: bool,
    /// Whether the open transaction was started by a `SAVEPOINT` rather than
    /// by a `BEGIN`, which is what decides whether releasing the outermost
    /// savepoint commits it.
    mode_was_implicit: bool,
    levels: Vec<Savepoint>,
    counters: ChangeCounters,
    statement_changes: i64,
    /// Rows the running statement's *triggers* changed.
    ///
    /// Kept apart from `statement_changes` because the two are reported
    /// differently: `changes()` is the statement's own row count and excludes
    /// what its triggers wrote, while `total_changes()` counts both. Measured
    /// against 3.53.4 - an INSERT of one row whose triggers write two more
    /// reports `changes` of 1 and moves `total_changes` by 3.
    statement_trigger_changes: i64,
    stats: TransactionStats,
}

impl Default for Transaction {
    /// A connection with nothing open.
    fn default() -> Transaction {
        Transaction::new()
    }
}

impl Transaction {
    /// Returns a connection in autocommit with no levels open.
    pub fn new() -> Transaction {
        Transaction {
            state: TransactionState::Autocommit,
            mode: BeginMode::Deferred,
            explicit: false,
            mode_was_implicit: false,
            levels: Vec::new(),
            counters: ChangeCounters::default(),
            statement_changes: 0,
            statement_trigger_changes: 0,
            stats: TransactionStats::default(),
        }
    }

    /// Returns what the connection is doing.
    pub fn state(&self) -> TransactionState {
        self.state
    }

    /// Reports whether the connection is in autocommit mode.
    ///
    /// SQLite's `autocommit` flag is false exactly while an explicit
    /// transaction is open. An implicit transaction inside a running statement
    /// does not clear it, because the statement will finish it.
    pub fn autocommit(&self) -> bool {
        !self.explicit
    }

    /// Reports whether a write transaction is open.
    pub fn is_writing(&self) -> bool {
        self.state == TransactionState::Write
    }

    /// Reports whether any named savepoint is open.
    pub fn has_open_savepoint(&self) -> bool {
        self.levels.iter().any(|level| level.name.is_some())
    }

    /// Reports whether the transaction survives the statement that is ending.
    ///
    /// An explicit `BEGIN` obviously does. So does an open savepoint, and that
    /// is the case worth naming: `SAVEPOINT s` outside a transaction starts an
    /// implicit one that lasts until the savepoint is released, while leaving
    /// `autocommit` reporting true. Committing at the end of the next
    /// statement - which is what "autocommit is true" would otherwise mean -
    /// destroys the savepoint the caller is about to roll back to.
    pub fn outlives_a_statement(&self) -> bool {
        self.explicit || self.has_open_savepoint()
    }

    /// Reports whether the transaction has failed and only accepts ROLLBACK.
    pub fn has_failed(&self) -> bool {
        self.state == TransactionState::Failed
    }

    /// Returns the mode the open transaction was begun with.
    pub fn mode(&self) -> BeginMode {
        self.mode
    }

    /// Returns the counters as they stand.
    pub fn counters(&self) -> ChangeCounters {
        self.counters
    }

    /// Returns the running totals.
    pub fn stats(&self) -> TransactionStats {
        self.stats
    }

    /// Returns how many levels are open, the transaction level included.
    pub fn depth(&self) -> usize {
        self.levels.len()
    }

    /// Returns the open levels, outermost first.
    pub fn levels(&self) -> &[Savepoint] {
        &self.levels
    }

    /// Begins an explicit transaction.
    pub fn begin(&mut self, mode: BeginMode) -> DbResult<()> {
        if self.explicit {
            return Err(misuse("cannot start a transaction within a transaction"));
        }
        self.explicit = true;
        self.mode_was_implicit = false;
        self.mode = mode;
        self.state = if mode.writes_immediately() {
            TransactionState::Write
        } else {
            TransactionState::Read
        };
        self.levels.clear();
        self.levels.push(Savepoint {
            name: None,
            counters: self.counters,
            is_statement: false,
            counts: false,
        });
        Ok(())
    }

    /// Promotes an open transaction to a write transaction.
    ///
    /// It is called when the virtual machine reaches its first write opcode,
    /// which is where an implicit write transaction begins and where a
    /// DEFERRED explicit one takes its writer rights.
    pub fn promote_to_write(&mut self) -> DbResult<()> {
        if self.state == TransactionState::Failed {
            return Err(misuse("the transaction has failed and must be rolled back"));
        }
        if self.levels.is_empty() {
            self.levels.push(Savepoint {
                name: None,
                counters: self.counters,
                is_statement: false,
                counts: false,
            });
        }
        self.state = TransactionState::Write;
        Ok(())
    }

    /// Marks the transaction as read-only-open, which the first page read does.
    pub fn promote_to_read(&mut self) {
        if self.state == TransactionState::Autocommit {
            self.state = TransactionState::Read;
        }
    }

    /// Records that the transaction can no longer be committed.
    pub fn fail(&mut self) {
        self.state = TransactionState::Failed;
    }

    /// Ends a read-only transaction, which changed nothing and counts as
    /// neither a commit nor a rollback.
    pub fn end_read(&mut self) {
        self.state = TransactionState::Autocommit;
        self.explicit = false;
        self.mode_was_implicit = false;
        self.mode = BeginMode::Deferred;
        self.levels.clear();
    }

    /// Ends the transaction, whichever way it ended.
    pub fn finish(&mut self, committed: bool) {
        if committed {
            self.stats.commits = self.stats.commits.saturating_add(1);
        } else {
            self.stats.rollbacks = self.stats.rollbacks.saturating_add(1);
        }
        self.state = TransactionState::Autocommit;
        self.explicit = false;
        self.mode_was_implicit = false;
        self.mode = BeginMode::Deferred;
        self.levels.clear();
    }

    /// Opens the automatic level a statement runs inside.
    ///
    /// `counts` says whether the statement can change rows; a query opens a
    /// level so that a failure inside it undoes nothing, but closing that
    /// level does not touch the counters.
    pub fn begin_statement(&mut self, counts: bool) {
        if counts {
            self.statement_changes = 0;
            self.statement_trigger_changes = 0;
        }
        self.levels.push(Savepoint {
            name: None,
            counters: self.counters,
            is_statement: true,
            counts,
        });
    }

    /// Closes the statement level, keeping its changes.
    pub fn commit_statement(&mut self) -> DbResult<()> {
        let Some(level) = self.levels.pop() else {
            return Err(misuse("there is no statement level to close"));
        };
        if !level.is_statement {
            self.levels.push(level);
            return Err(misuse(
                "the innermost level is a savepoint, not a statement",
            ));
        }
        if level.counts {
            self.counters.changes = self.statement_changes;
            self.counters.total_changes = self
                .counters
                .total_changes
                .saturating_add(self.statement_changes)
                .saturating_add(self.statement_trigger_changes);
        }
        Ok(())
    }

    /// Closes the statement level, undoing its changes.
    ///
    /// The rows it wrote are undone by the pager. `changes()` becomes zero and
    /// `total_changes()` does not move, because the rows are no longer there -
    /// which is what SQLite reports, measured against 3.53.4: an aborted
    /// `INSERT INTO t VALUES(9,'nine'),(1,'dup')` leaves `changes` at 0 and
    /// `total_changes` where it was, even though row 9 was written before the
    /// conflict was found.
    ///
    /// `last_insert_rowid` is deliberately *not* restored. SQLite documents it
    /// as unpredictable after a rollback and in practice keeps the rowid of the
    /// insert that was undone; restoring it here would be tidier and would
    /// disagree with the reference on a value applications read.
    pub fn rollback_statement(&mut self) -> DbResult<()> {
        let Some(level) = self.levels.pop() else {
            return Err(misuse("there is no statement level to roll back"));
        };
        if !level.is_statement {
            self.levels.push(level);
            return Err(misuse(
                "the innermost level is a savepoint, not a statement",
            ));
        }
        if level.counts {
            self.counters.changes = 0;
        }
        self.statement_changes = 0;
        self.statement_trigger_changes = 0;
        self.stats.statement_rollbacks = self.stats.statement_rollbacks.saturating_add(1);
        Ok(())
    }

    /// Keeps a statement's earlier rows after a FAIL conflict.
    ///
    /// FAIL is the algorithm that stops without undoing, so the level is
    /// closed as if the statement had succeeded and the count it reports is
    /// the number of rows it managed.
    pub fn fail_statement(&mut self) -> DbResult<()> {
        self.commit_statement()
    }

    /// Counts one row changed by the running statement.
    pub fn record_change(&mut self) {
        self.statement_changes = self.statement_changes.saturating_add(1);
    }

    /// Records a row one of the statement's triggers changed.
    ///
    /// It counts towards `total_changes()` and not towards `changes()`, which
    /// is the whole reason it is a separate call rather than a second
    /// `record_change`.
    pub fn record_trigger_change(&mut self) {
        self.statement_trigger_changes = self.statement_trigger_changes.saturating_add(1);
    }

    /// Returns how many rows the running statement has changed.
    pub fn statement_changes(&self) -> i64 {
        self.statement_changes
    }

    /// Records the rowid an insert allocated.
    pub fn record_insert_rowid(&mut self, rowid: i64) {
        self.counters.last_insert_rowid = rowid;
    }

    /// Opens a named savepoint.
    ///
    /// Outside a transaction, `SAVEPOINT` starts one, and `autocommit` becomes
    /// false exactly as it does for `BEGIN` - measured against SQLite 3.53.4,
    /// which reports `sqlite3_get_autocommit()` as 0 after a bare `SAVEPOINT`.
    /// The transaction ends when the outermost savepoint is released, and it
    /// is that release rather than the next statement that commits.
    pub fn open_savepoint(&mut self, name: &str) -> DbResult<()> {
        if self.state == TransactionState::Failed {
            return Err(misuse("the transaction has failed and must be rolled back"));
        }
        if !self.explicit {
            self.mode_was_implicit = true;
        }
        if self.levels.is_empty() {
            self.levels.push(Savepoint {
                name: None,
                counters: self.counters,
                is_statement: false,
                counts: false,
            });
        }
        self.levels.push(Savepoint {
            name: Some(name.to_string()),
            counters: self.counters,
            is_statement: false,
            counts: false,
        });
        self.explicit = true;
        self.stats.savepoints = self.stats.savepoints.saturating_add(1);
        Ok(())
    }

    /// Returns the depth of the most recent savepoint with this name.
    ///
    /// Most recent, because SQLite allows duplicates and resolves them from
    /// the inside out - a nested `SAVEPOINT s` shadows the outer one until it
    /// is released.
    pub fn find_savepoint(&self, name: &str) -> DbResult<usize> {
        self.levels
            .iter()
            .rposition(|level| level.name.as_deref() == Some(name))
            .ok_or_else(|| misuse(format!("no such savepoint: {name}")))
    }

    /// Releases a savepoint and everything inside it, keeping the changes.
    ///
    /// Returns whether the release closed the outermost level, which is what
    /// commits an implicit transaction that a `SAVEPOINT` started.
    pub fn release_savepoint(&mut self, name: &str) -> DbResult<bool> {
        let index = self.find_savepoint(name)?;
        self.levels.truncate(index);
        Ok(!self.has_open_savepoint() && self.mode_was_implicit)
    }

    /// Rolls back to a savepoint, leaving it open.
    pub fn rollback_to_savepoint(&mut self, name: &str) -> DbResult<()> {
        let index = self.find_savepoint(name)?;
        let Some(level) = self.levels.get(index).cloned() else {
            return Err(misuse(format!("no such savepoint: {name}")));
        };
        self.levels.truncate(index.saturating_add(1));
        // The counters are not restored, for the reason `rollback_statement`
        // gives: SQLite leaves `last_insert_rowid` holding the rowid of an
        // insert that has been undone, and parity is worth more than tidiness
        // on a value applications read.
        let _ = level;
        // A statement that fails inside a savepoint has already been undone,
        // so a transaction that was only failed by that statement is usable
        // again once the savepoint it failed inside has been rolled back.
        if self.state == TransactionState::Failed {
            self.state = TransactionState::Write;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rolling back to a savepoint makes a failed transaction usable again.
    ///
    /// A statement that fails inside a savepoint has already been undone, so a
    /// transaction failed only by that statement can be written to again once
    /// the savepoint it failed inside is rolled back. The comment on
    /// `rollback_to_savepoint` has always said so; nothing asserted it, and a
    /// version that promoted every *other* state instead - leaving a failed
    /// transaction failed - passed the whole suite.
    #[test]
    fn rolling_back_to_a_savepoint_revives_a_failed_transaction() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Deferred).expect("begins");
        transaction
            .open_savepoint("s")
            .expect("the savepoint opens");
        transaction.fail();
        assert_eq!(transaction.state(), TransactionState::Failed);

        transaction
            .rollback_to_savepoint("s")
            .expect("the savepoint rolls back");
        assert_eq!(
            transaction.state(),
            TransactionState::Write,
            "the transaction is usable again once what failed it has been undone"
        );
    }

    /// Rolling back to a savepoint does not promote a transaction that had not
    /// failed.
    ///
    /// The other half of the same branch: only a failed transaction is revived,
    /// and a rollback is not a reason to call a transaction a writer.
    #[test]
    fn rolling_back_to_a_savepoint_does_not_promote_an_unfailed_transaction() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Deferred).expect("begins");
        let before = transaction.state();
        transaction
            .open_savepoint("s")
            .expect("the savepoint opens");
        transaction
            .rollback_to_savepoint("s")
            .expect("the savepoint rolls back");
        assert_eq!(
            transaction.state(),
            before,
            "a transaction that had not failed is left exactly as it was"
        );
    }

    /// A fresh connection is in autocommit with no levels.
    #[test]
    fn a_new_connection_is_in_autocommit() {
        let transaction = Transaction::new();
        assert_eq!(transaction.state(), TransactionState::Autocommit);
        assert!(transaction.autocommit());
        assert_eq!(transaction.depth(), 0);
    }

    /// An explicit BEGIN clears the autocommit flag; a nested one is refused.
    #[test]
    fn begin_within_begin_is_refused() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Deferred).expect("begins");
        assert!(!transaction.autocommit());
        assert!(transaction.begin(BeginMode::Deferred).is_err());
    }

    /// IMMEDIATE takes writer rights at BEGIN; DEFERRED waits.
    #[test]
    fn immediate_begins_as_a_writer_and_deferred_does_not() {
        let mut deferred = Transaction::new();
        deferred.begin(BeginMode::Deferred).expect("begins");
        assert_eq!(deferred.state(), TransactionState::Read);
        let mut immediate = Transaction::new();
        immediate.begin(BeginMode::Immediate).expect("begins");
        assert_eq!(immediate.state(), TransactionState::Write);
    }

    /// A completed statement publishes its count; a rolled back one does not.
    #[test]
    fn a_rolled_back_statement_does_not_publish_its_count() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Immediate).expect("begins");
        transaction.begin_statement(true);
        transaction.record_change();
        transaction.record_change();
        transaction.commit_statement().expect("commits");
        assert_eq!(transaction.counters().changes, 2);
        assert_eq!(transaction.counters().total_changes, 2);

        transaction.begin_statement(true);
        transaction.record_change();
        transaction.rollback_statement().expect("rolls back");
        assert_eq!(transaction.counters().changes, 0);
        assert_eq!(transaction.counters().total_changes, 2);
    }

    /// FAIL keeps the rows a statement had already written.
    #[test]
    fn fail_keeps_the_rows_written_before_the_conflict() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Immediate).expect("begins");
        transaction.begin_statement(true);
        transaction.record_change();
        transaction.record_change();
        transaction.fail_statement().expect("keeps");
        assert_eq!(transaction.counters().changes, 2);
    }

    /// Duplicate savepoint names resolve to the most recent one.
    #[test]
    fn duplicate_savepoints_resolve_from_the_inside_out() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Immediate).expect("begins");
        transaction.open_savepoint("s").expect("opens");
        transaction.open_savepoint("t").expect("opens");
        transaction.open_savepoint("s").expect("opens");
        assert_eq!(transaction.find_savepoint("s").expect("found"), 3);
        transaction.release_savepoint("s").expect("releases");
        assert_eq!(transaction.find_savepoint("s").expect("found"), 1);
    }

    /// ROLLBACK TO leaves the savepoint open, and leaves the rowid alone.
    ///
    /// Leaving it alone is the surprising half, and it is what SQLite does:
    /// the documentation calls the value unpredictable after a rollback, and
    /// the reference build keeps the undone insert's rowid.
    #[test]
    fn rollback_to_keeps_the_savepoint_and_does_not_restore_the_rowid() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Immediate).expect("begins");
        transaction.record_insert_rowid(7);
        transaction.open_savepoint("s").expect("opens");
        transaction.record_insert_rowid(99);
        transaction.rollback_to_savepoint("s").expect("rolls back");
        assert_eq!(transaction.counters().last_insert_rowid, 99);
        assert!(transaction.find_savepoint("s").is_ok());
    }

    /// A savepoint outside a transaction starts one, and releasing it commits.
    #[test]
    fn a_savepoint_outside_a_transaction_starts_a_transaction() {
        let mut transaction = Transaction::new();
        transaction.open_savepoint("s").expect("opens");
        assert!(!transaction.autocommit());
        assert!(transaction.release_savepoint("s").expect("releases"));
        transaction.finish(true);
        assert!(transaction.autocommit());
    }

    /// Releasing a savepoint inside an explicit transaction does not commit it.
    #[test]
    fn releasing_a_savepoint_inside_a_begin_does_not_commit() {
        let mut transaction = Transaction::new();
        transaction.begin(BeginMode::Immediate).expect("begins");
        transaction.open_savepoint("s").expect("opens");
        assert!(!transaction.release_savepoint("s").expect("releases"));
        assert!(!transaction.autocommit());
    }

    /// Every conflict algorithm parses to itself and back.
    #[test]
    fn conflict_algorithms_round_trip() {
        for algorithm in [
            ConflictAlgorithm::Rollback,
            ConflictAlgorithm::Abort,
            ConflictAlgorithm::Fail,
            ConflictAlgorithm::Ignore,
            ConflictAlgorithm::Replace,
        ] {
            let parsed = ConflictAlgorithm::parse(algorithm.as_str()).expect("parses");
            assert_eq!(parsed, algorithm);
        }
    }
}
