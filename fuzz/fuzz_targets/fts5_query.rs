//! Fuzzes the FTS5 `MATCH` query parser with arbitrary bytes.
//!
//! Invariant: **a `MATCH` argument is refused or parsed, and never takes the
//! process down.** It is the second largest untrusted input in the product
//! after SQL text itself, and it is a *different* grammar read by a
//! *different* parser: phrases, `NEAR`, column filters, `AND`/`OR`/`NOT`,
//! quoting and nesting, none of which the SQL lexer sees (task-2066 section
//! 4.4.7).
//!
//! It takes bytes rather than text, because that is what the parser takes. A
//! `MATCH` argument arrives as a value, and a value can be a blob or text that
//! is not valid UTF-8 - so an input this target skipped for not decoding would
//! be skipping exactly the inputs a caller can send and the parser has to
//! survive.
//!
//! All three tokenizers this build has, because the tokenizer is what a query
//! *means* and each takes a different path through the same text: `ascii`
//! separates on anything outside `A-Za-z0-9`, `unicode61` reads characters,
//! and `porter` stems whatever another tokenizer produced. The column list is
//! two names, so a column filter has something to resolve and something to
//! fail to resolve.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inillucent_ext::vtab::fts5::expr::Query;
use inillucent_ext::vtab::fts5::tokenize::Tokenizer;

fuzz_target!(|data: &[u8]| {
    let columns = vec![b"title".to_vec(), b"body".to_vec()];
    let unicode = Tokenizer::Unicode61 {
        remove_diacritics: true,
        extra_tokens: Vec::new(),
        separators: Vec::new(),
    };
    let tokenizers = [
        Tokenizer::Ascii,
        unicode.clone(),
        Tokenizer::Porter(Box::new(unicode)),
    ];
    for tokenizer in &tokenizers {
        let _ = Query::parse(data, tokenizer, &columns);
    }
});
