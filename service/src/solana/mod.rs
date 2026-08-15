//! Solana-side chain plumbing: RPC client (pinned to `finalized`
//! commitment), account decoding, instruction encoding, attestation-proof
//! building, and bounded transaction confirmation (docs/03-architecture.md).

pub mod accounts;
pub mod confirm;
pub mod ed25519;
pub mod indexer;
pub mod instructions;
pub mod rpc;
