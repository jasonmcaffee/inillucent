//! The `differential` tier of `inillucent-compat`'s integration tests.
//!
//! What the tier is for: graded against the pinned SQLite 3.53.4.
//!
//! Invariant: **this file declares one module per suite and nothing else, and
//! each module is one target in `tests/selection.toml`, run by
//! `inillucent-testrun` in a process of its own.** The suites used to be one
//! integration test binary each, 149 of them, and every one linked the same
//! compat library and 23 crates. One binary per tier links them once. The runner
//! lists this binary's tests, groups them by module, and starts the binary once
//! per module with `--exact` and that module's names, so a suite still has its
//! own process, its own timing row and its own kill budget.
//!
//! A new suite is a file in this directory, a `mod` line here, and a row
//! with `name = "differential"` and `module = "<file>"` in
//! `tests/selection.toml`. `selection::discover` finds a module file with no
//! row, so `no_test_hides_outside_the_map` names one that was forgotten.

mod advanced_sql;
mod attach;
mod catalog;
mod cli;
mod differential;
mod differential_part8;
mod dml_differential;
mod dml_subqueries;
mod foreign_keys;
mod fts5;
mod fts5_parity;
mod json;
mod lifecycle;
mod migrate_sqlite;
mod new_engine_ddl;
mod new_engine_differential;
mod new_engine_extents;
mod new_engine_pragma;
mod new_engine_vtab;
mod new_engine_writes;
mod numeric_text;
mod oracle;
mod ordering;
mod planner;
mod pragma;
mod registers;
mod result_names_and_codes;
mod rtree;
mod schema_forms;
mod semantics;
mod services;
mod syntax;
mod temp_objects;
mod trigger_depth;
