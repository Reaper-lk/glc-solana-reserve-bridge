//! Off-chain bridge service: Goldcoin/Solana chain plumbing, the reserve
//! ledger, and reconciliation (docs/03-architecture.md,
//! docs/07-implementation-plan.md Phase 0/1).
//!
//! This crate does not yet include: attestation signing clients, the
//! settlement orchestrator, Goldcoin vault/payout construction, or
//! operator tooling (CLI/health/metrics endpoints) — those are later
//! phases (see IMPLEMENTATION_LOG.md).

pub mod goldcoin;
pub mod ledger;
pub mod reconciliation;
pub mod solana;
