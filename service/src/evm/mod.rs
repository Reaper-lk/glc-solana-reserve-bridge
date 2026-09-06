//! Generic EVM primitive types: addresses, fixed 32-byte hashes, chain
//! ids, the 256-bit ABI/RPC boundary word, Ethereum JSON-RPC hex
//! encoding, keccak-256, EIP-712 domain building blocks, compact ECDSA
//! signatures, and EVM log identity.
//!
//! # Why this module exists
//!
//! The bridge is being extended to span GLC L1 <-> Solana **and** GLC L1
//! <-> an EVM chain. Every EVM value that will eventually cross that
//! boundary — a contract address, a transaction hash, a log index, an
//! ERC-20 `uint256` — arrives as an untrusted string from a JSON-RPC
//! response or an operator's config file. Handling those as `String`,
//! `Vec<u8>` or bare integers is how a bridge ends up paying out to a
//! truncated address, folding a deposit twice because two hashes compared
//! equal after a case fold, or silently taking the low 128 bits of a
//! 256-bit word. This module is the one place those shapes are parsed,
//! and after parsing the shape is guaranteed by the type.
//!
//! Every type here follows the same three rules, which are the reason to
//! prefer them over the raw representation at every call site:
//!
//! 1. **Exact internal width.** An [`address::EvmAddress`] *is* 20 bytes
//!    and an [`hash::EvmTxHash`] *is* 32; there is no constructor that
//!    truncates or zero-pads to reach that width.
//! 2. **Strict parsing, no guessing.** The `0x` prefix is mandatory and
//!    must be exactly `0x`; the digit count must be exact; a value that
//!    does not parse cleanly is an error, never a value to guess at. This
//!    is the same discipline [`crate::api::atomic`] applies to monetary
//!    amounts on the wire, for the same reason.
//! 3. **Canonical, deterministic display that round-trips.** One value has
//!    exactly one textual form, and feeding that form back to the parser
//!    yields the same value. Proven per type in the tests.
//!
//! # Scope: pure functions, and now the cryptography they feed
//!
//! Nothing in this module opens a socket or builds an RPC client — that
//! is [`crate::robinhood::rpc`]'s job, and this module has no dependency
//! on it. Every function here is pure: given the same inputs it produces
//! the same bytes, and it reads no clock, no network and no database.
//!
//! What HAS changed since the types phase is that three of these modules
//! now do cryptography rather than only describing its shapes:
//!
//! - [`secp`] signs a digest, recovers an address from a signature, and
//!   enforces EIP-2's low-`s` rule — the verifier [`signature`]'s docs
//!   said the rule would land with.
//! - [`tx`] builds, signs and hashes the two EVM transaction envelopes.
//! - [`rlp`] and [`abi`] are the encodings those need.
//!
//! [`secp::EvmSecretKey`] is the only type in this module that can hold a
//! secret. It has no accessor that yields its bytes and its `Debug` is
//! redacted; every other type here is public data by construction.
//!
//! # Two different things called a "chain id"
//!
//! [`chain_id::EvmChainId`] is the **raw EIP-155 chain id** — the integer
//! an EVM node returns from `eth_chainId` and the integer that goes into
//! an EIP-712 domain. It identifies a network to the EVM, and mainnet and
//! testnet are two different values (see [`networks`]).
//!
//! It is **not** the bridge's own internal chain discriminant, the closed
//! `'goldcoin'`/`'solana'`/`'robinhood'` vocabulary the ledger stores to
//! namespace a request's source identity. That one deliberately is not
//! network-qualified (one ledger database belongs to exactly one
//! deployment on exactly one network) and is a database discriminant, not
//! a wire value. Nothing in this module produces or consumes it, and no
//! code should ever convert one to the other implicitly: the mapping
//! belongs in an explicit, tested function at the point the EVM indexer
//! folds an event into the ledger.
//!
//! # Dependencies
//!
//! One new direct dependency, `sha3`, for keccak-256 — needed by EIP-55
//! address checksums and by EIP-712 hashing. See [`keccak`] for why that
//! crate specifically, and why nothing here hand-rolls a hash. No
//! 256-bit arithmetic library is pulled in, because [`u256::EvmU256`]
//! performs no arithmetic at all — see its docs.

pub mod abi;
pub mod address;
pub mod chain_id;
pub mod eip712;
pub mod hash;
pub mod hex;
pub mod keccak;
pub mod log;
pub mod networks;
pub mod quantity;
pub mod rlp;
pub mod secp;
pub mod signature;
pub mod tx;
pub mod u256;

pub use abi::{AbiDecodeError, Calldata};
pub use address::{EvmAddress, EvmAddressError};
pub use chain_id::{EvmChainId, EvmChainIdError};
pub use eip712::Eip712Domain;
pub use hash::{EvmBlockHash, EvmHashError, EvmTxHash};
pub use hex::EvmHexError;
pub use keccak::keccak256;
pub use log::{EvmLogId, EvmLogLocation};
pub use quantity::EvmQuantityError;
pub use secp::{EvmSecpError, EvmSecretKey};
pub use signature::{EvmSignature, EvmSignatureError};
pub use tx::{SignedTransaction, TxEnvelope, TxFees, UnsignedTransaction};
pub use u256::{EvmU256, EvmU256Error};
