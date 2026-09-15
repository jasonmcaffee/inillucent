//! Session-scoped `total_changes()` accounting.
//!
//! Invariant: **`total_changes()` answers for one connection's session, never
//! for every session an `ImportedDatabase` has ever handed out.**
//! `ImportedDatabase::changed_ever` is one counter shared across every
//! session that database ever hands out, because a session number only ever
//! scoped `temp` - so without this a fresh connection reported every row a
//! *different* connection had written before it existed: a setup script run
//! on its own throw-away connection, followed by a test's own connection
//! changing three rows, read `total_changes()` as six.
//! [`SessionChanges::record_open`] records what `changed_ever` read the
//! instant each session opened, and [`SessionChanges::total_changes`]
//! subtracts that back off - so `changed_ever` itself never has to become
//! per-session and lose the one counter every write already updates.
//!
//! Came out of `crates/inillucent-engine/src/lib.rs`, which was at its
//! recorded ceiling when this fix landed: the field, the write in
//! `open_session`, and the read in `total_changes` are one idea, *what this
//! session started from*, and none of it is reached from anywhere else in
//! the crate.

use std::cell::RefCell;
use std::collections::HashMap;

/// Every open session's own baseline against the shared `changed_ever`
/// counter.
#[derive(Default)]
pub(crate) struct SessionChanges {
    baseline: RefCell<HashMap<u64, i64>>,
}

impl SessionChanges {
    /// Records a session's baseline the first time that session runs anything.
    ///
    /// **The second and later calls do nothing**, because the baseline is what
    /// `changed_ever` stood at when the connection opened, and a connection
    /// that has already run a statement has moved it. Recording it on first use
    /// rather than when the number was handed out is what lets
    /// `Database::session` open a connection without borrowing the engine at
    /// all - see `connect.rs` and task-1962 A11.
    ///
    /// @param session - the connection
    /// @param changed_ever - the shared counter's current value
    pub(crate) fn record_open_once(&self, session: u64, changed_ever: i64) {
        self.baseline
            .borrow_mut()
            .entry(session)
            .or_insert(changed_ever);
    }

    /// Returns how many rows `session` alone has changed, given
    /// `changed_ever`'s current value.
    ///
    /// @param session - the connection asking
    /// @param changed_ever - the shared counter's current value
    pub(crate) fn total_changes(&self, session: u64, changed_ever: i64) -> i64 {
        let opened_at = self.baseline.borrow().get(&session).copied().unwrap_or(0);
        changed_ever.saturating_sub(opened_at)
    }
}
