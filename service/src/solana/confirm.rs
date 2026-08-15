//! Bounded transaction confirmation. Reused design from the old bridge's
//! `solana::confirm` (docs/01-reuse-inventory.md, ADR-0030): acceptance by
//! an RPC node is not inclusion — a transaction can be accepted and never
//! land (blockhash expiry, dropped slot, skipped leader). Reporting success
//! on acceptance alone is exactly the defect ADR-0030 exists to close (the
//! old bridge's `glc-admin pause` once did this, and an operator believing
//! a pause had taken effect when it hadn't is a directional wrong-way
//! failure for a circuit breaker).
//!
//! # Why polling, and why it terminates
//!
//! There is no push notification for "this transaction landed". What makes
//! the wait *bounded* is not the clock but [`SolanaRpc::is_blockhash_valid`]:
//! once a transaction's blockhash can no longer land, that transaction can
//! never confirm, so waiting longer is pointless and the failure is
//! reported immediately. The deadline is only a backstop for an RPC that
//! never answers.

use std::time::Duration;

use solana_sdk::hash::Hash;
use solana_sdk::signature::Signature;

use super::rpc::{SolanaRpc, SolanaRpcError};

#[derive(Debug, Clone, Copy)]
pub struct ConfirmPolicy {
    pub deadline: Duration,
    pub poll_interval: Duration,
}

impl Default for ConfirmPolicy {
    fn default() -> Self {
        ConfirmPolicy {
            deadline: Duration::from_secs(90),
            poll_interval: Duration::from_millis(500),
        }
    }
}

/// Why a transaction could not be confirmed. Kept distinct because an
/// operator's/orchestrator's next move differs for each: a rejection is a
/// bug or stale precondition (re-running fails the same way); an expiry is
/// safe to retry immediately (idempotent rebuild — nothing it would have
/// done has happened); a timeout leaves the outcome genuinely UNKNOWN and
/// the on-chain postcondition must be read back before anything is retried
/// (never assume either way).
#[derive(Debug, thiserror::Error)]
pub enum ConfirmFailure {
    #[error("transaction {signature} was rejected on chain: {reason}")]
    Rejected {
        signature: Signature,
        reason: String,
    },
    #[error("transaction {signature} expired before it confirmed; nothing it would have done has taken effect")]
    Expired { signature: Signature },
    #[error("transaction {signature} was neither confirmed nor expired within {waited:?}; its outcome is UNKNOWN — read on-chain state back before retrying")]
    TimedOut {
        signature: Signature,
        waited: Duration,
    },
    #[error("could not determine the fate of transaction {signature}: {source}")]
    Rpc {
        signature: Signature,
        #[source]
        source: SolanaRpcError,
    },
}

/// Waits until `signature` has demonstrably confirmed at `finalized`
/// commitment, demonstrably failed, or demonstrably can never land. Never
/// returns `Ok` on a transaction it has not actually observed succeed.
pub async fn confirm_transaction<R: SolanaRpc>(
    rpc: &R,
    signature: &Signature,
    blockhash: &Hash,
    policy: ConfirmPolicy,
) -> Result<(), ConfirmFailure> {
    let started = std::time::Instant::now();
    loop {
        match rpc.get_signature_status(signature).await {
            Ok(Some(Ok(()))) => return Ok(()),
            Ok(Some(Err(reason))) => {
                return Err(ConfirmFailure::Rejected {
                    signature: *signature,
                    reason,
                })
            }
            Ok(None) => {}
            Err(source) => {
                return Err(ConfirmFailure::Rpc {
                    signature: *signature,
                    source,
                })
            }
        }

        let still_landable =
            rpc.is_blockhash_valid(blockhash)
                .await
                .map_err(|source| ConfirmFailure::Rpc {
                    signature: *signature,
                    source,
                })?;
        if !still_landable {
            // The blockhash read happens after the status read, so the
            // transaction may have confirmed in between — ask once more
            // before declaring expiry, so a completed action is never
            // reported as a failure.
            return match rpc.get_signature_status(signature).await {
                Ok(Some(Ok(()))) => Ok(()),
                Ok(Some(Err(reason))) => Err(ConfirmFailure::Rejected {
                    signature: *signature,
                    reason,
                }),
                Ok(None) => Err(ConfirmFailure::Expired {
                    signature: *signature,
                }),
                Err(source) => Err(ConfirmFailure::Rpc {
                    signature: *signature,
                    source,
                }),
            };
        }

        let waited = started.elapsed();
        if waited >= policy.deadline {
            return Err(ConfirmFailure::TimedOut {
                signature: *signature,
                waited,
            });
        }
        tokio::time::sleep(policy.poll_interval).await;
    }
}

#[cfg(test)]
mod tests;
