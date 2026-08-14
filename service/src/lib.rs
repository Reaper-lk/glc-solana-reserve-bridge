//! Off-chain bridge service: Goldcoin/Solana chain plumbing, the reserve
//! ledger, reconciliation, and Goldcoin vault/payout construction +
//! internal-custody signing (docs/03-architecture.md,
//! docs/07-implementation-plan.md Phase 0/1 and Phase 3).
//!
//! This crate does not yet include: the settlement orchestrator tying
//! indexers/ledger/signing into a continuous service loop, or operator
//! tooling (CLI/health/metrics endpoints) — those are later phases (see
//! IMPLEMENTATION_LOG.md).

pub mod goldcoin;
pub mod ledger;
pub mod reconciliation;
pub mod signing;
pub mod solana;
