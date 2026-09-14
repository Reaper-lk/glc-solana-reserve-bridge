//! Returning a never-broadcast in-band Solana refund to `ManualReview`
//! (schema v36) — the LEDGER half of `glc-admin
//! refund-return-to-manual-review`.
//!
//! Requests 4140 and 4185 had `refund-manual-review` BEGUN on 2026-09-13:
//! a `solana_refunds` row was written and the request moved to
//! `RefundPending` — and then the on-chain program turned out not to
//! dispatch `refund_withdraw`, so the refund could never be built, let
//! alone broadcast. The row sits in `Pending` forever, and its mere
//! existence is what keeps the request out of the out-of-band workflow
//! (`Ledger::manual_solana_refund_eligibility` refuses any refund
//! lifecycle). This module is the ONE audited way to undo that begin:
//!
//! - [`Ledger::refund_return_verdict`] — read-only; every ledger-side
//!   guard, answered as a [`RefundReturnVerdict`].
//! - [`Ledger::return_refund_to_manual_review`] — re-runs the guards in
//!   its own write transaction, requires the chain proof the CLI obtained
//!   ([`RefundReturnChainProof`]: nothing landed under the refund's nonce,
//!   the obligation is still `Pending`), copies the `solana_refunds` row
//!   verbatim into `solana_refunds_retired` (with who/when/why), deletes
//!   it from `solana_refunds`, and moves the request `RefundPending ->
//!   ManualReview` with state-log reason `out_of_band_refund_recovery`.
//!   The request row's own columns — hold, disposition, park reason,
//!   requester, obligation — are untouched. Moves no funds.
//!
//! Only a lifecycle in the exact pre-broadcast shape qualifies: state
//! `Pending`, no signature, no blockhash, no broadcast/confirm time, and
//! a state log that never recorded `RefundBroadcast` or `Refunded`. A
//! lifecycle that ever reached the network ends through its own path
//! (`refund-manual-review`), never through this one.

use rusqlite::{Connection, OptionalExtension};

use super::{
    log_transition, row_to_request, row_to_solana_refund, write_tx, BridgeRequest, Ledger,
    LedgerError, RequestState, SolanaRefund, SolanaRefundState, SELECT_REQUEST,
    SELECT_SOLANA_REFUND,
};

/// The state-log reason (and `retire_reason`) every return writes.
pub const OUT_OF_BAND_REFUND_RECOVERY: &str = "out_of_band_refund_recovery";

/// The read-only answer to "may this request be returned?".
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum RefundReturnVerdict {
    /// Every ledger-side guard holds; the chain proof is still owed.
    SafeToReturn {
        request: BridgeRequest,
        refund: SolanaRefund,
    },
    /// Already returned by this operation (request `ManualReview`, the
    /// lifecycle retired) — a rerun writes nothing.
    AlreadyReturned {
        request: BridgeRequest,
        retired: RetiredSolanaRefund,
    },
    /// A guard failed; the reason.
    Refused(String),
}

/// A row of `solana_refunds_retired`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredSolanaRefund {
    pub id: i64,
    pub request_id: i64,
    pub obligation_index: u64,
    pub nonce: u64,
    pub amount_solana_atomic: u64,
    pub retired_at: i64,
    pub retired_by: String,
    pub retire_reason: String,
    pub retire_note: String,
}

/// What the chain said, read at `finalized`, immediately before the
/// return — constructed only by `solana::manual_refund::prove_refund_never_landed`
/// after every check passed. The ledger refuses to return without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundReturnChainProof {
    pub request_id: i64,
    pub nonce: u64,
    /// The refund nonce's `rebalance_withdrawal` PDA does not exist: no
    /// refund transaction ever landed under this nonce.
    pub nonce_pda_absent: bool,
    /// The deposit obligation is still `Pending` (not paid, not refunded).
    pub obligation_pending: bool,
    pub checked_at: i64,
}

/// Outcome of [`Ledger::return_refund_to_manual_review`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundReturnOutcome {
    Returned(RetiredSolanaRefund),
    /// Zero writes.
    AlreadyReturned(RetiredSolanaRefund),
}

impl Ledger {
    /// Read-only: every ledger-side guard for returning `request_id`.
    pub fn refund_return_verdict(
        &self,
        request_id: i64,
    ) -> Result<RefundReturnVerdict, LedgerError> {
        Self::refund_return_verdict_in(&self.conn, request_id)
    }

    fn refund_return_verdict_in(
        conn: &Connection,
        request_id: i64,
    ) -> Result<RefundReturnVerdict, LedgerError> {
        use RefundReturnVerdict::Refused;
        let Some(request) = conn
            .query_row(SELECT_REQUEST, [request_id], row_to_request)
            .optional()?
        else {
            return Ok(Refused(format!("request {request_id} does not exist")));
        };
        let refund: Option<SolanaRefund> = conn
            .query_row(
                &format!("{SELECT_SOLANA_REFUND} WHERE request_id = ?1"),
                [request_id],
                row_to_solana_refund,
            )
            .optional()?;
        let retired = Self::latest_retired_in(conn, request_id)?;

        // Idempotency first: already returned by this operation.
        if request.state == RequestState::ManualReview {
            if let (Some(retired), None) = (retired.clone(), &refund) {
                return Ok(RefundReturnVerdict::AlreadyReturned { request, retired });
            }
        }
        if request.state != RequestState::RefundPending {
            return Ok(Refused(format!(
                "state is {}, not RefundPending",
                request.state.as_str()
            )));
        }
        if !request.direction.source_is_solana() {
            return Ok(Refused(format!(
                "route {} is not Solana-sourced — not a Solana refund case",
                request.direction.as_str()
            )));
        }
        let Some(refund) = refund else {
            return Ok(Refused(
                "RefundPending but no solana_refunds row exists — inconsistent; investigate"
                    .to_string(),
            ));
        };
        if refund.state != SolanaRefundState::Pending {
            return Ok(Refused(format!(
                "refund lifecycle is {}, not Pending — a refund transaction was recorded; it \
                 ends through refund-manual-review, never through this operation",
                refund.state.as_str()
            )));
        }
        if refund.refund_signature.is_some()
            || refund.recent_blockhash.is_some()
            || refund.broadcast_at.is_some()
            || refund.confirmed_at.is_some()
        {
            return Ok(Refused(
                "refund row carries a signature/blockhash/broadcast/confirm marker — not the \
                 pre-broadcast shape"
                    .to_string(),
            ));
        }
        if request.requester != Some(refund.requester)
            || request.source_obligation_index != Some(refund.obligation_index)
        {
            return Ok(Refused(
                "refund row's requester/obligation disagree with the request — inconsistent; \
                 investigate"
                    .to_string(),
            ));
        }
        // The state log must never have seen the network.
        let seen_network: i64 = conn.query_row(
            "SELECT COUNT(*) FROM bridge_request_state_log
              WHERE request_id = ?1 AND to_state IN ('RefundBroadcast', 'Refunded')",
            [request_id],
            |r| r.get(0),
        )?;
        if seen_network > 0 {
            return Ok(Refused(
                "the state log records RefundBroadcast/Refunded for this request — a refund \
                 transaction was once broadcast"
                    .to_string(),
            ));
        }
        let last: Option<(Option<String>, String)> = conn
            .query_row(
                "SELECT from_state, to_state FROM bridge_request_state_log
                  WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match last {
            Some((Some(from), to)) if from == "ManualReview" && to == "RefundPending" => {}
            other => {
                return Ok(Refused(format!(
                    "the last state-log entry is {other:?}, expected ManualReview -> RefundPending"
                )));
            }
        }
        // Nothing paid, nothing else refunding, nothing closed.
        let destination_txid: Option<Vec<u8>> = conn
            .query_row(
                "SELECT destination_txid FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if destination_txid.is_some() {
            return Ok(Refused(
                "a destination transaction was submitted for this request".to_string(),
            ));
        }
        let payouts: i64 = conn.query_row(
            "SELECT COUNT(*) FROM goldcoin_payouts WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if payouts > 0 {
            return Ok(Refused(
                "a Goldcoin payout row exists for this request".to_string(),
            ));
        }
        let robinhood: i64 = conn.query_row(
            "SELECT COUNT(*) FROM robinhood_transactions WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if robinhood > 0 {
            return Ok(Refused(
                "a Robinhood operation exists for this request".to_string(),
            ));
        }
        let other_refunds: i64 = conn.query_row(
            "SELECT COUNT(*) FROM goldcoin_refunds WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if other_refunds > 0 {
            return Ok(Refused(
                "a Goldcoin refund lifecycle also exists — duplicate refund".to_string(),
            ));
        }
        if let Some(c) = Self::request_closure_in(conn, request_id)? {
            return Ok(Refused(format!(
                "already closed as {} (reference {})",
                c.disposition.as_str(),
                c.reference
            )));
        }
        let manual: i64 = conn.query_row(
            "SELECT COUNT(*) FROM manual_solana_refunds WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if manual > 0 {
            return Ok(Refused(
                "a manual Solana refund is already recorded".to_string(),
            ));
        }
        Ok(RefundReturnVerdict::SafeToReturn { request, refund })
    }

    /// EXECUTE: retire the never-broadcast lifecycle and return the
    /// request to `ManualReview`. Re-runs every guard inside the write
    /// transaction; requires a fresh chain proof for THIS request and
    /// nonce. Broadcasts nothing, refunds nothing, pays nothing.
    pub fn return_refund_to_manual_review(
        &mut self,
        request_id: i64,
        proof: &RefundReturnChainProof,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<RefundReturnOutcome, LedgerError> {
        let refuse = |detail: String| LedgerError::ManualReviewNotRecoverable {
            id: request_id,
            detail,
        };
        let note = note.trim();
        if note.is_empty() {
            return Err(refuse("a non-empty note is required".to_string()));
        }
        let tx = write_tx(&mut self.conn)?;
        let (request, refund) = match Self::refund_return_verdict_in(&tx, request_id)? {
            RefundReturnVerdict::SafeToReturn { request, refund } => (request, refund),
            RefundReturnVerdict::AlreadyReturned { retired, .. } => {
                tx.rollback()?;
                return Ok(RefundReturnOutcome::AlreadyReturned(retired));
            }
            RefundReturnVerdict::Refused(reason) => {
                tx.rollback()?;
                return Err(refuse(reason));
            }
        };
        if proof.request_id != request_id
            || proof.nonce != refund.nonce
            || !proof.nonce_pda_absent
            || !proof.obligation_pending
        {
            tx.rollback()?;
            return Err(refuse(
                "the chain proof does not cover this request/nonce or does not prove that \
                 nothing landed — refusing"
                    .to_string(),
            ));
        }
        tx.execute(
            "INSERT INTO solana_refunds_retired
                (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                 destination_token_account, reserve_mint, token_program, manual_review_reason,
                 note, created_by, state, attestation_epoch, refund_signature, recent_blockhash,
                 created_at, broadcast_at, confirmed_at, retired_at, retired_by, retire_reason,
                 retire_note)
             SELECT request_id, obligation_index, nonce, amount_solana_atomic, requester,
                    destination_token_account, reserve_mint, token_program, manual_review_reason,
                    note, created_by, state, attestation_epoch, refund_signature, recent_blockhash,
                    created_at, broadcast_at, confirmed_at, ?2, ?3, ?4, ?5
               FROM solana_refunds WHERE request_id = ?1 AND state = 'Pending'",
            rusqlite::params![request_id, now, actor, OUT_OF_BAND_REFUND_RECOVERY, note],
        )?;
        let deleted = tx.execute(
            "DELETE FROM solana_refunds WHERE request_id = ?1 AND state = 'Pending'
                AND refund_signature IS NULL AND recent_blockhash IS NULL",
            [request_id],
        )?;
        if deleted != 1 {
            tx.rollback()?;
            return Err(refuse(format!(
                "expected to retire exactly one Pending refund row, matched {deleted}"
            )));
        }
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2 AND state = ?3",
            rusqlite::params![
                RequestState::ManualReview,
                request_id,
                RequestState::RefundPending
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(request.state),
            RequestState::ManualReview,
            now,
            Some(OUT_OF_BAND_REFUND_RECOVERY),
            actor,
        )?;
        let retired =
            Self::latest_retired_in(&tx, request_id)?.expect("the retired row was just inserted");
        tx.commit()?;
        Ok(RefundReturnOutcome::Returned(retired))
    }

    /// The latest retired lifecycle of `request_id`, if any.
    pub fn latest_retired_solana_refund(
        &self,
        request_id: i64,
    ) -> Result<Option<RetiredSolanaRefund>, LedgerError> {
        Self::latest_retired_in(&self.conn, request_id)
    }

    fn latest_retired_in(
        conn: &Connection,
        request_id: i64,
    ) -> Result<Option<RetiredSolanaRefund>, LedgerError> {
        Ok(conn
            .query_row(
                "SELECT id, request_id, obligation_index, nonce, amount_solana_atomic, retired_at,
                        retired_by, retire_reason, retire_note
                   FROM solana_refunds_retired WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
                [request_id],
                |r| {
                    Ok(RetiredSolanaRefund {
                        id: r.get(0)?,
                        request_id: r.get(1)?,
                        obligation_index: r.get::<_, i64>(2)? as u64,
                        nonce: r.get::<_, i64>(3)? as u64,
                        amount_solana_atomic: r.get::<_, i64>(4)? as u64,
                        retired_at: r.get(5)?,
                        retired_by: r.get(6)?,
                        retire_reason: r.get(7)?,
                        retire_note: r.get(8)?,
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests;
