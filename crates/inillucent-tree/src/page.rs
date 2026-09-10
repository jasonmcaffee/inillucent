//! The page primitives, re-exported from where they now live.
//!
//! Invariant: there is exactly one definition of the page header in the
//! workspace. Phase 1 put it here, because `inillucent-tree` was the lowest crate
//! that knew what a page was. Phase 2 introduced `inillucent-pool` *below* the
//! tree, because a page header is a property of the file format rather than of
//! the B+tree and the pool has to read one before it knows whether the page is
//! a leaf at all, so the module moved down and this is what is left of it.
//!
//! Nothing else changed: `inillucent_tree::page::PageId`, `write_common`,
//! `read_u32` and the rest are the same items at the same paths, so every
//! caller written against Phase 1 still compiles. The alternative - a second
//! copy of the header offsets in the tree - is the specific mistake this
//! re-export exists to refuse.

pub use inillucent_pool::page::*;
