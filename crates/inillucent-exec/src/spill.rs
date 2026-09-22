//! Where a buffering operator puts rows it cannot hold.
//!
//! Invariant: **a spill changes when a row is compared, never how.** The rows
//! that come out of an external sort are the rows a sort in memory would have
//! emitted, in the same order, because both use `compare_by` on the same
//! `OwnedDatum`s. What the spill changes is that the whole set is never
//! resident at once.
//!
//! ## Why the trait is here and the implementation is not
//!
//! `Sort` lives in this crate, and `docs/invariants/layering.toml` does not let
//! this crate depend on `inillucent-vfs` - a query executor has no business
//! naming a file system. `inillucent-engine` may, and already depends on this
//! crate, so the dependency points the right way when the trait is declared
//! here and implemented there (task-2066 §4.3.6).
//!
//! `TreeCatalog::spill` answers `None` by default. An operator with no spill
//! file behaves exactly as it did before this existed: it buffers everything
//! and the byte budget refuses it if a budget was armed.
//!
//! ## What a run is
//!
//! A run is rows in sorted order, each one a `u32` count of values followed by
//! that many tagged values - `Datum::encode_tagged`, which is the same
//! self-describing form the delta area and the exception heap store. Reading a
//! run back is `Datum::decode_tagged` in a loop.
//!
//! There is no header and no checksum. A run is written and read by one
//! statement inside one process, it never outlives the operator, and the file
//! is opened with `delete_on_close`: a format that survived a crash would be a
//! format somebody has to keep compatible.

use inillucent_base::{DbResult, PrimaryCode};
use inillucent_tree::datum::{Datum, OwnedDatum};

/// A file a buffering operator writes runs to and reads them back from.
///
/// Deliberately narrower than the VFS's own file: an operator appends, then
/// reads from an offset, and never locks, syncs, truncates or renames. A
/// narrow trait is what lets the test implementation below be eleven lines.
pub trait SpillFile {
    /// Appends bytes and returns the offset they were written at.
    ///
    /// @param bytes - what to write
    fn append(&mut self, bytes: &[u8]) -> DbResult<u64>;

    /// Reads into `out`, filling it, starting at `offset`.
    ///
    /// @param offset - where to read from
    /// @param out - the buffer to fill
    fn read_at(&self, offset: u64, out: &mut [u8]) -> DbResult<()>;
}

/// Opens the files a statement's buffering operators spill to.
///
/// One per statement rather than one per operator, so a query with two sorts
/// in it does not open two files when one would do; the runs carry their own
/// offsets.
pub trait Spill {
    /// Returns a fresh file to write runs to.
    fn open(&self) -> DbResult<Box<dyn SpillFile>>;
}

/// Where one run lives in a spill file.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    /// Where the run's first row starts.
    pub at: u64,
    /// How many bytes the run occupies.
    pub bytes: u64,
    /// How many rows it holds.
    pub rows: usize,
}

/// Encodes rows into a run's bytes.
///
/// The caller has already sorted them; this only writes.
///
/// @param rows - the rows, in the order they will be read back
pub fn encode_run(rows: &[Vec<OwnedDatum>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        let width = u32::try_from(row.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&width.to_le_bytes());
        for value in row {
            value.borrow().encode_tagged(&mut out);
        }
    }
    out
}

/// Reads one row from the front of a run's bytes.
///
/// Returns the row and how many bytes it consumed, or `None` at the end.
///
/// **The values are copied into owned storage** rather than borrowed from the
/// buffer, because the merge holds one row per run while the buffer that row
/// came from is refilled underneath it.
///
/// @param bytes - the remaining bytes of the run
pub fn decode_row(bytes: &[u8]) -> DbResult<Option<(Vec<OwnedDatum>, usize)>> {
    let Some(header) = bytes.get(..4) else {
        return Ok(None);
    };
    let mut width = [0u8; 4];
    width.copy_from_slice(header);
    let width = u32::from_le_bytes(width) as usize;
    let mut row = Vec::with_capacity(width);
    let mut at = 4usize;
    for _ in 0..width {
        let rest = bytes
            .get(at..)
            .ok_or_else(|| short("a spilled row ends inside a value"))?;
        // **Re-classified on the way out.** `decode_tagged` reports `Corrupt`,
        // which is the right answer about a page and the wrong one about a
        // spill file: these bytes were written by this process minutes ago and
        // are deleted on close, so a caller told their data is corrupt would
        // go and look at a database that is fine.
        let (value, taken) = Datum::decode_tagged(rest)
            .map_err(|why| short(&format!("a spilled row did not decode: {}", why.message())))?;
        row.push(OwnedDatum::from_datum(&value));
        at = at.saturating_add(taken);
    }
    Ok(Some((row, at)))
}

/// How many bytes of a run are held at once while merging.
///
/// Sixty-four kilobytes a run, so a sixteen-way merge holds a megabyte. Large
/// enough that a row almost never spans two fills, small enough that the
/// merge's own footprint is not the thing that runs out of memory.
const WINDOW: usize = 64 * 1024;

/// One run, read back a window at a time.
///
/// **The whole run is never resident**, which is the half of an external sort
/// that makes it external: a merge that read every run into memory would have
/// the same high-water mark as the sort it replaced.
pub struct RunReader {
    /// Where the run lives and how long it is.
    run: Run,
    /// The run-relative offset of `held[0]`.
    start: u64,
    /// The window.
    held: Vec<u8>,
    /// Where in `held` the next row starts.
    at: usize,
    /// How large the next fill will be.
    ///
    /// **It grows, because a row can be larger than the window.** A blob is
    /// bounded by the page size and the page size can be larger than 64 KiB,
    /// so a fixed window would decode the front of such a row, find the value
    /// running past the buffer, and report a truncated run for a run that is
    /// perfectly good. Doubling until the row fits is bounded by the row.
    window: usize,
}

impl RunReader {
    /// Returns a reader positioned at the start of a run.
    ///
    /// @param run - where the run lives
    pub fn new(run: Run) -> RunReader {
        RunReader {
            run,
            start: 0,
            held: Vec::new(),
            at: 0,
            window: WINDOW,
        }
    }

    /// Returns the next row of the run, or `None` when it is exhausted.
    ///
    /// @param file - the spill file the run lives in
    pub fn next(&mut self, file: &dyn SpillFile) -> DbResult<Option<Vec<OwnedDatum>>> {
        loop {
            match decode_row(self.held.get(self.at..).unwrap_or(&[])) {
                Ok(Some((row, taken))) => {
                    self.at = self.at.saturating_add(taken);
                    return Ok(Some(row));
                }
                Ok(None) | Err(_) => {}
            }
            let consumed = self
                .start
                .saturating_add(u64::try_from(self.at).unwrap_or(0));
            if consumed >= self.run.bytes {
                return Ok(None);
            }
            let remaining = self.run.bytes.saturating_sub(consumed);
            let want = usize::try_from(remaining.min(self.window as u64)).unwrap_or(self.window);
            let already = self.held.len().saturating_sub(self.at);
            if want <= already {
                // Everything left is already in the window and it still does
                // not decode, so the run is short. `decode_row` says which.
                return decode_row(self.held.get(self.at..).unwrap_or(&[]))
                    .map(|held| held.map(|(row, _)| row));
            }
            let mut buffer = vec![0u8; want];
            file.read_at(self.run.at.saturating_add(consumed), &mut buffer)?;
            self.start = consumed;
            self.held = buffer;
            self.at = 0;
            // Grow for the next attempt, so a row larger than the window is
            // reached by doubling rather than reported as truncated.
            self.window = self.window.saturating_mul(2);
        }
    }
}

/// The failure a truncated run reports.
///
/// A spill file is written and read by one statement in one process, so a run
/// that does not decode is this engine's own defect rather than a damaged
/// file, and it says so rather than reporting corruption a user could act on.
///
/// @param said - what was wrong
fn short(said: &str) -> inillucent_base::DbError {
    inillucent_base::DbError::primary(PrimaryCode::Internal).with_message(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run round-trips every value kind, in order.
    #[test]
    fn a_run_reads_back_the_rows_it_was_written_from() {
        let rows = vec![
            vec![OwnedDatum::Null, OwnedDatum::Int(-9)],
            vec![OwnedDatum::Real(1.5), OwnedDatum::Text(b"hello".to_vec())],
            vec![OwnedDatum::Blob(vec![0, 255, 7]), OwnedDatum::Int(0)],
        ];
        let bytes = encode_run(&rows);
        let mut read: Vec<Vec<OwnedDatum>> = Vec::new();
        let mut at = 0usize;
        while let Some((row, taken)) = decode_row(bytes.get(at..).unwrap_or(&[])).unwrap() {
            read.push(row);
            at = at.saturating_add(taken);
            if taken == 0 {
                break;
            }
        }
        assert_eq!(
            read, rows,
            "a run did not read back what it was written from"
        );
    }

    /// An empty run reads back as no rows rather than as one empty row.
    #[test]
    fn an_empty_run_holds_nothing() {
        let bytes = encode_run(&[]);
        assert!(bytes.is_empty());
        assert!(decode_row(&bytes).unwrap().is_none());
    }

    /// A run cut short says so rather than answering a row it did not read.
    ///
    /// The bytes come from this process, so the failure is `Internal`: a
    /// caller can do nothing about it and it is not damage to their file.
    #[test]
    fn a_truncated_run_is_refused() {
        let rows = vec![vec![OwnedDatum::Text(b"a long enough value".to_vec())]];
        let bytes = encode_run(&rows);
        let cut = bytes.get(..bytes.len() - 5).unwrap_or(&[]);
        let failure = decode_row(cut).expect_err("a truncated run must not decode");
        assert_eq!(failure.code(), PrimaryCode::Internal);
    }

    /// A spill file held in memory, for the cases below.
    #[derive(Default)]
    struct InMemory {
        bytes: std::cell::RefCell<Vec<u8>>,
    }

    impl SpillFile for InMemory {
        fn append(&mut self, bytes: &[u8]) -> DbResult<u64> {
            let mut held = self.bytes.borrow_mut();
            let at = held.len() as u64;
            held.extend_from_slice(bytes);
            Ok(at)
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> DbResult<()> {
            let held = self.bytes.borrow();
            let from = usize::try_from(offset).unwrap_or(usize::MAX);
            let slice = held
                .get(from..from.saturating_add(out.len()))
                .ok_or_else(|| short("a spill read ran past the file"))?;
            out.copy_from_slice(slice);
            Ok(())
        }
    }

    /// Writes one run and returns where it landed.
    fn write(file: &mut InMemory, rows: &[Vec<OwnedDatum>]) -> Run {
        let bytes = encode_run(rows);
        let at = file.append(&bytes).expect("a run is written");
        Run {
            at,
            bytes: bytes.len() as u64,
            rows: rows.len(),
        }
    }

    /// A run reads back through the reader exactly as it was written.
    #[test]
    fn a_reader_walks_a_run_to_its_end() {
        let mut file = InMemory::default();
        let rows: Vec<Vec<OwnedDatum>> = (0..500)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n),
                    OwnedDatum::Text(format!("row-{n:04}").into_bytes()),
                ]
            })
            .collect();
        let run = write(&mut file, &rows);
        let mut reader = RunReader::new(run);
        let mut read = Vec::new();
        while let Some(row) = reader.next(&file).expect("a row") {
            read.push(row);
        }
        assert_eq!(read, rows, "the reader did not walk the whole run");
    }

    /// **A row larger than the window is read, not reported as truncated.**
    ///
    /// The window is 64 KiB and a value is bounded by the page size, which can
    /// be larger. A fixed window would decode the front of such a row, find
    /// the value running past the buffer, and call a perfectly good run short.
    #[test]
    fn a_row_larger_than_the_window_is_read() {
        let mut file = InMemory::default();
        let rows = vec![
            vec![OwnedDatum::Blob(vec![7u8; 200 * 1024])],
            vec![OwnedDatum::Int(1)],
        ];
        let run = write(&mut file, &rows);
        let mut reader = RunReader::new(run);
        let mut read = Vec::new();
        while let Some(row) = reader.next(&file).expect("a row") {
            read.push(row);
        }
        assert_eq!(read, rows, "a row past the window was not read back");
    }

    /// Two runs in one file do not read each other's rows.
    ///
    /// The offsets are the whole of what keeps them apart, so a reader that
    /// ignored `Run::bytes` would walk off the end of its own run and into the
    /// next one - and the rows would still decode, which is what makes it
    /// worth a case.
    #[test]
    fn two_runs_in_one_file_stay_apart() {
        let mut file = InMemory::default();
        let first: Vec<Vec<OwnedDatum>> = (0..20).map(|n| vec![OwnedDatum::Int(n)]).collect();
        let second: Vec<Vec<OwnedDatum>> = (100..130).map(|n| vec![OwnedDatum::Int(n)]).collect();
        let one = write(&mut file, &first);
        let two = write(&mut file, &second);

        for (run, expected) in [(one, &first), (two, &second)] {
            let mut reader = RunReader::new(run);
            let mut read = Vec::new();
            while let Some(row) = reader.next(&file).expect("a row") {
                read.push(row);
            }
            assert_eq!(&read, expected, "a run read past its own bytes");
        }
    }
}
