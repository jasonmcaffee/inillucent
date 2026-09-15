//! `ImportedDatabase`, in the parts its methods already cluster into.
//!
//! Invariant: **nothing here is a new decision.** Every module below holds a run
//! of methods that were adjacent in one `impl` block of 3,773 lines, moved
//! whole, with no signature changed and no behaviour altered - task-1962's A1
//! step 1, whose whole point is to be a diff a reviewer can read before the two
//! steps that follow it are not.
//!
//! **Why the type is still one type.** Splitting the *file* and splitting the
//! *type* are different changes with different risks. The file split is
//! mechanical and reviewable on its own; the type split - `Database`, `Session`
//! and `Writer`, which is what turns one `RefCell` over the whole engine into
//! three - touches every call site in four crates. Doing the second without the
//! first would have produced one diff nobody could read.
//!
//! **Why a child module can see the fields.** `ImportedDatabase`'s fields are
//! private to the crate root, and privacy in Rust is a module tree rather than
//! a file: a descendant of the module an item is declared in sees it. `ddl.rs`,
//! `pragma.rs` and `vtab.rs` have relied on that since they were written.
//!
//! | module | what it holds |
//! |---|---|
//! | [`open`] | importing a database, creating one, opening and recovering one |
//! | [`accessors`] | everything a caller can ask without running a statement |
//! | [`statements`] | planning a statement, preparing one, running one |
//! | [`counters`] | the counters a connection reports, and the random seed |
//! | [`batch`] | the transaction manager: begin, undo, rollback, commit, vote |
//! | [`integrity`] | walking every tree and every index |
//! | [`functions`] | the function registry, the collations, the authorizer, the levers |
//! | [`compiled`] | the write path: compiling a statement and applying what it decided |
//! | [`explain`] | rendering `EXPLAIN` and `EXPLAIN QUERY PLAN` as rows |
//! | [`rowshape`] | what a table's rows look like on disk, and reading a schema back |
//! | [`state`] | the six groups `ImportedDatabase`'s fields are made of |
//! | [`keys`] | settling the foreign keys a statement left outstanding |
//! | [`locks`] | taking the file lock, and choosing the journal |
//! | [`write`] | where a statement's writes go, and what undoes them |
//! | [`tables`] | questions asked about one table's declaration |

pub mod accessors;
pub mod batch;
pub mod compiled;
pub mod counters;
pub mod explain;
pub mod functions;
pub mod integrity;
pub mod keys;
pub mod locks;
pub mod open;
pub mod rowshape;
pub mod state;
pub mod statements;
pub mod tables;
pub mod write;
