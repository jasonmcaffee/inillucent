//! An executable model of what the engine is supposed to do, and the traces
//! that drive it.
//!
//! Invariant: **this crate cannot call the engine.** Its only dependency is the
//! error type. A reference implementation that linked the thing it is grading
//! could share a bug with it, and the two agreeing would then be evidence of
//! nothing - so the engine arrives as a dev-dependency of the *driver*, in
//! `tests/`, and the model has no way to reach it even by accident.
//!
//! ## What a model is for, and what it is not for
//!
//! It is not a second engine. It is the smallest thing that can say what the
//! right answer is: a `BTreeMap` per tree, a copy of it per open transaction,
//! and a list of the commits that have happened. It is slow, it holds
//! everything in memory, and it is written to be read rather than to run - so
//! that when it and the engine disagree, the question "which one is wrong" has
//! an answer somebody can reach by reading forty lines.
//!
//! ## Durability is a *prefix* property, not an equality
//!
//! This is the part a model of a database has to get right and the part it is
//! easiest to get wrong. Under `synchronous = FULL` a committed transaction is
//! on the media when `commit` returns, so a crash loses nothing. Under `NORMAL`
//! or `OFF` it may not be, so a crash may lose the most recent commits - and
//! that is not a bug, it is what the setting means.
//!
//! So the model does not say "after a crash the state is X". It says: the state
//! is `apply(commits[..n])` for **some** `n` at least as large as the number of
//! commits known durable and no larger than the number of commits made. That is
//! the honest statement, it is what [`Model::states_after_crash`] returns, and
//! asserting anything stronger would be asserting something the engine never
//! promised. Asserting anything *weaker* - "some subset of the commits
//! survived" - would let a torn commit through, which is the failure this whole
//! phase exists to rule out.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
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

pub mod model;
pub mod trace;

pub use model::{Model, State, Violation};
pub use trace::{Generator, Op, Trace};
