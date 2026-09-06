//! [`EvmAddress`]: a 20-byte EVM account address.

use std::fmt;
use std::str::FromStr;

use super::hex::{self, EvmHexError};
use super::keccak::keccak256;

/// An EVM account address: exactly 20 bytes, always.
///
/// # Parsing rules
///
/// [`EvmAddress::from_str`] accepts exactly one shape:
///
/// - a mandatory, exactly-`0x` prefix (never `0X`, never absent);
/// - exactly 40 hex digits — 39 or 41 is an error, not something to pad or
///   truncate;
/// - all digits ASCII hex;
/// - **and**, if the string mixes upper- and lowercase letter digits, a
///   correct [EIP-55] checksum.
///
/// [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
///
/// # Why the checksum is verified only for genuinely mixed case
///
/// EIP-55 hides a checksum in the *case pattern* of an address's letter
/// digits. A string whose letters are all lowercase, or all uppercase,
/// therefore carries no checksum information at all — it is the plain hex
/// form, which is what most tooling, config files and log lines contain,
/// and rejecting it would reject the majority of legitimate input.
///
/// A string that mixes the two cases is claiming to carry a checksum. If
/// that claim is false, the address has been corrupted (a mistyped digit,
/// a truncated copy/paste that was re-padded elsewhere) and this is the
/// only place that corruption is detectable at all. So mixed case is
/// verified strictly, and a mismatch is an error rather than a warning:
/// this bridge's whole reason to have a typed address is that the next
/// thing anyone does with one is pay value to it.
///
/// # Why [`Display`] is lowercase, not EIP-55
///
/// Two different jobs, two different forms, and they are kept apart on
/// purpose:
///
/// - [`Display`] emits `0x` + 40 **lowercase** digits. It is the canonical
///   machine form: allocation-cheap, hash-free, byte-comparable, stable
///   under any future keccak change, and it is what a dedup key, a log
///   line, a database column and an error message all want. It round-trips
///   through the parser by construction.
/// - [`EvmAddress::to_checksum_string`] emits the EIP-55 mixed-case form.
///   That one is for a human — an operator pasting an address into a
///   config file or a block explorer — because it is the form that can
///   catch their typo. It also round-trips.
///
/// Making `Display` the checksummed form would have meant every log line
/// silently computing a keccak hash, and — worse — two spellings of the
/// same address flowing through comparisons depending on which one the
/// call site used. One machine form, one human form, both parseable.
///
/// [`Display`]: fmt::Display
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvmAddress([u8; 20]);

/// The number of bytes in an EVM address. Fixed by the EVM.
pub const ADDRESS_BYTES: usize = 20;

/// The number of hex digits in the textual form of an address.
pub const ADDRESS_HEX_DIGITS: usize = ADDRESS_BYTES * 2;

/// Why a string or byte sequence is not a valid EVM address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmAddressError {
    /// The `0x`-prefixed 40-hex-digit shape itself is wrong.
    #[error("invalid EVM address: {0}")]
    Hex(#[from] EvmHexError),
    /// The right shape, but the mixed-case EIP-55 checksum does not match
    /// the bytes. Reported with the correctly checksummed spelling of the
    /// bytes as given, so an operator can see exactly which digits differ.
    #[error(
        "EVM address {given} mixes upper- and lowercase digits, so it claims an EIP-55 \
         checksum, but the correct checksummed form of those bytes is {expected} — refusing \
         an address whose checksum does not verify"
    )]
    ChecksumMismatch { given: String, expected: String },
    /// A byte sequence of the wrong width was offered. Never truncated and
    /// never zero-padded to 20 bytes.
    #[error("an EVM address is exactly 20 bytes, got {actual}")]
    WrongByteLength { actual: usize },
}

impl EvmAddress {
    /// The zero address, `0x0000...0000`.
    ///
    /// Deliberately **not** treated as "no address": it is a real, valid
    /// address that the EVM uses as the burn sink and as the `to` of a
    /// contract creation, and a bridge that silently accepted it as a
    /// payout destination would burn user value. Callers that need to
    /// exclude it check [`EvmAddress::is_zero`] explicitly.
    pub const ZERO: EvmAddress = EvmAddress([0u8; ADDRESS_BYTES]);

    /// Wraps 20 bytes that are already an address.
    pub const fn from_bytes(bytes: [u8; ADDRESS_BYTES]) -> EvmAddress {
        EvmAddress(bytes)
    }

    /// Wraps a slice that must be **exactly** 20 bytes long.
    pub fn try_from_slice(bytes: &[u8]) -> Result<EvmAddress, EvmAddressError> {
        <[u8; ADDRESS_BYTES]>::try_from(bytes)
            .map(EvmAddress)
            .map_err(|_| EvmAddressError::WrongByteLength {
                actual: bytes.len(),
            })
    }

    /// The 20 address bytes.
    pub const fn as_bytes(&self) -> &[u8; ADDRESS_BYTES] {
        &self.0
    }

    /// The 20 address bytes, by value.
    pub const fn to_bytes(self) -> [u8; ADDRESS_BYTES] {
        self.0
    }

    /// Whether this is [`EvmAddress::ZERO`]. See that constant on why the
    /// check is the caller's to make.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; ADDRESS_BYTES]
    }

    /// The [EIP-55] mixed-case checksummed form: `0x` + 40 digits whose
    /// letter case encodes a keccak-256 checksum over the lowercase form.
    ///
    /// The human-facing spelling — see the type docs on why this is not
    /// what [`fmt::Display`] produces.
    ///
    /// [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
    pub fn to_checksum_string(&self) -> String {
        // EIP-55: hash the 40 LOWERCASE hex digits (as ASCII, without the
        // "0x"), then uppercase digit i iff nibble i of that hash is >= 8.
        let lower = crate::goldcoin::hex::encode(&self.0);
        let digest = keccak256(lower.as_bytes());

        let mut out = String::with_capacity(hex::PREFIX.len() + ADDRESS_HEX_DIGITS);
        out.push_str(hex::PREFIX);
        for (index, digit) in lower.chars().enumerate() {
            // Nibbles are indexed high-then-low within each digest byte,
            // matching the digit order.
            let nibble = if index.is_multiple_of(2) {
                digest[index / 2] >> 4
            } else {
                digest[index / 2] & 0x0f
            };
            if nibble >= 8 {
                out.push(digit.to_ascii_uppercase());
            } else {
                // Already lowercase; digits 0-9 have no case to change.
                out.push(digit);
            }
        }
        out
    }

    /// Whether `s` carries EIP-55 checksum information, i.e. whether its
    /// letter digits use both cases. Digits `0`-`9` have no case and are
    /// irrelevant to the question.
    fn claims_a_checksum(body: &str) -> bool {
        body.chars().any(|c| c.is_ascii_uppercase()) && body.chars().any(|c| c.is_ascii_lowercase())
    }
}

impl FromStr for EvmAddress {
    type Err = EvmAddressError;

    fn from_str(s: &str) -> Result<EvmAddress, EvmAddressError> {
        let bytes = hex::decode_fixed::<ADDRESS_BYTES>(s)?;
        let address = EvmAddress(bytes);

        // The shape is valid, so the body is exactly the 40 digits after
        // the prefix; only now is it meaningful to ask about their case.
        let body = &s[hex::PREFIX.len()..];
        if EvmAddress::claims_a_checksum(body) {
            let expected = address.to_checksum_string();
            if expected[hex::PREFIX.len()..] != *body {
                return Err(EvmAddressError::ChecksumMismatch {
                    given: s.to_string(),
                    expected,
                });
            }
        }
        Ok(address)
    }
}

impl fmt::Display for EvmAddress {
    /// `0x` + 40 lowercase hex digits. See the type docs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode_lower(&self.0))
    }
}

impl fmt::Debug for EvmAddress {
    /// Hand-written rather than derived: the derived form prints 20 decimal
    /// integers, which is unreadable in a test failure or an `{:?}` log
    /// line and impossible to compare against an address from any other
    /// source.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EvmAddress({self})")
    }
}

#[cfg(test)]
mod tests;
