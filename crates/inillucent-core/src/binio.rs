//! Fixed-width binary reading and writing for the parts of an index that are
//! large enough for the encoding to matter.
//!
//! The store used to be written with `serde_json::to_writer`, and `Store` holds
//! the entire chunk corpus in one `String` - 450 MB of it on the real mailbox.
//! Escaping that as JSON and parsing it back is most of what a save and a load
//! cost, for a structure that is already a flat byte array and a set of
//! fixed-width integers.
//!
//! Invariant: **every read is bounded by what the source actually supplied,
//! not by a length the source claimed.** These readers take a count out of the
//! bytes they are decoding and that count is a number an attacker chooses: a
//! `.rdb` segment is a page somebody else could have written. Sizing a buffer
//! from it before the bytes are known to be there is not an error that can be
//! returned - an allocation of a few gigabytes goes through
//! `handle_alloc_error` and aborts the process - so the readers grow in fixed
//! steps and fail with `UnexpectedEof` instead.

use std::io::{Read, Write};

/// How much is allocated before the source has been shown to hold it.
///
/// **Every reader below sized its buffer from a length it had just read out of
/// the file and allocated that before asking whether the bytes were there
/// (task-1932, H4).** A `.rdb` segment row is bytes somebody else can write to,
/// and `vec![0u8; length]` on a claimed length of a few hundred gigabytes does
/// not return an error - it goes through `handle_alloc_error`, which aborts the
/// process. The doc comment on `persist::read_section` named that as the
/// failure it existed to prevent, and the ceiling it checked first was 1 TiB.
///
/// Reading in fixed steps is what actually prevents it: the buffer never grows
/// past what has already arrived plus one step, so a claimed length the source
/// cannot satisfy costs this allocation and then fails with `UnexpectedEof`.
/// A megabyte is large enough that a real index of any size pays a negligible
/// number of extra `read_exact` calls, and small enough that a hostile length
/// costs nothing.
const CHUNK_BYTES: usize = 1 << 20;

/// Reads exactly `n` records, growing the buffer as the bytes arrive.
///
/// The buffer is a `Vec<T>` rather than bytes that are cast afterwards, for the
/// alignment reason `read_u32_vec` gives: a `Vec<u8>` is aligned to one byte,
/// so casting it to a wider type is a run-time failure a small corpus can miss.
///
/// @param r - the source
/// @param n - how many records to read
fn read_records<T: bytemuck::Pod + bytemuck::Zeroable>(
    r: &mut impl Read,
    n: usize,
) -> std::io::Result<Vec<T>> {
    let width = std::mem::size_of::<T>().max(1);
    let step = (CHUNK_BYTES / width).max(1);
    let mut records: Vec<T> = Vec::new();
    let mut done: usize = 0;
    while done < n {
        let want = step.min(n.saturating_sub(done));
        records.resize_with(done.saturating_add(want), T::zeroed);
        let Some(slot) = records.get_mut(done..) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a record buffer could not be grown",
            ));
        };
        r.read_exact(bytemuck::cast_slice_mut(slot))?;
        done = done.saturating_add(want);
    }
    Ok(records)
}

/// A `u32` standing for "no value", so an `Option<u32>` needs no tag byte.
///
/// Safe because the values it stands in for are dictionary identifiers, which are
/// dense from zero: a corpus would need four billion distinct authors to reach it.
pub const NONE_ID: u32 = u32::MAX;

/// Writes a `u32` as four little-endian bytes.
///
/// @param w - where the bytes go
/// @param v - the value
pub fn write_u32(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes a `u64` as eight little-endian bytes.
///
/// @param w - where the bytes go
/// @param v - the value
pub fn write_u64(w: &mut impl Write, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes an `i64` as eight little-endian bytes.
///
/// @param w - where the bytes go
/// @param v - the value
pub fn write_i64(w: &mut impl Write, v: i64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Reads a `u32` back, or fails if four bytes are not there.
///
/// @param r - the source
pub fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Reads a `u64` back, or fails if eight bytes are not there.
///
/// @param r - the source
pub fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Reads an `i64` back, or fails if eight bytes are not there.
///
/// @param r - the source
pub fn read_i64(r: &mut impl Read) -> std::io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

/// A string as a byte length and its UTF-8 bytes.
pub fn write_str(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    write_u32(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

/// Reads back a string written by [`write_str`].
///
/// The length is a number out of the source, so the bytes are read in steps
/// rather than allocated from it - see `read_records`.
///
/// @param r - the source
pub fn read_str(r: &mut impl Read) -> std::io::Result<String> {
    let len = read_u32(r)? as usize;
    let bytes = read_records::<u8>(r, len)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// A `u32` array as a count and one contiguous little-endian block.
pub fn write_u32_slice(w: &mut impl Write, values: &[u32]) -> std::io::Result<()> {
    write_u64(w, values.len() as u64)?;
    w.write_all(bytemuck::cast_slice(values))
}

/// Reads into a `u32` buffer rather than into bytes that are then cast.
///
/// A `Vec<u8>` is aligned to one byte, so casting it to a wider type is a
/// run-time alignment failure that a small test corpus can miss and a large one
/// hits. Allocating the target type first makes the alignment the compiler's
/// problem and removes the copy as well.
pub fn read_u32_vec(r: &mut impl Read) -> std::io::Result<Vec<u32>> {
    let n = read_u64(r)? as usize;
    read_records(r, n)
}

/// Reads `n` plain-old-data records into an aligned buffer. See `read_u32_vec`
/// for why this is not a byte read followed by a cast.
pub fn read_pod_vec<T: bytemuck::Pod + bytemuck::Zeroable>(
    r: &mut impl Read,
    n: usize,
) -> std::io::Result<Vec<T>> {
    read_records(r, n)
}

/// A byte block as a length and its bytes, read back as UTF-8.
pub fn write_text(w: &mut impl Write, text: &str) -> std::io::Result<()> {
    write_u64(w, text.len() as u64)?;
    w.write_all(text.as_bytes())
}

/// Reads back a string written by [`write_text`], whose length is a `u64`.
///
/// @param r - the source
pub fn read_text(r: &mut impl Read) -> std::io::Result<String> {
    let len = read_u64(r)? as usize;
    let bytes = read_records::<u8>(r, len)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
