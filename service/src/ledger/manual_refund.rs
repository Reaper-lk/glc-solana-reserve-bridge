//! Manual (out-of-band) Solana refunds — the LEDGER half (schema v35).
//!
//! The 2026-09-13 remediation left a backlog of Solana-sourced deposits
//! parked in `ManualReview` that this service cannot refund through its
//! own tooling: `refund-manual-review` needs the on-chain `refund_withdraw`
//! instruction, and the deployed program does not dispatch it
//! (`solana::program_compat`). The operator refunds them instead from a
//! SEPARATELY FUNDED wallet — a key this service never holds, loads, or
//! records anything about beyond its public key — and this module records
//! the outcome:
//!
//! 1. [`Ledger::manual_solana_refund_eligibility`] is the one place the
//!    ledger-side "may this request be refunded on Solana at all" question
//!    is answered. `glc-admin manual-refund-export` asks it for every
//!    `ManualReview` row (read-only), and
//!    [`Ledger::record_manual_solana_refund`] asks it AGAIN, inside its
//!    own write transaction, immediately before recording — so a request
//!    that changed between export and import is refused, never closed on
//!    stale evidence.
//! 2. [`Ledger::record_manual_solana_refund`] writes the
//!    `manual_solana_refunds` row and the `refunded_out_of_band` closure
//!    ([`Ledger::close_manual_review_in`], v32) in ONE transaction, with
//!    the finalized transaction signature as the closure's `reference`.
//!    Neither row can exist without the other.
//!
//! What this module does NOT do: verify anything on chain. The chain-side
//! verification (signature finalized, exact amount, reserve mint, the
//! request's own requester as recipient, the memo naming the request) is
//! `solana::manual_refund`'s job, and its verified figures arrive here as
//! [`ManualSolanaRefundInputs`]. The ledger trusts those inputs exactly as
//! far as `solana_refunds` trusts `VerifiedRefundInputs`: the caller has
//! already refused every mismatch, and the row records what was verified.

use rusqlite::{Connection, OptionalExtension};

use super::{
    row_to_request, write_tx, BridgeRequest, CloseOutcome, ClosureDisposition, Ledger, LedgerError,
    ManualReviewDisposition, OperatorDecision, RequestClosure, RequestState, SELECT_REQUEST,
};

/// One recorded, chain-verified, imported manual refund — a row of
/// `manual_solana_refunds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualSolanaRefund {
    pub id: i64,
    pub request_id: i64,
    pub batch_id: String,
    /// Always `"solana"` (schema `CHECK`); carried so a view never has to
    /// assume it.
    pub network: String,
    pub mint: [u8; 32],
    /// The operator's separately funded wallet — the transaction's fee
    /// payer and the `transfer_checked` authority, read off the finalized
    /// transaction, never off the manifest alone.
    pub refund_wallet: [u8; 32],
    /// The request's own `requester` — the original depositor.
    pub recipient: [u8; 32],
    pub recipient_token_account: [u8; 32],
    /// Reserve-mint units (the mint's live decimals).
    pub amount_atomic: u64,
    /// The same amount in the ledger's canonical 8-decimal unit — equal to
    /// the request's `gross_amount_atomic` (a refund returns the gross
    /// deposit; no fee ever accrued).
    pub amount_canonical_atomic: u64,
    pub tx_signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub submitted_at: Option<i64>,
    pub finalized_at: Option<i64>,
    pub imported_at: i64,
    pub imported_by: String,
    pub note: String,
}

/// The chain-verified facts `record_manual_solana_refund` stores. Every
/// field was checked against the finalized transaction AND the request
/// by `solana::manual_refund::verify_landed_refund` before it gets here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualSolanaRefundInputs {
    pub batch_id: String,
    pub mint: [u8; 32],
    pub refund_wallet: [u8; 32],
    pub recipient: [u8; 32],
    pub recipient_token_account: [u8; 32],
    pub amount_atomic: u64,
    pub amount_canonical_atomic: u64,
    pub tx_signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub submitted_at: Option<i64>,
    pub finalized_at: Option<i64>,
}

/// Result of [`Ledger::record_manual_solana_refund`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualRefundRecordOutcome {
    /// Row and closure written; the request is now `Closed`.
    Recorded(ManualSolanaRefund, RequestClosure),
    /// The SAME signature was already recorded for this request — nothing
    /// written (an idempotent re-import).
    AlreadyRecorded(ManualSolanaRefund),
}

impl Ledger {
    /// Every request currently in `ManualReview`, ascending id — the
    /// export's candidate set. Read-only.
    pub fn manual_review_request_ids(&self) -> Result<Vec<i64>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM bridge_requests WHERE state = 'ManualReview' ORDER BY id")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// The LEDGER-side eligibility of `request_id` for an out-of-band
    /// Solana refund, read-only. `Ok` returns the row; `Err` carries the
    /// exact reason (`LedgerError::ManualReviewNotRecoverable`). Every
    /// refusal, in order:
    ///
    /// - the request must exist and be in `ManualReview`;
    /// - its SOURCE must be Solana (`SolToGlc`/`SolToRhn`): the deposit
    ///   being returned is an on-chain `WithdrawalObligation` whose
    ///   `requester` is the refund destination. A Robinhood- or
    ///   Goldcoin-sourced deposit is refunded on ITS chain by its own
    ///   tooling; nothing in its row names an authoritative Solana refund
    ///   destination (a `RhnToSol` row's `recipient` is where the PAYOUT
    ///   was to go, not where the depositor's principal came from), so it
    ///   is never converted into a Solana refund here;
    /// - `requester` and `source_obligation_index` must be recorded (the
    ///   chain-side verification needs both);
    /// - no destination transaction, no Goldcoin payout row of any state,
    ///   no Robinhood operation of any kind — any of those is a settlement
    ///   in flight or an incident, not a refund;
    /// - no refund lifecycle (`solana_refunds`/`goldcoin_refunds`/
    ///   Robinhood refund): a refund that was begun in-band ends through
    ///   its own path;
    /// - no closure and no earlier manual refund;
    /// - no `process` operator decision (the operator chose the payout);
    /// - a rapid-burst hold is refused before `review_after`, exactly as
    ///   `close_manual_review` refuses it.
    pub fn manual_solana_refund_eligibility(
        &self,
        request_id: i64,
        now: i64,
    ) -> Result<BridgeRequest, LedgerError> {
        Self::manual_solana_refund_eligibility_in(&self.conn, request_id, now)
    }

    fn manual_solana_refund_eligibility_in(
        conn: &Connection,
        request_id: i64,
        now: i64,
    ) -> Result<BridgeRequest, LedgerError> {
        let refuse = |detail: String| LedgerError::ManualReviewNotRecoverable {
            id: request_id,
            detail,
        };
        let request = conn
            .query_row(SELECT_REQUEST, [request_id], row_to_request)
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        if request.state != RequestState::ManualReview {
            return Err(refuse(format!(
                "state is {}, not ManualReview",
                request.state.as_str()
            )));
        }
        if !request.direction.source_is_solana() {
            return Err(refuse(format!(
                "route {} is not Solana-sourced: its deposit is refunded on {} by that chain's \
                 own refund tooling, and the row names no authoritative Solana refund destination",
                request.direction.as_str(),
                if request.direction.source_is_goldcoin() {
                    "Goldcoin"
                } else {
                    "Robinhood"
                }
            )));
        }
        if request.requester.is_none() {
            return Err(refuse(
                "no requester (Solana depositor) is recorded for this request".to_string(),
            ));
        }
        if request.source_obligation_index.is_none() {
            return Err(refuse(
                "no source_obligation_index is recorded for this request".to_string(),
            ));
        }
        let destination_txid: Option<Vec<u8>> = conn
            .query_row(
                "SELECT destination_txid FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if destination_txid.is_some() {
            return Err(refuse(
                "a destination transaction was submitted for this request — a payout to confirm \
                 or an incident, not a refund"
                    .to_string(),
            ));
        }
        let payout_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM goldcoin_payouts WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if payout_rows > 0 {
            return Err(refuse(
                "a Goldcoin payout row exists for this request — a settlement was begun; not a \
                 refund candidate"
                    .to_string(),
            ));
        }
        let robinhood_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM robinhood_transactions WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if robinhood_rows > 0 {
            return Err(refuse(
                "a Robinhood operation exists for this request — a competing settlement or \
                 refund; not a refund candidate"
                    .to_string(),
            ));
        }
        if Self::refund_lifecycle_exists_in(conn, request_id)? {
            return Err(refuse(
                "a refund lifecycle already exists for this request (solana_refunds / \
                 goldcoin_refunds / Robinhood refund) — it ends through its own path"
                    .to_string(),
            ));
        }
        if let Some(closure) = Self::request_closure_in(conn, request_id)? {
            return Err(refuse(format!(
                "already closed as {} (reference {}) by {} at {}",
                closure.disposition.as_str(),
                closure.reference,
                closure.actor,
                closure.closed_at
            )));
        }
        if let Some(existing) = Self::manual_solana_refund_in(conn, request_id)? {
            return Err(refuse(format!(
                "a manual Solana refund is already recorded (signature {}, batch {})",
                existing.tx_signature, existing.batch_id
            )));
        }
        if request.operator_decision == Some(OperatorDecision::Process) {
            return Err(refuse(
                "the operator recorded the PROCESS decision on this request — it is to be paid \
                 out, not refunded"
                    .to_string(),
            ));
        }
        if request.manual_review_disposition == ManualReviewDisposition::RapidBurstHold
            && !request.review_available(now)
        {
            return Err(refuse(format!(
                "rapid-burst hold: the minimum review hold has not elapsed (review_after={}, \
                 now={})",
                request.review_after.unwrap_or_default(),
                now
            )));
        }
        Ok(request)
    }

    /// Records a chain-verified manual refund and closes the request as
    /// `refunded_out_of_band` — one transaction, both rows or neither.
    ///
    /// Idempotent on the signature: the same `(request, signature)` again
    /// returns [`ManualRefundRecordOutcome::AlreadyRecorded`] and writes
    /// nothing. A different signature for an already-recorded request, or
    /// a signature already recorded for ANOTHER request, is refused — a
    /// request is refunded once and a transaction proves one refund.
    /// Re-runs [`Self::manual_solana_refund_eligibility`] inside the
    /// transaction, so nothing that happened since the export can be
    /// closed over.
    pub fn record_manual_solana_refund(
        &mut self,
        request_id: i64,
        inputs: &ManualSolanaRefundInputs,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ManualRefundRecordOutcome, LedgerError> {
        let refuse = |detail: String| LedgerError::ManualReviewNotRecoverable {
            id: request_id,
            detail,
        };
        let note = note.trim();
        if note.is_empty() {
            return Err(refuse(
                "a manual refund record needs a non-empty note".to_string(),
            ));
        }
        if inputs.tx_signature.trim().is_empty() {
            return Err(refuse(
                "a manual refund record needs a transaction signature".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        if let Some(existing) = Self::manual_solana_refund_in(&tx, request_id)? {
            tx.rollback()?;
            if existing.tx_signature == inputs.tx_signature {
                return Ok(ManualRefundRecordOutcome::AlreadyRecorded(existing));
            }
            return Err(refuse(format!(
                "a manual Solana refund is already recorded with a DIFFERENT signature ({}, batch \
                 {}) — a request is refunded once; investigate before doing anything else",
                existing.tx_signature, existing.batch_id
            )));
        }
        if let Some(other) = Self::manual_solana_refund_by_signature_in(&tx, &inputs.tx_signature)?
        {
            tx.rollback()?;
            return Err(refuse(format!(
                "signature {} is already recorded as the refund of request {} — one transaction \
                 proves one refund",
                inputs.tx_signature, other.request_id
            )));
        }
        let request = match Self::manual_solana_refund_eligibility_in(&tx, request_id, now) {
            Ok(r) => r,
            Err(e) => {
                tx.rollback()?;
                return Err(e);
            }
        };
        if request.requester != Some(inputs.recipient) {
            tx.rollback()?;
            return Err(refuse(
                "the verified recipient is not this request's requester".to_string(),
            ));
        }
        if request.gross_amount_atomic != inputs.amount_canonical_atomic {
            tx.rollback()?;
            return Err(refuse(format!(
                "the verified canonical amount ({}) is not this request's gross deposit ({})",
                inputs.amount_canonical_atomic, request.gross_amount_atomic
            )));
        }
        let closure = match Self::close_manual_review_in(
            &tx,
            request_id,
            ClosureDisposition::RefundedOutOfBand,
            &inputs.tx_signature,
            note,
            actor,
            now,
        ) {
            Ok(CloseOutcome::Closed(c)) => c,
            Ok(CloseOutcome::AlreadyClosed(c)) => {
                // Unreachable after the eligibility check above (which
                // refuses any existing closure); fail closed regardless.
                tx.rollback()?;
                return Err(refuse(format!(
                    "already closed as {} (reference {})",
                    c.disposition.as_str(),
                    c.reference
                )));
            }
            Err(e) => {
                tx.rollback()?;
                return Err(e);
            }
        };
        tx.execute(
            "INSERT INTO manual_solana_refunds
                (request_id, batch_id, network, mint, refund_wallet, recipient,
                 recipient_token_account, amount_atomic, amount_canonical_atomic, tx_signature,
                 slot, block_time, submitted_at, finalized_at, imported_at, imported_by, note)
             VALUES (?1, ?2, 'solana', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16)",
            rusqlite::params![
                request_id,
                inputs.batch_id,
                &inputs.mint[..],
                &inputs.refund_wallet[..],
                &inputs.recipient[..],
                &inputs.recipient_token_account[..],
                inputs.amount_atomic as i64,
                inputs.amount_canonical_atomic as i64,
                inputs.tx_signature,
                inputs.slot as i64,
                inputs.block_time,
                inputs.submitted_at,
                inputs.finalized_at,
                now,
                actor,
                note,
            ],
        )?;
        let row = Self::manual_solana_refund_in(&tx, request_id)?
            .expect("the manual refund row was just inserted");
        tx.commit()?;
        Ok(ManualRefundRecordOutcome::Recorded(row, closure))
    }

    /// The recorded manual refund of `request_id`, if any.
    pub fn get_manual_solana_refund(
        &self,
        request_id: i64,
    ) -> Result<Option<ManualSolanaRefund>, LedgerError> {
        Self::manual_solana_refund_in(&self.conn, request_id)
    }

    /// The recorded manual refund proven by `signature`, if any.
    pub fn get_manual_solana_refund_by_signature(
        &self,
        signature: &str,
    ) -> Result<Option<ManualSolanaRefund>, LedgerError> {
        Self::manual_solana_refund_by_signature_in(&self.conn, signature)
    }

    /// Every recorded manual refund, newest import first, at most `limit`.
    pub fn list_manual_solana_refunds(
        &self,
        limit: usize,
    ) -> Result<Vec<ManualSolanaRefund>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{MANUAL_REFUND_SELECT} ORDER BY imported_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit as i64], row_to_manual_refund)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn manual_solana_refund_in(
        conn: &Connection,
        request_id: i64,
    ) -> Result<Option<ManualSolanaRefund>, LedgerError> {
        Ok(conn
            .query_row(
                &format!("{MANUAL_REFUND_SELECT} WHERE request_id = ?1"),
                [request_id],
                row_to_manual_refund,
            )
            .optional()?)
    }

    fn manual_solana_refund_by_signature_in(
        conn: &Connection,
        signature: &str,
    ) -> Result<Option<ManualSolanaRefund>, LedgerError> {
        Ok(conn
            .query_row(
                &format!("{MANUAL_REFUND_SELECT} WHERE tx_signature = ?1"),
                [signature],
                row_to_manual_refund,
            )
            .optional()?)
    }
}

const MANUAL_REFUND_SELECT: &str =
    "SELECT id, request_id, batch_id, network, mint, refund_wallet, recipient,
            recipient_token_account, amount_atomic, amount_canonical_atomic, tx_signature, slot,
            block_time, submitted_at, finalized_at, imported_at, imported_by, note
       FROM manual_solana_refunds";

fn blob32(r: &rusqlite::Row, idx: usize) -> rusqlite::Result<[u8; 32]> {
    let v: Vec<u8> = r.get(idx)?;
    v.as_slice().try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "expected 32 bytes".into(),
        )
    })
}

fn row_to_manual_refund(r: &rusqlite::Row) -> rusqlite::Result<ManualSolanaRefund> {
    Ok(ManualSolanaRefund {
        id: r.get(0)?,
        request_id: r.get(1)?,
        batch_id: r.get(2)?,
        network: r.get(3)?,
        mint: blob32(r, 4)?,
        refund_wallet: blob32(r, 5)?,
        recipient: blob32(r, 6)?,
        recipient_token_account: blob32(r, 7)?,
        amount_atomic: r.get::<_, i64>(8)? as u64,
        amount_canonical_atomic: r.get::<_, i64>(9)? as u64,
        tx_signature: r.get(10)?,
        slot: r.get::<_, i64>(11)? as u64,
        block_time: r.get(12)?,
        submitted_at: r.get(13)?,
        finalized_at: r.get(14)?,
        imported_at: r.get(15)?,
        imported_by: r.get(16)?,
        note: r.get(17)?,
    })
}

#[cfg(test)]
mod tests;
