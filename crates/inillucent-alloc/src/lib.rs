//! A size-classed free list over the system allocator.
//!
//! Invariant: **it recycles rather than accumulating.** A bump arena that never
//! frees is the shortest thing to write and the wrong thing to ship: thirty
//! rounds of the gate's plan would grow it without bound, and what it measured
//! would be page faults rather than allocation. This keeps one intrusive list
//! per size class, capped, and hands a block back to the system allocator when
//! the class is full.
//!
//! ## Why the engine has one at all
//!
//! Because allocation is where a compile goes. A trivial compile was measured
//! at **25 heap allocations, with the Windows C runtime heap at 59% of the
//! time**, and a size-classed free list at **17% overall** on the same
//! plan - which is why Phase 3's Part E names it the cheapest first move rather
//! than one of the several structural changes beside it. The same shape is
//! visible outside compilation: `CREATE INDEX` over a hundred thousand rows
//! builds two allocations per row just to hold the key and the rowid, and the
//! gate's `schema.index` spends more time in its scan than SQLite spends on the
//! whole statement.
//!
//! What it removes is exactly what was in question: the size lookup, the
//! locking and the per-call bookkeeping the system allocator does. It does not
//! try to be a better allocator in general - above [`LARGEST`] and for any
//! alignment the system's own guarantee does not cover, the request is
//! forwarded unchanged.
//!
//! ## Why it never allocates
//!
//! An allocator that allocates re-enters itself, and a re-entrant allocator is
//! a deadlock or a stack overflow waiting for the right allocation pattern. So
//! the free lists are **intrusive**: a freed block holds the pointer to the
//! next free block of its class in its own first eight bytes, and the heads
//! live in a fixed-size array of `Cell`s in thread-local storage. Nothing here
//! calls `Vec`, `Box`, or anything that could.
//!
//! ## Why it is a crate of its own
//!
//! Because every other production crate in this workspace carries
//! `#![forbid(unsafe_code)]`, and an allocator cannot. Putting it here keeps
//! that true everywhere it is true today and confines the unsafe to one file
//! that has nothing else in it - no dependencies, first-party or otherwise, so
//! there is nothing it could re-enter itself through.
//!
//! ## Why it is per thread
//!
//! Because a shared list needs a lock, and the lock is most of what this exists
//! to remove. A block allocated on one thread and freed on another goes onto
//! the freeing thread's list, which is safe - the block is memory of a known
//! class, and the class is derived from the layout the caller hands back - and
//! at worst moves a block between threads. The per-class cap bounds what that
//! can cost.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The largest allocation the free list handles itself.
///
/// Above this the system allocator is asked directly: a big block is rare, and
/// keeping a free list of them would be a cache of things nothing asks for
/// twice.
pub const LARGEST: usize = 4_096;

/// The granularity of a size class, which is also the alignment every pooled
/// block is made with.
const GRAIN: usize = 16;

/// How many size classes the free list holds.
///
/// One per sixteen bytes up to [`LARGEST`], which is the granularity a `Vec<u8>`
/// of a row, a key or a name actually lands on.
const CLASSES: usize = LARGEST / GRAIN + 1;

/// How many **bytes** one class keeps before handing the rest back.
///
/// The cap is what makes this a recycler rather than a leak: a workload that
/// allocates a million blocks of one class and frees them all keeps a bounded
/// amount and returns the rest, so the process's footprint is bounded by the
/// classes rather than by the workload.
///
/// **Bytes rather than blocks.** The cap was a thousand and
/// twenty-four *blocks* per class - a fixed count over classes whose sizes
/// differ by two hundred and fifty-six times, so the same number meant sixteen
/// kilobytes in the smallest class and four megabytes in the largest. The
/// gate's write family paid for it: one round left 5.2 MiB of heap standing
/// that nothing live was using, and the process's high-water mark is exactly
/// what this ticket's memory bar reads.
///
/// Sixteen kilobytes per class keeps the small classes as deep as they were -
/// the sixteen-byte class still holds its thousand and twenty-four blocks,
/// which is where the free list's measured speed comes from - and bounds the
/// largest at four. The whole cache is at most `CLASSES * 16 KiB`, about
/// 4 MiB, rather than an unbounded function of which classes a workload
/// happened to touch.
const PER_CLASS_BYTES: usize = 64 << 10;

/// The block cap that was here before, kept as a ceiling.
///
/// **The byte cap only ever takes retention away.** Applying it alone would
/// have made the smallest class keep four thousand blocks where it used to keep
/// a thousand - more, not less - and the small classes are exactly where the
/// free list's measured speed comes from. Keeping the old count as a ceiling
/// means every class holds *at most* what it held before, and the large ones
/// hold far less.
const PER_CLASS_BLOCKS: usize = 1_024;

/// How many blocks of each class the free list keeps, worked out once.
///
/// **A table rather than the arithmetic, because `dealloc` is the hot path.**
/// The cap is a division and a pair of clamps, and computing it per free put a
/// divide on every deallocation the program makes. It is a constant of the
/// class, so it is a constant of the build.
const CAPS: [usize; CLASSES] = caps();

/// Builds [`CAPS`] at compile time.
///
/// The lower of the two caps, and at least one so no class is barred from
/// recycling by arithmetic. At 64 KiB and a 1,024-block ceiling: the 16-byte
/// class keeps its full thousand and twenty-four, unchanged, and the
/// 4,096-byte class keeps sixteen where it used to keep a thousand - which is
/// four megabytes of one class's free list that a write workload was leaving
/// standing.
#[allow(clippy::indexing_slicing)]
const fn caps() -> [usize; CLASSES] {
    let mut caps = [1usize; CLASSES];
    let mut class = 0usize;
    while class < CLASSES {
        let size = if class * GRAIN > GRAIN {
            class * GRAIN
        } else {
            GRAIN
        };
        let mut cap = PER_CLASS_BYTES / size;
        if cap > PER_CLASS_BLOCKS {
            cap = PER_CLASS_BLOCKS;
        }
        if cap < 1 {
            cap = 1;
        }
        caps[class] = cap;
        class += 1;
    }
    caps
}

/// Returns how many blocks of one class the free list keeps.
///
/// @param class - the size class
#[inline]
fn per_class(class: usize) -> usize {
    CAPS.get(class).copied().unwrap_or(1)
}

thread_local! {
    /// The head of each class's intrusive free list, or null.
    ///
    /// `const` on the whole initialiser, not only on the elements: it removes
    /// the lazy-initialisation check from every access, and an allocator's
    /// per-call cost is the thing this crate exists to keep small.
    static HEADS: [Cell<*mut u8>; CLASSES] =
        const { [const { Cell::new(std::ptr::null_mut()) }; CLASSES] };
    /// How many blocks each class is holding.
    static HELD: [Cell<usize>; CLASSES] = const { [const { Cell::new(0) }; CLASSES] };
}

/// Returns the size class an allocation falls in, if any.
///
/// `None` means "not ours": too large, too aligned, or too small to hold the
/// link a freed block stores in itself.
///
/// @param layout - the allocation's layout
#[inline]
fn class_of(layout: Layout) -> Option<usize> {
    if layout.size() > LARGEST
        || layout.align() > GRAIN
        || layout.size() < core::mem::size_of::<*mut u8>()
    {
        return None;
    }
    Some(layout.size().div_ceil(GRAIN))
}

/// Returns the layout a size class's blocks are allocated with.
///
/// @param class - the size class
#[inline]
fn layout_of(class: usize) -> Layout {
    // Every class is a multiple of the grain and aligned to it, so a block is
    // always at least as large and as aligned as any request in its class.
    Layout::from_size_align(class.saturating_mul(GRAIN).max(GRAIN), GRAIN)
        .unwrap_or_else(|_| Layout::new::<u128>())
}

/// A size-classed free list over the system allocator.
///
/// Install it in a binary with
///
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: inillucent_base::alloc::Pooled = inillucent_base::alloc::Pooled;
/// ```
///
/// It is per binary rather than per library because only a binary can name a
/// global allocator, and because the choice belongs to whoever is running the
/// program.
pub struct Pooled;

// SAFETY: every path either forwards to the system allocator unchanged, or
// hands back a block this allocator obtained from `System` with its class's own
// layout and has not handed out since. The class is derived from the layout on
// both sides, so a block is only ever reused for a request it is large enough
// and aligned enough for.
unsafe impl GlobalAlloc for Pooled {
    // SAFETY: a pooled block was allocated by `System.alloc` with the class's
    // layout, which is at least this request's size and alignment. The link
    // read out of the block was written by `dealloc` below and nothing has
    // touched the block since - it is not reachable by any other path while it
    // is on the list.
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = class_of(layout) else {
            // SAFETY: forwarded unchanged to the system allocator.
            return unsafe { System.alloc(layout) };
        };
        // `try_with`, because a thread tearing down has already dropped its
        // storage and a panic inside an allocator aborts the process. A request
        // that finds no list is simply a fresh block.
        let taken = HEADS
            .try_with(|heads| {
                let Some(head) = heads.get(class) else {
                    return std::ptr::null_mut();
                };
                let block = head.get();
                if block.is_null() {
                    return std::ptr::null_mut();
                }
                // SAFETY: the block is one this allocator made and put on the
                // list, and its first word is the link `dealloc` wrote.
                let next = unsafe { block.cast::<*mut u8>().read() };
                head.set(next);
                let _ = HELD.try_with(|held| {
                    if let Some(count) = held.get(class) {
                        count.set(count.get().saturating_sub(1));
                    }
                });
                block
            })
            .unwrap_or(std::ptr::null_mut());
        if !taken.is_null() {
            return taken;
        }
        // SAFETY: a fresh block of the class's own layout, which is at least as
        // large and as aligned as the request. It is freed with the same
        // layout, in `dealloc` below.
        unsafe { System.alloc(layout_of(class)) }
    }

    // SAFETY: the pointer and layout are the ones handed out above, so a
    // pointer with a pooled class is a block of at least `GRAIN` bytes and can
    // hold the link. Anything else goes back to the system allocator with the
    // layout it was made with.
    #[inline]
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let Some(class) = class_of(layout) else {
            // SAFETY: forwarded unchanged to the allocator that made it.
            return unsafe { System.dealloc(pointer, layout) };
        };
        let kept = HELD
            .try_with(|held| {
                let Some(count) = held.get(class) else {
                    return false;
                };
                if count.get() >= per_class(class) {
                    return false;
                }
                HEADS
                    .try_with(|heads| {
                        let Some(head) = heads.get(class) else {
                            return false;
                        };
                        // SAFETY: the block is at least eight bytes and is not
                        // reachable by anything else once the caller has freed
                        // it, so its first word is ours to use as the link.
                        unsafe { pointer.cast::<*mut u8>().write(head.get()) };
                        head.set(pointer);
                        count.set(count.get().saturating_add(1));
                        true
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if kept {
            return;
        }
        // SAFETY: freed with the layout it was allocated with in `alloc`.
        unsafe { System.dealloc(pointer, layout_of(class)) };
    }

    // SAFETY: a realloc that stays inside one class is the same block, because
    // every block in a class is the full class size. Anything else goes through
    // the default alloc-copy-free, which is what `GlobalAlloc` does when this
    // is not overridden.
    #[inline]
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old = class_of(layout);
        let new = Layout::from_size_align(new_size, layout.align())
            .ok()
            .and_then(class_of);
        if let (Some(old), Some(new)) = (old, new) {
            if old == new {
                return pointer;
            }
        }
        // SAFETY: the default behaviour, spelled out: a fresh block, the old
        // bytes copied into it, and the old block freed. Every argument is the
        // caller's or derived from it.
        unsafe {
            let Ok(wanted) = Layout::from_size_align(new_size, layout.align()) else {
                return std::ptr::null_mut();
            };
            let fresh = self.alloc(wanted);
            if !fresh.is_null() {
                std::ptr::copy_nonoverlapping(pointer, fresh, layout.size().min(new_size));
                self.dealloc(pointer, layout);
            }
            fresh
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classes cover what they claim to and refuse what they do not.
    #[test]
    fn the_classes_are_the_ones_documented() {
        assert_eq!(class_of(Layout::from_size_align(8, 8).unwrap()), Some(1));
        assert_eq!(class_of(Layout::from_size_align(16, 16).unwrap()), Some(1));
        assert_eq!(class_of(Layout::from_size_align(17, 8).unwrap()), Some(2));
        assert_eq!(
            class_of(Layout::from_size_align(LARGEST, 16).unwrap()),
            Some(CLASSES - 1)
        );
        // Too large, too aligned, and too small to hold the link.
        assert_eq!(
            class_of(Layout::from_size_align(LARGEST + 1, 8).unwrap()),
            None
        );
        assert_eq!(class_of(Layout::from_size_align(32, 32).unwrap()), None);
        assert_eq!(class_of(Layout::from_size_align(4, 4).unwrap()), None);
    }

    /// Every class's block is at least as large and as aligned as any request
    /// in it, which is the whole of why reuse is safe.
    #[test]
    fn a_class_block_covers_every_request_in_it() {
        for size in 8..=LARGEST {
            let Some(layout) = Layout::from_size_align(size, 8).ok() else {
                continue;
            };
            let Some(class) = class_of(layout) else {
                continue;
            };
            let block = layout_of(class);
            assert!(block.size() >= layout.size(), "size {size}");
            assert!(block.align() >= layout.align(), "size {size}");
        }
    }

    /// A block goes onto its class's list and comes back off it.
    ///
    /// Driven through the allocator itself rather than through the lists,
    /// because what has to hold is that a pointer handed back is one that was
    /// handed out - the intrusive link is an implementation detail and asserting
    /// on it would pin the implementation rather than the behaviour.
    #[test]
    fn a_freed_block_is_the_one_handed_back() {
        let layout = Layout::from_size_align(64, 8).expect("a layout");
        // SAFETY: every pointer here comes from this allocator and is freed
        // with the layout it was allocated with.
        unsafe {
            let first = Pooled.alloc(layout);
            assert!(!first.is_null());
            Pooled.dealloc(first, layout);
            let second = Pooled.alloc(layout);
            assert_eq!(first, second, "the freed block was not recycled");
            Pooled.dealloc(second, layout);
        }
    }

    /// A block that is written to and recycled does not carry its old bytes
    /// into a caller's hands as anything but uninitialised memory.
    ///
    /// The link is written over the first word of a freed block, so a caller
    /// that assumed a fresh allocation was zeroed would be reading it. Nothing
    /// may assume that of `alloc` - `alloc_zeroed` is the one that promises -
    /// and this pins that the link is confined to the block itself and does not
    /// run past its end.
    #[test]
    fn recycling_stays_inside_the_block() {
        let layout = Layout::from_size_align(16, 8).expect("a layout");
        // SAFETY: as above; the guard bytes are a second allocation this test
        // owns for the length of the check.
        unsafe {
            let guard = Pooled.alloc(layout);
            let block = Pooled.alloc(layout);
            std::ptr::write_bytes(guard, 0xAB, layout.size());
            Pooled.dealloc(block, layout);
            let again = Pooled.alloc(layout);
            assert_eq!(block, again);
            for at in 0..layout.size() {
                assert_eq!(guard.add(at).read(), 0xAB, "the link ran past its block");
            }
            Pooled.dealloc(again, layout);
            Pooled.dealloc(guard, layout);
        }
    }

    /// A realloc inside one class is the same block, and one that crosses a
    /// class keeps the bytes.
    #[test]
    fn realloc_keeps_the_bytes() {
        // SAFETY: every pointer comes from this allocator, and every layout is
        // the one the block was made with.
        unsafe {
            let small = Layout::from_size_align(16, 8).expect("a layout");
            let block = Pooled.alloc(small);
            std::ptr::write_bytes(block, 0x5A, small.size());
            let inside = Pooled.realloc(block, small, 12);
            assert_eq!(inside, block, "a realloc inside one class moved the block");
            let across = Pooled.realloc(inside, small, 200);
            assert!(!across.is_null());
            for at in 0..small.size() {
                assert_eq!(across.add(at).read(), 0x5A, "realloc lost a byte");
            }
            Pooled.dealloc(across, Layout::from_size_align(200, 8).expect("a layout"));
        }
    }
}
