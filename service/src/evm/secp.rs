//! secp256k1 for the EVM leg: signing a 32-byte digest, recovering the
//! signer's address from a signature, and the low-`s` rule both depend on.
//!
//! # What Phase E deferred, and why it lands here
//!
//! [`super::signature::EvmSignature`] documents that recovery,
//! verification and low-`s` enforcement were deliberately left out of the
//! types phase because "it lands with the verifier". This is that module.
//! It exists now because Phase F must (a) produce EIP-712 authorization
//! signatures a Solidity `ECDSA.recover` will accept, (b) verify locally
//! that a remote signer returned a signature from the identity it claims,
//! and (c) sign raw transactions for the submitter EOA.
//!
//! # The crate choice
//!
//! `libsecp256k1` 0.6 — already a direct dependency of this service for
//! Goldcoin vault multisig (`goldcoin::multisig`), pinned there for
//! `rand` 0.7 ABI compatibility. It is flagged unmaintained upstream
//! (RUSTSEC-2025-0161, acknowledged in `service/deny.toml` with a
//! standing P2 migration item), and this module does NOT change that
//! assessment or extend it silently: the P2 item already says "evaluate
//! migrating to an actively-maintained secp256k1 crate before production
//! custody keys are ever used with this code path", and every Robinhood
//! route ships disabled, so no production custody key runs through here
//! yet either.
//!
//! Adding `k256` instead would put a SECOND secp256k1 implementation in
//! one binary — two curve implementations, two constant-time stories, two
//! sets of advisories — to serve one migration that should move both call
//! sites at once. One curve implementation, one advisory, one migration.
//!
//! # Low-`s`, stated once
//!
//! EIP-2 requires `s <= n/2`; OpenZeppelin's `ECDSA.recover` — which
//! `GlcRobinhoodBridge._authorize` calls — REVERTS on a high-`s`
//! signature. ECDSA is malleable: `(r, s, v)` and `(r, n-s, v^1)` are both
//! valid signatures over the same message, so a signature is not a unique
//! identifier for an authorization unless one of the two is ruled out.
//!
//! [`SECP256K1_N`] and [`SECP256K1_HALF_N`] are the group order and its
//! half. They are not asserted against prose: [`tests`] cross-checks them
//! against `libsecp256k1`'s own `Scalar`, so a typo in a 32-byte constant
//! fails a test rather than silently accepting a malleable signature.
//!
//! Every signature this module PRODUCES is normalised low-`s`
//! ([`sign_digest`]); every signature it ACCEPTS is checked
//! ([`recover_address`] refuses a high-`s` input rather than recovering
//! from it), so the two directions agree.
//!
//! # No key material is stored here
//!
//! [`sign_digest`] takes a secret key by reference and returns a
//! signature. It holds nothing, caches nothing, and logs nothing. The
//! only type in this module that can carry a secret is
//! [`EvmSecretKey`], whose `Debug` prints a fixed redaction string and
//! which has no accessor that yields its bytes.

use std::fmt;

use libsecp256k1::{Message, PublicKey, RecoveryId, SecretKey, Signature as SecpSignature};

use super::address::EvmAddress;
use super::keccak::keccak256;
use super::signature::EvmSignature;

/// The secp256k1 group order `n`, big-endian.
pub const SECP256K1_N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

/// `n / 2`, rounded down — the inclusive upper bound EIP-2 places on `s`.
pub const SECP256K1_HALF_N: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmSecpError {
    #[error("not a valid secp256k1 secret key")]
    InvalidSecretKey,
    #[error("not a valid secp256k1 signature: {0}")]
    InvalidSignature(String),
    #[error(
        "signature has a high `s` value: ECDSA is malleable, and a Solidity ECDSA.recover \
         verifier rejects this signature rather than recovering from it"
    )]
    HighS,
    #[error("could not recover a public key from this signature")]
    Recovery,
}

/// A secp256k1 secret key for the EVM leg.
///
/// Wraps `libsecp256k1`'s type solely to control what can be done with
/// it: there is no method here that returns the bytes, `Debug` is
/// redacted, and `Clone` is deliberately not derived, so a key cannot be
/// duplicated into a log line, a struct that outlives its owner, or a
/// `#[derive(Debug)]` on some containing config type.
pub struct EvmSecretKey(SecretKey);

impl EvmSecretKey {
    /// Parses 32 raw big-endian bytes. Rejects zero and any value at or
    /// above the group order — `libsecp256k1`'s own validation, not a
    /// range check written here.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<EvmSecretKey, EvmSecpError> {
        SecretKey::parse(bytes)
            .map(EvmSecretKey)
            .map_err(|_| EvmSecpError::InvalidSecretKey)
    }

    /// The EVM address this key controls: the low 20 bytes of the
    /// keccak-256 of its 64-byte uncompressed public key, with the
    /// `0x04` prefix byte excluded.
    pub fn address(&self) -> EvmAddress {
        public_key_to_address(&PublicKey::from_secret_key(&self.0))
    }
}

impl fmt::Debug for EvmSecretKey {
    /// Prints the ADDRESS, never the key. An address is public and is the
    /// thing an operator actually needs when reading a log line; the
    /// secret has no rendering at all.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EvmSecretKey(<redacted, address {}>)", self.address())
    }
}

/// Signs a 32-byte digest, returning a compact `r || s || v` signature
/// with `v` in the 27/28 spelling and `s` normalised to the low half of
/// the curve order.
///
/// `digest` must ALREADY be the final 32 bytes to sign — for an EIP-712
/// authorization that is [`super::eip712::typed_data_hash`]'s output, and
/// for a transaction it is the keccak of the signing payload. Nothing is
/// hashed here: a function that both hashed and signed would make "which
/// preimage did this signature commit to" a question about this file
/// rather than about the caller that knows the answer.
pub fn sign_digest(key: &EvmSecretKey, digest: &[u8; 32]) -> EvmSignature {
    // `Message::parse` on a 32-byte array is total — it reduces the value
    // into the scalar field and cannot fail — so there is no error path
    // here to propagate.
    let message = Message::parse(digest);
    let (mut signature, recovery_id) = libsecp256k1::sign(&message, &key.0);

    // EIP-2 low-`s`. `normalize_s` flips `s` to `n - s` when it is in the
    // high half, and the flip must be detected BEFORE it happens because
    // the library reports nothing: flipping `s` also flips the recovery
    // parity, and leaving `v` alone after normalising produces a
    // signature that recovers to a DIFFERENT address — which a verifier
    // reports as an unauthorized signer rather than as a bug.
    let was_high = is_high_s(&signature.serialize()[32..].try_into().expect("32 bytes"));
    signature.normalize_s();
    let parity = (recovery_id.serialize() & 1) ^ u8::from(was_high);

    let serialized = signature.serialize();
    let mut compact = [0u8; 65];
    compact[..64].copy_from_slice(&serialized);
    compact[64] = 27 + parity;

    // Every component is non-zero and `v` is 27 or 28 by construction, so
    // the shape validation in `EvmSignature::from_bytes` cannot refuse
    // what was just built. Expressed as an expect rather than an unwrap so
    // a future change to that validation names itself in the panic.
    EvmSignature::from_bytes(compact).expect("a freshly produced signature is well-formed")
}

/// Recovers the EVM address that produced `signature` over `digest`.
///
/// Refuses a high-`s` signature outright rather than recovering from it,
/// so this agrees exactly with the Solidity verifier the signature is
/// ultimately checked by: a signature this function accepts is one
/// `ECDSA.recover` will also accept, and a signature it refuses is one the
/// contract would have reverted on. Accepting more here than the contract
/// does would let a signature pass local verification and then revert
/// on-chain after gas had been spent and a nonce consumed.
pub fn recover_address(
    digest: &[u8; 32],
    signature: &EvmSignature,
) -> Result<EvmAddress, EvmSecpError> {
    if is_high_s(signature.s()) {
        return Err(EvmSecpError::HighS);
    }
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(signature.r());
    rs[32..].copy_from_slice(signature.s());
    let parsed = SecpSignature::parse_standard(&rs)
        .map_err(|e| EvmSecpError::InvalidSignature(format!("{e:?}")))?;
    let recovery = RecoveryId::parse(signature.recovery_id())
        .map_err(|e| EvmSecpError::InvalidSignature(format!("recovery id: {e:?}")))?;
    let message = Message::parse(digest);
    let public =
        libsecp256k1::recover(&message, &parsed, &recovery).map_err(|_| EvmSecpError::Recovery)?;
    Ok(public_key_to_address(&public))
}

/// Whether `s` is above `n/2` — i.e. whether this is the malleable twin
/// of an equally valid signature, and therefore the one every EIP-2
/// verifier rejects.
///
/// A plain big-endian byte comparison: `s` and the bound are both 32-byte
/// big-endian integers, and lexicographic order on equal-length
/// big-endian bytes IS numeric order. No arithmetic, no bignum
/// dependency.
pub fn is_high_s(s: &[u8; 32]) -> bool {
    s.as_slice() > SECP256K1_HALF_N.as_slice()
}

/// The low 20 bytes of `keccak256(uncompressed_public_key[1..])`.
fn public_key_to_address(public: &PublicKey) -> EvmAddress {
    // 65 bytes: a `0x04` tag followed by the X and Y coordinates. The tag
    // is NOT part of the hashed preimage — including it yields a
    // completely different, wrong address.
    let serialized = public.serialize();
    let hash = keccak256(&serialized[1..]);
    let mut address = [0u8; 20];
    address.copy_from_slice(&hash[12..]);
    EvmAddress::from_bytes(address)
}

#[cfg(test)]
mod tests;
