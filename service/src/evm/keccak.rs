//! keccak-256, the EVM's hash function.
//!
//! # Why `sha3`, and why not hand-rolled
//!
//! Nothing here implements a hash. keccak-256 is a sponge construction
//! with a permutation, a rate, and a padding rule; writing one by hand
//! would be homegrown cryptography, and a padding or rate mistake produces
//! digests that look perfectly random while being wrong — which for an
//! EIP-712 authorization means a signature over a message nobody
//! authorized. [`sha3`] is the RustCrypto implementation, actively
//! maintained, MIT OR Apache-2.0, and constant-time by construction.
//!
//! It also costs this repository **zero new crates**: `sha3` 0.10 is
//! already in `Cargo.lock`, pulled in transitively by `solana-keccak-hasher`
//! via `solana-sdk`, and its own dependencies (`digest`, `keccak`,
//! `block-buffer`) are already present for `sha2`/`ripemd`, which this
//! crate already depends on directly from the same RustCrypto family. This
//! makes it a direct dependency of a crate that was already compiling it.
//!
//! # keccak-256 is not SHA3-256
//!
//! The two differ only in the domain-separation byte appended before
//! padding (`0x01` for original keccak, `0x06` for the FIPS-202 SHA-3), so
//! confusing them is easy, silent, and total: every digest differs. The
//! EVM uses the *original* keccak, i.e. [`sha3::Keccak256`], never
//! [`sha3::Sha3_256`]. The tests pin this down against published vectors
//! for both functions, so a future edit that swaps them fails loudly.

use sha3::{Digest, Keccak256};

/// The keccak-256 digest of `bytes`, as the EVM computes it.
pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

/// The keccak-256 digest of several byte ranges hashed as one contiguous
/// message, without allocating the concatenation.
///
/// EIP-712's `hashStruct` and `\x19\x01`-prefixed digests are both defined
/// over concatenations of 32-byte words; building those as a `Vec` first
/// works but makes it easy to hash a `Vec`'s *contents* in the wrong order
/// or to forget a field. Taking the parts explicitly keeps each call site's
/// field order visible on one line.
pub fn keccak256_concat(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests;
