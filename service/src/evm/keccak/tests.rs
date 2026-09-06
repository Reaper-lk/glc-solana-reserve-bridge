use super::*;

use crate::evm::hex::encode_lower;

/// The keccak-256 of the empty string. This is one of the most widely
/// republished constants in Ethereum — it is the hash of empty account
/// code, which every client hardcodes.
const KECCAK256_EMPTY: &str = "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";

#[test]
fn matches_the_published_empty_vector() {
    assert_eq!(encode_lower(&keccak256(b"")), KECCAK256_EMPTY);
}

#[test]
fn matches_the_published_abc_vector() {
    // Original keccak-256("abc"), NOT FIPS-202 SHA3-256("abc").
    assert_eq!(
        encode_lower(&keccak256(b"abc")),
        "0x4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
    );
}

#[test]
fn is_original_keccak_and_not_fips_sha3() {
    // The whole risk this test exists for: the two functions differ only
    // in one padding byte, so a swapped import is silent. SHA3-256("abc")
    // is a different, also-published constant, and must NOT be what
    // `keccak256` produces.
    let sha3_256_abc = "0x3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532";
    assert_ne!(encode_lower(&keccak256(b"abc")), sha3_256_abc);
    assert_eq!(
        encode_lower(&<sha3::Sha3_256 as sha3::Digest>::digest(b"abc")),
        sha3_256_abc,
        "the SHA3-256 vector itself must hold, or this test proves nothing"
    );
}

#[test]
fn agrees_with_the_independent_solana_keccak_implementation() {
    // Solana's `keccak` module is the same function (its on-chain syscall
    // exists for Ethereum interop). Cross-checking against a caller that
    // is already in this crate's dependency tree catches the one mistake a
    // published vector cannot: hashing through the wrong type or
    // finalizing incorrectly.
    for message in [
        b"".as_slice(),
        b"abc",
        b"the quick brown fox",
        &[0xffu8; 137],
    ] {
        assert_eq!(
            keccak256(message),
            solana_sdk::keccak::hash(message).to_bytes(),
            "disagreement on a {}-byte message",
            message.len()
        );
    }
}

#[test]
fn concat_equals_hashing_the_joined_bytes() {
    let a = [0x11u8; 32];
    let b = [0x22u8; 32];
    let c = b"tail".as_slice();

    let mut joined = Vec::new();
    joined.extend_from_slice(&a);
    joined.extend_from_slice(&b);
    joined.extend_from_slice(c);

    assert_eq!(keccak256_concat(&[&a, &b, c]), keccak256(&joined));
}

#[test]
fn concat_of_nothing_is_the_empty_digest() {
    assert_eq!(encode_lower(&keccak256_concat(&[])), KECCAK256_EMPTY);
    assert_eq!(keccak256_concat(&[b""]), keccak256(b""));
}

#[test]
fn concat_is_order_sensitive() {
    let a = [0xaau8; 32];
    let b = [0xbbu8; 32];
    assert_ne!(keccak256_concat(&[&a, &b]), keccak256_concat(&[&b, &a]));
}
