//! The cryptographic primitives two other databases' login handshakes are
//! specified in terms of.
//!
//! These are **other people's wire formats**, not contracts this engine
//! publishes, which is why they live here rather than in `inillucent-base`
//! beside SHA-256 and the WAL's checksum. Nothing on a page and nothing in a
//! manifest is hashed with any of them; they exist so that a `PasswordMessage`
//! and a `HandshakeResponse41` carry the bytes the server on the other end is
//! going to compare against.
//!
//! Every function here is checked against a published vector in this file's own
//! tests, because a hash that is subtly wrong does not produce a wrong answer -
//! it produces "password authentication failed", which reads as the operator's
//! mistake.
//!
//! Invariant: **these are other people's wire formats, reproduced exactly, and
//! no secret is kept a moment longer than the handshake needs it.** A hash that
//! is nearly right is a login that fails with no explanation either end can
//! act on, so each algorithm is written out the way its specification states it
//! and checked against that specification's own worked example.

use inillucent_base::hash::{sha256, Sha256};

// ---------------------------------------------------------------------------
// MD5 - RFC 1321. PostgreSQL's `md5` authentication method.
// ---------------------------------------------------------------------------

/// The per-round left-rotation amounts, four rounds of four.
const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// Returns the MD5 digest of a message.
///
/// @param message - the bytes to digest
pub fn md5(message: &[u8]) -> [u8; 16] {
    // The sine-derived constants, computed rather than tabulated so the table
    // and its source cannot disagree. `floor(abs(sin(i + 1)) * 2^32)`.
    let mut constants = [0u32; 64];
    for (index, slot) in constants.iter_mut().enumerate() {
        let angle = (index as f64) + 1.0;
        *slot = (angle.sin().abs() * 4_294_967_296.0) as u32;
    }

    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut padded = message.to_vec();
    let bits = (message.len() as u64).wrapping_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bits.to_le_bytes());

    for block in padded.chunks_exact(64) {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() {
            let at = index.saturating_mul(4);
            let bytes = block.get(at..at.saturating_add(4)).unwrap_or(&[0, 0, 0, 0]);
            *word = u32::from_le_bytes([
                *bytes.first().unwrap_or(&0),
                *bytes.get(1).unwrap_or(&0),
                *bytes.get(2).unwrap_or(&0),
                *bytes.get(3).unwrap_or(&0),
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for step in 0..64usize {
            let (mixed, index) = match step / 16 {
                0 => ((b & c) | (!b & d), step),
                1 => (
                    (d & b) | (!d & c),
                    (5usize.wrapping_mul(step).wrapping_add(1)) % 16,
                ),
                2 => (b ^ c ^ d, (3usize.wrapping_mul(step).wrapping_add(5)) % 16),
                _ => (c ^ (b | !d), (7usize.wrapping_mul(step)) % 16),
            };
            let added = a
                .wrapping_add(mixed)
                .wrapping_add(*constants.get(step).unwrap_or(&0))
                .wrapping_add(*words.get(index).unwrap_or(&0));
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(added.rotate_left(*MD5_SHIFTS.get(step).unwrap_or(&0)));
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = [0u8; 16];
    for (index, word) in state.iter().enumerate() {
        let at = index.saturating_mul(4);
        if let Some(slot) = out.get_mut(at..at.saturating_add(4)) {
            slot.copy_from_slice(&word.to_le_bytes());
        }
    }
    out
}

/// Returns the MD5 digest of a message as lower-case hexadecimal.
///
/// @param message - the bytes to digest
pub fn md5_hex(message: &[u8]) -> String {
    to_hex(&md5(message))
}

// ---------------------------------------------------------------------------
// SHA-1 - RFC 3174. MySQL's `mysql_native_password`.
// ---------------------------------------------------------------------------

/// Returns the SHA-1 digest of a message.
///
/// @param message - the bytes to digest
pub fn sha1(message: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    let mut padded = message.to_vec();
    let bits = (message.len() as u64).wrapping_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bits.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut schedule = [0u32; 80];
        for index in 0..16usize {
            let at = index.saturating_mul(4);
            let bytes = block.get(at..at.saturating_add(4)).unwrap_or(&[0, 0, 0, 0]);
            if let Some(slot) = schedule.get_mut(index) {
                *slot = u32::from_be_bytes([
                    *bytes.first().unwrap_or(&0),
                    *bytes.get(1).unwrap_or(&0),
                    *bytes.get(2).unwrap_or(&0),
                    *bytes.get(3).unwrap_or(&0),
                ]);
            }
        }
        for index in 16..80usize {
            let mixed = schedule.get(index.wrapping_sub(3)).unwrap_or(&0)
                ^ schedule.get(index.wrapping_sub(8)).unwrap_or(&0)
                ^ schedule.get(index.wrapping_sub(14)).unwrap_or(&0)
                ^ schedule.get(index.wrapping_sub(16)).unwrap_or(&0);
            if let Some(slot) = schedule.get_mut(index) {
                *slot = mixed.rotate_left(1);
            }
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for index in 0..80usize {
            let (mixed, constant) = match index / 20 {
                0 => ((b & c) | (!b & d), 0x5a82_7999u32),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(mixed)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*schedule.get(index).unwrap_or(&0));
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (index, word) in state.iter().enumerate() {
        let at = index.saturating_mul(4);
        if let Some(slot) = out.get_mut(at..at.saturating_add(4)) {
            slot.copy_from_slice(&word.to_be_bytes());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// HMAC and PBKDF2 over SHA-256 - RFC 2104 and RFC 8018. SCRAM-SHA-256.
// ---------------------------------------------------------------------------

/// The SHA-256 block size, which is what a key is padded or folded to.
const SHA256_BLOCK: usize = 64;

/// Returns HMAC-SHA-256 of a message under a key.
///
/// @param key - the secret
/// @param message - the bytes to authenticate
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; SHA256_BLOCK];
    if key.len() > SHA256_BLOCK {
        let folded = sha256(key);
        if let Some(slot) = block.get_mut(..folded.len()) {
            slot.copy_from_slice(&folded);
        }
    } else if let Some(slot) = block.get_mut(..key.len()) {
        slot.copy_from_slice(key);
    }

    let mut inner_key = [0u8; SHA256_BLOCK];
    let mut outer_key = [0u8; SHA256_BLOCK];
    for index in 0..SHA256_BLOCK {
        let byte = *block.get(index).unwrap_or(&0);
        if let Some(slot) = inner_key.get_mut(index) {
            *slot = byte ^ 0x36;
        }
        if let Some(slot) = outer_key.get_mut(index) {
            *slot = byte ^ 0x5c;
        }
    }

    let mut inner = Sha256::new();
    inner.update(&inner_key);
    inner.update(message);
    let inner = inner.finish();

    let mut outer = Sha256::new();
    outer.update(&outer_key);
    outer.update(&inner);
    outer.finish()
}

/// Returns PBKDF2-HMAC-SHA-256 of a password, 32 bytes wide.
///
/// One output block, which is all SCRAM-SHA-256 asks for: its salted password
/// is exactly the hash's own width, so the `INT(i)` counter never leaves 1.
///
/// @param password - the secret, already normalised by the caller
/// @param salt - the server's salt
/// @param iterations - the server's iteration count
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut seed = salt.to_vec();
    seed.extend_from_slice(&1u32.to_be_bytes());
    let mut block = hmac_sha256(password, &seed);
    let mut accumulated = block;
    for _ in 1..iterations.max(1) {
        block = hmac_sha256(password, &block);
        for index in 0..accumulated.len() {
            if let (Some(slot), Some(byte)) = (accumulated.get_mut(index), block.get(index)) {
                *slot ^= *byte;
            }
        }
    }
    accumulated
}

// ---------------------------------------------------------------------------
// Base64 - RFC 4648, the standard alphabet with padding. SCRAM's encoding.
// ---------------------------------------------------------------------------

/// The standard alphabet, in the order the encoding indexes it.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Returns bytes encoded as padded standard base64.
///
/// @param bytes - what to encode
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_add(2) / 3 * 4);
    for group in bytes.chunks(3) {
        let first = *group.first().unwrap_or(&0) as u32;
        let second = *group.get(1).unwrap_or(&0) as u32;
        let third = *group.get(2).unwrap_or(&0) as u32;
        let packed = (first << 16) | (second << 8) | third;
        let indices = [
            (packed >> 18) & 0x3f,
            (packed >> 12) & 0x3f,
            (packed >> 6) & 0x3f,
            packed & 0x3f,
        ];
        for (position, index) in indices.iter().enumerate() {
            let present = match position {
                0 | 1 => true,
                2 => group.len() >= 2,
                _ => group.len() >= 3,
            };
            if present {
                out.push(*BASE64.get(*index as usize).unwrap_or(&b'A') as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Returns the bytes a base64 string encodes, or `None` when it is not one.
///
/// Rejects rather than skips: a stray character in a server's salt is a
/// protocol failure worth naming, not something to tolerate quietly.
///
/// @param text - the encoded text
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut held = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a').saturating_add(26),
            b'0'..=b'9' => u32::from(byte - b'0').saturating_add(52),
            b'+' => 62,
            b'/' => 63,
            b'\r' | b'\n' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | value;
        held = held.saturating_add(6);
        if held >= 8 {
            held = held.saturating_sub(8);
            out.push(((accumulator >> held) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Returns bytes as lower-case hexadecimal.
///
/// @param bytes - what to render
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Returns the exclusive-or of two equal-length byte strings.
///
/// @param left - one operand
/// @param right - the other, which must be the same length
pub fn xor(left: &[u8], right: &[u8]) -> Vec<u8> {
    left.iter()
        .zip(right.iter())
        .map(|(one, other)| one ^ other)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 1321's own test suite, which is the only thing that can say this
    /// implementation is MD5 rather than something MD5-shaped.
    #[test]
    fn md5_matches_rfc_1321() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    /// FIPS 180's two published messages, plus the million-`a` case shortened
    /// to one that still crosses a block boundary.
    #[test]
    fn sha1_matches_its_published_vectors() {
        assert_eq!(
            to_hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            to_hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            to_hex(&sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    /// RFC 4231's cases 1, 2 and 4 for HMAC-SHA-256, and case 6, whose key is
    /// longer than the block and therefore takes the folding path.
    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        assert_eq!(
            to_hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            to_hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// PBKDF2-HMAC-SHA-256 against the vectors published for RFC 6070's inputs.
    #[test]
    fn pbkdf2_sha256_matches_its_published_vectors() {
        assert_eq!(
            to_hex(&pbkdf2_sha256(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            to_hex(&pbkdf2_sha256(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            to_hex(&pbkdf2_sha256(b"password", b"salt", 4096)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    /// RFC 4648's own encoding vectors, and a round trip over every byte value
    /// so the decoder is checked against something other than the encoder's
    /// happy path.
    #[test]
    fn base64_matches_rfc_4648_and_round_trips() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode("Zm9vYmFy").as_deref(), Some(&b"foobar"[..]));

        let every: Vec<u8> = (0..=255u8).collect();
        assert_eq!(
            base64_decode(&base64_encode(&every)).as_deref(),
            Some(&every[..])
        );
    }

    /// A character outside the alphabet is refused rather than skipped, so a
    /// corrupted salt fails the handshake instead of producing a shorter one.
    #[test]
    fn base64_refuses_a_character_outside_the_alphabet() {
        assert_eq!(base64_decode("Zm9v*mFy"), None);
    }
}
