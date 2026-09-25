//! The `durability` tier of `inillucent-compat`'s integration tests.
//!
//! What the tier is for: crashes, injected faults, corruption and concurrency.
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
//! with `name = "durability"` and `module = "<file>"` in
//! `tests/selection.toml`. `selection::discover` finds a module file with no
//! row, so `no_test_hides_outside_the_map` names one that was forgotten.

mod btree_model;
mod bulk_build_crash;
mod busy_timeout;
mod concurrency;
mod corruption;
mod durability;
mod fault_shapes;
mod fault_sweep;
mod faults;
mod fold_protocol;
mod free_map_checkpoint_crash;
mod multi_database_crash;
mod new_engine_free_map_recovery;
mod new_engine_recovery_shapes;
mod overflow_crash;
mod phase2_campaigns;
mod process_campaign;
mod process_concurrency;
mod process_crash;
mod process_readers;
mod reindex_crash;
mod search_crash;
mod storage_failure;
mod story_nikaya_crash;
mod vacuum_crash;
mod wal_crash;
