//! Fuzzes page-size and offset arithmetic with arbitrary header bytes.
//!
//! Invariant: a page size that came off disk is validated before it is used,
//! and every offset computed from it uses checked arithmetic. A malformed
//! header must produce an error, not a wild offset.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_base::ids::PageId;
use inillucent_base::page::{self, PageSize};

fuzz_target!(|data: &[u8]| {
    let mut encoded = [0u8; 2];
    for (slot, byte) in encoded.iter_mut().zip(data.iter()) {
        *slot = *byte;
    }
    let Ok(size) = PageSize::from_encoded(u16::from_be_bytes(encoded)) else {
        return;
    };
    assert!(size.bytes().is_power_of_two());
    assert!((512..=65_536).contains(&size.bytes()));
    for reserved in data.iter().take(8) {
        let _ = size.usable(*reserved);
    }
    let mut number = [0u8; 4];
    for (slot, byte) in number.iter_mut().zip(data.iter().skip(2)) {
        *slot = *byte;
    }
    if let Some(page) = PageId::new(u32::from_be_bytes(number)) {
        let offset = page::page_offset(size, page).expect("checked arithmetic cannot overflow here");
        assert_eq!(offset % u64::from(size.bytes()), 0);
    }
});
