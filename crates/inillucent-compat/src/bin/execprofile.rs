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
//!   inillucent-execprofile <sqlite fixture> [--iterations N] [--page-size N]

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

/// The same read, run twice: chain rebuilt per execution, and chain reused.
///
/// **The prize, measured before anything is refactored to collect it.**
/// `physical::build_statement` already holds the operator chain across
/// executions and rebuilds only the source, and nothing in the engine's
/// execution path calls it. This runs both sides over the same plan, the same
/// prepared stages and the same parameters, so the difference is the chain build
/// and nothing else.
///
/// @param fixture - the pristine database
/// @param iterations - how many executions to average over
/// @param page_size - the page size to import at
fn reused_chain(fixture: &Path, iterations: u32, page_size: usize) -> Result<(), String> {
    use inillucent_exec::physical;
    let copy = restore(fixture, "reuse")?;
    let database = ImportedDatabase::import_with(copy, page_size, 4_096)
        .map_err(|error| format!("import failed: {error:?}"))?;
    println!();
    println!("## the same statement, chain rebuilt against chain reused");
    println!(
        "  {:<40} {:>10} {:>10} {:>9}",
        "statement", "rebuilt", "reused", "saved"
    );
    for sql in [
        "SELECT 1",
        "SELECT label FROM main_table WHERE id = ?1",
        "SELECT sum(length(label)) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200",
    ] {
        let plan = match database.plan(sql) {
            Ok(plan) => plan,
            Err(error) => {
                println!("  {sql:<40} plan refused: {error:?}");
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
        // **One closure per arm, and each arm is run twice.** The first version
        // of this ran the rebuilt arm and then the reused one over a freshly
        // imported database, and reported a point lookup at 4,840 ns rebuilt
        // against 840 reused. Most of that was the **pool**: the first arm paid
        // every page fault and the second inherited a warm cache. Both arms now
        // get a warm pass of their own, and the whole pair runs twice with the
        // order reversed, so a warming advantage cancels instead of being
        // reported as a speed-up.
        let run_rebuilt = |count: u32| -> Result<f64, String> {
            let started = Instant::now();
            for iteration in 0..count {
                let sink = Box::new(inillucent_exec::ops::CollectInto::new(std::rc::Rc::new(
                    std::cell::RefCell::new(Vec::new()),
                )));
                let (mut pipeline, _) =
                    physical::build_prepared(&plan, &database, &prepared, &binds(iteration), sink)
                        .map_err(|error| format!("{sql}: {error:?}"))?;
                pipeline
                    .run()
                    .map_err(|error| format!("{sql}: {error:?}"))?;
            }
            Ok(started.elapsed().as_nanos() as f64 / f64::from(count.max(1)))
        };
        let warm = iterations.clamp(1, 500);
        run_rebuilt(warm)?;
        let rebuilt_first = run_rebuilt(iterations)?;

        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut statement = physical::build_statement(
            &plan,
            &database,
            &prepared,
            &binds(0),
            Box::new(inillucent_exec::ops::CollectInto::new(std::rc::Rc::clone(
                &collected,
            ))),
        )
        .map_err(|error| format!("{sql}: {error:?}"))?;
        if !statement.rebindable() {
            println!("  {sql:<40} the chain kept a parameter and refuses to re-run");
            continue;
        }
        let mut run_reused = |count: u32| -> Result<f64, String> {
            let started = Instant::now();
            for iteration in 0..count {
                collected.borrow_mut().clear();
                statement
                    .run(&binds(iteration))
                    .map_err(|error| format!("{sql}: {error:?}"))?;
            }
            Ok(started.elapsed().as_nanos() as f64 / f64::from(count.max(1)))
        };
        run_reused(warm)?;
        let reused_second = run_reused(iterations)?;
        let reused_first = run_reused(iterations)?;
        let rebuilt_second = run_rebuilt(iterations)?;
        let rebuilt = rebuilt_first.min(rebuilt_second);
        let reused = reused_first.min(reused_second);
        println!(
            "  {:<40} {:>9.0}ns {:>9.0}ns {:>8.0}%",
            sql,
            rebuilt,
            reused,
            (rebuilt / reused.max(1.0) - 1.0) * 100.0
        );
    }
    Ok(())
}
