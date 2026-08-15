//! Solana-side indexer: watches `BridgeConfig.obligation_count` at
//! `finalized` commitment and folds newly observed `WithdrawalObligation`
//! accounts into the ledger (docs/03-architecture.md, docs/06-schema.md).
//!
//! # Why this doesn't scan transaction history
//!
//! A `WithdrawalObligation` PDA's address is fully determined by its index
//! (`programs/glc-reserve-bridge/src/constants.rs`'s
//! `SEED_WITHDRAWAL_OBLIGATION` seed), and `BridgeConfig.obligation_count`
//! is the authoritative count of how many exist. So new deposits are
//! discovered by comparing the live count against
//! `Ledger::last_synced_obligation_count` and directly fetching the
//! resulting PDA range — no `getSignaturesForAddress`/`getTransaction`
//! parsing needed. This is simpler, cheaper, and (unlike history scanning)
//! has no pagination/ordering edge cases to get wrong.
//!
//! # Fail-closed behavior
//!
//! - `obligation_count` observed lower than what was already synced is
//!   treated as a hard error, never as "nothing changed" — finalized
//!   commitment is supposed to be monotonic, so this can only mean stale
//!   RPC state, an unexpected redeploy, or a misconfigured endpoint; the
//!   caller must not proceed.
//! - If the account for an index inside `[last_synced, count)` is missing
//!   (RPC returned `None` where the config says it must exist), the sync
//!   cursor is NOT advanced past it — the tick errors and the same range is
//!   retried next tick, rather than silently skipping a real deposit.

use thiserror::Error;

use crate::ledger::{Ledger, LedgerError};

use super::accounts::{self, decode_bridge_config, decode_withdrawal_obligation};
use super::rpc::{SolanaRpc, SolanaRpcError};

#[derive(Debug, Error)]
pub enum SolanaIndexerError {
    #[error("Solana node unavailable: {0}")]
    NodeUnavailable(SolanaRpcError),
    #[error("Solana RPC error: {0}")]
    Rpc(SolanaRpcError),
    #[error(
        "bridge_config account does not exist at {0} — bridge not initialized on this cluster"
    )]
    NotInitialized(solana_sdk::pubkey::Pubkey),
    #[error(
        "observed obligation_count {observed} is LESS than last synced {last_synced} — finalized \
         commitment must be monotonic; refusing to proceed on inconsistent chain state"
    )]
    StaleOrInconsistentChainState { last_synced: u64, observed: u64 },
    #[error("obligation account at index {0} is missing though bridge_config reports it exists")]
    MissingObligationAccount(u64),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaTickOutcome {
    NoNewObligations,
    Folded { count: u64 },
}

const INNER_RETRY_ATTEMPTS: u32 = 3;

pub struct SolanaIndexer<R: SolanaRpc> {
    rpc: R,
    ledger: Ledger,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<R: SolanaRpc> SolanaIndexer<R> {
    pub fn new(rpc: R, ledger: Ledger) -> Self {
        SolanaIndexer { rpc, ledger }
    }

    async fn call<T, F, Fut>(f: F) -> Result<T, SolanaIndexerError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, SolanaRpcError>>,
    {
        super::rpc::call_with_retry(INNER_RETRY_ATTEMPTS, f)
            .await
            .map_err(|e| {
                if e.is_retriable() {
                    SolanaIndexerError::NodeUnavailable(e)
                } else {
                    SolanaIndexerError::Rpc(e)
                }
            })
    }

    pub async fn tick(&mut self) -> Result<SolanaTickOutcome, SolanaIndexerError> {
        let slot = Self::call(|| self.rpc.get_slot()).await?;

        let config_pda = accounts::bridge_config_pda();
        let account = Self::call(|| self.rpc.get_account(&config_pda))
            .await?
            .ok_or(SolanaIndexerError::NotInitialized(config_pda))?;
        let config = decode_bridge_config(&account.data).map_err(SolanaIndexerError::Rpc)?;

        let last_synced = self.ledger.last_synced_obligation_count()?;
        if config.obligation_count < last_synced {
            return Err(SolanaIndexerError::StaleOrInconsistentChainState {
                last_synced,
                observed: config.obligation_count,
            });
        }
        if config.obligation_count == last_synced {
            self.ledger
                .set_last_synced_obligation_count(last_synced, slot, now_unix())?;
            return Ok(SolanaTickOutcome::NoNewObligations);
        }

        let new_indices: Vec<u64> = (last_synced..config.obligation_count).collect();
        let pdas: Vec<_> = new_indices
            .iter()
            .map(|i| accounts::withdrawal_obligation_pda(*i))
            .collect();
        let fetched = Self::call(|| self.rpc.get_multiple_accounts(&pdas)).await?;

        // Read once per tick, not per obligation — decimals are immutable
        // post-`InitializeMint`, and every obligation folded this tick
        // shares the same reserve mint (docs/20-bridge-fee.md).
        let solana_decimals = Self::call(|| {
            accounts::fetch_reserve_mint_decimals(&self.rpc, &config.reserve_token_mint)
        })
        .await?;

        let now = now_unix();
        for (index, maybe_account) in new_indices.iter().zip(fetched) {
            let account =
                maybe_account.ok_or(SolanaIndexerError::MissingObligationAccount(*index))?;
            let snap =
                decode_withdrawal_obligation(&account.data).map_err(SolanaIndexerError::Rpc)?;
            // `snap.amount` is the raw on-chain obligation's GROSS amount,
            // in the reserve mint's own live decimals. Widening to
            // canonical is always exact; the destination for SolToGlc is
            // Goldcoin, whose native unit already IS canonical, so no
            // further conversion is needed for `net_destination_atomic`
            // (docs/20-bridge-fee.md).
            let gross_canonical = crate::amount_conversion::SolanaAtomic(snap.amount)
                .to_canonical(solana_decimals)
                .map_err(|e| {
                    SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                        "obligation {index}: {e}"
                    )))
                })?;
            let fee_breakdown =
                crate::amount_conversion::compute_fee(gross_canonical).map_err(|e| {
                    SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                        "obligation {index}: {e}"
                    )))
                })?;
            let amounts = crate::ledger::RequestAmounts {
                gross_atomic: fee_breakdown.gross.0,
                fee_bps: fee_breakdown.fee_bps,
                fee_atomic: fee_breakdown.fee.0,
                net_atomic: fee_breakdown.net.0,
                net_destination_atomic: fee_breakdown.net.0,
            };
            self.ledger.fold_sol_deposit(
                snap.index,
                amounts,
                snap.requester.to_bytes(),
                &snap.glc_address,
                now,
            )?;
        }

        self.ledger
            .set_last_synced_obligation_count(config.obligation_count, slot, now)?;
        Ok(SolanaTickOutcome::Folded {
            count: new_indices.len() as u64,
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }
}

#[cfg(test)]
mod tests;
