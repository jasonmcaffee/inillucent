//! The spill file a buffering operator writes runs to, over the real VFS.
//!
//! Invariant: **a spill file never outlives the statement that made it.** It is
//! opened with `FileKind::Transient`, whose `OpenOptions` carry
//! `delete_on_close`, so the file system removes it when the last handle goes -
//! including when the process is killed part way through a sort, which is the
//! case a named file would leave behind.
//!
//! ## Why this is here and the trait is not
//!
//! `crate::spill::Spill` and `SpillFile` are declared in `inillucent-exec`,
//! where `Sort` lives, and `docs/invariants/layering.toml` does not let that
//! crate depend on `inillucent-vfs`: a query executor has no business naming a
//! file system. This crate may, and already depends on `inillucent-exec`, so
//! implementing the trait here is the direction the contract already allows
//! (task-2066 §4.3.6). No layering row changed for this.
//!
//! ## What it does not do
//!
//! It does not sync, lock, truncate or rename. A run is written once, read
//! back inside the same statement, and thrown away; a durability barrier on it
//! would be a barrier protecting bytes nobody will ever want again.

use std::sync::Arc;

use inillucent_base::{DbError, DbResult, PrimaryCode};
use inillucent_exec::spill::{Spill, SpillFile};
use inillucent_vfs::{FileKind, OpenOptions, Vfs, VfsFile};

/// Opens transient files on one database's file system.
pub(crate) struct VfsSpill {
    /// The file system the database was opened on.
    ///
    /// The database's own rather than a fresh one, for the reason
    /// `ImportedDatabase::vfs` gives about `MemoryVfs`: each memory file system
    /// is its own, so a spill file made on a new one would be invisible to
    /// everything else and a `:memory:` database would spill into a void.
    vfs: Arc<dyn Vfs>,
}

impl VfsSpill {
    /// Returns a factory over one file system.
    ///
    /// @param vfs - the file system the database was opened on
    pub(crate) fn new(vfs: Arc<dyn Vfs>) -> VfsSpill {
        VfsSpill { vfs }
    }
}

impl Spill for VfsSpill {
    fn open(&self) -> DbResult<Box<dyn SpillFile>> {
        let path = self
            .vfs
            .temp_path("sort")
            .map_err(|why| failed(&format!("no path for a spill file: {why:?}")))?;
        let file = self
            .vfs
            .open(&path, OpenOptions::of_kind(FileKind::Transient))
            .map_err(|why| failed(&format!("a spill file did not open: {why:?}")))?;
        Ok(Box::new(TransientRuns { file, end: 0 }))
    }
}

/// One transient file, appended to and read back from.
struct TransientRuns {
    /// The open file.
    file: Box<dyn VfsFile>,
    /// Where the next append goes.
    ///
    /// Kept rather than asked of the file on every append: the file is written
    /// by this one handle and nothing else can move its end, so asking would
    /// be a system call to learn a number this already knows.
    end: u64,
}

impl SpillFile for TransientRuns {
    fn append(&mut self, bytes: &[u8]) -> DbResult<u64> {
        let at = self.end;
        self.file
            .write_all_at(at, bytes)
            .map_err(|why| failed(&format!("a run did not write: {why:?}")))?;
        self.end = at.saturating_add(bytes.len() as u64);
        Ok(at)
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> DbResult<()> {
        self.file
            .read_exact_at(offset, out)
            .map_err(|why| failed(&format!("a run did not read back: {why:?}")))
    }
}

/// The failure a spill reports.
///
/// `Internal` rather than anything about the caller's file: a spill file is
/// this engine's own scratch, made and deleted inside one statement, so a
/// caller told their data was corrupt would go and look at a database that is
/// fine.
///
/// @param said - what went wrong
fn failed(said: &str) -> DbError {
    DbError::primary(PrimaryCode::Internal).with_message(said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_exec::spill::{decode_row, encode_run, Run, RunReader};
    use inillucent_tree::datum::OwnedDatum;

    /// Returns a spill factory over a fresh memory file system.
    fn factory() -> VfsSpill {
        VfsSpill::new(Arc::new(inillucent_vfs::memory::MemoryVfs::new()))
    }

    /// **A run written through the VFS reads back through the VFS.**
    ///
    /// This is the only thing that exercises `VfsSpill` at all. The shipped
    /// spill threshold is sixty-four mebibytes and nothing in the suite sorts
    /// that much - the largest table any test builds is the eighty mebibyte one
    /// in `story_large_table_nightly`, whose sorted rows come to about ten -
    /// so without these cases the file half of section 4.3.6 would be wired up
    /// and never run. `ops::order`'s own cases exercise the merge over an
    /// in-memory file; these exercise the file.
    #[test]
    fn a_run_written_through_the_vfs_reads_back() {
        let mut file = factory().open().expect("a spill file opens");
        let rows: Vec<Vec<OwnedDatum>> = (0..300)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n),
                    OwnedDatum::Text(format!("value-{n:04}").into_bytes()),
                ]
            })
            .collect();
        let bytes = encode_run(&rows);
        let at = file.append(&bytes).expect("a run is written");
        assert_eq!(at, 0, "the first run in a fresh file starts at zero");

        let mut reader = RunReader::new(Run {
            at,
            bytes: bytes.len() as u64,
            rows: rows.len(),
        });
        let mut read = Vec::new();
        while let Some(row) = reader.next(file.as_ref()).expect("a row") {
            read.push(row);
        }
        assert_eq!(read, rows, "a run through the VFS did not read back");
    }

    /// Two runs in one file land at different offsets and stay apart.
    ///
    /// `append` keeps the end rather than asking the file for it, so a bug
    /// there would put the second run on top of the first - and both would
    /// still decode, which is what makes it worth asserting the offset.
    #[test]
    fn a_second_run_lands_after_the_first() {
        let mut file = factory().open().expect("a spill file opens");
        let first = encode_run(&[vec![OwnedDatum::Int(1)], vec![OwnedDatum::Int(2)]]);
        let second = encode_run(&[vec![OwnedDatum::Int(3)]]);
        let one = file.append(&first).expect("the first run");
        let two = file.append(&second).expect("the second run");
        assert_eq!(one, 0);
        assert_eq!(
            two,
            first.len() as u64,
            "the second run did not start where the first ended"
        );

        let mut held = vec![0u8; second.len()];
        file.read_at(two, &mut held).expect("the second run reads");
        let (row, _) = decode_row(&held)
            .expect("it decodes")
            .expect("it holds a row");
        assert_eq!(row, vec![OwnedDatum::Int(3)]);
    }

    /// A read past the end is refused rather than answering zeroes.
    #[test]
    fn a_read_past_the_end_is_refused() {
        let mut file = factory().open().expect("a spill file opens");
        file.append(&[1, 2, 3, 4]).expect("some bytes");
        let mut held = vec![0u8; 64];
        let failure = file
            .read_at(0, &mut held)
            .expect_err("a read past the end must be refused");
        assert_eq!(failure.code(), PrimaryCode::Internal);
    }
}
