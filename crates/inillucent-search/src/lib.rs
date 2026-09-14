//! The existing BM25 / HNSW / hybrid retrieval engine, given a transactional
//! home and a SQL front door.
//!
//! Invariant: search state commits and rolls back with the transaction that
//! produced it. That is achieved by construction rather than by protocol -
//! every durable byte of a search index is an ordinary row in an ordinary
//! b-tree inside the same database file, so the pager's own transaction is the
//! search index's transaction and there is no second durability decision to get
//! wrong. `crates/inillucent-search/src/store.rs` is where that decision lives.
//!
//! ## What this is not
//!
//! It is not FTS5 and does not impersonate it. FTS5 is a *parity* feature: it
//! exists so that a database SQLite wrote can be read, its answers are compared
//! against the pinned release, and it is graded on matching them. This module
//! is graded on being the engine this repository already had - the measured
//! fusion, the coverage exponent, the proximity and phrase weighting, the
//! adaptive vector weight - and every one of those settings is left at the
//! value `inillucent-core` ships, because the quality scorecard that justifies them
//! was produced with them.
//!
//! ## The three declarations
//!
//! A retrieval engine that will not say what it is doing cannot be checked, so
//! `inillucent_search` declares, in the table's own `%_config` rows:
//!
//! - **exact or approximate.** `mode = 'exact'` compares every candidate and
//!   returns the true top k. `mode = 'approximate'` traverses the graph. The
//!   default is exact, because a caller who did not choose should get the
//!   answer that is correct by construction.
//! - **the distance.** Cosine, and a `CREATE` naming another one is refused
//!   rather than accepted and ignored.
//! - **the tokenizer.** One name, `porter`, matching the one tokenizer this
//!   build has - so a table records which analysis produced its terms.
//!
//! ## What the planner may use it for
//!
//! The search access path answers a query that names a query text, a query
//! vector, or both. It cannot enforce SQL uniqueness, drive a foreign key, or
//! satisfy an `ORDER BY` over anything but its own `rank`, and it does not
//! claim to: those need complete enumeration and an approximate path has none.
//! `k` and `recall` are the module's own controls and are separate from SQL
//! `LIMIT` on purpose - `LIMIT` trims a result, `k` decides how deep the
//! retrieval went, and conflating them would silently change recall whenever
//! somebody paginated.

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

pub mod adapter;
#[cfg(feature = "embed")]
pub mod embed;
// Not behind the feature, and deliberately: these are the sentences `embed`
// refuses with, and a module a default build does not compile is a module whose
// tests a default build does not run. See its own comment for what that cost.
pub mod embed_refusal;
pub mod merge;
pub mod module;
pub mod options;
pub mod store;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 13: transactional inillucent search and legacy migration";

/// Puts the search module into a registry.
///
/// Called once per connection, beside the other first-party modules. It is a
/// function rather than an entry in `Registry::with_builtins` because the
/// registry lives a layer below this crate: a virtual-table module that
/// depended on the retrieval engine would drag a vector index into every
/// database that only wanted SQL.
/// @param registry - the connection's registry
pub fn register(registry: &mut inillucent_ext::registry::Registry) {
    registry.register_module(std::sync::Arc::new(module::SearchModule));
    // **And `embed`**, when this build has it. It belongs here for the same
    // reason the module does - this crate is the one that links the retrieval
    // engine, and the SQL engine below it may not - and it is behind a feature
    // for the reason the retrieval engine's own `onnx` is: the model is a
    // native runtime, and a database that linked one whether or not anybody
    // asked would be paying for it on every open. See `embed`.
    #[cfg(feature = "embed")]
    embed::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module is reachable by the name a `CREATE VIRTUAL TABLE` writes.
    #[test]
    fn registering_makes_the_module_findable() {
        let mut registry = inillucent_ext::registry::Registry::default();
        register(&mut registry);
        assert!(registry.module(b"inillucent_search").is_some());
        assert!(registry.module(b"INILLUCENT_SEARCH").is_some());
    }
}
