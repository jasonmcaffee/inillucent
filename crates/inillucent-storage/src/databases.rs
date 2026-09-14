//! The databases one statement can reach.
//!
//! Invariant: a statement addresses a database by number, and the number means
//! the same thing everywhere - in the catalog the statement was bound against,
//! in the instruction that opens a cursor, and in whatever holds the pagers.
//! `main` is always zero. Everything else is where the connection put it, and
//! the connection is the only thing allowed to decide.
//!
//! This is a trait rather than a container because storage has no business
//! knowing what a database is *called*. A name is schema, the session owns it,
//! and what storage needs is the one question a running statement asks: which
//! pager does database three mean.

// **`MAIN_DATABASE` and the `PagerSet` trait are gone (task-1946, M1).** The
// trait was the retired engine's way of addressing an attached database by
// number; the shipping engine does that through `inillucent-engine`'s own
// schema list, and nothing outside this file ever named either of them - not
// the trait, not its two implementations, not the constant. What is left is the
// one constant `inillucent-catalog` reads.

/// The number the connection's temporary database always has.
///
/// It is fixed rather than assigned, and it is one for the same reason SQLite
/// makes it one: a statement bound before an `ATTACH` carries the numbers it
/// resolved against, and a temporary database that moved when something was
/// attached would move underneath them. Nothing is attached at one; the
/// attached databases start at two.
pub const TEMP_DATABASE: usize = 1;
