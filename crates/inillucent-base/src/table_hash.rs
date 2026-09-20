//! The hasher this engine's own hash tables use.
//!
//! Invariant: **it is chosen for speed over short byte strings and for nothing
//! else.** Every input is a key this engine encoded itself - a memcmp-comparable
//! key from `inillucent_tree::key`, a page number, a tree identifier - so nothing
//! here has to resist a deliberate collision, and a hash table keyed on one is
//! not a surface an attacker chooses the keys of.
//!
//! **Why not the standard library's.** `std::collections::HashMap` defaults to
//! SipHash 1-3 with a per-process random key, which is the right default for a
//! map whose keys may come from a request and the wrong one here. SipHash
//! processes eight bytes per round with a four-round finalisation, so a
//! four-byte key costs about as much as a thirty-two byte one; `scan.distinct`
//! hashes a short encoded key once per row over a hundred thousand rows and
//! keeps sixty-four of them, and the hashing was measurable against the
//! comparison it was avoiding (task-2000, design 4).
//!
//! **What this is.** The FxHash construction with one addition: fold each chunk of
//! the input into a 64-bit accumulator with a rotate, an exclusive-or and a
//! multiplication by a large odd constant, and multiply the incoming chunk by a
//! second odd constant on the way in. Two multiplications per eight bytes, no
//! finalisation round, and the multiplies' high bits carry into the low ones so
//! the table's own masking sees mixed bits.
//!
//! **The second multiplication is not decoration - it was measured.** Without it,
//! plain FxHash, the keys this engine actually produces collide within the first
//! few thousand: `0x0a` followed by the big-endian bytes of an integer, which is
//! what `inillucent_tree::key` encodes a rowid as, collided at 2,048 with 31 on
//! the first sweep. The reason is structural rather than unlucky. The accumulator
//! is rotated five places between chunks, so the top five bits of one chunk land
//! in the low eight bits of the next fold - which is exactly where a nine-byte
//! key's one-byte tail lives, so two keys whose leading word differs only in its
//! top byte can cancel against a tail that differs by the same amount.
//! Multiplying each chunk by [`WORD`] before it is folded spreads a one-byte
//! difference over all sixty-four bits, so the cancellation has nothing to line
//! up with. Re-swept afterwards: two hundred thousand keys of each of five shapes
//! - the rowid encoding, a bare little-endian integer, `row-<n>` text, every
//! three-byte string, and a two-column key - all distinct.
//!
//! **What it is not good at, stated so nobody reaches for it wrongly.** It is
//! not a checksum - `crate::checksum` is, and it is what a page carries. It is
//! not a digest - `crate::hash::Sha256` is, and it is what the migration
//! manifest records. And it is not collision resistant in any sense: two keys
//! differing only in their top bits can land in the same bucket, which costs a
//! comparison and never a wrong answer, because a hash table compares the keys
//! it finds.

use std::hash::{BuildHasher, Hasher};

/// The multiplier: 2^64 divided by the golden ratio, rounded to an odd number.
///
/// Odd so the multiplication is invertible modulo 2^64, which is what makes it a
/// permutation rather than a map that loses information. The particular value is
/// the one FxHash uses and rustc's own tables use, so the distribution it
/// produces over short keys is measured by somebody.
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The multiplier applied to each chunk on the way in.
///
/// 2^64 divided by the golden ratio again, at a different rounding - it only has
/// to be odd and unrelated to [`SEED`]. See the module header for the collision
/// this exists to prevent.
const WORD: u64 = 0x9E37_79B9_7F4A_7C15;

/// How far the accumulator is rotated before each fold.
///
/// Five is FxHash's. It moves the previous chunk's high bits down into the
/// region the next multiplication mixes, which is what stops a long key's early
/// bytes being washed out by the ones after them.
const ROTATE: u32 = 5;

/// A hasher that folds its input eight bytes at a time.
///
/// See the module header for what it is for and what it is not for.
#[derive(Clone, Copy, Debug)]
pub struct TableHasher {
    /// What has been folded in so far.
    state: u64,
}

impl Default for TableHasher {
    /// Starts at [`SEED`] rather than at zero.
    ///
    /// **Zero is a fixed point of the fold.** `(0.rotate_left(5) ^ 0 * WORD) * SEED`
    /// is zero, so a hasher starting at zero and fed any number of zero bytes stays
    /// at zero - which makes an empty key, a key of one zero byte and a key of eight
    /// of them all hash alike, and an all-zero key is a shape a fixed-width column
    /// encoding produces constantly. Any non-zero start removes the fixed point;
    /// this one is a constant the module already has.
    fn default() -> TableHasher {
        TableHasher { state: SEED }
    }
}

impl TableHasher {
    /// Folds one word in.
    ///
    /// @param word - the bytes to fold, already widened
    #[inline]
    fn fold(&mut self, word: u64) {
        self.state = self.state.rotate_left(ROTATE) ^ word.wrapping_mul(WORD);
        self.state = self.state.wrapping_mul(SEED);
    }
}

impl Hasher for TableHasher {
    /// Folds a byte string in, eight bytes at a time and then the remainder.
    ///
    /// The remainder is folded as one zero-extended word rather than byte by byte,
    /// so a thirteen-byte key costs two folds rather than six.
    ///
    /// **The length is folded in afterwards, as a word of its own.** Zero
    /// extending the tail makes `[1]` and `[1, 0]` the same word, and a key
    /// encoding reaches that pair constantly; folding the length separately tells
    /// them apart without putting the length where a data byte can cancel against
    /// it, which is the mistake the module header records.
    ///
    /// @param bytes - the bytes to fold
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(head) = rest.get(..8) {
            let mut word = [0u8; 8];
            word.copy_from_slice(head);
            self.fold(u64::from_le_bytes(word));
            rest = rest.get(8..).unwrap_or(&[]);
        }
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            if let Some(slot) = word.get_mut(..rest.len()) {
                slot.copy_from_slice(rest);
            }
            self.fold(u64::from_le_bytes(word));
        }
        self.fold(bytes.len() as u64);
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.fold(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.fold(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.fold(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.fold(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }
}

/// The `BuildHasher` a `HashMap` or `HashSet` is parameterised with.
///
/// `HashSet<Vec<u8>, BuildTableHasher>` is the shape every caller wants; the
/// aliases below spell it so the call sites do not have to.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildTableHasher;

impl BuildHasher for BuildTableHasher {
    type Hasher = TableHasher;

    fn build_hasher(&self) -> TableHasher {
        TableHasher::default()
    }
}

/// A `HashMap` over keys this engine encoded itself.
pub type TableMap<K, V> = std::collections::HashMap<K, V, BuildTableHasher>;

/// A `HashSet` over keys this engine encoded itself.
pub type TableSet<K> = std::collections::HashSet<K, BuildTableHasher>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the hash of one byte string.
    ///
    /// @param bytes - the input
    fn hashed(bytes: &[u8]) -> u64 {
        let mut hasher = TableHasher::default();
        hasher.write(bytes);
        hasher.finish()
    }

    /// The keys this engine produces do not collide, over five shapes.
    ///
    /// **The sweep that found the flaw in plain FxHash** - see the module header
    /// for what the flaw was. Two hundred thousand keys of each shape rather than a
    /// round number for its own sake: the failure was at 2,048, and a sweep that
    /// stopped at a thousand would have reported a hasher that does not work as one
    /// that does.
    ///
    /// Not a collision-resistance claim. A collision costs a comparison and never a
    /// wrong answer, because a hash table compares the keys it finds. What this
    /// asserts is that a table of realistic keys does not degenerate.
    #[test]
    fn the_key_shapes_this_engine_produces_do_not_collide() {
        let shapes: [(&str, fn(u64) -> Vec<u8>); 5] = [
            ("a rowid key", |value| {
                let mut key = vec![0x0au8];
                key.extend_from_slice(&value.to_be_bytes());
                key
            }),
            ("a bare little-endian integer", |value| {
                value.to_le_bytes().to_vec()
            }),
            ("text", |value| format!("row-{value}").into_bytes()),
            ("three bytes", |value| {
                (value as u32)
                    .to_be_bytes()
                    .get(1..)
                    .unwrap_or(&[])
                    .to_vec()
            }),
            ("a two-column key", |value| {
                let mut key = vec![0x0au8];
                key.extend_from_slice(&(value % 64).to_be_bytes());
                key.push(0x0a);
                key.extend_from_slice(&(value / 64).to_be_bytes());
                key
            }),
        ];
        for (name, shape) in shapes {
            let mut seen = std::collections::HashMap::new();
            for value in 0..200_000u64 {
                let key = shape(value);
                if let Some(earlier) = seen.insert(hashed(&key), value) {
                    panic!("{name}: {value} hashes the same as {earlier}");
                }
            }
        }
    }

    /// A shorter string and a zero-extended one are different inputs.
    ///
    /// The length is folded in for exactly this: without it `[1]` and `[1, 0]` would
    /// both be the word `1` and would hash alike, which is a degenerate case a key
    /// encoding reaches all the time.
    #[test]
    fn a_zero_extended_key_is_not_the_shorter_one() {
        assert_ne!(hashed(&[1]), hashed(&[1, 0]));
        assert_ne!(hashed(&[1, 0]), hashed(&[1, 0, 0]));
        assert_ne!(hashed(b"a"), hashed(b"a\0\0\0\0\0\0\0"));
    }

    /// Zero bytes are not a fixed point.
    ///
    /// **The reason the initial state is not zero** - see `Default for TableHasher`.
    /// An empty key, one zero byte and eight of them all have to be different, and a
    /// hasher starting at zero made them the same number, because zero folds to
    /// zero. `DISTINCT` over zero compared columns encodes an empty key and a
    /// fixed-width column of zeros encodes a run of them, so both shapes are real.
    #[test]
    fn a_run_of_zero_bytes_is_not_a_fixed_point() {
        let empty = hashed(&[]);
        let one = hashed(&[0]);
        let eight = hashed(&[0; 8]);
        let nine = hashed(&[0; 9]);
        assert_ne!(empty, one);
        assert_ne!(one, eight);
        assert_ne!(eight, nine);
        assert_ne!(empty, eight);
    }

    /// A word folded directly and the same word folded as eight bytes differ only
    /// by the length fold `write` adds.
    ///
    /// `Hash for u64` calls `write_u64`, and a `HashMap<u64, _>` therefore never
    /// reaches `write`; a `HashMap<Vec<u8>, _>` only ever reaches `write`. Nothing
    /// mixes the two, so they do not have to agree - but the difference has to be
    /// the length fold and nothing else, which is what says `write_u64` folds a
    /// value the same way `write` folds a chunk.
    #[test]
    fn a_word_and_its_bytes_differ_only_by_the_length() {
        let value = 0x0102_0304_0506_0708u64;
        let mut words = TableHasher::default();
        words.write_u64(value);
        words.fold(8);
        let mut bytes = TableHasher::default();
        bytes.write(&value.to_le_bytes());
        assert_eq!(words.finish(), bytes.finish());
    }

    /// The aliases build a table that works.
    ///
    /// One case rather than none, because `TableMap` and `TableSet` are what every
    /// caller names and a type alias that does not compile into a usable table is a
    /// thing a doc comment cannot catch.
    #[test]
    fn the_aliases_make_a_working_table() {
        let mut map: TableMap<Vec<u8>, u32> = TableMap::default();
        map.insert(b"one".to_vec(), 1);
        map.insert(b"two".to_vec(), 2);
        assert_eq!(map.get(b"one".as_slice()), Some(&1));
        assert_eq!(map.get(b"three".as_slice()), None);
        let mut set: TableSet<Vec<u8>> = TableSet::default();
        assert!(set.insert(b"one".to_vec()));
        assert!(!set.insert(b"one".to_vec()));
    }
}
