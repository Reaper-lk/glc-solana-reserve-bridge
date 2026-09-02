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
//! floor and the reserve invariant — lives in
//! [`Ledger::resume_manual_review_sol_to_glc`] and is reached here only
//! by calling it (for real) or trialling it and rolling back (for the dry
//! run, [`Ledger::dry_run_resume_manual_review`]).
//!
//! # What this module adds
//!
//! One thing the ledger cannot know on its own: whether the original
//! on-chain deposit still says what the database says it says. The
//! ledger trusts `source_finalized_at`, written by the indexer from a
//! `finalized` read at fold time. This module re-reads the
//! `WithdrawalObligation` NOW, at `finalized`, and refuses unless it
//! exists, is still `Pending`, and its `requester` and `amount` match the
//! stored row exactly. Fail-closed: an unreachable RPC is a refusal, not
//! an assumption.
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

    Ok(ObligationVerification {
        obligation_index,
        obligation_pda,
        requester: obligation.requester,
        status: obligation.status,
        onchain_amount: obligation.amount,
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

/// Read-only candidate listing: `SolToGlc` requests parked in
/// `ManualReview` whose reason is on the recovery whitelist and which
/// have not entered a refund lifecycle. Purely informational — the
/// authority on eligibility is the dry run.
pub fn list_candidates(ledger: &Ledger) -> Result<Vec<BridgeRequest>, String> {
    let mut out = Vec::new();
    for req in ledger
        .requests_by_state(
            Direction::SolToGlc,
            crate::ledger::RequestState::ManualReview,
        )
        .map_err(|e| e.to_string())?
    {
        let whitelisted = req
            .manual_review_note
            .as_deref()
            .is_some_and(|r| Ledger::RECOVERABLE_MANUAL_REVIEW_REASONS.contains(&r));
        if !whitelisted {
            continue;
        }
        if ledger
            .get_solana_refund(req.id)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            continue;
        }
        out.push(req);
    }
    out.sort_by_key(|r| r.id);
    Ok(out)
}

#[cfg(test)]
mod tests;
