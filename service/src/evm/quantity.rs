//! Ethereum JSON-RPC hex encoding: `QUANTITY` and `DATA`.
//!
//! # Two encodings that look alike and are not interchangeable
//!
//! The Ethereum JSON-RPC specification defines two hex encodings, and every
//! `eth_*` field is one or the other:
//!
//! | | `QUANTITY` | `DATA` |
//! |---|---|---|
//! | means | a number | a byte string |
//! | width | **minimal** — no leading zeros | fixed by the byte count |
//! | zero | `"0x0"` | `"0x"` (empty) or `"0x00"` (one byte) |
//! | digit count | any, at least one | always even |
//! | examples | `blockNumber`, `logIndex`, `chainId`, `value` | `topics`, `data`, a hash, an address |
//!
//! `"0x0"` is a valid `QUANTITY` and an invalid `DATA` (odd digit count).
//! `"0x00"` is a valid `DATA` (one zero byte) and an invalid `QUANTITY`
//! (non-minimal). They are close enough to be confused and different enough
//! that confusing them corrupts values, so this module keeps them apart by
//! function name and rejects each other's forms.
//!
//! # Why leading zeros are rejected in a `QUANTITY`
//!
//! The specification requires the minimal form, so `"0x00"` and `"0x0123"`
//! are not values a conforming node emits. Accepting them would mean a
//! value has more than one spelling, which matters the moment a spelling is
//! used as a key: two `eth_getLogs` responses spelling the same log index
//! differently would dedup as two different logs. Rejecting them also
//! catches the genuinely dangerous case — a fixed-width `DATA` word being
//! read as a `QUANTITY`, which is a real client bug and which would
//! otherwise pass silently for every value whose top byte happens to be
//! non-zero.
//!
//! The one exception the specification itself carves out is `"0x0"` for
//! zero, which is accepted and is what the encoders emit.
//!
//! # No RPC client
//!
//! These are pure string functions. This phase builds no transport, issues
//! no request and parses no response envelope; a later phase's client is
//! expected to use these rather than reach for a general-purpose hex crate.

use super::hex::{self, EvmHexError};
use super::u256::{EvmU256, WORD_BYTES};

/// Why a JSON-RPC hex string could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmQuantityError {
    /// The `0x` prefix or the hex digits themselves are wrong.
    #[error("invalid Ethereum JSON-RPC hex value: {0}")]
    Hex(#[from] EvmHexError),
    /// `"0x"` with no digits. Legal for `DATA` (a zero-length byte string),
    /// never for a `QUANTITY` — a number needs at least one digit.
    #[error("a QUANTITY needs at least one hex digit after \"0x\" (\"0x0\" spells zero)")]
    EmptyQuantity,
    /// A sign character. Every JSON-RPC `QUANTITY` is unsigned; a negative
    /// value is not a representable quantity, and is reported as its own
    /// error rather than as an invalid digit so the cause is unambiguous.
    #[error("{found:?} is a sign character; every JSON-RPC QUANTITY is unsigned")]
    Signed { found: char },
    /// A non-minimal spelling, e.g. `"0x00"` or `"0x0123"`. See the module
    /// docs.
    #[error(
        "{value:?} is not the minimal QUANTITY form: a leading zero digit is only valid in \
         \"0x0\" itself, and a fixed-width value here usually means a DATA word is being \
         read as a number"
    )]
    LeadingZero { value: String },
    /// More digits than the destination width can hold. Never truncated.
    #[error(
        "QUANTITY {value:?} has {digits} hex digit(s), more than the {max_digits} that fit \
         a u{bits} — refusing to truncate"
    )]
    Overflow {
        value: String,
        digits: usize,
        max_digits: usize,
        bits: u32,
    },
}

/// The minimal-width `QUANTITY` spelling of a `u64`.
pub fn encode_quantity_u64(value: u64) -> String {
    // `{:x}` is already minimal-width for a primitive integer, and prints a
    // bare `0` for zero — which is exactly the specification's `"0x0"`.
    format!("{}{value:x}", hex::PREFIX)
}

/// The minimal-width `QUANTITY` spelling of a 256-bit word.
///
/// No 256-bit arithmetic is involved: the word's bytes are rendered and the
/// leading zero digits are then dropped, with zero rendering as `"0x0"`.
pub fn encode_quantity_u256(value: EvmU256) -> String {
    let full = crate::goldcoin::hex::encode(value.as_be_bytes());
    let trimmed = full.trim_start_matches('0');
    let mut out = String::with_capacity(hex::PREFIX.len() + trimmed.len().max(1));
    out.push_str(hex::PREFIX);
    if trimmed.is_empty() {
        out.push('0');
    } else {
        out.push_str(trimmed);
    }
    out
}

/// Validates a `QUANTITY` and returns its digits, without the `0x` and with
/// no leading zeros (so `"0x0"` yields `"0"`).
fn quantity_digits(s: &str) -> Result<&str, EvmQuantityError> {
    // A sign is checked before the prefix so that "-0x1" reports the sign
    // rather than a missing prefix, which is the more useful diagnosis.
    if let Some(found) = s.chars().next().filter(|c| *c == '-' || *c == '+') {
        return Err(EvmQuantityError::Signed { found });
    }
    let body = hex::strip_prefix(s)?;
    if body.is_empty() {
        return Err(EvmQuantityError::EmptyQuantity);
    }
    if let Some((position, found)) = hex::find_non_hex(body) {
        return Err(EvmQuantityError::Hex(EvmHexError::InvalidDigit {
            position,
            found,
        }));
    }
    if body.len() > 1 && body.starts_with('0') {
        return Err(EvmQuantityError::LeadingZero {
            value: s.chars().take(72).collect(),
        });
    }
    Ok(body)
}

/// Parses a `QUANTITY` into a `u64`.
///
/// Rejects a missing or non-`0x` prefix, an empty body, a sign, non-hex
/// digits, a non-minimal spelling, and anything above `u64::MAX`.
pub fn parse_quantity_u64(s: &str) -> Result<u64, EvmQuantityError> {
    let digits = quantity_digits(s)?;
    // The minimal form has no leading zeros, so more than 16 digits is
    // genuinely out of range rather than merely padded.
    if digits.len() > 16 {
        return Err(EvmQuantityError::Overflow {
            value: s.chars().take(72).collect(),
            digits: digits.len(),
            max_digits: 16,
            bits: 64,
        });
    }
    u64::from_str_radix(digits, 16).map_err(|_| EvmQuantityError::Overflow {
        value: s.chars().take(72).collect(),
        digits: digits.len(),
        max_digits: 16,
        bits: 64,
    })
}

/// Parses a `QUANTITY` into a 256-bit word — the full `uint256` range,
/// nothing narrowed.
///
/// Rejects the same malformed shapes as [`parse_quantity_u64`], plus
/// anything wider than 64 digits: `2^256 - 1` is 64 digits, so a 65-digit
/// minimal-form quantity is a value the EVM itself cannot represent, and is
/// refused rather than reduced.
pub fn parse_quantity_u256(s: &str) -> Result<EvmU256, EvmQuantityError> {
    let digits = quantity_digits(s)?;
    if digits.len() > WORD_BYTES * 2 {
        return Err(EvmQuantityError::Overflow {
            value: s.chars().take(72).collect(),
            digits: digits.len(),
            max_digits: WORD_BYTES * 2,
            bits: 256,
        });
    }
    // Left-pad the minimal digits to the full width, then decode. Padding
    // here is not the silent kind: the value is known to be in range because
    // of the digit-count check above, and a big-endian number's leading
    // zeros are part of its fixed-width encoding, not information.
    let padded = format!("{:0>width$}", digits, width = WORD_BYTES * 2);
    let bytes = crate::goldcoin::hex::decode_exact::<WORD_BYTES>(&padded).map_err(|_| {
        // Unreachable: `quantity_digits` already proved every character is
        // an ASCII hex digit and the padding adds only zeros.
        EvmQuantityError::Hex(EvmHexError::WrongLength {
            expected: WORD_BYTES * 2,
            actual: padded.len(),
        })
    })?;
    Ok(EvmU256::from_be_bytes(bytes))
}

/// The `DATA` spelling of a byte string: `0x` plus two lowercase digits per
/// byte, leading zeros kept, empty for an empty input.
pub fn encode_data(bytes: &[u8]) -> String {
    hex::encode_lower(bytes)
}

/// Parses a `DATA` value of any length into bytes. Requires an even digit
/// count; accepts the empty body `"0x"`.
pub fn parse_data(s: &str) -> Result<Vec<u8>, EvmQuantityError> {
    hex::decode_var(s).map_err(EvmQuantityError::from)
}

/// Parses a `DATA` value that must be exactly `N` bytes — a topic, a hash,
/// an ABI word. Never pads and never truncates to reach `N`.
pub fn parse_data_exact<const N: usize>(s: &str) -> Result<[u8; N], EvmQuantityError> {
    hex::decode_fixed::<N>(s).map_err(EvmQuantityError::from)
}

#[cfg(test)]
mod tests;
