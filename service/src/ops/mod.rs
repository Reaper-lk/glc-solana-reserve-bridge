//! Operational tooling: metrics, health, and reserve-invariant checks
//! (docs/07-implementation-plan.md Phase 5). Reused near-verbatim from the
//! old bridge's `ops/` module where the code is pure chain/ops mechanics
//! (docs/01-reuse-inventory.md), rewritten where it encoded a mint/burn or
//! federation-specific assumption.

pub mod alerting;
pub mod audit;
pub mod collector;
pub mod health;
pub mod indexer_status;
pub mod metrics;
pub mod reserve_health;
