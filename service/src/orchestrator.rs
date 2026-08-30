//! The orchestrator: wires the indexers, reconciliation, and the internal
//! signer groups into a single per-tick sweep that drives every
//! `bridge_requests` row forward (docs/03-architecture.md,
//! docs/07-implementation-plan.md Phase 4).
//!
//! # Non-blocking, poll-driven, never a single point of trust
//!
//! Every settlement step that touches a chain (submitting
//! `release_from_reserve`/`record_goldcoin_completion` to Solana,
//! broadcasting a Goldcoin payout) is split into a "submit" phase and a
//! separate "poll for confirmation" phase, run as two ordinary tick
//! phases rather than one call that blocks waiting for confirmation. This
//! matches how the rest of this codebase already treats chain state
//! (the Goldcoin/Solana indexers are themselves poll loops) and keeps a
//! single tick bounded and fast — no phase within a tick blocks on chain
//! finality.
//!
//! The orchestrator itself holds no threshold authority: every release and
//! every completion record requires collecting `attestation_threshold`
//! independent [`crate::signing::attestation`] signatures (ed25519) or
//! `vault_threshold` independent [`crate::signing::goldcoin_vault`]
//! partial signatures (secp256k1) — each produced by a signer that
//! re-derives its own claim from the ledger and a live chain read, never
//! from a value the orchestrator hands it. The orchestrator only
//! sequences these independent signers and submits what they jointly
//! produce; it cannot single-handedly authorize anything
//! (docs/02-trust-model.md: 2-of-3 internal threshold custody, not
//! federation).
//!
//! # Signer abstraction, not a concrete signer type
//!
//! The orchestrator holds its signer pools as `Box<dyn VaultSigner>`/
//! `Box<dyn AttestationSigner>` (`signing::signers`) — never a concrete
//! signer type — so a real HSM/KMS-backed implementation is a construction-
//! site change (whatever builds the `Vec` passed to `Orchestrator::new`),
//! not a change to any settlement logic in this file. Every entry in each
//! pool can be a different concrete implementation, matching the approved
//! trust model's requirement that the custody domains be genuinely
//! separate (docs/02-trust-model.md), not just separate in name. This
//! phase (docs/12-management-decisions.md item 2, still open) still only
//! wires up [`crate::signing::goldcoin_vault::DevVaultSigner`]/
//! [`crate::signing::attestation::DevAttestationSigner`] — an in-memory,
//! non-production stand-in — for local development and testing; see those
//! modules' docs.
//!
//! # Per-request failure never aborts a tick
//!
//! [`Orchestrator::tick`] processes every eligible request in every phase;
//! a failure attesting/signing/submitting/broadcasting for one request is
//! recorded in [`TickReport::errors`] and that request simply does not
//! progress this tick (retried next tick) — it never stops other requests
//! or other phases from being processed (fail-closed per request, not
//! fail-closed for the whole service).

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction as SolanaTransaction;

use crate::amount_conversion;
use crate::goldcoin::address::Network;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::indexer::{GoldcoinRpc, Indexer, TickOutcome as GoldcoinTickOutcome};
use crate::goldcoin::multisig;
use crate::goldcoin::rpc::BroadcastOutcome;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{
    Direction, Ledger, LedgerError, RequestState, ReserveDirection, ResumeManualReviewOutcome,
};
use crate::ops::indexer_status::IndexerStatus;
use crate::quota::{self, QuotaReport};
use crate::reconciliation::{self, ReconciliationReport};
use crate::signing::attestation::{
    self, independently_attest_completion, independently_attest_release, AttestationError,
};
use crate::signing::goldcoin_vault::{
    independently_sign_all_inputs, DevLedgerPayoutSource, SigningError,
};
use crate::signing::signers::{AttestationSigner, VaultSigner};
use crate::solana::accounts;
use crate::solana::ed25519;
use crate::solana::indexer::{SolanaIndexer, SolanaTickOutcome};
use crate::solana::instructions;
use crate::solana::rpc::{SolanaRpc, SolanaRpcError};

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("attestation error: {0}")]
    Attestation(#[from] AttestationError),
    #[error("vault signing error: {0}")]
    Signing(#[from] SigningError),
    #[error("multisig assembly error: {0}")]
    Multisig(#[from] multisig::MultisigError),
    #[error("solana rpc error: {0}")]
    SolanaRpc(#[from] SolanaRpcError),
    #[error("goldcoin rpc error: {0}")]
    GoldcoinRpc(#[from] crate::goldcoin::rpc::RpcError),
    #[error("attestation signers disagree on the message for request {0} — refusing to proceed")]
    InconsistentAttestation(i64),
    #[error("Goldcoin payout broadcast for request {0} reports missing inputs — vault UTXO conflict, needs operator attention")]
    PayoutBroadcastConflict(i64),
    #[error("request {0} is missing a field required to build its settlement transaction")]
    IncompleteRequest(i64),
    #[error("request {0}'s amount cannot be converted to the reserve mint's live decimals: {1}")]
    Conversion(i64, crate::amount_conversion::ConversionError),
}

#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    pub attestation_threshold: usize,
    pub vault_threshold: usize,
    pub required_goldcoin_confirmations: i64,
    pub fee_rate_per_kb: u64,
    pub dust_threshold: u64,
    pub max_inputs: usize,
    /// Target size (canonical atomic units) for each deterministic change
    /// FAN-OUT output a Goldcoin payout produces — see
    /// `goldcoin::coin::finalize_fanout`. Production-aware: sized relative
    /// to the current maximum net payout, never a stale historical limit.
    pub change_fanout_target_atomic: u64,
    /// Hard cap on how many change outputs one payout may ever produce.
    pub change_fanout_max_outputs: usize,
    pub reconciliation_tolerance: u64,
    /// Minimum confirmations for a vault output to be synced into
    /// `vault_utxos` and become eligible for coin selection (see
    /// `tick_vault_utxos`).
    pub vault_min_confirmations: i64,
    /// Which Goldcoin network vault/payout-destination addresses are
    /// encoded/decoded against (docs/16-p0-checkpoint.md) — the vault
    /// itself is constructed with this same network, so this only matters
    /// for decoding a Solana->GLC payout's destination address.
    pub goldcoin_network: Network,
    /// Bounds every individual signer call (`signing::signers` module
    /// docs) — defense in depth against a hanging/misbehaving signer
    /// implementation, applied on top of whatever timeout the
    /// implementation itself may enforce.
    pub signer_timeout: Duration,
    /// Upper bound on how many `ManualReview` requests
    /// `tick_auto_resume_utxo_liquidity_backlog` will resume in a single
    /// tick, regardless of how many more would otherwise pass every
    /// safety check — bounds worst-case tick duration on a large backlog
    /// and rate-limits how much demand re-enters the payout pipeline at
    /// once after a recovery (docs/09-runbook.md's "UTXO liquidity"
    /// section).
    pub max_auto_resumes_per_tick: usize,
}

impl OrchestratorConfig {
    /// The subset of this config every independent payout re-derivation
    /// needs, bundled into `goldcoin::payout::PayoutPolicy`.
    pub fn payout_policy(&self) -> crate::goldcoin::payout::PayoutPolicy {
        crate::goldcoin::payout::PayoutPolicy {
            fee_rate_per_kb: self.fee_rate_per_kb,
            dust_threshold: self.dust_threshold,
            max_inputs: self.max_inputs,
            change_fanout_target_atomic: self.change_fanout_target_atomic,
            change_fanout_max_outputs: self.change_fanout_max_outputs,
        }
    }
}

#[derive(Debug, Default)]
pub struct TickReport {
    pub goldcoin_indexer: Option<Result<GoldcoinTickOutcome, String>>,
    pub solana_indexer: Option<Result<SolanaTickOutcome, String>>,
    pub expired_reservations: u32,
    pub solana_reconciliation: Option<Result<ReconciliationReport, String>>,
    pub goldcoin_reconciliation: Option<Result<ReconciliationReport, String>>,
    /// A second, EARLIER run of the exact same GoldcoinReserve
    /// reconciliation pass as `goldcoin_reconciliation` above (see
    /// `Orchestrator::tick`'s own comment on why) — before any new
    /// SolToGlc obligation is admitted this tick, not after. Same formula,
    /// same auto-pause trigger/action; only its position (and therefore
    /// its freshness relative to admission) differs.
    pub goldcoin_pre_admission_reconciliation: Option<Result<ReconciliationReport, String>>,
    /// `GlcToSol`'s (release, direction byte 0) rolling-24h-volume quota
    /// check — see `crate::quota`'s module docs for the auto-pause-but-
    /// never-auto-unpause contract this enforces.
    pub solana_rolling_volume_quota: Option<Result<QuotaReport, String>>,
    /// `SolToGlc`'s (deposit, direction byte 1) rolling-24h-volume quota
    /// check.
    pub goldcoin_rolling_volume_quota: Option<Result<QuotaReport, String>>,
    pub releases_submitted: u32,
    pub releases_confirmed: u32,
    pub payouts_built: u32,
    pub payouts_confirmed: u32,
    pub completions_submitted: u32,
    pub completions_confirmed: u32,
    /// Outcome of this tick's automatic-recovery pass over `ManualReview`
    /// requests parked purely for `utxo_liquidity_low_at_fold` — see
    /// [`Orchestrator::tick_auto_resume_utxo_liquidity_backlog`]. `None`
    /// only if the pass itself could not run at all (a ledger read
    /// error); a normal tick where nothing was eligible or the pool was
    /// already too thin still produces `Some(AutoResumeReport)` with
    /// `resumed == 0`.
    pub goldcoin_utxo_liquidity_auto_resume: Option<AutoResumeReport>,
    pub errors: Vec<String>,
}

/// Summary of one call to
/// [`Orchestrator::tick_auto_resume_utxo_liquidity_backlog`] — surfaced in
/// [`TickReport`] so tests and operators can inspect the outcome
/// structurally, not only via logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoResumeReport {
    /// How many candidates were attempted (i.e. `resume_manual_review_sol_to_glc`
    /// was actually called) before the pass stopped, successfully or not.
    pub attempted: u32,
    /// How many of those attempts actually transitioned
    /// `ManualReview -> SourceFinalized` this tick.
    pub resumed: u32,
    /// How many attempts were refused with `LedgerError::RecipientRateLimited`
    /// — this recipient still has another qualifying obligation inside its
    /// rolling 24-hour window. Unlike every other refusal, this does NOT
    /// stop the batch: it says nothing about any OTHER recipient's
    /// eligibility, so the pass skips this one candidate and keeps
    /// draining the rest, oldest first.
    pub skipped: u32,
    /// Why the pass stopped before considering every remaining eligible
    /// candidate, if it did. `None` means every eligible candidate (up to
    /// `max_auto_resumes_per_tick`) was successfully resumed, or there
    /// were no eligible candidates at all.
    pub stopped_reason: Option<String>,
}

pub struct Orchestrator<GR: GoldcoinRpc, SR: SolanaRpc> {
    goldcoin_indexer: Indexer<GR>,
    solana_indexer: SolanaIndexer<SR>,
    ledger: Ledger,
    goldcoin_rpc: GR,
    solana_rpc: SR,
    vault: MultisigVault,
    vault_signers: Vec<Box<dyn VaultSigner>>,
    attestation_signers: Vec<Box<dyn AttestationSigner>>,
    /// Fee payer / transaction submitter for `release_from_reserve` and
    /// `record_goldcoin_completion`. Distinct from the attestation
    /// signers: paying fees and submitting a transaction is not a
    /// custody-authority action — the authority is entirely in the
    /// attestation signatures the transaction carries.
    submitter: Keypair,
    config: OrchestratorConfig,
    /// Updated from this tick loop's own `TickOutcome`/`SolanaTickOutcome`
    /// every tick — what `ops::health`'s `/health` endpoint polls between
    /// ticks to see a halted/stalled indexer that would otherwise be
    /// invisible (see `ops::indexer_status` module docs).
    goldcoin_indexer_status: Arc<IndexerStatus>,
    solana_indexer_status: Arc<IndexerStatus>,
}

impl<GR: GoldcoinRpc, SR: SolanaRpc> Orchestrator<GR, SR> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        goldcoin_indexer: Indexer<GR>,
        solana_indexer: SolanaIndexer<SR>,
        ledger: Ledger,
        goldcoin_rpc: GR,
        solana_rpc: SR,
        vault: MultisigVault,
        vault_signers: Vec<Box<dyn VaultSigner>>,
        attestation_signers: Vec<Box<dyn AttestationSigner>>,
        submitter: Keypair,
        config: OrchestratorConfig,
        now: i64,
    ) -> Self {
        Orchestrator {
            goldcoin_indexer,
            solana_indexer,
            ledger,
            goldcoin_rpc,
            solana_rpc,
            vault,
            vault_signers,
            attestation_signers,
            submitter,
            config,
            goldcoin_indexer_status: Arc::new(IndexerStatus::new(now)),
            solana_indexer_status: Arc::new(IndexerStatus::new(now)),
        }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Shared liveness status for the Goldcoin indexer — clone the `Arc`
    /// into an `ops::collector::OpsCollector` to expose it over `/health`.
    pub fn goldcoin_indexer_status(&self) -> Arc<IndexerStatus> {
        Arc::clone(&self.goldcoin_indexer_status)
    }

    /// Shared liveness status for the Solana indexer — see
    /// [`Orchestrator::goldcoin_indexer_status`].
    pub fn solana_indexer_status(&self) -> Arc<IndexerStatus> {
        Arc::clone(&self.solana_indexer_status)
    }

    pub async fn tick(&mut self, now: i64) -> TickReport {
        let mut report = TickReport::default();

        let goldcoin_outcome = self.goldcoin_indexer.tick().await;
        match &goldcoin_outcome {
            Ok(GoldcoinTickOutcome::Progressed { reorg, .. }) => {
                self.goldcoin_indexer_status.record_tick(now);
                if let Some(reorg) = reorg {
                    self.goldcoin_indexer_status
                        .record_reorg(reorg.old_tip_height - reorg.fork_height);
                }
            }
            Ok(GoldcoinTickOutcome::Halted { attempted_depth }) => {
                self.goldcoin_indexer_status.record_halt(*attempted_depth);
            }
            Ok(GoldcoinTickOutcome::PostFinalityReorgHalted {
                fork_height,
                old_tip_height,
                ..
            }) => {
                // Reuses the same halted/health signal as the generic
                // max-reorg-depth halt — this IS a real halt of the
                // indexer, and the real, persisted safety mechanism is
                // the global reserve pause `Ledger::
                // record_post_finality_reorg` already set, not this
                // status flag; this only makes it visible on `/health`.
                self.goldcoin_indexer_status
                    .record_halt(old_tip_height - fork_height);
            }
            Err(_) => {} // transient failure; staleness accumulates, retried next tick
        }
        report.goldcoin_indexer = Some(goldcoin_outcome.map_err(|e| e.to_string()));

        // Before admitting any newly observed SolToGlc obligation this
        // tick, run the SAME reconciliation pass that otherwise only runs
        // at the end of the tick — so `Ledger::fold_sol_deposit`'s
        // admission check (`balance - protected_minimum -
        // reserved_liquidity >= amount`, unchanged) sees the freshest
        // balance available, rather than whatever the LAST tick's
        // end-of-tick pass happened to leave cached. Without this, that
        // cached figure can go stale for an unbounded number of ticks if
        // reconciliation itself is skipping (a transient RPC failure calls
        // `reconciliation::record_skipped` and returns without refreshing
        // it) — independent of whether new obligations keep arriving and
        // getting admitted against it in the meantime.
        //
        // Deliberately reuses `tick_goldcoin_reconciliation` verbatim
        // (same formula, same auto-pause trigger/action, same "never
        // auto-unpause" rule) rather than a bare balance write: a bare
        // write would silently update the cache reconciliation's own
        // "unexplained balance drop" detection compares against, without
        // ever evaluating whether that drop itself was a problem — which
        // would weaken that detection, not just leave it unchanged. Running
        // the real, unchanged check twice (once here, once at the existing
        // end-of-tick position) means any genuine, pre-existing breach
        // gets caught and paused before it can be compounded by more
        // admissions this tick, while a batch that individually fits
        // within available capacity is never affected — `fold_sol_deposit`
        // itself never lets `reserved_liquidity` exceed what a given
        // snapshot supports, so this second pass only ever fires for a
        // problem that already existed, not for parking one oversized
        // request out of an otherwise-fine batch. The pre-existing
        // end-of-tick call, its report field, and its position are
        // unchanged; this only adds an earlier, additional pass, recorded
        // separately in `report.goldcoin_pre_admission_reconciliation`.
        report.goldcoin_pre_admission_reconciliation = self.tick_goldcoin_reconciliation(now).await;

        let solana_outcome = self.solana_indexer.tick().await;
        if solana_outcome.is_ok() {
            self.solana_indexer_status.record_tick(now);
        }
        report.solana_indexer = Some(solana_outcome.map_err(|e| e.to_string()));

        report.expired_reservations = match self.ledger.expire_reservations(now) {
            Ok(n) => n,
            Err(e) => {
                report.errors.push(format!("expire_reservations: {e}"));
                0
            }
        };

        self.tick_release_settlements(now, &mut report).await;
        self.tick_release_confirmations(now, &mut report).await;
        self.tick_vault_utxos(now, &mut report).await;
        self.tick_goldcoin_payouts(now, &mut report).await;
        self.tick_goldcoin_payout_confirmations(now, &mut report)
            .await;
        self.tick_goldcoin_completions(now, &mut report).await;
        self.tick_goldcoin_completion_confirmations(now, &mut report)
            .await;

        // Deliberately last: reconciliation compares the live on-chain
        // balance against this service's own bookkeeping
        // (`reserved_liquidity`/`pending_obligations`), and a settlement
        // that lands on-chain is only reflected in that bookkeeping once
        // `tick_release_confirmations`/`tick_goldcoin_completion_confirmations`
        // above have run — a real bug this phase's real-node testing
        // caught: running reconciliation first meant the very first
        // successful release's on-chain balance drop was compared against
        // still-unadjusted bookkeeping and misclassified as an unexplained
        // breach, auto-pausing the reserve after its first legitimate
        // settlement. Running reconciliation after every phase that can
        // move the ledger's own committed/settled totals keeps the
        // comparison honest. Both directions run every tick — Goldcoin's
        // reserve gets exactly the same automatic breach detection as
        // Solana's, not a weaker check (see `tick_goldcoin_reconciliation`).
        report.solana_reconciliation = self.tick_solana_reconciliation(now).await;
        report.goldcoin_reconciliation = self.tick_goldcoin_reconciliation(now).await;

        // Rolling-24h-volume quota enforcement (crate::quota module docs):
        // independent of reconciliation above — a quota exhaustion is not
        // a balance discrepancy, so it does not need to run after the
        // bookkeeping-affecting phases the reconciliation-ordering
        // comment above is about. Both directions checked every tick,
        // same as reconciliation.
        report.solana_rolling_volume_quota = self.tick_rolling_volume_quota(now, 0).await;
        report.goldcoin_rolling_volume_quota = self.tick_rolling_volume_quota(now, 1).await;

        // Deliberately last: automatic recovery must see the FRESHEST
        // possible `paused` state (this tick's own reconciliation AND
        // quota checks, both above, have already run) and the freshest
        // `available_utxo_count` (this tick's own `tick_goldcoin_payouts`,
        // above, has already consumed whatever it consumed) — never a
        // phase behind either signal. See
        // `tick_auto_resume_utxo_liquidity_backlog`'s own docs for why
        // this runs in-tick rather than as a separate periodic worker.
        report.goldcoin_utxo_liquidity_auto_resume =
            match self.tick_auto_resume_utxo_liquidity_backlog(now).await {
                Ok(r) => Some(r),
                Err(e) => {
                    report
                        .errors
                        .push(format!("tick_auto_resume_utxo_liquidity_backlog: {e}"));
                    None
                }
            };

        report
    }

    /// One direction's rolling-24h-volume quota tick: fetches the live
    /// `BridgeConfig` and that direction's `RollingVolumeWindow`
    /// (`direction_byte`: `0` = release/`GlcToSol`/`SolanaReserve`, `1` =
    /// deposit/`SolToGlc`/`GoldcoinReserve` — `accounts::
    /// rolling_volume_window_pda`'s convention), then delegates to
    /// [`quota::enforce_rolling_volume_quota`]. `None` (not an error) if
    /// the window account does not exist yet — same "not initialized yet"
    /// tolerance `tick_solana_reconciliation` already applies to the
    /// reserve token account.
    async fn tick_rolling_volume_quota(
        &mut self,
        now: i64,
        direction_byte: u8,
    ) -> Option<Result<QuotaReport, String>> {
        let reserve_direction = if direction_byte == 0 {
            ReserveDirection::SolanaReserve
        } else {
            ReserveDirection::GoldcoinReserve
        };
        let config = match attestation::fetch_bridge_config(&self.solana_rpc).await {
            Ok(c) => c,
            Err(e) => return Some(Err(e.to_string())),
        };
        let pda = accounts::rolling_volume_window_pda(direction_byte);
        let account = match self.solana_rpc.get_account(&pda).await {
            Ok(Some(a)) => a,
            Ok(None) => return None,
            Err(e) => return Some(Err(e.to_string())),
        };
        let window = match accounts::decode_rolling_volume_window(&account.data) {
            Ok(w) => w,
            Err(e) => return Some(Err(e.to_string())),
        };
        Some(
            quota::enforce_rolling_volume_quota(
                &mut self.ledger,
                reserve_direction,
                config.rolling_volume_limit,
                config.rolling_window_seconds,
                config.min_transfer_amount,
                window,
                now,
            )
            .map_err(|e| e.to_string()),
        )
    }

    // ------------------------------------------------ automatic UTXO-liquidity recovery --

    /// Automatically reconsiders `SolToGlc` requests parked in
    /// `ManualReview` for exactly three reasons —
    /// `Ledger::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW`
    /// (`"utxo_liquidity_low_at_fold"`),
    /// `Ledger::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED`
    /// (`"recipient_rate_limited"`), and
    /// `Ledger::MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED`
    /// (`"source_wallet_rate_limited"`) — resuming each one that still
    /// passes every safety check, oldest first, so an operator no longer
    /// has to run `glc-admin resume-manual-review` by hand once the
    /// condition that originally parked it clears: the mature UTXO pool
    /// recovering (docs/09-runbook.md's "UTXO liquidity" section), or
    /// either rolling 24-hour rate-limit window aging out (docs/09-runbook.md's
    /// recipient/source-wallet rate limit sections).
    ///
    /// # Why this runs here, in-tick, rather than a separate periodic worker
    ///
    /// Every admission/resume decision in this codebase is made by
    /// exactly one actor holding the one `&mut Ledger` — this tick loop.
    /// A separate worker on its own cadence, reading `available_utxo_count`
    /// and calling `resume_manual_review_sol_to_glc` independently, would
    /// reintroduce a second decision-maker racing the first; SQLite's own
    /// locking would prevent row corruption, but the simple
    /// single-writer-per-moment reasoning the rest of this design relies
    /// on would no longer hold. Running as the LAST phase of `tick()`
    /// (after this same tick's own end-of-tick reconciliation and
    /// rolling-volume-quota checks, both above) means this pass always
    /// sees the freshest possible `paused` state and the freshest
    /// `available_utxo_count` — never a phase behind either signal, which
    /// running any earlier (e.g. right after the Goldcoin indexer or
    /// `tick_vault_utxos`) would risk.
    ///
    /// # Reuses `resume_manual_review_sol_to_glc` verbatim — no separate logic
    ///
    /// Every per-request safety check (reason match, source finalized, no
    /// existing payout/destination, the count-based UTXO-liquidity gate,
    /// the value-based reserve invariant) is the EXACT SAME function
    /// `glc-admin resume-manual-review` calls, re-checked fresh on every
    /// single call — never a parallel re-implementation that could drift.
    /// The only thing this method owns is: which candidates are even
    /// considered (this one reason, oldest first), when to stop, and
    /// logging — never a duplicate safety decision.
    ///
    /// # Stop conditions (checked in order; any one stops the WHOLE pass)
    ///
    /// - `GoldcoinReserve` is currently `paused` — covers both a hard
    ///   reserve-invariant breach (`reconciliation::reconcile`) and a
    ///   rolling-volume-quota exhaustion (`quota::enforce_rolling_volume_quota`),
    ///   since both already funnel through this exact flag; no separate
    ///   quota/invariant check is needed here.
    /// - `GoldcoinReserve` admission is explicitly `admission_closed` —
    ///   deliberately STRICTER here than `resume_manual_review_sol_to_glc`
    ///   itself (which never checks this, by design, for a single
    ///   attended operator action): an operator who explicitly closed
    ///   admission — e.g. mid-investigation — should never have an
    ///   unattended background pass quietly draining the backlog behind
    ///   them. A human resuming one request by hand is still unaffected.
    /// - `max_auto_resumes_per_tick` reached.
    /// - Any individual `resume_manual_review_sol_to_glc` call returns
    ///   `Err` — stops immediately, never skips to the next candidate;
    ///   an unexpected failure on one candidate is a signal to stop and
    ///   let a human look, not a reason to keep going. The TWO exceptions:
    ///   `LedgerError::RecipientRateLimited` and
    ///   `LedgerError::SourceWalletRateLimited` are each a per-recipient
    ///   or per-wallet, independent condition that says nothing about any
    ///   other candidate's eligibility, so either one increments
    ///   `AutoResumeReport::skipped` and the pass continues to the next
    ///   candidate instead of stopping — this is what lets a mixed batch of
    ///   parked reasons still drain oldest-first without one still-rate-
    ///   limited recipient or wallet stalling unrelated, eligible
    ///   candidates behind it.
    async fn tick_auto_resume_utxo_liquidity_backlog(
        &mut self,
        now: i64,
    ) -> Result<AutoResumeReport, LedgerError> {
        let mut result = AutoResumeReport::default();

        if self.ledger.is_paused(ReserveDirection::GoldcoinReserve)? {
            result.stopped_reason =
                Some("GoldcoinReserve is paused (hard invariant or rolling-volume quota)".into());
            tracing::info!(
                target: "auto_resume",
                reason = %result.stopped_reason.as_deref().unwrap(),
                "auto-resume: skipped this tick"
            );
            return Ok(result);
        }
        if self
            .ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)?
        {
            result.stopped_reason = Some("GoldcoinReserve admission is explicitly closed".into());
            tracing::info!(
                target: "auto_resume",
                reason = %result.stopped_reason.as_deref().unwrap(),
                "auto-resume: skipped this tick"
            );
            return Ok(result);
        }

        let candidates: Vec<i64> = self
            .ledger
            .requests_by_state(Direction::SolToGlc, RequestState::ManualReview)?
            .into_iter()
            .filter(|r| {
                matches!(
                    r.manual_review_note.as_deref(),
                    Some(Ledger::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW)
                        | Some(Ledger::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED)
                        | Some(Ledger::MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED)
                )
            })
            .map(|r| r.id)
            .collect();

        for request_id in candidates {
            if result.attempted >= self.config.max_auto_resumes_per_tick as u32 {
                result.stopped_reason = Some(format!(
                    "max_auto_resumes_per_tick ({}) reached",
                    self.config.max_auto_resumes_per_tick
                ));
                tracing::info!(
                    target: "auto_resume",
                    reason = %result.stopped_reason.as_deref().unwrap(),
                    resumed = result.resumed,
                    "auto-resume: batch stopped"
                );
                break;
            }
            result.attempted += 1;
            tracing::info!(target: "auto_resume", request_id, "auto-resume: attempting");
            match self.ledger.resume_manual_review_sol_to_glc(
                request_id,
                "auto-resume: utxo liquidity recovered",
                "auto-resume",
                now,
            ) {
                Ok(ResumeManualReviewOutcome::Resumed) => {
                    result.resumed += 1;
                    tracing::info!(target: "auto_resume", request_id, "auto-resume: succeeded");
                }
                Ok(ResumeManualReviewOutcome::AlreadyResumed { state }) => {
                    // Defensive only: our own query above only ever
                    // selects rows currently in ManualReview, so this
                    // should not occur in practice.
                    tracing::info!(
                        target: "auto_resume",
                        request_id,
                        state = ?state,
                        "auto-resume: already resumed, nothing to do"
                    );
                }
                Err(LedgerError::RecipientRateLimited { retry_after, .. }) => {
                    // A per-recipient, independent condition — says nothing
                    // about any OTHER candidate's eligibility, so skip this
                    // one and keep draining the rest oldest-first, rather
                    // than stopping the whole batch.
                    result.skipped += 1;
                    tracing::info!(
                        target: "auto_resume",
                        request_id,
                        retry_after,
                        "auto-resume: skipped, recipient still rate-limited"
                    );
                }
                Err(LedgerError::SourceWalletRateLimited { retry_after, .. }) => {
                    // The Solana-source-wallet twin of the arm just above —
                    // also a per-wallet, independent condition that must
                    // never stall unrelated candidates behind it.
                    result.skipped += 1;
                    tracing::info!(
                        target: "auto_resume",
                        request_id,
                        retry_after,
                        "auto-resume: skipped, source wallet still rate-limited"
                    );
                }
                Err(e) => {
                    result.stopped_reason = Some(format!("request {request_id}: {e}"));
                    tracing::warn!(
                        target: "auto_resume",
                        request_id,
                        error = %e,
                        resumed = result.resumed,
                        "auto-resume: batch stopped"
                    );
                    break;
                }
            }
        }

        Ok(result)
    }

    // --------------------------------------------------------- reconciliation --

    /// Solana-reserve reconciliation: the reserve authority's SPL token
    /// account balance is a clean, already-available live read.
    async fn tick_solana_reconciliation(
        &mut self,
        now: i64,
    ) -> Option<Result<ReconciliationReport, String>> {
        let config = match attestation::fetch_bridge_config(&self.solana_rpc).await {
            Ok(c) => c,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::SolanaReserve,
                    &format!("could not read bridge_config: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let reserve_authority = accounts::reserve_authority_pda();
        let ata = accounts::associated_token_address(
            &reserve_authority,
            &config.reserve_token_mint,
            &config.reserve_token_program,
        );
        let account = match self.solana_rpc.get_account(&ata).await {
            Ok(a) => a,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::SolanaReserve,
                    &format!("could not read reserve token account: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let Some(account) = account else {
            let _ = reconciliation::record_skipped(
                &mut self.ledger,
                ReserveDirection::SolanaReserve,
                "reserve token account does not exist yet",
                now,
            );
            return None;
        };
        let observed_balance = match accounts::decode_token_account_amount(&account.data) {
            Ok(b) => b,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::SolanaReserve,
                    &format!("malformed reserve token account: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        Some(
            reconciliation::reconcile(
                &mut self.ledger,
                ReserveDirection::SolanaReserve,
                observed_balance,
                self.config.reconciliation_tolerance,
                now,
            )
            .map_err(|e| e.to_string()),
        )
    }

    /// Goldcoin-reserve reconciliation: sums a fresh, independent
    /// `listunspent` read against the vault address. Deliberately not the
    /// ledger's own `vault_utxos` cache — `tick_vault_utxos` (run earlier
    /// this same tick) populates that cache from the same kind of read,
    /// and reconciling against a cache this service itself just wrote
    /// would never catch a bug in that write path. Reading the chain
    /// directly here, independent of the service's own bookkeeping, is
    /// the same discipline `tick_solana_reconciliation` already follows
    /// by reading the SPL token account directly rather than trusting a
    /// cached balance.
    async fn tick_goldcoin_reconciliation(
        &mut self,
        now: i64,
    ) -> Option<Result<ReconciliationReport, String>> {
        let addresses = match self.watched_goldcoin_addresses() {
            Ok(a) => a,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::GoldcoinReserve,
                    &format!("could not enumerate watched Goldcoin addresses: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let entries = match self
            .goldcoin_rpc
            .list_unspent(self.config.vault_min_confirmations, &addresses)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::GoldcoinReserve,
                    &format!("could not read vault UTXOs: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let observed_balance: u64 = entries
            .iter()
            .filter(|e| e.solvable)
            .map(|e| crate::goldcoin::deposit::glc_to_atomic(e.amount))
            .sum();
        Some(
            reconciliation::reconcile(
                &mut self.ledger,
                ReserveDirection::GoldcoinReserve,
                observed_balance,
                self.config.reconciliation_tolerance,
                now,
            )
            .map_err(|e| e.to_string()),
        )
    }

    // -------------------------------------------------- GlcToSol: release --

    async fn tick_release_settlements(&mut self, now: i64, report: &mut TickReport) {
        let requests = match self
            .ledger
            .requests_by_state(Direction::GlcToSol, RequestState::SourceFinalized)
        {
            Ok(r) => r,
            Err(e) => {
                report
                    .errors
                    .push(format!("requests_by_state(GlcToSol, SourceFinalized): {e}"));
                return;
            }
        };
        for request in requests {
            match self.submit_release(request.id, now).await {
                Ok(()) => report.releases_submitted += 1,
                Err(e) => report
                    .errors
                    .push(format!("release request {}: {e}", request.id)),
            }
        }
    }

    async fn submit_release(&mut self, request_id: i64, now: i64) -> Result<(), OrchestratorError> {
        let mut sigs = Vec::with_capacity(self.config.attestation_threshold);
        let mut message = None;
        for signer in self
            .attestation_signers
            .iter()
            .take(self.config.attestation_threshold)
        {
            let (pubkey, signature, msg) = independently_attest_release(
                signer.as_ref(),
                &self.ledger,
                &self.solana_rpc,
                request_id,
                self.config.signer_timeout,
            )
            .await?;
            if let Some(prev) = &message {
                if prev != &msg {
                    return Err(OrchestratorError::InconsistentAttestation(request_id));
                }
            } else {
                message = Some(msg);
            }
            log_signature_grant(
                &mut self.ledger,
                "attestation",
                &pubkey.to_string(),
                request_id,
                now,
            );
            sigs.push((pubkey, signature));
        }
        let message = message.ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        self.ledger
            .record_attestation(request_id, "release", &message, now)?;

        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        let txid = request
            .source_txid
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let vout = request
            .source_vout
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let recipient = Pubkey::try_from(request.recipient.as_slice())
            .map_err(|_| OrchestratorError::IncompleteRequest(request_id))?;

        let key_set = attestation::fetch_attestation_key_set(&self.solana_rpc).await?;
        let config = attestation::fetch_bridge_config(&self.solana_rpc).await?;
        let solana_decimals =
            accounts::fetch_reserve_mint_decimals(&self.solana_rpc, &config.reserve_token_mint)
                .await?;
        // Must match exactly what every attesting signer independently
        // signed in `independently_attest_release` above — both derive it
        // the same way, from the same immutable, live-read mint decimals
        // and the same recompute-from-gross discipline (never trusting the
        // ledger's own stored fee/net columns directly), so they always
        // agree (docs/18-token-2022-support.md, docs/20-bridge-fee.md).
        let fee_breakdown = amount_conversion::verify_fee_breakdown(
            request.gross_amount_atomic,
            request.fee_bps,
            request.fee_amount_atomic,
            request.net_amount_atomic,
        )
        .map_err(|e| OrchestratorError::Conversion(request_id, e))?;
        let solana_amount = fee_breakdown
            .net
            .to_solana(solana_decimals)
            .map_err(|e| OrchestratorError::Conversion(request_id, e))?
            .0;

        // The recipient column stores the OWNER pubkey; the release moves
        // funds to that owner's canonical ATA, which the on-chain program
        // requires to already exist (no `init_if_needed` there, by
        // design). Guarantee the precondition in the SAME transaction with
        // an idempotent canonical-ATA creation (no-op when it already
        // exists; fails closed on a non-canonical occupant) — atomic with
        // the release, so a retry never sees partial state and simply
        // carries the same idempotent instruction again. The proof must
        // stay at relative -1 from the release, so the creation goes
        // first.
        let create_ata_ix = instructions::create_recipient_ata_idempotent(
            &self.submitter.pubkey(),
            &recipient,
            &config.reserve_token_mint,
            &config.reserve_token_program,
        );
        let proof_ix = ed25519::build_attestation_proof(&sigs, &message);
        let release_ix = instructions::release_from_reserve(
            &self.submitter.pubkey(),
            &config.reserve_token_mint,
            &config.reserve_token_program,
            &recipient,
            txid,
            vout,
            solana_amount,
            key_set.epoch,
        );
        let blockhash = self.solana_rpc.get_latest_blockhash().await?;
        let tx = SolanaTransaction::new_signed_with_payer(
            &[create_ata_ix, proof_ix, release_ix],
            Some(&self.submitter.pubkey()),
            &[&self.submitter],
            blockhash,
        );
        let signature = self.solana_rpc.send_transaction(&tx).await?;
        self.ledger
            .record_release_submitted(request_id, signature_bytes(&signature), now)?;
        Ok(())
    }

    async fn tick_release_confirmations(&mut self, now: i64, report: &mut TickReport) {
        let requests = match self
            .ledger
            .requests_by_state(Direction::GlcToSol, RequestState::DestinationSubmitted)
        {
            Ok(r) => r,
            Err(e) => {
                report.errors.push(format!(
                    "requests_by_state(GlcToSol, DestinationSubmitted): {e}"
                ));
                return;
            }
        };
        for request in requests {
            let destination_txid = match self.ledger.get_destination_txid(request.id) {
                Ok(v) => v,
                Err(e) => {
                    report
                        .errors
                        .push(format!("release request {}: {e}", request.id));
                    continue;
                }
            };
            let Some(sig_bytes) = destination_txid.and_then(|v| <[u8; 64]>::try_from(v).ok())
            else {
                report.errors.push(format!(
                    "release request {}: missing/malformed destination_txid",
                    request.id
                ));
                continue;
            };
            let signature = Signature::from(sig_bytes);
            match self.solana_rpc.get_signature_status(&signature).await {
                Ok(Some(Ok(()))) => match self.ledger.mark_release_confirmed(request.id, now) {
                    Ok(()) => report.releases_confirmed += 1,
                    Err(e) => report
                        .errors
                        .push(format!("release request {}: {e}", request.id)),
                },
                Ok(Some(Err(reason))) => report.errors.push(format!(
                    "release request {} REJECTED on chain: {reason}",
                    request.id
                )),
                Ok(None) => {} // still pending; retried next tick
                Err(e) => report
                    .errors
                    .push(format!("release request {}: {e}", request.id)),
            }
        }
    }

    // ------------------------------------------------- SolToGlc: payout --

    /// Syncs the vault's live UTXO set into the ledger via `listunspent`
    /// (`solvable`, not `spendable` — docs/goldcoin-rpc-notes.md) so coin
    /// selection has real chain data to select from. Nothing else in this
    /// codebase populates `vault_utxos` from a live read — without this
    /// phase, `tick_goldcoin_payouts` would always fail with "insufficient
    /// funds" against a real node (a real gap Phase 6 real-node testing
    /// caught: every existing test seeded `vault_utxos` directly).
    async fn tick_vault_utxos(&mut self, now: i64, report: &mut TickReport) {
        let addresses = match self.watched_goldcoin_addresses() {
            Ok(a) => a,
            Err(e) => {
                report
                    .errors
                    .push(format!("watched_goldcoin_addresses: {e}"));
                return;
            }
        };
        let entries = match self
            .goldcoin_rpc
            .list_unspent(self.config.vault_min_confirmations, &addresses)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                report.errors.push(format!("list_unspent: {e}"));
                return;
            }
        };
        let observed: Vec<(VaultUtxo, i64, String)> = entries
            .iter()
            .filter(|e| e.solvable)
            .filter_map(|e| {
                let txid: [u8; 32] = crate::goldcoin::hex::decode_exact(&e.txid).ok()?;
                Some((
                    VaultUtxo {
                        txid,
                        vout: e.vout,
                        amount_atomic: crate::goldcoin::deposit::glc_to_atomic(e.amount),
                        script_pubkey_hex: e.script_pub_key.clone(),
                    },
                    e.confirmations,
                    e.script_pub_key.clone(),
                ))
            })
            .collect();
        if let Err(e) =
            self.ledger
                .sync_vault_utxos(&observed, self.config.vault_min_confirmations, now)
        {
            report.errors.push(format!("sync_vault_utxos: {e}"));
        }
    }

    /// Every Goldcoin address this service must watch for spendable vault
    /// funds: the shared legacy vault, plus every per-request derived
    /// deposit address ever assigned (`Ledger::all_glc_to_sol_deposit_
    /// addresses`) — a settled request's derived-address UTXO can still
    /// sit unswept, so the full historical set is watched, not just
    /// currently-open requests. Without this, a per-request deposit would
    /// never be discovered as spendable at all, regardless of any signing
    /// logic (`tick_vault_utxos`/`tick_goldcoin_reconciliation`).
    fn watched_goldcoin_addresses(&self) -> Result<Vec<String>, LedgerError> {
        let mut addresses = vec![self.vault.address().to_string()];
        addresses.extend(self.ledger.all_glc_to_sol_deposit_addresses()?);
        Ok(addresses)
    }

    async fn tick_goldcoin_payouts(&mut self, now: i64, report: &mut TickReport) {
        let requests = match self
            .ledger
            .requests_by_state(Direction::SolToGlc, RequestState::SourceFinalized)
        {
            Ok(r) => r,
            Err(e) => {
                report
                    .errors
                    .push(format!("requests_by_state(SolToGlc, SourceFinalized): {e}"));
                return;
            }
        };
        for request in requests {
            match self.ledger.get_goldcoin_payout(request.id) {
                Ok(Some(_)) => continue, // a previous attempt already exists; needs operator attention if stuck
                Ok(None) => {}
                Err(e) => {
                    report
                        .errors
                        .push(format!("payout request {}: {e}", request.id));
                    continue;
                }
            }
            match self.build_and_broadcast_payout(request.id, now).await {
                Ok(()) => report.payouts_built += 1,
                Err(e) => report
                    .errors
                    .push(format!("payout request {}: {e}", request.id)),
            }
        }
    }

    async fn build_and_broadcast_payout(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let threshold = self.config.vault_threshold;
        let source = DevLedgerPayoutSource {
            ledger: &self.ledger,
        };

        let policy = self.config.payout_policy();
        let (plan, mut tx, partials) = independently_sign_all_inputs(
            &self.vault_signers,
            &self.vault,
            &source,
            request_id,
            threshold,
            &policy,
            self.config.goldcoin_network,
            self.config.signer_timeout,
        )
        .await?;
        for signer in &self.vault_signers[..threshold] {
            log_signature_grant(
                &mut self.ledger,
                "goldcoin_payout",
                &crate::goldcoin::hex::encode(&signer.public_key()),
                request_id,
                now,
            );
        }

        let commitment_hash: [u8; 32] = Sha256::digest(tx.serialize()).into();
        let unsigned_hex = crate::goldcoin::hex::encode(&tx.serialize());

        self.ledger
            .reserve_vault_utxos(request_id, &plan.inputs, now)?;
        self.ledger.record_goldcoin_payout_built(
            request_id,
            &plan,
            commitment_hash,
            &unsigned_hex,
            now,
        )?;

        for (input_index, input_partials) in partials.iter().enumerate() {
            let input_vault = &plan.input_contexts[input_index].vault;
            let sighash = tx.sighash_all(input_index, &input_vault.redeem_script());
            tx.inputs[input_index].script_sig =
                multisig::assemble(input_vault, &sighash, input_partials)?;
        }
        let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
        self.ledger
            .record_goldcoin_payout_signed(request_id, &signed_hex, now)?;

        match self.goldcoin_rpc.send_raw_transaction(&signed_hex).await? {
            BroadcastOutcome::Accepted { .. }
            | BroadcastOutcome::AlreadyInChain
            | BroadcastOutcome::AlreadyInMempool => {
                self.ledger
                    .record_goldcoin_payout_broadcast(request_id, tx.txid(), now)?;
                Ok(())
            }
            BroadcastOutcome::MissingInputs => {
                Err(OrchestratorError::PayoutBroadcastConflict(request_id))
            }
        }
    }

    async fn tick_goldcoin_payout_confirmations(&mut self, now: i64, report: &mut TickReport) {
        let Some((tip_height, _)) = self.ledger.goldcoin_chain_tip().unwrap_or(None) else {
            return; // no indexed Goldcoin tip yet
        };
        // Both the pre-threshold ('Broadcast' / request DestinationSubmitted)
        // AND the post-threshold ('Confirmed' / request DestinationConfirmed)
        // payouts: a payout must keep being checked against Goldcoin RPC
        // until it is actually Settled, not merely until it first crosses
        // the required depth — otherwise its observed confirmation count
        // (goldcoin_payouts.confirmations and the operator-facing
        // bridge_requests.destination_confirmations) freezes at the
        // threshold value for however long the Solana completion leg takes,
        // and an operator can no longer tell a healthy deepening payout
        // from a stalled one. 'Completed' payouts (request Settled) drop
        // out, so this stays bounded by the unsettled backlog.
        let mut request_ids = Vec::new();
        for payout_state in ["Broadcast", "Confirmed"] {
            match self.ledger.goldcoin_payouts_in_state(payout_state) {
                Ok(ids) => request_ids.extend(ids),
                Err(e) => {
                    report
                        .errors
                        .push(format!("goldcoin_payouts_in_state({payout_state}): {e}"));
                    return;
                }
            }
        }
        for request_id in request_ids {
            let payout = match self.ledger.get_goldcoin_payout(request_id) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    report
                        .errors
                        .push(format!("payout request {request_id}: {e}"));
                    continue;
                }
            };
            let Some(txid) = payout.txid else { continue };
            let txid_hex = crate::goldcoin::hex::encode(&txid);
            let decoded = match self.goldcoin_rpc.get_raw_transaction(&txid_hex).await {
                Ok(d) => d,
                Err(e) => {
                    report
                        .errors
                        .push(format!("payout request {request_id}: {e}"));
                    continue;
                }
            };
            let confirmations = decoded.confirmations.unwrap_or(0);
            match self.ledger.update_goldcoin_payout_confirmations(
                request_id,
                confirmations,
                tip_height,
                self.config.required_goldcoin_confirmations,
                now,
            ) {
                // Count only the tick that actually fired the
                // Broadcast -> Confirmed transition — refreshing an
                // already-Confirmed payout's depth every tick must not
                // re-count it as a newly confirmed payout.
                Ok(transitioned) => {
                    if transitioned {
                        report.payouts_confirmed += 1;
                    }
                }
                Err(e) => report
                    .errors
                    .push(format!("payout request {request_id}: {e}")),
            }
        }
    }

    // -------------------------------------------- SolToGlc: completion --

    async fn tick_goldcoin_completions(&mut self, now: i64, report: &mut TickReport) {
        let request_ids = match self.ledger.goldcoin_payouts_in_state("Confirmed") {
            Ok(ids) => ids,
            Err(e) => {
                report
                    .errors
                    .push(format!("goldcoin_payouts_in_state(Confirmed): {e}"));
                return;
            }
        };
        for request_id in request_ids {
            let already_submitted = match self.ledger.get_goldcoin_payout(request_id) {
                Ok(Some(p)) => p.onchain_completion_signature.is_some(),
                Ok(None) => continue,
                Err(e) => {
                    report
                        .errors
                        .push(format!("completion request {request_id}: {e}"));
                    continue;
                }
            };
            if already_submitted {
                continue;
            }
            match self.submit_completion(request_id, now).await {
                Ok(()) => report.completions_submitted += 1,
                Err(e) => report
                    .errors
                    .push(format!("completion request {request_id}: {e}")),
            }
        }
    }

    async fn submit_completion(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let mut sigs = Vec::with_capacity(self.config.attestation_threshold);
        let mut message = None;
        for signer in self
            .attestation_signers
            .iter()
            .take(self.config.attestation_threshold)
        {
            let (pubkey, signature, msg) = independently_attest_completion(
                signer.as_ref(),
                &self.ledger,
                &self.solana_rpc,
                request_id,
                self.config.signer_timeout,
            )
            .await?;
            if let Some(prev) = &message {
                if prev != &msg {
                    return Err(OrchestratorError::InconsistentAttestation(request_id));
                }
            } else {
                message = Some(msg);
            }
            log_signature_grant(
                &mut self.ledger,
                "attestation",
                &pubkey.to_string(),
                request_id,
                now,
            );
            sigs.push((pubkey, signature));
        }
        let message = message.ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        self.ledger
            .record_attestation(request_id, "completion", &message, now)?;

        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        let obligation_index = request
            .source_obligation_index
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let payout = self
            .ledger
            .get_goldcoin_payout(request_id)?
            .ok_or(LedgerError::PayoutNotFound(request_id))?;
        let payout_txid = payout
            .txid
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let payout_height = payout
            .mined_height
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;

        let key_set = attestation::fetch_attestation_key_set(&self.solana_rpc).await?;

        let proof_ix = ed25519::build_attestation_proof(&sigs, &message);
        let completion_ix = instructions::record_goldcoin_completion(
            &self.submitter.pubkey(),
            obligation_index,
            payout_txid,
            payout_height as u64,
            payout.payout_atomic,
            key_set.epoch,
        );
        let blockhash = self.solana_rpc.get_latest_blockhash().await?;
        let tx = SolanaTransaction::new_signed_with_payer(
            &[proof_ix, completion_ix],
            Some(&self.submitter.pubkey()),
            &[&self.submitter],
            blockhash,
        );
        let signature = self.solana_rpc.send_transaction(&tx).await?;
        self.ledger.record_goldcoin_completion_submitted(
            request_id,
            signature_bytes(&signature),
            now,
        )?;
        Ok(())
    }

    async fn tick_goldcoin_completion_confirmations(&mut self, now: i64, report: &mut TickReport) {
        let request_ids = match self.ledger.goldcoin_payouts_in_state("Confirmed") {
            Ok(ids) => ids,
            Err(e) => {
                report
                    .errors
                    .push(format!("goldcoin_payouts_in_state(Confirmed): {e}"));
                return;
            }
        };
        for request_id in request_ids {
            let payout = match self.ledger.get_goldcoin_payout(request_id) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    report
                        .errors
                        .push(format!("completion request {request_id}: {e}"));
                    continue;
                }
            };
            let Some(sig_bytes) = payout.onchain_completion_signature else {
                continue;
            };
            let signature = Signature::from(sig_bytes);
            match self.solana_rpc.get_signature_status(&signature).await {
                Ok(Some(Ok(()))) => match self
                    .ledger
                    .mark_goldcoin_completion_confirmed(request_id, now)
                {
                    Ok(()) => report.completions_confirmed += 1,
                    Err(e) => report
                        .errors
                        .push(format!("completion request {request_id}: {e}")),
                },
                Ok(Some(Err(reason))) => {
                    // The tracked transaction landed and FAILED. The usual
                    // cause after a re-submission is a benign race: an
                    // earlier attempt already completed the obligation, so
                    // this one was rejected as a duplicate ("completion is
                    // terminal and irreversible"). The obligation's own
                    // terminal status is the ground truth — settle on it if
                    // it says Completed, and only report an error otherwise.
                    match self.obligation_completed_onchain(request_id).await {
                        Ok(true) => match self
                            .ledger
                            .mark_goldcoin_completion_confirmed(request_id, now)
                        {
                            Ok(()) => report.completions_confirmed += 1,
                            Err(e) => report
                                .errors
                                .push(format!("completion request {request_id}: {e}")),
                        },
                        Ok(false) => report.errors.push(format!(
                            "completion request {request_id} REJECTED on chain: {reason}"
                        )),
                        Err(e) => report.errors.push(format!(
                            "completion request {request_id} REJECTED on chain ({reason}); \
                             obligation read-back also failed: {e}"
                        )),
                    }
                }
                Ok(None) => {
                    // Not observed yet. Within the grace window that just
                    // means "in flight; check again next tick". PAST the
                    // window, `None` can no longer mean in-flight — a
                    // Solana blockhash expires in well under a minute, so
                    // the submission either (a) landed long enough ago to
                    // have aged out of the node's recent-signature-status
                    // cache (`get_signature_statuses` without
                    // `searchTransactionHistory` only covers recent
                    // slots), or (b) was dropped and can never land.
                    // Without this arm, either case left the request stuck
                    // in DestinationConfirmed FOREVER, silently: the
                    // recorded signature suppressed any re-submission and
                    // this poll answered `None` on every subsequent tick.
                    // Disambiguate via the obligation's terminal on-chain
                    // status (ADR-0030: read the postcondition back,
                    // never assume): Completed -> the completion landed,
                    // settle now; still Pending -> re-attest and re-submit
                    // with a fresh blockhash (replacing the tracked
                    // signature), through the exact same
                    // `submit_completion` path as the first attempt.
                    let submitted_at = payout.onchain_completion_submitted_at.unwrap_or(0);
                    if now - submitted_at < COMPLETION_RESUBMIT_AFTER_SECS {
                        continue; // still plausibly in flight; retried next tick
                    }
                    match self.obligation_completed_onchain(request_id).await {
                        Ok(true) => match self
                            .ledger
                            .mark_goldcoin_completion_confirmed(request_id, now)
                        {
                            Ok(()) => report.completions_confirmed += 1,
                            Err(e) => report
                                .errors
                                .push(format!("completion request {request_id}: {e}")),
                        },
                        Ok(false) => match self.submit_completion(request_id, now).await {
                            Ok(()) => report.completions_submitted += 1,
                            Err(e) => report.errors.push(format!(
                                "completion request {request_id} re-submission: {e}"
                            )),
                        },
                        Err(e) => report
                            .errors
                            .push(format!("completion request {request_id}: {e}")),
                    }
                }
                Err(e) => report
                    .errors
                    .push(format!("completion request {request_id}: {e}")),
            }
        }
    }

    /// Whether `request_id`'s `WithdrawalObligation` has reached its
    /// terminal `Completed` status on Solana — the chain's own record that
    /// a `record_goldcoin_completion` for this obligation executed
    /// (regardless of which submitted transaction carried it). This is the
    /// settlement witness of last resort when a completion signature is no
    /// longer observable via the status cache.
    async fn obligation_completed_onchain(
        &self,
        request_id: i64,
    ) -> Result<bool, OrchestratorError> {
        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        let obligation_index = request
            .source_obligation_index
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let obligation_pda = accounts::withdrawal_obligation_pda(obligation_index);
        let account = self.solana_rpc.get_account(&obligation_pda).await?.ok_or(
            OrchestratorError::SolanaRpc(SolanaRpcError::Malformed(format!(
                "withdrawal obligation {obligation_index} account missing"
            ))),
        )?;
        let obligation = accounts::decode_withdrawal_obligation(&account.data)?;
        Ok(obligation.status == accounts::WITHDRAWAL_STATUS_COMPLETED)
    }
}

/// How long a submitted `record_goldcoin_completion` transaction may go
/// unobserved (`get_signature_status` = `None`) before the orchestrator
/// stops treating it as in-flight and actively recovers (read the
/// obligation's terminal status back; re-submit if still Pending). A
/// Solana blockhash expires after ~60–90 seconds, so five minutes is far
/// past any window in which the original transaction could still land —
/// generous enough to never race a merely slow finalization, small enough
/// that a dropped completion self-heals within minutes instead of
/// stalling settlement indefinitely.
const COMPLETION_RESUBMIT_AFTER_SECS: i64 = 300;

fn signature_bytes(signature: &Signature) -> [u8; 64] {
    signature.as_ref().try_into().unwrap()
}

/// Best-effort identity-only audit entry (docs/06-schema.md's
/// `signature_grant_log` — never key material). Deliberately never
/// propagates its own failure into the settlement path it's logging: an
/// audit-trail write must not be able to block a release/payout.
fn log_signature_grant(
    ledger: &mut Ledger,
    action_type: &str,
    identity: &str,
    request_id: i64,
    now: i64,
) {
    if let Err(e) =
        ledger.record_signature_grant(action_type, identity, Some(request_id), "info", now)
    {
        tracing::warn!(error = %e, action_type, identity, request_id, "failed to record signature_grant_log entry (non-fatal, audit trail only)");
    }
}

#[cfg(test)]
pub(crate) mod tests;
