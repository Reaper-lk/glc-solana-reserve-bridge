//! ManualReview -> Goldcoin L1 settlement recovery: the Solana-side
//! proof that the original deposit is real, unspent by any settlement,
//! and exactly what the ledger says it is — plus the operator-facing dry
//! run and execute wrappers.
//!
//! # What this module is NOT
//!
//! It is not a settlement implementation. Recovery re-admits the request
//! into the EXISTING Goldcoin payout pipeline by transitioning it
//! `ManualReview -> SourceFinalized`; `Orchestrator::tick_goldcoin_payouts`
//! then carries it through the same build/sign/broadcast/confirm path
//! every other SolToGlc request uses. There is deliberately no second
//! payout implementation here.
//!
//! Nor does it re-implement the eligibility rules. Every ledger-side
//! check — state, reason whitelist, refund-lifecycle exclusion, existing
//! payout, the unconditional rate-limit re-checks, the UTXO-liquidity
//! floor, the confirmed-liquidity admission safety buffer and the reserve
//! invariant — lives in [`Ledger::resume_manual_review_sol_to_glc`] and is
//! reached here only by calling it (for real) or trialling it and rolling
//! back (for the dry run, [`Ledger::dry_run_resume_manual_review`]).
//!
//! That list is deliberately not maintained by hand anywhere that could
//! drift: because the dry run TRIALS the real function rather than
//! previewing a copy of its guards, a gate added to the resume path is
//! picked up here automatically. The admission safety buffer added on
//! 2026-09-02 arrived exactly that way — it gates recovery today without
//! this module having been changed for it. What this module does owe an
//! operator is visibility, so [`SettleContext`] reports the buffer
//! alongside the capacity figures it already showed.
//!
//! # What this module adds
//!
//! One thing the ledger cannot know on its own: whether the original
//! on-chain deposit still says what the database says it says. The
//! ledger trusts `source_finalized_at`, written by the indexer from a
//! `finalized` read at fold time. This module re-reads the
//! `WithdrawalObligation` NOW, at `finalized`, and refuses unless it
//! exists, is still `Pending`, and its `requester`, `amount` AND
//! `glc_address` match the stored row exactly. Fail-closed: an
//! unreachable RPC is a refusal, not an assumption.
//!
//! The destination is checked for the same reason as the other two, and
//! matters most: `bridge_requests.recipient` is a copy the indexer made
//! of `WithdrawalObligation.glc_address` at fold time, and it is the
//! field that decides who receives the Goldcoin. Verifying the depositor
//! and the amount while trusting the stored destination would leave the
//! one value that directs the money outside the proof.
//!
//! # What the chain can no longer tell us
//!
//! `status == Pending` means no COMPLETION has been recorded. Since the
//! 2026-09-02 reserve-withdrawal hardening it no longer means the deposit
//! is untouched: `refund_withdraw` returns the depositor's funds without
//! taking the obligation as `mut`, so a refunded deposit stays `Pending`
//! on chain forever (`complete_goldcoin_payout` is the only writer of
//! `status`). The defense against paying a refunded deposit a second time
//! is therefore entirely database-side — the `solana_refunds` ROW check
//! in [`Ledger::resume_manual_review_sol_to_glc`], which refuses on a row
//! in ANY state and is written before the refund transaction is ever
//! broadcast. That ordering is what makes the row, not the chain, the
//! reliable witness here. A refund executed wholly out of band, leaving
//! no row, would defeat it; that requires the same attestation quorum
//! that could move reserve funds directly, so it is a stated assumption
//! of this module rather than a gap it can close.
//!
//! # Discovery answers with the same predicate it enforces
//!
//! [`list_candidates`] does not decide eligibility for itself. It applies
//! only the structural membership test ([`candidate_ids`]: SolToGlc,
//! `ManualReview`, a reason on the shared
//! [`Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS`] list, no refund row) and
//! then reports, for each candidate, the verdict from the SAME
//! rolled-back trial [`dry_run_settle`] uses. A listing that says
//! "recoverable" and a dry run that says "WOULD RE-ADMIT" are therefore
//! the same computation, not two policies that agree by maintenance.
//!
//! They were two policies once, and they disagreed: the listing filtered
//! on a hand-maintained reason list that never received
//! `liquidity_buffer_low_at_fold` when the admission safety buffer added
//! it to the resume path, so three parked production requests reported as
//! ELIGIBLE by `manual-review-settle` were absent from
//! `manual-review-settle-list` entirely. Both surfaces now read one list
//! through one predicate, pinned in both directions by
//! `ledger::tests::resume_acceptance_matches_the_recoverable_reason_list`.
//!
//! Discovery also never applies an admission-time gate the recovery path
//! itself does not — see [`candidate_ids`] on why the rolling-24h
//! rate-limit ACCESSORS, which answer the admission question rather than
//! the recovery one, must not be used to filter candidates.
//!
//! # Custody
//!
//! Re-admission moves no funds and signs nothing — it is a ledger
//! transition. No keypair and no signer is loaded anywhere in this
//! module. The funds movement happens later, in the normal payout
//! pipeline, under the vault signers it already uses.

use solana_sdk::pubkey::Pubkey;

use crate::admin_api::audited_resume_manual_review;
use crate::amount_conversion::CanonicalAtomic;
use crate::ledger::{
    BridgeRequest, Direction, Ledger, ReserveDirection, ResumeDryRunOutcome,
    ResumeManualReviewOutcome,
};
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

/// `WithdrawalStatus::Pending` — the only status a recoverable deposit
/// may have. `Broadcast`/`Completed` are settlement evidence and refuse.
const WITHDRAWAL_STATUS_PENDING: u8 = 0;

/// The on-chain facts, verified against the stored request.
#[derive(Debug, Clone)]
pub struct ObligationVerification {
    pub obligation_index: u64,
    pub obligation_pda: Pubkey,
    /// The depositor, from the obligation — verified equal to the stored
    /// `bridge_requests.requester`.
    pub requester: Pubkey,
    pub status: u8,
    /// The deposited amount on chain, in the mint's native units.
    pub onchain_amount: u64,
    /// The Goldcoin destination the depositor committed to ON CHAIN
    /// (`WithdrawalObligation.glc_address`, already trimmed to
    /// `glc_address_len` by the decoder) — verified equal to the stored
    /// `bridge_requests.recipient`.
    pub onchain_glc_address: Vec<u8>,
    /// The stored canonical gross narrowed to the live mint decimals —
    /// verified equal to `onchain_amount`.
    pub expected_amount: u64,
    pub mint_decimals: u8,
    pub reserve_mint: Pubkey,
}

/// Re-reads the original deposit at `finalized` and proves it still
/// matches the stored request. Read-only; every failure is a refusal.
pub async fn verify_obligation<R: SolanaRpc>(
    rpc: &R,
    request: &BridgeRequest,
) -> Result<ObligationVerification, String> {
    if request.direction != Direction::SolToGlc {
        return Err(format!(
            "request {} is {:?}, not SolToGlc",
            request.id, request.direction
        ));
    }
    let obligation_index = request
        .source_obligation_index
        .ok_or_else(|| format!("request {} has no source_obligation_index", request.id))?;
    let stored_requester = request
        .requester
        .ok_or_else(|| format!("request {} has no requester recorded", request.id))?;

    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config does not exist on this cluster")?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
    if config.reserve_token_mint == Pubkey::default() {
        return Err("reserve vault is not configured on this deployment".to_string());
    }

    let obligation_pda = accounts::withdrawal_obligation_pda(obligation_index);
    let obligation_account = rpc
        .get_account(&obligation_pda)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "REFUSING — withdrawal obligation #{obligation_index} does not exist at \
                 {obligation_pda}; the original deposit cannot be proven"
            )
        })?;
    let obligation = accounts::decode_withdrawal_obligation(&obligation_account.data)
        .map_err(|e| e.to_string())?;

    if obligation.index != obligation_index {
        return Err(format!(
            "REFUSING — obligation PDA {obligation_pda} decodes to index {}, expected \
             {obligation_index}",
            obligation.index
        ));
    }
    if obligation.requester.to_bytes() != stored_requester {
        return Err(format!(
            "REFUSING — stored requester does not match the on-chain obligation's requester \
             ({}); the database and chain disagree about the original depositor",
            obligation.requester
        ));
    }
    if obligation.status != WITHDRAWAL_STATUS_PENDING {
        return Err(format!(
            "REFUSING — on-chain obligation #{obligation_index} status is {} (not Pending): \
             settlement evidence exists on chain",
            obligation.status
        ));
    }

    let mint_decimals = accounts::fetch_reserve_mint_decimals(rpc, &config.reserve_token_mint)
        .await
        .map_err(|e| e.to_string())?;
    let expected = CanonicalAtomic(request.gross_amount_atomic)
        .to_solana(mint_decimals)
        .map_err(|e| format!("stored canonical gross does not narrow exactly: {e}"))?;
    if expected.0 != obligation.amount {
        return Err(format!(
            "REFUSING — stored gross ({} canonical -> {} native) does not equal the on-chain \
             deposited amount ({} native)",
            request.gross_amount_atomic, expected.0, obligation.amount
        ));
    }

    // The destination is the field that decides WHO is paid, so it is
    // verified against the chain exactly like the depositor and the
    // amount are. `bridge_requests.recipient` is itself a copy of this
    // very field, taken by the indexer at fold time
    // (`fold_sol_deposit(.., &snap.glc_address, ..)`), so the two must be
    // byte-identical; any divergence means the database no longer
    // describes the deposit it claims to, and re-admitting would pay the
    // wrong Goldcoin address with funds that cannot be recalled.
    //
    // An empty on-chain address is refused rather than treated as a
    // wildcard: `record_goldcoin_completion` requires
    // `1 <= glc_address_len <= 64`, so a zero-length destination could
    // never be completed on chain even if the payout were made.
    if obligation.glc_address.is_empty() {
        return Err(format!(
            "REFUSING — on-chain obligation #{obligation_index} carries an empty Goldcoin \
             destination; the deposit names no payable address"
        ));
    }
    if obligation.glc_address != request.recipient {
        return Err(format!(
            "REFUSING — stored recipient does not match the on-chain obligation's Goldcoin \
             destination for #{obligation_index}; the database and chain disagree about where \
             this deposit is payable (stored {} bytes, on-chain {} bytes)",
            request.recipient.len(),
            obligation.glc_address.len()
        ));
    }

    Ok(ObligationVerification {
        obligation_index,
        obligation_pda,
        requester: obligation.requester,
        status: obligation.status,
        onchain_amount: obligation.amount,
        onchain_glc_address: obligation.glc_address.clone(),
        expected_amount: expected.0,
        mint_decimals,
        reserve_mint: config.reserve_token_mint,
    })
}

/// Read-only reserve/liquidity context for the operator's report. Every
/// value comes from an existing accessor; nothing here gates anything —
/// the gate is the ledger trial.
#[derive(Debug, Clone)]
pub struct SettleContext {
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub reserved_liquidity: u64,
    pub pending_obligations: u64,
    pub available_capacity: i64,
    pub net_destination_atomic: u64,
    pub available_utxo_count: u32,
    pub mature_available_atomic: u64,
    pub recipient_rate_limited_until: Option<i64>,
    pub source_wallet_rate_limited_until: Option<i64>,
    /// The confirmed-liquidity admission safety buffer (added 2026-09-02),
    /// in the Goldcoin reserve's atomic units; `0` means the feature is
    /// disabled on this deployment.
    pub admission_buffer_atomic: i64,
    /// Confirmed unreserved headroom the buffer requires before admission
    /// reopens, in the same units.
    pub admission_reopen_atomic: i64,
    /// Whether that gate is currently CLOSED. Reported because a closed
    /// gate is the one refusal an operator cannot infer from the capacity
    /// figures above — headroom can look ample while the gate still
    /// blocks, since it reopens only on a genuine recovery to
    /// `admission_reopen_atomic`, never on a single reading back over the
    /// line. Informational only: the authority remains the ledger trial.
    pub liquidity_admission_closed: bool,
}

fn settle_context(
    ledger: &Ledger,
    request: &BridgeRequest,
    now: i64,
) -> Result<SettleContext, String> {
    let (total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations) =
        ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .map_err(|e| e.to_string())?;
    let available_capacity = ledger
        .available_capacity(ReserveDirection::GoldcoinReserve)
        .map_err(|e| e.to_string())?;
    let pool = ledger.utxo_pool_health(now).map_err(|e| e.to_string())?;
    let recipient_rate_limited_until = ledger
        .sol_to_glc_recipient_rate_limited_until(&request.recipient, now)
        .map_err(|e| e.to_string())?;
    let source_wallet_rate_limited_until = match request.requester {
        Some(w) => ledger
            .sol_to_glc_source_wallet_rate_limited_until(&w, now)
            .map_err(|e| e.to_string())?,
        None => None,
    };
    let (admission_buffer_atomic, admission_reopen_atomic) = ledger
        .admission_liquidity_thresholds(ReserveDirection::GoldcoinReserve)
        .map_err(|e| e.to_string())?;
    let liquidity_admission_closed = ledger
        .is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)
        .map_err(|e| e.to_string())?;
    Ok(SettleContext {
        total_reserve_balance,
        protected_minimum,
        reserved_liquidity,
        pending_obligations,
        available_capacity,
        net_destination_atomic: request.net_destination_atomic,
        available_utxo_count: pool.available_utxo_count,
        mature_available_atomic: pool.mature_available_atomic,
        recipient_rate_limited_until,
        source_wallet_rate_limited_until,
        admission_buffer_atomic,
        admission_reopen_atomic,
        liquidity_admission_closed,
    })
}

/// The full dry run: chain proof + the rolled-back ledger trial + context.
#[derive(Debug)]
pub struct SettleDryRunReport {
    pub request: BridgeRequest,
    /// `Err` carries the fail-closed reason the deposit could not be
    /// proven; that is itself a refusal.
    pub chain: Result<ObligationVerification, String>,
    /// What an execute would do against current live state — determined
    /// by running the real function and rolling it back.
    pub ledger: ResumeDryRunOutcome,
    pub context: SettleContext,
    /// Both halves clear: an execute would re-admit the request.
    pub would_settle: bool,
}

/// Strictly read-only. Contacts no signer, loads no keypair, moves no
/// funds, and persists nothing — the ledger trial is rolled back.
pub async fn dry_run_settle<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    request_id: i64,
    now: i64,
) -> Result<SettleDryRunReport, String> {
    // Scoped so no `&Ledger` is alive across the chain read below
    // (`Ledger` is not `Sync`).
    let request = {
        ledger
            .get_request(request_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("bridge request {request_id} not found"))?
    };
    let chain = verify_obligation(rpc, &request).await;
    let ledger_outcome = ledger
        .dry_run_resume_manual_review(request_id, now)
        .map_err(|e| e.to_string())?;
    let context = settle_context(ledger, &request, now)?;
    let would_settle = chain.is_ok() && matches!(ledger_outcome, ResumeDryRunOutcome::WouldResume);
    Ok(SettleDryRunReport {
        request,
        chain,
        ledger: ledger_outcome,
        context,
        would_settle,
    })
}

/// Guarded execution. Proves the deposit on chain FIRST, then performs
/// the atomic, audited re-admission, which independently re-runs every
/// ledger check under the write lock.
///
/// Ordering rationale: the chain proof is a read and cannot be made
/// atomic with the ledger transaction, so the ledger checks are the
/// authority on anything racy. The only chain fact that could change
/// between the two is the obligation's status, and that can only advance
/// via a Goldcoin completion — which requires a `goldcoin_payouts` row,
/// which the ledger transaction refuses on. The window is closed.
pub async fn execute_settle<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
) -> Result<ResumeManualReviewOutcome, String> {
    let request = {
        ledger
            .get_request(request_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("bridge request {request_id} not found"))?
    };
    verify_obligation(rpc, &request).await?;
    let (outcome, _receipt) =
        audited_resume_manual_review(ledger, request_id, note, actor).map_err(|e| e.to_string())?;
    Ok(outcome)
}

/// One row of the recovery-candidate listing: the parked request, plus
/// the verdict an operator would get from running the real command on it.
#[derive(Debug)]
pub struct SettleCandidate {
    pub request: BridgeRequest,
    /// Produced by [`Ledger::dry_run_resume_manual_review`] — the SAME
    /// rolled-back trial of the real `resume_manual_review_sol_to_glc`
    /// that [`dry_run_settle`] reports, evaluated against the same live
    /// state. Never a preview of the rules; the rules themselves.
    pub ledger: ResumeDryRunOutcome,
}

impl SettleCandidate {
    /// The ledger half of the verdict clears. For the full answer an
    /// execute would give, the on-chain deposit proof must clear too —
    /// [`list_candidate_reports`] (or `manual-review-settle` on the
    /// single request) supplies that half.
    pub fn ledger_would_resume(&self) -> bool {
        matches!(self.ledger, ResumeDryRunOutcome::WouldResume)
    }
}

/// Which requests are recovery CANDIDATES at all — the structural
/// membership question, kept deliberately separate from the eligibility
/// question.
///
/// Membership is only: `SolToGlc`, currently `ManualReview`, a reason on
/// [`Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS`] (asked through
/// [`Ledger::is_recoverable_manual_review_reason`], the same predicate
/// `resume_manual_review_sol_to_glc` itself now uses), and no
/// `solana_refunds` row — the two permanent, non-self-clearing facts that
/// mean "this request is not on the recovery path at all". Both can only
/// ever HIDE a request that the resume path would also refuse; neither
/// can show one it would not.
///
/// Nothing time-varying is applied here, and in particular NONE of:
///
/// - the recipient rolling-24h window,
/// - the source-wallet rolling-24h window,
/// - the mature-UTXO floor, the reserve invariant, or the
///   confirmed-liquidity admission safety buffer.
///
/// Those are eligibility, not membership, and belong to the trial alone.
/// Filtering discovery on any of them would hide exactly the requests an
/// operator most needs to find. Two of them would also be plain wrong
/// here: [`Ledger::sol_to_glc_recipient_rate_limited_until`] and its
/// source-wallet twin answer the ADMISSION-time question ("may a brand
/// new deposit for these bytes be admitted?"), which counts the
/// candidate's own row and any row that arrived after it. The recovery
/// path deliberately asks a different question — may this ALREADY
/// ACCEPTED deposit proceed, blocked only by a STRICT PREDECESSOR — so a
/// parked request is routinely "rate limited" by the admission-time
/// accessor while being genuinely eligible for recovery. The report those
/// accessors feed ([`SettleContext`]) is informational for that reason
/// and gates nothing.
fn candidate_ids(ledger: &Ledger) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    for req in ledger
        .requests_by_state(
            Direction::SolToGlc,
            crate::ledger::RequestState::ManualReview,
        )
        .map_err(|e| e.to_string())?
    {
        if !Ledger::is_recoverable_manual_review_reason(req.manual_review_note.as_deref()) {
            continue;
        }
        if ledger
            .get_solana_refund(req.id)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            continue;
        }
        out.push(req.id);
    }
    out.sort_unstable();
    Ok(out)
}

/// Read-only candidate listing with the canonical ledger verdict for
/// each: every candidate ([`candidate_ids`]) trialled through the real
/// resume and rolled back, exactly as [`dry_run_settle`] does.
///
/// Returns candidates whose verdict REFUSES as well as those that would
/// resume, carrying the refusal reason. That is the point of the listing:
/// a request blocked today by a window that ages out, or by headroom that
/// recovers, is a request an operator needs to see now — it is a
/// candidate whose condition has not cleared yet, not a non-candidate.
/// Callers that want only the ready ones filter on
/// [`SettleCandidate::ledger_would_resume`].
///
/// Takes `&mut Ledger` because the trial briefly holds SQLite's write
/// lock before rolling back; nothing is persisted by this call.
pub fn list_candidates(ledger: &mut Ledger, now: i64) -> Result<Vec<SettleCandidate>, String> {
    let ids = candidate_ids(ledger)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let request = ledger
            .get_request(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("bridge request {id} disappeared during listing"))?;
        let ledger_outcome = ledger
            .dry_run_resume_manual_review(id, now)
            .map_err(|e| e.to_string())?;
        out.push(SettleCandidate {
            request,
            ledger: ledger_outcome,
        });
    }
    Ok(out)
}

/// The full-fidelity listing: [`dry_run_settle`] — chain proof included —
/// run for every candidate, so each row's `would_settle` is bit-for-bit
/// the verdict `manual-review-settle --request-id N` prints for that same
/// request at that same `now`. Costs a few `finalized` account reads per
/// candidate, which is why the ledger-only [`list_candidates`] still
/// exists for a quick, RPC-free look.
///
/// Strictly read-only, on both halves: the chain reads are reads, and the
/// ledger trials roll back.
pub async fn list_candidate_reports<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    now: i64,
) -> Result<Vec<SettleDryRunReport>, String> {
    let ids = candidate_ids(ledger)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(dry_run_settle(rpc, ledger, id, now).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
