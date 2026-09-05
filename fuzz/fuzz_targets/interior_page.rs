//! Fuzzes the interior page decoder with arbitrary page bytes.
//!
//! Invariant: an interior page is the one structure in the file whose contents
//! are *addresses*. A page that arrives damaged and is believed sends every
//! descent below it somewhere else, so every offset it names is bounds checked
//! before it is used and an unordered slot array is a corruption rather than a
//! case to handle.
//!
//! The target reads every key and every swip, runs a descent against several
//! probes, and runs the ordering validation - the three things a descent, a
//! writeback and the integrity checker respectively do.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_pool::interior::{self, InteriorRef};

fuzz_target!(|data: &[u8]| {
    // Writeback runs over raw bytes rather than a parsed page, so it is
    // fuzzed the same way.
    let _ = interior::swip_offsets_of(data);

    let Ok(page) = InteriorRef::parse(data) else {
        return;
    };
    let _ = page.validate();
    let _ = page.key_columns();
    let _ = page.level();
    for slot in 0..page.count().min(4_096) {
        let _ = page.key(slot);
    }
    for child in 0..page.children().min(4_096) {
        let _ = page.swip(child);
        let _ = page.swip_offset(child);
    }
    let _ = page.rightmost();
    let _ = page.swip_offsets();
    for probe in [&b""[..], b"\x00", b"\xFF\xFF\xFF\xFF", b"key"] {
        let _ = page.child_for(probe);
    }
});
