//! Savepoints: a named point inside a transaction, and what hears about it.
//!
//! Invariant: **the undo log and the connected modules are told the same thing
//! about the same point.** A savepoint is a mark in the undo log and a number a
//! virtual table module records against its own staging area, and a
//! `ROLLBACK TO` has to mean one thing to both - so the level a module is given
//! when a point opens is the level it is given when one is abandoned.
//!
//! These three methods are here rather than in `lib.rs` because they are one
//! idea and `lib.rs` is nearly eight thousand lines. task-1932 changed all
//! three - a module now hears about `savepoint` and `release`, where before it
//! heard about neither - which is what made the seam worth taking.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;

use crate::ImportedDatabase;

impl ImportedDatabase {
    /// Marks a point inside the open transaction.
    ///
    /// **Every connected module flushes first.** The undo log records writes to
    /// the shadow *trees*, so anything a module is still holding in memory is
    /// invisible to it - and a later `ROLLBACK TO` would either keep staged
    /// rows belonging to the abandoned part, or throw away rows written before
    /// the point. Flushing now puts everything before the point under the undo
    /// log, so the buffer that is left belongs entirely to the part that may be
    /// abandoned.
    ///
    /// A savepoint is rare and a flush is not free, which is the right way
    /// round: the alternative is a module buffer the undo log cannot see.
    ///
    /// @param name - the savepoint's name
    pub fn savepoint(&mut self, name: &[u8]) -> DbResult<()> {
        self.sync_modules()?;
        // **Told after the flush and before the mark (task-1932, M2).** The
        // level a module is given is how many savepoints were already open,
        // which is the same number `rollback_to` later hands it: a module
        // numbers its own marks by what it was told, so the two have to agree.
        let level = i32::try_from(self.writing.marks().borrow().len()).unwrap_or(i32::MAX);
        self.savepoint_modules(level)?;
        let held = self.writing.undo().borrow().len();
        self.writing
            .marks()
            .borrow_mut()
            .push((name.to_ascii_lowercase(), held));
        Ok(())
    }

    /// Undoes back to a savepoint, keeping the transaction open.
    ///
    /// @param name - the savepoint's name
    pub fn rollback_to(&mut self, name: &[u8]) -> DbResult<()> {
        // **The level of the savepoint being returned to, not how deep the
        // nesting currently is.** `SAVEPOINT a; SAVEPOINT b; ROLLBACK TO a`
        // has two marks and a target level of zero, and a module told "two"
        // would keep the state belonging to `b` - the savepoint that was just
        // abandoned. A module numbers its own marks by what it was given, so
        // the number has to mean the same thing to both sides.
        //
        // A name the transaction does not hold is left to `undo_to` to refuse,
        // so that the error is the one it has always been.
        let folded = name.to_ascii_lowercase();
        // The borrow ends with the statement: every line below this one calls
        // back into the database, and the group is reachable from there.
        let found = self
            .writing
            .marks()
            .borrow()
            .iter()
            .rposition(|(held, _)| *held == folded);
        let Some(position) = found else {
            // **A name no savepoint holds changes nothing, modules included.**
            // Defaulting the level to zero and telling the modules anyway made
            // `ROLLBACK TO a_name_that_is_not_open` discard a buffered virtual
            // table's pending writes and *then* report the error - a failed
            // statement with a side effect, which is the one thing a failed
            // statement may not have. `undo_to` refuses it below with the
            // message it has always used.
            self.undo_to(Some(name))?;
            self.refresh_catalog();
            return Ok(());
        };
        let level = i32::try_from(position).unwrap_or(i32::MAX);
        let told = self.rollback_modules(Some(level));
        let undone = self.undo_to(Some(name));
        self.refresh_catalog();
        undone?;
        told?;
        Ok(())
    }

    /// Forgets a savepoint without undoing anything.
    ///
    /// @param name - the savepoint's name
    pub fn release(&mut self, name: &[u8]) -> DbResult<()> {
        let folded = name.to_ascii_lowercase();
        // The borrow ends with the statement: every line below this one calls
        // back into the database, and the group is reachable from there.
        let found = self
            .writing
            .marks()
            .borrow()
            .iter()
            .rposition(|(held, _)| *held == folded);
        let Some(position) = found else {
            return Err(refusal(format!(
                "no such savepoint: {}",
                String::from_utf8_lossy(name)
            )));
        };
        // **Before the marks are truncated, so the level means what it meant
        // when the savepoint was opened (task-1932, M2).** A module was never
        // told about a release at all, so one that had staged anything under a
        // savepoint had no moment at which to fold it into the transaction.
        let level = i32::try_from(position).unwrap_or(i32::MAX);
        let told = self.release_modules(level);
        self.writing.marks().borrow_mut().truncate(position);
        told
    }
}
