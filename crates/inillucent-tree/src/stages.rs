//! Where one row's write into a leaf spends its time.
//!
//! Invariant: **nothing here records unless a harness turned it on.** Every
//! field is filled once per call to `PagedTree::write_row`, which is the write
//! path an `INSERT` and a virtual table's shadow row both reach, so a timer
//! that was always on would put fifteen clock reads into the hottest loop the
//! engine has. `crates/inillucent-compat/tests/put_stages.rs` asserts the
//! default is off.
//!
//! ## What this is for
//!
//! task-2025 measured a shadow row write down to `PagedTree::put` and stopped
//! there: 1,505 of 1,508 writes found their leaf from the hint, compacted
//! nothing and split nothing, and still cost 1.8 to 2.2 microseconds each. The
//! stages above that call were all small - the engine's insert arm 0.44 to 0.55
//! microseconds a document, borrowing the row 0.07 ms across all 1,508 writes -
//! so the remaining time is inside this one function and no number said where.
//! Naming the pieces is what turns "`put` costs two microseconds" into a stage
//! a change can be aimed at.
//!
//! ## Why the caller adds its own nanoseconds up first
//!
//! A `Cell<PutStages>` copies the whole struct out and back on every write to
//! it, and this struct is seventeen counters wide. So `write_row` keeps its
//! measurements in locals and calls [`record`] once, at the end of the write -
//! the same reason `WriteStats` is read once and written once there. task-2006
//! took the timers that ran on every write out of that path for exactly this
//! cost, and they are only back because they are behind a switch.

/// Where one pass of `PagedTree::write_row` spent its time, in nanoseconds.
///
/// `whole` is the call; every other duration is a part of it, so
/// `whole` minus the rest is what the function does and does not name -
/// the key vector, the key encoding, the two counter updates and the calls
/// themselves.
///
/// `apply` is `apply_row`, and `orphans`, `logging` and `modify` are its three
/// pieces. `plan` and `delta` are inside `modify`, which also pays for
/// resolving the frame, marking it dirty and parsing the leaf header.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PutStages {
    /// Writes that reached the end of `write_row` and returned.
    ///
    /// A row the delta area cannot take at any fill is packed into the sorted
    /// region instead and returns from `place_row_outside_the_delta_area`, so
    /// it is not counted here. That path needs a page too small for the shipped
    /// default to reach at all (task-2033).
    pub rows: u64,
    /// The whole of one `write_row`.
    pub whole: u128,
    /// Encoding the row, and writing any value too wide for a leaf out of line.
    pub encode: u128,
    /// Finding the leaf: the hint's two comparisons, or a descent when it misses.
    pub find: u128,
    /// `locate_and_read_previous`: the page parse, the binary search, the delta walk.
    pub locate: u128,
    /// Of `locate`: pinning the page and parsing its header, before the key is looked for.
    pub fetch: u128,
    /// Of `locate`: the binary search of the delta directory, or the scan of a
    /// delta area format 1 wrote.
    pub deltas: u128,
    /// Of `locate`: the binary search of the sorted region.
    pub search: u128,
    /// Asking the leaf whether the row fits, which is a second `modify` of the page.
    pub room: u128,
    /// Of `room`: parsing the leaf and doing the arithmetic, without the `modify` around it.
    ///
    /// **The one number that prices the duplicate.** This is the same
    /// arithmetic `plan` does inside `apply_row`, computed a second time
    /// because the plan's offsets cannot be carried across the log record when
    /// the write displaces a row. Whether they can be carried when it displaces
    /// nothing is the change this split is asked to justify.
    pub roomwork: u128,
    /// `record_undo`: the before-image a transaction needs to be able to abandon.
    pub undo: u128,
    /// The whole of `apply_row`.
    pub apply: u128,
    /// `orphaned_extents`: the question asked on every write that answers on almost none.
    pub orphans: u128,
    /// `log.log(InsertRow)`: one write-ahead record carrying the encoded row's bytes.
    pub logging: u128,
    /// The `modify` that writes the page: resolving the frame, and the mutation inside it.
    pub modify: u128,
    /// `plan_encoded` inside that `modify`: deciding where in the delta area the row goes.
    pub plan: u128,
    /// `apply_delta` and `set_lsn` inside it: the row's bytes reaching the page.
    pub delta: u128,
    /// Writes that found their leaf full and compacted or split it before writing.
    ///
    /// **The one class that has to be separable from the rest.** task-2025
    /// measured 44 compactions and 2 splits over 1,508 shadow row writes and
    /// those 46 writes were the whole 1.28 to 1.59 ms of making room, so an
    /// average over all 1,508 hides the number the ticket is about - what a
    /// write that makes no room costs.
    pub remade: u64,
    /// Nanoseconds inside `make_room`, which is the compaction or the split.
    pub making: u128,
    /// The whole of the `write_row` calls that made room, so the rest can be taken out.
    ///
    /// A total only: one write hands over `whole` and `remade`, and [`record`]
    /// is what adds the one to this when the other says to.
    pub remade_whole: u128,
}

thread_local! {
    /// Where this thread's leaf writes have spent their time.
    ///
    /// On the thread rather than on the tree, because a statement writes
    /// several trees - a table and each of its indexes, a module and each of
    /// its shadow tables - and the question is where the time went, not which
    /// tree it went into. `crate::write::WriteStats` is per tree and the engine
    /// adds every tree's up to answer at all.
    static PUT_STAGES: std::cell::Cell<PutStages> =
        const { std::cell::Cell::new(PutStages {
            rows: 0,
            whole: 0,
            encode: 0,
            find: 0,
            locate: 0,
            fetch: 0,
            deltas: 0,
            search: 0,
            room: 0,
            roomwork: 0,
            undo: 0,
            apply: 0,
            orphans: 0,
            logging: 0,
            modify: 0,
            plan: 0,
            delta: 0,
            remade: 0,
            making: 0,
            remade_whole: 0,
        }) };

    /// Whether this thread is recording where its leaf writes go.
    ///
    /// Read at every timed site, so a build nobody is measuring takes one
    /// thread-local `bool` and never reads a clock.
    static RECORDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reports whether this thread is recording where its leaf writes go.
pub fn recording() -> bool {
    RECORDING.with(|held| held.get())
}

/// Returns a clock, but only while this thread is recording.
///
/// So an unmeasured write pays a `bool` rather than a `QueryPerformanceCounter`.
pub fn clock() -> Option<std::time::Instant> {
    recording().then(std::time::Instant::now)
}

/// Returns the nanoseconds since a clock, or zero when there was none.
///
/// @param started - the clock [`clock`] handed out, if it handed one out
pub fn elapsed(started: Option<std::time::Instant>) -> u128 {
    started.map_or(0, |at| at.elapsed().as_nanos())
}

/// Adds one write's measurements to what this thread has spent.
///
/// Does nothing when this thread is not recording, so the caller may build its
/// numbers unconditionally and pay nothing for them.
///
/// @param one - one write's durations, each of them zero when no clock was read
pub fn record(one: PutStages) {
    if !recording() {
        return;
    }
    PUT_STAGES.with(|held| {
        let mut total = held.get();
        total.rows = total.rows.saturating_add(one.rows);
        total.whole = total.whole.saturating_add(one.whole);
        total.encode = total.encode.saturating_add(one.encode);
        total.find = total.find.saturating_add(one.find);
        total.locate = total.locate.saturating_add(one.locate);
        total.fetch = total.fetch.saturating_add(one.fetch);
        total.deltas = total.deltas.saturating_add(one.deltas);
        total.search = total.search.saturating_add(one.search);
        total.room = total.room.saturating_add(one.room);
        total.roomwork = total.roomwork.saturating_add(one.roomwork);
        total.undo = total.undo.saturating_add(one.undo);
        total.apply = total.apply.saturating_add(one.apply);
        total.orphans = total.orphans.saturating_add(one.orphans);
        total.logging = total.logging.saturating_add(one.logging);
        total.modify = total.modify.saturating_add(one.modify);
        total.plan = total.plan.saturating_add(one.plan);
        total.delta = total.delta.saturating_add(one.delta);
        total.remade = total.remade.saturating_add(one.remade);
        total.making = total.making.saturating_add(one.making);
        // A write that made room contributes the whole of itself here, which is
        // what lets a report take those writes out: 46 of task-2025's 1,508 made
        // room and they were the whole of its cost, and the question this split
        // is asked is what the other 1,462 paid.
        if one.remade > 0 {
            total.remade_whole = total.remade_whole.saturating_add(one.whole);
        }
        held.set(total);
    });
}

/// Adds the two halves of one `LeafRef::locate` to this thread's tally.
///
/// **Written straight into the tally rather than handed back**, because
/// `locate` is on a read view shared with recovery and the compaction's own
/// source, and threading an out parameter through it for a measurement would
/// change a function three callers share. It costs one more copy of the tally
/// through its `Cell` per write, which is paid only while a harness is
/// recording.
///
/// @param deltas - nanoseconds scanning the delta area
/// @param search - nanoseconds in the binary search of the sorted region
pub fn add_locate(deltas: u128, search: u128) {
    if !recording() {
        return;
    }
    PUT_STAGES.with(|held| {
        let mut total = held.get();
        total.deltas = total.deltas.saturating_add(deltas);
        total.search = total.search.saturating_add(search);
        held.set(total);
    });
}

/// Returns where the leaf writes since the last switch have gone.
pub fn taken() -> PutStages {
    PUT_STAGES.with(|held| held.get())
}

/// Starts or stops recording where leaf writes go, and clears the tally.
///
/// **A switch rather than a reset, because the recording costs something.** A
/// harness turns it on around the workload it wants the split for and off
/// again afterwards, so every other workload in the same process is measured on
/// the code an application runs. This is `ImportedDatabase::record_module_stages`
/// one layer down and it is a switch for the same reason.
///
/// @param on - whether to record from here
pub fn record_put_stages(on: bool) {
    PUT_STAGES.with(|held| held.set(PutStages::default()));
    RECORDING.with(|held| held.set(on));
}
