//! The math functions, re-exported from where they now live.
//!
//! Invariant: there is one implementation of each of these in the workspace.
//! They moved down into `inillucent-scalar` in Phase 2 of the rearchitecture so
//! that the vectorised executor and this bytecode VM call the same code rather
//! than two copies that agree until the next fix. Every path a caller used
//! before still resolves, and every test that was written against them moved
//! with them.

pub use inillucent_scalar::mathfn::*;
