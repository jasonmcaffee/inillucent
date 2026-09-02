//! Fallible byte buffers for pages and frames.
//!
//! Invariant: every allocation here is fallible. A database asked to open with
//! a 64 KiB page size on a machine that cannot supply the memory returns
//! `SQLITE_NOMEM`; it does not abort the process, which is what
//! `Vec::with_capacity` would do.
//!
//! The module also counts what it allocates. The counters are the cheapest
//! honest answer to "did this change allocate more per page?", and phase 1's
//! performance work is instrumentation rather than optimisation, so they exist
//! from the start rather than being retrofitted once a hot path is suspect.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::bytes;
use crate::error::{no_mem, DbResult};
use crate::page::PageSize;

/// How many buffers have been allocated since the process started.
static BUFFERS_ALLOCATED: AtomicU64 = AtomicU64::new(0);

/// How many bytes of buffer have been allocated since the process started.
static BYTES_ALLOCATED: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the buffer allocation counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationStats {
    /// Buffers allocated since the process started.
    pub buffers: u64,
    /// Bytes of buffer allocated since the process started.
    pub bytes: u64,
}

impl AllocationStats {
    /// Returns the counters accumulated between this snapshot and a later one.
    pub fn since(self, earlier: AllocationStats) -> AllocationStats {
        AllocationStats {
            buffers: self.buffers.saturating_sub(earlier.buffers),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// Reads the buffer allocation counters.
pub fn allocation_stats() -> AllocationStats {
    AllocationStats {
        buffers: BUFFERS_ALLOCATED.load(Ordering::Relaxed),
        bytes: BYTES_ALLOCATED.load(Ordering::Relaxed),
    }
}

/// Allocates a zeroed byte buffer, returning `SQLITE_NOMEM` on failure.
pub fn try_zeroed(len: usize) -> DbResult<Box<[u8]>> {
    let mut buffer: Vec<u8> = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| no_mem("cannot allocate a byte buffer"))?;
    buffer.resize(len, 0);
    BUFFERS_ALLOCATED.fetch_add(1, Ordering::Relaxed);
    BYTES_ALLOCATED.fetch_add(len as u64, Ordering::Relaxed);
    Ok(buffer.into_boxed_slice())
}

/// Allocates a buffer holding a copy of `source`.
pub fn try_copy_of(source: &[u8]) -> DbResult<Box<[u8]>> {
    let mut buffer = try_zeroed(source.len())?;
    for (slot, byte) in buffer.iter_mut().zip(source.iter()) {
        *slot = *byte;
    }
    Ok(buffer)
}

/// One database page's worth of bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageBuffer {
    size: PageSize,
    bytes: Box<[u8]>,
}

impl PageBuffer {
    /// Allocates a zeroed page of `size` bytes.
    pub fn zeroed(size: PageSize) -> DbResult<PageBuffer> {
        Ok(PageBuffer {
            size,
            bytes: try_zeroed(size.as_usize())?,
        })
    }

    /// Wraps bytes that are already exactly one page long.
    pub fn from_bytes(size: PageSize, bytes: Box<[u8]>) -> DbResult<PageBuffer> {
        if bytes.len() != size.as_usize() {
            return Err(crate::error::corrupt(
                "page buffer is not exactly one page long",
            ));
        }
        Ok(PageBuffer { size, bytes })
    }

    /// Returns the page size this buffer was allocated for.
    pub fn size(&self) -> PageSize {
        self.size
    }

    /// Returns the page bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the page bytes for modification.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Reads a big-endian 16-bit field at `offset`.
    pub fn read_u16(&self, offset: usize) -> DbResult<u16> {
        bytes::read_u16(&self.bytes, offset)
    }

    /// Reads a big-endian 32-bit field at `offset`.
    pub fn read_u32(&self, offset: usize) -> DbResult<u32> {
        bytes::read_u32(&self.bytes, offset)
    }

    /// Writes a big-endian 16-bit field at `offset`.
    pub fn write_u16(&mut self, offset: usize, value: u16) -> DbResult<()> {
        bytes::write_u16(&mut self.bytes, offset, value)
    }

    /// Writes a big-endian 32-bit field at `offset`.
    pub fn write_u32(&mut self, offset: usize, value: u32) -> DbResult<()> {
        bytes::write_u32(&mut self.bytes, offset, value)
    }

    /// Fills the page with zeroes without releasing its memory.
    pub fn clear(&mut self) {
        for byte in self.bytes.iter_mut() {
            *byte = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::PrimaryCode;

    /// A page buffer is exactly one page long and starts zeroed.
    #[test]
    fn a_page_buffer_is_exactly_one_page_of_zeroes() {
        let size = PageSize::new(4096).unwrap();
        let page = PageBuffer::zeroed(size).unwrap();
        assert_eq!(page.as_slice().len(), 4096);
        assert!(page.as_slice().iter().all(|byte| *byte == 0));
    }

    /// Wrapping bytes of the wrong length is refused rather than silently
    /// producing a page whose tail reads past its own buffer.
    #[test]
    fn wrapping_the_wrong_length_is_refused() {
        let size = PageSize::new(1024).unwrap();
        let short = try_zeroed(1023).unwrap();
        assert_eq!(
            PageBuffer::from_bytes(size, short).unwrap_err().code(),
            PrimaryCode::Corrupt
        );
    }

    /// An allocation far larger than any machine has must return NOMEM rather
    /// than aborting, which is the whole reason the module exists.
    #[test]
    fn an_impossible_allocation_returns_nomem() {
        let error = try_zeroed(usize::MAX / 2).expect_err("this cannot be allocated");
        assert_eq!(error.code(), PrimaryCode::NoMem);
    }

    /// The counters must move by exactly what was asked for.
    #[test]
    fn allocation_counters_track_what_was_allocated() {
        let before = allocation_stats();
        let _first = try_zeroed(4096).unwrap();
        let _second = try_zeroed(1024).unwrap();
        let delta = allocation_stats().since(before);
        assert!(delta.buffers >= 2, "{delta:?}");
        assert!(delta.bytes >= 5120, "{delta:?}");
    }

    /// Field accessors go through the checked byte codecs, so an out-of-range
    /// offset is an error rather than a panic.
    #[test]
    fn field_accessors_are_bounds_checked() {
        let size = PageSize::new(512).unwrap();
        let mut page = PageBuffer::zeroed(size).unwrap();
        page.write_u32(508, 0xdead_beef).unwrap();
        assert_eq!(page.read_u32(508).unwrap(), 0xdead_beef);
        assert_eq!(page.read_u32(509).unwrap_err().code(), PrimaryCode::Corrupt);
        page.clear();
        assert_eq!(page.read_u32(508).unwrap(), 0);
    }
}
