//! SHA3, in all four published widths.
//!
//! Invariant: the digests match the published vectors, which are checked below
//! rather than assumed. Every width is one function of a *rate* - 224, 256, 384
//! and 512 differ only in how many bytes of the sponge are absorbed per block
//! and how many are squeezed out at the end - so writing four functions would
//! be writing one function four times.
//!
//! It is here rather than pulled in from a crate because `sha3_query()` is part
//! of a **compatibility contract**: the reference shell's `.sha3sum` hashes a
//! database's logical content through a documented byte encoding, and the whole
//! value of implementing it is that the two engines' answers can be compared.
//! A digest that changed when a dependency was upgraded would break exactly the
//! comparison it exists for.
//!
//! `inillucent-compat`'s SHA3-256 is the same algorithm and was here first, in a
//! test-only crate; it now delegates here, because a hash with two
//! implementations is one that eventually disagrees with itself.

/// The Keccak-f[1600] round constants.
const ROUNDS: [u64; 24] = [
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

/// The rotation offsets of the rho step, in the order the pi step visits lanes.
const ROTATIONS: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];

/// The lane the pi step moves each rotated lane to.
const PI_LANES: [usize; 24] = [
    10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
];

/// Returns whether a digest width is one SHA3 publishes.
///
/// @param bits - the requested width
pub fn is_sha3_width(bits: u32) -> bool {
    matches!(bits, 224 | 256 | 384 | 512)
}

/// A SHA3 sponge that can be fed in pieces.
///
/// **Incremental because `sha3_agg()` and `sha3_query()` are.** Both hash a
/// whole result set, and buffering one into a byte vector first would mean
/// holding a copy of the database in memory to hash it.
pub struct Sha3 {
    /// The sponge.
    state: [u64; 25],
    /// How many bytes are absorbed per permutation.
    rate: usize,
    /// The partial block not yet absorbed.
    pending: Vec<u8>,
    /// How many bytes the digest is.
    output: usize,
}

impl Sha3 {
    /// Returns a sponge for one of the four published widths.
    ///
    /// An unpublished width is refused by the caller rather than rounded to a
    /// neighbour, because a digest under a made-up parameter set is a number
    /// nothing else in the world can reproduce.
    ///
    /// @param bits - 224, 256, 384 or 512
    pub fn new(bits: u32) -> Sha3 {
        let output = (bits as usize) / 8;
        Sha3 {
            state: [0u64; 25],
            // The rate is the block size minus twice the capacity, and the
            // capacity is twice the digest width - which is the whole of what
            // separates the four functions.
            rate: 200usize.saturating_sub(output.saturating_mul(2)),
            pending: Vec::new(),
            output,
        }
    }

    /// Feeds bytes in.
    ///
    /// @param data - the next piece of the message
    pub fn update(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let mut offset = 0usize;
        while offset.saturating_add(self.rate) <= self.pending.len() {
            let block: Vec<u8> = self
                .pending
                .get(offset..offset.saturating_add(self.rate))
                .unwrap_or(&[])
                .to_vec();
            absorb(&mut self.state, &block);
            keccak_f(&mut self.state);
            offset = offset.saturating_add(self.rate);
        }
        self.pending.drain(..offset);
    }

    /// Pads, absorbs the last block and squeezes the digest out.
    pub fn finish(mut self) -> Vec<u8> {
        let mut last = vec![0u8; self.rate];
        for (slot, byte) in last.iter_mut().zip(self.pending.iter()) {
            *slot = *byte;
        }
        // The SHA3 domain separator, then the final bit of the pad. They may
        // land on the same byte, which is why the second is an or.
        if let Some(slot) = last.get_mut(self.pending.len()) {
            *slot = 0x06;
        }
        if let Some(slot) = last.get_mut(self.rate.saturating_sub(1)) {
            *slot |= 0x80;
        }
        absorb(&mut self.state, &last);
        keccak_f(&mut self.state);
        let mut digest = Vec::with_capacity(self.output);
        // Squeeze: read the rate out a lane at a time, permuting whenever the
        // block runs out. Only SHA3-224 through 512 are here and all four have
        // an output shorter than one rate, so the loop runs once - it is
        // written as a loop anyway because a sponge that only worked for short
        // outputs would be a trap for the next caller.
        'squeeze: loop {
            for index in 0..(self.rate / 8) {
                let bytes = self.state.get(index).copied().unwrap_or(0).to_le_bytes();
                for byte in bytes {
                    if digest.len() == self.output {
                        break 'squeeze;
                    }
                    digest.push(byte);
                }
            }
            if digest.len() == self.output {
                break;
            }
            keccak_f(&mut self.state);
        }
        digest
    }
}

/// Returns the SHA3 digest of a whole message.
///
/// @param data - the message
/// @param bits - 224, 256, 384 or 512
pub fn sha3(data: &[u8], bits: u32) -> Vec<u8> {
    let mut sponge = Sha3::new(bits);
    sponge.update(data);
    sponge.finish()
}

/// Returns the SHA3-256 digest, which is the width the release manifests use.
///
/// @param data - the message
pub fn sha3_256(data: &[u8]) -> [u8; 32] {
    let digest = sha3(data, 256);
    let mut out = [0u8; 32];
    for (slot, byte) in out.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    out
}

/// Exclusive-ors one rate-sized block into the state.
fn absorb(state: &mut [u64; 25], block: &[u8]) {
    for (index, chunk) in block.chunks_exact(8).enumerate() {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(chunk);
        if let Some(lane) = state.get_mut(index) {
            *lane ^= u64::from_le_bytes(bytes);
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
    for round in ROUNDS {
        theta(state);
        rho_and_pi(state);
        chi(state);
        if let Some(first) = state.get_mut(0) {
            *first ^= round;
        }
    }
}

/// The theta step: mixes each column's parity into its neighbours.
///
/// **The arithmetic here is all lane addressing over a fixed 5x5 state**, so
/// `column + 20` and `row * 5 + column` are bounded by 24 and cannot overflow a
/// `usize`. The crate denies `arithmetic_side_effects` because a page offset or
/// a record length that wraps is a corruption; a constant-bounded index into a
/// 25-element array is not that, and writing each one as `saturating_add` would
/// obscure the permutation FIPS 202 specifies without changing a value.
#[allow(clippy::arithmetic_side_effects)]
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
///
/// Bounded lane addressing, as in `theta`.
#[allow(clippy::arithmetic_side_effects)]
fn rho_and_pi(state: &mut [u64; 25]) {
    let mut carried = lane(state, 1);
    for step in 0..24 {
        let target = PI_LANES.get(step).copied().unwrap_or(0);
        let rotation = ROTATIONS.get(step).copied().unwrap_or(0);
        let displaced = lane(state, target);
        if let Some(slot) = state.get_mut(target) {
            *slot = carried.rotate_left(rotation);
        }
        carried = displaced;
    }
}

/// The chi step: a non-linear mix along each row.
///
/// Bounded lane addressing, as in `theta`.
#[allow(clippy::arithmetic_side_effects)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders a digest as lowercase hex.
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The published vectors for all four widths, so a rate that is wrong for
    /// one of them cannot hide behind another that is right.
    #[test]
    fn every_width_matches_its_published_vectors() {
        assert_eq!(
            hex(&sha3(b"", 224)),
            "6b4e03423667dbb73b6e15454f0eb1abd4597f9a1b078e3f5b5a6bc7"
        );
        assert_eq!(
            hex(&sha3(b"abc", 224)),
            "e642824c3f8cf24ad09234ee7d3c766fc9a3a5168d0c94ad73b46fdf"
        );
        assert_eq!(
            hex(&sha3(b"", 256)),
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"
        );
        assert_eq!(
            hex(&sha3(b"abc", 256)),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
        assert_eq!(
            hex(&sha3(b"", 384)),
            "0c63a75b845e4f7d01107d852e4c2485c51a50aaaa94fc61995e71bbee983a2ac3713831264adb47fb6bd1e058d5f004"
        );
        assert_eq!(
            hex(&sha3(b"abc", 384)),
            "ec01498288516fc926459f58e2c6ad8df9b473cb0fc08c2596da7cf0e49be4b298d88cea927ac7f539f1edf228376d25"
        );
        assert_eq!(
            hex(&sha3(b"", 512)),
            "a69f73cca23a9ac5c8b567dc185a756e97c982164fe25859e0d1dcc1475c80a615b2123af1f5f94c11e3e9402c3ac558f500199d95b6d3e301758586281dcd26"
        );
        assert_eq!(
            hex(&sha3(b"abc", 512)),
            "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0"
        );
    }

    /// A message exactly one block long, which is where a padding mistake shows.
    #[test]
    fn a_message_of_exactly_one_block_pads_correctly() {
        assert_eq!(
            hex(&sha3(&[b'a'; 136], 256)),
            "3fc5559f14db8e453a0a3091edbd2bc25e11528d81c66fa570a4efdcc2695ee1"
        );
    }

    /// Feeding the message in pieces is the same as feeding it whole, which is
    /// the whole reason the sponge is incremental.
    #[test]
    fn feeding_in_pieces_is_the_same_as_feeding_it_whole() {
        let whole = sha3(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            256,
        );
        let mut sponge = Sha3::new(256);
        for piece in [
            &b"abcdbcdecdefdefg"[..],
            &b"efghfghighijhijk"[..],
            &b"ijkljklmklmnlmno"[..],
            &b"mnopnopq"[..],
        ] {
            sponge.update(piece);
        }
        assert_eq!(sponge.finish(), whole);
    }

    /// Only the four published widths are widths.
    #[test]
    fn an_unpublished_width_is_not_one() {
        assert!(is_sha3_width(224));
        assert!(is_sha3_width(512));
        assert!(!is_sha3_width(128));
        assert!(!is_sha3_width(0));
    }
}
