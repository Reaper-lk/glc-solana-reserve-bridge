//! Off-chain bridge service: Goldcoin/Solana chain plumbing, the reserve
//! ledger, reconciliation, Goldcoin vault/payout construction, the
//! internal ed25519/secp256k1 threshold signer groups, the orchestrator
//! tick loop tying them all together, operator tooling
//! (`ops`/`glc-admin`/`glc-audit`), production config loading
//! (docs/03-architecture.md, docs/07-implementation-plan.md), and the
//! read-only Robinhood Network deposit indexer (`robinhood`), which
//! observes a chain the bridge does not yet transact on.

pub mod admin_api;
pub mod amount_conversion;
pub mod api;
pub mod chain_policy;
pub mod chains;
pub mod config;
pub mod daemon;
pub mod evm;
pub mod fees;
pub mod goldcoin;
pub mod ledger;
pub mod ops;
pub mod orchestrator;
pub mod quota;
pub mod rebalance;
pub mod reconciliation;
pub mod robinhood;
pub mod routes;
pub mod signing;
pub mod solana;
