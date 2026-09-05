//! `sqlite3_malloc` and the family that has to be able to free what it returns.
//!
//! Invariant: a block this module hands to C is one this module can take back.
//! Rust's allocator needs the layout to free a block, and C's `free`-shaped
//! call site does not have it, so every block carries its own size in a header
//! immediately before the pointer the caller sees. That header is the whole
//! trick, and it is why `sqlite3_free` may only ever be called on a pointer
//! `sqlite3_malloc`, `sqlite3_realloc` or one of the string builders returned -
//! exactly the rule SQLite states.

use std::alloc::{alloc, dealloc, Layout};
use std::os::raw::{c_char, c_void};

/// How many bytes sit in front of the pointer the caller sees.
///
/// One `usize`, holding the size of the whole block including this header, and
/// the alignment is `usize`'s so the returned pointer is aligned for anything a
/// C caller will put in it.
const HEADER: usize = std::mem::size_of::<usize>();

/// Returns the layout a block of `total` bytes was allocated with.
fn layout_of(total: usize) -> Option<Layout> {
    Layout::from_size_align(total, std::mem::align_of::<usize>()).ok()
}

/// Allocates `bytes` bytes, returning a pointer C may pass to `sqlite3_free`.
///
/// A request for zero returns null, which is what SQLite does; a caller that
/// treats null as failure is therefore right, and one that frees it is safe
/// because `sqlite3_free(NULL)` is a no-op.
///
/// # Safety
///
/// The returned pointer must be freed with [`sqlite3_free`] and no other
/// deallocator.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_malloc(bytes: std::os::raw::c_int) -> *mut c_void {
    if bytes <= 0 {
        return std::ptr::null_mut();
    }
    sqlite3_malloc64(bytes as u64)
}

/// Allocates `bytes` bytes, taking a 64-bit size.
///
/// # Safety
///
/// As [`sqlite3_malloc`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_malloc64(bytes: u64) -> *mut c_void {
    let Ok(size) = usize::try_from(bytes) else {
        return std::ptr::null_mut();
    };
    if size == 0 {
        return std::ptr::null_mut();
    }
    let Some(total) = size.checked_add(HEADER) else {
        return std::ptr::null_mut();
    };
    let Some(layout) = layout_of(total) else {
        return std::ptr::null_mut();
    };
    let block = alloc(layout);
    if block.is_null() {
        return std::ptr::null_mut();
    }
    block.cast::<usize>().write(total);
    block.add(HEADER).cast::<c_void>()
}

/// Returns how many bytes a block holds, not counting the header.
///
/// # Safety
///
/// The pointer must be null or one [`sqlite3_malloc`] returned.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_msize(block: *mut c_void) -> u64 {
    if block.is_null() {
        return 0;
    }
    let start = block.cast::<u8>().sub(HEADER);
    let total = start.cast::<usize>().read();
    (total.saturating_sub(HEADER)) as u64
}

/// Frees a block this library allocated. Null is a no-op.
///
/// # Safety
///
/// The pointer must be null or one [`sqlite3_malloc`], [`sqlite3_realloc`] or a
/// documented string-returning entry point handed back, and must not be freed
/// twice.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_free(block: *mut c_void) {
    if block.is_null() {
        return;
    }
    let start = block.cast::<u8>().sub(HEADER);
    let total = start.cast::<usize>().read();
    let Some(layout) = layout_of(total) else {
        return;
    };
    dealloc(start, layout);
}

/// Grows or shrinks a block, copying what fits.
///
/// # Safety
///
/// As [`sqlite3_free`] for the old pointer, and as [`sqlite3_malloc`] for the
/// new one.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_realloc(
    block: *mut c_void,
    bytes: std::os::raw::c_int,
) -> *mut c_void {
    sqlite3_realloc64(block, bytes.max(0) as u64)
}

/// Grows or shrinks a block, taking a 64-bit size.
///
/// # Safety
///
/// As [`sqlite3_realloc`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_realloc64(block: *mut c_void, bytes: u64) -> *mut c_void {
    if block.is_null() {
        return sqlite3_malloc64(bytes);
    }
    if bytes == 0 {
        sqlite3_free(block);
        return std::ptr::null_mut();
    }
    let old = sqlite3_msize(block) as usize;
    let fresh = sqlite3_malloc64(bytes);
    if fresh.is_null() {
        return std::ptr::null_mut();
    }
    let keep = old.min(bytes as usize);
    std::ptr::copy_nonoverlapping(block.cast::<u8>(), fresh.cast::<u8>(), keep);
    sqlite3_free(block);
    fresh
}

/// Copies bytes into a block this library allocated, NUL-terminated.
///
/// Used wherever an entry point owes the caller a string it must free: the
/// terminator is added so the result is usable as a C string even when the
/// caller ignores the length.
pub(crate) fn owned_c_string(bytes: &[u8]) -> *mut c_char {
    // SAFETY: the block is freshly allocated for exactly this many bytes plus
    // the terminator, and nothing else holds it yet.
    unsafe {
        let block = sqlite3_malloc64(bytes.len() as u64 + 1);
        if block.is_null() {
            return std::ptr::null_mut();
        }
        let start = block.cast::<u8>();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), start, bytes.len());
        start.add(bytes.len()).write(0);
        block.cast::<c_char>()
    }
}

/// Copies bytes into a block this library allocated, without a terminator.
pub(crate) fn owned_bytes(bytes: &[u8]) -> *mut u8 {
    // SAFETY: as `owned_c_string`, without the terminator.
    unsafe {
        let block = sqlite3_malloc64(bytes.len().max(1) as u64);
        if block.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), block.cast::<u8>(), bytes.len());
        block.cast::<u8>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block round-trips through the header that carries its size.
    #[test]
    fn a_block_remembers_how_big_it_is() {
        unsafe {
            let block = sqlite3_malloc(40);
            assert!(!block.is_null());
            assert_eq!(sqlite3_msize(block), 40);
            sqlite3_free(block);
        }
    }

    /// Growing a block keeps what was in it.
    #[test]
    fn growing_a_block_keeps_its_contents() {
        unsafe {
            let block = sqlite3_malloc(4).cast::<u8>();
            std::ptr::copy_nonoverlapping(b"abcd".as_ptr(), block, 4);
            let grown = sqlite3_realloc(block.cast(), 16).cast::<u8>();
            assert_eq!(std::slice::from_raw_parts(grown, 4), b"abcd");
            assert_eq!(sqlite3_msize(grown.cast()), 16);
            sqlite3_free(grown.cast());
        }
    }

    /// Zero bytes is null, and freeing null does nothing.
    #[test]
    fn zero_is_null_and_null_is_free_to_free() {
        unsafe {
            assert!(sqlite3_malloc(0).is_null());
            assert!(sqlite3_malloc(-1).is_null());
            assert_eq!(sqlite3_msize(std::ptr::null_mut()), 0);
            sqlite3_free(std::ptr::null_mut());
        }
    }
}
