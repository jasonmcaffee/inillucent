//! The dialect's scalar layer: SQLite's arithmetic, three-valued logic, value
//! coercions, and the whole built-in function set.
//!
//! Invariant: there is one implementation of `substr()` in this workspace, and
//! of `length()`, and of `LIKE`, and of `strftime()`, and of every other
//! built-in. The rearchitecture had two executors in the tree at once - the
//! bytecode virtual machine, until Phase 5 deleted it, and the vectorised
//! executor in `inillucent-exec` that replaced it - and the function bodies
//! were moved here rather than copied into the second one. Copying them would
//! have produced two `substr()`s that agree today and disagree after the next
//! fix.
//!
//! The virtual machine's `eval`, `builtin`, `pattern`, `mathfn`, `printf` and
//! `datetime` modules were re-exports of the modules here for as long as it
//! existed, so the move broke no caller and no test on the way through.
//!
//! ## What is in here and what is not
//!
//! In: everything that is a pure function of values. `eval`'s arithmetic and
//! comparison, the scalar functions, the `LIKE`/`GLOB` matcher, the math
//! functions, `printf`, and the date and time functions.
//!
//! Not in: anything that needs a cursor, a register file, a statement or a
//! connection. Aggregates stay with their executors, because an accumulator is
//! a piece of operator state rather than a function of values - and there are
//! genuinely two of them, the VM's stepping over `Value`s and the vectorised
//! executor's stepping over a column at a time with a compensated float sum.
//!
//! Window *frames* are here even so, in [`window`]. Which rows a frame contains
//! is not operator state: it is a pure function of two row counts and a handful
//! of booleans, and it is where the dialect's hard cases live - `EXCLUDE TIES`
//! differing from `EXCLUDE GROUP` by one row, `CURRENT ROW` meaning the peer
//! group under `RANGE` and the row under `ROWS`, a `GROUPS` offset counting
//! peer groups. Both executors bring their own comparisons and their own
//! accumulators to it and share the arithmetic.
//!
//! ## Why it sits above `inillucent-sql`
//!
//! Because a built-in is identified by `inillucent_sql::function::ScalarFunc`, and
//! the binder is what resolves a name to one. That is the only edge; nothing
//! here parses, binds, plans or reads a page.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
// **These four were named in the `cfg_attr(test, allow(...))` block below and
// denied nowhere (task-1932, H9).** `policy.rs` matched on the lint's *name*,
// which is in that block, so this crate passed the check while allowing every
// one of them - 71 `expect`s, two `unwrap`s, two `panic!`s and 22 direct index
// expressions in `geopoly.rs`, in a crate that evaluates every scalar function
// over values a caller chose.
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

/// Asserts two values are bit-identical.
///
/// `Value` deliberately has no `PartialEq`: SQL equality is a three-valued
/// question with an affinity and a collation attached, and a derived `==` would
/// be the wrong answer wearing the right operator. Tests that want "these are
/// the same bits" ask for exactly that.
///
/// It moved here with the modules whose tests use it.
#[cfg(test)]
macro_rules! assert_same {
    ($left:expr, $right:expr $(,)?) => {{
        let left = $left;
        let right = $right;
        assert!(left.identical(&right), "{:?} is not {:?}", left, right);
    }};
}

pub mod builtin;
pub mod datetime;
pub mod eval;
pub mod geopoly;
pub mod json;
pub mod mathfn;
pub mod pattern;
pub mod printf;
pub mod regexp;
pub mod window;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 2: a real read-only engine on a buffer pool";
