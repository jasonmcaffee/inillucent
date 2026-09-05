//! Fuzzes the checked big-endian readers at arbitrary offsets.
//!
//! Invariant: every reader is total. Any slice and any offset produce a value
//! or a `SQLITE_CORRUPT` error, never a panic and never an out-of-bounds read.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_base::bytes;

fuzz_target!(|data: &[u8]| {
    for offset in 0..data.len().saturating_add(9) {
        let _ = bytes::read_u8(data, offset);
        let _ = bytes::read_u16(data, offset);
        let _ = bytes::read_u24(data, offset);
        let _ = bytes::read_u32(data, offset);
        let _ = bytes::read_u48(data, offset);
        let _ = bytes::read_u64(data, offset);
        let _ = bytes::read_f64(data, offset);
    }
    let mut reader = bytes::ByteReader::new(data);
    while reader.remaining() > 0 && reader.u8().is_ok() {}
});
