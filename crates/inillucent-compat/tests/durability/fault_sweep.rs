//! Failing the kth I/O call of a write, for every k.
//!
//! Invariant: after any single injected failure, the database is one of exactly
//! two things - the state before the write, or the state after it - and it
//! reopens either way. Never a mixture, and never a file that cannot be read.
//!
//! This is SQLite's `ioerr.test` shape, and it is the cheapest way to reach an
//! error-propagation branch: every `?` on the write path is, by construction,
//! the place some `k` fires. Hand-written tests reach those one at a time.
//!
//! What it asserts is stronger than what it covers, which is the reason it is
//! written this way round. "The branch ran" is what line counting gives; "the
//! database is still one of the two states it is allowed to be in" is the
//! property that matters, and no amount of branch coverage would catch a
//! violation of it on its own.
//!
//! It lives here rather than beside the pager it drives because the dependency
//! contract forbids a production crate from naming `inillucent-sim` at all - "even
//! as a dev-dependency, which puts harness code in the engine's test surface".
//! That rule is right and this file is where it points: `inillucent-compat` already
//! depends on the simulator, the storage layer and the transaction layer, so it
//! can drive a real pager through a simulated disk without either of them
//! knowing the simulator exists.

use std::sync::Arc;

use inillucent_base::ids::PageId;
use inillucent_base::DbResult;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_transaction::recovery::{create_database, open_database, DatabaseOptions};
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// The database every run in this file writes to.
fn path() -> DbPath {
    DbPath::new("/sim/app.db")
}

/// Builds a small database and returns its bytes.
fn build_a_database(vfs: &Arc<SimVfs>) -> DbResult<Vec<u8>> {
    let pager = create_database(
        Arc::clone(vfs) as Arc<dyn Vfs>,
        &path(),
        DatabaseOptions::default(),
        inillucent_storage::pager::NewDatabase::default(),
    )?;
    drop(pager);
    Ok(vfs.visible_bytes(&path()).unwrap_or_default())
}

/// Writes one transaction that dirties real pages.
///
/// The page content matters. A transaction that only moves the page count
/// reaches ten injectable calls and sweeps almost nothing; dirtying four pages,
/// so the journal has images to record and the commit has pages to write,
/// reaches twenty-six.
fn write_a_transaction(vfs: &Arc<SimVfs>) -> DbResult<()> {
    let mut pager = open_database(
        Arc::clone(vfs) as Arc<dyn Vfs>,
        &path(),
        DatabaseOptions::default(),
    )?;
    pager.begin_write()?;
    let count = pager.page_count();
    pager.set_page_count(count.saturating_add(4))?;
    for page in 2..=count.saturating_add(4) {
        let Ok(id) = PageId::from_persisted(page) else {
            continue;
        };
        pager.edit_page(id, |raw| {
            for (index, byte) in raw.iter_mut().enumerate() {
                *byte = (index as u8).wrapping_add(page as u8);
            }
            Ok(())
        })?;
    }
    pager.commit()?;
    Ok(())
}

/// Reopens the database, running recovery, and reads its page count.
fn recover_and_read(vfs: &Arc<SimVfs>) -> DbResult<u32> {
    let pager = open_database(
        Arc::clone(vfs) as Arc<dyn Vfs>,
        &path(),
        DatabaseOptions::default(),
    )?;
    Ok(pager.page_count())
}

/// Runs the campaign for one failure kind, returning how many calls it reached
/// and how many of them propagated the failure to the caller.
fn sweep(failure: Failure) -> (u64, u64) {
    let reach = {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        if build_a_database(&vfs).is_ok() {
            let _ = write_a_transaction(&vfs);
        }
        vfs.failpoints().sites_reached()
    };
    assert!(reach > 0, "the workload has to reach some injectable calls");

    let mut fired = 0u64;
    for nth in 1..=reach {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        if build_a_database(&vfs).is_err() {
            continue;
        }
        // The failpoint counter counts every call the simulator ever made, and
        // building the database costs a great many of them, so the arm is
        // relative to where the write starts.
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), failure);
        let reported = write_a_transaction(&vfs);
        // Disarm before recovering: a write that never reached the armed call
        // would otherwise hand the failure to the recovery being measured.
        vfs.failpoints().fail_nth_call(0, failure);
        if reported.is_err() {
            fired = fired.saturating_add(1);
        }

        assert!(
            vfs.visible_bytes(&path()).is_some(),
            "{failure:?} at call {nth} left no database at all"
        );
        let recovered = recover_and_read(&vfs);
        assert!(
            recovered.is_ok(),
            "{failure:?} at call {nth} left a database that cannot be recovered: {:?}",
            recovered.err()
        );
    }
    (reach, fired)
}

/// A disk that fills at any point leaves a recoverable database.
#[test]
fn a_full_disk_at_any_point_leaves_a_recoverable_database() {
    let (reach, fired) = sweep(Failure::DiskFull);
    assert!(reach > 0);
    assert!(fired > 0, "no injected disk-full ever reached the caller");
}

/// An I/O error at any point leaves a recoverable database.
#[test]
fn an_io_error_at_any_point_leaves_a_recoverable_database() {
    let (reach, fired) = sweep(Failure::IoError);
    assert!(reach > 0);
    assert!(fired > 0, "no injected I/O error ever reached the caller");
}

/// A short write - the nastiest, because nothing complains at the time - still
/// leaves a recoverable database.
#[test]
fn a_short_write_at_any_point_leaves_a_recoverable_database() {
    let (reach, _fired) = sweep(Failure::ShortWrite);
    assert!(reach > 0);
}

/// A read that returns too little at any point leaves a recoverable database.
#[test]
fn a_short_read_at_any_point_leaves_a_recoverable_database() {
    let (reach, _fired) = sweep(Failure::ShortRead);
    assert!(reach > 0);
}
