//! Built-in JSON, virtual tables, extension registries, and the loadable
//! extension policy.
//!
//! Invariant: an extension observes engine state only through the registered
//! contracts it was handed. A module is given the roots of its own shadow
//! tables and a pager to read them through; it never resolves a name, never
//! opens a transaction of its own, and never sees a table it was not told
//! about. That is what makes "hostile virtual table" a bounded problem rather
//! than an open one: the worst a broken module can do is answer wrongly about
//! its own rows.
//!
//! The crate has three parts. `json` is pure - values in, values out, no
//! database - and implements the JSON built-ins and the binary format they
//! share. `vtab` is the virtual-table contract: what a module is asked, in what
//! order, and what it may answer. `registry` is what a connection holds: the
//! modules, collations and virtual file systems that have been registered, and
//! the policy flags that decide which of them a schema is allowed to name.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

/// The JSON built-ins, which now live in `inillucent-scalar`.
///
/// **Moved rather than copied.** The TDD's Phase 4 puts the JSON functions on
/// the new executor's expression path, and `inillucent-exec` sits *below* this
/// crate - it never resolves a name, so it cannot depend on the crate that
/// registers modules. The same move `inillucent-scalar` was created for in Phase 2
/// applies unchanged here: one implementation of `json_extract`, in a crate both
/// executors depend on, rather than two that agree until the next fix.
///
/// Every path a caller wrote against `inillucent_ext::json` still resolves.
pub use inillucent_scalar::json;
pub mod registry;
pub mod shadow;
pub mod vtab;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 11: full built-ins, PRAGMAs, virtual tables, FTS5, and R-Tree";
