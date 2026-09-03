//! Bytecode representation, compiler, verifier, virtual machine, cursors, and
//! relational operators.
//!
//! Invariant: nothing runs that the verifier has not accepted. The compiler
//! produces a program, the verifier proves that every jump lands inside it,
//! every register is written before it is read and every cursor is used as the
//! kind it was opened, and only then does the machine execute it. That order is
//! the whole safety argument: the machine has no bounds check of its own to
//! forget, because the program was proved in range before it started.
//!
//! Module map, in the order a statement moves through them:
//!
//! - [`program`] - instructions, operands, and the compiled program;
//! - [`compile`] - a physical plan in, a program out;
//! - [`verify`] - the independent check the machine relies on;
//! - [`eval`] - SQLite's arithmetic, comparison and three-valued logic;
//! - [`builtin`] - the scalar functions;
//! - [`pattern`] - `LIKE` and `GLOB`;
//! - [`aggregate`] - the accumulators;
//! - [`sorter`] - the sorter and the distinct set;
//! - [`machine`] - registers, cursors, and the step loop.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics out of paths that run compiled programs.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

/// Asserts two values are bit-identical.
///
/// `Value` deliberately has no `PartialEq`: SQL equality is a three-valued
/// question with an affinity and a collation attached, and a derived `==` would
/// be the wrong answer wearing the right operator. Tests that want "these are
/// the same bits" ask for exactly that.
#[cfg(test)]
macro_rules! assert_same {
    ($left:expr, $right:expr $(,)?) => {{
        let left = $left;
        let right = $right;
        assert!(left.identical(&right), "{:?} is not {:?}", left, right);
    }};
}

#[cfg(test)]
pub(crate) use assert_same;

pub mod aggregate;
pub mod builtin;
pub mod compile;
pub mod eval;
pub mod machine;
pub mod pattern;
pub mod program;
pub mod sorter;
pub mod verify;

pub use compile::{compile, compile_select};
pub use machine::{Machine, MachineState, StepOutcome};
pub use program::{Instruction, Opcode, Program, ProgramDependencies, ResultColumn};
pub use verify::{verify, verify_operands, VerifyError};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 6: catalog, binder, expression VM, and read-only SELECT";
