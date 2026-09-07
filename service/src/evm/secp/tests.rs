//! secp256k1 tests. The curve constants are cross-checked against the
//! library's own arithmetic rather than against prose, and the
//! address-derivation is pinned to a published key/address pair.

use super::*;

/// A well-known test key and its address. This exact pair appears in
/// countless Ethereum test fixtures (it is the key `0x01`), which is what
/// makes it useful: an independent third party has already published what
/// the answer must be.
const KEY_ONE: [u8; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];
const KEY_ONE_ADDRESS: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

fn key(byte: u8) -> EvmSecretKey {
    let mut bytes = [0u8; 32];
    bytes[31] = byte;
    EvmSecretKey::from_bytes(&bytes).unwrap()
}

#[test]
fn group_order_constant_matches_the_library() {
    // The library exposes the order as the curve's own constant; a typo
    // in this file's 32 bytes fails here rather than silently changing
    // what counts as a high `s`.
    // Prove the property that DEFINES n: n-1 is a valid secret key and n
    // itself is not. The library's own scalar validation is the authority
    // here, not a second copy of the constant.
    let mut n_minus_one = SECP256K1_N;
    n_minus_one[31] -= 1;
    assert!(
        EvmSecretKey::from_bytes(&n_minus_one).is_ok(),
        "n-1 must be a valid secret key"
    );
    assert!(
        EvmSecretKey::from_bytes(&SECP256K1_N).is_err(),
        "n itself must not be a valid secret key — if it is, SECP256K1_N is wrong"
    );
}

#[test]
fn half_n_is_exactly_n_over_two() {
    // Long addition: HALF_N + HALF_N + 1 == N, because n is odd.
    let mut sum = [0u8; 32];
    let mut carry = 1u16; // the +1
    for i in (0..32).rev() {
        let total = u16::from(SECP256K1_HALF_N[i]) * 2 + carry;
        sum[i] = (total & 0xff) as u8;
        carry = total >> 8;
    }
    assert_eq!(carry, 0);
    assert_eq!(sum, SECP256K1_N, "HALF_N*2 + 1 must equal N");
}

#[test]
fn derives_the_published_address_for_the_key_one() {
    let key = EvmSecretKey::from_bytes(&KEY_ONE).unwrap();
    assert_eq!(key.address().to_checksum_string(), KEY_ONE_ADDRESS);
}

#[test]
fn rejects_a_zero_secret_key() {
    assert!(matches!(
        EvmSecretKey::from_bytes(&[0u8; 32]),
        Err(EvmSecpError::InvalidSecretKey)
    ));
}

#[test]
fn debug_never_prints_key_material() {
    let key = EvmSecretKey::from_bytes(&KEY_ONE).unwrap();
    let rendered = format!("{key:?}");
    assert!(rendered.contains("redacted"), "{rendered}");
    // `Display` for an address is lowercase hex; the checksum spelling is
    // a separate rendering. Compare case-insensitively so this test pins
    // "the address is present", not which casing Display happens to use.
    assert!(
        rendered
            .to_lowercase()
            .contains(&KEY_ONE_ADDRESS.to_lowercase()),
        "{rendered}"
    );
    // The key is all zeros but the last byte; the giveaway would be a run
    // of hex zeros. Assert the strongest available thing: no 64-hex-digit
    // run appears anywhere in the rendering.
    let hex_run = rendered
        .chars()
        .collect::<Vec<_>>()
        .windows(64)
        .any(|w| w.iter().all(|c| c.is_ascii_hexdigit()));
    assert!(!hex_run, "a 32-byte hex run appeared in Debug: {rendered}");
}

#[test]
fn sign_then_recover_round_trips_to_the_signing_address() {
    let key = key(7);
    let digest = crate::evm::keccak::keccak256(b"an authorization digest");
    let signature = sign_digest(&key, &digest);
    assert_eq!(recover_address(&digest, &signature).unwrap(), key.address());
}

#[test]
fn every_produced_signature_is_low_s() {
    // Sampled across many keys and messages: high-`s` output would make a
    // Solidity `ECDSA.recover` revert, so this property is load-bearing
    // and not merely tidy.
    for k in 1u8..=64 {
        let key = key(k);
        for m in 0u8..8 {
            let digest = crate::evm::keccak::keccak256(&[k, m]);
            let signature = sign_digest(&key, &digest);
            assert!(
                !is_high_s(signature.s()),
                "key {k} message {m} produced a high-s signature"
            );
            assert!(matches!(signature.v(), 27 | 28));
            assert_eq!(recover_address(&digest, &signature).unwrap(), key.address());
        }
    }
}

#[test]
fn recovery_refuses_the_malleable_twin_rather_than_recovering_from_it() {
    let key = key(9);
    let digest = crate::evm::keccak::keccak256(b"malleability");
    let signature = sign_digest(&key, &digest);

    // Build the twin: s' = n - s, v flipped. It is an equally valid
    // ECDSA signature over the same message, and every EIP-2 verifier
    // (including the OpenZeppelin ECDSA the bridge contract uses) must
    // refuse it.
    let mut twin = [0u8; 65];
    twin[..32].copy_from_slice(signature.r());
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let diff = i16::from(SECP256K1_N[i]) - i16::from(signature.s()[i]) - borrow;
        if diff < 0 {
            twin[32 + i] = (diff + 256) as u8;
            borrow = 1;
        } else {
            twin[32 + i] = diff as u8;
            borrow = 0;
        }
    }
    twin[64] = if signature.v() == 27 { 28 } else { 27 };

    let twin = crate::evm::signature::EvmSignature::from_bytes(twin).unwrap();
    assert!(is_high_s(twin.s()), "the twin must be the high-s one");
    assert_eq!(recover_address(&digest, &twin), Err(EvmSecpError::HighS));
}

#[test]
fn a_signature_over_a_different_digest_recovers_to_a_different_address() {
    // Not an error — recovery always yields SOME address — so the check a
    // verifier must perform is "is the recovered address authorized",
    // which is what the bridge contract does. This pins that a tampered
    // digest does not silently recover to the real signer.
    let key = key(11);
    let digest = crate::evm::keccak::keccak256(b"original");
    let tampered = crate::evm::keccak::keccak256(b"tampered");
    let signature = sign_digest(&key, &digest);
    let recovered = recover_address(&tampered, &signature);
    match recovered {
        Ok(address) => assert_ne!(address, key.address()),
        // A digest that recovers to nothing at all is equally fine.
        Err(EvmSecpError::Recovery) => {}
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn signing_is_deterministic_rfc6979() {
    // The same key and digest must produce byte-identical signatures, so
    // a re-signed authorization after a restart is the same one and not a
    // second, differently-shaped signature over the same payload.
    let key = key(13);
    let digest = crate::evm::keccak::keccak256(b"determinism");
    let a = sign_digest(&key, &digest);
    let b = sign_digest(&key, &digest);
    assert_eq!(a.to_bytes(), b.to_bytes());
}

#[test]
fn distinct_keys_produce_distinct_addresses() {
    let mut seen = std::collections::HashSet::new();
    for k in 1u8..=32 {
        assert!(seen.insert(key(k).address()), "address collision at {k}");
    }
}
