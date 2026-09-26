//! The SQL statement matrix: every statement form, in every context an
//! application puts it in, on files, graded against the pinned SQLite 3.53.4.
//!
//! Invariant: **no case is compared in an order its query did not ask for, no
//! error is compared by its wording, and no case passes without both engines
//! having run it on a real file.** The first two are the pitfalls every
//! project that does this reports (section 2 of the design); the third is why
//! the corpus this replaces never reopened anything.
//!
//! The design is `tasks/task-2135-sql-statement-matrix-tdd.md`. The layers:
//!
//! - **Layer 1**, hand written construct cases in
//!   `tests/corpora/matrix/<family>/*.slt`, read by [`case`].
//! - **Layer 2**, interaction cases generated from covering arrays built by
//!   [`cover`] over the axes each template in [`templates`] declares.
//! - **Layer 3**, every generated `SELECT` in the wrappings that keep its
//!   meaning, and the [`properties`] that need no oracle.
//! - **Layer 4**, random statements for the nightly run, from [`random`], with
//!   failures reduced by [`shrink`] and kept in the retained corpus.
//!
//! [`run`] grades one case; [`group`] decides which cases a test runs and
//! asserts once at the end; [`known`] reads the lists of cases allowed to
//! disagree; [`surfaces`] runs cases through the other ways an application
//! reaches the engine; [`inventory`] proves every AST variant, function,
//! pragma, module, collation, syntax production and capability row has a case.

pub mod bind;
pub mod case;
pub mod convert;
pub mod cover;
pub mod grade;
pub mod group;
pub mod inventory;
pub mod known;
pub mod limited;
pub mod properties;
pub mod random;
pub mod run;
pub mod shrink;
pub mod surfaces;
pub mod templates;
pub mod union;
