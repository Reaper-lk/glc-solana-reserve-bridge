//! Off-chain bridge service: Goldcoin/Solana chain plumbing, the reserve
//! ledger, reconciliation, Goldcoin vault/payout construction, the
//! internal ed25519/secp256k1 threshold signer groups, and the
//! orchestrator tick loop tying them all together
//! (docs/03-architecture.md, docs/07-implementation-plan.md Phase 0/1
//! through Phase 4).
//!
//! This crate does not yet include operator tooling (CLI/health/metrics
//! endpoints) or a long-running process entrypoint driving
//! [`orchestrator::Orchestrator::tick`] on an interval — those are later
//! phases (see IMPLEMENTATION_LOG.md).

pub mod goldcoin;
pub mod ledger;
pub mod ops;
pub mod orchestrator;
pub mod reconciliation;
pub mod signing;
pub mod solana;
