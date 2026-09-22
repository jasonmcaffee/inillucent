//! The pragmas that check something, and the checkpoint.
//!
//! Invariant: **a check reads and never repairs.** `integrity_check` is a
//! question about the file, and one that quietly fixed what it found would
//! answer `ok` about a file it had just changed.

use inillucent_base::{DbError, DbResult, PrimaryCode};
use inillucent_sql::declare::argument_text;
use inillucent_sql::directive::PragmaArgument;
use inillucent_tree::datum::OwnedDatum;

use crate::engine::integrity::CheckDepth;
use crate::Outcome;

impl crate::ImportedDatabase {
    /// Reports every row whose foreign key has no parent.
    ///
    /// **It is a query, not a scan written by hand**, so it uses the planner
    /// and the indexes an ordinary query would: a check over a million-row
    /// child with an index on its key is a lookup per row rather than a second
    /// scan. `inillucent-sql`'s `violation_query` builds it, which is the same
    /// text a deferred constraint is tested with at commit - so the pragma and
    /// the commit cannot disagree about what a violation is.
    ///
    /// @param argument - one table to check, or none for every table
    pub(crate) fn pragma_foreign_key_check(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let only = argument.map(|argument| argument_text(argument).to_ascii_lowercase());
        let mut rows = Vec::new();
        for query in self.schema.violation_queries(only.as_deref())? {
            for row in self.query_internally(&query.sql)? {
                rows.push(vec![
                    OwnedDatum::Text(query.child.clone()),
                    row.first().cloned().unwrap_or(OwnedDatum::Null),
                    OwnedDatum::Text(query.parent.clone()),
                    OwnedDatum::Int(i64::from(query.key)),
                ]);
            }
        }
        Ok(Outcome {
            rows,
            names: std::rc::Rc::new(vec![
                "table".into(),
                "rowid".into(),
                "parent".into(),
                "fkid".into(),
            ]),
            changes: Default::default(),
        })
    }
    /// Runs the integrity checker over every tree.
    ///
    /// `ok` when they all hold, and the first failure otherwise, which is the
    /// shape SQLite's answer has.
    ///
    /// **Two names, and now two amounts of reading.** They used to be the same
    /// pass, because this engine had no cheaper variant to offer. `quick_check`
    /// reads every tree's own shape and accounts for every page of the file;
    /// `integrity_check` does that and then reads each index against the table
    /// it is on.
    ///
    /// **The line is drawn at the index pass because that is the expensive
    /// one**, which was measured rather than assumed: the index pass walks each
    /// index and its table and merges them, where the page walk reads a tree's
    /// interior pages and its leaves and takes an out-of-line value's pages
    /// from the reference in the leaf it is already holding. Over a table of
    /// sixty out-of-line values the whole page walk cost 4 page fetches on top
    /// of 133.
    ///
    /// The pinned SQLite 3.53.4 draws it in the same place: its `quick_check`
    /// omits index content against table content, `UNIQUE`, `CHECK` and
    /// `NOT NULL`, and still accounts for every page of the file
    /// (`sqlite3BtreeIntegrityCheck`, which both pragmas reach).
    ///
    /// @param column - which of the two names asked for this, and so which one
    ///   the answer is reported under
    /// @param depth - how much of the file that name reads
    pub(crate) fn pragma_integrity_check(
        &mut self,
        column: &str,
        depth: CheckDepth,
    ) -> DbResult<Outcome> {
        let answer = match self.check_trees_to(depth) {
            Ok(()) => b"ok".to_vec(),
            Err(error) => error
                .detail()
                .unwrap_or(error.message())
                .as_bytes()
                .to_vec(),
        };
        Ok(Outcome {
            rows: vec![vec![OwnedDatum::Text(answer)]],
            names: std::rc::Rc::new(vec![column.into()]),
            changes: Default::default(),
        })
    }
    /// Checkpoints the log into the data file.
    ///
    /// SQLite answers three integers: whether it was blocked, how many frames
    /// the log held, and how many of them were moved. The engine's checkpoint is
    /// not blockable from here - there is one writer - so the first is always
    /// zero, and the second and third are always equal because a checkpoint here
    /// always moves everything.
    ///
    /// The number reported is **pages the checkpoint wrote**, counted off the
    /// pool rather than off the log. SQLite's log holds one frame per dirty page
    /// and this one holds a record per change, so a record count would be a
    /// bigger number meaning something else; the pages written is the same
    /// physical quantity SQLite's frame count is.
    pub(crate) fn pragma_wal_checkpoint(&mut self) -> DbResult<Outcome> {
        // **Refused, not answered `1 | -1 | -1`, once the open transaction has
        // written anything.** The pinned reference checkpoints fine after a
        // bare `BEGIN` - no write lock is held yet - and answers `database
        // table is locked` (`SQLITE_LOCKED`) the moment a statement has
        // written, because this connection is itself the lock a checkpoint
        // needs. A `busy` row here would let a script read it, believe
        // nothing happened, and `COMMIT` over a checkpoint that in fact never
        // ran; recording the checkpoint's start no earlier than the open
        // transaction's own first record - which is what letting it proceed
        // would require - is exactly the no-steal argument `holds_uncommitted`
        // makes, so this is refused rather than made honest.
        if self.writing.batch().is_some() && self.writing.touched() != 0 {
            return Err(DbError::primary(PrimaryCode::Locked)
                .with_detail("cannot checkpoint: a transaction has written and not committed"));
        }
        // **Minus one twice when there is no log to check point.** SQLite
        // answers `0|-1|-1` under a rollback journal because the two counts are
        // "frames in the log" and "frames moved", and a database with no
        // write-ahead log has neither - which is a different statement from
        // "no frames moved". A caller polling the second column to decide
        // whether a checkpoint is due needs to be able to tell those apart.
        if self.pragmas.journal_mode() != inillucent_pool::journal::JournalMode::Wal {
            self.checkpoint()?;
            return Ok(Outcome {
                rows: vec![vec![
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(-1),
                    OwnedDatum::Int(-1),
                ]],
                names: std::rc::Rc::new(vec!["busy".into(), "log".into(), "checkpointed".into()]),
                changes: Default::default(),
            });
        }
        let before = self.storage.database.pool().stats().writes;
        self.checkpoint()?;
        let moved = self
            .storage
            .database
            .pool()
            .stats()
            .writes
            .saturating_sub(before) as i64;
        Ok(Outcome {
            rows: vec![vec![
                OwnedDatum::Int(0),
                OwnedDatum::Int(moved),
                OwnedDatum::Int(moved),
            ]],
            names: std::rc::Rc::new(vec!["busy".into(), "log".into(), "checkpointed".into()]),
            changes: Default::default(),
        })
    }
}
