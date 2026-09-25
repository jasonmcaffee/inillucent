//! The `nightly` tier of `inillucent-compat`'s integration tests.
//!
//! What the tier is for: the long forms, run on a schedule rather than on a change.
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
//! with `name = "nightly"` and `module = "<file>"` in
//! `tests/selection.toml`. `selection::discover` finds a module file with no
//! row, so `no_test_hides_outside_the_map` names one that was forgotten.

mod release_format_history;
mod story_large_table_nightly;
