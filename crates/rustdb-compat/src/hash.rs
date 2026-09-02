//! SHA-256 and SHA3-256.
//!
//! Invariant: both functions match their published test vectors, which are
//! checked below rather than assumed.
//!
//! These are here rather than pulled in from a crate for two reasons. The
//! evidence model hashes artifacts and the reference metadata pins SQLite's
//! published SHA3-256 sums, so both algorithms are part of a contract that must
//! not change when a dependency is upgraded; and neither is used for anything
//! secret, so there is no argument for a hardened implementation. They are
//! test-only either way: `rustdb-compat` never enters a production graph.

/// The SHA-256 round constants.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Returns the SHA-256 digest of `data` as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&sha256(data))
}

/// Returns the SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut padded = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());
    let (blocks, _) = padded.as_chunks::<64>();
    for block in blocks {
        compress_sha256(&mut state, block);
    }
    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        for (offset, byte) in bytes.iter().enumerate() {
            if let Some(slot) = digest.get_mut(index * 4 + offset) {
                *slot = *byte;
            }
        }
    }
    digest
}

/// Runs one SHA-256 compression round over a 64-byte block.
fn compress_sha256(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0u32; 64];
    let (words, _) = block.as_chunks::<4>();
    for (index, chunk) in words.iter().enumerate().take(16) {
        if let Some(entry) = schedule.get_mut(index) {
            *entry = u32::from_be_bytes(*chunk);
        }
    }
    for index in 16..64 {
        let read = |offset: usize| schedule.get(offset).copied().unwrap_or(0);
        let s0 = read(index - 15).rotate_right(7)
            ^ read(index - 15).rotate_right(18)
            ^ (read(index - 15) >> 3);
        let s1 = read(index - 2).rotate_right(17)
            ^ read(index - 2).rotate_right(19)
            ^ (read(index - 2) >> 10);
        let value = read(index - 16)
            .wrapping_add(s0)
            .wrapping_add(read(index - 7))
            .wrapping_add(s1);
        if let Some(entry) = schedule.get_mut(index) {
            *entry = value;
        }
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(choose)
            .wrapping_add(SHA256_K.get(index).copied().unwrap_or(0))
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

/// The Keccak-f[1600] round constants.
const KECCAK_ROUNDS: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808a,
    0x8000000080008000,
    0x000000000000808b,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008a,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000a,
    0x000000008000808b,
    0x800000000000008b,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800a,
    0x800000008000000a,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
];

/// The rotation offsets of the Keccak rho step, in the order the pi step
/// visits the lanes.
const KECCAK_ROTATIONS: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];

/// The lane the pi step moves each rotated lane to.
const KECCAK_PI_LANES: [usize; 24] = [
    10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
];

/// Returns the SHA3-256 digest of `data` as lowercase hex.
///
/// SQLite publishes this hash for every release artifact, so the reference
/// pinning checks a download against the same function the project used.
pub fn sha3_256_hex(data: &[u8]) -> String {
    to_hex(&sha3_256(data))
}

/// Returns the SHA3-256 digest of `data`.
pub fn sha3_256(data: &[u8]) -> [u8; 32] {
    let rate = 136usize;
    let mut state = [0u64; 25];
    let mut offset = 0usize;
    while offset + rate <= data.len() {
        if let Some(block) = data.get(offset..offset + rate) {
            absorb(&mut state, block);
            keccak_f(&mut state);
        }
        offset = offset.saturating_add(rate);
    }
    let mut last = vec![0u8; rate];
    let tail = data.get(offset..).unwrap_or(&[]);
    for (slot, byte) in last.iter_mut().zip(tail.iter()) {
        *slot = *byte;
    }
    if let Some(slot) = last.get_mut(tail.len()) {
        *slot = 0x06;
    }
    if let Some(slot) = last.get_mut(rate - 1) {
        *slot |= 0x80;
    }
    absorb(&mut state, &last);
    keccak_f(&mut state);
    let mut digest = [0u8; 32];
    for lane in 0..4 {
        let bytes = state.get(lane).copied().unwrap_or(0).to_le_bytes();
        for (offset, byte) in bytes.iter().enumerate() {
            if let Some(slot) = digest.get_mut(lane * 8 + offset) {
                *slot = *byte;
            }
        }
    }
    digest
}

/// Exclusive-ors one rate-sized block into the state.
fn absorb(state: &mut [u64; 25], block: &[u8]) {
    let (words, _) = block.as_chunks::<8>();
    for (index, chunk) in words.iter().enumerate() {
        if let Some(lane) = state.get_mut(index) {
            *lane ^= u64::from_le_bytes(*chunk);
        }
    }
}

/// Reads one lane, returning zero for an index the state does not have.
fn lane(state: &[u64; 25], index: usize) -> u64 {
    state.get(index).copied().unwrap_or(0)
}

/// Runs the 24 rounds of the Keccak-f[1600] permutation.
///
/// The state is indexed as `x + 5 * y`, which is the layout the published
/// pseudo-code uses; the rotation and lane tables above are in that same
/// convention, so the three steps below are transcriptions of the specification
/// rather than a re-derivation of it.
fn keccak_f(state: &mut [u64; 25]) {
    for round in KECCAK_ROUNDS {
        theta(state);
        rho_and_pi(state);
        chi(state);
        if let Some(first) = state.get_mut(0) {
            *first ^= round;
        }
    }
}

/// The theta step: mixes each column's parity into its neighbours.
fn theta(state: &mut [u64; 25]) {
    let mut parity = [0u64; 5];
    for (column, slot) in parity.iter_mut().enumerate() {
        *slot = lane(state, column)
            ^ lane(state, column + 5)
            ^ lane(state, column + 10)
            ^ lane(state, column + 15)
            ^ lane(state, column + 20);
    }
    for column in 0..5 {
        let left = parity.get((column + 4) % 5).copied().unwrap_or(0);
        let right = parity.get((column + 1) % 5).copied().unwrap_or(0);
        let mixed = left ^ right.rotate_left(1);
        for row in 0..5 {
            if let Some(slot) = state.get_mut(row * 5 + column) {
                *slot ^= mixed;
            }
        }
    }
}

/// The rho and pi steps: rotate each lane and move it to its new position.
fn rho_and_pi(state: &mut [u64; 25]) {
    let mut carried = lane(state, 1);
    for step in 0..24 {
        let target = KECCAK_PI_LANES.get(step).copied().unwrap_or(0);
        let rotation = KECCAK_ROTATIONS.get(step).copied().unwrap_or(0);
        let displaced = lane(state, target);
        if let Some(slot) = state.get_mut(target) {
            *slot = carried.rotate_left(rotation);
        }
        carried = displaced;
    }
}

/// The chi step: a non-linear mix along each row.
fn chi(state: &mut [u64; 25]) {
    for row in 0..5 {
        let base = row * 5;
        let mut lanes = [0u64; 5];
        for (column, slot) in lanes.iter_mut().enumerate() {
            *slot = lane(state, base + column);
        }
        for column in 0..5 {
            let next = lanes.get((column + 1) % 5).copied().unwrap_or(0);
            let after = lanes.get((column + 2) % 5).copied().unwrap_or(0);
            if let Some(slot) = state.get_mut(base + column) {
                *slot ^= (!next) & after;
            }
        }
    }
}

/// Renders bytes as lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published SHA-256 vectors.
    #[test]
    fn sha256_matches_its_published_vectors() {
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
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The published SHA3-256 vectors, including one that is exactly one block
    /// long, which is where a padding mistake shows up.
    #[test]
    fn sha3_256_matches_its_published_vectors() {
        assert_eq!(
            sha3_256_hex(b""),
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"
        );
        assert_eq!(
            sha3_256_hex(b"abc"),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
        assert_eq!(
            sha3_256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "41c0dba2a9d6240849100376a8235e2c82e1b9998a999e21db32dd97496d3376"
        );
        assert_eq!(
            sha3_256_hex(&[b'a'; 136]),
            "3fc5559f14db8e453a0a3091edbd2bc25e11528d81c66fa570a4efdcc2695ee1"
        );
        assert_eq!(
            sha3_256_hex(&vec![b'a'; 1_000_000]),
            "5c8875ae474a3634ba4fd55ec85bffd661f32aca75c6d699d0cdcb6c115891c1"
        );
    }

    /// Hex rendering must be lowercase and fixed width, because the digests go
    /// into artifacts that are compared as text.
    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(sha256_hex(b"x").len(), 64);
        assert_eq!(sha3_256_hex(b"x").len(), 64);
    }
}
