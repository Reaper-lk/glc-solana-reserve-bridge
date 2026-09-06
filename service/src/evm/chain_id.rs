//! [`EvmChainId`]: a raw EIP-155 chain id.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use super::quantity::{self, EvmQuantityError};
use super::u256::EvmU256;

/// A raw [EIP-155] chain id: the integer an EVM node returns from
/// `eth_chainId`, the integer a signed transaction commits to for replay
/// protection, and the `chainId` field of an EIP-712 domain.
///
/// [EIP-155]: https://eips.ethereum.org/EIPS/eip-155
///
/// # Representation
///
/// A [`NonZeroU64`]. Unsigned, so there is no negative chain id to reject at
/// runtime — the representation makes one unrepresentable. Non-zero for the
/// same reason: see [`EvmChainId::new`].
///
/// `u64` covers every chain id in existence by an enormous margin (the
/// largest in common use is around 10 digits). [EIP-2294] proposes a tighter
/// bound than `u64`, but is still a draft and its exact constant has moved;
/// pinning a specific ceiling from a non-final EIP would mean either
/// rejecting a chain that a node happily reports, or silently changing that
/// behaviour later. So the only bound enforced here is the one that is not
/// in dispute — a chain id is a positive integer — and the representation's
/// own `u64` ceiling.
///
/// [EIP-2294]: https://eips.ethereum.org/EIPS/eip-2294
///
/// # This is not the bridge's internal chain discriminant
///
/// See the [module-level docs][crate::evm] on the two different things
/// called a "chain id". This one is a wire value that distinguishes mainnet
/// from testnet; the ledger's `'goldcoin'`/`'solana'`/`'robinhood'`
/// discriminant is a database vocabulary that deliberately does not. They
/// must only ever be related by an explicit, tested mapping.
///
/// # Decimal or hex, never "whichever it looks like"
///
/// Chain ids appear in both spellings in the wild: decimal in
/// documentation, config files and human speech; hex `QUANTITY` in
/// JSON-RPC. `4663` and `0x4663` are **different chain ids** (4663 and
/// 18019), so a parser that accepted both spellings through one entry point
/// and guessed by the presence of a prefix would be one missing `0x` away
/// from signing for the wrong network. The two spellings therefore have two
/// separate, explicitly named functions — [`EvmChainId::from_str`] for
/// decimal and [`EvmChainId::from_quantity_hex`] for JSON-RPC — and each
/// rejects the other's form with a message saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvmChainId(NonZeroU64);

/// Why a value is not a usable EVM chain id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmChainIdError {
    /// Chain id 0 was offered. See [`EvmChainId::new`].
    #[error(
        "0 is not a usable EVM chain id: EIP-155 treats it as \"no replay protection\", and \
         a chain id is also how a signed authorisation is bound to one network"
    )]
    Zero,
    /// A decimal string was not a plain, canonical non-negative integer.
    #[error("{value:?} is not a decimal EVM chain id: {reason}")]
    NotDecimal { value: String, reason: &'static str },
    /// A hex `QUANTITY` string was malformed.
    #[error("invalid EVM chain id quantity: {0}")]
    Quantity(#[from] EvmQuantityError),
}

impl EvmChainId {
    /// Validates a chain id.
    ///
    /// The one rejected value is **zero**, and it is rejected on purpose
    /// rather than accepted as a "not configured" placeholder. EIP-155
    /// gives chain id 0 the specific meaning "no replay protection", and a
    /// chain id is the field that binds an EIP-712 authorisation to one
    /// network — an authorisation signed under chain id 0 is not scoped to
    /// any chain at all, and is exactly the sort of value that must not be
    /// reachable by forgetting to set a config field. Absence is spelled
    /// `Option<EvmChainId>`, never `EvmChainId(0)`.
    pub fn new(id: u64) -> Result<EvmChainId, EvmChainIdError> {
        NonZeroU64::new(id)
            .map(EvmChainId)
            .ok_or(EvmChainIdError::Zero)
    }

    /// Wraps an already-non-zero chain id. `const`, so a compile-time
    /// network constant needs no runtime check and no unwrap at startup —
    /// see [`crate::evm::networks`].
    pub const fn from_nonzero(id: NonZeroU64) -> EvmChainId {
        EvmChainId(id)
    }

    /// The chain id as a plain integer.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// The chain id as a 256-bit word, which is how EIP-712 encodes the
    /// `uint256 chainId` field of a domain.
    pub const fn to_u256(self) -> EvmU256 {
        EvmU256::from_u64(self.0.get())
    }

    /// The Ethereum JSON-RPC `QUANTITY` spelling — minimal-width hex, as
    /// `eth_chainId` returns it.
    pub fn to_quantity_hex(self) -> String {
        quantity::encode_quantity_u64(self.0.get())
    }

    /// Parses the JSON-RPC `QUANTITY` spelling. Rejects a decimal string:
    /// see the type docs on why the two spellings are separate entry
    /// points.
    pub fn from_quantity_hex(s: &str) -> Result<EvmChainId, EvmChainIdError> {
        let raw = quantity::parse_quantity_u64(s)?;
        EvmChainId::new(raw)
    }
}

impl FromStr for EvmChainId {
    type Err = EvmChainIdError;

    /// Parses the **decimal** spelling, strictly: ASCII digits only, at
    /// least one, no sign, no whitespace, no `0x`, no underscores.
    ///
    /// Leading zeros are rejected along with the rest of the non-canonical
    /// spellings. `0004663` denotes the same number, but accepting more than
    /// one spelling of a value that gets compared against a configured
    /// constant means the comparison can fail for a reason that is invisible
    /// in a diff.
    fn from_str(s: &str) -> Result<EvmChainId, EvmChainIdError> {
        let reject = |reason: &'static str| EvmChainIdError::NotDecimal {
            value: s.chars().take(32).collect(),
            reason,
        };

        if s.is_empty() {
            return Err(reject("it is empty"));
        }
        if s.starts_with("0x") || s.starts_with("0X") {
            return Err(reject(
                "it looks like a hex quantity; use EvmChainId::from_quantity_hex for that \
                 spelling, since 0x4663 and 4663 are different chain ids",
            ));
        }
        if !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(reject(
                "it contains a character that is not an ASCII digit (no sign, whitespace or \
                 separators are accepted)",
            ));
        }
        if s.len() > 1 && s.starts_with('0') {
            return Err(reject(
                "it has a leading zero, which is not the canonical spelling",
            ));
        }

        let raw = s
            .parse::<u64>()
            .map_err(|_| reject("it does not fit an unsigned 64-bit integer"))?;
        EvmChainId::new(raw)
    }
}

impl fmt::Display for EvmChainId {
    /// Decimal, which is how a chain id is written everywhere except a
    /// JSON-RPC payload. Round-trips through [`EvmChainId::from_str`]; the
    /// hex form round-trips through [`EvmChainId::from_quantity_hex`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.get())
    }
}

#[cfg(test)]
mod tests;
