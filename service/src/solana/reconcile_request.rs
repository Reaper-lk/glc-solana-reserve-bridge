//! **Chain-terminal reconciliation of a Solana-side leg** — the Solana
//! twin of `robinhood::reconcile_settlement` (#106), for the two shapes
//! found on 2026-09-13:
//!
//! - **Completion landed** (`SolToGlc`, request `DestinationConfirmed`):
//!   the Goldcoin payout confirmed and `complete_goldcoin_payout` DID
//!   execute (obligation `Completed`, carrying this request's payout
//!   txid), but the confirmation poll never recorded it — and kept
//!   re-sending (requests 4119: 123 failed `ObligationAlreadyCompleted`
//!   attempts; 4256: 69).
//! - **Release landed** (`RhnToSol`, request `DestinationSubmitted`):
//!   `release_from_reserve` finalized and paid the recipient, but the
//!   signature aged out of the node's status cache before the poll saw
//!   it, and nothing ever consulted the chain (request 4105).
//!
//! [`prove`] re-reads everything (ledger + chain, at the RPC's finalized
//! commitment) and yields `SAFE_TO_RECONCILE` / `ALREADY_RECONCILED` /
//! `REFUSE: <first mismatch>`. [`apply`] runs the SAME bookkeeping the
//! normal poll would have run (`Ledger::goldcoin_completion_confirmed_in`
//! / `release_confirmed_in`) with the state-log reason
//! `chain_terminal_reconciliation`. It sends nothing, retries nothing,
//! touches no other request, no route, no threshold.
//!
//! Nothing is operator-supplied: the shape is chosen by the request's own
//! route and state, and every fact is cross-checked between the ledger
//! and the chain.

use solana_sdk::pubkey::Pubkey;

use crate::amount_conversion::{self, CanonicalAtomic};
use crate::goldcoin::hex;
use crate::ledger::{Direction, Ledger, LedgerError, ReconcileOutcome, RequestState};

use super::accounts;
use super::rpc::{SolanaRpc, SolanaRpcError};

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("chain read: {0}")]
    Rpc(#[from] SolanaRpcError),
}

/// Which local bookkeeping the chain proves is owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `SolToGlc` `DestinationConfirmed` whose obligation is `Completed`
    /// on chain with this request's payout recorded.
    CompletionLanded,
    /// `RhnToSol` `DestinationSubmitted` whose release finalized and whose
    /// claim PDA exists with this request's amount and recipient.
    ReleaseLanded,
}

/// Everything [`apply`] needs; assembled only by [`prove`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub request_id: i64,
    pub shape: Shape,
    pub direction: Direction,
    /// `CompletionLanded`: the Solana obligation index. `ReleaseLanded`:
    /// the claim PDA's `(txid, vout)` identity is in `evidence`.
    pub obligation_index: Option<u64>,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    SafeToReconcile(Proof),
    AlreadyReconciled,
    Refuse(String),
}

#[derive(Debug, Clone)]
pub struct Report {
    pub request_id: i64,
    pub route: Option<Direction>,
    pub shape: Option<Shape>,
    pub local_state: String,
    pub chain_state: String,
    pub chain_evidence: String,
    pub destination: String,
    pub refunds: String,
    pub duplicates: String,
    pub verdict: Verdict,
}

impl Report {
    pub fn render(&self) -> String {
        let mut o = String::new();
        o.push_str(&format!("request: {}\n", self.request_id));
        o.push_str(&format!(
            "route: {}\n",
            self.route.map(|r| r.as_str()).unwrap_or("-")
        ));
        o.push_str(&format!(
            "shape: {}\n",
            match self.shape {
                Some(Shape::CompletionLanded) => "completion_landed (SolToGlc)",
                Some(Shape::ReleaseLanded) => "release_landed (RhnToSol)",
                None => "-",
            }
        ));
        o.push_str(&format!("local request state: {}\n", self.local_state));
        o.push_str(&format!("chain state: {}\n", self.chain_state));
        o.push_str(&format!("chain evidence: {}\n", self.chain_evidence));
        o.push_str(&format!("destination: {}\n", self.destination));
        o.push_str(&format!("refunds: {}\n", self.refunds));
        o.push_str(&format!("duplicates: {}\n", self.duplicates));
        o.push_str("\nverdict:\n\n");
        match &self.verdict {
            Verdict::SafeToReconcile(p) => {
                o.push_str(&format!("SAFE_TO_RECONCILE ({})\n", p.evidence))
            }
            Verdict::AlreadyReconciled => o.push_str("ALREADY_RECONCILED\n"),
            Verdict::Refuse(r) => o.push_str(&format!("REFUSE: {r}\n")),
        }
        o
    }
}

/// Builds the proof. Read-only. `&mut Ledger` for Send-ness only.
pub async fn prove<R: SolanaRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    required_goldcoin_confirmations: i64,
    request_id: i64,
) -> Result<Report, ReconcileError> {
    let mut report = Report {
        request_id,
        route: None,
        shape: None,
        local_state: "-".into(),
        chain_state: "-".into(),
        chain_evidence: "-".into(),
        destination: "-".into(),
        refunds: "-".into(),
        duplicates: "-".into(),
        verdict: Verdict::Refuse("not evaluated".into()),
    };
    macro_rules! refuse {
        ($($arg:tt)*) => {{
            report.verdict = Verdict::Refuse(format!($($arg)*));
            return Ok(report);
        }};
    }
    let Some(request) = ledger.get_request(request_id)? else {
        refuse!("request {request_id} does not exist");
    };
    report.route = Some(request.direction);
    report.local_state = request.state.as_str().to_string();

    // Conflicts first — a refund lifecycle or a closure rules out any
    // reconciliation regardless of shape.
    let mut conflicts = Vec::new();
    if ledger.get_goldcoin_refund(request_id)?.is_some() {
        conflicts.push("goldcoin refund");
    }
    if ledger.get_solana_refund(request_id)?.is_some() {
        conflicts.push("solana refund");
    }
    if ledger.request_closure(request_id)?.is_some() {
        conflicts.push("request closure");
    }
    if matches!(
        request.state,
        RequestState::RefundPending | RequestState::RefundBroadcast | RequestState::Refunded
    ) {
        conflicts.push("refund lifecycle state");
    }
    report.refunds = if conflicts.is_empty() {
        "none".into()
    } else {
        conflicts.join(", ")
    };
    if !conflicts.is_empty() {
        refuse!("conflicting financial outcome: {}", conflicts.join(", "));
    }

    // The bridge config + mint decimals, for exact amount comparison.
    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await?
        .ok_or_else(|| SolanaRpcError::Malformed("bridge_config does not exist".into()))?;
    let config = accounts::decode_bridge_config(&config_account.data)?;
    let decimals = accounts::fetch_reserve_mint_decimals(rpc, &config.reserve_token_mint).await?;

    match request.direction {
        // ------------------------------------------------ completion --
        Direction::SolToGlc => {
            report.shape = Some(Shape::CompletionLanded);
            if request.state == RequestState::Settled {
                report.verdict = Verdict::AlreadyReconciled;
                return Ok(report);
            }
            if request.state != RequestState::DestinationConfirmed {
                refuse!(
                    "request is {} — only DestinationConfirmed is behind a chain-side Completed",
                    request.state.as_str()
                );
            }
            let Some(index) = request.source_obligation_index else {
                refuse!("request names no Solana obligation");
            };
            let Some(payout) = ledger.get_goldcoin_payout(request_id)? else {
                refuse!("no Goldcoin payout row exists for request {request_id}");
            };
            let Some(full) = ledger.get_goldcoin_payout_full(request_id)? else {
                refuse!("no Goldcoin payout row exists for request {request_id}");
            };
            let Some(payout_txid) = payout.txid else {
                refuse!("the Goldcoin payout was never broadcast");
            };
            report.destination = format!(
                "goldcoin txid {} {} amount {} confirmations {}",
                hex::encode(&payout_txid),
                payout.state,
                payout.payout_atomic,
                payout.confirmations
            );
            if ledger.get_destination_txid(request_id)?.unwrap_or_default() != payout_txid.to_vec()
            {
                refuse!("the request's destination_txid is not the payout row's txid");
            }
            if payout.payout_atomic != request.net_destination_atomic {
                refuse!(
                    "payout amount {} != net destination amount {}",
                    payout.payout_atomic,
                    request.net_destination_atomic
                );
            }
            let dest_addr = String::from_utf8_lossy(&request.recipient)
                .trim_end_matches('\0')
                .to_string();
            match crate::goldcoin::address::base58check_decode(&dest_addr) {
                Ok((_, hash)) if hash == full.dest_p2pkh_hash => {}
                Ok(_) => refuse!("the payout destination is not the request's recipient"),
                Err(e) => refuse!("the request's recipient is not a base58check address: {e}"),
            }
            if payout.state != "Confirmed" && payout.state != "Completed" {
                refuse!("the Goldcoin payout is {} — not confirmed", payout.state);
            }
            if payout.confirmations < required_goldcoin_confirmations {
                refuse!(
                    "the Goldcoin payout has {} confirmations; {required_goldcoin_confirmations} \
                     required",
                    payout.confirmations
                );
            }
            let claimants = ledger.goldcoin_payout_requests_with_txid(&payout_txid)?;
            report.duplicates = format!("requests claiming this payout txid: {claimants:?}");
            if claimants != vec![request_id] {
                refuse!("Goldcoin txid is claimed by requests {claimants:?}");
            }

            // Chain: the obligation this request names, at finalized commitment.
            let pda = accounts::withdrawal_obligation_pda(index);
            let Some(account) = rpc.get_account(&pda).await? else {
                refuse!("obligation #{index} account {pda} does not exist");
            };
            let obligation = accounts::decode_withdrawal_obligation(&account.data)?;
            report.chain_state = accounts::withdrawal_status_name(obligation.status).to_string();
            if obligation.index != index {
                refuse!(
                    "account {pda} holds obligation #{} not #{index}",
                    obligation.index
                );
            }
            if obligation.status != accounts::WITHDRAWAL_STATUS_COMPLETED {
                refuse!(
                    "obligation #{index} is {} on chain, not Completed",
                    accounts::withdrawal_status_name(obligation.status)
                );
            }
            let Some((chain_txid, chain_height)) =
                accounts::decode_obligation_payout_record(&account.data)?
            else {
                refuse!("obligation #{index} is Completed but carries no payout record");
            };
            report.chain_evidence = format!(
                "obligation #{index} Completed, payout record txid {} height {chain_height}",
                hex::encode(&chain_txid)
            );
            if chain_txid != payout_txid {
                refuse!(
                    "the obligation's recorded payout txid {} is not this request's payout {}",
                    hex::encode(&chain_txid),
                    hex::encode(&payout_txid)
                );
            }
            match request.requester {
                Some(r) if obligation.requester.to_bytes() == r => {}
                Some(_) => refuse!("the obligation's requester is not the request's requester"),
                None => refuse!("the request records no requester"),
            }
            if obligation.glc_address != request.recipient {
                refuse!("the obligation's Goldcoin destination is not the request's recipient");
            }
            let expected_gross = CanonicalAtomic(request.gross_amount_atomic)
                .to_solana(decimals)
                .map_err(|e| SolanaRpcError::Malformed(format!("gross does not narrow: {e}")))?;
            if obligation.amount != expected_gross.0 {
                refuse!(
                    "the obligation amount {} != the request's gross {} (mint units)",
                    obligation.amount,
                    expected_gross.0
                );
            }
            report.verdict = Verdict::SafeToReconcile(Proof {
                request_id,
                shape: Shape::CompletionLanded,
                direction: request.direction,
                obligation_index: Some(index),
                evidence: format!(
                    "obligation #{index} Completed with payout {} at height {chain_height}",
                    hex::encode(&chain_txid)
                ),
            });
            Ok(report)
        }
        // --------------------------------------------------- release --
        Direction::RhnToSol => {
            report.shape = Some(Shape::ReleaseLanded);
            if matches!(
                request.state,
                RequestState::DestinationConfirmed | RequestState::Settled
            ) {
                report.verdict = Verdict::AlreadyReconciled;
                return Ok(report);
            }
            if request.state != RequestState::DestinationSubmitted {
                refuse!(
                    "request is {} — only DestinationSubmitted is behind a finalized release",
                    request.state.as_str()
                );
            }
            let Some(sig) = ledger
                .get_destination_txid(request_id)?
                .and_then(|v| <[u8; 64]>::try_from(v).ok())
            else {
                refuse!("the request carries no Solana release signature");
            };
            let signature = solana_sdk::signature::Signature::from(sig);
            let (Some(txid), Some(vout)) = (request.source_txid, request.source_vout) else {
                refuse!("the request records no (source txid, vout) — the release claim identity");
            };
            let recipient = match Pubkey::try_from(request.recipient.as_slice()) {
                Ok(p) => p,
                Err(_) => refuse!("the request's recipient is not a Solana pubkey"),
            };
            report.destination = format!("solana release {signature} to {recipient}");
            let fee = amount_conversion::verify_fee_breakdown(
                request.gross_amount_atomic,
                request.fee_bps,
                request.fee_amount_atomic,
                request.net_amount_atomic,
            )
            .map_err(|e| SolanaRpcError::Malformed(format!("fee breakdown: {e}")))?;
            let expected_net = fee
                .net
                .to_solana(decimals)
                .map_err(|e| SolanaRpcError::Malformed(format!("net does not narrow: {e}")))?;

            // Chain 1: the signature is FINALIZED and succeeded (the RPC
            // answers `Some` only at finalized commitment).
            match rpc.get_signature_status(&signature).await? {
                Some(Ok(())) => {}
                Some(Err(e)) => refuse!("the release transaction FAILED on chain: {e}"),
                None => {
                    // Not in the status cache: the claim PDA decides below —
                    // a finalized release always leaves it behind.
                }
            }
            // Chain 2: the claim PDA — exactly one release for (txid, vout),
            // for exactly this amount to exactly this recipient.
            let pda = accounts::deposit_claim_pda(&txid, vout);
            let Some(account) = rpc.get_account(&pda).await? else {
                refuse!(
                    "no release claim exists for (txid {}, vout {vout}) at {pda}: the release \
                     has not executed",
                    hex::encode(&txid)
                );
            };
            let claim = accounts::decode_deposit_claim(&account.data)?;
            report.chain_state = "claim present (release executed exactly once)".into();
            report.chain_evidence = format!(
                "claim {pda}: amount {} recipient {} epoch {} slot {}",
                claim.amount, claim.recipient, claim.attestation_epoch, claim.slot_created
            );
            if claim.txid != txid || claim.vout != vout {
                refuse!("the claim account's identity is not this request's (txid, vout)");
            }
            if claim.recipient != recipient {
                refuse!(
                    "the release paid {} — not the request's recipient {recipient}",
                    claim.recipient
                );
            }
            if claim.amount != expected_net.0 {
                refuse!(
                    "the release amount {} != the request's net {} (mint units)",
                    claim.amount,
                    expected_net.0
                );
            }
            report.duplicates =
                "one claim PDA per (txid, vout): a second release is impossible on chain".into();
            report.verdict = Verdict::SafeToReconcile(Proof {
                request_id,
                shape: Shape::ReleaseLanded,
                direction: request.direction,
                obligation_index: request.source_obligation_index,
                evidence: format!(
                    "release {signature} finalized; claim {pda} amount {} to {recipient}",
                    claim.amount
                ),
            });
            Ok(report)
        }
        other => refuse!(
            "{} has no Solana-side reconciliation shape (SolToGlc completion or RhnToSol release)",
            other.as_str()
        ),
    }
}

/// The write — only ever with a proof [`prove`] produced.
pub fn apply(
    ledger: &mut Ledger,
    proof: &Proof,
    actor: &str,
    now: i64,
) -> Result<ReconcileOutcome, LedgerError> {
    match proof.shape {
        Shape::CompletionLanded => {
            ledger.reconcile_goldcoin_completion_from_chain(proof.request_id, actor, now)
        }
        Shape::ReleaseLanded => ledger.reconcile_release_from_chain(proof.request_id, actor, now),
    }
}

#[cfg(test)]
mod tests;
