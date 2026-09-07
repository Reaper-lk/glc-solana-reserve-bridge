//! [`EvmLogId`] and [`EvmLogLocation`]: the identity and the chain position
//! of an EVM log.
//!
//! # Why a log needs a typed identity
//!
//! An EVM deposit is observed as a log. Deciding whether a log has already
//! been folded into the ledger — the replay guard on the whole inbound leg —
//! comes down to comparing that log's identity against what has been seen
//! before, and the identity is *not* the transaction hash alone: one
//! transaction can emit the same event many times, and each occurrence is a
//! separate deposit. It is the pair `(transaction hash, log index)`.
//!
//! Carried as a `(String, u64)`, or as two loose parameters, that pair is
//! one argument transposition away from treating two different deposits as
//! the same one, or the same deposit as two. As a type it is a single value
//! with `Eq`, `Ord` and `Hash`, which is what a dedup set or a unique index
//! actually wants.
//!
//! # Identity versus position
//!
//! The two are deliberately separate types:
//!
//! - [`EvmLogId`] is the log's **identity**: what makes it that log and not
//!   another. Transaction hash and log index, nothing else. Two identical
//!   ids are the same log.
//! - [`EvmLogLocation`] is where that log currently **sits in a chain
//!   history**: additionally the block number and the block hash. Those are
//!   what a reorg check needs, and they are exactly the parts that can
//!   change without the log's identity changing — a reorg can move a
//!   transaction to a different block, or remove it entirely.
//!
//! Conflating them is how a reorg-aware indexer ends up keyed on a value
//! that a reorg mutates.
//!
//! # Not persisted in this phase
//!
//! Nothing here is written to the ledger, and no schema knows about it. The
//! ledger's source-identity columns and their replay guard are a separate
//! change with a migration of its own; this is the in-memory type that
//! change is expected to be built on top of. [`fmt::Display`] is for logs
//! and error messages, not a storage format — a durable encoding should be
//! chosen deliberately, with the schema, not inherited by accident from
//! whatever `Display` happened to produce.

use std::fmt;

use super::hash::{EvmBlockHash, EvmTxHash};

/// The identity of a single EVM log: which transaction emitted it, and which
/// of that transaction's logs it is.
///
/// # Why `log_index` is a `u64`
///
/// A receipt's log index is a `uint256` in the ABI, and Ethereum JSON-RPC
/// returns it as a hex `QUANTITY` of unbounded width. In reality it is
/// bounded by the number of logs in a block, which is bounded by the block
/// gas limit and is a small number — no chain has ever produced a log index
/// that needed more than a handful of digits, and one that overflowed a `u64`
/// would require a block roughly 10^19 logs long.
///
/// So a `u64` is the honest representation, and the narrowing happens **at
/// the boundary, explicitly**: a caller decoding a JSON-RPC response parses
/// the quantity into an [`crate::evm::EvmU256`] and narrows it with
/// [`crate::evm::EvmU256::try_to_u64`], which errors rather than truncating.
/// What is refused there is not a real log — it is a malformed or hostile
/// response — and refusing it is the correct outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvmLogId {
    /// The transaction that emitted the log.
    pub tx_hash: EvmTxHash,
    /// The log's index within its **block** (not within its transaction) —
    /// which is what `eth_getLogs` reports, and what makes the pair unique.
    pub log_index: u64,
}

impl EvmLogId {
    /// Builds a log identity.
    pub const fn new(tx_hash: EvmTxHash, log_index: u64) -> EvmLogId {
        EvmLogId { tx_hash, log_index }
    }
}

impl fmt::Display for EvmLogId {
    /// `<tx hash>#<log index>` — compact, unambiguous, and greppable against
    /// a block explorer. See the module docs: this is a diagnostic form, not
    /// a storage format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.tx_hash, self.log_index)
    }
}

/// Where a log sits in a chain history: its identity plus the block that
/// currently contains it.
///
/// The block hash is carried alongside the block number on purpose. A block
/// *number* is not a stable reference — after a reorg, the same height holds
/// a different block, possibly without this log at all — whereas the block
/// hash names one specific block for all time. Holding both is what lets an
/// observer ask the question that matters ("is the block I saw this log in
/// still canonical?") rather than the one that merely looks similar ("is
/// there still something at that height?").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvmLogLocation {
    /// The log's identity, stable across reorgs.
    pub id: EvmLogId,
    /// The height of the block that contained it when it was observed.
    pub block_number: u64,
    /// The hash of that specific block — the reorg-detecting half.
    pub block_hash: EvmBlockHash,
}

impl EvmLogLocation {
    /// Builds a log location.
    pub const fn new(id: EvmLogId, block_number: u64, block_hash: EvmBlockHash) -> EvmLogLocation {
        EvmLogLocation {
            id,
            block_number,
            block_hash,
        }
    }

    /// The log's identity.
    pub const fn id(&self) -> EvmLogId {
        self.id
    }

    /// The key to sort observed logs into canonical chain order:
    /// `(block number, log index)`.
    ///
    /// Provided as an explicit method rather than an `Ord` implementation,
    /// deliberately. Two locations from *different* blocks can share a
    /// `(block number, log index)` pair — that is exactly what a reorg
    /// produces — so an `Ord` built on this key would report `Equal` for two
    /// values that are not `Eq`, breaking the contract between the two
    /// traits and, with it, every `BTreeMap` and `sort_by` that relied on
    /// it. Sorting is a thing a caller asks for; ordering is not a property
    /// this type has.
    pub const fn chain_order_key(&self) -> (u64, u64) {
        (self.block_number, self.id.log_index)
    }
}

impl fmt::Display for EvmLogLocation {
    /// `<tx hash>#<log index> in block <number> (<block hash>)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} in block {} ({})",
            self.id, self.block_number, self.block_hash
        )
    }
}

#[cfg(test)]
mod tests;
