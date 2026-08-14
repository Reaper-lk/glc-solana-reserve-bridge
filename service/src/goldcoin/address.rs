//! Base58Check codec and P2PKH/P2SH address encode/decode for Goldcoin.
//!
//! Reused algorithm from the old bridge's `withdrawal::address`
//! (docs/01-reuse-inventory.md: transaction-construction primitives are
//! chain-mechanics, reusable regardless of trust model). Hand-rolled rather
//! than an external base58 crate — this project prefers hand-rolled parsers
//! over trusting external shapes it hasn't independently verified (same
//! discipline as the RPC client and the on-chain program's `verification.rs`).
//!
//! # Version bytes — regtest only, verified against a real node
//!
//! - P2PKH: `0x6f` (`m`/`n` address prefix) — matches Bitcoin **testnet's**
//!   P2PKH byte, not Bitcoin mainnet's `0x00`.
//! - P2SH (vault): `0x3a` (`Q` address prefix) — Goldcoin-specific, matching
//!   **neither** Bitcoin mainnet (`0x05` → `3`) nor Bitcoin testnet
//!   (`0xc4` → `2`). The old bridge's engineering flagged this as the
//!   single fact most likely to be gotten wrong by a naive Bitcoin-lineage
//!   assumption, and pinned it with a golden vector against real
//!   `createmultisig` output — reused verbatim below
//!   ([`crate::goldcoin::vault`]'s tests).
//!
//! Mainnet version bytes are deliberately absent: this phase is
//! regtest-only by design (docs/11-testing-plan.md — real-node acceptance
//! testing is Phase 6). A mainnet port must independently re-verify against
//! a real Goldcoin mainnet node before reuse — never assume from public
//! docs (see docs/12-management-decisions.md's general caution on
//! unverified chain assumptions).

use ripemd::Ripemd160;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const P2PKH_VERSION_REGTEST: u8 = 0x6f;
pub const P2SH_VERSION_REGTEST: u8 = 0x3a;

const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("empty address string")]
    Empty,
    #[error("invalid base58 character {0:?}")]
    InvalidChar(char),
    #[error("decoded payload has wrong length: expected 25 bytes, got {0}")]
    WrongLength(usize),
    #[error("checksum mismatch")]
    BadChecksum,
    #[error("unexpected version byte {actual:#04x}, expected {expected:#04x}")]
    WrongVersion { expected: u8, actual: u8 },
}

pub fn hash160(data: &[u8]) -> [u8; 20] {
    let sha = Sha256::digest(data);
    let ripe = Ripemd160::digest(sha);
    ripe.into()
}

fn base58_encode(bytes: &[u8]) -> String {
    let leading_zeros = bytes.iter().take_while(|&&b| b == 0).count();
    // Empty, not `vec![0]`: an all-zero input must encode to the
    // leading-'1' prefix ALONE, with no extra trailing digit. A `[0]`
    // placeholder here would survive an all-zero input (the accumulator
    // never touches it) and get emitted as a spurious digit — caught by
    // `base58_round_trips_arbitrary_bytes` before this shipped.
    let mut digits: Vec<u8> = Vec::new();
    for &byte in bytes {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut s: Vec<u8> = std::iter::repeat_n(ALPHABET[0], leading_zeros).collect();
    s.extend(digits.iter().rev().map(|&d| ALPHABET[d as usize]));
    String::from_utf8(s).unwrap()
}

fn base58_decode(s: &str) -> Result<Vec<u8>, AddressError> {
    if s.is_empty() {
        return Err(AddressError::Empty);
    }
    let leading_ones = s.bytes().take_while(|&c| c == b'1').count();
    // Empty, not `vec![0]` — same fix as `base58_encode`, and for the same
    // reason: an all-'1' input (numeric value 0) must decode to the
    // leading-zero-byte prefix ALONE, with no extra trailing zero byte.
    let mut bytes: Vec<u8> = Vec::new();
    for c in s.chars() {
        let digit = ALPHABET
            .iter()
            .position(|&a| a == c as u8)
            .ok_or(AddressError::InvalidChar(c))? as u32;
        let mut carry = digit;
        for b in bytes.iter_mut() {
            carry += (*b as u32) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out: Vec<u8> = std::iter::repeat_n(0u8, leading_ones).collect();
    out.extend(bytes.iter().rev());
    Ok(out)
}

fn checksum(payload: &[u8]) -> [u8; 4] {
    let first = Sha256::digest(payload);
    let second = Sha256::digest(first);
    second[0..4].try_into().unwrap()
}

pub fn base58check_encode(version: u8, hash: &[u8; 20]) -> String {
    let mut payload = vec![version];
    payload.extend_from_slice(hash);
    let cs = checksum(&payload);
    payload.extend_from_slice(&cs);
    base58_encode(&payload)
}

pub fn base58check_decode(s: &str) -> Result<(u8, [u8; 20]), AddressError> {
    let decoded = base58_decode(s)?;
    if decoded.len() != 25 {
        return Err(AddressError::WrongLength(decoded.len()));
    }
    let (payload, given_checksum) = decoded.split_at(21);
    if checksum(payload) != given_checksum {
        return Err(AddressError::BadChecksum);
    }
    let version = payload[0];
    let hash: [u8; 20] = payload[1..21].try_into().unwrap();
    Ok((version, hash))
}

pub fn encode_p2pkh(hash: &[u8; 20]) -> String {
    base58check_encode(P2PKH_VERSION_REGTEST, hash)
}

pub fn decode_p2pkh(s: &str) -> Result<[u8; 20], AddressError> {
    let (version, hash) = base58check_decode(s)?;
    if version != P2PKH_VERSION_REGTEST {
        return Err(AddressError::WrongVersion {
            expected: P2PKH_VERSION_REGTEST,
            actual: version,
        });
    }
    Ok(hash)
}

pub fn encode_p2sh(hash: &[u8; 20]) -> String {
    base58check_encode(P2SH_VERSION_REGTEST, hash)
}

pub fn decode_p2sh(s: &str) -> Result<[u8; 20], AddressError> {
    let (version, hash) = base58check_decode(s)?;
    if version != P2SH_VERSION_REGTEST {
        return Err(AddressError::WrongVersion {
            expected: P2SH_VERSION_REGTEST,
            actual: version,
        });
    }
    Ok(hash)
}

/// `OP_DUP OP_HASH160 <20> <hash> OP_EQUALVERIFY OP_CHECKSIG` — the classic
/// P2PKH scriptPubKey, used for the Goldcoin-side payout destination.
pub fn p2pkh_script_hex(hash: &[u8; 20]) -> String {
    let mut script = vec![0x76u8, 0xa9, 0x14];
    script.extend_from_slice(hash);
    script.extend_from_slice(&[0x88, 0xac]);
    crate::goldcoin::hex::encode(&script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_round_trips_arbitrary_bytes() {
        for bytes in [&[0u8][..], &[1, 2, 3], &[0, 0, 1, 2, 3], &[255u8; 32]] {
            let s = base58_encode(bytes);
            assert_eq!(base58_decode(&s).unwrap(), bytes);
        }
    }

    #[test]
    fn leading_zero_bytes_become_leading_ones() {
        assert_eq!(
            base58_encode(&[0, 0, 5]),
            "11".to_string() + &base58_encode(&[5])
        );
    }

    #[test]
    fn rejects_invalid_characters() {
        assert_eq!(
            base58_decode("0OIl").unwrap_err(),
            AddressError::InvalidChar('0')
        );
    }

    #[test]
    fn p2pkh_round_trip() {
        let hash = [
            0x5au8, 0x7a, 0xb7, 0xad, 0xf8, 0x18, 0x5c, 0x27, 0xb3, 0xf5, 0x41, 0x04, 0xcd, 0xcc,
            0xfe, 0x1f, 0xf0, 0xcd, 0x54, 0xcf,
        ];
        let addr = encode_p2pkh(&hash);
        assert!(
            addr.starts_with('m') || addr.starts_with('n'),
            "regtest P2PKH must use the testnet-style m/n prefix, got {addr}"
        );
        assert_eq!(decode_p2pkh(&addr).unwrap(), hash);
    }

    #[test]
    fn p2sh_round_trip_and_prefix() {
        let hash = [0x11u8; 20];
        let addr = encode_p2sh(&hash);
        assert!(
            addr.starts_with('Q'),
            "regtest P2SH must use Goldcoin's Q prefix, got {addr}"
        );
        assert_eq!(decode_p2sh(&addr).unwrap(), hash);
    }

    #[test]
    fn p2sh_decode_rejects_p2pkh_version_byte() {
        let hash = [0x22u8; 20];
        let p2pkh_addr = encode_p2pkh(&hash);
        assert_eq!(
            decode_p2sh(&p2pkh_addr).unwrap_err(),
            AddressError::WrongVersion {
                expected: P2SH_VERSION_REGTEST,
                actual: P2PKH_VERSION_REGTEST
            }
        );
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let hash = [0x33u8; 20];
        let mut addr = encode_p2pkh(&hash);
        addr.push('1'); // corrupt
        assert!(matches!(
            base58check_decode(&addr),
            Err(AddressError::BadChecksum) | Err(AddressError::WrongLength(_))
        ));
    }

    #[test]
    fn hash160_matches_known_vector() {
        // SHA256("") then RIPEMD160 of that — a fixed, well-known vector
        // independent of any Goldcoin-specific behavior, pinning the
        // primitive itself.
        let h = hash160(b"");
        assert_eq!(
            crate::goldcoin::hex::encode(&h),
            "b472a266d0bd89c13706a4132ccfb16f7c3b9fcb"
        );
    }

    #[test]
    fn p2pkh_script_hex_matches_expected_template() {
        let hash = [0xABu8; 20];
        let script = p2pkh_script_hex(&hash);
        assert_eq!(script, format!("76a914{}88ac", "ab".repeat(20)));
    }
}
