//! Authorization-quorum tests.
//!
//! Every signature a signer returns is verified three ways before it
//! counts — recoverable, matching the claimed identity, and a member of
//! the contract's authorized set — and each of those is a refusal rather
//! than a fallback. These tests exercise all three, plus the
//! liveness-tolerance the 2-of-3 exists for.

use super::*;
use crate::evm::secp::{self, EvmSecretKey};
use crate::robinhood::testkit::{signer_addresses, signer_key};
use std::time::Duration;

fn timeout() -> Duration {
    Duration::from_secs(5)
}

fn digest() -> [u8; 32] {
    crate::evm::keccak256(b"an authorization digest")
}

fn dev(index: u8) -> Box<dyn EvmAuthSigner> {
    Box::new(DevEvmAuthSigner::new(signer_key(index)))
}

/// A signer that always fails, standing in for a custody domain that is
/// unreachable or has declined.
struct FailingSigner {
    address: EvmAddress,
    detail: &'static str,
}

impl EvmAuthSigner for FailingSigner {
    fn address(&self) -> EvmAddress {
        self.address
    }
    fn sign_digest<'a>(
        &'a self,
        _digest: &'a [u8; 32],
    ) -> BoxFut<'a, Result<EvmSignature, SignerError>> {
        Box::pin(async move {
            Err(SignerError::Unavailable {
                identity: self.address.to_checksum_string(),
                detail: self.detail.to_string(),
            })
        })
    }
}

/// A signer that claims one identity and signs with another key — the
/// case local verification exists to catch.
struct LyingSigner {
    claimed: EvmAddress,
    key: EvmSecretKey,
}

impl EvmAuthSigner for LyingSigner {
    fn address(&self) -> EvmAddress {
        self.claimed
    }
    fn sign_digest<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFut<'a, Result<EvmSignature, SignerError>> {
        Box::pin(async move { Ok(secp::sign_digest(&self.key, digest)) })
    }
}

/// A signer that returns a HIGH-`s` signature — structurally valid ECDSA
/// that OpenZeppelin's `ECDSA.recover` (and therefore the contract)
/// rejects.
struct MalleableSigner {
    key: EvmSecretKey,
}

impl EvmAuthSigner for MalleableSigner {
    fn address(&self) -> EvmAddress {
        self.key.address()
    }
    fn sign_digest<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFut<'a, Result<EvmSignature, SignerError>> {
        Box::pin(async move {
            let good = secp::sign_digest(&self.key, digest);
            // Flip to the malleable twin: s' = n - s, v flipped.
            let mut twin = [0u8; 65];
            twin[..32].copy_from_slice(good.r());
            let mut borrow = 0i16;
            for i in (0..32).rev() {
                let diff = i16::from(secp::SECP256K1_N[i]) - i16::from(good.s()[i]) - borrow;
                if diff < 0 {
                    twin[32 + i] = (diff + 256) as u8;
                    borrow = 1;
                } else {
                    twin[32 + i] = diff as u8;
                    borrow = 0;
                }
            }
            twin[64] = if good.v() == 27 { 28 } else { 27 };
            Ok(EvmSignature::from_bytes(twin).expect("a structurally valid signature"))
        })
    }
}

#[tokio::test]
async fn a_healthy_pool_produces_exactly_two_distinct_verified_signatures() {
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![dev(0), dev(1), dev(2)];
    let quorum = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .expect("a healthy pool assembles a quorum");

    assert_eq!(quorum.signatures.len(), SIGNER_THRESHOLD);
    assert_ne!(
        quorum.signatures[0].0, quorum.signatures[1].0,
        "two distinct custody domains"
    );
    assert_eq!(quorum.digest, digest());
    // Every stored signature recovers to the address stored alongside it.
    for (address, signature) in quorum.signatures {
        assert_eq!(
            secp::recover_address(&digest(), &signature).unwrap(),
            address
        );
    }
}

#[tokio::test]
async fn exactly_two_are_collected_even_when_three_are_available() {
    // The contract requires EXACTLY `SIGNER_THRESHOLD` and reverts on any
    // other count, so collecting a third would produce calldata the
    // contract refuses.
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![dev(0), dev(1), dev(2)];
    let quorum = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap();
    assert_eq!(quorum.signature_bytes().len(), 2);
    assert_eq!(quorum.for_storage().len(), 2);
}

#[tokio::test]
async fn one_unreachable_custody_domain_is_tolerated() {
    // The whole point of 2-of-3: one domain down is not an outage.
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![
        Box::new(FailingSigner {
            address: signer_addresses()[0],
            detail: "endpoint unreachable",
        }),
        dev(1),
        dev(2),
    ];
    let quorum = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .expect("one failure must be tolerated");
    assert_ne!(quorum.signatures[0].0, signer_addresses()[0]);
}

#[tokio::test]
async fn two_unreachable_domains_cannot_produce_a_quorum() {
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![
        Box::new(FailingSigner {
            address: signer_addresses()[0],
            detail: "down",
        }),
        Box::new(FailingSigner {
            address: signer_addresses()[1],
            detail: "down",
        }),
        dev(2),
    ];
    let err = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap_err();
    assert!(matches!(err, QuorumError::SignerFailed { .. }), "{err}");
}

#[tokio::test]
async fn a_signer_that_misreports_its_own_identity_is_refused_not_skipped() {
    // Answering wrongly is not a liveness failure, so it aborts rather
    // than falling through to the next domain: a domain that returns
    // something it should not have is indistinguishable from one that is
    // compromised.
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![
        Box::new(LyingSigner {
            claimed: signer_addresses()[0],
            key: signer_key(1),
        }),
        dev(1),
        dev(2),
    ];
    let err = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap_err();
    assert!(matches!(err, QuorumError::IdentityMismatch { .. }), "{err}");
}

#[tokio::test]
async fn a_high_s_signature_is_refused_because_the_contract_would_reject_it() {
    // ECDSA is malleable. A verifier that accepted the high-`s` twin
    // would pass a signature that `ECDSA.recover` reverts on — spending
    // gas and a nonce to produce a revert.
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![
        Box::new(MalleableSigner { key: signer_key(0) }),
        dev(1),
        dev(2),
    ];
    let err = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap_err();
    assert!(matches!(err, QuorumError::Unrecoverable { .. }), "{err}");
}

#[tokio::test]
async fn a_signature_from_outside_the_contracts_signer_set_is_refused() {
    // Caught locally, before a nonce is allocated — rather than by the
    // contract, after gas was spent.
    let rogue = EvmSecretKey::from_bytes(&{
        let mut b = [0u8; 32];
        b[31] = 0x77;
        b
    })
    .unwrap();
    let pool: Vec<Box<dyn EvmAuthSigner>> =
        vec![Box::new(DevEvmAuthSigner::new(rogue)), dev(1), dev(2)];
    let err = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap_err();
    assert!(matches!(err, QuorumError::NotAuthorized { .. }), "{err}");
}

#[tokio::test]
async fn a_pool_that_lists_one_domain_twice_cannot_produce_a_quorum_from_it() {
    // Two signatures from one custody domain is a quorum of one, and the
    // contract reverts on `DuplicateSignerSignature`.
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![dev(0), dev(0)];
    let err = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap_err();
    assert!(matches!(err, QuorumError::NotEnoughSigners { .. }), "{err}");
}

#[tokio::test]
async fn fewer_signers_than_the_threshold_is_refused_before_anything_is_asked() {
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![dev(0)];
    assert!(matches!(
        collect_quorum(&pool, &signer_addresses(), &digest(), timeout()).await,
        Err(QuorumError::NotEnoughSigners {
            available: 1,
            required: 2
        })
    ));
    // An EMPTY pool — the production posture until the remote EIP-712
    // signer protocol ships — likewise cannot authorize anything.
    let empty: Vec<Box<dyn EvmAuthSigner>> = Vec::new();
    assert!(
        collect_quorum(&empty, &signer_addresses(), &digest(), timeout())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn signing_a_different_digest_produces_a_different_quorum() {
    // A quorum carries the digest it is over, so it cannot be paired with
    // a different payload.
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![dev(0), dev(1), dev(2)];
    let a = collect_quorum(&pool, &signer_addresses(), &digest(), timeout())
        .await
        .unwrap();
    let other = crate::evm::keccak256(b"a different authorization");
    let b = collect_quorum(&pool, &signer_addresses(), &other, timeout())
        .await
        .unwrap();
    assert_ne!(a.digest, b.digest);
    assert_ne!(a.signatures[0].1.to_bytes(), b.signatures[0].1.to_bytes());
}

#[tokio::test]
async fn a_slow_signer_times_out_and_is_treated_as_unavailable() {
    struct SlowSigner {
        address: EvmAddress,
    }
    impl EvmAuthSigner for SlowSigner {
        fn address(&self) -> EvmAddress {
            self.address
        }
        fn sign_digest<'a>(
            &'a self,
            _digest: &'a [u8; 32],
        ) -> BoxFut<'a, Result<EvmSignature, SignerError>> {
            Box::pin(async move {
                // Real time, not a paused clock: `tokio`'s test-util
                // feature is not enabled in this crate, and a 200ms sleep
                // against a 50ms timeout is deterministic enough without it.
                tokio::time::sleep(Duration::from_millis(200)).await;
                unreachable!("the timeout must fire first")
            })
        }
    }
    let pool: Vec<Box<dyn EvmAuthSigner>> = vec![
        Box::new(SlowSigner {
            address: signer_addresses()[0],
        }),
        dev(1),
        dev(2),
    ];
    // A timeout is a liveness failure, so the 2-of-3 tolerates it.
    let quorum = collect_quorum(
        &pool,
        &signer_addresses(),
        &digest(),
        Duration::from_millis(50),
    )
    .await
    .expect("a timed-out domain must be tolerated like any other failure");
    assert_ne!(quorum.signatures[0].0, signer_addresses()[0]);
}

#[test]
fn the_dev_signer_never_renders_key_material() {
    let signer = DevEvmAuthSigner::new(signer_key(0));
    let rendered = format!("{signer:?}");
    assert!(rendered.contains(&signer_key(0).address().to_string()));
    let hex_run = rendered
        .chars()
        .collect::<Vec<_>>()
        .windows(64)
        .any(|w| w.iter().all(|c| c.is_ascii_hexdigit()));
    assert!(!hex_run, "a 32-byte hex run appeared: {rendered}");
}

#[test]
fn the_threshold_matches_the_contracts_own_constant() {
    // `GlcRobinhoodBridge.SIGNER_THRESHOLD` is 2 and is a CONSTANT there,
    // not configuration: a quorum that can be reconfigured is a quorum
    // that can be configured to one.
    assert_eq!(SIGNER_THRESHOLD, 2);
}
