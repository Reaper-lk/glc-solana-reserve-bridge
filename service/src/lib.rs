//! Off-chain bridge service: Goldcoin/Solana chain plumbing, the reserve
//! ledger, reconciliation, Goldcoin vault/payout construction, the
//! internal ed25519/secp256k1 threshold signer groups, the orchestrator
//! tick loop tying them all together, operator tooling
//! (`ops`/`glc-admin`/`glc-audit`), and production config loading
//! (docs/03-architecture.md, docs/07-implementation-plan.md).

pub mod amount_conversion;
pub mod api;
pub mod config;
pub mod daemon;
pub mod goldcoin;
pub mod ledger;
pub mod ops;
pub mod orchestrator;
pub mod rebalance;
pub mod reconciliation;
pub mod signing;
pub mod solana;
