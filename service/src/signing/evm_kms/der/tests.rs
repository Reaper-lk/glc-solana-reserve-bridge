//! DER -> `r || s || v`.
//!
//! Every case here signs with a real secp256k1 key rather than asserting
//! against canned bytes, because the properties under test — that `s` is
//! normalised, that `v` is PROVEN by recovery, and that a signature from
//! the wrong key cannot be converted at all — are properties of real
//! signatures, and a fixture would let the code and the fixture drift
//! together.

use libsecp256k1::{Message, SecretKey, Signature};

use super::*;
use crate::evm::secp::{is_high_s, sign_digest, EvmSecretKey};

fn secret(seed: u8) -> SecretKey {
    let mut bytes = [7u8; 32];
    bytes[31] = seed.max(1);
    bytes[0] = seed.wrapping_mul(5).max(1);
    SecretKey::parse(&bytes).expect("a valid secret key")
}

fn address_of(key: &SecretKey) -> EvmAddress {
    EvmSecretKey::from_bytes(&key.serialize())
        .expect("a valid secret key")
        .address()
}

/// What a compliant KMS returns: DER over the digest, canonical `s`.
fn der_over(key: &SecretKey, digest: &[u8; 32]) -> Vec<u8> {
    let (mut signature, _) = libsecp256k1::sign(&Message::parse(digest), key);
    signature.normalize_s();
    signature.serialize_der().as_ref().to_vec()
}

/// The malleable twin: `(r, n - s)`. A signing backend that does not
/// normalise is entitled to return this, and it verifies just as well —
/// which is exactly why the EVM's verifier refuses it.
fn high_s_der_over(key: &SecretKey, digest: &[u8; 32]) -> Vec<u8> {
    let (mut signature, _) = libsecp256k1::sign(&Message::parse(digest), key);
    signature.normalize_s();
    let mut twin = signature;
    twin.s = -twin.s;
    twin.serialize_der().as_ref().to_vec()
}

fn s_bytes(signature: &EvmSignature) -> [u8; 32] {
    *signature.s()
}

// ------------------------------------------------------------ happy path --

#[test]
fn a_canonical_der_signature_converts_and_recovers_to_the_configured_signer() {
    let key = secret(1);
    let digest = [0x11u8; 32];
    let converted =
        kms_der_to_evm_signature(&der_over(&key, &digest), &digest, address_of(&key)).unwrap();

    assert!(!converted.normalized_high_s);
    assert!(matches!(converted.signature.v(), 27 | 28));
    assert!(!is_high_s(&s_bytes(&converted.signature)));
    assert_eq!(
        secp::recover_address(&digest, &converted.signature).unwrap(),
        address_of(&key)
    );
}

/// The conversion must agree, byte for byte, with the signature this
/// crate's own signer produces for the same key and digest. `libsecp256k1`
/// signs deterministically (RFC 6979), so any disagreement would be a
/// disagreement about encoding, normalisation or the recovery byte —
/// precisely what this module exists to get right.
#[test]
fn the_conversion_reproduces_this_crate_s_own_signature_exactly() {
    for seed in 1u8..=12 {
        let key = secret(seed);
        let digest = [seed.wrapping_mul(17); 32];
        let reference = sign_digest(
            &EvmSecretKey::from_bytes(&key.serialize()).unwrap(),
            &digest,
        );
        let converted =
            kms_der_to_evm_signature(&der_over(&key, &digest), &digest, address_of(&key)).unwrap();
        assert_eq!(
            converted.signature.to_bytes(),
            reference.to_bytes(),
            "seed {seed}"
        );
    }
}

// --------------------------------------------------------------- low-`s` --

#[test]
fn a_high_s_signature_is_reported_normalised_and_still_recovers() {
    let key = secret(3);
    let digest = [0x33u8; 32];
    let high = high_s_der_over(&key, &digest);
    // Test setup: the twin really is the non-canonical half.
    let mut parsed = Signature::parse_der(&high).unwrap();
    assert!(is_high_s(&parsed.serialize()[32..].try_into().unwrap()));

    let converted = kms_der_to_evm_signature(&high, &digest, address_of(&key)).unwrap();
    assert!(
        converted.normalized_high_s,
        "the flip must be reported, not silently repaired"
    );
    assert!(!is_high_s(&s_bytes(&converted.signature)));
    assert_eq!(
        secp::recover_address(&digest, &converted.signature).unwrap(),
        address_of(&key)
    );

    // And the normalised result is the canonical signature itself — the
    // `v` was flipped along with `s`, which is the bug this module exists
    // to not have.
    parsed.normalize_s();
    let canonical =
        kms_der_to_evm_signature(&der_over(&key, &digest), &digest, address_of(&key)).unwrap();
    assert_eq!(
        converted.signature.to_bytes(),
        canonical.signature.to_bytes()
    );
}

#[test]
fn no_conversion_ever_yields_a_high_s_signature() {
    for seed in 1u8..=20 {
        let key = secret(seed);
        let digest = [seed.wrapping_add(0x40); 32];
        for der in [der_over(&key, &digest), high_s_der_over(&key, &digest)] {
            let converted = kms_der_to_evm_signature(&der, &digest, address_of(&key)).unwrap();
            assert!(
                !is_high_s(&s_bytes(&converted.signature)),
                "seed {seed} produced a high-s signature"
            );
        }
    }
}

// ------------------------------------------------------ `v` is not guessed --

/// Exactly one recovery byte works, and the conversion picks it by
/// recovering — so the OTHER one must not recover to the same address.
#[test]
fn exactly_one_recovery_byte_recovers_to_the_signer() {
    for seed in 1u8..=15 {
        let key = secret(seed);
        let digest = [seed.wrapping_mul(11); 32];
        let converted =
            kms_der_to_evm_signature(&der_over(&key, &digest), &digest, address_of(&key)).unwrap();

        let chosen = converted.signature.to_bytes();
        let mut other = chosen;
        other[64] = if chosen[64] == 27 { 28 } else { 27 };
        let other = EvmSignature::from_bytes(other).unwrap();
        let recovered = secp::recover_address(&digest, &other);
        assert!(
            recovered.is_err() || recovered.unwrap() != address_of(&key),
            "seed {seed}: both parities recovered to the signer, so `v` was not determined"
        );
    }
}

// -------------------------------------------------------------- refusals --

#[test]
fn a_signature_from_a_different_key_is_refused() {
    let signer = secret(4);
    let impostor = secret(5);
    let digest = [0x44u8; 32];
    let err = kms_der_to_evm_signature(&der_over(&impostor, &digest), &digest, address_of(&signer))
        .unwrap_err();
    assert_eq!(
        err,
        DerConversionError::NoRecoveryMatch {
            expected: address_of(&signer).to_checksum_string()
        }
    );
}

/// The digest is part of the proof: a signature over something else
/// cannot be converted, even though it is a perfectly valid signature
/// from the right key.
#[test]
fn a_signature_over_a_different_digest_is_refused() {
    let key = secret(6);
    let signed = [0x66u8; 32];
    let claimed = [0x77u8; 32];
    let err =
        kms_der_to_evm_signature(&der_over(&key, &signed), &claimed, address_of(&key)).unwrap_err();
    assert!(
        matches!(err, DerConversionError::NoRecoveryMatch { .. }),
        "{err:?}"
    );
}

#[test]
fn malformed_der_is_refused_rather_than_interpreted() {
    let key = secret(8);
    let digest = [0x88u8; 32];
    let good = der_over(&key, &digest);

    let mut truncated = good.clone();
    truncated.pop();
    let mut corrupted = good.clone();
    corrupted[0] ^= 0xff;
    // A valid signature with trailing bytes: `parse_der` is the strict
    // parser, so this is not silently accepted.
    let mut trailing = good.clone();
    trailing.push(0x00);

    for (label, der) in [
        ("empty", Vec::new()),
        ("garbage", vec![0xde, 0xad, 0xbe, 0xef]),
        ("truncated", truncated),
        ("corrupted tag", corrupted),
        ("trailing bytes", trailing),
        // The 64-byte compact form is not DER, and must not be mistaken
        // for it.
        ("compact r||s", good[good.len() - 64..].to_vec()),
    ] {
        let err = kms_der_to_evm_signature(&der, &digest, address_of(&key)).unwrap_err();
        assert!(
            matches!(err, DerConversionError::MalformedDer { .. }),
            "{label} produced {err:?}"
        );
    }
}

/// A well-formed DER signature is still refused when the caller's
/// expected identity is simply somebody else's address — there is no
/// path that returns "the signature anyway".
#[test]
fn an_unrelated_expected_address_never_yields_a_signature() {
    let key = secret(9);
    let digest = [0x99u8; 32];
    let der = der_over(&key, &digest);
    for byte in [0x00u8, 0x01, 0xaa, 0xff] {
        let err = kms_der_to_evm_signature(&der, &digest, EvmAddress::from_bytes([byte; 20]))
            .unwrap_err();
        assert!(
            matches!(err, DerConversionError::NoRecoveryMatch { .. }),
            "{err:?}"
        );
    }
}
