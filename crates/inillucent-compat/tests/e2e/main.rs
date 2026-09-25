//! The `e2e` tier of `inillucent-compat`'s integration tests.
//!
//! What the tier is for: the public surfaces an application binds to, end to end.
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
//! with `name = "e2e"` and `module = "<file>"` in
//! `tests/selection.toml`. `selection::discover` finds a module file with no
//! row, so `no_test_hides_outside_the_map` names one that was forgotten.

mod budgets;
mod cli_arguments;
mod cli_batch;
mod cli_commands;
mod confinement;
mod dot_commands;
mod mcp_cancel;
mod mcp_replay;
mod mcp_session;
mod mcp_wire;
mod migrate_realistic;
mod release_format;
mod setup_embeddings;
