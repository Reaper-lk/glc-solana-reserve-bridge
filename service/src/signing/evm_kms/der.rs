//! AWS KMS's DER-encoded ECDSA signature -> the contract's 65-byte
//! compact `r || s || v`.
//!
//! # The two things KMS does not give you
//!
//! `kms:Sign` over an `ECC_SECG_P256K1` key returns an X9.62 DER
//! `SEQUENCE { INTEGER r, INTEGER s }`. Two properties the EVM requires
//! are simply absent from that encoding:
//!
//! 1. **A recovery byte.** DER carries no `v`. `ecrecover` — and
//!    therefore OpenZeppelin's `ECDSA.recover`, which
//!    `GlcRobinhoodBridge._authorize` calls — cannot verify a signature
//!    without one.
//! 2. **Any low-`s` guarantee.** ECDSA is malleable: `(r, s)` and
//!    `(r, n-s)` are both valid over the same digest. EIP-2 requires
//!    `s <= n/2` and `ECDSA.recover` REVERTS otherwise (see
//!    [`crate::evm::secp`]'s "Low-`s`, stated once").
//!
//! # `v` is proven, not guessed
//!
//! There are exactly two candidate recovery bytes, and this module tries
//! both and RECOVERS each against the digest that was signed, keeping the
//! one whose recovered address equals the signer identity this domain was
//! independently configured with. That is a proof, evaluated with
//! [`crate::evm::secp::recover_address`] — the same function
//! [`crate::signing::remote::RemoteEvmAuthSigner`] uses to check this
//! service's answers on the bridge side, so the two ends cannot disagree
//! about what "recovers correctly" means.
//!
//! Guessing instead (assuming a parity, or trusting a `v` from anywhere)
//! would produce a signature that recovers to *some other address*. A
//! contract reports that as an unauthorized signer, after gas is spent;
//! here it is a startup failure or a `500`, before anything is broadcast.
//!
//! If NEITHER candidate recovers to the configured address, the answer is
//! refused outright — the key that signed is not the key this domain
//! claims to be. There is no fallback that returns "the signature anyway".
//!
//! # Normalisation order
//!
//! `s` is inspected BEFORE it is normalised, and the fact that it was
//! high is reported on [`ConvertedSignature::normalized_high_s`]. This
//! module does not *need* the flag to pick `v` (it proves `v` by
//! recovery either way), but a high-`s` answer from a KMS key is a real
//! operational fact — it means the returned signature would have been
//! rejected on-chain verbatim — and silently repairing it without being
//! able to say so would hide it.
//!
//! Nothing high-`s` can leave this module: the normalised value is
//! re-checked against [`crate::evm::secp::is_high_s`] before any
//! candidate is built, and [`crate::evm::secp::recover_address`] refuses
//! a high-`s` input in any case.

use libsecp256k1::Signature as SecpSignature;

use crate::evm::secp::{self, is_high_s};
use crate::evm::{EvmAddress, EvmSignature};

/// A KMS answer, converted and proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertedSignature {
    /// 65 bytes, `v` in the 27/28 spelling, `s` in the low half.
    pub signature: EvmSignature,
    /// True when KMS returned the high-`s` twin and it was normalised.
    /// Reported for operator visibility; see the module docs.
    pub normalized_high_s: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DerConversionError {
    #[error(
        "the signing backend returned {len} bytes that are not a strict DER ECDSA signature — \
         refusing to interpret them"
    )]
    MalformedDer { len: usize },
    #[error("the signing backend's (r, s) pair is not a well-formed compact signature: {detail}")]
    NotWellFormed { detail: String },
    #[error(
        "the signing backend's signature normalised to a high `s`, which every EIP-2 verifier \
         rejects — this is a bug in the conversion, not in the backend, and must never be served"
    )]
    StillHighS,
    #[error(
        "neither recovery byte recovers the signing backend's signature to this domain's \
         configured signer identity {expected}. The key that produced it is not the key this \
         domain is provisioned as, or it signed a different digest"
    )]
    NoRecoveryMatch { expected: String },
    #[error(
        "BOTH recovery bytes recover to {expected}, which is cryptographically impossible for a \
         well-formed signature — failing closed rather than picking one"
    )]
    AmbiguousRecovery { expected: String },
}

/// The full conversion: strict DER in, proven `r || s || v` out.
///
/// `digest` must be the exact 32 bytes that were handed to the signing
/// backend — for an authorization,
/// [`crate::signing::evm_policy::EvmAuthDecision::digest`]; for the
/// startup check, [`super::kms::startup_challenge_digest`]. Passing a
/// different digest cannot produce a wrong-but-accepted answer: recovery
/// would then yield some unrelated address and the conversion fails.
///
/// `expected` is the domain's own configured signer address, never a
/// value taken from a request or from the backend's response.
pub fn kms_der_to_evm_signature(
    der: &[u8],
    digest: &[u8; 32],
    expected: EvmAddress,
) -> Result<ConvertedSignature, DerConversionError> {
    // STRICT parsing: `parse_der`, not `parse_der_lax`. A backend that
    // emits a non-canonical encoding is a backend this code does not
    // understand, and guessing at its intent is exactly the class of
    // leniency that turns a malformed signature into a valid one for
    // something else.
    let mut parsed = SecpSignature::parse_der(der)
        .map_err(|_| DerConversionError::MalformedDer { len: der.len() })?;

    // (3) Detect high-`s` BEFORE normalising: `normalize_s` reports
    // nothing, so afterwards the fact is unrecoverable.
    let before = parsed.serialize();
    let normalized_high_s = is_high_s(s_of(&before));

    // (4) Normalise.
    parsed.normalize_s();
    let compact_rs = parsed.serialize();

    // Defense in depth, and cheap: this is the single place a high-`s`
    // signature could escape into a response.
    if is_high_s(s_of(&compact_rs)) {
        return Err(DerConversionError::StillHighS);
    }

    // Shape-check the (r, s) pair once — it is identical for both
    // candidates, so a zero component is a property of the backend's
    // answer rather than of the recovery byte under trial.
    if let Err(e) = EvmSignature::from_bytes(with_recovery_byte(&compact_rs, 27)) {
        return Err(DerConversionError::NotWellFormed {
            detail: e.to_string(),
        });
    }

    // (5)-(7) Try both parities and keep the one that RECOVERS to the
    // configured identity.
    let mut matched: Option<EvmSignature> = None;
    let mut matches = 0usize;
    for v in [27u8, 28u8] {
        let Ok(candidate) = EvmSignature::from_bytes(with_recovery_byte(&compact_rs, v)) else {
            continue;
        };
        // A candidate that does not recover at all is simply not the
        // right parity — that is the normal outcome for one of the two.
        if let Ok(recovered) = secp::recover_address(digest, &candidate) {
            if recovered == expected {
                matched = Some(candidate);
                matches += 1;
            }
        }
    }

    match (matches, matched) {
        (1, Some(signature)) => Ok(ConvertedSignature {
            signature,
            normalized_high_s,
        }),
        // (9) Fail closed. No "return it anyway" branch exists.
        (0, _) => Err(DerConversionError::NoRecoveryMatch {
            expected: expected.to_checksum_string(),
        }),
        _ => Err(DerConversionError::AmbiguousRecovery {
            expected: expected.to_checksum_string(),
        }),
    }
}

/// The `s` half of a 64-byte `r || s`.
fn s_of(compact_rs: &[u8; 64]) -> &[u8; 32] {
    compact_rs[32..]
        .try_into()
        .expect("the second half of 64 bytes is 32 bytes")
}

fn with_recovery_byte(compact_rs: &[u8; 64], v: u8) -> [u8; 65] {
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(compact_rs);
    out[64] = v;
    out
}

#[cfg(test)]
mod tests;
