//! The startup identity proof.

use super::fake::{FakeBehaviour, FakeKms};
use super::*;
use crate::evm::EvmAddress;

#[tokio::test]
async fn a_matching_key_proves_the_configured_identity() {
    let kms = FakeKms::new(0x21);
    prove_key_controls_address(&kms, kms.address())
        .await
        .expect("a key that controls the address must prove it");
    assert_eq!(
        kms.calls(),
        1,
        "the proof is one signature, not a retry loop"
    );
    assert_eq!(
        kms.signed_digests(),
        vec![startup_challenge_digest()],
        "the proof must sign the fixed challenge and nothing else"
    );
}

/// The whole point: a configured address the key does not control is a
/// startup failure, not a warning. Serving `GET /v2/evm-identity` with
/// an address this process cannot sign as would let the bridge build a
/// quorum around a signer that can never contribute to it.
#[tokio::test]
async fn a_key_that_controls_a_different_address_fails_the_proof() {
    let kms = FakeKms::new(0x22);
    let wrong = kms.wrong_address();
    assert_ne!(wrong, kms.address(), "test setup");

    let err = prove_key_controls_address(&kms, wrong).await.unwrap_err();
    assert!(
        matches!(
            err,
            IdentityProofError::Conversion(
                super::super::der::DerConversionError::NoRecoveryMatch { .. }
            )
        ),
        "{err:?}"
    );

    // And symmetrically: the right key against an unrelated address.
    let err = prove_key_controls_address(&kms, EvmAddress::from_bytes([0xab; 20]))
        .await
        .unwrap_err();
    assert!(matches!(err, IdentityProofError::Conversion(_)), "{err:?}");
}

#[tokio::test]
async fn a_backend_that_answers_with_a_different_keys_signature_fails_the_proof() {
    let kms = FakeKms::new(0x23);
    kms.set_behaviour(FakeBehaviour::SignWithWrongKey);
    let err = prove_key_controls_address(&kms, kms.address())
        .await
        .unwrap_err();
    assert!(matches!(err, IdentityProofError::Conversion(_)), "{err:?}");
}

#[tokio::test]
async fn a_backend_that_answers_with_malformed_der_fails_the_proof() {
    let kms = FakeKms::new(0x24);
    kms.set_behaviour(FakeBehaviour::ReturnGarbage);
    let err = prove_key_controls_address(&kms, kms.address())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            IdentityProofError::Conversion(
                super::super::der::DerConversionError::MalformedDer { .. }
            )
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_unavailable_backend_fails_the_proof_rather_than_starting_anyway() {
    let kms = FakeKms::new(0x25);
    kms.set_behaviour(FakeBehaviour::Fail(KmsError::Unavailable(
        "connection refused".to_string(),
    )));
    let err = prove_key_controls_address(&kms, kms.address())
        .await
        .unwrap_err();
    assert!(matches!(err, IdentityProofError::Kms(_)), "{err:?}");
}

/// A high-`s` answer is a real signature from the right key, so it
/// proves control — the conversion normalises it, exactly as it does on
/// the signing path.
#[tokio::test]
async fn a_high_s_answer_still_proves_control() {
    let kms = FakeKms::new(0x26);
    kms.set_behaviour(FakeBehaviour::SignHighS);
    prove_key_controls_address(&kms, kms.address())
        .await
        .expect("a high-s signature from the right key still proves control");
}

// ------------------------------------------------------------ the challenge --

/// The challenge is fixed and domain-separated, and — critically — it is
/// not shaped like anything this signer will ever be asked to authorize.
/// An EIP-712 typed-data hash is `keccak256` of a 66-byte preimage
/// beginning `0x19 0x01`; this is `keccak256` of an ASCII string.
#[test]
fn the_challenge_is_fixed_and_cannot_be_an_eip712_digest() {
    assert_eq!(startup_challenge_digest(), startup_challenge_digest());
    assert_eq!(
        startup_challenge_digest(),
        crate::evm::keccak::keccak256(STARTUP_CHALLENGE_DOMAIN)
    );

    // Its preimage is neither 66 bytes nor 0x1901-prefixed, so it is not
    // the preimage of any typed-data hash.
    assert_ne!(STARTUP_CHALLENGE_DOMAIN.len(), 66);
    assert_ne!(&STARTUP_CHALLENGE_DOMAIN[..2], &[0x19, 0x01]);
    assert!(STARTUP_CHALLENGE_DOMAIN.starts_with(b"glc.reserve-bridge.robinhood"));
}

/// The proof signs the challenge and nothing derived from a request.
#[tokio::test]
async fn the_proof_never_signs_anything_but_the_challenge() {
    let kms = FakeKms::new(0x27);
    for _ in 0..3 {
        prove_key_controls_address(&kms, kms.address())
            .await
            .unwrap();
    }
    let signed = kms.signed_digests();
    assert_eq!(signed.len(), 3);
    assert!(signed.iter().all(|d| *d == startup_challenge_digest()));
}

#[test]
fn kms_error_categories_are_stable_and_carry_no_detail() {
    let cases = [
        (
            KmsError::Unavailable("host down".into()),
            "unavailable",
            true,
        ),
        (KmsError::Refused("AccessDenied".into()), "refused", false),
        (KmsError::EmptySignature, "empty_signature", false),
        (
            KmsError::WrongKeyShape("RSA_2048".into()),
            "wrong_key_shape",
            false,
        ),
    ];
    for (err, category, retriable) in cases {
        assert_eq!(err.category(), category);
        assert_eq!(err.is_retriable(), retriable);
        // The category is what gets logged and returned to the bridge; it
        // must be a fixed token, never the interpolated detail.
        assert!(!err.category().contains(' '));
    }
}
