//! Goldcoin-side chain plumbing: RPC client, indexer, deposit extraction,
//! address/multisig-vault codec, coin selection, and payout construction
//! (docs/03-architecture.md).

pub mod address;
pub mod coin;
pub mod deposit;
pub mod derivation;
pub mod hex;
pub mod indexer;
pub mod liquidity;
pub mod multisig;
pub mod payout;
pub mod payout_recovery;
pub mod refund;
pub mod rpc;
pub mod split;
pub mod tx;
pub mod vault;
