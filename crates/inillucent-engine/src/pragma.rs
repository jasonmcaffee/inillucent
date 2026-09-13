//! The `PRAGMA` set, re-profiled for the new engine.
//!
//! Invariant: **a pragma either means something here or answers the way SQLite
//! answers one it does not recognise, and which of the two it is is written
//! down.** SQLite's own convention for an unknown pragma is a no-op that
//! returns no rows and no error - `PRAGMA nonesuch;` is silent - and the
//! statement after it runs. Anything that returned an error would break a
//! caller for whom the pragma was a hint, and anything that *silently* accepted
//! a setting it does not honour would be worse: `PRAGMA journal_mode = DELETE`
//! answering `delete` on an engine that only has a write-ahead log is a lie a
//! caller would act on.
//!
//! So the set is in two parts, and the manifest's `pragma.*` rows name them:
//!
//! - **Honoured.** `cache_size`, `synchronous`, `busy_timeout`, `foreign_keys`,
//!   `journal_mode`, `locking_mode`, `auto_vacuum`, `secure_delete`,
//!   `integrity_check`, `quick_check`, `wal_checkpoint`, `table_info` and the
//!   rest of the schema and pager reporters. These do what they say.
//! - **Reported.** A value this engine has exactly one of: `encoding` is
//!   `UTF-8` and takes nothing else. Setting one of these to what it already is
//!   succeeds and setting it to anything else refuses.
//!
//! There used to be a third part, **Silent**, and there is not one any more -
//! see the section below. `journal_mode` and `locking_mode` used to be in the
//! second part and are both real switches now, once the rollback journal and
//! the file-locking protocol were built; `PRAGMA journal_mode`
//! reports `delete` by default because the reference does and because the gate
//! says it costs nothing.
//!
//! The distinction that matters is between *reported* and *refused*. A pragma
//! that exists and was given a value the engine cannot honour is **refused**,
//! because accepting it would be answering a question wrongly.
//!
//! ## Nothing SQLite lists is silent any more
//!
//! **Silence was the wrong third option.** Of the 67 pragmas SQLite's own
//! `pragma_list` names, 21 answered here and *38 were accepted and answered
//! nothing at all* - no value and no error, which a caller cannot tell from an
//! empty result. A pragma that returns nothing is indistinguishable from one
//! that returned no rows, so an application had no way to find out that its
//! `PRAGMA user_version` had gone nowhere.
//!
//! So the rule is now: **a name on SQLite's list either answers or refuses.**
//! [`SQLITE_PRAGMAS`] is that list, and anything on it this engine has no
//! answer for is refused by name. A pragma on *nobody's* list -
//! `PRAGMA nonesuch` - is still silent, because that is what SQLite does with
//! one and the parity is the point.
//!
//! The dispositions, then, are three:
//!
//! - **Honoured** - it does what it says. `user_version`, `application_id` and
//!   `schema_version` live in the meta page; `max_page_count`, `query_only`
//!   and `recursive_triggers` are read where they are acted on; `cache_size`
//!   caps how many pages the pool holds.
//! - **Reported** - a value this engine has exactly one of. Readable, and a
//!   write that would change it is refused: `encoding` is `UTF-8`. Setting one
//!   of these to what it already is succeeds; setting it to something else
//!   refuses.
//! - **Refused** - the subject does not exist here, and the refusal names it.
//!   `docs/feature-comparison.md` measures what is left in this column, which is
//!   now nothing on SQLite's own list.

use inillucent_base::error::refusal;
use inillucent_base::{DbError, DbResult, PrimaryCode};
use inillucent_sql::declare::{argument_boolean, argument_integer, argument_text};
use inillucent_sql::directive::PragmaArgument;
use inillucent_tree::datum::OwnedDatum;
use inillucent_wal::Synchronous;

use super::{ImportedDatabase, Outcome};

impl ImportedDatabase {
    /// Runs one `PRAGMA`.
    ///
    /// @param name - the pragma's folded name
    /// @param argument - the value it was given, when it was given one
    pub(super) fn pragma(
        &mut self,
        name: &[u8],
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        // **The read-only pragmas first, and through the same function the
        // table-valued form uses.** `PRAGMA table_info(t)` and `SELECT * FROM
        // pragma_table_info('t')` are the same question; if they were two
        // implementations they would eventually be two answers, and the one
        // nobody tested would be the wrong one.
        if let Some(outcome) = self.pragma_rows(name, argument)? {
            return Ok(outcome);
        }
        match name {
            // `default_cache_size` is the deprecated spelling of the same
            // setting, and SQLite still answers it.
            b"cache_size" | b"default_cache_size" => self.pragma_cache_size(argument),
            b"synchronous" => self.pragma_synchronous(argument),
            b"busy_timeout" => self.pragma_busy_timeout(argument),
            b"foreign_keys" => self.pragma_flag(argument),
            b"defer_foreign_keys" => self.pragma_defer(argument),
            b"foreign_key_check" => self.pragma_foreign_key_check(argument),
            b"journal_mode" => self.pragma_journal_mode(argument),
            b"encoding" => self.pragma_fixed_word(argument, "encoding", b"UTF-8"),
            b"locking_mode" => self.pragma_locking_mode(argument),
            b"integrity_check" => self.pragma_integrity_check("integrity_check"),
            b"quick_check" => self.pragma_integrity_check("quick_check"),
            b"wal_checkpoint" => self.pragma_wal_checkpoint(),
            b"page_size" => Ok(named_integer("page_size", self.page_size as i64)),
            b"page_count" => Ok(named_integer(
                "page_count",
                self.database.pool().page_count() as i64,
            )),
            b"freelist_count" => Ok(named_integer(
                "freelist_count",
                self.database.free_pages() as i64,
            )),
            b"user_version" => self.pragma_user_version(argument),
            b"application_id" => self.pragma_application_id(argument),
            b"schema_version" => Ok(named_integer(
                "schema_version",
                i64::from(self.database.schema_cookie()),
            )),
            // **A connection-visible counter, not a file one.** SQLite's
            // `data_version` changes when *another* connection has committed;
            // one connection watching its own writes always sees the same
            // number, and this engine holds the file exclusively, so that
            // number is 1 and stays 1. It is reported rather than refused
            // because the answer is correct, not because the subject is absent.
            b"data_version" => Ok(named_integer("data_version", 1)),
            b"max_page_count" => self.pragma_max_page_count(argument),
            // **Remembered, and exceeded.** `analysis_limit` caps how many rows
            // `ANALYZE` samples per index; this engine's `ANALYZE` walks the
            // whole table, which is *more* than the cap asks for and so is
            // never a wrong answer - only a slower one. Recording it keeps a
            // script that sets it running and reads back what it set, which is
            // what the reference does.
            b"analysis_limit" => self.pragma_analysis_limit(argument),
            b"case_sensitive_like" => self.pragma_case_sensitive_like(argument),
            // **Remembered, and there is nothing for it to permit.** In SQLite
            // this unlocks a write to `sqlite_schema`; here the binder refuses
            // a write to any reserved-prefix table whatever the flag says, and
            // a module's shadow table is an *ordinary* table that a write
            // reaches without it. So the value is recorded and reported and
            // changes nothing - which is a fact about this engine's schema
            // rather than a setting quietly dropped, and it lets a script
            // written for the reference run unchanged.
            b"writable_schema" => self.pragma_writable_schema(argument),
            b"query_only" => self.pragma_query_only(argument),
            b"recursive_triggers" => self.pragma_recursive_triggers(argument),
            b"auto_vacuum" => self.pragma_auto_vacuum(argument),
            b"secure_delete" => self.pragma_secure_delete(argument),
            b"ignore_check_constraints" => self.pragma_ignore_check_constraints(argument),
            b"automatic_index" => self.pragma_automatic_index(argument),
            // This engine's temporary tables live in memory, so DEFAULT and
            // MEMORY are both what it already does and FILE is the one value it
            // cannot be. SQLite reports the setting rather than the state, so a
            // caller that wrote MEMORY reads 2 back.
            b"temp_store" => self.pragma_temp_store(argument),
            // **The pragmas that do nothing here and nothing observable in
            // SQLite either.** `PRAGMA optimize` decides whether to re-ANALYZE
            // and answers no rows; `shrink_memory` releases what a page cache
            // is holding; `incremental_vacuum` moves free pages when
            // `auto_vacuum` is on, which it never is in either engine's default.
            // Answering nothing is the *right* answer for these, and it is the
            // answer SQLite gives, so they are named here rather than falling
            // through to the refusal - a refusal would be a difference invented
            // by the rule rather than found by it.
            b"incremental_vacuum" => self.pragma_incremental_vacuum(argument),
            b"optimize" | b"shrink_memory" | b"data_store_directory" | b"temp_store_directory" => {
                Ok(Outcome::empty())
            }
            // Reported: a number this engine has exactly one of. Read it and
            // get the truth; set it to that value and nothing happens; set it
            // to anything else and it refuses rather than pretending.
            name if reported_value(name).is_some() => {
                let (value, spellings) = reported_value(name).unwrap_or((0, &[]));
                pragma_fixed_number(argument, &String::from_utf8_lossy(name), value, spellings)
            }
            // On SQLite's list and not implemented here: refused by name, so a
            // caller can tell "no" from "nothing".
            name if is_sqlite_pragma(name) => Err(refusal(format!(
                "PRAGMA {} is not implemented by this engine",
                String::from_utf8_lossy(name)
            ))),
            // SQLite's own answer to a pragma it does not know: no rows, no
            // error, and the next statement runs.
            _ => Ok(Outcome::empty()),
        }
    }

    /// Answers the pragmas that only read, under `&self`.
    ///
    /// **The shared half of "one implementation, two spellings".** The directive
    /// form reaches it through [`ImportedDatabase::pragma`]; the table-valued
    /// form - `SELECT * FROM pragma_table_info('t')` - reaches it from inside a
    /// virtual-table scan, which runs under `&self` and so cannot go through the
    /// `&mut self` dispatcher at all. Splitting them by mutability rather than
    /// by name is what keeps the two spellings on one answer: a pragma that only
    /// reads is here and is reachable both ways, and one that changes something
    /// is a directive and has no table-valued form, which is SQLite's rule too.
    ///
    /// `Ok(None)` means this pragma is not one of the read-only ones.
    ///
    /// @param name - the pragma's folded name
    /// @param argument - the value it was given, when it was given one
    pub(super) fn pragma_rows(
        &self,
        name: &[u8],
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Option<Outcome>> {
        Ok(Some(match name {
            b"foreign_key_list" => self.pragma_foreign_key_list(argument)?,
            b"table_info" => self.pragma_table_info(argument, false)?,
            b"table_xinfo" => self.pragma_table_info(argument, true)?,
            b"index_list" => self.pragma_index_list(argument)?,
            b"index_info" => self.pragma_index_info(argument, false)?,
            b"index_xinfo" => self.pragma_index_info(argument, true)?,
            b"table_list" => self.pragma_table_list(argument)?,
            b"collation_list" => self.pragma_collation_list(),
            b"pragma_list" => list_of("name", SQLITE_PRAGMAS),
            b"module_list" => {
                let mut names = self.registry.module_names();
                names.extend(ENGINE_MODULES.iter().map(|name| (*name).to_string()));
                names.sort();
                names.dedup();
                list_of("name", &names)
            }
            b"function_list" => pragma_function_list(),
            b"compile_options" => list_of("compile_options", COMPILE_OPTIONS),
            b"database_list" => Outcome {
                rows: self.database_list(),
                names: vec!["seq".into(), "name".into(), "file".into()],
                changes: Default::default(),
            },
            _ => return Ok(None),
        }))
    }

    /// Returns the names of every pragma with a table-valued form.
    ///
    /// One per read-only pragma above, which is the set that has rows to be a
    /// function *of*. A pragma whose whole content is a setting is a directive
    /// and nothing else, and a function that answered nothing would be
    /// indistinguishable from one that found nothing.
    pub(super) fn pragma_function_names() -> &'static [&'static str] {
        &[
            "pragma_collation_list",
            "pragma_compile_options",
            "pragma_database_list",
            "pragma_foreign_key_list",
            "pragma_function_list",
            "pragma_index_info",
            "pragma_index_list",
            "pragma_index_xinfo",
            "pragma_module_list",
            "pragma_pragma_list",
            "pragma_table_info",
            "pragma_table_list",
            "pragma_table_xinfo",
        ]
    }

    /// Returns one row per database this connection holds, in schema order.
    ///
    /// `main` first, then the connection's temporary database when it has made
    /// one, then the attachments in the order they arrived - which is the order
    /// an unqualified name is resolved in, minus `temp`'s place at the front of
    /// it, and the cheapest end-to-end check that the schema set is what the
    /// connection thinks it is.
    ///
    /// A database with no file - `temp`, and `ATTACH ':memory:'` - reports an
    /// empty path, which is what SQLite reports for the same thing.
    fn database_list(&self) -> Vec<Vec<OwnedDatum>> {
        let mut rows = Vec::new();
        for (seq, at) in self.schema_numbers().into_iter().enumerate() {
            let (name, file) = match at {
                super::MAIN => (
                    b"main".to_vec(),
                    self.path.to_string_lossy().as_bytes().to_vec(),
                ),
                _ => match self.schema_at(at) {
                    Some(held) => (
                        held.name.clone(),
                        held.path
                            .as_ref()
                            .map(|path| path.to_string_lossy().as_bytes().to_vec())
                            .unwrap_or_default(),
                    ),
                    None => continue,
                },
            };
            rows.push(vec![
                OwnedDatum::Int(seq as i64),
                OwnedDatum::Text(name),
                OwnedDatum::Text(file),
            ]);
        }
        rows
    }

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
    fn pragma_cache_size(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let page_size = self.page_size.max(1);
        let Some(argument) = argument else {
            let bytes = self.frames.saturating_mul(page_size);
            return Ok(named_integer(
                "cache_size",
                self.cache_size.unwrap_or(-((bytes / 1024) as i64)),
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
        if wanted > self.database.pool().frames() {
            self.database.pool_mut().grow_frames(wanted)?;
            self.frames = self.database.pool().frames();
        }
        let pool = self.database.pool();
        let held = pool.frames() as i64;
        pool.set_budget(pages.clamp(1, held) as usize);
        // Still the truth rather than the request, for the one case the grow
        // could not satisfy: an allocation that the platform refused reports
        // what the pool ended up with, in the units the caller used.
        self.cache_size = Some(if pages > held {
            if asked < 0 {
                -(held.saturating_mul(page_size as i64) / 1024)
            } else {
                held
            }
        } else {
            asked
        });
        Ok(Outcome::empty())
    }

    /// Reads or sets how much of a commit reaches the platter.
    fn pragma_synchronous(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "synchronous",
                match self.wal.synchronous() {
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
    fn pragma_busy_timeout(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(named_integer("timeout", self.busy_timeout_ms as i64)),
            Some(argument) => {
                self.busy_timeout_ms = argument_integer(argument).max(0) as u64;
                Ok(named_integer("timeout", self.busy_timeout_ms as i64))
            }
        }
    }

    /// Reads or sets a boolean flag the engine records and reports.
    fn pragma_flag(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(named_integer("foreign_keys", i64::from(self.foreign_keys))),
            Some(argument) => {
                let asked = argument_boolean(argument);
                // **The compiled statements go with it.** Whether keys are
                // enforced is decided by the binder, once, when a statement is
                // compiled - so a statement compiled while the setting was off
                // carries no key checks and would keep carrying none after the
                // pragma turned them on. Which is exactly the shape of bug this
                // pragma exists to avoid, since the symptom is a write that is
                // accepted rather than an error that is reported.
                if asked != self.foreign_keys {
                    self.forget_compiled_statements();
                }
                self.foreign_keys = asked;
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
    fn pragma_defer(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(named_integer(
                "defer_foreign_keys",
                i64::from(self.defer_foreign_keys),
            )),
            Some(argument) => {
                let asked = argument_boolean(argument);
                if asked != self.defer_foreign_keys {
                    self.forget_compiled_statements();
                }
                self.defer_foreign_keys = asked;
                Ok(Outcome::empty())
            }
        }
    }

    /// Reports the foreign keys one table declares, in SQLite's own columns.
    ///
    /// One row per key column rather than per key: a composite key reports its
    /// columns in `seq` order under one `id`, which is how an application
    /// reconstructs the pair.
    ///
    /// @param argument - the table named in the pragma
    fn pragma_foreign_key_list(&self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let names = vec![
            "id".into(),
            "seq".into(),
            "table".into(),
            "from".into(),
            "to".into(),
            "on_update".into(),
            "on_delete".into(),
            "match".into(),
        ];
        let Some(table) = self.named_table(argument) else {
            return Ok(Outcome {
                rows: Vec::new(),
                names,
                changes: Default::default(),
            });
        };
        let mut rows = Vec::new();
        for key in &table.foreign_keys {
            let parent = self
                .tables
                .iter()
                .find(|candidate| candidate.folded == key.parent_folded);
            let targets = parent
                .and_then(|parent| inillucent_sql::foreign_key::parent_columns(key, parent))
                .unwrap_or_default();
            for (position, column) in key.columns.iter().enumerate() {
                let from = table
                    .columns
                    .get(usize::from(*column))
                    .map(|info| info.name.clone())
                    .unwrap_or_default();
                rows.push(vec![
                    OwnedDatum::Int(i64::from(key.id)),
                    OwnedDatum::Int(position as i64),
                    OwnedDatum::Text(key.parent.clone()),
                    OwnedDatum::Text(from),
                    match targets.get(position) {
                        Some(name) => OwnedDatum::Text(name.clone()),
                        None => OwnedDatum::Null,
                    },
                    OwnedDatum::Text(action_name(key.on_update).as_bytes().to_vec()),
                    OwnedDatum::Text(action_name(key.on_delete).as_bytes().to_vec()),
                    OwnedDatum::Text(if key.match_clause.is_empty() {
                        b"NONE".to_vec()
                    } else {
                        key.match_clause.clone()
                    }),
                ]);
            }
        }
        Ok(Outcome {
            rows,
            names,
            changes: Default::default(),
        })
    }

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
    fn pragma_foreign_key_check(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let only = argument.map(|argument| argument_text(argument).to_ascii_lowercase());
        let mut rows = Vec::new();
        for query in self.violation_queries(only.as_deref())? {
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
            names: vec![
                "table".into(),
                "rowid".into(),
                "parent".into(),
                "fkid".into(),
            ],
            changes: Default::default(),
        })
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
    fn pragma_fixed_word(
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

    /// Runs the integrity checker over every tree.
    ///
    /// `ok` when they all hold, and the first failure otherwise, which is the
    /// shape SQLite's answer has.
    ///
    /// **One check, two names.** `integrity_check` and `quick_check` are the
    /// same pass here - this engine has no faster, sampled variant of the
    /// walk - and SQLite's own two pragmas name their column after whichever
    /// of the two was asked, not after a shared implementation. Reporting
    /// `integrity_check` for both told a caller that ran `PRAGMA quick_check`
    /// it had received the wrong pragma's answer.
    ///
    /// @param column - which of the two names asked for this, and so which one
    ///   the answer is reported under
    fn pragma_integrity_check(&mut self, column: &str) -> DbResult<Outcome> {
        let answer = match self.check_trees() {
            Ok(()) => b"ok".to_vec(),
            Err(error) => error
                .detail()
                .unwrap_or(error.message())
                .as_bytes()
                .to_vec(),
        };
        Ok(Outcome {
            rows: vec![vec![OwnedDatum::Text(answer)]],
            names: vec![column.into()],
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
    fn pragma_wal_checkpoint(&mut self) -> DbResult<Outcome> {
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
        if self.batch.get().is_some() && self.touched != 0 {
            return Err(DbError::primary(PrimaryCode::Locked)
                .with_detail("cannot checkpoint: a transaction has written and not committed"));
        }
        // **Minus one twice when there is no log to check point.** SQLite
        // answers `0|-1|-1` under a rollback journal because the two counts are
        // "frames in the log" and "frames moved", and a database with no
        // write-ahead log has neither - which is a different statement from
        // "no frames moved". A caller polling the second column to decide
        // whether a checkpoint is due needs to be able to tell those apart.
        if self.journal_mode() != inillucent_pool::journal::JournalMode::Wal {
            self.checkpoint()?;
            return Ok(Outcome {
                rows: vec![vec![
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(-1),
                    OwnedDatum::Int(-1),
                ]],
                names: vec!["busy".into(), "log".into(), "checkpointed".into()],
                changes: Default::default(),
            });
        }
        let before = self.database.pool().stats().writes;
        self.checkpoint()?;
        let moved = self.database.pool().stats().writes.saturating_sub(before) as i64;
        Ok(Outcome {
            rows: vec![vec![
                OwnedDatum::Int(0),
                OwnedDatum::Int(moved),
                OwnedDatum::Int(moved),
            ]],
            names: vec!["busy".into(), "log".into(), "checkpointed".into()],
            changes: Default::default(),
        })
    }

    /// Describes one table's columns.
    ///
    /// `extended` is `table_xinfo`, which differs in two ways: it shows the
    /// columns `table_info` hides - a virtual table's arguments and a generated
    /// column - and it carries a seventh column saying which kind of hidden
    /// each one is. That seventh column is why an ORM can tell a generated
    /// column from an ordinary one, and it was missing.
    ///
    /// @param argument - the table named in the pragma
    /// @param extended - whether this is the `xinfo` spelling
    fn pragma_table_info(
        &self,
        argument: Option<&PragmaArgument>,
        extended: bool,
    ) -> DbResult<Outcome> {
        // **The names come first and are answered even when nothing matched.**
        // A statement's result columns are a fact about the *pragma*, not about
        // whether the table it was asked about exists - and the table-valued
        // form reads them at catalog time, with no argument at all, to learn
        // what columns `pragma_table_info` declares.
        let mut names: Vec<String> = ["cid", "name", "type", "notnull", "dflt_value", "pk"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        if extended {
            names.push("hidden".to_string());
        }
        let Some(table) = self.named_table(argument) else {
            return Ok(Outcome {
                rows: Vec::new(),
                names,
                changes: Default::default(),
            });
        };
        // **A view has columns, and they are the columns its `SELECT`
        // produces.** Nothing in the file records them, so they are bound here
        // - which is why `PRAGMA table_info(v)` answered nothing at all, and
        // why every ORM that reads this pragma could not see a view.
        let bound;
        let columns: &[inillucent_sql::catalog_view::ColumnInfo] =
            if table.kind == inillucent_sql::catalog_view::TableKind::View {
                bound = self.view_columns(table);
                &bound
            } else {
                &table.columns
            };
        let mut rows = Vec::new();
        // **`cid` counts the columns the pragma reports, not the columns the
        // table declares.** They are the same number until a table has a
        // generated column, which the plain form hides - and SQLite then
        // numbers what is left 0, 1, 2 rather than leaving a gap where the
        // hidden one was. The extended form shows every column, so its `cid`
        // is the declared position.
        let mut cid = 0i64;
        for (position, column) in columns.iter().enumerate() {
            if !extended && (column.hidden || column.generated) {
                continue;
            }
            let reported = if extended { position as i64 } else { cid };
            cid = cid.saturating_add(1);
            // 1 is a virtual table's hidden column; 2 is a VIRTUAL generated
            // column and 3 a STORED one. The two generated codes are the way
            // round SQLite has them, which is not the way round the keywords
            // suggest.
            let hidden = if column.generated {
                if column.stored {
                    3
                } else {
                    2
                }
            } else {
                i64::from(column.hidden)
            };
            let mut row = vec![
                OwnedDatum::Int(reported),
                OwnedDatum::Text(column.name.clone()),
                OwnedDatum::Text(column.declared_type.clone()),
                OwnedDatum::Int(i64::from(column.not_null)),
                match &column.default_sql {
                    Some(text) => OwnedDatum::Text(text.clone()),
                    None => OwnedDatum::Null,
                },
                // `primary_key_position` is already one-based, which is what
                // SQLite's `pk` column holds, and zero for a column that is not
                // in the key.
                OwnedDatum::Int(column.primary_key_position.map(i64::from).unwrap_or(0)),
            ];
            if extended {
                row.push(OwnedDatum::Int(hidden));
            }
            rows.push(row);
        }
        Ok(Outcome {
            rows,
            names,
            changes: Default::default(),
        })
    }

    /// Returns the columns a view's `SELECT` produces.
    ///
    /// Bound rather than stored, because the file records a view's text and not
    /// its shape. A view whose body no longer binds - a table it reads was
    /// dropped - answers no columns rather than failing the pragma, which is
    /// what SQLite does with the same thing.
    ///
    /// @param table - the view
    fn view_columns(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
    ) -> Vec<inillucent_sql::catalog_view::ColumnInfo> {
        let Some(body) = table.view.as_ref() else {
            return Vec::new();
        };
        let authorizer = inillucent_sql::bind::AllowAll;
        let mut binder = inillucent_sql::bind::Binder::new(&self.catalog, &body.ast, &authorizer);
        let Ok(bound) = binder.bind_select(body.select) else {
            return Vec::new();
        };
        let declared = body.columns.clone();
        bound
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| {
                let name = declared
                    .get(position)
                    .cloned()
                    .unwrap_or_else(|| column.name.clone());
                inillucent_sql::catalog_view::ColumnInfo {
                    folded: name.to_ascii_lowercase(),
                    name,
                    declared_type: column.declared_type.clone(),
                    affinity: inillucent_value::affinity::for_column(&column.declared_type),
                    collation: b"binary".to_vec(),
                    not_null: false,
                    not_null_conflict: None,
                    primary_key_conflict: None,
                    default_sql: None,
                    primary_key_position: None,
                    hidden: false,
                    generated: false,
                    stored: true,
                    generated_sql: None,
                }
            })
            .collect()
    }

    /// Lists one table's indexes, newest first, as SQLite does.
    fn pragma_index_list(&self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let names: Vec<String> = ["seq", "name", "unique", "origin", "partial"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        let Some(table) = self.named_table(argument) else {
            return Ok(Outcome {
                rows: Vec::new(),
                names,
                changes: Default::default(),
            });
        };
        // **A `WITHOUT ROWID` table's own primary key has no tree of its own
        // - and `PRAGMA index_list` reports it anyway.** SQLite writes no
        // `sqlite_autoindex` row to `sqlite_schema` for it and builds no
        // second b-tree (`convertToWithoutRowidTable` repoints the in-memory
        // `Index` at the table's own root and skips the schema write), but
        // that `Index` stays on the table's index chain, and `index_list`
        // walks the chain rather than the schema table. So the reference
        // answers `[0, "sqlite_autoindex_u_1", 1, "pk", 0]` for a `WITHOUT
        // ROWID` table's composite key, which this row used to filter out on
        // the theory that SQLite never lists it - checked against the pinned
        // `sqlite3.c`'s own `PragTyp_INDEX_LIST` case, which has no such
        // filter.
        let rows: Vec<Vec<OwnedDatum>> = table
            .indexes
            .iter()
            .rev()
            .enumerate()
            .map(|(seq, index)| {
                let automatic = index.name.starts_with(b"sqlite_autoindex_");
                vec![
                    OwnedDatum::Int(seq as i64),
                    OwnedDatum::Text(index.name.clone()),
                    OwnedDatum::Int(i64::from(index.unique)),
                    OwnedDatum::Text(if automatic {
                        b"pk".to_vec()
                    } else {
                        b"c".to_vec()
                    }),
                    // **The `partial` column, which was a hard zero while a
                    // partial index could not be created.** It can now, and an
                    // application asks this column precisely to find out
                    // whether an index answers every row - so answering `0` for
                    // one that does not is the kind of difference that only
                    // shows up in somebody's data.
                    OwnedDatum::Int(i64::from(index.partial_sql.is_some())),
                ]
            })
            .collect();
        Ok(Outcome {
            rows,
            names,
            changes: Default::default(),
        })
    }

    /// Describes one index's key columns.
    ///
    /// `extended` is `index_xinfo`, which answered the same three columns as
    /// `index_info` and so said nothing the plain form did not. It has six:
    /// the key's sort direction, the collation it is ordered by, and whether
    /// the entry is a *key* column or one of the row-identifying columns the
    /// index carries after them - plus the trailing row for the rowid itself,
    /// which is what makes an index's real key visible.
    ///
    /// @param argument - the index named in the pragma
    /// @param extended - whether this is the `xinfo` spelling
    fn pragma_index_info(
        &self,
        argument: Option<&PragmaArgument>,
        extended: bool,
    ) -> DbResult<Outcome> {
        let mut names: Vec<String> = ["seqno", "cid", "name"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        if extended {
            for held in ["desc", "coll", "key"] {
                names.push(held.to_string());
            }
        }
        let empty = Outcome {
            rows: Vec::new(),
            names: names.clone(),
            changes: Default::default(),
        };
        let Some(argument) = argument else {
            return Ok(empty);
        };
        let wanted = argument_text(argument).to_ascii_lowercase().into_bytes();
        let found = self.tables.iter().find_map(|table| {
            table
                .indexes
                .iter()
                .find(|index| index.folded == wanted)
                .map(|index| (table, index))
        });
        let Some((table, index)) = found else {
            return Ok(empty);
        };
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::new();
        for (seq, key) in index.columns.iter().enumerate() {
            let mut row = vec![
                OwnedDatum::Int(seq as i64),
                OwnedDatum::Int(key.column.map(i64::from).unwrap_or(-2)),
                match key.column.and_then(|at| table.column(at)) {
                    Some(column) => OwnedDatum::Text(column.name.clone()),
                    None => OwnedDatum::Null,
                },
            ];
            if extended {
                row.push(OwnedDatum::Int(i64::from(key.declared_descending)));
                row.push(OwnedDatum::Text(collation_name(&key.collation)));
                row.push(OwnedDatum::Int(1));
            }
            rows.push(row);
        }
        if extended && table.has_rowid() {
            // **The row every index has and none of them declares.** An index
            // over a rowid table carries the rowid after its key columns, which
            // is how a lookup finds the table row; SQLite reports it as cid -1
            // with a NULL name and `key` 0, and an application reading this to
            // work out an index's real width needs it.
            rows.push(vec![
                OwnedDatum::Int(rows.len() as i64),
                OwnedDatum::Int(-1),
                OwnedDatum::Null,
                OwnedDatum::Int(0),
                OwnedDatum::Text(b"BINARY".to_vec()),
                OwnedDatum::Int(0),
            ]);
        }
        Ok(Outcome {
            rows,
            names,
            changes: Default::default(),
        })
    }

    /// Lists every table in the schema, or one of them by name.
    ///
    /// **A view is reported as a view, and the schema tables are reported.**
    /// Every row used to say `table`, so a caller reading this to decide what
    /// it could write to was told a view was writable; and `sqlite_schema` and
    /// `sqlite_temp_schema` were missing, which SQLite lists last and a tool
    /// walking the catalog expects to find.
    ///
    /// **`PRAGMA table_list(t)` narrows to one table.** The pinned reference's
    /// own `PragTyp_TABLE_LIST` case skips every row whose name does not match
    /// the argument case-insensitively; this used to answer the full list
    /// regardless, which told a caller asking about one table what every
    /// table looked like.
    ///
    /// @param argument - the table to narrow to, when one was given
    fn pragma_table_list(&self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let wanted = argument.map(|argument| argument_text(argument).to_ascii_lowercase());
        let matches = |folded: &[u8]| {
            wanted
                .as_deref()
                .is_none_or(|wanted| folded == wanted.as_bytes())
        };
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::new();
        // Newest first, which is the order SQLite reports and the order
        // `index_list` already uses for the same reason.
        for table in self.tables.iter().rev() {
            if table.folded == b"sqlite_schema" || table.folded == b"sqlite_temp_schema" {
                continue;
            }
            if !matches(&table.folded) {
                continue;
            }
            let kind: &[u8] = match table.kind {
                inillucent_sql::catalog_view::TableKind::View => b"view",
                inillucent_sql::catalog_view::TableKind::Virtual => b"virtual",
                _ => b"table",
            };
            let ncol = if table.kind == inillucent_sql::catalog_view::TableKind::View {
                self.view_columns(table).len() as i64
            } else {
                table.columns.len() as i64
            };
            rows.push(vec![
                OwnedDatum::Text(b"main".to_vec()),
                OwnedDatum::Text(table.name.clone()),
                OwnedDatum::Text(kind.to_vec()),
                OwnedDatum::Int(ncol),
                // **`wr` reported the table's own flag, not a constant.** Every
                // row said 0 regardless of how the table was declared, so a
                // `CREATE TABLE ... WITHOUT ROWID` table was told apart from an
                // ordinary one by nothing this pragma answers - `table_info`
                // still named its columns correctly, but a caller that reads
                // `table_list` to decide whether a table has a rowid before
                // choosing how to reference a row was told every table did.
                OwnedDatum::Int(i64::from(table.without_rowid)),
                OwnedDatum::Int(i64::from(table.strict)),
            ]);
        }
        for (schema, name) in [
            (b"main".as_slice(), b"sqlite_schema".as_slice()),
            (b"temp".as_slice(), b"sqlite_temp_schema".as_slice()),
        ] {
            if !matches(name) {
                continue;
            }
            rows.push(vec![
                OwnedDatum::Text(schema.to_vec()),
                OwnedDatum::Text(name.to_vec()),
                OwnedDatum::Text(b"table".to_vec()),
                OwnedDatum::Int(5),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
            ]);
        }
        let rows: Vec<Vec<OwnedDatum>> = rows;
        Ok(Outcome {
            rows,
            names: vec![
                "schema".into(),
                "name".into(),
                "type".into(),
                "ncol".into(),
                "wr".into(),
                "strict".into(),
            ],
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
    fn pragma_user_version(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "user_version",
                i64::from(self.database.user_version()),
            ));
        };
        let value = argument_integer(argument) as i32;
        self.database.set_user_version(value);
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
    fn pragma_application_id(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "application_id",
                i64::from(self.database.application_id()),
            ));
        };
        let value = argument_integer(argument) as i32;
        self.database.set_application_id(value);
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
    fn pragma_max_page_count(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        if let Some(argument) = argument {
            let asked = argument_integer(argument);
            let held = self.database.pool().page_count() as i64;
            self.max_page_count = asked.max(held).min(DEFAULT_MAX_PAGE_COUNT);
        }
        Ok(named_integer("max_page_count", self.max_page_count))
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
    fn pragma_case_sensitive_like(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(Outcome::empty());
        };
        let asked = argument_boolean(argument);
        if asked != self.case_sensitive_like {
            self.forget_compiled_statements();
        }
        self.case_sensitive_like = asked;
        Ok(Outcome::empty())
    }

    /// Reads or sets how many rows `ANALYZE` may sample per index.
    ///
    /// See the dispatcher: this engine walks the whole table, which is more
    /// than any cap asks for.
    ///
    /// @param argument - the value it was given, when it was given one
    fn pragma_analysis_limit(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        if let Some(argument) = argument {
            self.analysis_limit = argument_integer(argument).max(0);
        }
        Ok(named_integer("analysis_limit", self.analysis_limit))
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
    fn pragma_locking_mode(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
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
    fn locking_word(&self) -> &'static str {
        if self.locking_exclusive() {
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
    fn pragma_journal_mode(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(word_row("journal_mode", self.journal_mode().word()));
        };
        let asked = argument_text(argument);
        if let Some(mode) = inillucent_pool::journal::JournalMode::named(asked.trim()) {
            // **Defensive mode refuses `OFF` and says so by not moving.** That
            // is SQLite's own rule and it is why the reference's shell - which
            // turns the flag on - answers `PRAGMA journal_mode = OFF` with the
            // mode that was already in force. `off` protects nothing, so a
            // connection that has asked to be protected from itself cannot have
            // it.
            if self.defensive && mode == inillucent_pool::journal::JournalMode::Off {
                return Ok(word_row("journal_mode", self.journal_mode().word()));
            }
            self.set_journal_mode(mode)?;
        }
        Ok(word_row("journal_mode", self.journal_mode().word()))
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
    fn pragma_auto_vacuum(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer("auto_vacuum", i64::from(self.auto_vacuum)));
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
                .tables
                .iter()
                .all(|table| table.folded.starts_with(b"sqlite_"))
            {
                self.auto_vacuum = mode;
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
    fn pragma_incremental_vacuum(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        if self.auto_vacuum != 2 {
            return Ok(Outcome::empty());
        }
        let pages = argument.map(argument_integer).unwrap_or(i64::MAX).max(0);
        self.reclaim_free_pages(usize::try_from(pages).unwrap_or(usize::MAX))?;
        Ok(Outcome::empty())
    }

    /// Reads or sets `secure_delete`.
    ///
    /// @param argument - the setting, when one was given
    fn pragma_secure_delete(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "secure_delete",
                i64::from(self.secure_delete),
            ));
        };
        self.secure_delete = match argument_text(argument).trim().to_ascii_lowercase().as_str() {
            "2" | "fast" => 2,
            _ => u8::from(argument_boolean(argument)),
        };
        Ok(named_integer(
            "secure_delete",
            i64::from(self.secure_delete),
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
    fn pragma_ignore_check_constraints(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "ignore_check_constraints",
                i64::from(self.ignore_check_constraints),
            ));
        };
        let asked = argument_boolean(argument);
        if asked != self.ignore_check_constraints {
            self.forget_compiled_statements();
        }
        self.ignore_check_constraints = asked;
        Ok(Outcome::empty())
    }

    /// Reads or sets `automatic_index`.
    ///
    /// @param argument - the setting, when one was given
    fn pragma_automatic_index(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "automatic_index",
                i64::from(self.automatic_index),
            ));
        };
        let asked = argument_boolean(argument);
        if asked != self.automatic_index {
            self.forget_compiled_statements();
        }
        self.automatic_index = asked;
        // The planner reads the lever, not the field: a plan carries the levers
        // it was built under, so the two have to move together.
        self.set_automatic_index(asked);
        Ok(Outcome::empty())
    }

    /// Reads or sets `writable_schema`, which this engine records and honours
    /// by having nothing for it to unlock.
    ///
    /// See the dispatcher for why. `PRAGMA writable_schema` with no argument
    /// reports what was set, which is what the reference does.
    ///
    /// @param argument - the value it was given, when it was given one
    fn pragma_writable_schema(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            // **Zero, whatever was set.** It is what the reference reads back
            // too - it clears the flag as soon as the schema is re-read - and
            // here it is the literal truth: there is nothing this engine's
            // catalog will let a statement write with the flag on that it
            // refuses with it off.
            return Ok(named_integer("writable_schema", 0));
        };
        self.writable_schema = argument_boolean(argument);
        Ok(Outcome::empty())
    }

    /// Reads or sets whether this connection may write.
    ///
    /// **Honoured rather than remembered.** A caller sets `query_only` to make
    /// a mistake impossible, so a connection that recorded it and wrote anyway
    /// would be worse than one that refused the pragma outright.
    ///
    /// @param argument - the value it was given, when it was given one
    fn pragma_query_only(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer("query_only", i64::from(self.query_only)));
        };
        self.query_only = argument_boolean(argument);
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
    fn pragma_recursive_triggers(
        &mut self,
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer(
                "recursive_triggers",
                i64::from(self.recursive_triggers),
            ));
        };
        let asked = argument_boolean(argument);
        if asked != self.recursive_triggers {
            self.forget_compiled_statements();
        }
        self.recursive_triggers = asked;
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
    fn pragma_temp_store(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(named_integer("temp_store", self.temp_store));
        };
        let text = argument_text(argument).trim().to_ascii_lowercase();
        self.temp_store = match text.as_str() {
            "0" | "default" => 0,
            "2" | "memory" => 2,
            other => {
                return Err(refusal(format!(
                    "temp_store {other} is not available here; temporary tables live in memory"
                )))
            }
        };
        Ok(Outcome::empty())
    }

    /// Lists the collations this connection can order by.
    ///
    /// The three built in, then whatever the application registered, which is
    /// what SQLite reports and in the same shape.
    fn pragma_collation_list(&self) -> Outcome {
        // The order is the reference's own, which is neither alphabetical nor
        // registration order - it is the order its hash table happens to walk
        // in. Neither engine's order carries meaning, so matching the pinned
        // build's costs nothing and makes the two transcripts comparable.
        let mut names: Vec<String> = ["decimal", "BINARY", "NOCASE", "RTRIM", "uint"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        for (name, _) in &self.collations {
            if !names.iter().any(|held| held.eq_ignore_ascii_case(name)) {
                names.push(name.clone());
            }
        }
        Outcome {
            rows: names
                .iter()
                .enumerate()
                .map(|(seq, name)| {
                    vec![
                        OwnedDatum::Int(seq as i64),
                        OwnedDatum::Text(name.as_bytes().to_vec()),
                    ]
                })
                .collect(),
            names: vec!["seq".into(), "name".into()],
            changes: Default::default(),
        }
    }

    /// Returns the table a pragma's argument names.
    fn named_table(
        &self,
        argument: Option<&PragmaArgument>,
    ) -> Option<&inillucent_sql::catalog_view::TableInfo> {
        let argument = argument?;
        let wanted = argument_text(argument).to_ascii_lowercase().into_bytes();
        self.tables.iter().find(|table| table.folded == wanted).or(
            if wanted == b"sqlite_schema" || wanted == b"sqlite_master" {
                Some(&self.schema_info)
            } else {
                None
            },
        )
    }
}

/// Returns a one-row, one-column answer holding a word, named after the
/// pragma that reports it.
///
/// **Named after the pragma, because that is what SQLite names it.** SQLite's
/// own `returnSingleInt`/`returnSingleText` helpers take the label as an
/// argument at every call site rather than deriving it, and for a simple
/// single-value pragma the label its own call sites pass is the pragma's own
/// name - `PRAGMA journal_mode` answers a column called `journal_mode`, not
/// `value`. A caller that reads the column by name rather than by position -
/// which is exactly what a script comparing this engine's answer against
/// SQLite's would do - was being told the wrong column existed.
///
/// @param name - the column name SQLite reports for this pragma
/// @param word - the value
fn word_row(name: &str, word: &str) -> Outcome {
    Outcome {
        rows: vec![vec![OwnedDatum::Text(word.as_bytes().to_vec())]],
        names: vec![name.into()],
        changes: Default::default(),
    }
}

/// Returns a one-row, one-column answer holding a number, named after the
/// pragma that reports it.
///
/// See [`word_row`] for why the column is named after the pragma rather than
/// called `value`.
///
/// @param name - the column name SQLite reports for this pragma
/// @param value - the number
fn named_integer(name: &str, value: i64) -> Outcome {
    Outcome {
        rows: vec![vec![OwnedDatum::Int(value)]],
        names: vec![name.into()],
        changes: Default::default(),
    }
}

/// Returns the spelling `PRAGMA foreign_key_list` reports for an action.
///
/// @param action - the referential action the key declared
fn action_name(action: inillucent_sql::ast::ReferentialAction) -> &'static str {
    use inillucent_sql::ast::ReferentialAction;
    match action {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
        ReferentialAction::Cascade => "CASCADE",
    }
}

/// The ceiling `PRAGMA max_page_count` reports when nothing has been set.
///
/// SQLite's own maximum, so a caller reading it before setting it is told the
/// same number by either engine.
pub(crate) const DEFAULT_MAX_PAGE_COUNT: i64 = 4_294_967_294;

/// Returns whether a name is one SQLite's own `pragma_list` carries.
///
/// The whole point of the list: a name on it that this engine does not answer
/// is **refused**, so a caller can tell "this engine will not do that" from
/// "that returned no rows". A name on nobody's list stays silent, which is what
/// SQLite does with one.
///
/// @param name - the pragma's folded name
fn is_sqlite_pragma(name: &[u8]) -> bool {
    SQLITE_PRAGMAS
        .iter()
        .any(|held| held.as_bytes().eq_ignore_ascii_case(name))
}

/// Answers a pragma this engine has exactly one value for.
///
/// Reading it gives that value; setting it to what it already is succeeds and
/// setting it to anything else refuses. That is the rule `journal_mode = DELETE`
/// has always followed, said once for the pragmas that now need it.
///
/// @param argument - the value it was given, when it was given one
/// @param name - the pragma's name, for the refusal
/// @param value - the number it reports
/// @param accepted - the spellings that mean that number
fn pragma_fixed_number(
    argument: Option<&PragmaArgument>,
    name: &str,
    value: i64,
    accepted: &[&str],
) -> DbResult<Outcome> {
    let Some(argument) = argument else {
        return Ok(named_integer(name, value));
    };
    let text = argument_text(argument).trim().to_ascii_lowercase();
    if accepted.iter().any(|held| *held == text) {
        return Ok(Outcome::empty());
    }
    Err(refusal(format!(
        "this engine's {name} is {value} and cannot be set to {text}"
    )))
}

/// Returns a one-column answer over a list of names.
///
/// @param column - what the column is called
/// @param values - the names, in the order they are reported
fn list_of<S: AsRef<str>>(column: &str, values: &[S]) -> Outcome {
    Outcome {
        rows: values
            .iter()
            .map(|value| vec![OwnedDatum::Text(value.as_ref().as_bytes().to_vec())])
            .collect(),
        names: vec![column.to_string()],
        changes: Default::default(),
    }
}

/// Every pragma name SQLite 3.53.4's own `pragma_list` reports.
///
/// **The list is the contract.** A name here that this engine does not answer
/// is refused by name rather than silently accepted - 38 of these used to
/// answer nothing at all. It is also what
/// `pragma_pragma_list` reports, so a tool can ask this engine what it knows
/// about - and get the same set of names either engine would give, with the
/// difference showing up as a refusal rather than as an absence.
pub(crate) const SQLITE_PRAGMAS: &[&str] = &[
    "analysis_limit",
    "application_id",
    "auto_vacuum",
    "automatic_index",
    "busy_timeout",
    "cache_size",
    "cache_spill",
    "case_sensitive_like",
    "cell_size_check",
    "checkpoint_fullfsync",
    "collation_list",
    "compile_options",
    "count_changes",
    "data_store_directory",
    "data_version",
    "database_list",
    "default_cache_size",
    "defer_foreign_keys",
    "empty_result_callbacks",
    "encoding",
    "foreign_key_check",
    "foreign_key_list",
    "foreign_keys",
    "freelist_count",
    "full_column_names",
    "fullfsync",
    "function_list",
    "hard_heap_limit",
    "ignore_check_constraints",
    "incremental_vacuum",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "journal_mode",
    "journal_size_limit",
    "legacy_alter_table",
    "locking_mode",
    "max_page_count",
    "mmap_size",
    "module_list",
    "optimize",
    "page_count",
    "page_size",
    "pragma_list",
    "query_only",
    "quick_check",
    "read_uncommitted",
    "recursive_triggers",
    "reverse_unordered_selects",
    "schema_version",
    "secure_delete",
    "short_column_names",
    "shrink_memory",
    "soft_heap_limit",
    "synchronous",
    "table_info",
    "table_list",
    "table_xinfo",
    "temp_store",
    "temp_store_directory",
    "threads",
    "trusted_schema",
    "user_version",
    "wal_autocheckpoint",
    "wal_checkpoint",
    "writable_schema",
];

/// What `PRAGMA compile_options` reports about this build.
///
/// One line per entry of [`inillucent_base::COMPILE_OPTIONS`], which is where
/// the list lives so that `sqlite_compileoption_get` and
/// `sqlite_compileoption_used` answer from the same one.
pub(crate) const COMPILE_OPTIONS: &[&str] = inillucent_base::COMPILE_OPTIONS;

/// Lists the functions this engine answers, in SQLite's own columns.
///
/// Read straight out of `inillucent_sql::function::every_function`, which is
/// the same list the obligations report and the old engine's `function_list`
/// read - so a function that exists is reported by every route or by none.
/// `enc` is `utf8` because that is the only text encoding here.
fn pragma_function_list() -> Outcome {
    let rows = inillucent_sql::function::every_function()
        .into_iter()
        .map(|entry| {
            vec![
                OwnedDatum::Text(entry.name.as_bytes().to_vec()),
                OwnedDatum::Int(1),
                OwnedDatum::Text(entry.kind.as_bytes().to_vec()),
                OwnedDatum::Text(b"utf8".to_vec()),
                OwnedDatum::Int(entry.arity),
                OwnedDatum::Int(entry.flags),
            ]
        })
        .collect();
    Outcome {
        rows,
        names: vec![
            "name".into(),
            "builtin".into(),
            "type".into(),
            "enc".into(),
            "narg".into(),
            "flags".into(),
        ],
        changes: Default::default(),
    }
}

/// Returns the name `index_xinfo` reports a key's collation by.
///
/// The catalog folds a collation name; SQLite reports the spelling it uses in
/// its own answers, which is upper case for the three built in and the name as
/// registered for anything else.
///
/// @param folded - the folded collation name the key carries
fn collation_name(folded: &[u8]) -> Vec<u8> {
    match folded {
        b"" | b"binary" => b"BINARY".to_vec(),
        b"nocase" => b"NOCASE".to_vec(),
        b"rtrim" => b"RTRIM".to_vec(),
        other => other.to_vec(),
    }
}

/// The eponymous tables the engine itself presents, which are modules too.
///
/// **They are answered by the engine rather than by a `Module`, and the
/// register did not know about them.** `dbstat` and `sqlite_dbpage` describe
/// the *pages* under every tree, and `bytecode`, `tables_used`, `sqlite_stmt`
/// and `completion` describe statements - neither is a thing a `Module` can
/// see, so both are implemented in `crate::inspect` and `crate::introspect` and
/// registered nowhere. `pragma_module_list` therefore reported fourteen names
/// where SQLite reports nineteen, while every one of these answered exactly as
/// SQLite's does when it was called.
///
/// A caller that reads the register to decide what it may use was
/// being told less than the truth, silently, and that is the one shape of
/// difference this project treats as a defect rather than a choice.
const ENGINE_MODULES: &[&str] = &[
    "bytecode",
    "completion",
    "dbstat",
    "sqlite_dbpage",
    "sqlite_stmt",
    "tables_used",
];

/// The spellings that mean "off" for a pragma this engine reports as zero.
const OFF: &[&str] = &["0", "off", "false", "no"];

/// The spellings that mean "on", for a pragma this engine reports as one.
const ON: &[&str] = &["1", "on", "true", "yes"];

/// Returns the one value this engine has for a pragma, and how to spell it.
///
/// **The third disposition, as a table.** Each of these is a setting whose
/// subject exists here and has exactly one state: this engine runs on one
/// thread, maps no pages, limits no heap, spills no cache and has no legacy
/// callback API for `count_changes` to change the shape of. Reading one is
/// answering a question truthfully; setting it to what it already is costs
/// nothing; setting it to anything else is refused, because accepting a
/// setting that will not be honoured is the failure this whole part is about.
///
/// Four of them are values SQLite reports differently, and each difference is
/// this engine telling the truth about itself: `automatic_index` is 0 because
/// it never builds one where SQLite reports 1; `cache_spill` is 0 because the
/// pool evicts by clock rather than at a threshold; `wal_autocheckpoint` is 0
/// because the log is folded in at an explicit checkpoint rather than every
/// thousand frames; `default_cache_size` follows the pool this file was opened
/// with. They are named in `docs/feature-comparison.md` for that reason.
///
/// @param name - the pragma's folded name
fn reported_value(name: &[u8]) -> Option<(i64, &'static [&'static str])> {
    Some(match name {
        // The pool evicts by clock rather than at a spill threshold.
        b"cache_spill" => (0, OFF),
        // Not a page format with cells to size-check.
        b"cell_size_check" => (0, OFF),
        b"checkpoint_fullfsync" => (0, OFF),
        b"fullfsync" => (0, OFF),
        // The legacy callback API these three shape does not exist here.
        b"count_changes" => (0, OFF),
        b"empty_result_callbacks" => (0, OFF),
        b"full_column_names" => (0, OFF),
        // Which SQLite also reports as 1.
        b"short_column_names" => (1, ON),
        // No heap limit of either kind.
        b"hard_heap_limit" => (0, OFF),
        b"soft_heap_limit" => (0, OFF),

        // The log is rolled at a checkpoint rather than trimmed to a size.
        b"journal_size_limit" => (-1, &["-1"]),
        // `ALTER TABLE RENAME` here always rewrites the references, which is
        // what SQLite's non-legacy behaviour is.
        b"legacy_alter_table" => (0, OFF),
        // Pages are read, never mapped.
        b"mmap_size" => (0, OFF),
        // One writer, one pool, no shared cache to read uncommitted from.
        b"read_uncommitted" => (0, OFF),
        b"reverse_unordered_selects" => (0, OFF),
        // Single threaded by design; the sorter and the tree builder are too.
        b"threads" => (0, OFF),
        // A schema object is never treated as trusted input here, and the
        // catalog is not writable as a table.
        b"trusted_schema" => (0, OFF),
        // The log is folded in at an explicit checkpoint rather than every N
        // frames, so there is no frame count to set.
        b"wal_autocheckpoint" => (0, OFF),
        _ => return None,
    })
}
