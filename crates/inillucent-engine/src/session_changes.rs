//! The counters each connection reports: `changes()`, `total_changes()` and
//! `last_insert_rowid()`.
//!
//! Invariant: **a connection reads only its own counters.** SQLite keeps all
//! three on the connection, so a statement on one connection never moves
//! another connection's numbers. Every connection a `Database` hands out
//! shares one `ImportedDatabase`, and its `Counters` used to hold one copy of
//! each number: session A's `changes()` read 1 and its `total_changes()` 4
//! after session B inserted one row, where SQLite answers 3 and 3. An
//! application with a pool of connections read another thread's counters.
//!
//! The running session's numbers stay in the three cells on `Counters`, which
//! every write already updates and every scalar already reads, so a
//! connection that is the only one pays nothing. [`SessionChanges`] holds the
//! numbers of every session that is not running, and `use_session` swaps them
//! when the running session changes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// One connection's three counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionCounters {
    /// What `last_insert_rowid()` answers.
    pub(crate) last_rowid: i64,
    /// What `changes()` answers.
    pub(crate) last_changes: i64,
    /// What `total_changes()` answers.
    pub(crate) total_changes: i64,
}

/// The counters of every session that is not the running one.
#[derive(Default)]
pub(crate) struct SessionChanges {
    /// The session whose numbers are in the cells on `Counters`.
    running: Cell<u64>,
    /// Every other session's numbers, by session.
    parked: RefCell<HashMap<u64, SessionCounters>>,
}

impl SessionChanges {
    /// Returns the session whose numbers are in the live cells.
    pub(crate) fn running(&self) -> u64 {
        self.running.get()
    }

    /// Makes `session` the running one.
    ///
    /// Returns the numbers to load into the live cells: `session`'s own, or
    /// zeros for a session that has run nothing. `live` is what the cells
    /// hold now, which belongs to the session that was running and is kept
    /// for it.
    ///
    /// @param session - the session about to run
    /// @param live - the live cells' values, the outgoing session's numbers
    pub(crate) fn switch_to(&self, session: u64, live: SessionCounters) -> SessionCounters {
        let outgoing = self.running.replace(session);
        let mut parked = self.parked.borrow_mut();
        parked.insert(outgoing, live);
        parked.remove(&session).unwrap_or_default()
    }

    /// Returns one session's numbers without making it the running one.
    ///
    /// @param session - the session asking
    /// @param live - the live cells' values, which are the running session's
    pub(crate) fn read(&self, session: u64, live: SessionCounters) -> SessionCounters {
        if session == self.running.get() {
            return live;
        }
        self.parked
            .borrow()
            .get(&session)
            .copied()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each session gets back what it left, and a new one starts at zero.
    #[test]
    fn a_session_keeps_its_own_counters_across_another_sessions_work() {
        let sessions = SessionChanges::default();
        let a = SessionCounters {
            last_rowid: 3,
            last_changes: 3,
            total_changes: 3,
        };
        assert_eq!(sessions.switch_to(2, a), SessionCounters::default());
        let b = SessionCounters {
            last_rowid: 4,
            last_changes: 1,
            total_changes: 1,
        };
        assert_eq!(sessions.read(0, b), a);
        assert_eq!(sessions.read(2, b), b);
        assert_eq!(sessions.switch_to(0, b), a);
        assert_eq!(sessions.read(2, a), b);
    }
}
