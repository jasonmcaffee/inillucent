//! The `engine` tier of `inillucent-compat`'s integration tests.
//!
//! What the tier is for: SQL and storage behaviour over real database files.
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
//! with `name = "engine"` and `module = "<file>"` in
//! `tests/selection.toml`. `selection::discover` finds a module file with no
//! row, so `no_test_hides_outside_the_map` names one that was forgotten.

mod analyze_reopen;
mod analyze_same_session;
mod autoindex_reopen;
mod budget;
mod compiled_chain_reuse;
mod conformance;
mod delete_order;
mod dml;
mod embed_direct_only;
mod format_refusal;
mod fts5_legacy_layout;
mod functions;
mod hostile;
mod levers;
mod limited_writes;
mod module_stages;
mod multi_database_commit;
mod multi_database_names;
mod multi_database_participants;
mod new_engine_connect;
mod new_engine_create;
mod new_engine_explain;
mod new_engine_extent_packing;
mod new_engine_log_lead;
mod new_engine_log_retire;
mod new_engine_page_ownership;
mod new_engine_rollback;
mod new_engine_search;
mod new_engine_slt;
mod new_engine_statement;
mod new_engine_subquery;
mod new_engine_surface;
mod new_engine_tree_identity;
mod new_engine_vtab_stream;
mod page_sizes;
mod plan_cache;
mod pqs_differential;
mod prepared_schema;
mod put_stages;
mod readonly_pragmas;
mod reentrant_connection;
mod reindex_without_rowid;
mod schema_function_policy;
mod search;
mod search_recall;
mod segment_delta_chain;
mod segment_merge_bound;
mod segmented_generations;
mod sql;
mod storage;
mod subquery_values;
mod tlp_differential;
mod torn_page_with_image;
mod vacuum_on_vfs;
mod vector;
mod vector_metric;
mod vtab;
mod vtab_lifecycle;
mod wal;
