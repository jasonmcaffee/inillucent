//! DEFLATE and zlib, so an archive written by one engine is read by the other.
//!
//! Invariant: **what this decodes is exactly what RFC 1951 defines, and what it
//! encodes is something every decoder accepts.** The two halves have different
//! bars on purpose, and the difference is worth stating because it is the one
//! thing a caller can see:
//!
//! - **Decoding is a format contract.** A `.sqlar` row or a zip member written
//!   by SQLite, by `zip`, or by anything else is a stream this has to read
//!   byte for byte or the data is lost. Stored, fixed-Huffman and
//!   dynamic-Huffman blocks are all here, because a real archive has all three.
//! - **Encoding is a choice among many valid ones.** RFC 1951 does not say
//!   which matches a compressor must find or how it must split its blocks, so
//!   two conforming encoders produce different bytes for the same input and
//!   both are right. This one uses fixed Huffman codes over a hash-chain
//!   matcher: smaller than the input on anything repetitive, larger on nothing,
//!   and readable by zlib, by `unzip` and by SQLite.
//!
//! So a round trip through this is exact, a stream SQLite wrote reads exactly,
//! and a stream this wrote is *shorter or longer* than the one SQLite would
//! have written for the same input. `sqlar_compress` is the visible
//! consequence: it stores whichever of the two is smaller, so a row it wrote
//! may hold the raw bytes where SQLite's held a compressed copy, and
//! `sqlar_uncompress` reads either.

use crate::error::corrupt;
use crate::DbResult;

/// The largest distance a match may reach back.
const WINDOW: usize = 32_768;

/// The longest match the format can encode.
const LONGEST_MATCH: usize = 258;

/// The shortest match worth encoding, since a shorter one costs more than it saves.
const SHORTEST_MATCH: usize = 3;

/// How many earlier positions the matcher will look at for one hash.
///
/// A bound rather than a preference: without one a file of a single repeated
/// byte walks a chain as long as the file at every position.
const CHAIN_LIMIT: usize = 32;

/// The base length each length code stands for.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// How many extra bits each length code carries.
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// The base distance each distance code stands for.
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12_289, 16_385, 24_577,
];

/// How many extra bits each distance code carries.
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order the code-length code lengths are written in.
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Returns the raw DEFLATE stream decompressed.
///
/// @param input - the compressed bytes
pub fn inflate(input: &[u8]) -> DbResult<Vec<u8>> {
    let mut bits = BitReader::new(input);
    let mut out: Vec<u8> = Vec::with_capacity(input.len().saturating_mul(4));
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => stored_block(&mut bits, &mut out)?,
            1 => {
                let (literals, distances) = fixed_trees();
                huffman_block(&mut bits, &mut out, &literals, &distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_trees(&mut bits)?;
                huffman_block(&mut bits, &mut out, &literals, &distances)?;
            }
            _ => return Err(corrupt("deflate: a block type that does not exist")),
        }
        if last {
            return Ok(out);
        }
    }
}

/// Returns the input as a raw DEFLATE stream.
///
/// @param input - the bytes to compress
pub fn deflate(input: &[u8]) -> Vec<u8> {
    let mut bits = BitWriter::default();
    // One block, final, fixed Huffman. Splitting into several blocks with
    // per-block dynamic codes is what a tuned compressor does and what makes
    // its output smaller than this; it is also what makes two compressors
    // disagree, and this is the readable end of that trade.
    bits.push(1, 1);
    bits.push(1, 2);
    let mut chains = Chains::over(input);
    let mut at = 0usize;
    while at < input.len() {
        let (length, distance) = chains.longest(input, at);
        if length >= SHORTEST_MATCH {
            write_match(&mut bits, length, distance);
            for step in 0..length {
                chains.insert(input, at.saturating_add(step));
            }
            at = at.saturating_add(length);
            continue;
        }
        write_literal(&mut bits, input.get(at).copied().unwrap_or(0));
        chains.insert(input, at);
        at = at.saturating_add(1);
    }
    write_fixed(&mut bits, 256);
    bits.finish()
}

/// Returns a zlib stream decompressed, checking its header and checksum.
///
/// @param input - the zlib stream
pub fn zlib_decompress(input: &[u8]) -> DbResult<Vec<u8>> {
    let (Some(first), Some(second)) = (input.first(), input.get(1)) else {
        return Err(corrupt("zlib: the stream is too short to have a header"));
    };
    if first & 0x0f != 8 {
        return Err(corrupt("zlib: the compression method is not deflate"));
    }
    if (u16::from(*first) << 8 | u16::from(*second)) % 31 != 0 {
        return Err(corrupt("zlib: the header check bits are wrong"));
    }
    if second & 0x20 != 0 {
        return Err(corrupt("zlib: a preset dictionary is not supported"));
    }
    let body = input.get(2..).unwrap_or_default();
    let out = inflate(body)?;
    // The trailing Adler-32 is checked when it is there. A stream cut short of
    // its checksum has still decoded, and reporting the bytes is more useful
    // than refusing them - which is what `unzip -FF` exists for.
    if let Some(tail) = input.get(input.len().saturating_sub(4)..) {
        let mut expected = [0u8; 4];
        expected.copy_from_slice(tail);
        let found = adler32(&out);
        if u32::from_be_bytes(expected) != found && input.len() > 6 {
            return Err(corrupt("zlib: the checksum does not match the data"));
        }
    }
    Ok(out)
}

/// Returns the input as a zlib stream.
///
/// @param input - the bytes to compress
pub fn zlib_compress(input: &[u8]) -> Vec<u8> {
    // `78 9c`: deflate, a 32 KiB window, the default compression level. The
    // same two bytes zlib writes, so a decoder that switches on them sees what
    // it expects.
    let mut out = vec![0x78, 0x9c];
    out.extend_from_slice(&deflate(input));
    out.extend_from_slice(&adler32(input).to_be_bytes());
    out
}

/// Returns the Adler-32 checksum of a buffer.
///
/// @param data - the bytes to sum
pub fn adler32(data: &[u8]) -> u32 {
    let mut low = 1u32;
    let mut high = 0u32;
    for byte in data {
        low = (low.saturating_add(u32::from(*byte))) % 65_521;
        high = (high.saturating_add(low)) % 65_521;
    }
    (high << 16) | low
}

/// Copies a stored block, which carries its length and no codes at all.
///
/// @param bits - the stream being read
/// @param out - where the bytes go
fn stored_block(bits: &mut BitReader<'_>, out: &mut Vec<u8>) -> DbResult<()> {
    bits.align();
    let length = usize::from(bits.take_u16()?);
    let complement = bits.take_u16()?;
    if length != usize::from(!complement) {
        return Err(corrupt("deflate: a stored block's length is contradicted"));
    }
    for _ in 0..length {
        out.push(bits.take_byte()?);
    }
    Ok(())
}

/// Decodes one Huffman-coded block into the output.
///
/// @param bits - the stream being read
/// @param out - where the bytes go
/// @param literals - the literal and length tree
/// @param distances - the distance tree
fn huffman_block(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    literals: &Tree,
    distances: &Tree,
) -> DbResult<()> {
    loop {
        let symbol = literals.decode(bits)?;
        if symbol == 256 {
            return Ok(());
        }
        if symbol < 256 {
            out.push(symbol as u8);
            continue;
        }
        let index = usize::from(symbol.saturating_sub(257));
        let (Some(base), Some(extra)) = (LENGTH_BASE.get(index), LENGTH_EXTRA.get(index)) else {
            return Err(corrupt("deflate: a length code that does not exist"));
        };
        let length = usize::from(*base).saturating_add(bits.take(u32::from(*extra))? as usize);
        let code = usize::from(distances.decode(bits)?);
        let (Some(base), Some(extra)) = (DISTANCE_BASE.get(code), DISTANCE_EXTRA.get(code)) else {
            return Err(corrupt("deflate: a distance code that does not exist"));
        };
        let distance = usize::from(*base).saturating_add(bits.take(u32::from(*extra))? as usize);
        if distance == 0 || distance > out.len() {
            return Err(corrupt("deflate: a match reaches before the output"));
        }
        // Byte at a time, because the match may overlap its own output - which
        // is how a run of one byte is encoded, and is the reason this cannot be
        // a `copy_within`.
        let from = out.len().saturating_sub(distance);
        for step in 0..length {
            let byte = out.get(from.saturating_add(step)).copied().unwrap_or(0);
            out.push(byte);
        }
    }
}

/// Returns the two trees a fixed-Huffman block uses.
fn fixed_trees() -> (Tree, Tree) {
    let mut lengths = [0u8; 288];
    for (symbol, slot) in lengths.iter_mut().enumerate() {
        *slot = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    (Tree::from_lengths(&lengths), Tree::from_lengths(&[5u8; 30]))
}

/// Reads the two trees a dynamic-Huffman block declares.
///
/// @param bits - the stream being read
fn dynamic_trees(bits: &mut BitReader<'_>) -> DbResult<(Tree, Tree)> {
    let literal_count = (bits.take(5)? as usize).saturating_add(257);
    let distance_count = (bits.take(5)? as usize).saturating_add(1);
    let code_count = (bits.take(4)? as usize).saturating_add(4);
    let mut code_lengths = [0u8; 19];
    for at in 0..code_count {
        let Some(slot) = CODE_LENGTH_ORDER.get(at) else {
            break;
        };
        if let Some(held) = code_lengths.get_mut(*slot) {
            *held = bits.take(3)? as u8;
        }
    }
    let code_tree = Tree::from_lengths(&code_lengths);
    let total = literal_count.saturating_add(distance_count);
    let mut lengths: Vec<u8> = Vec::with_capacity(total);
    while lengths.len() < total {
        let symbol = code_tree.decode(bits)?;
        match symbol {
            0..=15 => lengths.push(symbol as u8),
            16 => {
                let Some(previous) = lengths.last().copied() else {
                    return Err(corrupt("deflate: a repeat with nothing to repeat"));
                };
                let count = (bits.take(2)? as usize).saturating_add(3);
                for _ in 0..count {
                    lengths.push(previous);
                }
            }
            17 => {
                let count = (bits.take(3)? as usize).saturating_add(3);
                lengths.resize(lengths.len().saturating_add(count), 0);
            }
            18 => {
                let count = (bits.take(7)? as usize).saturating_add(11);
                lengths.resize(lengths.len().saturating_add(count), 0);
            }
            _ => return Err(corrupt("deflate: a code-length symbol that does not exist")),
        }
    }
    lengths.truncate(total);
    let (literals, distances) = lengths.split_at(literal_count);
    Ok((Tree::from_lengths(literals), Tree::from_lengths(distances)))
}

/// Writes one literal byte with the fixed literal code.
///
/// @param bits - the stream being written
/// @param byte - the literal
fn write_literal(bits: &mut BitWriter, byte: u8) {
    write_fixed(bits, u16::from(byte));
}

/// Writes one back-reference with the fixed codes.
///
/// @param bits - the stream being written
/// @param length - how many bytes the match covers
/// @param distance - how far back it starts
fn write_match(bits: &mut BitWriter, length: usize, distance: usize) {
    let mut code = 0usize;
    for (at, base) in LENGTH_BASE.iter().enumerate() {
        if usize::from(*base) <= length {
            code = at;
        }
    }
    write_fixed(bits, (code as u16).saturating_add(257));
    let extra = LENGTH_EXTRA.get(code).copied().unwrap_or(0);
    let base = usize::from(LENGTH_BASE.get(code).copied().unwrap_or(3));
    if extra > 0 {
        bits.push(length.saturating_sub(base) as u32, u32::from(extra));
    }
    let mut code = 0usize;
    for (at, base) in DISTANCE_BASE.iter().enumerate() {
        if usize::from(*base) <= distance {
            code = at;
        }
    }
    // A distance code is five bits, most significant first, in the fixed tree.
    bits.push_reversed(code as u32, 5);
    let extra = DISTANCE_EXTRA.get(code).copied().unwrap_or(0);
    let base = usize::from(DISTANCE_BASE.get(code).copied().unwrap_or(1));
    if extra > 0 {
        bits.push(distance.saturating_sub(base) as u32, u32::from(extra));
    }
}

/// Writes one symbol with the fixed literal and length tree.
///
/// The four ranges RFC 1951 §3.2.6 tabulates, written most significant bit
/// first as every Huffman code in the format is.
///
/// @param bits - the stream being written
/// @param symbol - the symbol to write
fn write_fixed(bits: &mut BitWriter, symbol: u16) {
    match symbol {
        0..=143 => bits.push_reversed(u32::from(symbol).saturating_add(0x30), 8),
        144..=255 => bits.push_reversed(
            u32::from(symbol).saturating_sub(144).saturating_add(0x190),
            9,
        ),
        256..=279 => bits.push_reversed(u32::from(symbol).saturating_sub(256), 7),
        _ => bits.push_reversed(
            u32::from(symbol).saturating_sub(280).saturating_add(0xc0),
            8,
        ),
    }
}

/// The hash chains a match is looked up through.
struct Chains {
    /// The most recent position each three-byte hash was seen at.
    head: Vec<u32>,
    /// The previous position with the same hash, per position.
    previous: Vec<u32>,
}

/// How many buckets the hash table has.
const BUCKETS: usize = 1 << 15;

impl Chains {
    /// Returns an empty set of chains sized for one input.
    ///
    /// @param input - the bytes about to be compressed
    fn over(input: &[u8]) -> Chains {
        Chains {
            head: vec![u32::MAX; BUCKETS],
            previous: vec![u32::MAX; input.len().saturating_add(1)],
        }
    }

    /// Returns the bucket the three bytes at a position fall in.
    ///
    /// @param input - the bytes being compressed
    /// @param at - where to read from
    fn bucket(input: &[u8], at: usize) -> usize {
        let first = u32::from(input.get(at).copied().unwrap_or(0));
        let second = u32::from(input.get(at.saturating_add(1)).copied().unwrap_or(0));
        let third = u32::from(input.get(at.saturating_add(2)).copied().unwrap_or(0));
        let mixed = first.wrapping_mul(0x9e37_79b1)
            ^ second.wrapping_mul(0x85eb_ca6b)
            ^ third.wrapping_mul(0xc2b2_ae35);
        (mixed >> 17) as usize % BUCKETS
    }

    /// Records that a position exists, so later positions can match against it.
    ///
    /// @param input - the bytes being compressed
    /// @param at - the position to record
    fn insert(&mut self, input: &[u8], at: usize) {
        if at.saturating_add(SHORTEST_MATCH) > input.len() {
            return;
        }
        let bucket = Chains::bucket(input, at);
        let Some(head) = self.head.get_mut(bucket) else {
            return;
        };
        if let Some(slot) = self.previous.get_mut(at) {
            *slot = *head;
        }
        *head = at as u32;
    }

    /// Returns the longest match at a position, and how far back it starts.
    ///
    /// @param input - the bytes being compressed
    /// @param at - where the match would begin
    fn longest(&self, input: &[u8], at: usize) -> (usize, usize) {
        if at.saturating_add(SHORTEST_MATCH) > input.len() {
            return (0, 0);
        }
        let bucket = Chains::bucket(input, at);
        let mut candidate = self.head.get(bucket).copied().unwrap_or(u32::MAX);
        let mut best = (0usize, 0usize);
        let mut looked = 0usize;
        while candidate != u32::MAX && looked < CHAIN_LIMIT {
            let start = candidate as usize;
            if start >= at || at.saturating_sub(start) > WINDOW {
                break;
            }
            let mut length = 0usize;
            while length < LONGEST_MATCH
                && at.saturating_add(length) < input.len()
                && input.get(start.saturating_add(length)) == input.get(at.saturating_add(length))
            {
                length = length.saturating_add(1);
            }
            if length > best.0 {
                best = (length, at.saturating_sub(start));
                if length >= LONGEST_MATCH {
                    break;
                }
            }
            candidate = self.previous.get(start).copied().unwrap_or(u32::MAX);
            looked = looked.saturating_add(1);
        }
        best
    }
}

/// A least-significant-bit-first reader over a byte stream.
struct BitReader<'a> {
    bytes: &'a [u8],
    at: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    /// Returns a reader over a buffer.
    fn new(bytes: &'a [u8]) -> BitReader<'a> {
        BitReader {
            bytes,
            at: 0,
            bit: 0,
        }
    }

    /// Reads one bit, least significant first within each byte.
    fn bit(&mut self) -> DbResult<u32> {
        let Some(byte) = self.bytes.get(self.at) else {
            return Err(corrupt("deflate: the stream ends inside a code"));
        };
        let value = u32::from(*byte >> self.bit) & 1;
        self.bit = self.bit.saturating_add(1);
        if self.bit == 8 {
            self.bit = 0;
            self.at = self.at.saturating_add(1);
        }
        Ok(value)
    }

    /// Reads several bits as a little-endian integer.
    ///
    /// @param count - how many bits
    fn take(&mut self, count: u32) -> DbResult<u32> {
        let mut value = 0u32;
        for step in 0..count {
            value |= self.bit()? << step;
        }
        Ok(value)
    }

    /// Moves to the next byte boundary.
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.at = self.at.saturating_add(1);
        }
    }

    /// Reads one whole byte, which is only valid on a boundary.
    fn take_byte(&mut self) -> DbResult<u8> {
        let Some(byte) = self.bytes.get(self.at) else {
            return Err(corrupt("deflate: the stream ends inside a stored block"));
        };
        self.at = self.at.saturating_add(1);
        Ok(*byte)
    }

    /// Reads a little-endian sixteen-bit integer on a boundary.
    fn take_u16(&mut self) -> DbResult<u16> {
        let low = u16::from(self.take_byte()?);
        let high = u16::from(self.take_byte()?);
        Ok(low | (high << 8))
    }
}

/// A least-significant-bit-first writer.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    partial: u32,
    held: u32,
}

impl BitWriter {
    /// Writes an integer, least significant bit first.
    ///
    /// @param value - the bits
    /// @param count - how many of them
    fn push(&mut self, value: u32, count: u32) {
        for step in 0..count {
            self.push_bit((value >> step) & 1);
        }
    }

    /// Writes an integer, most significant bit first, as a Huffman code is.
    ///
    /// @param value - the code
    /// @param count - how many bits it is
    fn push_reversed(&mut self, value: u32, count: u32) {
        for step in (0..count).rev() {
            self.push_bit((value >> step) & 1);
        }
    }

    /// Writes one bit.
    fn push_bit(&mut self, bit: u32) {
        self.partial |= (bit & 1) << self.held;
        self.held = self.held.saturating_add(1);
        if self.held == 8 {
            self.bytes.push(self.partial as u8);
            self.partial = 0;
            self.held = 0;
        }
    }

    /// Returns the stream, padding the last byte with zeroes.
    fn finish(mut self) -> Vec<u8> {
        if self.held > 0 {
            self.bytes.push(self.partial as u8);
        }
        self.bytes
    }
}

/// A canonical Huffman decoding table.
struct Tree {
    /// How many codes there are of each length, indexed by length.
    counts: [u16; 16],
    /// The symbols, ordered by code length and then by symbol.
    symbols: Vec<u16>,
}

impl Tree {
    /// Returns the canonical tree a list of code lengths describes.
    ///
    /// @param lengths - one code length per symbol, zero for an absent symbol
    fn from_lengths(lengths: &[u8]) -> Tree {
        let mut counts = [0u16; 16];
        for length in lengths {
            if let Some(slot) = counts.get_mut(usize::from(*length)) {
                *slot = slot.saturating_add(1);
            }
        }
        if let Some(zero) = counts.first_mut() {
            *zero = 0;
        }
        let mut offsets = [0u16; 16];
        let mut running = 0u16;
        // Skipping length zero, which means "this symbol has no code" and whose
        // offset is never read.
        for (length, slot) in offsets.iter_mut().enumerate().skip(1) {
            *slot = running;
            running = running.saturating_add(counts.get(length).copied().unwrap_or(0));
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, length) in lengths.iter().enumerate() {
            if *length == 0 {
                continue;
            }
            let Some(offset) = offsets.get_mut(usize::from(*length)) else {
                continue;
            };
            if let Some(slot) = symbols.get_mut(usize::from(*offset)) {
                *slot = symbol as u16;
            }
            *offset = offset.saturating_add(1);
        }
        Tree { counts, symbols }
    }

    /// Reads one symbol out of the stream.
    ///
    /// The canonical decode: walk the lengths, tracking the first code of each
    /// length and how many there are, until the code so far is one of them.
    ///
    /// @param bits - the stream being read
    fn decode(&self, bits: &mut BitReader<'_>) -> DbResult<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for length in 1..16 {
            code |= bits.bit()? as i32;
            let count = i32::from(self.counts.get(length).copied().unwrap_or(0));
            if code.saturating_sub(first) < count {
                let at = index.saturating_add(code.saturating_sub(first));
                let Some(symbol) = usize::try_from(at).ok().and_then(|at| self.symbols.get(at))
                else {
                    return Err(corrupt("deflate: a code with no symbol"));
                };
                return Ok(*symbol);
            }
            index = index.saturating_add(count);
            first = first.saturating_add(count).saturating_mul(2);
            code = code.saturating_mul(2);
        }
        Err(corrupt("deflate: a code longer than fifteen bits"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip is the property everything else rests on.
    #[test]
    fn what_is_deflated_inflates_to_itself() {
        for case in [
            Vec::new(),
            b"a".to_vec(),
            b"hello world".to_vec(),
            b"hello world hello world hello world".to_vec(),
            vec![0u8; 4096],
            (0..=255u8).cycle().take(10_000).collect(),
        ] {
            let packed = deflate(&case);
            assert_eq!(inflate(&packed).expect("it inflates"), case);
        }
    }

    /// Repetition is what a compressor is for, and this one has to earn it.
    #[test]
    fn repetition_gets_smaller() {
        let case = b"the quick brown fox ".repeat(200);
        let packed = deflate(&case);
        assert!(
            packed.len() < case.len() / 8,
            "4,000 bytes of twenty-byte repeats compressed to {}",
            packed.len()
        );
    }

    /// The zlib wrapper is a header, the stream and an Adler-32.
    #[test]
    fn a_zlib_stream_round_trips_and_carries_its_checksum() {
        let case = b"sqlar stores its rows this way".repeat(20);
        let packed = zlib_compress(&case);
        assert_eq!(packed.first(), Some(&0x78));
        assert_eq!(packed.get(1), Some(&0x9c));
        assert_eq!(zlib_decompress(&packed).expect("it decompresses"), case);
        let mut damaged = packed.clone();
        let last = damaged.len().saturating_sub(1);
        damaged[last] ^= 0xff;
        assert!(
            zlib_decompress(&damaged).is_err(),
            "a bad checksum is caught"
        );
    }

    /// Adler-32 against the values RFC 1950 and zlib's own tests publish.
    #[test]
    fn adler_matches_the_published_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    /// **A stream zlib wrote, not one this wrote.** The bytes are what SQLite's
    /// `sqlar_compress(zeroblob(1000))` produced, read out of the reference
    /// shell, and reading them is the whole reason the decoder exists.
    #[test]
    fn a_stream_the_reference_wrote_decodes() {
        let packed = [
            0x78u8, 0x9C, 0x63, 0x60, 0x18, 0x05, 0xA3, 0x60, 0x14, 0x0C, 0x77, 0x00, 0x00, 0x03,
            0xE8, 0x00, 0x01,
        ];
        assert_eq!(
            zlib_decompress(&packed).expect("it decompresses"),
            vec![0u8; 1000]
        );
    }
}
