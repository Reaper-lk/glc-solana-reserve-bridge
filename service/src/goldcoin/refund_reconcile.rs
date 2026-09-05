//! Confirmation reconciliation for GlcToSol refunds that have ALREADY
//! been broadcast.
//!
//! # What this exists to fix
//!
//! `goldcoin::refund` builds, signs and broadcasts a refund, and records
//! the resulting txid as `goldcoin_refunds.state = 'Broadcast'` with the
//! request at `RequestState::RefundBroadcast`. Nothing then advanced it.
//! [`crate::ledger::Ledger::record_goldcoin_refund_confirmed`] — the
//! terminal transition, and the only place the request's stranded
//! SolanaReserve reservation is released — had no production caller at
//! all: not the orchestrator, not the CLI, not the admin API. A refund
//! whose transaction was long since confirmed on Goldcoin therefore stayed
//! `RefundBroadcast` forever, kept appearing in
//! `glc-admin glc-refund-list --open-only`, and kept holding reserved
//! capacity that the chain had already discharged.
//!
//! The danger in that is not the stale row. It is what a stale row invites:
//! an operator looking at a months-old "open" refund can reasonably
//! conclude the refund never went out, and send another one. The money is
//! already gone from the vault, and a second refund would send it twice,
//! unrecallably.
//!
//! # This module cannot broadcast anything — structurally, not by promise
//!
//! Reconciliation takes [`RefundConfirmationRpc`], a trait with exactly ONE
//! method: `get_raw_transaction`. It is deliberately NOT
//! [`crate::goldcoin::refund::RefundRpc`], which also carries
//! `send_raw_transaction`. Inside this module the generic parameter is
//! bounded only by the read-only trait, so a call to broadcast, or to
//! anything else a node can be asked to DO, is not merely discouraged —
//! it does not compile.
//!
//! Nothing here builds a transaction, selects a UTXO, contacts a signer,
//! signs, broadcasts, or writes any evidence column. The stored txid is
//! read and never replaced. The only write this path can cause is
//! [`crate::ledger::Ledger::reconcile_goldcoin_refund_confirmations`],
//! which updates the observed depth and — at or above the required depth —
//! commits the terminal transition.
//!
//! # The transaction is verified, not merely counted
//!
//! Reaching the required depth is necessary and not sufficient. Before any
//! transition, the transaction the node returned is checked against the
//! evidence the ledger recorded when it was broadcast:
//!
//! - the node's reported `txid` must equal the stored txid — a node
//!   answering about a DIFFERENT transaction is caught here rather than
//!   being counted as this refund's depth;
//! - output 0 must pay the stored refund destination for exactly the
//!   stored refund amount — the same "output 0 is the refund" invariant
//!   [`crate::goldcoin::refund::validate_refund_tx`] enforced before the
//!   bytes were ever signed;
//! - the inputs must be exactly the stored reserved outpoints, in order.
//!
//! Any mismatch is a refusal that writes nothing and is reported loudly.
//! It is never resolved by rebuilding, re-broadcasting or overwriting the
//! stored evidence: a disagreement between the chain and our own record of
//! what we broadcast is a question for a human, and the safe state to leave
//! it in is exactly the one it is already in.
//!
//! # Fail-closed on every unknown
//!
//! An unreachable node, a transaction the node does not know, a malformed
//! response, an absent `confirmations` field (which for this node means
//! mempool-only, never "zero and therefore fine") — each leaves the row
//! untouched and is reported. Unknown chain state is never read as
//! progress, and never as evidence that the refund failed either.

use crate::goldcoin::rpc::{DecodedTransaction, RpcError};
use crate::ledger::{GoldcoinRefundRow, GoldcoinRefundState, Ledger};

/// The Goldcoin RPC surface confirmation reconciliation needs: one read.
///
/// Kept separate from [`crate::goldcoin::refund::RefundRpc`] on purpose —
/// see this module's docs. A blanket implementation covers every
/// [`crate::goldcoin::indexer::GoldcoinRpc`], so the orchestrator passes
/// its existing client and still gets a compile-time guarantee that this
/// code path cannot reach a broadcast.
pub trait RefundConfirmationRpc {
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> impl std::future::Future<Output = Result<DecodedTransaction, RpcError>> + Send;
}

impl<T: crate::goldcoin::indexer::GoldcoinRpc> RefundConfirmationRpc for T {
    // Deliberately NOT an `async fn`: returning the inner future directly
    // means its `+ Send` guarantee comes from `GoldcoinRpc`'s own declared
    // return type, rather than having to be re-proven for a generated
    // future that holds `&self` across an await — which would force a
    // `Sync` bound onto every caller, including `Orchestrator`'s `GR`.
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> impl std::future::Future<Output = Result<DecodedTransaction, RpcError>> + Send {
        crate::goldcoin::indexer::GoldcoinRpc::get_raw_transaction(self, txid_hex)
    }
}

/// What one reconciliation attempt concluded. Every variant except
/// [`RefundReconcileOutcome::Confirmed`] leaves the refund's lifecycle
/// state exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundReconcileOutcome {
    /// The stored transaction verified and had reached the required depth:
    /// `Broadcast -> Refunded` was committed by THIS call, releasing the
    /// reservation once.
    Confirmed { confirmations: i64 },
    /// Verified, but not deep enough yet. The observed depth was recorded;
    /// nothing else changed.
    Pending { confirmations: i64, required: i64 },
    /// The refund was already terminal before this pass. The observed depth
    /// was refreshed; the reservation was NOT released again.
    AlreadyRefunded { confirmations: i64 },
    /// The node could not be asked, or could not answer about this
    /// transaction. Nothing was written. Explicitly NOT evidence that the
    /// refund did not happen.
    Unavailable { reason: String },
    /// The node answered, and what it returned does not match the evidence
    /// recorded when this refund was broadcast. Nothing was written, and
    /// nothing here will attempt to repair it.
    Mismatch { reason: String },
}

/// Verifies that `decoded` really is the refund this row recorded.
///
/// Read-only and total: every failure is a reason string, never a panic
/// and never a write. `inputs` are the persisted reserved outpoints in
/// construction order (`goldcoin_refund_inputs`).
pub fn verify_broadcast_evidence(
    row: &GoldcoinRefundRow,
    inputs: &[([u8; 32], u32, u64)],
    decoded: &DecodedTransaction,
) -> Result<(), String> {
    let stored_txid = row
        .txid
        .ok_or_else(|| "refund is Broadcast but records no txid".to_string())?;

    // 1. Identity. The txid commits to the whole transaction, so this
    //    single equality is the strongest check available; the structural
    //    checks below defend against a node that reports a txid not
    //    actually derived from the bytes it sent us.
    let reported = crate::goldcoin::hex::decode_vec(&decoded.txid)
        .map_err(|e| format!("node reported an undecodable txid {:?}: {e}", decoded.txid))?;
    if reported.as_slice() != stored_txid.as_slice() {
        return Err(format!(
            "node returned transaction {} when asked about {} — refusing to read another \
             transaction's depth as this refund's",
            decoded.txid,
            crate::goldcoin::hex::encode(&stored_txid)
        ));
    }

    // 2. Output 0 is the refund: exact stored destination, exact stored
    //    amount. Same invariant `validate_refund_tx` enforced pre-signing.
    let first = decoded
        .vout
        .first()
        .ok_or_else(|| "confirmed refund transaction reports no outputs".to_string())?;
    if first.n != 0 {
        return Err(format!(
            "first reported output is index {}, not 0; cannot identify the refund output",
            first.n
        ));
    }
    let expected_script = crate::goldcoin::address::p2pkh_script_hex(&row.refund_dest_p2pkh_hash);
    if !first
        .script_pub_key
        .hex
        .eq_ignore_ascii_case(&expected_script)
    {
        return Err(format!(
            "confirmed transaction's output 0 does not pay the recorded refund destination {}",
            row.refund_dest_address
        ));
    }
    let observed_atomic = crate::goldcoin::deposit::glc_to_atomic(first.value);
    if observed_atomic != row.refund_amount_atomic {
        return Err(format!(
            "confirmed transaction pays {observed_atomic} atomic to the refund destination but \
             the recorded refund amount is {}",
            row.refund_amount_atomic
        ));
    }

    // 3. Inputs are exactly the reserved outpoints, in order. A refund that
    //    spent something else is not the transaction this row describes,
    //    however much its other fields agree.
    if decoded.vin.len() != inputs.len() {
        return Err(format!(
            "confirmed transaction spends {} inputs but {} were reserved for this refund",
            decoded.vin.len(),
            inputs.len()
        ));
    }
    for (i, (vin, (txid, vout, _))) in decoded.vin.iter().zip(inputs.iter()).enumerate() {
        let Some((prev_txid, prev_vout)) = vin.prevout() else {
            return Err(format!(
                "input {i} of the confirmed transaction reports no traceable previous outpoint"
            ));
        };
        let prev = crate::goldcoin::hex::decode_vec(prev_txid)
            .map_err(|e| format!("input {i} reports an undecodable previous txid: {e}"))?;
        if prev.as_slice() != txid.as_slice() || prev_vout != *vout {
            return Err(format!(
                "input {i} of the confirmed transaction is not the outpoint reserved for this \
                 refund"
            ));
        }
    }
    Ok(())
}

/// Reconciles ONE refund row against the chain.
///
/// Safe to call on any request id, in any state, any number of times, from
/// any number of ticks: the state decides the outcome, and the only
/// transition it can cause is the depth-gated `Broadcast -> Refunded` one,
/// committed atomically inside the ledger.
pub async fn reconcile_one<R: RefundConfirmationRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    request_id: i64,
    required_confirmations: i64,
    now: i64,
) -> Result<RefundReconcileOutcome, String> {
    // Scoped so no borrow of the ledger is held across the RPC await.
    let (row, inputs) = {
        let row = ledger
            .get_goldcoin_refund(request_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no Goldcoin refund row for request {request_id}"))?;
        let inputs = ledger
            .get_goldcoin_refund_inputs(request_id)
            .map_err(|e| e.to_string())?;
        (row, inputs)
    };

    match row.state {
        // A refund that was never broadcast has no on-chain depth to
        // reconcile, and this pass must never be what advances it.
        GoldcoinRefundState::Built | GoldcoinRefundState::Signed => {
            return Ok(RefundReconcileOutcome::Unavailable {
                reason: format!(
                    "refund is {} — not broadcast, so there is nothing to reconcile",
                    row.state.as_str()
                ),
            })
        }
        GoldcoinRefundState::Broadcast | GoldcoinRefundState::Refunded => {}
    }

    let Some(stored_txid) = row.txid else {
        return Ok(RefundReconcileOutcome::Mismatch {
            reason: "refund records no txid despite having been broadcast".to_string(),
        });
    };
    let txid_hex = crate::goldcoin::hex::encode(&stored_txid);

    let decoded = match rpc.get_raw_transaction(&txid_hex).await {
        Ok(d) => d,
        Err(e) => {
            // Includes the node not knowing the transaction. That is not
            // proof the refund failed — a pruned, resyncing or simply
            // different node answers the same way — so it never writes.
            return Ok(RefundReconcileOutcome::Unavailable {
                reason: format!("could not read refund transaction {txid_hex}: {e}"),
            });
        }
    };

    if let Err(reason) = verify_broadcast_evidence(&row, &inputs, &decoded) {
        return Ok(RefundReconcileOutcome::Mismatch { reason });
    }

    // Absent entirely means mempool-only for this node (see
    // `DecodedTransaction::confirmations`); it is a real 0, not a missing
    // reading, and 0 can never reach a positive threshold.
    let confirmations = decoded.confirmations.unwrap_or(0);

    let transitioned = ledger
        .reconcile_goldcoin_refund_confirmations(
            request_id,
            confirmations,
            required_confirmations,
            now,
        )
        .map_err(|e| e.to_string())?;

    Ok(if transitioned {
        RefundReconcileOutcome::Confirmed { confirmations }
    } else if row.state == GoldcoinRefundState::Refunded {
        RefundReconcileOutcome::AlreadyRefunded { confirmations }
    } else {
        RefundReconcileOutcome::Pending {
            confirmations,
            required: required_confirmations,
        }
    })
}

/// Summary of one reconciliation pass over every broadcast refund.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefundReconcileReport {
    /// Rows examined this pass.
    pub checked: u32,
    /// Rows this pass transitioned `Broadcast -> Refunded`.
    pub confirmed: u32,
    /// Verified but not yet deep enough.
    pub pending: u32,
    /// Left alone because the chain could not be consulted.
    pub unavailable: u32,
    /// Left alone because the chain disagreed with the stored evidence.
    /// Any non-zero value here needs a human.
    pub mismatched: u32,
}

/// Reconciles every refund currently in `Broadcast`, oldest first.
///
/// One row's problem never stops the pass: an unreachable node or a
/// mismatched transaction is recorded and the next row is still checked,
/// because neither says anything about any other refund. Nothing in this
/// pass can move funds, so there is no ordering or fairness concern to
/// protect — unlike the admission paths, where stopping early is itself a
/// safety property.
pub async fn reconcile_broadcast_refunds<R: RefundConfirmationRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    required_confirmations: i64,
    now: i64,
) -> Result<(RefundReconcileReport, Vec<String>), String> {
    let ids = ledger
        .goldcoin_refunds_in_state(GoldcoinRefundState::Broadcast.as_str())
        .map_err(|e| e.to_string())?;

    let mut report = RefundReconcileReport::default();
    let mut problems = Vec::new();
    for request_id in ids {
        report.checked += 1;
        match reconcile_one(rpc, ledger, request_id, required_confirmations, now).await {
            Ok(RefundReconcileOutcome::Confirmed { .. }) => report.confirmed += 1,
            Ok(RefundReconcileOutcome::Pending { .. }) => report.pending += 1,
            Ok(RefundReconcileOutcome::AlreadyRefunded { .. }) => {}
            Ok(RefundReconcileOutcome::Unavailable { reason }) => {
                report.unavailable += 1;
                problems.push(format!("glc refund {request_id}: {reason}"));
            }
            Ok(RefundReconcileOutcome::Mismatch { reason }) => {
                report.mismatched += 1;
                problems.push(format!(
                    "glc refund {request_id}: REFUSING to reconcile — {reason}. The stored \
                     broadcast is left untouched and authoritative; do NOT send another refund \
                     for this request"
                ));
            }
            Err(e) => {
                report.unavailable += 1;
                problems.push(format!("glc refund {request_id}: {e}"));
            }
        }
    }
    Ok((report, problems))
}

#[cfg(test)]
mod tests;
