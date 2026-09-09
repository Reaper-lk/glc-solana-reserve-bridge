//! Robinhood reserve reconciliation: the live `balanceOf(bridge)` read
//! that turns `RobinhoodReserve` from a row seeded at zero into a row
//! backed by an actually-observed chain balance.
//!
//! # The gap this closes
//!
//! `crate::bin::glc_bridge_daemon` calls `Ledger::configure_reserve(...,
//! initial_balance: 0, ...)` for every direction, and that zero is only
//! ever consulted the first time a direction is configured — the comment
//! there says "reconciliation's very next tick overwrites it with a real
//! observed balance regardless". That was true for `GoldcoinReserve` and
//! `SolanaReserve`, which
//! [`crate::orchestrator::Orchestrator::tick_goldcoin_reconciliation`]
//! and [`crate::orchestrator::Orchestrator::tick_solana_reconciliation`]
//! drive every tick. It was NOT true for `RobinhoodReserve`: nothing
//! ever read the Robinhood chain for a balance, so the seeded `0` stood
//! forever and `glc-admin robinhood-status` reported
//!
//! ```text
//! balance          0
//! protected min    2000000000000
//! available        -2000000000000
//! invariant holds  false
//! ```
//!
//! on a bridge contract holding 1,000,100 GLC. That is not a cosmetic
//! reporting bug. `available` and `invariant holds` are the figures
//! admission and settlement gate on, so a reserve that is in fact fully
//! funded reads as catastrophically insolvent.
//!
//! # What this module is allowed to do
//!
//! It READS. Its RPC bound is [`EvmRpc`] + [`EvmCallRpc`] — chain id,
//! block number, and `eth_call`. [`EvmSubmitRpc`] is deliberately absent
//! from the bound, so no instantiation of [`ReserveReconciler`] can
//! broadcast a transaction, and that is a property of the type rather
//! than of a flag.
//!
//! It opens no route. It settles nothing. It holds no key. Its single
//! write is the one every reserve direction already performs:
//! [`crate::reconciliation::reconcile`], which owns the balance refresh,
//! the solvency invariant, the unexplained-drop check, the audit finding
//! and the auto-pause. Nothing here writes SQLite directly and nothing
//! here re-implements any part of that policy.
//!
//! # Why it does not require `[robinhood.settlement]`
//!
//! Reading a balance needs a token address, a holder address and an
//! endpoint. All three come from `[robinhood.indexer]`
//! ([`RobinhoodIndexerConfig::expected_token`],
//! [`RobinhoodIndexerConfig::bridge_contract`],
//! [`RobinhoodIndexerConfig::rpc_url`]). None of them comes from
//! `[robinhood.settlement]`, which exists to authorize and broadcast —
//! capabilities a read does not need and must not acquire in order to
//! happen.
//!
//! This matters concretely for the deployment this was written for:
//! production runs the indexer with no `[robinhood.settlement]` section
//! at all. Requiring settlement config to observe a balance would have
//! meant the only way to make the reserve report the truth was to hand
//! the process a submitter key and an authorization quorum — i.e. the
//! read would have been paid for with the ability to move funds. It is
//! also why this is its OWN loop rather than a phase of
//! [`super::daemon::run_settlement`], whose bound includes
//! [`EvmSubmitRpc`] and whose existence is conditional on a verified
//! settlement deployment.
//!
//! `glc-admin robinhood-status` reads the on-chain half through
//! `[robinhood.settlement].bridge_contract`; this uses
//! `[robinhood.indexer].bridge_contract`. `crate::config` already
//! requires the two to be equal when both sections are present, so they
//! never disagree — and only the indexer's copy is guaranteed to exist.
//!
//! # Fail-closed, in every direction that can fail
//!
//! [`crate::reconciliation::reconcile`] takes an already-observed `u64`
//! and has no "unknown" input by design. Every step below that could
//! fail to produce a real number therefore ends in
//! [`crate::reconciliation::record_skipped`] and returns — the cached
//! balance is left exactly as it was, the skip is auditable, and no
//! value is invented:
//!
//! - the endpoint is unreachable, or times out;
//! - `eth_chainId` disagrees with the configured chain (checked EVERY
//!   tick, not once at startup, for the reason
//!   [`RobinhoodIndexerConfig::chain_id`] records: an endpoint can be
//!   repointed underneath a running process);
//! - the chain has not yet produced `confirmation_depth` blocks;
//! - the token's live `decimals()` is not 18
//!   ([`ensure_robinhood_decimals`] — a differently-scaled token is not
//!   this asset, and its balance is not this reserve's balance);
//! - the returned word exceeds `u128`, or scales down past `u64`.
//!
//! A breach found by a read that DID succeed pauses through
//! `reconcile`'s existing path, and nothing here ever calls
//! `set_paused(.., false, ..)`: un-pausing stays operator-controlled,
//! exactly as the reconciliation module's docs require.
//!
//! # The balance is read at confirmation depth, not at the head
//!
//! `balanceOf` is evaluated at `head + 1 - confirmation_depth` rather
//! than `latest`, using the same depth and the same `head - block + 1`
//! arithmetic the deposit indexer uses. A head-block balance can be
//! reorged away; reconciling against one would let a reorg present as an
//! unexplained drop and auto-pause a reserve that never lost anything.
//!
//! # Sub-canonical dust
//!
//! The ledger is 8-decimal and the token is 18-decimal, so an observed
//! balance need not be exactly representable. See
//! [`RobinhoodAtomic::to_canonical_floor`] for why an observation floors
//! (and alarms) where a claim would refuse: the ERC-20 is
//! permissionless, so refusing would let anyone freeze reserve
//! reconciliation forever with a one-wei transfer, and the floor is a
//! lower bound whose error can only ever cause a pause, never mask one.

use crate::amount_conversion::robinhood::{ensure_robinhood_decimals, RobinhoodAtomic};
use crate::ledger::{Ledger, LedgerError, ReserveDirection};
use crate::reconciliation::{self, ReconciliationReport};

use super::calls::TokenReader;
use super::config::RobinhoodIndexerConfig;
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmRpc};

/// What one reconciliation tick did.
///
/// `Skipped` and `NotConfigured` are separate variants because they are
/// different operational facts: the first means "this reserve exists and
/// this tick could not read it", the second means "this deployment has
/// no Robinhood reserve at all". Collapsing them would report a missing
/// endpoint and a missing config section as the same condition.
#[derive(Debug, Clone)]
pub enum ReserveTickOutcome {
    /// No `[reserve.robinhood]` section, so no `reserve_ledger` row.
    /// Nothing to reconcile and nothing to record — the fail-closed
    /// default is "this reserve does not exist", and inventing a skip
    /// finding against a row that was never created would be noise, not
    /// an audit trail.
    NotConfigured,
    /// The tick could not obtain a real balance. Already recorded via
    /// [`reconciliation::record_skipped`]; the cached balance is
    /// untouched.
    Skipped { reason: String },
    /// A real balance was observed and put through
    /// [`reconciliation::reconcile`].
    Reconciled {
        report: ReconciliationReport,
        /// The 18-decimal remainder below one canonical unit that the
        /// bridge contract holds and the canonical ledger cannot express.
        /// Non-zero is alarmed, not silent — see the module docs.
        dust_remainder: u128,
        /// The block height the balance was read at.
        block: u64,
    },
}

/// Reads the Robinhood bridge's ERC-20 balance and feeds it through the
/// shared reconciliation path. See the module docs for what it may and
/// may not do.
pub struct ReserveReconciler<R> {
    rpc: R,
    /// `[robinhood.indexer]`, which is where `expected_token`,
    /// `bridge_contract`, `chain_id` and `confirmation_depth` all come
    /// from. Deliberately the WHOLE resolved config rather than four
    /// loose fields: four addresses passed positionally is how a holder
    /// and a token get swapped.
    config: RobinhoodIndexerConfig,
    /// `[reserve].reconciliation_tolerance` — the same value the
    /// Goldcoin and Solana reconcilers are driven with. Not a
    /// Robinhood-specific knob.
    tolerance: u64,
}

impl<R: EvmRpc + EvmCallRpc> ReserveReconciler<R> {
    pub fn new(rpc: R, config: RobinhoodIndexerConfig, tolerance: u64) -> ReserveReconciler<R> {
        ReserveReconciler {
            rpc,
            config,
            tolerance,
        }
    }

    /// The token whose `balanceOf` is read, and the holder it is read
    /// for. Exposed for tests and operator tooling so the pair can be
    /// asserted as configured rather than inferred.
    pub fn observed_pair(&self) -> (crate::evm::EvmAddress, crate::evm::EvmAddress) {
        (self.config.expected_token, self.config.bridge_contract)
    }

    /// One reconciliation tick.
    ///
    /// Never returns an error: every failure is either recorded as a
    /// skip and reported as [`ReserveTickOutcome::Skipped`], or — for a
    /// ledger failure, where recording the skip is itself impossible —
    /// reported as a skip whose reason names the ledger error. The
    /// caller is a loop; a `Result` here would only ever be logged.
    pub async fn tick(&self, ledger: &mut Ledger, now: i64) -> ReserveTickOutcome {
        // "Configured" is detected through the ledger's own typed error
        // rather than a separate existence probe, exactly as
        // `super::admin::reserve_report` does it: `reserve_snapshot`
        // already distinguishes "no row" from every other failure, and a
        // second probe could answer differently from the read that
        // follows it.
        match ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve) {
            Ok(_) => {}
            Err(LedgerError::ReserveNotInitialized(_)) => return ReserveTickOutcome::NotConfigured,
            Err(e) => {
                return self.skip(ledger, format!("could not read the reserve row: {e}"), now)
            }
        }

        // Checked every tick, not once at startup — see the module docs.
        // A repointed endpoint that we kept reading would reconcile this
        // reserve against a DIFFERENT CHAIN's balance, which is the one
        // way a read-only component could still cause a loss.
        match self.rpc.chain_id().await {
            Ok(observed) if observed == self.config.chain_id => {}
            Ok(observed) => {
                return self.skip(
                    ledger,
                    format!(
                        "endpoint reports chain id {} but [robinhood.indexer].chain_id is {} — \
                         refusing to reconcile this reserve against another chain's balance",
                        observed.get(),
                        self.config.chain_id.get()
                    ),
                    now,
                )
            }
            Err(e) => return self.skip(ledger, format!("could not read eth_chainId: {e}"), now),
        }

        let head = match self.rpc.block_number().await {
            Ok(h) => h,
            Err(e) => {
                return self.skip(ledger, format!("could not read eth_blockNumber: {e}"), now)
            }
        };
        // `head - block + 1 == confirmation_depth`, the same arithmetic
        // `super::indexer` uses, so "depth 1" means the head block on
        // both paths rather than meaning two different things.
        let Some(block) = (head + 1).checked_sub(self.config.confirmation_depth) else {
            return self.skip(
                ledger,
                format!(
                    "chain head {head} is shallower than the configured confirmation_depth {} — \
                     no block is confirmed enough to read a reserve balance from yet",
                    self.config.confirmation_depth
                ),
                now,
            );
        };
        let at = EvmBlockTag::Number(block);
        let token = TokenReader::new(self.config.expected_token);

        // The amount model takes no decimals parameter on purpose (see
        // `ROBINHOOD_DECIMALS`), so this is the one place the assumption
        // behind every conversion below is checked against the chain. A
        // token that is not 18-decimal is not this asset, and its
        // balance is not this reserve's balance.
        match token.decimals(&self.rpc, at).await {
            Ok(observed) => {
                if let Err(e) = ensure_robinhood_decimals(observed) {
                    return self.skip(ledger, format!("token decimals rejected: {e}"), now);
                }
            }
            Err(e) => {
                return self.skip(
                    ledger,
                    format!("could not read the token's decimals() at block {block}: {e}"),
                    now,
                )
            }
        }

        let word = match token
            .balance_of(&self.rpc, self.config.bridge_contract, at)
            .await
        {
            Ok(w) => w,
            Err(e) => {
                return self.skip(
                    ledger,
                    format!(
                        "could not read balanceOf({}) on token {} at block {block}: {e}",
                        self.config.bridge_contract, self.config.expected_token
                    ),
                    now,
                )
            }
        };

        // The project's amount model, not a local rescale: the 10^10
        // factor lives in exactly one place and this is a consumer of it.
        let floor = match RobinhoodAtomic::try_from_u256(word).and_then(|a| a.to_canonical_floor())
        {
            Ok(f) => f,
            Err(e) => {
                return self.skip(
                    ledger,
                    format!("observed balance {word} is not a usable reserve amount: {e}"),
                    now,
                )
            }
        };

        if !floor.is_exact() {
            // Alarmed, never silent. The value reconciled below is a
            // lower bound, which is the pause-safe direction, but an
            // operator should still know the contract is holding units
            // the ledger has no representation for.
            tracing::error!(
                target: "robinhood_reserve",
                block,
                bridge_contract = %self.config.bridge_contract,
                token = %self.config.expected_token,
                observed_robinhood_atomic = %word,
                dust_remainder = floor.remainder,
                canonical_floor = floor.canonical.0,
                "the Robinhood bridge holds sub-canonical dust: the 18-decimal balance is not \
                 exactly representable in the ledger's 8-decimal unit. Reconciling against the \
                 exact FLOOR, which understates the reserve and can therefore only ever cause a \
                 pause, never mask one."
            );
            if let Err(e) = reconciliation::record_note(
                ledger,
                ReserveDirection::RobinhoodReserve,
                &format!(
                    "sub-canonical dust: balanceOf at block {block} is {word} (18 dp), \
                     remainder {} below one canonical unit; reconciled against the floor {}",
                    floor.remainder, floor.canonical.0
                ),
                now,
            ) {
                tracing::warn!(
                    target: "robinhood_reserve",
                    error = %e,
                    "could not record the dust observation"
                );
            }
        }

        // The one write, and it is the shared one: `reconcile` owns the
        // balance refresh, the invariant, the drop check, the audit
        // finding and the auto-pause for every direction alike.
        match reconciliation::reconcile(
            ledger,
            ReserveDirection::RobinhoodReserve,
            floor.canonical.0,
            self.tolerance,
            now,
        ) {
            Ok(report) => ReserveTickOutcome::Reconciled {
                report,
                dust_remainder: floor.remainder,
                block,
            },
            Err(e) => self.skip(ledger, format!("reconcile failed: {e}"), now),
        }
    }

    /// Records the skip so it is auditable rather than silently absent,
    /// and reports it. Deliberately does NOT touch the cached balance —
    /// that is the whole point of skipping.
    fn skip(&self, ledger: &mut Ledger, reason: String, now: i64) -> ReserveTickOutcome {
        if let Err(e) =
            reconciliation::record_skipped(ledger, ReserveDirection::RobinhoodReserve, &reason, now)
        {
            // The ledger itself is unavailable. Nothing further can be
            // persisted, so the log is the only remaining channel — but
            // the outcome still reports the original reason, not this
            // secondary failure.
            tracing::warn!(
                target: "robinhood_reserve",
                error = %e,
                %reason,
                "could not record a skipped Robinhood reserve reconciliation"
            );
        }
        ReserveTickOutcome::Skipped { reason }
    }
}

#[cfg(test)]
mod tests;
