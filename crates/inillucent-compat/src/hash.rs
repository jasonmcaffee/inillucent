//! SHA-256 and SHA3-256, both from `inillucent-base`.
//!
//! Invariant: both functions match their published test vectors, which are
//! checked below rather than assumed.
//!
//! These are implemented in this workspace rather than pulled in from a crate
//! for two reasons. The evidence model hashes artifacts and the reference
//! metadata pins SQLite's published SHA3-256 sums, so both algorithms are part
//! of a contract that must not change when a dependency is upgraded; and
//! neither is used for anything secret, so there is no argument for a hardened
//! implementation.
//!
//! **Both now come from `inillucent-base` (task-1962, A12).** This file used to
//! hold a second `Keccak-f[1600]` whose `theta` and `chi` were byte identical
//! with `inillucent_base::sha3`'s, while its SHA-256 already delegated. Two
//! implementations of one hash is exactly the kind of thing that drifts, and
//! the manifests a rollback decision is made from would be where it showed up.

pub use inillucent_base::hash::{sha256, sha256_hex, to_hex};

/// Returns the SHA3-256 digest of `data`.
pub fn sha3_256(data: &[u8]) -> [u8; 32] {
    inillucent_base::sha3::sha3_256(data)
}

/// Returns the SHA3-256 digest of `data` as lowercase hex.
pub fn sha3_256_hex(data: &[u8]) -> String {
    to_hex(&sha3_256(data))
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
