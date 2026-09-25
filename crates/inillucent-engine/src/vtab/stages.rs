//! Where a write into a virtual table spends its time.
//!
//! Invariant: **nothing here records unless a harness turned it on.** The
//! counters time every shadow row write and every pass through the insert arm,
//! and always-on they took `extension.rtree.insert` from a paired 1.29x to 1.10x
//! over two gate runs each (task-2025). The gate is the binary that decides
//! whether a family cleared its bar, so a default that records is a gate
//! measuring instrumented code and publishing the number as the engine's.
//! `crates/inillucent-compat/tests/engine/module_stages.rs` asserts the default.
//!
//! On the harness's side like [`crate::StageTimings`], and nothing in the engine
//! reads what it writes.

use crate::ImportedDatabase;

/// Where a write into a virtual table spends its time, in nanoseconds.
///
/// **Because the module's own breakdown accounted for less than half of the
/// workload (task-2025).** `inillucent_ext::vtab::fts5::BuildStages::whole` times
/// the whole of `Fts5Table::add` and read 3.9 ms of `extension.fts.build`'s
/// 8.07 ms; the other 4.2 ms was in the engine, between `execute_statement` and
/// that call, and no number said where. Naming the pieces here is what turns
/// "everything else" into a stage a change can be aimed at, and what it turned
/// out to be was the transaction's own commit rather than any of this.
///
/// `change` minus `update` is the plumbing `change_module` builds per row - the
/// map removal, the `WalLog`, the `WriteStore`, the `Context` - and `whole`
/// minus `values` minus `change` is the rest of the insert arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModuleStages {
    /// How many rows the insert arm handed to a module.
    pub rows: u64,
    /// The whole of one row's pass through the insert arm.
    pub whole: u128,
    /// Building the row's values: the owned copies, the column map, the rowid.
    pub values: u128,
    /// The whole of one `change_module`, per call.
    pub change: u128,
    /// The module's own `update`, inside `change_module`.
    pub update: u128,
    /// How many rows the module wrote into one of its shadow tables.
    pub shadow_writes: u64,
    /// Borrowing a module's row as the tree's own data, before the tree sees it.
    pub datums: u128,
    /// The tree write itself, once the row is borrowed.
    pub put: u128,
}

thread_local! {
    /// Where this thread's virtual table writes have spent their time.
    ///
    /// **On the thread rather than on the connection, because the two halves
    /// are measured in different places.** The insert arm and `change_module`
    /// have a `&mut ImportedDatabase` to hand; `WriteStore::write_row` does not
    /// - it is a borrow of three of the database's fields, handed to a module,
    /// and it is where a shadow row is actually written. A module's own stage
    /// timings are kept this way for the same reason.
    static MODULE_STAGES: std::cell::Cell<ModuleStages> =
        const { std::cell::Cell::new(ModuleStages {
            rows: 0,
            whole: 0,
            values: 0,
            change: 0,
            update: 0,
            shadow_writes: 0,
            datums: 0,
            put: 0,
        }) };

    /// Whether this thread is recording where its virtual table writes go.
    ///
    /// **Off, and it has to be off, for the reason this module's own invariant
    /// gives.** Eight `Instant::now` calls a row is nothing against
    /// `extension.fts.build`'s 15 us a document and it is 15% of
    /// `extension.rtree.insert`'s 4.5. This is read at every timed site, so a
    /// build nobody is measuring takes one thread-local `bool` and never reads a
    /// clock.
    static RECORDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reports whether this thread is recording virtual table write stages.
pub(crate) fn recording() -> bool {
    RECORDING.with(|held| held.get())
}

/// Returns a clock, but only while this thread is recording.
///
/// So an unmeasured write pays a `bool` rather than a `QueryPerformanceCounter`.
pub(crate) fn clock() -> Option<std::time::Instant> {
    recording().then(std::time::Instant::now)
}

/// Returns the nanoseconds since a clock, or zero when there was none.
///
/// @param started - the clock [`clock`] handed out, if it handed one out
pub(crate) fn elapsed(started: Option<std::time::Instant>) -> u128 {
    started.map_or(0, |at| at.elapsed().as_nanos())
}

/// Adds one measurement to what this thread's virtual table writes have spent.
///
/// Does nothing when this thread is not recording, so a caller may build its
/// numbers unconditionally and pay nothing for them.
///
/// @param edit - what to add
pub(crate) fn record(edit: impl FnOnce(&mut ModuleStages)) {
    if !recording() {
        return;
    }
    MODULE_STAGES.with(|held| {
        let mut stages = held.get();
        edit(&mut stages);
        held.set(stages);
    });
}

impl ImportedDatabase {
    /// Returns where the virtual table writes since the last switch have gone.
    pub fn module_stage_nanos(&self) -> ModuleStages {
        MODULE_STAGES.with(|held| held.get())
    }

    /// Starts or stops recording where the virtual table writes go, and clears the tally.
    ///
    /// **A switch rather than a reset, because the recording costs something.**
    /// `inillucent_ext::vtab::fts5::reset_build_stages` on the module's side can
    /// be a reset: it clears counters that are always kept. These are not always
    /// kept - see this module's invariant - so a harness turns them on around the
    /// workload it wants the split for and off again afterwards, and every other
    /// workload in the same process is measured on the code an application runs.
    ///
    /// @param on - whether to record from here
    pub fn record_module_stages(&self, on: bool) {
        MODULE_STAGES.with(|held| held.set(ModuleStages::default()));
        RECORDING.with(|held| held.set(on));
    }
}
