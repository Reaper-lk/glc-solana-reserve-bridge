//! [`EvmU256`]: the 256-bit EVM ABI/RPC boundary word.
//!
//! # What this type is for, and what it deliberately is not
//!
//! The EVM ABI and `eth_*` JSON-RPC speak in 256-bit words. An ERC-20
//! `balanceOf`, `Transfer` event value or `allowance` is a `uint256`, and a
//! bridge that decoded one into a `u64` — or into a `u128` without checking
//! — would be one hostile or malfunctioning chain read away from paying out
//! a truncated amount.
//!
//! This service's **accounting** does not speak in 256-bit words and must
//! not start. Its canonical ledger unit is an 8-decimal `u64`
//! ([`crate::amount_conversion`]); the EVM-side 18-decimal unit is a
//! `u128`, which holds every amount the bridge can represent with nine
//! orders of magnitude to spare. [`EvmU256`] sits strictly at the boundary
//! between the two: it exists to **hold** a word off the wire long enough
//! to check it and narrow it, and to widen an outbound value losslessly.
//! It is not, and must not become, the type the bridge does arithmetic in.
//!
//! # Why there is no 256-bit arithmetic library here
//!
//! Because there is no 256-bit arithmetic. The type has no `Add`, no `Mul`,
//! no `checked_*`; the complete set of operations is: wrap 32 big-endian
//! bytes, hand 32 big-endian bytes back, widen exactly from `u64`/`u128`,
//! narrow to `u64`/`u128` with an error on overflow, compare, and encode as
//! hex. Every one of those is a byte move, a zero-check or a comparison —
//! there is nothing here that a library like `ruint` or `alloy-primitives`
//! would implement better, and nothing that could be got subtly wrong the
//! way a 256-bit multiply could.
//!
//! Adding one anyway would have costs and no benefit: it puts a
//! general-purpose big-integer type in scope at every call site that
//! touches an amount, which is an invitation to do accounting in it, which
//! is the exact defect the `u64`/`u128` split exists to prevent. (`ruint`
//! 1.20 also declares `rust-version = 1.90`, above this repository's pinned
//! stable toolchain, so it would raise the floor as well.)
//!
//! If a later phase genuinely needs 256-bit arithmetic — the plausible case
//! is comparing against or computing a contract-side limit that cannot be
//! narrowed first — the migration is mechanical and lossless: every EVM
//! library agrees on big-endian bytes, so
//! [`EvmU256::to_be_bytes`]/[`EvmU256::from_be_bytes`] convert to and from
//! `ruint::aliases::U256` (or `alloy_primitives::U256`) in one call with no
//! semantic change. Nothing about this type has to be unpicked first. That
//! is the reason the byte accessors, and not the integer conversions, are
//! the primary interface.
//!
//! # Ordering is numeric
//!
//! `PartialOrd`/`Ord` are derived over the big-endian byte array. For an
//! unsigned big-endian encoding, lexicographic byte order and numeric order
//! are the same relation — the most significant byte is compared first, and
//! ties fall through to the next — so the derived comparison is the correct
//! numeric one, with no need to widen anything. The tests pin this down at
//! the boundaries where a mistake would show.

use std::fmt;

use super::hex::{self, EvmHexError};

/// The number of bytes in a 256-bit EVM word.
pub const WORD_BYTES: usize = 32;

/// Why a 256-bit word could not be narrowed to a native integer.
///
/// Narrowing is the only fallible operation this type has, and it fails
/// loudly rather than truncating: a word too large for the destination is
/// either a different asset's units, a decoding error, or an attack, and
/// none of those should quietly become a payable amount.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmU256Error {
    /// The word does not fit the destination width. `bits` is the
    /// destination width, so the message says what it would have had to
    /// discard.
    #[error(
        "EVM word {value} does not fit a u{bits}: bits above the low {bits} are set, and \
         truncating them would change the value"
    )]
    Overflow { value: EvmU256, bits: u32 },
    /// A byte sequence of the wrong width was offered as a word. Never
    /// left-padded and never truncated.
    #[error("a 256-bit EVM word is exactly 32 bytes, got {actual}")]
    WrongByteLength { actual: usize },
    /// The hex form of a word was malformed. See
    /// [`crate::evm::quantity`] for the JSON-RPC quantity spelling, which
    /// has different (minimal-form) rules from this fixed-width one.
    #[error("invalid 256-bit EVM word: {0}")]
    Hex(#[from] EvmHexError),
}

/// A 256-bit unsigned EVM word, stored big-endian — the byte order the ABI,
/// `eth_getLogs` topics and data, and every EVM library already agree on.
///
/// Deliberately inert: no arithmetic operators, no `From<u64>`-style
/// implicit widening in expressions, no `Default`. See the module docs for
/// why, and for the migration path if 256-bit arithmetic ever becomes
/// genuinely necessary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvmU256([u8; WORD_BYTES]);

impl EvmU256 {
    /// Zero.
    pub const ZERO: EvmU256 = EvmU256([0u8; WORD_BYTES]);

    /// One.
    pub const ONE: EvmU256 = EvmU256::from_u128(1);

    /// `2^256 - 1`, the largest representable word.
    pub const MAX: EvmU256 = EvmU256([0xffu8; WORD_BYTES]);

    /// Wraps 32 big-endian bytes as they came off the wire.
    pub const fn from_be_bytes(bytes: [u8; WORD_BYTES]) -> EvmU256 {
        EvmU256(bytes)
    }

    /// The 32 big-endian bytes, for ABI encoding.
    pub const fn to_be_bytes(self) -> [u8; WORD_BYTES] {
        self.0
    }

    /// The 32 big-endian bytes, by reference.
    pub const fn as_be_bytes(&self) -> &[u8; WORD_BYTES] {
        &self.0
    }

    /// Wraps a slice that must be **exactly** 32 bytes long. An ABI word is
    /// never shorter and never longer; a short read is a decoding failure,
    /// not a value to left-pad.
    pub fn try_from_be_slice(bytes: &[u8]) -> Result<EvmU256, EvmU256Error> {
        <[u8; WORD_BYTES]>::try_from(bytes)
            .map(EvmU256)
            .map_err(|_| EvmU256Error::WrongByteLength {
                actual: bytes.len(),
            })
    }

    /// Lossless widening of a `u128`. Exact and infallible by construction:
    /// every `u128` value is a `u256` value, so there is no error case and
    /// no `Result` to ignore.
    pub const fn from_u128(value: u128) -> EvmU256 {
        let low = value.to_be_bytes();
        let mut bytes = [0u8; WORD_BYTES];
        let mut i = 0;
        // Right-align the 16-byte value in the 32-byte word; the high 16
        // bytes stay zero. A `while` loop rather than an iterator because
        // this is a `const fn`.
        while i < 16 {
            bytes[16 + i] = low[i];
            i += 1;
        }
        EvmU256(bytes)
    }

    /// Lossless widening of a `u64`. Exact and infallible, as
    /// [`EvmU256::from_u128`].
    pub const fn from_u64(value: u64) -> EvmU256 {
        EvmU256::from_u128(value as u128)
    }

    /// Narrows to a `u128`, rejecting any word whose high 128 bits are set.
    /// Never truncates, never casts with `as`, never saturates.
    pub fn try_to_u128(self) -> Result<u128, EvmU256Error> {
        let (high, low) = self.0.split_at(WORD_BYTES / 2);
        if high.iter().any(|byte| *byte != 0) {
            return Err(EvmU256Error::Overflow {
                value: self,
                bits: 128,
            });
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(low);
        Ok(u128::from_be_bytes(buf))
    }

    /// Narrows to a `u64`, rejecting any word whose high 192 bits are set.
    ///
    /// Useful for the values that genuinely are `u64`-shaped in practice
    /// even though the ABI declares them `uint256`: a block number, a log
    /// index, a nonce, a timestamp.
    pub fn try_to_u64(self) -> Result<u64, EvmU256Error> {
        let (high, low) = self.0.split_at(WORD_BYTES - 8);
        if high.iter().any(|byte| *byte != 0) {
            return Err(EvmU256Error::Overflow {
                value: self,
                bits: 64,
            });
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(low);
        Ok(u64::from_be_bytes(buf))
    }

    /// Whether this word is zero.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; WORD_BYTES]
    }

    /// The full-width, `0x`-prefixed, 64-lowercase-digit form: the
    /// **`DATA`** spelling of the word, all 32 bytes, leading zeros kept.
    ///
    /// This is what [`fmt::Display`] produces, and it is *not* the
    /// Ethereum JSON-RPC `QUANTITY` spelling, which is minimal-width — see
    /// [`crate::evm::quantity`]. The two are kept apart on purpose: a value
    /// going into a `QUANTITY` field has to be spelled minimally to be
    /// spec-conformant, and a value going into a topic or a `DATA` field has
    /// to be full width, and there is no single form that is correct for
    /// both.
    pub fn to_word_hex(self) -> String {
        hex::encode_lower(&self.0)
    }

    /// Parses the full-width `DATA` spelling: a mandatory `0x` and exactly
    /// 64 hex digits of any case. For the minimal JSON-RPC `QUANTITY`
    /// spelling use [`crate::evm::quantity::parse_quantity_u256`].
    pub fn from_word_hex(s: &str) -> Result<EvmU256, EvmU256Error> {
        hex::decode_fixed::<WORD_BYTES>(s)
            .map(EvmU256)
            .map_err(EvmU256Error::from)
    }
}

impl From<u128> for EvmU256 {
    fn from(value: u128) -> EvmU256 {
        EvmU256::from_u128(value)
    }
}

impl From<u64> for EvmU256 {
    fn from(value: u64) -> EvmU256 {
        EvmU256::from_u64(value)
    }
}

impl TryFrom<EvmU256> for u128 {
    type Error = EvmU256Error;

    fn try_from(value: EvmU256) -> Result<u128, EvmU256Error> {
        value.try_to_u128()
    }
}

impl TryFrom<EvmU256> for u64 {
    type Error = EvmU256Error;

    fn try_from(value: EvmU256) -> Result<u64, EvmU256Error> {
        value.try_to_u64()
    }
}

impl fmt::Display for EvmU256 {
    /// The full-width word — see [`EvmU256::to_word_hex`]. Never
    /// abbreviated, so an error message identifies the offending word
    /// unambiguously and a reader can tell at a glance that it is a 256-bit
    /// value and not a small integer.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_word_hex())
    }
}

impl fmt::Debug for EvmU256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EvmU256({self})")
    }
}

#[cfg(test)]
mod tests;
