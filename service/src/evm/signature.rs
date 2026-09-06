//! [`EvmSignature`]: the 65-byte compact ECDSA signature the EVM uses.
//!
//! # Scope: representation only, deliberately no cryptography
//!
//! This module parses, validates the shape of, and re-encodes a signature.
//! It does **not** recover a public key, verify a signature against a
//! message, hold a private key, or produce a signature. Those are all
//! deferred, on purpose:
//!
//! - **Recovery/verification** needs a secp256k1 implementation, and that
//!   choice is not obvious here. This crate already depends on
//!   `libsecp256k1` 0.6 for the Goldcoin vault, but that crate is flagged
//!   unmaintained upstream in `deny.toml` (RUSTSEC-2025-0161) and is
//!   deliberately confined to dev/test key material; adding EVM
//!   authorisation verification on top of it would extend an
//!   already-tracked dependency into a new production trust path. The
//!   maintained alternative (`k256`) is a genuinely new cryptographic
//!   dependency. Either way the decision belongs in the same reviewable
//!   change as the verification code and the policy it enforces — not in a
//!   types phase, where nothing would exercise it.
//! - **Low-`s` (malleability) enforcement** — EIP-2's requirement that
//!   `s <= n/2`, which Solidity verifiers such as OpenZeppelin's `ECDSA`
//!   enforce — is a *verification policy*, and it needs the secp256k1 group
//!   order as a constant. Introducing that constant with no verification
//!   code to cross-check it against a real curve implementation would mean
//!   a security-critical number that nothing validates. It lands with the
//!   verifier.
//!
//! What this module does provide is the thing every one of those will need
//! first, and the thing that is genuinely testable now: a type that is
//! exactly 65 bytes, that splits into `r`, `s` and `v` correctly, and that
//! cannot be constructed from a value which is structurally not a
//! signature.
//!
//! # The shape
//!
//! The 65-byte compact form, in the byte order `ecrecover`, `eth_sign` and
//! every EIP-712 signing library agree on:
//!
//! ```text
//! bytes  0..32   r   big-endian
//! bytes 32..64   s   big-endian
//! byte     64    v   recovery identifier: 27 or 28 (or 0 or 1)
//! ```
//!
//! This is **not** the EIP-155 transaction-signature encoding, where `v`
//! carries the chain id and does not fit a byte at all. This bridge's use
//! for a signature is an off-chain typed-data authorisation, which is
//! always the compact form; a transaction signature is produced and
//! consumed inside a signing library and never needs this type.

use std::fmt;
use std::str::FromStr;

use super::hex::{self, EvmHexError};

/// The number of bytes in a compact ECDSA signature: 32 + 32 + 1.
pub const SIGNATURE_BYTES: usize = 65;

/// The number of hex digits in the textual form of a signature.
pub const SIGNATURE_HEX_DIGITS: usize = SIGNATURE_BYTES * 2;

/// Why a value is not a well-formed compact ECDSA signature.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmSignatureError {
    /// The `0x`-prefixed 130-hex-digit shape itself is wrong.
    #[error("invalid EVM signature: {0}")]
    Hex(#[from] EvmHexError),
    /// A byte sequence of the wrong width was offered. In particular a
    /// 64-byte value — an `r || s` pair with the recovery byte lost — is
    /// refused rather than completed with a guessed `v`.
    #[error("a compact EVM signature is exactly 65 bytes (r || s || v), got {actual}")]
    WrongByteLength { actual: usize },
    /// `v` is not a compact-form recovery identifier.
    #[error(
        "signature recovery byte v = {found} is not one of 27, 28, 0 or 1 — an EIP-155 \
         transaction v (which encodes the chain id) does not fit this form and must not be \
         truncated into it"
    )]
    InvalidRecoveryByte { found: u8 },
    /// `r` or `s` is zero. No valid ECDSA signature has either component
    /// zero, and a zero pair is the classic "unsigned" sentinel that a
    /// careless verifier accepts — so it is refused at the door.
    #[error("signature component {component} is zero, which no valid ECDSA signature has")]
    ZeroComponent { component: &'static str },
}

/// A 65-byte compact ECDSA signature: `r || s || v`.
///
/// Every construction path validates the shape (see
/// [`EvmSignature::from_bytes`]), so a value of this type is structurally a
/// signature. It is *not* a verified one — see the module docs on what this
/// phase deliberately leaves out.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvmSignature {
    r: [u8; 32],
    s: [u8; 32],
    v: u8,
}

impl EvmSignature {
    /// Validates and wraps 65 bytes in `r || s || v` order.
    ///
    /// Rejects a `v` that is not a compact recovery identifier, and a zero
    /// `r` or `s`. Everything else about the components is a question for a
    /// verifier, not a parser: whether `r` and `s` are below the group
    /// order, and whether the signature is actually over the message the
    /// caller has in mind, are not answerable here.
    pub fn from_bytes(bytes: [u8; SIGNATURE_BYTES]) -> Result<EvmSignature, EvmSignatureError> {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&bytes[..32]);
        s.copy_from_slice(&bytes[32..64]);
        let v = bytes[64];

        if r == [0u8; 32] {
            return Err(EvmSignatureError::ZeroComponent { component: "r" });
        }
        if s == [0u8; 32] {
            return Err(EvmSignatureError::ZeroComponent { component: "s" });
        }
        if !matches!(v, 0 | 1 | 27 | 28) {
            return Err(EvmSignatureError::InvalidRecoveryByte { found: v });
        }

        Ok(EvmSignature { r, s, v })
    }

    /// Validates and wraps a slice that must be **exactly** 65 bytes long.
    pub fn try_from_slice(bytes: &[u8]) -> Result<EvmSignature, EvmSignatureError> {
        let array = <[u8; SIGNATURE_BYTES]>::try_from(bytes).map_err(|_| {
            EvmSignatureError::WrongByteLength {
                actual: bytes.len(),
            }
        })?;
        EvmSignature::from_bytes(array)
    }

    /// The 65 bytes in `r || s || v` order — the form to hand to a contract
    /// call or a verifier.
    pub fn to_bytes(self) -> [u8; SIGNATURE_BYTES] {
        let mut out = [0u8; SIGNATURE_BYTES];
        out[..32].copy_from_slice(&self.r);
        out[32..64].copy_from_slice(&self.s);
        out[64] = self.v;
        out
    }

    /// The `r` component, big-endian.
    pub const fn r(&self) -> &[u8; 32] {
        &self.r
    }

    /// The `s` component, big-endian.
    pub const fn s(&self) -> &[u8; 32] {
        &self.s
    }

    /// The raw recovery byte, exactly as it arrived: 27, 28, 0 or 1.
    pub const fn v(&self) -> u8 {
        self.v
    }

    /// The recovery identifier normalised to 0 or 1.
    ///
    /// Both spellings are in use — 27/28 comes from Ethereum's original
    /// encoding, 0/1 is what a raw ECDSA library returns — and normalising
    /// is a subtraction that is easy to do twice or not at all. Doing it in
    /// one place, infallibly (the constructor already excluded every other
    /// byte), removes the question from every call site.
    pub const fn recovery_id(&self) -> u8 {
        match self.v {
            27 | 28 => self.v - 27,
            // 0 or 1; the constructor rejects everything else.
            other => other,
        }
    }
}

impl FromStr for EvmSignature {
    type Err = EvmSignatureError;

    /// A mandatory, exactly-`0x` prefix and exactly 130 hex digits of any
    /// case, followed by the same component validation as
    /// [`EvmSignature::from_bytes`].
    fn from_str(s: &str) -> Result<EvmSignature, EvmSignatureError> {
        let bytes = hex::decode_fixed::<SIGNATURE_BYTES>(s)?;
        EvmSignature::from_bytes(bytes)
    }
}

impl fmt::Display for EvmSignature {
    /// `0x` + 130 lowercase hex digits. Round-trips through
    /// [`EvmSignature::from_str`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode_lower(&self.to_bytes()))
    }
}

impl fmt::Debug for EvmSignature {
    /// A signature is not a secret — it is published on chain — so printing
    /// it in full is safe, and being able to compare a logged signature
    /// against the one a client sent is worth more than brevity. (No
    /// private key ever reaches this type; see the module docs.)
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EvmSignature({self})")
    }
}

#[cfg(test)]
mod tests;
