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
    /// The rate NEW `SolToGlc` requests price at, resolved by ROUTE from
    /// `[fees]` at config load (`crate::fees::RouteFees`).
    ///
    /// Held as a value this indexer was GIVEN rather than read from a
    /// constant, for the same reason `robinhood::Settler` holds its own:
    /// the rate belongs to this route, and a component that reached for a
    /// global would be one edit away from charging Solana's depositors
    /// Robinhood's price. Snapshotted onto each request at fold time and
    /// immutable thereafter, so an in-flight request keeps settling at
    /// the rate it was created under when this value changes.
    fee_bps: u64,
    /// The `SolToRhn` fold's inputs, present only when that route is
    /// PRICED (`[fees].SolToRhn`) — see [`SolanaIndexer::with_sol_to_rhn`].
    ///
    /// # What decides which route a Solana deposit is on
    ///
    /// The Solana program records no route: `deposit_to_reserve` stores
    /// an opaque destination payload and nothing else. The destination's
    /// SPELLING is what tells the two apart, and it does so structurally
    /// rather than heuristically: a Robinhood destination is the ASCII
    /// text `0x` followed by 40 hex digits, and `0` is not in the base58
    /// alphabet, so no Goldcoin address can ever begin with `0x`. The
    /// same argument [`crate::ledger::TransferAddressFilter`] already
    /// relies on. A payload that begins with `0x` but is not a valid EVM
    /// address (bad length, bad hex, failed EIP-55 checksum) is still a
    /// Robinhood-bound deposit — it is folded as `SolToRhn`, parked as
    /// undeliverable, and refundable on Solana — never a Goldcoin one.
    ///
    /// `None` means classification is OFF and every deposit folds as
    /// `SolToGlc`, exactly as before the route existed.
    sol_to_rhn: Option<SolToRhnFold>,
}

/// See [`SolanaIndexer::with_sol_to_rhn`].
pub struct SolToRhnFold {
    pub fee_bps: u64,
    pub route_gate: std::sync::Arc<crate::routes::RouteGate>,
}

/// Whether a Solana deposit's opaque destination payload names a
/// Robinhood (EVM) address rather than a Goldcoin one — by its `0x`
/// prefix alone, which no base58 string can carry. Says nothing about
/// whether the address is VALID; that is [`parse_robinhood_destination`]'s
/// job.
pub fn destination_is_robinhood(payload: &[u8]) -> bool {
    payload.starts_with(b"0x")
}

/// Parses a Robinhood-bound destination payload as the EVM address the
/// payout will be sent to: `0x` + 40 hex digits, EIP-55 checksum
/// honoured when the spelling carries one (`EvmAddress`'s own rule), and
/// never the zero address — the EVM burn sink, which `POST /transfers`
/// refuses for `GlcToRhn` for the same reason.
pub fn parse_robinhood_destination(
    payload: &[u8],
) -> Result<crate::evm::address::EvmAddress, String> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| "the destination payload is not valid UTF-8".to_string())?;
    let address = text
        .parse::<crate::evm::address::EvmAddress>()
        .map_err(|e| e.to_string())?;
    if address.is_zero() {
        return Err("the zero address is the EVM burn sink, not a payout destination".to_string());
    }
    Ok(address)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<R: SolanaRpc> SolanaIndexer<R> {
    pub fn new(rpc: R, ledger: Ledger, fee_bps: u64) -> Self {
        SolanaIndexer {
            rpc,
            ledger,
            fee_bps,
            sol_to_rhn: None,
        }
    }

    /// Turns on `SolToRhn` classification: a deposit whose destination
    /// begins with `0x` folds as `SolToRhn` at `fold.fee_bps`, payable
    /// only while `fold.route_gate` reports the route open. Without this,
    /// every deposit folds as `SolToGlc` (see the field docs).
    pub fn with_sol_to_rhn(mut self, fold: SolToRhnFold) -> Self {
        self.sol_to_rhn = Some(fold);
        self
    }

    /// The in-place form of [`SolanaIndexer::with_sol_to_rhn`], for a
    /// caller that has already handed this indexer to the orchestrator.
    pub fn set_sol_to_rhn(&mut self, fold: Option<SolToRhnFold>) {
        self.sol_to_rhn = fold;
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
        // The enablement verdict for `SolToRhn`, read once per tick and
        // applied to every Robinhood-bound deposit folded in it — the same
        // once-per-tick discipline the Robinhood settlement loop keeps.
        let sol_to_rhn_open = self.sol_to_rhn.as_ref().map(|f| {
            f.route_gate
                .is_enabled(&self.ledger, crate::routes::Route::SolToRhn)
        });
        for (index, maybe_account) in new_indices.iter().zip(fetched) {
            let account =
                maybe_account.ok_or(SolanaIndexerError::MissingObligationAccount(*index))?;
            let snap =
                decode_withdrawal_obligation(&account.data).map_err(SolanaIndexerError::Rpc)?;
            if let (Some(fold), Some(route_open)) = (&self.sol_to_rhn, sol_to_rhn_open) {
                if destination_is_robinhood(&snap.glc_address) {
                    self.fold_to_robinhood(&snap, fold.fee_bps, solana_decimals, route_open, now)?;
                    continue;
                }
            }
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
            // `SolToGlc`'s own configured rate — never the compiled-in
            // global, which is what this used to read.
            let fee_breakdown =
                crate::amount_conversion::compute_fee_at_bps(gross_canonical, self.fee_bps)
                    .map_err(|e| {
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

    /// Folds one Robinhood-bound obligation as `SolToRhn`.
    ///
    /// The amount path is the `SolToGlc` one up to the net: raw mint
    /// units widened exactly to canonical, the fee at THIS route's rate.
    /// The destination reserve (`RobinhoodReserve`) is accounted in
    /// canonical units, so `net_destination_atomic` is the canonical net;
    /// the 18-decimal widening the payout will perform is exercised here
    /// and discarded purely to prove deliverability before any capacity
    /// is held, exactly as `POST /transfers` does for `GlcToRhn`.
    ///
    /// An undeliverable destination (a `0x` payload that is not a valid
    /// EVM address, or the zero address — the EVM burn sink) folds
    /// PARKED rather than refused: the deposit is real and irreversible,
    /// and the park is what makes it visible and refundable.
    fn fold_to_robinhood(
        &mut self,
        snap: &accounts::WithdrawalObligationSnapshot,
        fee_bps: u64,
        solana_decimals: u8,
        route_open: bool,
        now: i64,
    ) -> Result<(), SolanaIndexerError> {
        let index = snap.index;
        let gross_canonical = crate::amount_conversion::SolanaAtomic(snap.amount)
            .to_canonical(solana_decimals)
            .map_err(|e| {
                SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                    "obligation {index}: {e}"
                )))
            })?;
        let fee_breakdown = crate::amount_conversion::compute_fee_at_bps(gross_canonical, fee_bps)
            .map_err(|e| {
                SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                    "obligation {index}: {e}"
                )))
            })?;
        // Always exact for any canonical amount; still checked rather
        // than assumed, and a failure here is a fold-time refusal to
        // hold capacity for a payout the settler would then refuse.
        fee_breakdown.net.to_robinhood().map_err(|e| {
            SolanaIndexerError::Rpc(SolanaRpcError::Malformed(format!(
                "obligation {index}: net entitlement is not representable at Robinhood's \
                 precision: {e}"
            )))
        })?;
        let amounts = crate::ledger::RequestAmounts {
            gross_atomic: fee_breakdown.gross.0,
            fee_bps: fee_breakdown.fee_bps,
            fee_atomic: fee_breakdown.fee.0,
            net_atomic: fee_breakdown.net.0,
            net_destination_atomic: fee_breakdown.net.0,
        };
        let recipient = match parse_robinhood_destination(&snap.glc_address) {
            Ok(address) => Ok(address),
            Err(detail) => Err(format!("undeliverable destination: {detail}")),
        };
        self.ledger.fold_sol_deposit_to_robinhood(
            index,
            amounts,
            snap.requester.to_bytes(),
            recipient.as_ref().ok().map(|a| a.to_bytes()),
            &snap.glc_address,
            route_open,
            recipient.as_ref().err().map(String::as_str),
            now,
        )?;
        Ok(())
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
