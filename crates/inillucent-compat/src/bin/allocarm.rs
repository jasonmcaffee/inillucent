//! The gate's engine arm, run under a different allocator.
//!
//! Invariant: this is a **measurement arm, not a product**. The TDD's Part 5
//! asks whether the engine's Linux numbers are held back by the system
//! allocator, and the only way to answer that is to run the same workloads with
//! a different one and compare. Nothing this binary does reaches the engine, the
//! gate, or any shipped path: it replaces `GlobalAlloc` for its own process and
//! runs the plan.
//!
//! The allocator is a **size-classed free list over the system allocator**,
//! not a bump arena. A bump arena that never frees is the shortest thing to
//! write and the wrong thing to measure: thirty rounds of the medium plan would
//! grow it without bound and the number it produced would be about page faults
//! rather than about `malloc`. A free list recycles, which is what an allocator
//! does, and it removes exactly what was in question - the size lookup, the
//! locking and the bookkeeping glibc does per call.
//!
//! Usage:
//!   inillucent-allocarm `<sqlite fixture>` [--rounds N] [--scale S] [--system]
//!
//! `--system` runs the identical loop on the system allocator, so the pair is
//! one binary and one code path with one thing changed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::plan_for;
use inillucent_exec::physical::Params;

/// The largest allocation the free list handles itself.
///
/// Above this the system allocator is asked directly: a big block is rare, and
/// keeping a free list of them would be a cache of things nothing asks for
/// twice.
const LARGEST: usize = 4_096;

/// How many size classes the free list holds.
///
/// One per 16 bytes up to `LARGEST`, which is the granularity a `Vec<u8>` of a
/// row or a key actually lands on.
const CLASSES: usize = LARGEST / 16 + 1;

/// Whether the free list is in use, read once per allocation.
static POOLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// The per-class free lists, as raw pointers this thread owns.
    static FREE: RefCell<Vec<Vec<*mut u8>>> = RefCell::new(vec![Vec::new(); CLASSES]);
}

/// Returns the size class an allocation of this layout falls in, if any.
///
/// `None` means "not ours": too large, or wanting an alignment the system
/// allocator's 16-byte guarantee does not cover.
///
/// @param layout - the allocation's layout
fn class_of(layout: Layout) -> Option<usize> {
    if layout.size() > LARGEST || layout.align() > 16 {
        return None;
    }
    Some(layout.size().div_ceil(16))
}

/// A size-classed free list over the system allocator.
struct PoolAllocator;

// SAFETY: every path either forwards to the system allocator unchanged, or
// hands back a block this allocator obtained from the system allocator for the
// same size class and has not handed out since. The class is derived from the
// layout on both sides, so a block is only ever reused for a request the block
// is large enough for.
unsafe impl GlobalAlloc for PoolAllocator {
    // SAFETY: the layout is the caller's. A pooled block was allocated by
    // `System.alloc` with the class's own layout, which is at least this
    // request's size and alignment.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if POOLED.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(class) = class_of(layout) {
                // `try_with`, because a thread tearing down has already
                // dropped its lists and a panic inside an allocator aborts the
                // process. A block that finds no list is simply a fresh one.
                let held = FREE
                    .try_with(|lists| match lists.try_borrow_mut() {
                        Ok(mut lists) => lists.get_mut(class).and_then(Vec::pop),
                        Err(_) => None,
                    })
                    .unwrap_or(None);
                if let Some(pointer) = held {
                    return pointer;
                }
                // SAFETY: a fresh block of the class's own layout, which is at
                // least as large and as aligned as the request.
                return unsafe { System.alloc(layout_of(class)) };
            }
        }
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    // SAFETY: the pointer and layout are the ones handed out above. A pooled
    // block goes back on its own class's list and is not freed; anything else
    // goes back to the system allocator with the layout it was made with.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if POOLED.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(class) = class_of(layout) {
                let kept = FREE
                    .try_with(|lists| match lists.try_borrow_mut() {
                        Ok(mut lists) => match lists.get_mut(class) {
                            Some(list) if list.len() < 1_024 => {
                                list.push(pointer);
                                true
                            }
                            _ => false,
                        },
                        Err(_) => false,
                    })
                    .unwrap_or(false);
                if kept {
                    return;
                }
                // SAFETY: freed with the layout it was allocated with above.
                unsafe { System.dealloc(pointer, layout_of(class)) };
                return;
            }
        }
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.dealloc(pointer, layout) }
    }
}

/// Returns the layout a size class's blocks are allocated with.
///
/// @param class - the size class
fn layout_of(class: usize) -> Layout {
    Layout::from_size_align(class.saturating_mul(16).max(16), 16)
        .unwrap_or_else(|_| Layout::new::<u128>())
}

#[global_allocator]
static ALLOCATOR: PoolAllocator = PoolAllocator;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-allocarm <sqlite fixture> [--rounds N] [--scale S] [--system]"
        );
        return ExitCode::from(2);
    };
    let system = arguments.iter().any(|value| value == "--system");
    POOLED.store(!system, std::sync::atomic::Ordering::Relaxed);
    let rounds: usize = flag(&arguments, "--rounds")
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    let scale = flag(&arguments, "--scale").unwrap_or_else(|| "medium".to_string());
    let mut plan = plan_for(&scale);
    plan.setup.clear();
    // The read families only, and only the ones that bind nothing. A write
    // workload changes the database it measures, so a loop over one copy would
    // be measuring a different table each round; a workload with a `?1` in it
    // would need the gate's binder, and this arm answers a question about the
    // allocator rather than about parameters. What is left is the scans, the
    // sorts and the joins - which is where the allocations are.
    plan.workloads.retain(|workload| {
        workload.binds.is_empty()
            && (workload.family.starts_with("read") || workload.family == "open.prepare")
    });
    println!("## allocator: {}", if system { "system" } else { "pooled" });
    println!("  scale : {scale}");
    println!("  rounds: {rounds}");
    let scratch = std::env::temp_dir().join(format!("inillucent-allocarm-{}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&scratch) {
        eprintln!("scratch: {error}");
        return ExitCode::FAILURE;
    }
    let copy = scratch.join("arm.db");
    if let Err(error) = std::fs::copy(fixture, &copy) {
        eprintln!("copy: {error}");
        return ExitCode::FAILURE;
    }
    let mut database = match ImportedDatabase::import_with(copy, 32_768, 4_096) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("import: {}", error.message());
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = database.warm() {
        eprintln!("warm: {}", error.message());
        return ExitCode::FAILURE;
    }
    let mut totals: Vec<(String, Vec<f64>)> = Vec::new();
    for workload in &plan.workloads {
        totals.push((workload.name.clone(), Vec::with_capacity(rounds)));
    }
    for _ in 0..rounds {
        for (index, workload) in plan.workloads.iter().enumerate() {
            let started = Instant::now();
            for iteration in 0..workload.repeat {
                let params = Params::new();
                let _ = iteration;
                if database.execute_any(&workload.sql, &params).is_err() {
                    break;
                }
            }
            let spent = started.elapsed().as_nanos() as f64 / 1e6;
            if let Some((_, samples)) = totals.get_mut(index) {
                samples.push(spent);
            }
        }
    }
    println!("  {:<24} {:>12}", "workload", "median ms");
    let mut whole = 0.0;
    for (name, samples) in &mut totals {
        samples.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        let middle = samples.get(samples.len() / 2).copied().unwrap_or(0.0);
        whole += middle;
        println!("  {name:<24} {middle:>12.2}");
    }
    println!("  {:<24} {:>12.2}", "TOTAL", whole);
    ExitCode::SUCCESS
}

/// Returns a flag's value, when it was given.
///
/// @param arguments - the command line
/// @param name - the flag, with its dashes
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1)).cloned()
}
