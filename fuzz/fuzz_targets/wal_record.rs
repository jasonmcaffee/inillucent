//! Fuzzes the WAL record decoder with arbitrary bytes.
//!
//! Invariant: a log record is bytes a crash wrote, so the decoder must produce a
//! `DbError` and never a panic, a wrong answer or an out-of-bounds read. This is
//! the same contract the page targets hold, and the log is where it matters
//! most: a record that decoded wrongly is applied to the data file without
//! anybody looking at it.
//!
//! The target does not merely decode one record: it walks the buffer as a
//! *stream*, exactly as recovery does, because the bug a single-record target
//! cannot find is the one where a record's declared length advances the cursor
//! to somewhere the next decode reads out of bounds. It also re-encodes what it
//! decoded and checks the two agree, which catches a decoder that accepts bytes
//! its own encoder could never have produced.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inillucent_wal::record::Record;

fuzz_target!(|data: &[u8]| {
    let mut at = 0usize;
    let mut seen = 0usize;
    loop {
        let Some(tail) = data.get(at..) else {
            return;
        };
        let record = match Record::decode(tail) {
            Ok(Some(record)) => record,
            Ok(None) | Err(_) => return,
        };
        // A record must declare a length that advances the cursor, or a stream
        // of them would not terminate. The decoder refuses a length below the
        // header size, so this is a claim about the decoder rather than a
        // defence against it - and a claim worth failing on.
        assert!(record.length >= 32, "a record decoded with no length");

        // Whatever the header claimed, reading the body must not panic.
        let _ = record.pages();

        // What was decoded must re-encode to the same bytes. A decoder that
        // accepted a record its encoder could not have written would be
        // accepting a shape the format does not define.
        // Not a condition: a record that decoded is already inside the ceiling
        // the encoder checks, so this cannot fail, and a failure would be a
        // finding rather than a case to skip.
        let mut again = Vec::new();
        record.encode(&mut again).expect("a decoded record re-encodes");
        let original = tail.get(..record.length).unwrap_or(&[]);
        assert_eq!(
            again.as_slice(),
            original,
            "a record did not re-encode to the bytes it was decoded from"
        );

        at = at.saturating_add(record.length);
        seen = seen.saturating_add(1);
        if seen > 4_096 {
            return;
        }
    }
});
