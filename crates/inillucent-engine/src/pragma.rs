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
//! `SQLITE_PRAGMAS` is that list, and anything on it this engine has no
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

use std::collections::BTreeMap;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_sql::declare::argument_text;
use inillucent_sql::directive::PragmaArgument;
use inillucent_tree::datum::OwnedDatum;

use super::{ImportedDatabase, Outcome, MAIN};

// **The three modules this file is made of (task-1962, A1 step 1).** It was
// 1,930 lines and 85 pragma names in one `impl` block. What is left here is
// the name-to-handler table, which is the thing a reader looking for one
// pragma actually wants, and the three subjects it dispatches to.
mod integrity;
mod schema;
mod tuning;

impl ImportedDatabase {
    /// Runs one `PRAGMA`.
    ///
    /// **A qualifier names which attached database the pragma is about**
    /// (task-2066 section 4.2, item 25). It used to be discarded in `ddl.rs`,
    /// so every pragma that is about a file answered about `main`: after
    /// `ATTACH ':memory:' AS aux`, `PRAGMA aux.user_version = 7` wrote main's
    /// four bytes and `PRAGMA main.user_version` read 7 back, where SQLite
    /// answers 7 and 0.
    ///
    /// The pragmas it changes are the ones that are *about a file* -
    /// `user_version`, `application_id`, `schema_version`, `page_count`,
    /// `freelist_count`, and the schema pragmas that list a table's columns
    /// and indexes. The rest are about the connection - `cache_size`,
    /// `busy_timeout`, `foreign_keys` and their kind - and SQLite ignores a
    /// qualifier on those as well.
    ///
    /// @param name - the pragma's folded name
    /// @param argument - the value it was given, when it was given one
    /// @param at - the attached database it was qualified with, `None` for
    ///             the unqualified form, which means `main`
    pub(super) fn pragma(
        &mut self,
        name: &[u8],
        argument: Option<&PragmaArgument>,
        at: Option<usize>,
    ) -> DbResult<Outcome> {
        // **The read-only pragmas first, and through the same function the
        // table-valued form uses.** `PRAGMA table_info(t)` and `SELECT * FROM
        // pragma_table_info('t')` are the same question; if they were two
        // implementations they would eventually be two answers, and the one
        // nobody tested would be the wrong one.
        if let Some(outcome) = self.pragma_rows_of(name, argument, at)? {
            return Ok(outcome);
        }
        match name {
            // `default_cache_size` is the deprecated spelling of the same
            // setting, and SQLite still answers it.
            b"cache_size" | b"default_cache_size" => self.pragma_cache_size(argument),
            b"synchronous" => self.pragma_synchronous(argument),
            b"busy_timeout" => self.pragma_busy_timeout(argument),
            b"foreign_keys" => self.pragma_flag(argument),
            b"trusted_schema" => self.pragma_trusted_schema(argument),
            b"defer_foreign_keys" => self.pragma_defer(argument),
            b"foreign_key_check" => self.pragma_foreign_key_check(argument),
            b"journal_mode" => self.pragma_journal_mode(argument),
            b"encoding" => self.pragma_fixed_word(argument, "encoding", b"UTF-8"),
            b"locking_mode" => self.pragma_locking_mode(argument),
            b"integrity_check" => self.pragma_integrity_check(
                "integrity_check",
                crate::engine::integrity::CheckDepth::Full,
            ),
            b"quick_check" => self
                .pragma_integrity_check("quick_check", crate::engine::integrity::CheckDepth::Quick),
            b"wal_checkpoint" => self.pragma_wal_checkpoint(),
            b"page_size" => Ok(named_integer("page_size", self.storage.page_size as i64)),
            b"page_count" => Ok(named_integer(
                "page_count",
                self.file_of(at)?.pool().page_count() as i64,
            )),
            b"freelist_count" => Ok(named_integer(
                "freelist_count",
                self.file_of(at)?.free_pages() as i64,
            )),
            b"user_version" => self.pragma_user_version(argument, at),
            b"application_id" => self.pragma_application_id(argument, at),
            b"schema_version" => Ok(named_integer(
                "schema_version",
                i64::from(self.file_of(at)?.schema_cookie()),
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
    /// Returns the file a qualified pragma is about.
    ///
    /// `None` means the unqualified form, which is `main` - the same rule the
    /// binder uses everywhere else a name may carry a schema.
    ///
    /// @param at - the attached database the pragma named
    fn file_of(&self, at: Option<usize>) -> DbResult<&inillucent_pool::Database> {
        self.schema_file(at.unwrap_or(MAIN))
            .ok_or_else(|| refusal("that pragma names a database that is not attached"))
    }

    /// Returns the file a qualified pragma is about, to write into.
    ///
    /// @param at - the attached database the pragma named
    fn file_of_mut(&mut self, at: Option<usize>) -> DbResult<&mut inillucent_pool::Database> {
        let at = at.unwrap_or(MAIN);
        if at == MAIN {
            return Ok(&mut self.storage.database);
        }
        self.session_state
            .schema_at_mut(at)
            .map(|held| &mut held.database)
            .ok_or_else(|| refusal("that pragma names a database that is not attached"))
    }

    pub(super) fn pragma_rows(
        &self,
        name: &[u8],
        argument: Option<&PragmaArgument>,
    ) -> DbResult<Option<Outcome>> {
        self.pragma_rows_of(name, argument, None)
    }

    /// The same, for a caller that knows which attached database was named.
    ///
    /// @param name - the pragma's folded name
    /// @param argument - the value it was given, when it was given one
    /// @param at - the attached database it was qualified with, `None` for
    ///             every database, which is the unqualified form's answer
    pub(super) fn pragma_rows_of(
        &self,
        name: &[u8],
        argument: Option<&PragmaArgument>,
        at: Option<usize>,
    ) -> DbResult<Option<Outcome>> {
        Ok(Some(match name {
            b"foreign_key_list" => self.pragma_foreign_key_list(argument, at)?,
            b"table_info" => self.pragma_table_info(argument, false, at)?,
            b"table_xinfo" => self.pragma_table_info(argument, true, at)?,
            b"index_list" => self.pragma_index_list(argument, at)?,
            b"index_info" => self.pragma_index_info(argument, false, at)?,
            b"index_xinfo" => self.pragma_index_info(argument, true, at)?,
            b"table_list" => self.pragma_table_list(argument, at)?,
            b"collation_list" => self.pragma_collation_list(),
            b"pragma_list" => list_of("name", &listed_pragmas()),
            b"module_list" => {
                let mut names = self.session_state.registry.module_names();
                names.extend(ENGINE_MODULES.iter().map(|name| (*name).to_string()));
                names.sort();
                names.dedup();
                list_of("name", &names)
            }
            b"function_list" => pragma_function_list(&self.session_state.registry),
            b"compile_options" => list_of("compile_options", COMPILE_OPTIONS),
            b"database_list" => Outcome {
                rows: self.database_list(),
                names: std::rc::Rc::new(vec!["seq".into(), "name".into(), "file".into()]),
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
        names: std::rc::Rc::new(vec![name.into()]),
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
        names: std::rc::Rc::new(vec![name.into()]),
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
        names: std::rc::Rc::new(vec![column.to_string()]),
        changes: Default::default(),
    }
}

/// The pragma names SQLite compiles only on Windows.
///
/// **`data_store_directory` is `SQLITE_OS_WIN`-only, and this engine listed it
/// everywhere (task-1946, M5).** SQLite's `pragma.h` builds its table from the
/// same `#if` the VFS is chosen by, so a Linux build's `pragma_list` is one name
/// shorter than a Windows build's. Nothing here could see that until the first
/// Linux run that had an oracle to compare against - the oracle stage had failed
/// on every Linux run there had ever been - and
/// `registers.rs::the_pragma_register_agrees_exactly` then reported the two
/// lists differing by exactly this name.
///
/// It stays in `SQLITE_PRAGMAS` rather than being cut, because the list is also
/// what decides whether an unanswered name is refused or ignored, and that
/// decision is the same on both platforms: `PRAGMA data_store_directory` is a
/// pragma either way, and a Linux caller asking for it gets SQLite's own silence
/// rather than an error.
const WINDOWS_ONLY_PRAGMAS: [&str; 1] = ["data_store_directory"];

/// Returns the names `pragma_list` reports on this platform.
///
/// @returns every name in `SQLITE_PRAGMAS` that this platform's SQLite has
fn listed_pragmas() -> Vec<&'static str> {
    SQLITE_PRAGMAS
        .iter()
        .copied()
        .filter(|name| cfg!(windows) || !WINDOWS_ONLY_PRAGMAS.contains(name))
        .collect()
}

/// Every pragma name SQLite 3.53.4's own `pragma_list` reports.
///
/// **The list is the contract.** A name here that this engine does not answer
/// is refused by name rather than silently accepted - 38 of these used to
/// answer nothing at all. It is also what
/// `pragma_pragma_list` reports, so a tool can ask this engine what it knows
/// about - and get the same set of names either engine would give, with the
/// difference showing up as a refusal rather than as an absence.
///
/// One name on it is platform-dependent; `listed_pragmas` is what
/// `pragma_list` reports, and `WINDOWS_ONLY_PRAGMAS` says which.
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

/// Lists the functions this connection answers, in SQLite's own columns.
///
/// The built-ins come from `inillucent_sql::function::every_function`, which is
/// the same list the obligations report and the old engine's `function_list`
/// read - so a built-in that exists is reported by every route or by none.
/// `enc` is `utf8` because that is the only text encoding here.
///
/// **And then what this connection has registered, which is the half that was
/// missing.** `PRAGMA function_list` read only the static list, so every
/// function reached through `inillucent_ext`'s registry answered when it was
/// called and was never named: `create_scalar_function` and
/// `create_aggregate_function` are the surface an application defines its own
/// through, and `embed(TEXT)` is one this repository ships. On a 0.1.2 binary
/// with the `embed` feature, `SELECT length(embed('hello'))` returned 3072 and
/// `inillucent functions embed` listed nothing.
///
/// That is the exact shape this register was audited for once already, and
/// `crates/inillucent-compat/tests/registers.rs` says why it is worse than an
/// error: *"a register that under-reports answers every call correctly - it
/// just does not admit that it can"*. `PRAGMA module_list` two arms above has
/// merged the registry's modules in all along; this is the same merge for
/// functions.
///
/// A registration that shadows a built-in - same name, same arity - **replaces**
/// its row rather than adding a second one, because the registration is what a
/// call will actually reach, and its flags are the ones that describe it. It
/// keeps the built-in's place in the listing so the order does not move.
///
/// @param registry - what this connection reaches functions through
fn pragma_function_list(registry: &inillucent_ext::registry::Registry) -> Outcome {
    let mut rows: Vec<Vec<OwnedDatum>> = Vec::new();
    let mut placed: BTreeMap<(String, i64), usize> = BTreeMap::new();
    for entry in inillucent_sql::function::every_function() {
        placed.insert((entry.name.to_ascii_lowercase(), entry.arity), rows.len());
        rows.push(function_row(
            entry.name,
            true,
            entry.kind,
            entry.arity,
            entry.flags,
        ));
    }
    for held in registry.functions() {
        let name = held.name.to_ascii_lowercase();
        let arity = i64::from(held.arity);
        let kind = match held.is_aggregate() {
            true => "a",
            false => "s",
        };
        let row = function_row(&name, false, kind, arity, registered_flags(held.flags));
        // Every index in `placed` was `rows.len()` when it was recorded and
        // nothing shortens `rows`, so the lookup always finds its row. The
        // crate denies `indexing_slicing`, and a `get_mut` that says so costs
        // nothing here.
        match placed.get(&(name.clone(), arity)).copied() {
            Some(at) => {
                if let Some(built_in) = rows.get_mut(at) {
                    *built_in = row;
                }
            }
            None => {
                placed.insert((name, arity), rows.len());
                rows.push(row);
            }
        }
    }
    Outcome {
        rows,
        names: std::rc::Rc::new(vec![
            "name".into(),
            "builtin".into(),
            "type".into(),
            "enc".into(),
            "narg".into(),
            "flags".into(),
        ]),
        changes: Default::default(),
    }
}

/// Builds one `PRAGMA function_list` row.
///
/// @param name - the name SQL calls it by
/// @param builtin - whether it is one of the engine's own
/// @param kind - `s` for a scalar, `a` for an aggregate, `w` for a window
/// @param arity - how many arguments, or -1 for any number
/// @param flags - the flag word the C surface reports
fn function_row(name: &str, builtin: bool, kind: &str, arity: i64, flags: i64) -> Vec<OwnedDatum> {
    vec![
        OwnedDatum::Text(name.as_bytes().to_vec()),
        OwnedDatum::Int(i64::from(builtin)),
        OwnedDatum::Text(kind.as_bytes().to_vec()),
        OwnedDatum::Text(b"utf8".to_vec()),
        OwnedDatum::Int(arity),
        OwnedDatum::Int(flags),
    ]
}

/// Returns the flag word `function_list` reports for a registered function.
///
/// The same two bits a built-in is described with, so one column means one
/// thing. `direct_only` has no bit in this column and is not reported: what it
/// governs is whether a *schema* may name the function rather than what the
/// function is, and SQLite's own `function_list` does not report it either.
///
/// It is what `FunctionFlags::external()` sets, which is the constructor anything
/// registered from outside should use - not what the `Default` derive gives,
/// which is every flag false (task-1969, 7.4). This sentence said "the default"
/// and `inillucent-search`'s `embed` took the derive at its word.
///
/// @param flags - what the registration promised about itself
fn registered_flags(flags: inillucent_ext::registry::FunctionFlags) -> i64 {
    let mut word = 0;
    if flags.innocuous {
        word |= inillucent_sql::function::INNOCUOUS_FLAG;
    }
    if flags.deterministic {
        word |= inillucent_sql::function::DETERMINISTIC_FLAG;
    }
    word
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
        // The log is folded in at an explicit checkpoint rather than every N
        // frames, so there is no frame count to set.
        b"wal_autocheckpoint" => (0, OFF),
        _ => return None,
    })
}
