//! Base58Check codec and P2PKH/P2SH address encode/decode for Goldcoin.
//!
//! Reused algorithm from the old bridge's `withdrawal::address`
//! (docs/01-reuse-inventory.md: transaction-construction primitives are
//! chain-mechanics, reusable regardless of trust model). Hand-rolled rather
//! than an external base58 crate — this project prefers hand-rolled parsers
//! over trusting external shapes it hasn't independently verified (same
//! discipline as the RPC client and the on-chain program's `verification.rs`).
//!
//! # Version bytes — verified against a real goldcoind, both networks
//!
//! Testnet/regtest (`Network::Testnet`):
//! - P2PKH: `0x6f` (`m`/`n` address prefix) — matches Bitcoin **testnet's**
//!   P2PKH byte, not Bitcoin mainnet's `0x00`.
//! - P2SH (vault): `0x3a` (`Q` address prefix) — Goldcoin-specific, matching
//!   **neither** Bitcoin mainnet (`0x05` → `3`) nor Bitcoin testnet
//!   (`0xc4` → `2`).
//!
//! Mainnet (`Network::Mainnet`), verified this phase (docs/16-p0-checkpoint.md)
//! by running the real `goldcoind` binary in an isolated, network-disabled
//! mainnet-mode session (no peers, no sync — `getnewaddress`/
//! `createmultisig` are pure local key/script math, no chain state needed)
//! and decoding its output:
//! - P2PKH: `0x20` (`E` address prefix) — verified against a real
//!   `getnewaddress` + `validateaddress` round trip, checksum confirmed.
//! - P2SH (vault): `0x32` (`M` address prefix) — verified against a real
//!   2-of-3 `createmultisig` output, `hash160(redeemScript)` independently
//!   recomputed and confirmed to match the decoded payload.
//!
//! Both networks' checksum is the standard double-SHA256 first-4-bytes
//! construction (confirmed identical to Bitcoin's), and both use the same
//! Base58Check alphabet. Testnet and regtest were independently verified to
//! share identical version bytes (a real `goldcoind -testnet` session
//! produces the same `0x6f`/`0x3a` this module already had pinned for
//! regtest), so one `Network::Testnet` variant correctly covers both.
//!
//! No source code for Goldcoin's `chainparams.cpp` was available locally to
//! read directly (the build tree that once held it was already gone); the
//! real compiled `goldcoind` binary was used as the authoritative source
//! instead — asking it to generate and validate real addresses is exactly
//! as authoritative as reading the source that produced it, and has the
//! advantage of being independently checksum/hash160-verified against its
//! own output rather than trusted at face value.

use ripemd::Ripemd160;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// `m`/`n` prefix — verified against a real `goldcoind -regtest` and a real
/// `goldcoind -testnet` session; both produce this same byte.
pub const P2PKH_VERSION_TESTNET: u8 = 0x6f;
/// `Q` prefix — same verification as [`P2PKH_VERSION_TESTNET`].
pub const P2SH_VERSION_TESTNET: u8 = 0x3a;
/// `E` prefix — verified against a real, isolated, network-disabled
/// `goldcoind` mainnet session (docs/16-p0-checkpoint.md).
pub const P2PKH_VERSION_MAINNET: u8 = 0x20;
/// `M` prefix — same verification as [`P2PKH_VERSION_MAINNET`].
pub const P2SH_VERSION_MAINNET: u8 = 0x32;

/// Which Goldcoin network an address/script belongs to. Every
/// encode/decode function in this module takes one explicitly — there is
/// deliberately no default, so a caller can never silently derive a
/// mainnet-shaped value while thinking it configured testnet, or vice
/// versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    /// Covers both testnet and regtest — verified to share identical
    /// version bytes (see module docs).
    Testnet,
}

impl Network {
    pub fn p2pkh_version(self) -> u8 {
        match self {
            Network::Mainnet => P2PKH_VERSION_MAINNET,
            Network::Testnet => P2PKH_VERSION_TESTNET,
        }
    }

    pub fn p2sh_version(self) -> u8 {
        match self {
            Network::Mainnet => P2SH_VERSION_MAINNET,
            Network::Testnet => P2SH_VERSION_TESTNET,
        }
    }
}

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
    #[error("scriptPubKey is not valid hex: {0}")]
    ScriptNotHex(String),
    #[error(
        "scriptPubKey is not a canonical P2PKH script (expected 25 bytes \
         76a914<20-byte-hash>88ac, got {len} bytes starting {prefix})"
    )]
    NotP2pkhScript { len: usize, prefix: String },
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

pub fn encode_p2pkh(hash: &[u8; 20], network: Network) -> String {
    base58check_encode(network.p2pkh_version(), hash)
}

pub fn decode_p2pkh(s: &str, network: Network) -> Result<[u8; 20], AddressError> {
    let (version, hash) = base58check_decode(s)?;
    let expected = network.p2pkh_version();
    if version != expected {
        return Err(AddressError::WrongVersion {
            expected,
            actual: version,
        });
    }
    Ok(hash)
}

pub fn encode_p2sh(hash: &[u8; 20], network: Network) -> String {
    base58check_encode(network.p2sh_version(), hash)
}

pub fn decode_p2sh(s: &str, network: Network) -> Result<[u8; 20], AddressError> {
    let (version, hash) = base58check_decode(s)?;
    let expected = network.p2sh_version();
    if version != expected {
        return Err(AddressError::WrongVersion {
            expected,
            actual: version,
        });
    }
    Ok(hash)
}

/// `OP_DUP OP_HASH160 <20> <hash> OP_EQUALVERIFY OP_CHECKSIG` — the classic
/// P2PKH scriptPubKey, used for the Goldcoin-side payout destination.
/// Network-independent: the script format is the same on every network,
/// only the address *encoding* of a hash differs.
pub fn p2pkh_script_hex(hash: &[u8; 20]) -> String {
    let mut script = vec![0x76u8, 0xa9, 0x14];
    script.extend_from_slice(hash);
    script.extend_from_slice(&[0x88, 0xac]);
    crate::goldcoin::hex::encode(&script)
}

/// The exact inverse of [`p2pkh_script_hex`]: recovers the 20-byte
/// hash160 from a scriptPubKey, accepting ONLY the canonical 25-byte
/// P2PKH template `76 a9 14 <20 bytes> 88 ac`.
///
/// Deliberately strict, because this is how a refund destination is
/// derived from what a depositor actually spent
/// (`crate::goldcoin::refund`). Every other script form — P2SH, bare
/// multisig, P2PK, OP_RETURN, segwit, or a P2PKH-shaped script with any
/// byte out of place — is rejected rather than approximated. There is no
/// "best effort" reading of a script that decides where real money is
/// sent: an unrecognised script means the sender cannot be established
/// unambiguously, which is a refusal.
///
/// Network-independent for the same reason as its inverse: the script
/// template is identical on every network; only the base58 *encoding* of
/// the recovered hash differs, and that is [`encode_p2pkh`]'s job.
pub fn p2pkh_hash_from_script_hex(script_hex: &str) -> Result<[u8; 20], AddressError> {
    let bytes = crate::goldcoin::hex::decode_vec(script_hex)
        .map_err(|e| AddressError::ScriptNotHex(e.to_string()))?;
    let malformed = || AddressError::NotP2pkhScript {
        len: bytes.len(),
        prefix: script_hex.chars().take(8).collect(),
    };
    if bytes.len() != 25 {
        return Err(malformed());
    }
    // Check every fixed byte of the template, not just the prefix: a
    // script that merely STARTS like P2PKH but ends differently spends to
    // different rules entirely.
    if bytes[0] != 0x76 || bytes[1] != 0xa9 || bytes[2] != 0x14 {
        return Err(malformed());
    }
    if bytes[23] != 0x88 || bytes[24] != 0xac {
        return Err(malformed());
    }
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&bytes[3..23]);
    Ok(hash)
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
    fn p2pkh_round_trip_testnet() {
        let hash = [
            0x5au8, 0x7a, 0xb7, 0xad, 0xf8, 0x18, 0x5c, 0x27, 0xb3, 0xf5, 0x41, 0x04, 0xcd, 0xcc,
            0xfe, 0x1f, 0xf0, 0xcd, 0x54, 0xcf,
        ];
        let addr = encode_p2pkh(&hash, Network::Testnet);
        assert!(
            addr.starts_with('m') || addr.starts_with('n'),
            "testnet/regtest P2PKH must use the testnet-style m/n prefix, got {addr}"
        );
        assert_eq!(decode_p2pkh(&addr, Network::Testnet).unwrap(), hash);
    }

    #[test]
    fn p2sh_round_trip_and_prefix_testnet() {
        let hash = [0x11u8; 20];
        let addr = encode_p2sh(&hash, Network::Testnet);
        assert!(
            addr.starts_with('Q'),
            "testnet/regtest P2SH must use Goldcoin's Q prefix, got {addr}"
        );
        assert_eq!(decode_p2sh(&addr, Network::Testnet).unwrap(), hash);
    }

    #[test]
    fn p2sh_decode_rejects_p2pkh_version_byte() {
        let hash = [0x22u8; 20];
        let p2pkh_addr = encode_p2pkh(&hash, Network::Testnet);
        assert_eq!(
            decode_p2sh(&p2pkh_addr, Network::Testnet).unwrap_err(),
            AddressError::WrongVersion {
                expected: P2SH_VERSION_TESTNET,
                actual: P2PKH_VERSION_TESTNET
            }
        );
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let hash = [0x33u8; 20];
        let mut addr = encode_p2pkh(&hash, Network::Testnet);
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

    // ------------------------------------------- mainnet golden vectors --
    //
    // Real addresses produced by a real, isolated, network-disabled
    // `goldcoind` mainnet session (docs/16-p0-checkpoint.md) —
    // independently checksum-verified and, for the P2SH vector,
    // hash160(redeemScript)-verified against the decoded payload before
    // being pinned here. Not fabricated or guessed from Bitcoin/Litecoin
    // conventions.

    #[test]
    fn mainnet_p2pkh_golden_vector() {
        // Real `getnewaddress` output; scriptPubKey from a real
        // `validateaddress` call was
        // 76a914f4df99de081ed239e7431d3478b96bb8e7b44fa988ac, confirming
        // this hash160.
        let hash: [u8; 20] = [
            0xf4, 0xdf, 0x99, 0xde, 0x08, 0x1e, 0xd2, 0x39, 0xe7, 0x43, 0x1d, 0x34, 0x78, 0xb9,
            0x6b, 0xb8, 0xe7, 0xb4, 0x4f, 0xa9,
        ];
        let addr = encode_p2pkh(&hash, Network::Mainnet);
        assert_eq!(addr, "EG95FMYz9Pju3r6z6gA5tNoShHMXGjEHwj");
        assert!(
            addr.starts_with('E'),
            "mainnet P2PKH must use Goldcoin's E prefix, got {addr}"
        );
        assert_eq!(decode_p2pkh(&addr, Network::Mainnet).unwrap(), hash);
    }

    #[test]
    fn mainnet_p2sh_multisig_golden_vector() {
        // Real 2-of-3 `createmultisig` output; hash160(redeemScript) was
        // independently recomputed from the real redeemScript bytes and
        // confirmed to match this decoded payload before being pinned.
        let hash: [u8; 20] = [
            0x31, 0x7c, 0x35, 0xa8, 0xdc, 0x8c, 0x6d, 0x4e, 0xe4, 0x49, 0x06, 0x77, 0xb6, 0x37,
            0x61, 0x61, 0x54, 0x8a, 0x5b, 0xb4,
        ];
        let addr = encode_p2sh(&hash, Network::Mainnet);
        assert_eq!(addr, "MCQp94i1bMnZeqdLg1Y53FqWtLcz1Q1BkY");
        assert!(
            addr.starts_with('M'),
            "mainnet P2SH must use Goldcoin's M prefix, got {addr}"
        );
        assert_eq!(decode_p2sh(&addr, Network::Mainnet).unwrap(), hash);
    }

    #[test]
    fn mainnet_and_testnet_never_accept_each_others_addresses() {
        let hash = [0x44u8; 20];
        let mainnet_addr = encode_p2pkh(&hash, Network::Mainnet);
        let testnet_addr = encode_p2pkh(&hash, Network::Testnet);
        assert_ne!(mainnet_addr, testnet_addr);
        assert!(decode_p2pkh(&mainnet_addr, Network::Testnet).is_err());
        assert!(decode_p2pkh(&testnet_addr, Network::Mainnet).is_err());
    }
}
