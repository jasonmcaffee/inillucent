//! SHA-256.
//!
//! Invariant: the function matches its published test vectors, which are
//! checked below rather than assumed.
//!
//! It is here rather than pulled in from a crate because it is part of a
//! contract that must not change when a dependency is upgraded: the migration
//! tool records a digest of every source section it copied and of every target
//! table it wrote, and a rollback decision is made by comparing those digests
//! against a fresh pass. A hash that quietly changed between releases would
//! turn every retained manifest into a false alarm.
//!
//! It is not used for anything secret and nothing here needs to resist a
//! deliberate collision - the inputs are this engine's own files - so a plain,
//! readable implementation is the right one.

/// The SHA-256 round constants.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The initial state, which is the fractional part of the square roots of the
/// first eight primes.
const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A SHA-256 that can be fed in pieces.
///
/// Streaming rather than one-shot because the things being hashed here are
/// files and table scans: reading a four-gigabyte index into memory to digest
/// it would defeat the point of a bounded migration.
#[derive(Clone, Debug)]
pub struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    filled: usize,
    length: u64,
}

impl Default for Sha256 {
    /// Returns an empty hasher.
    fn default() -> Sha256 {
        Sha256::new()
    }
}

impl Sha256 {
    /// Returns a hasher over nothing.
    pub fn new() -> Sha256 {
        Sha256 {
            state: INITIAL,
            buffer: [0u8; 64],
            filled: 0,
            length: 0,
        }
    }

    /// Adds bytes to the hash.
    pub fn update(&mut self, data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        let mut rest = data;
        while !rest.is_empty() {
            let room = 64usize.saturating_sub(self.filled);
            let take = room.min(rest.len());
            let Some(head) = rest.get(..take) else {
                return;
            };
            if let Some(slot) = self
                .buffer
                .get_mut(self.filled..self.filled.saturating_add(take))
            {
                slot.copy_from_slice(head);
            }
            self.filled = self.filled.saturating_add(take);
            rest = rest.get(take..).unwrap_or(&[]);
            if self.filled == 64 {
                let block = self.buffer;
                compress(&mut self.state, &block);
                self.filled = 0;
            }
        }
    }

    /// Finishes the hash and returns the digest.
    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.length.wrapping_mul(8);
        self.update_raw(&[0x80]);
        while self.filled != 56 {
            self.update_raw(&[0]);
        }
        self.update_raw(&bits.to_be_bytes());
        let mut digest = [0u8; 32];
        for (index, word) in self.state.iter().enumerate() {
            let bytes = word.to_be_bytes();
            for (offset, byte) in bytes.iter().enumerate() {
                if let Some(slot) = digest.get_mut(index.saturating_mul(4).saturating_add(offset)) {
                    *slot = *byte;
                }
            }
        }
        digest
    }

    /// Adds padding bytes without counting them in the length.
    fn update_raw(&mut self, data: &[u8]) {
        let length = self.length;
        self.update(data);
        self.length = length;
    }

    /// Finishes the hash and returns it as lowercase hexadecimal.
    pub fn hex(self) -> String {
        to_hex(&self.finish())
    }
}

/// Returns the SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finish()
}

/// Returns the SHA-256 digest of `data` as lowercase hexadecimal.
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&sha256(data))
}

/// Renders bytes as lowercase hexadecimal.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Runs one compression round over a 64-byte block.
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0u32; 64];
    for index in 0usize..16 {
        let start = index.saturating_mul(4);
        let mut word = [0u8; 4];
        if let Some(slice) = block.get(start..start.saturating_add(4)) {
            word.copy_from_slice(slice);
        }
        if let Some(entry) = schedule.get_mut(index) {
            *entry = u32::from_be_bytes(word);
        }
    }
    for index in 16usize..64 {
        let read = |offset: usize| schedule.get(offset).copied().unwrap_or(0);
        let previous = read(index.saturating_sub(15));
        let recent = read(index.saturating_sub(2));
        let s0 = previous.rotate_right(7) ^ previous.rotate_right(18) ^ (previous >> 3);
        let s1 = recent.rotate_right(17) ^ recent.rotate_right(19) ^ (recent >> 10);
        let value = read(index.saturating_sub(16))
            .wrapping_add(s0)
            .wrapping_add(read(index.saturating_sub(7)))
            .wrapping_add(s1);
        if let Some(entry) = schedule.get_mut(index) {
            *entry = value;
        }
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0usize..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(choose)
            .wrapping_add(K.get(index).copied().unwrap_or(0))
            .wrapping_add(schedule.get(index).copied().unwrap_or(0));
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    let round = [a, b, c, d, e, f, g, h];
    for (slot, value) in state.iter_mut().zip(round.iter()) {
        *slot = slot.wrapping_add(*value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published vectors, which are what "this is SHA-256" means.
    #[test]
    fn the_published_vectors_match() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Feeding the same bytes in pieces gives the same digest as one call,
    /// which is the whole point of the streaming form.
    #[test]
    fn a_streamed_digest_matches_a_single_call() {
        let data: Vec<u8> = (0..1000u32).map(|value| value as u8).collect();
        let mut hasher = Sha256::new();
        for chunk in data.chunks(7) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.hex(), sha256_hex(&data));
    }

    /// A digest is sensitive to the whole input, including its length.
    #[test]
    fn a_longer_input_hashes_differently() {
        assert_ne!(sha256_hex(b"a"), sha256_hex(b"a\0"));
        assert_ne!(sha256_hex(&[0u8; 64]), sha256_hex(&[0u8; 65]));
    }
}
