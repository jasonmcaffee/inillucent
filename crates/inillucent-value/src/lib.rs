//! First-party values, affinities, collations, expression primitives, and
//! record codecs.
//!
//! Invariant: a value never carries a storage class its bytes do not justify,
//! and every conversion is explicit and fallible.
//!
//! This crate is the half of SQLite that has no I/O in it and every subtlety.
//! The five storage classes, the affinity rules that read a declared type as a
//! string of characters rather than as a name, `CAST` and its deliberate
//! disagreements with affinity, the exact comparison of an integer against a
//! double, the three built-in collations, and the record format that a page
//! stores a row in - all of it decided here, once, so that nothing above has
//! to make the same decision a second time and make it differently.
//!
//! Module map, in the order a value moves through them:
//!
//! - [`encoding`] - UTF-8 and the two UTF-16 forms, and conversion between;
//! - [`value`] - the value model itself, borrowed or owned;
//! - [`numeric`] - SQLite's own text-to-number scanners and number-to-text;
//! - [`fpdecode`] - SQLite's conversion of a double to decimal digits, and
//!   the real `printf` conversions built on it;
//! - [`affinity`] - classifying a declared type, and applying the result;
//! - [`cast`] - `CAST`, which is a command rather than a preference;
//! - [`collation`] - BINARY, NOCASE, RTRIM, and the named registry;
//! - [`compare`] - ordering, three-valued logic, and comparison affinity;
//! - [`record`] - serial types, lazy record decode, and key comparison.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics and wrapping out of paths that read persistent bytes.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod affinity;
pub mod cast;
pub mod collation;
pub mod compare;
pub mod encoding;
pub mod fpdecode;
pub mod numeric;
pub mod record;
pub mod value;
pub mod vector;

pub use affinity::Affinity;
pub use collation::{Collation, CollationRegistry};
pub use compare::{SqlOrdering, Truth};
pub use encoding::TextEncoding;
pub use record::{KeyColumn, KeyInfo, RecordRef, SerialType};
pub use value::{BlobValue, Bytes, MemValue, StorageClass, TextValue, Value, ValueFlags};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 2: values, affinities, collations, and records";
