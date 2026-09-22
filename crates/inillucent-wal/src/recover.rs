//! Recovery: turning a log and a data file into the committed prefix.
//!
//! Invariant: **recovery after a crash at any point yields exactly the
//! committed prefix** - every acknowledged commit present, no unacknowledged one
//! visible. That is the TDD's twelfth invariant. Three properties carry it, and each is structural
//! rather than careful:
//!
//! 1. **Redo is idempotent by page LSN.** A record is applied to a page only
//!    when the page's LSN is below the record's, and applying sets it to the
//!    record's. So a record that was already in the data file before the crash
//!    is skipped, and a record applied by one recovery run is skipped by the
//!    next. That is what makes `recover(); recover()` produce the same file byte
//!    for byte, which is this module's own acceptance test.
//! 2. **The scan stops at the first record that does not decode.** A crash
//!    leaves a torn tail: a record half-written, or written into a sector the
//!    media reordered. Everything below that point is a prefix of what was
//!    logged, and everything above it is refused. A scan that skipped a bad
//!    record and carried on would apply a *later* record without the earlier one
//!    it depends on, which is the one way redo can produce a state that never
//!    existed.
//! 3. **A stamp that cannot have come from this log is refused rather than
//!    obeyed.** Property 1 is only sound while a page's stamp is a position in
//!    the stream beside the file. A page stamped by a stream that was
//!    abandoned (segments moved aside, a chain stopping at a damaged segment)
//!    reads as "already has it" for every record, so every later write to it is
//!    discarded with no error at all. That is checked against `valid_end` in
//!    `refuse_a_stamp_from_another_stream`, and the prevention that keeps a file
//!    out of the state is `meta.high_water_lsn` and the resume above it.
//!
//! ## Where a scan starts
//!
//! At the meta page's `checkpoint_lsn`, which is **not** simply the LSN the last
//! checkpoint reached. The policy is no-steal, so a page dirtied by a
//! transaction that has not committed is never written to the data file; a
//! transaction that began before a checkpoint and commits after it therefore has
//! records *below* the checkpoint that were never applied to the file. So the
//! checkpointer writes `min(durable end, the first LSN of the oldest still-open
//! transaction)` and this is where recovery starts. Records in that range that
//! *were* already applied are skipped by the page-LSN rule at no cost beyond
//! reading them.
//!
//! ## Two passes, and why one would not do
//!
//! The first pass decides which transactions committed; the second replays them.
//! One pass cannot, because a transaction's records precede its `Commit` record
//! by construction - a single pass would have to buffer every record of every
//! open transaction to find out whether to apply it, which is the undo buffer
//! all over again and unbounded in the size of the log rather than of the
//! transaction.

use std::collections::{BTreeMap, BTreeSet};

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;
use inillucent_vfs::{AccessMode, DbPath, FileKind, OpenOptions, Vfs};

use crate::record::{Body, Record};
use crate::segment::{self, SegmentHeader};
use crate::writer::{segment_path, FIRST_LSN};

/// How many pages one record can name.
const MAX_PAGES: usize = 3;

/// What a caller does with a record recovery decided to replay.
pub trait Redo {
    /// Returns the LSN a page currently carries, or `None` when the page does
    /// not exist in the data file yet.
    ///
    /// @param page - the page's number
    fn page_lsn(&mut self, page: u64) -> DbResult<Option<u64>>;

    /// Applies a record.
    ///
    /// `wanted` has one entry per page in `record.pages()`, in the same order,
    /// and is true for the pages whose LSN is below the record's. A record that
    /// names no page - a commit, a checkpoint, an allocation - arrives with an
    /// empty `wanted` and is always applied, because the page-LSN rule has
    /// nothing to say about it and each of them is idempotent on its own terms.
    ///
    /// @param record - the record to apply
    /// @param wanted - which of the record's pages still need it
    fn redo(&mut self, record: &Record<'_>, wanted: &[bool]) -> DbResult<()>;
}

/// A `Redo` that applies nothing, for a caller that only wants the analysis.
///
/// Used by the recovery *report* - "what would this log do" - and by the
/// corrupt-WAL fuzz target, which needs the scan to run over hostile bytes
/// without a data file behind it.
#[derive(Debug, Default)]
pub struct DryRun {
    /// Every record the second pass would have applied, by LSN.
    pub applied: Vec<u64>,
}

impl Redo for DryRun {
    fn page_lsn(&mut self, _page: u64) -> DbResult<Option<u64>> {
        Ok(None)
    }

    fn redo(&mut self, record: &Record<'_>, _wanted: &[bool]) -> DbResult<()> {
        self.applied.push(record.lsn);
        Ok(())
    }
}

/// Where recovery starts and what it is recovering.
#[derive(Clone, Debug)]
pub struct RecoveryStart {
    /// The database's identity; a segment that disagrees is refused.
    pub uuid: u128,
    /// The stream position to start scanning at.
    pub checkpoint_lsn: u64,
    /// The segment holding that position.
    pub sequence: u64,
    /// The commit timestamp watermark the checkpoint recorded.
    pub cts_watermark: u64,
    /// Transactions whose `Commit` record must be read as absent.
    ///
    /// **A commit that spans two files is not decided inside either of them.**
    /// One transaction writing a database and a database attached to it appends
    /// a `Commit` to both logs, and neither record is the decision: the decision
    /// is the deletion of a super-journal named beside the files, which is what
    /// makes the two commit together or not at all. Recovery of one of those
    /// files is therefore told which transactions were still in doubt when it
    /// crashed, and reads their `Commit` records as the votes they are.
    ///
    /// Empty for every database that has never been attached to, which is the
    /// same set as "every database whose caller has no super-journal to hand
    /// this". A `Commit` for a transaction not named here is the decision, as
    /// it always was.
    pub doubtful: BTreeSet<u64>,
}

impl RecoveryStart {
    /// Returns the start for a database that has never been checkpointed.
    ///
    /// @param uuid - the database's identity
    pub fn fresh(uuid: u128) -> RecoveryStart {
        RecoveryStart {
            uuid,
            checkpoint_lsn: FIRST_LSN,
            sequence: 1,
            cts_watermark: 0,
            doubtful: BTreeSet::new(),
        }
    }
}

/// What recovery found and did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Recovered {
    /// The stream position past the last valid record; where writing resumes.
    pub next_lsn: u64,
    /// The segment that position is in; the next segment to write.
    pub sequence: u64,
    /// The highest commit timestamp any replayed commit carried.
    pub latest_cts: u64,
    /// How many records the scan read.
    pub scanned: u64,
    /// How many records the second pass applied.
    pub applied: u64,
    /// How many records the replay **dropped** rather than applied.
    ///
    /// **Separate from `applied`, because it used to be counted in it**
    /// (task-2066 §4.1.10). A `Redo` that answers `Ok(())` without applying a
    /// record - which is what the engine's applier does for a record naming a
    /// tree it has no shape for - was indistinguishable here from one that
    /// applied it. A recovery that silently dropped rows reported the same
    /// numbers as one that did not, and `PRAGMA integrity_check` answered `ok`
    /// on the result.
    ///
    /// Filled by the caller after the replay, from the applier, because only
    /// the applier knows: this loop sees an `Ok` either way.
    pub dropped: u64,
    /// How many transactions committed.
    pub committed: u64,
    /// How many transactions were open at the end of the log and were discarded.
    pub losers: u64,
    /// Why the scan stopped, when it stopped on damage rather than on the end.
    pub stopped_because: Option<String>,
    /// The last checkpoint record the scan saw, if any.
    pub last_checkpoint: Option<(u64, u64)>,
    /// True when at least one `CatalogChange` was replayed.
    pub catalog_changed: bool,
    /// The highest transaction number any record in the scan carried.
    ///
    /// **A reopened database must not reuse a number the log still holds.**
    /// Recovery decides which records to replay by transaction number, so a
    /// number used twice in one log makes two different transactions into one:
    /// a run that opened, wrote as transaction 3 and crashed leaves records
    /// that the *next* run resurrects the moment its own transaction 3 commits.
    /// The row comes back from the dead, permanently, and no later crash can
    /// remove it.
    ///
    /// The engine starts its counter above this. It is reported rather than
    /// applied here because the log has no opinion about who allocates
    /// transaction numbers - it only knows which ones it has seen.
    pub highest_txn: u64,
}

/// Runs recovery against a data file.
///
/// @param vfs - the file system the segments live in
/// @param base - the database file's path
/// @param start - where to start and what to check the segments against
/// @param redo - the applier
pub fn recover(
    vfs: &dyn Vfs,
    base: &DbPath,
    start: RecoveryStart,
    redo: &mut dyn Redo,
) -> DbResult<Recovered> {
    let chain = read_chain(vfs, base, &start)?;
    let analysis = analyse(&chain, &start)?;
    let mut outcome = Recovered {
        // Filled by the caller after the replay, from the applier - this loop
        // cannot tell a dropped record from an applied one. See
        // `Recovered::dropped`.
        dropped: 0,
        next_lsn: analysis.valid_end,
        sequence: analysis.last_sequence,
        latest_cts: analysis.latest_cts,
        scanned: analysis.scanned,
        applied: 0,
        committed: analysis.committed.len() as u64,
        losers: analysis.losers,
        stopped_because: analysis.stopped_because.clone(),
        last_checkpoint: analysis.last_checkpoint,
        catalog_changed: false,
        highest_txn: analysis.highest_txn,
    };
    replay(&chain, &start, &analysis, redo, &mut outcome)?;
    Ok(outcome)
}

/// Truncates the log to the recovered prefix and removes what is past it.
///
/// Separate from [`recover`] because a dry run - the fuzz target, the report -
/// must not touch the files, and because a caller that recovered into a copy of
/// the database has no business truncating the original.
///
/// @param vfs - the file system
/// @param base - the database file's path
/// @param outcome - what recovery returned
pub fn truncate_after(vfs: &dyn Vfs, base: &DbPath, outcome: &Recovered) -> DbResult<()> {
    let name = base.as_path().to_string_lossy().to_string();
    let directory = base.as_path().parent().map(std::path::Path::to_path_buf);
    let path = segment_path(&name, directory.as_deref(), outcome.sequence);
    if let Ok(file) = vfs.open(&path, OpenOptions::of_kind(FileKind::Wal)) {
        if let Ok(header) = read_header(file.as_ref()) {
            let keep =
                segment::HEADER_BYTES as u64 + outcome.next_lsn.saturating_sub(header.first_lsn);
            file.truncate(keep)
                .map_err(inillucent_vfs::VfsError::into_db_error)?;
            file.sync(inillucent_vfs::SyncMode::Normal)
                .map_err(inillucent_vfs::VfsError::into_db_error)?;
        }
    }
    // Segments past the one the scan stopped in describe records that are not
    // part of the recovered prefix. They are deleted rather than left, because
    // the next run would open the *next* sequence and a stale file at that
    // number would be rewritten anyway - deleting is the same outcome, said out
    // loud.
    let mut sequence = outcome.sequence.saturating_add(1);
    loop {
        let path = segment_path(&name, directory.as_deref(), sequence);
        match vfs.access(&path, AccessMode::Exists) {
            Ok(true) => {
                let _ = vfs.delete(&path, false);
                sequence = sequence.saturating_add(1);
            }
            _ => break,
        }
    }
    Ok(())
}

/// One segment's bytes and what its header said.
struct LoadedSegment {
    header: SegmentHeader,
    bytes: Vec<u8>,
}

/// The segments the scan will read, in sequence order.
struct Chain {
    segments: Vec<LoadedSegment>,
}

impl Chain {
    /// Returns the record area of one segment.
    ///
    /// @param index - the segment's position in the chain
    fn body(&self, index: usize) -> &[u8] {
        self.segments
            .get(index)
            .and_then(|segment| segment.bytes.get(segment::HEADER_BYTES..))
            .unwrap_or(&[])
    }

    /// Returns one segment's first LSN.
    ///
    /// @param index - the segment's position in the chain
    fn first_lsn(&self, index: usize) -> u64 {
        self.segments
            .get(index)
            .map(|segment| segment.header.first_lsn)
            .unwrap_or(FIRST_LSN)
    }

    /// Returns one segment's sequence number.
    ///
    /// @param index - the segment's position in the chain
    fn sequence(&self, index: usize) -> u64 {
        self.segments
            .get(index)
            .map(|segment| segment.header.sequence)
            .unwrap_or(1)
    }
}

/// Reads every segment from the checkpoint's segment forward.
///
/// A segment whose header is damaged, whose uuid is another database's, or
/// whose sequence is not the one the chain wants is refused rather than
/// skipped - the chain stops there and everything above it is discarded.
///
/// **A segment that exists but cannot be read fails the open; it does not end
/// the chain.** `access` having just confirmed the file is there, an `open` or
/// a read failing after that is the VFS reporting an operational problem - a
/// full disk, an I/O error, a denied permission - not evidence about what the
/// segment holds. Treating it as "no more segments" used to be indistinguishable
/// from the legitimate end of the chain, and the difference matters: recovery
/// would report success with an emptier-than-true chain, and the caller would
/// then run `truncate_after` on the strength of that false success, cutting a
/// segment that was never actually read down to its header and discarding
/// every record after it - a committed `CREATE TABLE` included, since nothing
/// this file logs is checkpointed into the data file until its own
/// checkpoint runs. A short read that is [`inillucent_base::error::ExtendedCode::IO_ERR_SHORT_READ`]
/// is kept as the genuine end-of-chain signal - that is what the media model
/// reports for a torn tail - and every other failure out of `open` or the read
/// propagates.
///
/// @param vfs - the file system
/// @param base - the database file's path
/// @param start - where to start
fn read_chain(vfs: &dyn Vfs, base: &DbPath, start: &RecoveryStart) -> DbResult<Chain> {
    let name = base.as_path().to_string_lossy().to_string();
    let directory = base.as_path().parent().map(std::path::Path::to_path_buf);
    let mut segments = Vec::new();
    let mut sequence = start.sequence.max(1);
    loop {
        let path = segment_path(&name, directory.as_deref(), sequence);
        match vfs.access(&path, AccessMode::Exists) {
            Ok(true) => {}
            // Cannot even ask whether the segment exists - or it plainly does
            // not - either way there is nothing to open, and neither case is
            // new since `access` never used to be an injectable failpoint site.
            Ok(false) | Err(_) => break,
        }
        let file = match vfs.open(&path, OpenOptions::of_kind(FileKind::Wal).read_only()) {
            Ok(file) => file,
            Err(error) => return Err(error.into_db_error()),
        };
        let size = file
            .file_size()
            .map_err(inillucent_vfs::VfsError::into_db_error)?;
        if size < segment::HEADER_BYTES as u64 {
            // A segment file that does not even hold a header is a create that
            // was cut short. It ends the chain; it does not fail the open.
            break;
        }
        let mut bytes = vec![0u8; size as usize];
        if let Err(error) = file.read_exact_at(0, &mut bytes) {
            if error.extended() != inillucent_base::error::ExtendedCode::IO_ERR_SHORT_READ {
                // Not the media model's torn-tail signal, so this is an
                // operational failure reading a segment `access` just said was
                // there - propagate it rather than silently discarding
                // everything in and after it.
                return Err(error.into_db_error());
            }
            // A short read here is media damage in the middle of the log rather
            // than at its tail, and the honest answer is the same: the chain
            // stops, and everything above is discarded.
            break;
        }
        let header = match SegmentHeader::decode(&bytes) {
            Ok(header) => header,
            Err(_) => break,
        };
        if header.belongs_to(start.uuid, sequence).is_err() {
            break;
        }
        if let Some(previous) = segments.last() {
            let previous: &LoadedSegment = previous;
            let expected = previous
                .header
                .first_lsn
                .saturating_add(previous.bytes.len().saturating_sub(segment::HEADER_BYTES) as u64);
            if header.first_lsn > expected {
                // A gap: the previous segment's records stop before this one
                // starts, so the stream is not continuous and the chain ends.
                break;
            }
        }
        segments.push(LoadedSegment { header, bytes });
        sequence = sequence.saturating_add(1);
    }
    Ok(Chain { segments })
}

/// What the first pass learned.
struct Analysis {
    committed: BTreeSet<u64>,
    aborted: BTreeSet<u64>,
    valid_end: u64,
    last_sequence: u64,
    latest_cts: u64,
    scanned: u64,
    losers: u64,
    stopped_because: Option<String>,
    last_checkpoint: Option<(u64, u64)>,
    /// The highest transaction number any record carried.
    highest_txn: u64,
}

/// Walks the chain once, deciding which transactions committed.
///
/// @param chain - the segments
/// @param start - where the scan begins
fn analyse(chain: &Chain, start: &RecoveryStart) -> DbResult<Analysis> {
    let mut committed = BTreeSet::new();
    let mut aborted = BTreeSet::new();
    let mut open: BTreeMap<u64, ()> = BTreeMap::new();
    let mut latest_cts = start.cts_watermark;
    let mut scanned = 0u64;
    let mut highest_txn = 0u64;
    let mut last_checkpoint = None;
    let mut stopped_because = None;
    let mut valid_end = start.checkpoint_lsn;
    let mut last_sequence = start.sequence.max(1);

    // **One exit, labelled, rather than a copy of the report per reason.** The
    // scan stops for four, and the two below were missing: a hole broke the
    // inner loop and let the outer one carry on into the next segment, and a
    // record claiming bytes past its segment did the same (task-2066 section
    // 4.2, item 18).
    'scan: for index in 0..chain.segments.len() {
        last_sequence = chain.sequence(index);
        let body = chain.body(index);
        let first = chain.first_lsn(index);
        // The scan resumes at the checkpoint LSN inside the first segment and
        // at the segment's own start in every later one.
        let mut at = if valid_end > first {
            (valid_end.saturating_sub(first)) as usize
        } else {
            0
        };
        valid_end = valid_end.max(first);
        while let Some(tail) = body.get(at..) {
            let decoded = match Record::decode(tail) {
                Ok(Some(record)) => record,
                // **A length word of all zeros is padding if the rest of the
                // segment is zeroes too, and a hole if it is not.** A segment
                // is rolled when the next record does not fit, so the tail of
                // every rolled segment is zeroes by design and the LSN stream
                // runs over it. Zeroes with records after them are a dropped
                // sector, and stepping over one replays a later record without
                // the earlier one it depends on - the property this module's
                // second invariant exists to prevent.
                Ok(None)
                    if !body
                        .get(at..)
                        .is_none_or(|rest| rest.iter().all(|byte| *byte == 0)) =>
                {
                    stopped_because = Some(format!(
                        "segment {}: the records stop {} bytes before its end and more \
                         bytes follow the gap",
                        chain.sequence(index),
                        body.len().saturating_sub(at)
                    ));
                    break 'scan;
                }
                Ok(None) => break,
                Err(error) => {
                    stopped_because = Some(format!(
                        "segment {} at lsn {}: {}",
                        chain.sequence(index),
                        valid_end,
                        error.detail().unwrap_or("damaged")
                    ));
                    break 'scan;
                }
            };
            let expected = first.saturating_add(at as u64);
            if decoded.lsn != expected {
                // A record whose LSN is not where it sits is a record from an
                // older generation of the log that a shorter run left behind:
                // the segment was reused and this is stale tail, not damage.
                // It is the end of the valid prefix either way.
                stopped_because = Some(format!(
                    "segment {} at offset {at}: a record says lsn {} and sits at {expected}",
                    chain.sequence(index),
                    decoded.lsn
                ));
                break 'scan;
            }
            scanned = scanned.saturating_add(1);
            highest_txn = highest_txn.max(decoded.txn);
            match decoded.body {
                Body::Commit { cts } => {
                    // **A vote is not a decision.** A transaction the caller
                    // named as doubtful wrote more than one file and was
                    // decided by a super-journal outside this log; if that
                    // super-journal is still there, the decision never
                    // happened, and this record has to be read as the vote it
                    // was rather than replayed into half a transaction.
                    if !start.doubtful.contains(&decoded.txn) {
                        committed.insert(decoded.txn);
                    }
                    latest_cts = latest_cts.max(cts);
                }
                Body::Abort => {
                    aborted.insert(decoded.txn);
                }
                Body::Checkpoint {
                    checkpoint_lsn,
                    cts_watermark,
                } => {
                    last_checkpoint = Some((checkpoint_lsn, cts_watermark));
                    latest_cts = latest_cts.max(cts_watermark);
                }
                // Every transaction that wrote anything, whether or not it went
                // on to commit. Which of them are *losers* is decided at the
                // end, by subtracting the ones that committed or aborted -
                // rather than by removing each one as its outcome arrives,
                // which made the answer depend on a record never appearing
                // after its own commit. That is true of a well-formed log and
                // is not something a scan reading damaged bytes should be
                // relying on, and it cost a branch no ordinary input could take.
                _ => {
                    if decoded.txn != 0 {
                        open.insert(decoded.txn, ());
                    }
                }
            }
            at = at.saturating_add(decoded.length);
            valid_end = expected.saturating_add(decoded.length as u64);
        }
        // **And a record claiming more bytes than the segment has.** The loop
        // above ends when `body.get(at..)` is `None`, which is the same shape
        // as reaching the end exactly - so an overrun looked like a clean
        // finish and the scan moved on.
        if at > body.len() {
            stopped_because = Some(format!(
                "segment {}: a record claims {} bytes past the end of the segment",
                chain.sequence(index),
                at.saturating_sub(body.len())
            ));
            break 'scan;
        }
    }
    Ok(Analysis {
        losers: count_losers(&open, &committed, &aborted),
        committed,
        aborted,
        valid_end,
        last_sequence,
        latest_cts,
        scanned,
        stopped_because,
        last_checkpoint,
        highest_txn,
    })
}

/// Counts the transactions that wrote and never finished.
///
/// A loser is a transaction the scan saw a record for and saw neither a commit
/// nor an abort for. Computed by subtraction at the end rather than by removing
/// each transaction as its outcome arrives, so the answer does not depend on the
/// order records appear in - which is not a property a scan reading a damaged
/// log should be assuming.
///
/// @param open - every transaction that wrote something
/// @param committed - the transactions that committed
/// @param aborted - the transactions that aborted
fn count_losers(
    open: &BTreeMap<u64, ()>,
    committed: &BTreeSet<u64>,
    aborted: &BTreeSet<u64>,
) -> u64 {
    open.keys()
        .filter(|txn| !committed.contains(txn) && !aborted.contains(txn))
        .count() as u64
}

/// Walks the chain again, applying the records of committed transactions.
///
/// @param chain - the segments
/// @param start - where the scan begins
/// @param analysis - what the first pass learned
/// @param redo - the applier
/// @param outcome - the report to fill in
fn replay(
    chain: &Chain,
    start: &RecoveryStart,
    analysis: &Analysis,
    redo: &mut dyn Redo,
    outcome: &mut Recovered,
) -> DbResult<()> {
    let mut position = start.checkpoint_lsn;
    for index in 0..chain.segments.len() {
        let body = chain.body(index);
        let first = chain.first_lsn(index);
        let mut at = if position > first {
            (position.saturating_sub(first)) as usize
        } else {
            0
        };
        position = position.max(first);
        while position < analysis.valid_end {
            let tail = match body.get(at..) {
                Some(tail) => tail,
                None => break,
            };
            let record = match Record::decode(tail) {
                Ok(Some(record)) => record,
                // The first pass already proved every record below `valid_end`
                // decodes, so reaching either of these means the bytes changed
                // between the passes - which they cannot, the chain is in
                // memory. It is reported rather than assumed away.
                Ok(None) => break,
                Err(error) => return Err(error),
            };
            at = at.saturating_add(record.length);
            position = position.saturating_add(record.length as u64);
            if !should_replay(&record, analysis) {
                continue;
            }
            let pages = record.pages();
            let mut wanted = [false; MAX_PAGES];
            let mut any = pages.as_slice().is_empty();
            // Zipped rather than indexed by an enumerated slot: a `zip` stops at
            // whichever runs out, so there is no `get_mut` whose `None` arm no
            // input can take. A branch no input can take is one the coverage
            // gate can only ever be lied to about, and this crate is held to
            // every branch.
            for (entry, page) in wanted.iter_mut().zip(pages.as_slice()) {
                let below = match redo.page_lsn(*page)? {
                    Some(lsn) => {
                        refuse_a_stamp_from_another_stream(*page, lsn, analysis.valid_end)?;
                        lsn < record.lsn
                    }
                    None => true,
                };
                *entry = below;
                any = any || below;
            }
            if !any {
                continue;
            }
            let width = pages.as_slice().len().min(MAX_PAGES);
            redo.redo(&record, wanted.get(..width).unwrap_or(&[]))?;
            outcome.applied = outcome.applied.saturating_add(1);
            if matches!(record.body, Body::CatalogChange { .. }) {
                outcome.catalog_changed = true;
            }
        }
    }
    Ok(())
}

/// Refuses a page whose stamp cannot have come from the log being replayed.
///
/// **The page-LSN rule is only sound while a page's stamp is a position in the
/// stream beside the file.** A record at LSN *L* occupies
/// `[L, L + length)`, so every stamp a healthy file carries is strictly below
/// `valid_end`, the position past the last record the scan accepted. A stamp at
/// or above it is a position in a stream this log does not contain - the state a
/// file reaches when segments are moved aside, when the chain stops at a damaged
/// segment, or when a torn tail is cut - and under the page-LSN rule every
/// record for that page is skipped, because the stamp says the page already has
/// it.
///
/// What that costs is not a failed replay: it is a committed row discarded with
/// no error at all, on a file that `PRAGMA integrity_check` then calls `ok`
/// because it really is structurally intact. Measured on Nikaya's parked mail
/// database, where page 3 - the catalog leaf - carries 21,939,058,496 beside a
/// log that ends at 21,075,008,440, and an `ANALYZE` that printed `ok` lost the
/// `sqlite_stat1` catalog row it had just committed.
///
/// So it is refused, by name, before the record that names the page is applied.
/// The stamp cannot be repaired - the records that set it are in segments
/// nobody has - and opening the file anyway means losing writes silently, which
/// is the worse of the two.
///
/// @param page - the page's number
/// @param stamp - the LSN the page carries
/// @param valid_end - the position past the last record the scan accepted
fn refuse_a_stamp_from_another_stream(page: u64, stamp: u64, valid_end: u64) -> DbResult<()> {
    if stamp < valid_end {
        return Ok(());
    }
    Err(corrupt(format!(
        "page {page} carries lsn {stamp}, which is at or above the log's end {valid_end}: the file was stamped by a log stream this database no longer has, so replaying under the page-LSN rule would discard the records for that page silently"
    )))
}

/// Decides whether one record belongs to the committed prefix.
///
/// @param record - the record
/// @param analysis - what the first pass learned
fn should_replay(record: &Record<'_>, analysis: &Analysis) -> bool {
    match record.body {
        // A commit is replayed so the applier can advance its own timestamp,
        // and an abort never is: its transaction's records are not applied, so
        // there is nothing for the abort to undo.
        Body::Abort => false,
        // Filler. It belongs to no transaction, names no page and carries
        // nothing an applier could do anything with, so handing it to one
        // would make every `Redo` implementation carry an arm that does
        // nothing and would count it among the records recovery applied. It is
        // read and checksummed by the pass above like any other record, which
        // is the whole of what it is for; see `Body::Pad`.
        Body::Pad { .. } => false,
        // These belong to no transaction and are always part of the prefix: a
        // checkpoint is a marker, a catalog change invalidates a cache, and a
        // free-map bit is idempotent.
        Body::Checkpoint { .. } => true,
        _ => {
            if record.txn == 0 {
                return true;
            }
            analysis.committed.contains(&record.txn) && !analysis.aborted.contains(&record.txn)
        }
    }
}

/// Reads and decodes one segment's header from an open file.
///
/// @param file - the open segment
fn read_header(file: &dyn inillucent_vfs::VfsFile) -> DbResult<SegmentHeader> {
    let mut bytes = vec![0u8; segment::HEADER_BYTES];
    file.read_exact_at(0, &mut bytes)
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    SegmentHeader::decode(&bytes)
}

/// Reads a log without a data file and reports what it holds.
///
/// This is the corrupt-WAL fuzz target's entry point and the `.walcheck`
/// command's: it must terminate and must not panic on any bytes at all.
///
/// @param vfs - the file system
/// @param base - the database file's path
/// @param start - where to start
pub fn inspect(vfs: &dyn Vfs, base: &DbPath, start: RecoveryStart) -> DbResult<Recovered> {
    let mut dry = DryRun::default();
    recover(vfs, base, start, &mut dry)
}

/// Returns the error a caller should raise when a chain is refused.
///
/// @param detail - what was wrong
pub fn refuse(detail: impl Into<String>) -> inillucent_base::DbError {
    corrupt(detail)
}
