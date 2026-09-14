//! The push-based vectorised executor: batches that borrow pinned pages, a
//! closure-compiled expression layer, and the operators over them.
//!
//! Invariant: nothing in a pipeline copies a value unless an operator's own
//! semantics require it to outlive its input. A scan hands downstream a slice
//! of the leaf; a filter hands on a selection vector rather than moving rows; a
//! projection that permutes columns rebuilds the batch out of the same borrowed
//! vectors. Sorting, hashing and aggregating copy, because they must, and the
//! type system says so: those are the only places `OwnedDatum` appears.
//!
//! This is the rearchitecture design's `inillucent-exec`, which
//! replaces `inillucent-vm`. What Phase 1 builds is the set the four
//! `read.analytical` shapes need - scan, filter, project, simple and grouped
//! aggregation, sort, top-n, distinct, limit - plus the closure compiler.
//! Joins, the point probe, window functions and the vtab protocol are Phase 2.
//!
//! ## Why there is no bytecode
//!
//! The old engine compiled a statement to a program and ran it one instruction
//! per row. Fable 5.1's review of that loop found
//! the cost was not in the dispatch at all - it was in what each instruction
//! did per row: an `Arc` clone for the limits, a `Vec` allocation per text
//! column, a `Value` clone into a register. A vectorised executor removes those
//! by construction rather than by optimising them: there is no register file to
//! clone into, and a column of text is a slice of a page.

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

pub mod aggregate;
pub mod autoindex;
pub mod batch;
pub mod compiled;
pub mod constant;
pub mod correlate;
pub mod declared;
pub mod dml;
pub mod expr;
pub mod insert_plan;
pub mod join;
pub mod lateral;
pub mod ops;
pub mod paged;
pub mod physical;
pub mod recursive;
pub mod scalar;
pub mod scan;
pub mod sequence;
pub mod setop;
pub mod subquery;
pub mod trigger;
pub mod window;
pub mod windowpass;

pub use aggregate::{Accumulator, AggregateKind};
pub use batch::{Batch, Vector, BATCH_ROWS};
pub use expr::{compile, ArithOp, CompareOp, Eval, Expr, StaticType};
pub use join::{
    HashJoin, IndexNestedLoopJoin, JoinKind, Materialize, NestedLoopJoin, RowStore, ValuesScan,
};
pub use ops::{
    AdjacentDistinct, AggregateSpec, Collect, CollectInto, Distinct, Filter, Flow, HashAggregate,
    Limit, Project, SimpleAggregate, Sink, Sort, SortKey, StreamAggregate, TopN,
};
pub use paged::{FullScan, PointProbe, ReverseScan, SkipScan as PagedSkipScan, SpanScan};
pub use physical::{SourceLayout, TreeCatalog};
pub use scan::{Projection, SkipScan, TableScan};
