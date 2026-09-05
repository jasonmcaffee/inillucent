//! Fixed-width binary reading and writing for the parts of an index that are
//! large enough for the encoding to matter.
//!
//! The store used to be written with `serde_json::to_writer`, and `Store` holds
//! the entire chunk corpus in one `String` - 450 MB of it on the real mailbox.
//! Escaping that as JSON and parsing it back is most of what a save and a load
//! cost, for a structure that is already a flat byte array and a set of
//! fixed-width integers.

use std::io::{Read, Write};

/// A `u32` standing for "no value", so an `Option<u32>` needs no tag byte.
///
/// Safe because the values it stands in for are dictionary identifiers, which are
/// dense from zero: a corpus would need four billion distinct authors to reach it.
pub const NONE_ID: u32 = u32::MAX;

pub fn write_u32(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

pub fn write_u64(w: &mut impl Write, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

pub fn write_i64(w: &mut impl Write, v: i64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

pub fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

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

pub fn read_str(r: &mut impl Read) -> std::io::Result<String> {
    let len = read_u32(r)? as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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
    let mut values = vec![0u32; n];
    r.read_exact(bytemuck::cast_slice_mut(&mut values))?;
    Ok(values)
}

/// Reads `n` plain-old-data records into an aligned buffer. See `read_u32_vec`
/// for why this is not a byte read followed by a cast.
pub fn read_pod_vec<T: bytemuck::Pod + bytemuck::Zeroable>(
    r: &mut impl Read,
    n: usize,
) -> std::io::Result<Vec<T>> {
    let mut records = vec![T::zeroed(); n];
    r.read_exact(bytemuck::cast_slice_mut(&mut records))?;
    Ok(records)
}

/// A byte block as a length and its bytes, read back as UTF-8.
pub fn write_text(w: &mut impl Write, text: &str) -> std::io::Result<()> {
    write_u64(w, text.len() as u64)?;
    w.write_all(text.as_bytes())
}

pub fn read_text(r: &mut impl Read) -> std::io::Result<String> {
    let len = read_u64(r)? as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
