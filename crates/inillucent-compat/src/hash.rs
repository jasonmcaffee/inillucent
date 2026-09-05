//! SHA-256 and SHA3-256.
//!
//! Invariant: both functions match their published test vectors, which are
//! checked below rather than assumed.
//!
//! These are here rather than pulled in from a crate for two reasons. The
//! evidence model hashes artifacts and the reference metadata pins SQLite's
//! published SHA3-256 sums, so both algorithms are part of a contract that must
//! not change when a dependency is upgraded; and neither is used for anything
//! secret, so there is no argument for a hardened implementation.
//!
//! SHA-256 now lives in `inillucent-base` and is re-exported here, because the
//! migration tool needs the same function in a production crate and a hash with
//! two implementations is one that eventually disagrees with itself. SHA3-256
//! stays here: nothing but the reference pinning uses it, and that is test-only.

/// Returns the SHA-256 digest of `data` as lowercase hex.
///
/// Delegated to `inillucent_base::hash`, which is where it moved when the migration
/// tool needed the same function in a production crate. Two implementations of
/// one hash is exactly the kind of thing that drifts, and the manifests a
/// rollback decision is made from would be the place it showed up.
pub fn sha256_hex(data: &[u8]) -> String {
    inillucent_base::hash::sha256_hex(data)
}

/// Returns the SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    inillucent_base::hash::sha256(data)
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
