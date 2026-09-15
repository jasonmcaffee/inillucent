//! What one *already prepared* statement costs to run, and how much of it is
//! `malloc`.
//!
//! Invariant: nothing here compiles anything inside the clock. `prepareprofile`
//! answers "what does a compile cost"; this answers the question the gate's
//! write and transaction families actually turn on, which is what a statement
//! costs on the two thousandth execution of it, with the plan already in hand.
//!
//! ## Why it exists
//!
//! Ablating the tree write out of `update_in_place` entirely - the statement
//! found its row, decided what to write, and then returned without
//! writing - showed `txn.large`'s apply time did not move: **1.54 microseconds
//! against 1.53**. So none of the gap that put the `transaction` family under
//! its floor is in the storage engine, and every hour spent on leaves and delta
//! areas was going to buy nothing. It is in the executor, ahead of the tree, and
//! measuring it needed a profile that could see inside one execution.
//!
//! The allocation counter is the part that says what to fix. Nanoseconds say
//! which statement is slow; allocations say why, and on this box a compile was
//! already known to be mostly the C runtime's heap: 59% of a trivial compile.
//!
//! Usage:
//!   inillucent-execprofile `<sqlite fixture>` [--iterations N] [--page-size N]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// How many times each statement runs before its total is divided.
const DEFAULT_ITERATIONS: u32 = 2_000;

/// How many rows `main_table` holds in the medium fixture, which is what the
/// gate's binds scatter over.
const ROWS: u64 = 100_000;

/// How many allocations the process has made.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// How many bytes those allocations asked for.
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Whether allocations are being attributed to a call site right now.
    ///
    /// **Off inside the allocator itself.** Capturing a backtrace allocates, so
    /// an unguarded capture recurses until the stack runs out. The flag is set
    /// while a capture is in progress and every allocation made during it is
    /// counted and otherwise ignored.
    static CAPTURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the attributing pass is running.
    static ATTRIBUTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// One line per distinct call site, with how many allocations it made.
    static SITES: std::cell::RefCell<std::collections::BTreeMap<String, u64>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Records the call site of one allocation, when attribution is switched on.
///
/// The frame kept is the first one inside this workspace that is not the
/// allocator or the standard library's own container code, because "a `Vec`
/// grew" is true of every allocation and says nothing about which one to stop
/// making.
fn attribute() {
    if !ATTRIBUTING.with(std::cell::Cell::get) || CAPTURING.with(std::cell::Cell::get) {
        return;
    }
    CAPTURING.with(|held| held.set(true));
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    // **Two frames, not one.** The first pass of this printed
    // `physical::impl$10::clone` twelve times for one `UPDATE` and there is no
    // way to act on that: the question is which caller clones twelve things.
    let mut kept: Vec<String> = Vec::new();
    for line in trace.lines() {
        let line = line.trim();
        if !line.contains("inillucent") {
            continue;
        }
        if line.contains("execprofile") || line.contains("CountingAllocator") {
            continue;
        }
        if let Some(at) = line.find(": ") {
            let frame = line.get(at.saturating_add(2)..).unwrap_or(line);
            let frame = frame.split("::closure").next().unwrap_or(frame);
            if kept.last().map(String::as_str) != Some(frame) {
                kept.push(frame.to_string());
            }
            if kept.len() == 3 {
                break;
            }
        }
    }
    let chosen = if kept.is_empty() {
        String::from("(unattributed)")
    } else {
        kept.join("  <-  ")
    };
    SITES.with(|held| {
        *held.borrow_mut().entry(chosen).or_insert(0) += 1;
    });
    CAPTURING.with(|held| held.set(false));
}

/// An allocator that counts, and otherwise delegates to the system one.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same
// arguments; the counters are the only addition and they touch no memory the
// allocator owns.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the layout is the caller's, forwarded unchanged.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        attribute();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.dealloc(pointer, layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One statement to profile, and how to bind it.
struct Case {
    /// What to print it as.
    name: &'static str,
    /// The statement.
    sql: &'static str,
    /// How to build the parameters for one iteration.
    binds: fn(u32) -> Vec<OwnedDatum>,
    /// Whether every iteration runs inside one open transaction.
    batched: bool,
}

/// The scattered rowid the gate's `Bind::Scatter` produces.
///
/// @param iteration - which iteration
fn scatter(iteration: u32) -> i64 {
    1 + (u64::from(iteration).wrapping_mul(2_654_435_761) % ROWS) as i64
}

/// The text the gate's `Bind::Text` produces.
///
/// @param iteration - which iteration
fn text(iteration: u32) -> OwnedDatum {
    OwnedDatum::Text(format!("row {iteration} lorem ipsum dolor sit amet consectetur").into_bytes())
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-execprofile <sqlite fixture> [--iterations N] [--page-size N]"
        );
        return ExitCode::from(2);
    };
    let iterations = flag(&arguments, "--iterations")
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let page_size = flag(&arguments, "--page-size")
        .and_then(|value| value.parse().ok())
        .unwrap_or(32_768usize);
    match run(Path::new(fixture), iterations, page_size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("execprofile: {why}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value that follows a flag, when it is there.
///
/// @param arguments - the command line
/// @param name - the flag to look for
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1)).cloned()
}

/// Copies the fixture so each case starts from the same bytes.
///
/// @param fixture - the pristine database
/// @param tag - what to call the copy
fn restore(fixture: &Path, tag: &str) -> Result<PathBuf, String> {
    let directory = inillucent_compat::workspace_root().join("_agent_output/execprofile");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let target = directory.join(format!("{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&target);
    std::fs::copy(fixture, &target).map_err(|error| format!("could not restore: {error}"))?;
    Ok(target)
}

/// Profiles every case and prints one row each.
///
/// @param fixture - the pristine database
/// @param iterations - how many executions to average over
/// @param page_size - the page size to import at
fn run(fixture: &Path, iterations: u32, page_size: usize) -> Result<(), String> {
    let cases = [
        Case {
            name: "txn.large (UPDATE, in txn)",
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1",
            binds: |iteration| vec![OwnedDatum::Int(scatter(iteration)), text(iteration)],
            batched: true,
        },
        Case {
            name: "txn.large, no row matches",
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1",
            binds: |iteration| {
                vec![
                    OwnedDatum::Int(90_000 + i64::from(iteration % 1_000)),
                    text(iteration),
                ]
            },
            batched: true,
        },
        Case {
            name: "point.rowid (SELECT)",
            sql: "SELECT label FROM main_table WHERE id = ?1",
            binds: |iteration| vec![OwnedDatum::Int(scatter(iteration))],
            batched: false,
        },
        Case {
            name: "SELECT 1",
            sql: "SELECT 1",
            binds: |_| Vec::new(),
            batched: false,
        },
    ];

    println!("## one prepared statement, executed {iterations} times");
    println!(
        "  {:<32} {:>10} {:>10} {:>12} {:>12}",
        "case", "ns each", "allocs", "alloc bytes", "total ms"
    );
    for case in &cases {
        let copy = restore(
            fixture,
            case.name.split_whitespace().next().unwrap_or("case"),
        )?;
        let mut database = ImportedDatabase::import_with(copy, page_size, 4_096)
            .map_err(|error| format!("import failed: {error:?}"))?;
        let statement = database
            .prepare_statement(case.sql)
            .map_err(|error| format!("{}: {error:?}", case.sql))?;
        // **No warm pass on a batched case, and the commit is inside the
        // clock.** The gate opens one transaction over a freshly restored
        // fixture, runs every statement in it, and commits - so a warm pass
        // would have already grown the rows this workload grows, turning every
        // measured write into the cheapest possible one, and leaving the commit
        // out would drop whatever the commit costs. Both were happening: the
        // profile read 1,489 ns a statement where the gate read 3,384.
        if !case.batched {
            for iteration in 0..iterations.min(50) {
                let params = Params::from_values((case.binds)(iteration));
                let _ = database.execute_timed(&statement, &params);
            }
        }
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
        let started = Instant::now();
        if case.batched {
            database.begin_batch();
        }
        for iteration in 0..iterations {
            let params = Params::from_values((case.binds)(iteration));
            database
                .execute_timed(&statement, &params)
                .map_err(|error| format!("{}: {error:?}", case.sql))?;
        }
        let statements = started.elapsed();
        if case.batched {
            database
                .commit_batch()
                .map_err(|error| format!("the commit: {error:?}"))?;
        }
        let elapsed = started.elapsed();
        if case.batched {
            println!(
                "      of which the commit: {:.2} ms of {:.2}",
                (elapsed.saturating_sub(statements)).as_nanos() as f64 / 1_000_000.0,
                elapsed.as_nanos() as f64 / 1_000_000.0,
            );
            database.begin_batch();
        }
        let allocations = ALLOCATIONS
            .load(Ordering::Relaxed)
            .saturating_sub(allocations);
        let bytes = ALLOCATED_BYTES
            .load(Ordering::Relaxed)
            .saturating_sub(bytes);
        let each = f64::from(iterations.max(1));
        println!(
            "  {:<32} {:>10.1} {:>10.2} {:>12.1} {:>12.2}",
            case.name,
            elapsed.as_nanos() as f64 / each,
            allocations as f64 / each,
            bytes as f64 / each,
            elapsed.as_nanos() as f64 / 1_000_000.0,
        );
        // **One execution, with every allocation attributed to the frame that
        // made it.** The count alone says a statement allocates thirty times;
        // it does not say which thirty lines to stop calling.
        SITES.with(|held| held.borrow_mut().clear());
        ATTRIBUTING.with(|held| held.set(true));
        let params = Params::from_values((case.binds)(iterations.saturating_add(1)));
        let _ = database.execute_timed(&statement, &params);
        ATTRIBUTING.with(|held| held.set(false));
        let mut sites: Vec<(String, u64)> =
            SITES.with(|held| held.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect());
        sites.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for (site, count) in sites.iter().take(12) {
            println!("      {count:>4}  {site}");
        }
        if case.batched {
            let _ = database.commit_batch();
        }
    }
    reused_chain(fixture, iterations, page_size)?;
    second_pass(fixture, iterations, page_size)?;
    paired_write_range_update(fixture, page_size)?;
    Ok(())
}

/// The gate's own ordering: the same rows updated to the same values twice.
///
/// **Because the gate and this profile disagreed by two and a half times, and
/// one of them had to be wrong about what it was measuring.** `fullgate` runs a
/// round's workloads in order against one database, and `txn.batched` and
/// `txn.large` are the same statement over the same scattered rowids with the
/// same `row {iteration} lorem ipsum ...` text - so by the time `txn.large`
/// runs, every row it touches already holds the bytes it is about to write.
/// This runs the loop twice and prints both, so the difference between "an
/// update that changes a value" and "an update that writes back what is already
/// there" is a number rather than an argument.
///
/// @param fixture - the pristine database
/// @param iterations - how many executions per pass
/// @param page_size - the page size to import at
fn second_pass(fixture: &Path, iterations: u32, page_size: usize) -> Result<(), String> {
    let copy = restore(fixture, "second")?;
    let mut database = ImportedDatabase::import_with(copy, page_size, 4_096)
        .map_err(|error| format!("import failed: {error:?}"))?;
    let sql = "UPDATE side_table SET note = ?2 WHERE id = ?1";
    let statement = database
        .prepare_statement(sql)
        .map_err(|error| format!("{sql}: {error:?}"))?;
    println!();
    println!("## the same update, run twice over the same rows");
    println!(
        "  {:<28} {:>10} {:>10} {:>9} {:>9} {:>9}",
        "pass", "ns each", "allocs", "inserted", "inplace", "compacted"
    );
    for pass in ["first, values differ", "second, values identical"] {
        let before = database.write_stats();
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        database.begin_batch();
        for iteration in 0..iterations {
            let params =
                Params::from_values(vec![OwnedDatum::Int(scatter(iteration)), text(iteration)]);
            database
                .execute_timed(&statement, &params)
                .map_err(|error| format!("{sql}: {error:?}"))?;
        }
        database
            .commit_batch()
            .map_err(|error| format!("the commit: {error:?}"))?;
        let elapsed = started.elapsed();
        let after = database.write_stats();
        let each = f64::from(iterations.max(1));
        let per = |now: u64, was: u64| (now.saturating_sub(was) as f64) / each;
        println!(
            "  {:<28} {:>10.1} {:>10.2} {:>9.2} {:>9.2} {:>9.3}",
            pass,
            elapsed.as_nanos() as f64 / each,
            (ALLOCATIONS
                .load(Ordering::Relaxed)
                .saturating_sub(allocations)) as f64
                / each,
            per(after.inserted, before.inserted),
            per(after.updated_in_place, before.updated_in_place),
            per(after.compactions, before.compactions),
        );
    }
    Ok(())
}

/// How many executions make up one block of the paired measurement.
///
/// @see [`reused_chain`]
const PAIRED_BLOCK: u32 = 200;

/// How many blocks of each arm the paired measurement runs, order swapped
/// every round.
///
/// @see [`reused_chain`]
const PAIRED_ROUNDS: u32 = 40;

/// Returns the middle value of a list, sorting it in place.
///
/// The even-length case averages the two middle values, which is the
/// ordinary definition and matters here because `PAIRED_ROUNDS` is even.
///
/// @param values - the numbers to find the median of
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = values.len();
    if len == 0 {
        return 0.0;
    }
    if len % 2 == 1 {
        values[len / 2]
    } else {
        (values[len / 2 - 1] + values[len / 2]) / 2.0
    }
}

/// The same read, run two ways: chain rebuilt per execution, and chain reused
/// - **paired**, because run sequentially the two arms disagree with
/// themselves.
///
/// **The sequential version was not trustworthy at every size.** It ran the
/// rebuilt arm to completion and then the reused arm to completion, twice each
/// with the order reversed to cancel a warming advantage - and that is still
/// wrong when the box itself moves during the run rather than only at the
/// start. Measured here on a 200-row range scan, the two arms disagreed by
/// about 30% between otherwise identical runs, which is more than the 11-14%
/// the sequential numbers reported as the reused arm's saving. A number that
/// size cannot be told apart from the machine's own noise by running each arm
/// once.
///
/// So the two arms are interleaved: each round runs one block of
/// [`PAIRED_BLOCK`] executions of one arm and then one block of the other,
/// both blocks over the **same key sequence** so neither arm gets an easier
/// set of rowids, and which arm goes first alternates every round so a
/// systematic first-mover effect (a page fault, a branch predictor warming up)
/// falls on both arms equally across the run. [`PAIRED_ROUNDS`] rounds are run
/// and the reported "saved" is the **median of the per-round differences**,
/// with a count of how many of the rounds the reused arm actually won - the
/// two numbers a reader needs to tell a real effect from a coin flip.
///
/// **The reused arm is `physical::try_compile` and `physical::Compiled::run`
/// - what `Cached::Select` actually calls - not `physical::Statement`.**
/// `Statement<'t>` could already hold a whole join tower across executions
/// before roadmap item 3 existed, because it borrows the catalog for `'t`;
/// measuring it would report what was already possible in the crate rather
/// than what the engine's cache now ships. A shape `try_compile` refuses -
/// none, once Stage 2 added the join tower - is skipped with a line saying so
/// rather than silently measuring nothing.
///
/// @param fixture - the pristine database
/// @param iterations - how many executions the warm-up pass uses
/// @param page_size - the page size to import at
fn reused_chain(fixture: &Path, iterations: u32, page_size: usize) -> Result<(), String> {
    use inillucent_exec::physical;
    let copy = restore(fixture, "reuse")?;
    let database = ImportedDatabase::import_with(copy, page_size, 4_096)
        .map_err(|error| format!("import failed: {error:?}"))?;
    println!();
    println!(
        "## the same statement, chain rebuilt against chain reused (paired, {PAIRED_ROUNDS} \
         rounds of {PAIRED_BLOCK}, arm order swapped every round)"
    );
    println!(
        "  {:<62} {:>10} {:>10} {:>7} {:>10}",
        "statement", "rebuilt", "reused", "saved", "rounds won"
    );
    for sql in [
        "SELECT 1",
        "SELECT label FROM main_table WHERE id = ?1",
        "SELECT sum(length(label)) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200",
        "SELECT count(key) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200",
        "SELECT count(*) FROM main_table JOIN side_table ON side_table.owner = \
         main_table.id WHERE main_table.id = ?1",
    ] {
        let plan = match database.plan(sql) {
            Ok(plan) => plan,
            Err(error) => {
                println!("  {sql:<62} plan refused: {error:?}");
                continue;
            }
        };
        let prepared = physical::prepare(&plan, &database, physical::ForcePlan::default())
            .map_err(|error| format!("{sql}: {error:?}"))?;
        let binds = |iteration: u32| {
            if sql.contains("?1") {
                Params::from_values(vec![OwnedDatum::Int(scatter(iteration))])
            } else {
                Params::new()
            }
        };
        // One block of `count` executions of the rebuilt arm, starting at key
        // `base` in the same scattered sequence every arm reads.
        let run_rebuilt_block = |base: u32, count: u32| -> Result<f64, String> {
            let started = Instant::now();
            for offset in 0..count {
                let sink = Box::new(inillucent_exec::ops::CollectInto::new(std::rc::Rc::new(
                    std::cell::RefCell::new(Vec::new()),
                )));
                let (mut pipeline, _) = physical::build_prepared(
                    &plan,
                    &database,
                    &prepared,
                    &binds(base.wrapping_add(offset)),
                    sink,
                )
                .map_err(|error| format!("{sql}: {error:?}"))?;
                pipeline
                    .run()
                    .map_err(|error| format!("{sql}: {error:?}"))?;
            }
            Ok(started.elapsed().as_nanos() as f64 / f64::from(count.max(1)))
        };

        // **The reused arm is `physical::Compiled`, not `physical::Statement`.**
        // `Statement<'t>` already held a chain across executions before this
        // ticket and could already build the whole join tower once, because
        // it borrows the catalog for `'t` - so measuring it would report what
        // was already possible rather than what `Cached::Select`'s
        // `physical::Slot` actually ships. `try_compile` is the function the
        // engine calls on a cache miss; measuring its own `Compiled::run` is
        // what makes this number the one to trust for roadmap item 3.
        let mut compiled = match physical::try_compile(&plan, &database, &prepared, &binds(0)) {
            Ok(Some(compiled)) => compiled,
            Ok(None) => {
                println!("  {sql:<62} not eligible for a compiled chain (try_compile refused)");
                continue;
            }
            Err(error) => return Err(format!("{sql}: {error:?}")),
        };
        // One block of `count` executions of the reused arm, over the same key
        // sequence as `run_rebuilt_block`.
        let mut run_reused_block = |base: u32, count: u32| -> Result<f64, String> {
            let started = Instant::now();
            for offset in 0..count {
                compiled
                    .run(&plan, &database, &binds(base.wrapping_add(offset)))
                    .map_err(|error| format!("{sql}: {error:?}"))?;
            }
            Ok(started.elapsed().as_nanos() as f64 / f64::from(count.max(1)))
        };

        // A throwaway warm-up over both arms, before any round is measured,
        // so the pages both arms read are already in the pool going in.
        let warm = PAIRED_BLOCK.min(iterations.max(1));
        run_rebuilt_block(0, warm)?;
        run_reused_block(0, warm)?;

        let mut rebuilt_ns = Vec::with_capacity(PAIRED_ROUNDS as usize);
        let mut reused_ns = Vec::with_capacity(PAIRED_ROUNDS as usize);
        let mut saved_pct = Vec::with_capacity(PAIRED_ROUNDS as usize);
        let mut reused_faster = 0u32;
        for round in 0..PAIRED_ROUNDS {
            let base = round.saturating_mul(PAIRED_BLOCK);
            let (rebuilt, reused) = if round % 2 == 0 {
                let rebuilt = run_rebuilt_block(base, PAIRED_BLOCK)?;
                let reused = run_reused_block(base, PAIRED_BLOCK)?;
                (rebuilt, reused)
            } else {
                let reused = run_reused_block(base, PAIRED_BLOCK)?;
                let rebuilt = run_rebuilt_block(base, PAIRED_BLOCK)?;
                (rebuilt, reused)
            };
            if reused < rebuilt {
                reused_faster = reused_faster.saturating_add(1);
            }
            saved_pct.push((rebuilt - reused) / rebuilt.max(1.0) * 100.0);
            rebuilt_ns.push(rebuilt);
            reused_ns.push(reused);
        }
        println!(
            "  {:<62} {:>8.0}ns {:>8.0}ns {:>6.0}% {:>6} of {}",
            sql,
            median(&mut rebuilt_ns),
            median(&mut reused_ns),
            median(&mut saved_pct),
            reused_faster,
            PAIRED_ROUNDS,
        );
    }
    Ok(())
}

/// Stage 3, paired the same way as the read side: the same `UPDATE` with a
/// range `WHERE`, run through a connection whose `Cached::Update` is never
/// reused (`Levers::PLAN_CACHE` off, so every `execute_any` call compiles a
/// fresh one) against one where it is (the default).
///
/// **A range, not a rowid equality, on purpose.** `WHERE id = ?1` is answered
/// by `physical::rowid_seek_key` before `CachedQuery`'s slot is ever asked -
/// that shape was already free - so this measures `WHERE id BETWEEN ?1 AND
/// ?1 + 5` instead, the shape Stage 3 actually changed anything for.
///
/// **Two database copies, not two arms over one file.** A write is not
/// idempotent the way a read is: running the same `UPDATE` twice over the
/// same file is two different questions, not the same question asked twice.
/// So the "rebuilt" and "reused" arms are two separate copies of the fixture,
/// each fed the *same* scattered key sequence and each accumulating its own
/// writes - which is still a fair pairing, because what is being compared is
/// the cost of the statement, not the cost of undoing it, and the block
/// order still swaps every round so neither database's own cache state (a
/// warmer pool, a page split just paid for) always lands on the same arm.
///
/// `write.delete` is not measured here: a paired `DELETE` shrinks its own
/// table round by round in a way an `UPDATE` does not, and making that
/// comparable (re-inserting between rounds, or bounding a delete to rows that
/// no longer exist) is a harness of its own rather than a small addition to
/// this one - left undone rather than rushed.
///
/// @param fixture - the pristine database
/// @param page_size - the page size to import at
fn paired_write_range_update(fixture: &Path, page_size: usize) -> Result<(), String> {
    println!();
    println!(
        "## the same UPDATE with a range WHERE, chain rebuilt against chain reused (paired, \
         {PAIRED_ROUNDS} rounds of {PAIRED_BLOCK}, arm order swapped every round)"
    );
    println!(
        "  {:<62} {:>10} {:>10} {:>7} {:>10}",
        "statement", "rebuilt", "reused", "saved", "rounds won"
    );
    let sql = "UPDATE side_table SET note = ?2 WHERE id BETWEEN ?1 AND ?1 + 5";

    let rebuilt_copy = restore(fixture, "write-rebuilt")?;
    let mut rebuilt_db = ImportedDatabase::import_with(rebuilt_copy, page_size, 4_096)
        .map_err(|error| format!("import failed: {error:?}"))?;
    rebuilt_db.disable_optimizations(inillucent_sql::plan::Levers::without(
        inillucent_sql::plan::Levers::PLAN_CACHE,
    ));

    let reused_copy = restore(fixture, "write-reused")?;
    let mut reused_db = ImportedDatabase::import_with(reused_copy, page_size, 4_096)
        .map_err(|error| format!("import failed: {error:?}"))?;

    let binds = |iteration: u32| vec![OwnedDatum::Int(scatter(iteration)), text(iteration)];

    // One block of `count` autocommit executions, starting at key `base` in
    // the scattered sequence both databases read. Autocommit, not a batch,
    // because `write.update.indexed`'s own grouping is one transaction per
    // statement - the shape this is meant to stand in for.
    let run_block =
        |database: &mut ImportedDatabase, base: u32, count: u32| -> Result<f64, String> {
            let started = Instant::now();
            for offset in 0..count {
                let params = Params::from_values(binds(base.wrapping_add(offset)));
                database
                    .execute_any(sql, &params)
                    .map_err(|error| format!("{sql}: {error:?}"))?;
            }
            Ok(started.elapsed().as_nanos() as f64 / f64::from(count.max(1)))
        };

    let warm = PAIRED_BLOCK;
    run_block(&mut rebuilt_db, 0, warm)?;
    run_block(&mut reused_db, 0, warm)?;

    let mut rebuilt_ns = Vec::with_capacity(PAIRED_ROUNDS as usize);
    let mut reused_ns = Vec::with_capacity(PAIRED_ROUNDS as usize);
    let mut saved_pct = Vec::with_capacity(PAIRED_ROUNDS as usize);
    let mut reused_faster = 0u32;
    for round in 0..PAIRED_ROUNDS {
        let base = round.saturating_mul(PAIRED_BLOCK);
        let (rebuilt, reused) = if round % 2 == 0 {
            let rebuilt = run_block(&mut rebuilt_db, base, PAIRED_BLOCK)?;
            let reused = run_block(&mut reused_db, base, PAIRED_BLOCK)?;
            (rebuilt, reused)
        } else {
            let reused = run_block(&mut reused_db, base, PAIRED_BLOCK)?;
            let rebuilt = run_block(&mut rebuilt_db, base, PAIRED_BLOCK)?;
            (rebuilt, reused)
        };
        if reused < rebuilt {
            reused_faster = reused_faster.saturating_add(1);
        }
        saved_pct.push((rebuilt - reused) / rebuilt.max(1.0) * 100.0);
        rebuilt_ns.push(rebuilt);
        reused_ns.push(reused);
    }
    println!(
        "  {:<62} {:>8.0}ns {:>8.0}ns {:>6.0}% {:>6} of {}",
        sql,
        median(&mut rebuilt_ns),
        median(&mut reused_ns),
        median(&mut saved_pct),
        reused_faster,
        PAIRED_ROUNDS,
    );
    Ok(())
}
