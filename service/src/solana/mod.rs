//! Solana-side chain plumbing: RPC client (pinned to `finalized`
//! commitment), account decoding, and the obligation-count-driven indexer
//! (docs/03-architecture.md).

pub mod accounts;
pub mod indexer;
pub mod rpc;
