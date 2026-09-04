//! What the machine needs from the connection beyond the pagers.
//!
//! Invariant: the machine never holds a module. It asks the host for one, uses
//! it inside a call, and gives it back - which is not a style choice but the
//! only shape that compiles: the module reads its shadow tables through the
//! same pagers the machine is holding, so the two cannot be borrowed at once.
//! The host takes the module out of its slot, hands it to the body along with a
//! context over the pagers, and puts it back afterwards, failure included.
//!
//! Splitting the dispatch this way also keeps the cost where it belongs. The
//! ordinary opcodes - the ones every statement runs - take a plain `&mut dyn
//! PagerSet` and know nothing about modules; only the eleven virtual opcodes
//! reach the host at all.

use rustdb_base::{error, DbResult};
use rustdb_ext::vtab::{Context, VirtualCursor, VirtualTable};
use rustdb_storage::{Pager, PagerSet};

use crate::program::VirtualRef;

/// What one virtual-table call answers.
///
/// `update` returns the rowid an insert allocated; every other method returns
/// nothing. One return type keeps the callback object-safe with no generics,
/// which is what a `&mut dyn FnMut` needs.
pub type VirtualAnswer = Option<i64>;

/// The connection, as the machine sees it.
pub trait Host {
    /// Returns the databases the connection has open.
    fn pagers(&mut self) -> &mut dyn PagerSet;

    /// Opens a cursor on one virtual table.
    fn open_virtual(&mut self, reference: &VirtualRef) -> DbResult<Box<dyn VirtualCursor>>;

    /// Runs a body with one virtual table and a context over the pagers.
    ///
    /// The body is a `&mut dyn FnMut` rather than a generic closure so that the
    /// whole trait stays object-safe: the machine holds the host as
    /// `&mut dyn Host` and cannot name a type parameter.
    fn with_virtual(
        &mut self,
        reference: &VirtualRef,
        body: &mut dyn FnMut(&mut dyn VirtualTable, &mut Context<'_>) -> DbResult<VirtualAnswer>,
    ) -> DbResult<VirtualAnswer>;
}

/// A pager on its own is a host with no modules.
///
/// It is what the machine's own tests run against, and what a statement that
/// names no virtual table needs. A statement that does name one gets the error
/// rather than a silent empty scan, because "this build has no modules" and
/// "this table has no rows" are different answers.
impl Host for Pager {
    /// The single database.
    fn pagers(&mut self) -> &mut dyn PagerSet {
        self
    }

    /// Refuses: there is no registry here to look a module up in.
    fn open_virtual(&mut self, reference: &VirtualRef) -> DbResult<Box<dyn VirtualCursor>> {
        Err(no_modules(reference))
    }

    /// Refuses, for the same reason.
    fn with_virtual(
        &mut self,
        reference: &VirtualRef,
        _body: &mut dyn FnMut(&mut dyn VirtualTable, &mut Context<'_>) -> DbResult<VirtualAnswer>,
    ) -> DbResult<VirtualAnswer> {
        Err(no_modules(reference))
    }
}

/// Returns the error a host with no module registry reports.
fn no_modules(reference: &VirtualRef) -> rustdb_base::DbError {
    error::misuse(format!(
        "no such module: {}",
        String::from_utf8_lossy(&reference.module.name)
    ))
}
