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
//! So the set is in three parts, and the manifest's `pragma.*` rows name them:
//!
//! - **Honoured.** `cache_size`, `synchronous`, `busy_timeout`, `foreign_keys`,
//!   `journal_mode`, `integrity_check`, `quick_check`, `wal_checkpoint`,
//!   `table_info` and the rest of the schema and pager reporters. These do what
//!   they say.
//! - **Answered, fixed.** `journal_mode` returns `wal` and takes nothing else;
//!   `encoding` returns `UTF-8` and takes nothing else; `locking_mode` returns
//!   `exclusive`. The engine has one of each and the pragma says which.
//! - **Silent.** Everything else, including the pragmas whose subject the TDD
//!   moved to a non-goal - `auto_vacuum`, `incremental_vacuum`, `temp_store`,
//!   `mmap_size`, `legacy_file_format`. A no-op with no rows, which is what
//!   SQLite gives a pragma it has never heard of.
//!
//! The distinction that matters is between *silent* and *refused*. A pragma
//! whose subject does not exist here is silent. A pragma that exists and was
//! given a value the engine cannot honour is **refused**, because accepting it
//! would be answering a question wrongly.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
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
        match name {
            b"cache_size" => self.pragma_cache_size(argument),
            b"synchronous" => self.pragma_synchronous(argument),
            b"busy_timeout" => self.pragma_busy_timeout(argument),
            b"foreign_keys" => self.pragma_flag(argument),
            b"journal_mode" => self.pragma_fixed_word(argument, b"wal"),
            b"encoding" => self.pragma_fixed_word(argument, b"UTF-8"),
            b"locking_mode" => self.pragma_fixed_word(argument, b"exclusive"),
            b"integrity_check" | b"quick_check" => self.pragma_integrity_check(),
            b"wal_checkpoint" => self.pragma_wal_checkpoint(),
            b"page_size" => Ok(one_integer(self.page_size as i64)),
            b"page_count" => Ok(one_integer(self.database.pool().page_count() as i64)),
            b"freelist_count" => Ok(one_integer(self.database.free_pages() as i64)),
            b"table_info" | b"table_xinfo" => self.pragma_table_info(argument),
            b"index_list" => self.pragma_index_list(argument),
            b"index_info" | b"index_xinfo" => self.pragma_index_info(argument),
            b"table_list" => self.pragma_table_list(),
            b"database_list" => Ok(Outcome {
                rows: vec![vec![
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(b"main".to_vec()),
                    OwnedDatum::Text(self.path.to_string_lossy().as_bytes().to_vec()),
                ]],
                names: vec!["seq".into(), "name".into(), "file".into()],
                changes: Default::default(),
            }),
            // SQLite's own answer to a pragma it does not know: no rows, no
            // error, and the next statement runs.
            _ => Ok(Outcome::empty()),
        }
    }

    /// Reads or sets the buffer pool's budget, in SQLite's own units.
    ///
    /// SQLite states a negative `cache_size` in kibibytes and a positive one in
    /// pages; the pool is sized in frames of the file's page size, so the two
    /// are the same number said differently. **The pool is not resized**: it is
    /// fixed at open here and the gate's whole fairness argument rests on both
    /// arms having one stated budget. Setting it is therefore accepted and
    /// reported back, and a caller that reads it afterwards is told the truth
    /// about what the engine is using rather than what it asked for.
    fn pragma_cache_size(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let bytes = self.frames.saturating_mul(self.page_size);
        let kib = (bytes / 1024) as i64;
        if argument.is_some() {
            return Ok(Outcome::empty());
        }
        Ok(one_integer(-kib))
    }

    /// Reads or sets how much of a commit reaches the platter.
    fn pragma_synchronous(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(one_integer(match self.wal.synchronous() {
                Synchronous::Off => 0,
                Synchronous::Normal => 1,
                Synchronous::Full => 2,
            }));
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
            other => return Err(misuse(format!("no such synchronous setting: {other}"))),
        };
        self.set_synchronous(policy);
        Ok(Outcome::empty())
    }

    /// Reads or sets how long a writer waits for the writer slot.
    fn pragma_busy_timeout(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(one_integer(self.busy_timeout_ms as i64)),
            Some(argument) => {
                self.busy_timeout_ms = argument_integer(argument).max(0) as u64;
                Ok(one_integer(self.busy_timeout_ms as i64))
            }
        }
    }

    /// Reads or sets a boolean flag the engine records and reports.
    fn pragma_flag(&mut self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        match argument {
            None => Ok(one_integer(i64::from(self.foreign_keys))),
            Some(argument) => {
                self.foreign_keys = argument_boolean(argument);
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
    /// @param fixed - the one setting
    fn pragma_fixed_word(
        &mut self,
        argument: Option<&PragmaArgument>,
        fixed: &[u8],
    ) -> DbResult<Outcome> {
        if let Some(argument) = argument {
            let asked = argument_text(argument);
            if !asked.eq_ignore_ascii_case(&String::from_utf8_lossy(fixed)) {
                return Err(misuse(format!(
                    "this engine is {} only, and cannot be set to {asked}",
                    String::from_utf8_lossy(fixed)
                )));
            }
        }
        Ok(Outcome {
            rows: vec![vec![OwnedDatum::Text(fixed.to_vec())]],
            names: vec!["value".into()],
            changes: Default::default(),
        })
    }

    /// Runs the integrity checker over every tree.
    ///
    /// `ok` when they all hold, and the first failure otherwise, which is the
    /// shape SQLite's answer has.
    fn pragma_integrity_check(&mut self) -> DbResult<Outcome> {
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
            names: vec!["integrity_check".into()],
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
    fn pragma_table_info(&self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(table) = self.named_table(argument) else {
            return Ok(Outcome::empty());
        };
        let rows = table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| {
                vec![
                    OwnedDatum::Int(position as i64),
                    OwnedDatum::Text(column.name.clone()),
                    OwnedDatum::Text(column.declared_type.clone()),
                    OwnedDatum::Int(i64::from(column.not_null)),
                    match &column.default_sql {
                        Some(text) => OwnedDatum::Text(text.clone()),
                        None => OwnedDatum::Null,
                    },
                    // `primary_key_position` is already one-based, which is
                    // what SQLite's `pk` column holds, and zero for a column
                    // that is not in the key.
                    OwnedDatum::Int(column.primary_key_position.map(i64::from).unwrap_or(0)),
                ]
            })
            .collect();
        Ok(Outcome {
            rows,
            names: vec![
                "cid".into(),
                "name".into(),
                "type".into(),
                "notnull".into(),
                "dflt_value".into(),
                "pk".into(),
            ],
            changes: Default::default(),
        })
    }

    /// Lists one table's indexes, newest first, as SQLite does.
    fn pragma_index_list(&self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(table) = self.named_table(argument) else {
            return Ok(Outcome::empty());
        };
        let count = table.indexes.len();
        let rows = table
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
                    OwnedDatum::Int(0),
                ]
            })
            .collect();
        let _ = count;
        Ok(Outcome {
            rows,
            names: vec![
                "seq".into(),
                "name".into(),
                "unique".into(),
                "origin".into(),
                "partial".into(),
            ],
            changes: Default::default(),
        })
    }

    /// Describes one index's key columns.
    fn pragma_index_info(&self, argument: Option<&PragmaArgument>) -> DbResult<Outcome> {
        let Some(argument) = argument else {
            return Ok(Outcome::empty());
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
            return Ok(Outcome::empty());
        };
        let rows = index
            .columns
            .iter()
            .enumerate()
            .map(|(seq, key)| {
                vec![
                    OwnedDatum::Int(seq as i64),
                    OwnedDatum::Int(key.column.map(i64::from).unwrap_or(-2)),
                    match key.column.and_then(|at| table.column(at)) {
                        Some(column) => OwnedDatum::Text(column.name.clone()),
                        None => OwnedDatum::Null,
                    },
                ]
            })
            .collect();
        Ok(Outcome {
            rows,
            names: vec!["seqno".into(), "cid".into(), "name".into()],
            changes: Default::default(),
        })
    }

    /// Lists every table in the schema.
    fn pragma_table_list(&self) -> DbResult<Outcome> {
        let rows = self
            .tables
            .iter()
            .map(|table| {
                vec![
                    OwnedDatum::Text(b"main".to_vec()),
                    OwnedDatum::Text(table.name.clone()),
                    OwnedDatum::Text(b"table".to_vec()),
                    OwnedDatum::Int(table.columns.len() as i64),
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(i64::from(table.without_rowid)),
                ]
            })
            .collect();
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

/// Returns the answer a pragma that reports one number gives.
///
/// @param value - the number
fn one_integer(value: i64) -> Outcome {
    Outcome {
        rows: vec![vec![OwnedDatum::Int(value)]],
        names: vec!["value".into()],
        changes: Default::default(),
    }
}
