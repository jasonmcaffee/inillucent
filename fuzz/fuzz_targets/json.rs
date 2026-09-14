//! Fuzzes the MCP request parser with arbitrary text.
//!
//! Invariant: the parser either answers a value or a message saying what is
//! wrong with the text, and never panics or recurses until the stack runs out.
//! Every `tools/call` an agent host sends arrives through this function, so its
//! input is not the application's own.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Lossy rather than skipping invalid UTF-8: the replacement character is
    // itself an input the parser has to handle, and skipping would throw away
    // most of what the fuzzer generates.
    let text = String::from_utf8_lossy(data);
    let _ = inillucent_cli::json::parse(&text);
});
