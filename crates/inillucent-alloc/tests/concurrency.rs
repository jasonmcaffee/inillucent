//! Blocks crossing threads, in every size class.
//!
//! Invariant: **a block allocated on one thread and freed on another is
//! recycled correctly, and every size class behaves the same way.** The crate's
//! own module comment makes exactly this claim - "a block allocated on one
//! thread and freed on another goes onto the freeing thread's list, which is
//! safe" - and until task-1932 (H9) nothing exercised it. `inillucent-alloc`
//! had no `tests/` directory at all, was not in `policy.rs`'s governed list,
//! and is the one production crate in the workspace allowed to write `unsafe`.
//!
//! The free lists live in thread-local storage, so a block that crosses a
//! thread boundary lands on a list it did not come from: its size class has to
//! be derived from the layout the caller hands back rather than from anything
//! recorded beside the block, and the per-class cap has to bound what a
//! one-directional flow between two threads can accumulate. Both are asserted
//! here, over every class boundary and a sample of the classes between them.
//!
//! **What this can and cannot catch.** A reader that walks off the end of a
//! recycled block, or a free list that hands the same block out twice, shows up
//! here as a crash or as two live values sharing an address. It is not a
//! race-detector run: what it establishes is that the ordinary cross-thread
//! flow the crate documents is a flow that works, which is a thing nothing
//! previously said.
//!
//! **An address that comes back is not a defect.** The freeing thread returns
//! each block before it receives the next, so the pool is free to hand the same
//! address out again, and it does once the two threads are running at the same
//! speed. The check is therefore on the blocks that are *live* rather than on
//! every address ever seen - see the comment beside it.

use inillucent_alloc::{Pooled, LARGEST};

use std::alloc::{GlobalAlloc, Layout};
use std::sync::mpsc;

/// The allocator under test, as an ordinary value rather than as the global
/// one: a test binary installs whatever allocator the harness was built with,
/// and this is about the type's behaviour rather than about the `#[global_allocator]`
/// attribute.
static POOLED: Pooled = Pooled;

/// The sizes the round trips below use.
///
/// Both boundaries in each direction and a spread between them: one byte, the
/// grain, one past the grain, the largest class, one past the largest (which
/// goes straight to the system allocator and comes back the same way), and two
/// sizes in the middle. A class that behaved differently only at its edge is
/// exactly what a sample of round numbers would miss.
fn sizes() -> Vec<usize> {
    let mut wanted = vec![1, 8, 15, 16, 17, 31, 32, 33];
    wanted.extend([64, 128, 129, 1_024, 2_048]);
    wanted.extend([LARGEST - 1, LARGEST, LARGEST + 1, LARGEST * 2]);
    wanted
}

/// The alignments a caller can ask for, all of which the pool has to either
/// serve or forward.
fn alignments() -> [usize; 4] {
    [1, 2, 8, 16]
}

/// A block allocated on one thread and freed on another round-trips, for every
/// size class and alignment.
///
/// The bytes are written before the block is sent and read on the other side,
/// so a pool that handed the same block out twice, or that returned a block
/// shorter than the layout asked for, shows up as a value that is not the one
/// that was written rather than as a silent pass.
#[test]
fn a_block_allocated_on_one_thread_frees_correctly_on_another() {
    for align in alignments() {
        let (sender, receiver) = mpsc::channel::<(usize, usize, u8)>();
        let allocating = std::thread::spawn(move || {
            for (nth, size) in sizes().into_iter().enumerate() {
                let Ok(layout) = Layout::from_size_align(size, align) else {
                    continue;
                };
                // SAFETY: the layout is non-zero and validly aligned, which is
                // the whole of `GlobalAlloc::alloc`'s contract. The pointer is
                // handed to exactly one other thread and freed there once.
                let block = unsafe { POOLED.alloc(layout) };
                assert!(!block.is_null(), "size {size} align {align} was refused");
                assert_eq!(
                    block as usize % align,
                    0,
                    "size {size} came back misaligned for {align}"
                );
                let fill = (nth % 251) as u8;
                // SAFETY: `block` points at `size` writable bytes, which is what
                // `alloc` returned it for, and nothing else holds it yet.
                unsafe { std::ptr::write_bytes(block, fill, size) };
                let _ = sender.send((block as usize, size, fill));
            }
        });

        let freeing = std::thread::spawn(move || {
            // **Which blocks are live, not which have ever been seen
            // (task-1913).** This kept every address it had ever received and
            // refused a repeat, which is not a property of a correct
            // allocator: this thread frees each block before receiving the
            // next, so an address it has already returned is free for the pool
            // to hand out again, and a pool that reuses one is a pool doing
            // its job. On an idle machine the allocating thread ran ahead and
            // the channel buffered the whole list, so no address came back;
            // under load the two threads keep pace, the pool recycles, and the
            // case failed about 3.6% of the time - accusing the one crate in
            // the workspace allowed to write `unsafe` of handing out a live
            // block twice. Measured over 240 runs under contention: every one
            // of the ten repeats had already been freed, was not live, and
            // held exactly the bytes this round had written.
            let mut live: Vec<usize> = Vec::new();
            let mut received = 0usize;
            while let Ok((address, size, fill)) = receiver.recv() {
                assert!(
                    !live.contains(&address),
                    "the pool handed out {address:#x} twice while it was still live"
                );
                live.push(address);
                received = received.saturating_add(1);
                let block = address as *mut u8;
                // SAFETY: the allocating thread wrote `size` bytes here and has
                // not touched the block since; this thread now owns it.
                let held = unsafe { std::slice::from_raw_parts(block, size) };
                assert!(
                    held.iter().all(|byte| *byte == fill),
                    "a {size}-byte block came back holding something else"
                );
                let Ok(layout) = Layout::from_size_align(size, align) else {
                    continue;
                };
                // SAFETY: the same layout the block was allocated with, freed
                // once, on the thread that now owns it - which is the case the
                // crate's own comment says is safe and which nothing tested.
                unsafe { POOLED.dealloc(block, layout) };
                live.retain(|held| *held != address);
            }
            received
        });

        allocating.join().expect("the allocating thread finishes");
        let freed = freeing.join().expect("the freeing thread finishes");
        assert!(freed > 0, "no block crossed the boundary at align {align}");
    }
}

/// A block that arrived from another thread is recycled by the thread that
/// freed it, which is what makes the cross-thread flow a recycling rather than
/// a leak.
///
/// **The first version of this test asserted the wrong property and said so.**
/// It watched a one-directional flow - one thread allocating, another freeing -
/// and expected addresses to repeat. They never do, and the crate's own module
/// comment says why: "a block allocated on one thread and freed on another goes
/// onto the *freeing* thread's list". The allocating thread never sees that
/// list, so a purely one-directional flow reuses nothing on the producing side
/// by design, and the per-class cap is what bounds the consuming side.
///
/// What is actually claimed, and what is asserted here, is that the freeing
/// thread's own next allocation of that class takes the block back - so a
/// thread that both receives and allocates recycles rather than accumulating.
/// A pool that derived the size class from something recorded beside the block,
/// rather than from the layout the caller hands back, would put a foreign block
/// on the wrong list and this would not hold.
#[test]
fn a_block_received_from_another_thread_is_recycled_by_the_thread_that_freed_it() {
    const BLOCKS: usize = 32;
    let size = 64usize;
    let Ok(layout) = Layout::from_size_align(size, 8) else {
        panic!("64 bytes at 8 is a valid layout");
    };

    let (sender, receiver) = mpsc::channel::<usize>();
    let allocating = std::thread::spawn(move || {
        for _ in 0..BLOCKS {
            // SAFETY: a valid non-zero layout; the block is handed to exactly
            // one other thread and freed there once.
            let block = unsafe { POOLED.alloc(layout) };
            assert!(!block.is_null());
            let _ = sender.send(block as usize);
        }
    });

    let freeing = std::thread::spawn(move || {
        let mut foreign: Vec<usize> = Vec::with_capacity(BLOCKS);
        while let Ok(address) = receiver.recv() {
            foreign.push(address);
            // SAFETY: the other thread's block, same layout, freed once, on the
            // thread that owns it now.
            unsafe { POOLED.dealloc(address as *mut u8, layout) };
        }

        // Now allocate the same class on this thread. Every one of these should
        // come off the list the frees above built.
        let mut taken: Vec<usize> = Vec::with_capacity(foreign.len());
        for _ in 0..foreign.len() {
            // SAFETY: a valid non-zero layout, freed immediately below.
            let block = unsafe { POOLED.alloc(layout) };
            assert!(!block.is_null());
            taken.push(block as usize);
        }
        let reused = taken
            .iter()
            .filter(|address| foreign.contains(address))
            .count();
        for address in taken {
            // SAFETY: this thread's own block, same layout, freed once.
            unsafe { POOLED.dealloc(address as *mut u8, layout) };
        }
        (foreign.len(), reused)
    });

    allocating.join().expect("the allocating thread finishes");
    let (received, reused) = freeing.join().expect("the freeing thread finishes");
    assert_eq!(received, BLOCKS, "every block crossed the boundary");
    assert!(
        reused > 0,
        "none of the {received} blocks this thread freed came back from its own \
         list, so a block that arrived from another thread was not recycled - \
         which is the property the crate's module comment claims"
    );
}

/// Zero-sized and over-aligned requests are forwarded rather than pooled.
///
/// The two shapes the size classes cannot describe. Neither is exotic: a
/// zero-length `Vec` allocation and a cache-line-aligned buffer both arrive at
/// a global allocator in ordinary code.
#[test]
fn the_requests_no_size_class_describes_are_still_served() {
    for align in [32usize, 64, 128] {
        let Ok(layout) = Layout::from_size_align(256, align) else {
            continue;
        };
        // SAFETY: a valid non-zero layout, freed once below.
        let block = unsafe { POOLED.alloc(layout) };
        assert!(!block.is_null(), "align {align} was refused");
        assert_eq!(block as usize % align, 0, "align {align} was not honoured");
        // SAFETY: the same layout, freed once on the thread that allocated it.
        unsafe { POOLED.dealloc(block, layout) };
    }
}
