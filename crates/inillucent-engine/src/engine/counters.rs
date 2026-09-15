//! The counters a connection reports, and the seed the random built-ins advance.
//!
//! Invariant: **these are per database and not per statement.** A caller asks
//! the connection what the last statement did, having already dropped the
//! statement, which is where `sqlite3_changes` reads it from and why the numbers
//! live here.

impl crate::ImportedDatabase {
    /// Returns whether every statement is its own transaction.
    ///
    /// `false` between a `BEGIN` and its `COMMIT`, which is what
    /// `sqlite3_get_autocommit` answers and what the differential harness
    /// compares after every step.
    pub fn autocommit(&self) -> bool {
        self.writing.batch.get().is_none()
    }

    /// Returns the rowid the last `INSERT` assigned on this database.
    pub fn last_insert_rowid(&self) -> i64 {
        self.counters.last_rowid.get()
    }

    /// Returns how many rows every statement on **this connection** has
    /// changed.
    ///
    /// `changed_ever` is one counter shared by every connection this database
    /// has ever handed out, so the answer is the counter's value minus what it
    /// already read when `self.session_state.session` was opened - see
    /// `session_change_baseline`. `self.session_state.session` is always the caller's own:
    /// every entry point reaches this through `use_session`, which sets it
    /// first.
    pub fn total_changes(&self) -> i64 {
        self.counters.session_change_baseline.total_changes(
            self.session_state.session.get(),
            self.counters.changed_ever.get(),
        )
    }

    /// Returns how many rows the most recent write changed.
    ///
    /// What `sqlite3_changes` and the `changes()` scalar answer. The
    /// statement's own rows: a trigger body's go into `total_changes` and not
    /// into this, which is SQLite's rule.
    pub fn changes(&self) -> i64 {
        self.counters.last_changes.get()
    }

    /// Records what a write changed, on both counters.
    ///
    /// @param own - the rows the statement wrote itself
    /// @param all - the rows written under it, triggers included
    pub(crate) fn record_changes(&self, own: i64, all: i64) {
        self.counters.last_changes.set(own);
        self.counters
            .changed_ever
            .set(self.counters.changed_ever.get().saturating_add(all));
    }

    /// Records the rowid an `INSERT` assigned, when it assigned one.
    ///
    /// @param rowid - the key, or `None` when the statement wrote no row into a
    ///   table that has one
    pub(crate) fn remember_rowid(&self, rowid: Option<i64>) {
        if let Some(assigned) = rowid {
            self.counters.last_rowid.set(assigned);
        }
    }

    /// Returns what a statement's scalars should be told about the connection.
    ///
    /// `changes()`, `total_changes()` and `last_insert_rowid()` are constants
    /// for the length of one statement - SQLite updates them when a statement
    /// *finishes* - so they are read once here and compiled in, rather than
    /// asked per row.
    pub fn scalar_context(&self) -> inillucent_exec::scalar::Context {
        inillucent_exec::scalar::Context {
            changes: self.counters.last_changes.get(),
            total_changes: self.total_changes(),
            last_insert_rowid: self.counters.last_rowid.get(),
            seed: self.next_seed(),
            // So the `like(a, b)` function spelling follows the same pragma the
            // `LIKE` operator does.
            like_case_sensitive: self.pragmas.case_sensitive_like.get(),
        }
    }

    /// Returns a fresh seed for the random built-ins.
    ///
    /// **`random()` used to answer the same number for ever**, in
    /// every statement of every connection, because the new engine called the
    /// function library with a default context and the default seed is zero.
    /// One number is a legal answer to one call and a wrong answer to two, and
    /// an application seeding anything from it - a token, a sample, a shuffle -
    /// got a constant with no way to notice.
    ///
    /// The stream is `xoshiro256**`, started from the clock and the process id
    /// so two connections opened in the same millisecond do not share it, and
    /// advanced once per statement. Not cryptographic, which is also true of
    /// SQLite's `random()`.
    fn next_seed(&self) -> u64 {
        let mut rng = inillucent_base::rng::Rng::new(self.counters.seed.get());
        let next = rng.next_u64();
        self.counters.seed.set(next);
        next
    }
}
