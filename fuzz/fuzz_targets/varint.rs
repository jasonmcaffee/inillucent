//! Fuzzes the varint decoder with arbitrary bytes.
//!
//! Invariant: `decode` either returns a value and a length, or an error. It
//! never panics, never reads past the slice, and never returns a length longer
//! than the input. A corrupt database page is the first thing this codec sees.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_base::varint;

fuzz_target!(|data: &[u8]| {
    if let Ok(decoded) = varint::decode(data) {
        assert!(decoded.len <= data.len());
        assert!(decoded.len <= varint::MAX_LEN);
        let mut buffer = [0u8; varint::MAX_LEN];
        if varint::encoded_len(decoded.value) == decoded.len {
            let written = varint::encode(&mut buffer, decoded.value).expect("nine bytes is enough");
            assert_eq!(&buffer[..written], &data[..decoded.len], "decode/encode disagreed");
        }
    }
});
