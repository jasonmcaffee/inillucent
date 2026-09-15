//! The seeded twin of the write-ahead log's record fuzz target.
//!
//! Invariant: **a log record decodes to a value or an error, and whatever
//! decodes re-encodes to the bytes it came from.** A log record is bytes a
//! crash wrote, and it is the one codec whose output is applied to the data
//! file without anybody looking at it - a record that decoded wrongly is a page
//! written wrongly, during recovery, with no reader in the loop.
//!
//! The stable-toolchain twin of `fuzz/fuzz_targets/wal_record.rs`, written for
//! task-1961's T5. Like that target it walks the buffer as a *stream* rather
//! than decoding one record, because the defect a single-record sweep cannot
//! find is the one where a record's declared length advances the cursor to
//! somewhere the next decode reads out of bounds.

use inillucent_wal::record::{Body, Record};

/// How many inputs each sweep below reads.
const CASES: usize = 20_000;

/// Returns the next value of a deterministic generator.
///
/// @param state - the generator's state, advanced in place
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Walks a buffer as a stream of records, the way recovery does.
///
/// Returns how many records decoded, so a caller can assert the sweep read
/// something rather than refusing at the first byte every time.
///
/// @param data - the buffer to walk
fn walk(data: &[u8]) -> usize {
    let mut at = 0usize;
    let mut seen = 0usize;
    loop {
        let Some(tail) = data.get(at..) else {
            return seen;
        };
        let record = match Record::decode(tail) {
            Ok(Some(record)) => record,
            Ok(None) | Err(_) => return seen,
        };
        // A record must declare a length that advances the cursor, or a stream
        // of them would not terminate. The decoder refuses a length below the
        // header size, so this is a claim about the decoder rather than a
        // defence against it - and a claim worth failing on.
        assert!(record.length >= 32, "a record decoded with no length");
        let _ = record.pages();
        // What was decoded must re-encode to the same bytes. A decoder that
        // accepted a record its own encoder could not have written would be
        // accepting a shape the format does not define.
        let mut again = Vec::new();
        record
            .encode(&mut again)
            .expect("a decoded record re-encodes");
        assert_eq!(
            again.as_slice(),
            tail.get(..record.length).unwrap_or(&[]),
            "a record did not re-encode to the bytes it was decoded from"
        );
        at = at.saturating_add(record.length);
        seen = seen.saturating_add(1);
        if seen > 4_096 {
            return seen;
        }
    }
}

/// The record decoder is total over arbitrary bytes.
#[test]
fn the_record_decoder_is_total_over_arbitrary_bytes() {
    let mut state = 0x1961_0031_u64;
    for _ in 0..CASES {
        let length = (next(&mut state) as usize) % 256;
        let data: Vec<u8> = (0..length).map(|_| next(&mut state) as u8).collect();
        let _ = walk(&data);
    }
}

/// A record the encoder wrote survives being decoded and encoded again.
///
/// **The half a blind sweep cannot reach.** Random bytes almost never form a
/// record that decodes, so the sweep above is mostly a test of the refusal
/// path. This one starts from records the encoder produced, walks them as a
/// stream, and then mutates one byte at a time - which is the case where a
/// decoder believes a length field it should have checked.
#[test]
fn a_written_record_survives_a_mutated_byte() {
    let mut state = 0x1961_0032_u64;
    let mut decoded = 0usize;
    for _ in 0..CASES / 20 {
        let mut written = Vec::new();
        for index in 0..4u64 {
            let record = Record {
                lsn: next(&mut state),
                txn: next(&mut state),
                body: match index % 4 {
                    0 => Body::Commit {
                        cts: next(&mut state),
                    },
                    1 => Body::Abort,
                    2 => Body::AllocPage {
                        page: next(&mut state),
                    },
                    _ => Body::FreePage {
                        page: next(&mut state),
                    },
                },
                // Filled in by `encode`, which writes the header it computed
                // rather than the one it was handed.
                length: 0,
            };
            if record.encode(&mut written).is_err() {
                break;
            }
        }
        if written.is_empty() {
            continue;
        }
        decoded = decoded.saturating_add(walk(&written));
        let at = (next(&mut state) as usize) % written.len();
        if let Some(slot) = written.get_mut(at) {
            *slot ^= next(&mut state) as u8;
        }
        // Whatever the mutation did, the walk is total: it decodes, or it
        // stops. What it may not do is panic or read past the buffer.
        let _ = walk(&written);
    }
    assert!(
        decoded > 0,
        "no record the encoder wrote decoded again, so the sweep read nothing it wrote"
    );
}
