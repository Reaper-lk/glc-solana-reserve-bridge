//! [`EvmTxHash`] and [`EvmBlockHash`]: the EVM's two 32-byte identifiers.
//!
//! # Why two types for one representation
//!
//! A transaction hash and a block hash are both `bytes32` and are
//! indistinguishable by inspection, which is precisely the problem. The
//! EVM code this bridge will grow needs to hold both at once — a log's
//! identity is its transaction hash, and the reorg check that decides
//! whether that log is still canonical is against its *block* hash — and a
//! function that took `[u8; 32]` for both would accept them in either
//! order, forever, with no diagnostic. Two distinct newtypes make that
//! swap a compile error.
//!
//! The types therefore deliberately provide **no** conversion between each
//! other: no `From`, no `as_tx_hash()`, nothing. Their bytes are reachable
//! only through [`EvmTxHash::to_bytes`]/[`EvmBlockHash::to_bytes`], so
//! going from one to the other is possible but has to be written out, and
//! is then visible in review.
//!
//! # Parsing rules
//!
//! Identical for both, and the same strictness as
//! [`crate::evm::address`]: a mandatory, exactly-`0x` prefix and exactly
//! 64 hex digits. Unlike an address there is **no** checksum scheme for a
//! hash, so digit case carries no information: any case is accepted on
//! input, and lowercase is what is emitted.

use std::fmt;
use std::str::FromStr;

use super::hex::{self, EvmHexError};

/// The number of bytes in an EVM hash. Fixed by the EVM.
pub const HASH_BYTES: usize = 32;

/// The number of hex digits in the textual form of an EVM hash.
pub const HASH_HEX_DIGITS: usize = HASH_BYTES * 2;

/// Why a string or byte sequence is not a valid 32-byte EVM hash.
///
/// One error type shared by both hash types: the failure modes are
/// identical, and the message names which kind of value failed via the
/// `kind` field rather than through a second, duplicate enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmHashError {
    /// The `0x`-prefixed 64-hex-digit shape itself is wrong.
    #[error("invalid EVM {kind} hash: {source}")]
    Hex {
        kind: &'static str,
        #[source]
        source: EvmHexError,
    },
    /// A byte sequence of the wrong width was offered. Never truncated and
    /// never zero-padded to 32 bytes.
    #[error("an EVM {kind} hash is exactly 32 bytes, got {actual}")]
    WrongByteLength { kind: &'static str, actual: usize },
}

/// Defines one 32-byte hash newtype. Both types are generated from this so
/// that their parsing, display and byte handling cannot drift apart while
/// remaining, as the module docs require, mutually unconvertible.
macro_rules! evm_hash32 {
    ($name:ident, $kind:literal, $doc:literal) => {
        #[doc = $doc]
        #[doc = ""]
        #[doc = "Exactly 32 bytes. Parses from a mandatory-`0x`, exactly-64-hex-digit"]
        #[doc = "string of any digit case; displays as `0x` + 64 lowercase digits, which"]
        #[doc = "round-trips. See the module docs for why this is its own type rather"]
        #[doc = "than a `[u8; 32]`."]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; HASH_BYTES]);

        impl $name {
            /// The all-zero hash.
            ///
            /// A real, representable value, not a "none": the EVM uses it
            /// as the parent hash of the genesis block and JSON-RPC uses it
            /// for a pending block's hash. A caller that must exclude it
            /// checks explicitly.
            pub const ZERO: $name = $name([0u8; HASH_BYTES]);

            /// The kind of hash this is, for error messages.
            pub const KIND: &'static str = $kind;

            /// Wraps 32 bytes that are already this kind of hash.
            pub const fn from_bytes(bytes: [u8; HASH_BYTES]) -> $name {
                $name(bytes)
            }

            /// Wraps a slice that must be **exactly** 32 bytes long.
            pub fn try_from_slice(bytes: &[u8]) -> Result<$name, EvmHashError> {
                <[u8; HASH_BYTES]>::try_from(bytes).map($name).map_err(|_| {
                    EvmHashError::WrongByteLength {
                        kind: $kind,
                        actual: bytes.len(),
                    }
                })
            }

            /// The 32 hash bytes.
            pub const fn as_bytes(&self) -> &[u8; HASH_BYTES] {
                &self.0
            }

            /// The 32 hash bytes, by value.
            pub const fn to_bytes(self) -> [u8; HASH_BYTES] {
                self.0
            }

            /// Whether this is the all-zero hash. See [`Self::ZERO`].
            pub fn is_zero(&self) -> bool {
                self.0 == [0u8; HASH_BYTES]
            }
        }

        impl FromStr for $name {
            type Err = EvmHashError;

            fn from_str(s: &str) -> Result<$name, EvmHashError> {
                hex::decode_fixed::<HASH_BYTES>(s)
                    .map($name)
                    .map_err(|source| EvmHashError::Hex {
                        kind: $kind,
                        source,
                    })
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode_lower(&self.0))
            }
        }

        impl fmt::Debug for $name {
            /// Hand-written for the same reason as
            /// [`crate::evm::EvmAddress`]'s: 32 decimal integers are
            /// unreadable in a test failure or a log line.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }
    };
}

evm_hash32!(
    EvmTxHash,
    "transaction",
    "An EVM transaction hash: `keccak256` of the signed transaction."
);

evm_hash32!(
    EvmBlockHash,
    "block",
    "An EVM block hash: `keccak256` of the block header. Distinct from \
     [`EvmTxHash`] by type even though both are 32 bytes — it answers a \
     different question (which chain history a log is on, not which \
     transaction produced it)."
);

#[cfg(test)]
mod tests;
