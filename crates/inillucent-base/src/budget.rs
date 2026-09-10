//! What one request is allowed to cost, and the one place it is counted.
//!
//! Invariant: **a request that exceeds its budget stops, and the failure names
//! which budget it was.** An MCP server hands a database to an agent, and an
//! agent asks for what it asks for: `SELECT * FROM chunk` against six hundred
//! thousand rows is a reasonable-looking call that materialises gigabytes, and
//! nothing stopped it before this budget existed. `limit_of` in the command
//! surface even turned a negative row limit into zero, which means *unlimited*.
//!
//! ## Why it is a thread-local rather than a parameter
//!
//! The alternative was threading a `&Budget` through every operator, the tree
//! cursors, the sort and the vector search - about forty signatures, most of
//! which would carry it only to hand it on. That is the shape of change that
//! gets a `None` passed at one call site during a later refactor and quietly
//! stops enforcing anything.
//!
//! One connection runs on one thread (`drivers/inillucent-driver-capi`'s header
//! says so, and the engine's own buffer pool assumes it), so a thread-local is
//! exactly the scope a request occupies. [`Guard`] arms it for the length of a
//! call and restores whatever was there before, so a nested call - a virtual
//! table running a query of its own - cannot widen its caller's budget.
//!
//! ## Why cancellation is not a thread-local
//!
//! The flag is an [`Arc<AtomicBool>`] the caller owns, because the whole point
//! of a cancel is that it arrives from somewhere else: another thread, a signal
//! handler, an MCP client's second connection. Everything *else* here is
//! per-request and per-thread; the flag is the one part that is shared, and it
//! is shared deliberately.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{DbError, PrimaryCode};
use crate::DbResult;

/// What ran out.
///
/// Reported by name rather than as one "too big" so that a caller can act on
/// it: a row limit is answered by asking for fewer rows, a deadline by asking
/// for something cheaper, and a cancellation by nothing at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exceeded {
    /// More rows were produced than the request was allowed.
    Rows,
    /// More bytes were produced than the request was allowed.
    Bytes,
    /// The request ran longer than it was allowed.
    Time,
    /// Somebody asked for the request to stop.
    Cancelled,
}

impl Exceeded {
    /// Returns the word a structured error carries.
    ///
    /// Stable, because a client branches on it. It is the *budget's* name and
    /// not a sentence, so a translation or a retry rule can key on it.
    pub fn name(self) -> &'static str {
        match self {
            Exceeded::Rows => "rows",
            Exceeded::Bytes => "bytes",
            Exceeded::Time => "time",
            Exceeded::Cancelled => "cancelled",
        }
    }
}

/// What one request may spend.
#[derive(Clone, Debug)]
pub struct Limits {
    /// The most rows an operator may produce, or `None` for no bound.
    pub rows: Option<u64>,
    /// The most bytes of row data it may produce, or `None` for no bound.
    pub bytes: Option<u64>,
    /// How long it may run, or `None` for no bound.
    pub time: Option<Duration>,
}

impl Limits {
    /// Returns limits that bound nothing, which is what a library embedding
    /// the engine gets unless it asks otherwise.
    ///
    /// **Unbounded is the right default for an embedded database and the wrong
    /// one for a served database**, which is why this exists beside
    /// [`Limits::served`] rather than instead of it. An application that has
    /// linked the engine into its own process is not protecting itself from
    /// itself; a server handing an agent a tool is.
    pub fn unbounded() -> Limits {
        Limits {
            rows: None,
            bytes: None,
            time: None,
        }
    }

    /// Returns the budget a served request gets when nobody said otherwise.
    ///
    /// The numbers are chosen to be generous for a question and stingy for a
    /// mistake. Ten million rows is more than any answer an agent reads and far
    /// less than a table scan of a corpus; 256 MiB is the same ceiling
    /// `inillucent-remote` puts on a single protocol message, for the same
    /// reason; sixty seconds is longer than every query in the differential
    /// suite and shorter than a client's patience.
    pub fn served() -> Limits {
        Limits {
            rows: Some(10_000_000),
            bytes: Some(256 * 1024 * 1024),
            time: Some(Duration::from_secs(60)),
        }
    }

    /// Returns these limits with a different deadline.
    ///
    /// @param time - how long a request may run, or `None` for no bound
    pub fn with_time(mut self, time: Option<Duration>) -> Limits {
        self.time = time;
        self
    }

    /// Returns these limits with a different row bound.
    ///
    /// @param rows - the most rows a request may produce
    pub fn with_rows(mut self, rows: Option<u64>) -> Limits {
        self.rows = rows;
        self
    }

    /// Returns these limits with a different byte bound.
    ///
    /// @param bytes - the most bytes a request may produce
    pub fn with_bytes(mut self, bytes: Option<u64>) -> Limits {
        self.bytes = bytes;
        self
    }
}

impl Default for Limits {
    /// Returns unbounded limits.
    fn default() -> Limits {
        Limits::unbounded()
    }
}

/// One request's budget: what it may spend, and what it has spent.
#[derive(Debug)]
struct Spending {
    /// What it may spend.
    limits: Limits,
    /// When it must stop, computed once from `limits.time`.
    deadline: Option<Instant>,
    /// The flag a cancel from another thread sets.
    cancel: Arc<AtomicBool>,
    /// Rows produced so far.
    rows: u64,
    /// Bytes of row data produced so far.
    bytes: u64,
}

thread_local! {
    /// The budget this thread's request is running under, if any.
    static ACTIVE: RefCell<Option<Spending>> = const { RefCell::new(None) };
}

/// Arms a budget for the length of its lifetime.
///
/// Dropping it restores whatever budget was in force before, which is what
/// makes a nested call - a virtual table's own query, a trigger's statement -
/// run inside its caller's budget rather than beside it.
#[derive(Debug)]
pub struct Guard {
    /// What was armed before, restored on drop.
    previous: Option<Spending>,
}

impl Drop for Guard {
    /// Restores the budget that was in force before this one.
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|held| {
            *held.borrow_mut() = previous;
        });
    }
}

/// Arms a budget on this thread until the returned guard is dropped.
///
/// @param limits - what the request may spend
/// @param cancel - the flag another thread sets to stop it
pub fn arm(limits: Limits, cancel: Arc<AtomicBool>) -> Guard {
    // A cancel that arrived before the request started belongs to the request
    // that has finished, not to this one. Clearing it here is what makes the
    // flag safe to reuse across calls on one connection.
    cancel.store(false, Ordering::Relaxed);
    let spending = Spending {
        // `checked_add` because the crate denies wrapping arithmetic and an
        // `Instant` plus a caller's `Duration` is a caller's number. A window
        // so large it overflows the clock is the same thing as no deadline.
        deadline: limits
            .time
            .and_then(|window| Instant::now().checked_add(window)),
        limits,
        cancel,
        rows: 0,
        bytes: 0,
    };
    let previous = ACTIVE.with(|held| held.borrow_mut().replace(spending));
    Guard { previous }
}

/// Reports whether a budget is armed on this thread.
pub fn armed() -> bool {
    ACTIVE.with(|held| held.borrow().is_some())
}

/// Refuses when the request has been cancelled or has run out of time.
///
/// **Cheap enough to call in a loop**, which is the property that decides where
/// it can go: an atomic load and, when there is a deadline, one `Instant::now`.
/// Callers that run a tight inner loop check every batch rather than every row.
pub fn check() -> DbResult<()> {
    ACTIVE.with(|held| {
        let Some(spending) = held
            .borrow()
            .as_ref()
            .map(|spending| (spending.cancel.load(Ordering::Relaxed), spending.deadline))
        else {
            return Ok(());
        };
        let (cancelled, deadline) = spending;
        if cancelled {
            return Err(exceeded(Exceeded::Cancelled, 0, 0));
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(exceeded(Exceeded::Time, 0, 0));
            }
        }
        Ok(())
    })
}

/// Counts rows and their bytes against the budget, refusing when either runs out.
///
/// @param rows - how many rows were produced
/// @param bytes - roughly how many bytes they hold
pub fn spend(rows: u64, bytes: u64) -> DbResult<()> {
    ACTIVE.with(|held| {
        let mut borrowed = held.borrow_mut();
        let Some(spending) = borrowed.as_mut() else {
            return Ok(());
        };
        spending.rows = spending.rows.saturating_add(rows);
        spending.bytes = spending.bytes.saturating_add(bytes);
        if let Some(most) = spending.limits.rows {
            if spending.rows > most {
                return Err(exceeded(Exceeded::Rows, spending.rows, most));
            }
        }
        if let Some(most) = spending.limits.bytes {
            if spending.bytes > most {
                return Err(exceeded(Exceeded::Bytes, spending.bytes, most));
            }
        }
        Ok(())
    })
}

/// Returns what the armed request has spent so far, as `(rows, bytes)`.
///
/// Zero when nothing is armed, which is the same answer a request that has
/// produced nothing gives - and the caller that asks is reporting rather than
/// deciding.
pub fn spent() -> (u64, u64) {
    ACTIVE.with(|held| {
        held.borrow()
            .as_ref()
            .map(|spending| (spending.rows, spending.bytes))
            .unwrap_or((0, 0))
    })
}

/// Builds the failure a spent budget produces.
///
/// **`Interrupt`, and the same code for all four.** A caller distinguishes them
/// by the sentence, which names the budget; what they share is the property a
/// caller has to branch on first, which is that the connection is still usable
/// and the statement did not run. A row limit reported as `TooBig` and a
/// deadline reported as `Interrupt` would make that one question two.
///
/// @param what - which budget ran out
/// @param used - what the request had spent
/// @param most - what it was allowed
fn exceeded(what: Exceeded, used: u64, most: u64) -> DbError {
    let said = match what {
        Exceeded::Cancelled => "this request was cancelled.".to_string(),
        Exceeded::Time => "this request ran past the time it was allowed.".to_string(),
        Exceeded::Rows => format!(
            "this request produced {used} rows, past the {most} it was allowed. Ask for fewer \
             with a WHERE clause or a LIMIT."
        ),
        Exceeded::Bytes => format!(
            "this request produced {used} bytes of row data, past the {most} it was allowed. Ask \
             for fewer columns or fewer rows."
        ),
    };
    DbError::primary(PrimaryCode::Interrupt)
        .with_message(said.clone())
        // The budget's name, machine-readable, so a client can retry a
        // deadline and not retry a cancellation.
        .with_detail(format!("budget={} {said}", what.name()))
}

/// Returns which budget a failure names, when it is a budget failure.
///
/// The reader for the tag `exceeded` writes. It reads the detail rather than
/// the message because the message is prose a person sees and may be reworded.
///
/// @param error - the failure to classify
pub fn exceeded_kind(error: &DbError) -> Option<Exceeded> {
    let detail = error.detail()?;
    let rest = detail.strip_prefix("budget=")?;
    let name = rest.split_whitespace().next()?;
    match name {
        "rows" => Some(Exceeded::Rows),
        "bytes" => Some(Exceeded::Bytes),
        "time" => Some(Exceeded::Time),
        "cancelled" => Some(Exceeded::Cancelled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With nothing armed, nothing is spent and nothing is refused: the engine
    /// is not a sandbox unless somebody asked for one.
    #[test]
    fn an_unarmed_thread_spends_nothing() {
        assert!(!armed());
        assert!(check().is_ok());
        assert!(spend(u64::MAX, u64::MAX).is_ok());
        assert_eq!(spent(), (0, 0));
    }

    /// A row budget refuses once it is past, and names itself.
    #[test]
    fn a_row_budget_refuses_and_says_which_one() {
        let _guard = arm(
            Limits::unbounded().with_rows(Some(10)),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(spend(9, 0).is_ok());
        let refused = spend(2, 0).expect_err("the eleventh row is past ten");
        assert_eq!(exceeded_kind(&refused), Some(Exceeded::Rows));
        assert!(
            refused.message().contains("11 rows"),
            "{}",
            refused.message()
        );
    }

    /// A byte budget refuses independently of the row budget.
    #[test]
    fn a_byte_budget_refuses_on_its_own() {
        let _guard = arm(
            Limits::unbounded().with_bytes(Some(100)),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(spend(1_000_000, 99).is_ok());
        let refused = spend(0, 2).expect_err("101 bytes is past 100");
        assert_eq!(exceeded_kind(&refused), Some(Exceeded::Bytes));
    }

    /// A deadline that has passed refuses.
    #[test]
    fn a_passed_deadline_refuses() {
        let _guard = arm(
            Limits::unbounded().with_time(Some(Duration::from_millis(0))),
            Arc::new(AtomicBool::new(false)),
        );
        std::thread::sleep(Duration::from_millis(2));
        let refused = check().expect_err("the deadline has passed");
        assert_eq!(exceeded_kind(&refused), Some(Exceeded::Time));
    }

    /// A cancel set from another thread stops the request.
    #[test]
    fn a_cancel_from_another_thread_stops_it() {
        let flag = Arc::new(AtomicBool::new(false));
        let _guard = arm(Limits::unbounded(), Arc::clone(&flag));
        assert!(check().is_ok());
        let other = Arc::clone(&flag);
        std::thread::spawn(move || other.store(true, Ordering::Relaxed))
            .join()
            .expect("the setter runs");
        let refused = check().expect_err("a cancelled request stops");
        assert_eq!(exceeded_kind(&refused), Some(Exceeded::Cancelled));
    }

    /// A cancel left over from the previous request does not stop this one.
    #[test]
    fn a_stale_cancel_does_not_stop_the_next_request() {
        let flag = Arc::new(AtomicBool::new(true));
        let _guard = arm(Limits::unbounded(), Arc::clone(&flag));
        assert!(check().is_ok(), "arming clears a flag from the last call");
    }

    /// A nested budget cannot widen the one it is inside.
    ///
    /// The property that makes this safe to arm around a whole statement: a
    /// virtual table that ran a query of its own would otherwise be able to
    /// hand itself an unbounded one.
    #[test]
    fn a_nested_budget_is_restored_when_it_ends() {
        let outer = arm(
            Limits::unbounded().with_rows(Some(5)),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(spend(4, 0).is_ok());
        {
            let _inner = arm(Limits::unbounded(), Arc::new(AtomicBool::new(false)));
            assert!(spend(1_000, 0).is_ok(), "the inner budget is its own");
        }
        let refused = spend(2, 0).expect_err("the outer budget still counts its own four rows");
        assert_eq!(exceeded_kind(&refused), Some(Exceeded::Rows));
        drop(outer);
        assert!(!armed());
    }

    /// A failure that is not a budget failure is not read as one.
    #[test]
    fn an_ordinary_failure_names_no_budget() {
        assert_eq!(
            exceeded_kind(&crate::error::refusal("something else")),
            None
        );
    }
}
