//! The pragmas that set something: the cache, the log, the locks, the flags.
//!
//! Invariant: **a setting that cannot be honoured is refused rather than
//! accepted and ignored.** `journal_mode` and `locking_mode` answer with the
//! mode that is now in force, which is SQLite's own contract and is the only
//! way a caller can tell a change from a polite no.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_sql::declare::{argument_boolean, argument_integer, argument_text};
use inillucent_sql::directive::PragmaArgument;
use inillucent_tree::datum::OwnedDatum;
use inillucent_wal::Synchronous;

use crate::Outcome;

use super::*;

impl crate::ImportedDatabase {
    /// Reads or sets the buffer pool's budget, in SQLite's own units.
    ///
    /// SQLite states a negative `cache_size` in kibibytes and a positive one in
    /// pages; the pool is sized in frames of the file's page size, so the two
    /// are the same number said differently.
    ///
    /// **A larger cache than the pool was opened with grows the pool.**
    /// It used to clamp: the pool's frames were allocated at
    /// open, `set_budget` could only lower the ceiling inside them, and a
    /// caller asking for more read back what the engine had. The comment called
    /// that "the truth, not the request", and it was - but the request had no
    /// other way to be granted, and there is no flag on the shell or the
    /// command line either. So a two-gigabyte pool was reachable only by a
    /// program that linked the crate and called `Database::open_with`, and an
    /// attempt to reduce a crash just hit on a live database could not
    /// reproduce the pool size the crash happened under.
    ///
    /// Growing is cheap because a frame's buffer is allocated the first time
    /// that frame is claimed: what a large `cache_size` costs immediately is a
    /// latch, a pin counter and an empty vector per frame, and the pages
    /// themselves arrive as they are read. Nothing that is already resident
    /// moves. The pool never shrinks - a smaller cache lowers the budget, which
    /// is a ceiling on caching rather than a wall a statement runs into.
    pub(crate) fn pragma_cache_size(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let page_size = self.storage.page_size.max(1);
        let Some(argument) = argument else {
            let bytes = self.storage.frames.saturating_mul(page_size);
            return Ok(named_integer(
                "cache_size",
                self.pragmas
                    .cache_size
                    .get()
                    .unwrap_or(-((bytes / 1024) as i64)),
            ));
        };
        // SQLite's units: a negative number is kibibytes and a positive one is
        // pages, and it reads back what was written rather than what it derived
        // from it. So the sign is kept and the page count is worked out here.
        let asked = argument_integer(argument);
        let pages = if asked < 0 {
            (asked.saturating_neg().saturating_mul(1024) / page_size as i64).max(1)
        } else {
            asked.max(1)
        };
        let wanted = usize::try_from(pages).unwrap_or(usize::MAX);
        if wanted > self.storage.database.pool().frames() {
            self.storage.database.pool_mut().grow_frames(wanted)?;
            self.storage.frames = self.storage.database.pool().frames();
        }
        let pool = self.storage.database.pool();
        let held = pool.frames() as i64;
        pool.set_budget(pages.clamp(1, held) as usize);
        // Still the truth rather than the request, for the one case the grow
        // could not satisfy: an allocation that the platform refused reports
        // what the pool ended up with, in the units the caller used.
        self.pragmas.cache_size.set(Some(if pages > held {
            if asked < 0 {
                -(held.saturating_mul(page_size as i64) / 1024)
            } else {
                held
            }
        } else {
            asked
        }));
        Ok(Outcome::empty())
    }
    /// Reads or sets how much of a commit reaches the platter.
    pub(crate) fn pragma_synchronous(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "synchronous",
                match self.storage.wal.synchronous() {
                    Synchronous::Off => 0,
                    Synchronous::Normal => 1,
                    Synchronous::Full => 2,
                },
            ));
        };
        let text = argument_text(argument).trim().to_ascii_lowercase();
        let policy = match text.as_str() {
            "0" | "off" => Synchronous::Off,
            "1" | "normal" => Synchronous::Normal,
            "2" | "full" => Synchronous::Full,
            // SQLite's EXTRA syncs the directory as well as the file. There is
            // no directory entry to sync here, so it is FULL - and it is mapped
            // rather than refused, because refusing would break a caller that
            // asked for *more* durability than the engine can distinguish.
            "3" | "extra" => Synchronous::Full,
            other => return Err(refusal(format!("no such synchronous setting: {other}"))),
        };
        self.set_synchronous(policy);
        Ok(Outcome::empty())
    }
    /// Reads or sets how long a writer waits for the writer slot.
    ///
    /// **The one flag pragma whose column is not its own name.** SQLite calls
    /// this column `timeout`, not `busy_timeout` - checked against the pinned
    /// library's own `returnSingleInt(pParse, "timeout", ...)` call site - so a
    /// script that reads it by name gets what the reference gives it.
    pub(crate) fn pragma_busy_timeout(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        match argument {
            None => Ok(named_integer(
                "timeout",
                self.pragmas.busy_timeout_ms.get() as i64,
            )),
            Some(argument) => {
                self.pragmas
                    .busy_timeout_ms
                    .set(argument_integer(argument).max(0) as u64);
                Ok(named_integer(
                    "timeout",
                    self.pragmas.busy_timeout_ms.get() as i64,
                ))
            }
        }
    }
    /// Reads or sets a boolean flag the engine records and reports.
    pub(crate) fn pragma_flag(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(named_integer(
                "foreign_keys",
                i64::from(self.pragmas.foreign_keys.get()),
            )),
            Some(argument) => {
                let asked = argument_boolean(argument);
                // **The compiled statements go with it.** Whether keys are
                // enforced is decided by the binder, once, when a statement is
                // compiled - so a statement compiled while the setting was off
                // carries no key checks and would keep carrying none after the
                // pragma turned them on. Which is exactly the shape of bug this
                // pragma exists to avoid, since the symptom is a write that is
                // accepted rather than an error that is reported.
                if asked != self.pragmas.foreign_keys.get() {
                    self.forget_compiled_statements();
                }
                self.pragmas.foreign_keys.set(asked);
                Ok(Outcome::empty())
            }
        }
    }
    /// Reads or sets whether every immediate key check waits for the commit.
    ///
    /// It is a transaction's setting rather than a connection's - SQLite clears
    /// it at each commit or rollback - and it is read at bind time like
    /// `foreign_keys`, so the compiled statements go with it.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_defer(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(named_integer(
                "defer_foreign_keys",
                i64::from(self.pragmas.defer_foreign_keys.get()),
            )),
            Some(argument) => {
                let asked = argument_boolean(argument);
                if asked != self.pragmas.defer_foreign_keys.get() {
                    self.forget_compiled_statements();
                }
                self.pragmas.defer_foreign_keys.set(asked);
                Ok(Outcome::empty())
            }
        }
    }
    /// Answers a pragma the engine has exactly one setting for.
    ///
    /// Reading it returns that setting. Setting it to that setting is accepted
    /// and returns it. Setting it to anything else is **refused**, because the
    /// engine cannot do it and answering as though it had is the failure this
    /// distinction exists to prevent.
    ///
    /// @param argument - the value it was given, when it was given one
    /// @param name - the column SQLite reports this pragma's answer under
    /// @param fixed - the one setting
    pub(crate) fn pragma_fixed_word(
        &mut self,
        argument: Option<&PragmaArgument>,
        name: &str,
        fixed: &[u8],
    ) -> DbResult<Outcome> {
        if let Some(argument) = argument {
            let asked = argument_text(argument);
            if !asked.eq_ignore_ascii_case(&String::from_utf8_lossy(fixed)) {
                return Err(refusal(format!(
                    "this engine is {} only, and cannot be set to {asked}",
                    String::from_utf8_lossy(fixed)
                )));
            }
        }
        Ok(Outcome {
            rows: vec![vec![OwnedDatum::Text(fixed.to_vec())]],
            names: vec![name.into()],
            changes: Default::default(),
        })
    }
    /// Reads or writes the four bytes an application owns.
    ///
    /// **The single highest-value pragma on the list.** Every migration
    /// framework there is reads `user_version` to decide which migrations to
    /// run and writes it when one has run; an engine where it silently answers
    /// nothing cannot host one of them. It lives in the meta page, beside the
    /// catalog root, and reaches the file at the next checkpoint like every
    /// other meta field.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_user_version(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "user_version",
                i64::from(self.storage.database.user_version()),
            ));
        };
        let value = argument_integer(argument) as i32;
        self.storage.database.set_user_version(value);
        // **Checkpointed, because the meta page is not in the log.** Every
        // other write here is replayed from the WAL on the next open; a meta
        // field only reaches the file at a checkpoint, so one that was set and
        // not checkpointed would be silently forgotten - which is the exact
        // failure a migration framework cannot survive. SQLite pays a page
        // write and a journal for the same four bytes.
        self.checkpoint()?;
        Ok(Outcome::empty())
    }
    /// Reads or writes the four bytes that say what application owns the file.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_application_id(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "application_id",
                i64::from(self.storage.database.application_id()),
            ));
        };
        let value = argument_integer(argument) as i32;
        self.storage.database.set_application_id(value);
        self.checkpoint()?;
        Ok(Outcome::empty())
    }
    /// Reads or sets the ceiling on how large the file may grow.
    ///
    /// SQLite echoes the value back from a set, and reports its own maximum -
    /// `4294967294` - when nothing has been set. A ceiling below the pages the
    /// file already holds is not applied, which is SQLite's rule too: the
    /// answer is then the count rather than the request.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_max_page_count(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        if let Some(argument) = argument {
            let asked = argument_integer(argument);
            let held = self.storage.database.pool().page_count() as i64;
            self.pragmas
                .max_page_count
                .set(asked.max(held).min(DEFAULT_MAX_PAGE_COUNT));
        }
        Ok(named_integer(
            "max_page_count",
            self.pragmas.max_page_count.get(),
        ))
    }
    /// Sets whether `LIKE` compares ASCII letters exactly.
    ///
    /// **Write-only, which is what SQLite makes it**: `PRAGMA
    /// case_sensitive_like` with no argument returns no rows in either engine.
    /// The compiled statements go with it, for the reason `foreign_keys`
    /// documents - the setting is read when a `LIKE` is translated, so a
    /// statement compiled under the old one would keep the old behaviour, and
    /// the symptom would be a query quietly returning the wrong rows.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_case_sensitive_like(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(Outcome::empty());
        };
        let asked = argument_boolean(argument);
        if asked != self.pragmas.case_sensitive_like.get() {
            self.forget_compiled_statements();
        }
        self.pragmas.case_sensitive_like.set(asked);
        Ok(Outcome::empty())
    }
    /// Reads or sets how many rows `ANALYZE` may sample per index.
    ///
    /// See the dispatcher: this engine walks the whole table, which is more
    /// than any cap asks for.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_analysis_limit(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        if let Some(argument) = argument {
            self.pragmas
                .analysis_limit
                .set(argument_integer(argument).max(0));
        }
        Ok(named_integer(
            "analysis_limit",
            self.pragmas.analysis_limit.get(),
        ))
    }
    /// Reads or sets `locking_mode`.
    ///
    /// `normal` releases the file lock between transactions, so a second
    /// process may open the database; `exclusive` keeps it. Like `journal_mode`
    /// it answers with the mode that is in force rather than the one asked for,
    /// which is what SQLite does and what lets a script tell a refused switch
    /// from an honoured one.
    ///
    /// @param argument - the mode, when one was given
    pub(crate) fn pragma_locking_mode(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(word_row("locking_mode", self.locking_word()));
        };
        match argument_text(argument).trim().to_ascii_lowercase().as_str() {
            "normal" => self.set_locking_exclusive(false)?,
            "exclusive" => self.set_locking_exclusive(true)?,
            _ => {}
        }
        Ok(word_row("locking_mode", self.locking_word()))
    }
    /// Returns the word `locking_mode` reports.
    pub(crate) fn locking_word(&self) -> &'static str {
        if self.pragmas.locking_exclusive.get() {
            "exclusive"
        } else {
            "normal"
        }
    }
    /// Reads or sets `journal_mode`.
    ///
    /// **A real switch, not a reported one.** SQLite answers with the mode that
    /// is now in force, which is not always the one that was asked for - a
    /// request it cannot honour leaves the old mode and says so by returning
    /// it. That is why this returns a word rather than nothing, and why a
    /// refused switch is not an error.
    ///
    /// The word is what is *in force*, so a script that sets `DELETE` and reads
    /// back `wal` knows the switch did not happen.
    ///
    /// @param argument - the mode, when one was given
    pub(crate) fn pragma_journal_mode(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(word_row(
                "journal_mode",
                self.pragmas.journal_mode.get().word(),
            ));
        };
        let asked = argument_text(argument);
        if let Some(mode) = inillucent_pool::journal::JournalMode::named(asked.trim()) {
            // **Defensive mode refuses `OFF` and says so by not moving.** That
            // is SQLite's own rule and it is why the reference's shell - which
            // turns the flag on - answers `PRAGMA journal_mode = OFF` with the
            // mode that was already in force. `off` protects nothing, so a
            // connection that has asked to be protected from itself cannot have
            // it.
            if self.pragmas.defensive.get() && mode == inillucent_pool::journal::JournalMode::Off {
                return Ok(word_row(
                    "journal_mode",
                    self.pragmas.journal_mode.get().word(),
                ));
            }
            self.set_journal_mode(mode)?;
        }
        Ok(word_row(
            "journal_mode",
            self.pragmas.journal_mode.get().word(),
        ))
    }
    /// Reads or sets `auto_vacuum`.
    ///
    /// **The mode may only change while the database is empty**, which is
    /// SQLite own rule and not a limitation invented here: the mode decides
    /// whether the file carries the reverse-pointer map a vacuum walks, and a
    /// file already full of pages has no map to fill in retrospectively. A set
    /// on a database that already holds a table is accepted and *ignored*,
    /// exactly as the reference accepts and ignores it, and the read afterwards
    /// tells the truth about which mode is actually in force.
    ///
    /// @param argument - the mode, when one was given
    pub(crate) fn pragma_auto_vacuum(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "auto_vacuum",
                i64::from(self.pragmas.auto_vacuum.get()),
            ));
        };
        let asked = match argument_text(argument).trim().to_ascii_lowercase().as_str() {
            "0" | "none" => Some(0u8),
            "1" | "full" => Some(1),
            "2" | "incremental" => Some(2),
            _ => None,
        };
        // An unrecognised word is a no-op in SQLite rather than an error.
        if let Some(mode) = asked {
            if self
                .schema
                .tables
                .iter()
                .all(|table| table.folded.starts_with(b"sqlite_"))
            {
                self.pragmas.auto_vacuum.set(mode);
            }
        }
        Ok(Outcome::empty())
    }
    /// Runs `incremental_vacuum`, which moves free pages off the end of a file.
    ///
    /// It does nothing unless `auto_vacuum` is `incremental`, which is the
    /// reference behaviour and is why the whole thing is silent either way: the
    /// pragma returns no rows, and the only way to see what it did is
    /// `page_count`.
    ///
    /// @param argument - how many pages to move, or all of them
    pub(crate) fn pragma_incremental_vacuum(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        if self.pragmas.auto_vacuum.get() != 2 {
            return Ok(Outcome::empty());
        }
        let pages = argument.map(argument_integer).unwrap_or(i64::MAX).max(0);
        self.reclaim_free_pages(usize::try_from(pages).unwrap_or(usize::MAX))?;
        Ok(Outcome::empty())
    }
    /// Reads or sets `secure_delete`.
    ///
    /// @param argument - the setting, when one was given
    pub(crate) fn pragma_secure_delete(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "secure_delete",
                i64::from(self.pragmas.secure_delete.get()),
            ));
        };
        self.pragmas.secure_delete.set(
            match argument_text(argument).trim().to_ascii_lowercase().as_str() {
                "2" | "fast" => 2,
                _ => u8::from(argument_boolean(argument)),
            },
        );
        Ok(named_integer(
            "secure_delete",
            i64::from(self.pragmas.secure_delete.get()),
        ))
    }
    /// Reads or sets `ignore_check_constraints`.
    ///
    /// The compiled statements go with a change for the same reason
    /// `foreign_keys` throws them away: whether a `CHECK` is enforced is decided
    /// when a statement is bound, so a plan compiled under the old setting would
    /// keep the old behaviour - and the symptom would be a write that is
    /// accepted rather than an error that is reported.
    ///
    /// @param argument - the setting, when one was given
    pub(crate) fn pragma_ignore_check_constraints(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "ignore_check_constraints",
                i64::from(self.pragmas.ignore_check_constraints.get()),
            ));
        };
        let asked = argument_boolean(argument);
        if asked != self.pragmas.ignore_check_constraints.get() {
            self.forget_compiled_statements();
        }
        self.pragmas.ignore_check_constraints.set(asked);
        Ok(Outcome::empty())
    }
    /// Reads or sets `automatic_index`.
    ///
    /// @param argument - the setting, when one was given
    pub(crate) fn pragma_automatic_index(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "automatic_index",
                i64::from(self.pragmas.automatic_index.get()),
            ));
        };
        let asked = argument_boolean(argument);
        if asked != self.pragmas.automatic_index.get() {
            self.forget_compiled_statements();
        }
        self.pragmas.automatic_index.set(asked);
        // The planner reads the lever, not the field: a plan carries the levers
        // it was built under, so the two have to move together.
        self.pragmas.set_automatic_index(asked);
        Ok(Outcome::empty())
    }
    /// Reads or sets `writable_schema`, which this engine records and honours
    /// by having nothing for it to unlock.
    ///
    /// See the dispatcher for why. `PRAGMA writable_schema` with no argument
    /// reports what was set, which is what the reference does.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_writable_schema(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            // **Zero, whatever was set.** It is what the reference reads back
            // too - it clears the flag as soon as the schema is re-read - and
            // here it is the literal truth: there is nothing this engine's
            // catalog will let a statement write with the flag on that it
            // refuses with it off.
            return Ok(named_integer("writable_schema", 0));
        };
        self.pragmas.writable_schema.set(argument_boolean(argument));
        Ok(Outcome::empty())
    }
    /// Reads or sets whether this connection may write.
    ///
    /// **Honoured rather than remembered.** A caller sets `query_only` to make
    /// a mistake impossible, so a connection that recorded it and wrote anyway
    /// would be worse than one that refused the pragma outright.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_query_only(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "query_only",
                i64::from(self.pragmas.query_only.get()),
            ));
        };
        self.pragmas.query_only.set(argument_boolean(argument));
        Ok(Outcome::empty())
    }
    /// Reads or sets whether a trigger's own writes fire triggers.
    ///
    /// The compiled statements go with it, for the reason `foreign_keys`
    /// documents: whether a trigger's body may re-enter is decided when the
    /// body is compiled, so a statement compiled under the old setting would
    /// keep the old behaviour.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_recursive_triggers(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "recursive_triggers",
                i64::from(self.pragmas.recursive_triggers.get()),
            ));
        };
        let asked = argument_boolean(argument);
        if asked != self.pragmas.recursive_triggers.get() {
            self.forget_compiled_statements();
        }
        self.pragmas.recursive_triggers.set(asked);
        Ok(Outcome::empty())
    }
    /// Reads or sets where temporary tables live.
    ///
    /// This engine keeps them in memory, so `DEFAULT` and `MEMORY` are both
    /// what it already does and are accepted; `FILE` is the one value it cannot
    /// be, and is refused rather than accepted and ignored. SQLite reports the
    /// *setting* rather than the state, so a caller that wrote `MEMORY` reads
    /// `2` back and one that wrote nothing reads `0`.
    ///
    /// @param argument - the value it was given, when it was given one
    pub(crate) fn pragma_temp_store(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer("temp_store", self.pragmas.temp_store.get()));
        };
        let text = argument_text(argument).trim().to_ascii_lowercase();
        self.pragmas.temp_store.set(match text.as_str() {
            "0" | "default" => 0,
            "2" | "memory" => 2,
            other => {
                return Err(refusal(format!(
                    "temp_store {other} is not available here; temporary tables live in memory"
                )))
            }
        });
        Ok(Outcome::empty())
    }
}
