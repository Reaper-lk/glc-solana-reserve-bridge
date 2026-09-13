//! **Chain-terminal settlement reconciliation** — completing the LOCAL
//! record of a Robinhood-sourced settlement (`RhnToGlc`, `RhnToSol`) that
//! the chain already finished, after this service lost track of its own
//! transaction (2026-09-13, request 4244 / V2 obligation #57: broadcast,
//! three fee replacements, receipt never observed, replacement budget
//! exhausted, row parked `ManualReview`; nonce 109 had in fact executed
//! `executeSettlement` successfully).
//!
//! # What it is
//!
//! A proof, then a bookkeeping write. [`prove`] re-reads everything from
//! the chain and the ledger and either produces a [`SettlementProof`] or
//! a refusal naming the first thing that does not match. [`apply`] runs
//! the SAME completion body the settlement driver runs on a finalized
//! receipt (`Ledger::settle_confirmed_in`), plus the operation row's
//! receipt fields from the verified event's transaction, in one
//! transaction, with the state-log reason
//! `chain_terminal_reconciliation` and an admin audit row. It moves no
//! funds, sends nothing, re-broadcasts nothing.
//!
//! # What must be true (every item independently re-read, none inferred)
//!
//! Request: exists, Robinhood-sourced settlement route, bound to the
//! configured deployment, names its obligation, is `DestinationConfirmed`
//! (or already `Settled` → `AlreadyReconciled`).
//! Chain: `obligation(idx).status == Settled`; `requestExecuted(SETTLE,
//! id) == true` for the id derived from `(deployment, idx)`; EXACTLY one
//! `ObligationSettled(idx, id)` event, emitted by the configured contract;
//! that transaction's receipt exists, succeeded, is in the event's block,
//! carries the bridge event, was sent by the configured submitter, to the
//! configured contract, under the operation row's own nonce.
//! Ledger: exactly one `Settlement` row for the request, whose
//! `bridge_contract`, `chain_id`, `contract_request_id`, `obligation_index`
//! and `nonce` all match; state non-terminal (`ManualReview` is the
//! expected one) or `Finalized` only when the request is already
//! `Settled`; no other row anywhere carries the same `contract_request_id`;
//! no other request claims the same `(contract, obligation)`.
//! Destination: `RhnToGlc` — exactly one Goldcoin payout row for the
//! request, its `txid` equals the request's `destination_txid`, its amount
//! equals `net_destination_atomic`, its destination hash equals the
//! recipient's, it is `Confirmed`/`Completed` at the required depth, and
//! no other request's payout carries that txid; `RhnToSol` — the request
//! is `DestinationConfirmed` with a Solana release signature.
//! Conflicts: no Goldcoin refund, no Solana refund, no Robinhood refund
//! row, no closure, no payout row of another kind.
//!
//! `AlreadyExecuted` from the replay guard is NEVER enough on its own:
//! it is one of the inputs above, not the conclusion.

use crate::evm::{EvmAddress, EvmTxHash};
use crate::goldcoin::hex;
use crate::ledger::{
    Direction, Ledger, LedgerError, ReconcileOutcome, RequestState, RobinhoodTxKind,
    RobinhoodTxState,
};

use super::auth::{self, ACTION_SETTLE};
use super::calls::{BridgeReader, ContractReadError, OBLIGATION_STATUS_SETTLED};
use super::preflight::VerifiedDeployment;
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmLogFilter, EvmRpc, EvmRpcError, EvmSubmitRpc};

/// `keccak256("ObligationSettled(uint256,bytes32)")`.
pub fn obligation_settled_topic() -> [u8; 32] {
    crate::evm::keccak256(b"ObligationSettled(uint256,bytes32)")
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("chain read: {0}")]
    Rpc(#[from] EvmRpcError),
    #[error("contract read: {0}")]
    Contract(#[from] ContractReadError),
    #[error("{0}")]
    Auth(#[from] auth::AuthError),
}

/// Everything [`apply`] needs, each field re-read and cross-checked by
/// [`prove`]; nothing here is operator-supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementProof {
    pub request_id: i64,
    pub tx_id: i64,
    pub route: Direction,
    pub obligation_index: u64,
    pub contract_request_id: [u8; 32],
    pub chain_tx_hash: [u8; 32],
    pub block_number: u64,
    pub block_hash: [u8; 32],
    pub confirmations: i64,
    pub nonce: u64,
    pub submitter: EvmAddress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    SafeToReconcile(SettlementProof),
    AlreadyReconciled,
    Refuse(String),
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::SafeToReconcile(_) => "SAFE_TO_RECONCILE",
            Verdict::AlreadyReconciled => "ALREADY_RECONCILED",
            Verdict::Refuse(_) => "REFUSE",
        }
    }
}

/// The human-readable dry-run report.
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub request_id: i64,
    pub route: Option<Direction>,
    pub obligation_index: Option<u64>,
    pub chain_state: String,
    pub chain_settlement_tx: Option<String>,
    pub local_request_state: String,
    pub local_settlement_state: String,
    pub destination_payout: String,
    pub refunds: String,
    pub duplicates: String,
    pub verdict: Verdict,
}

impl ReconcileReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("request: {}\n", self.request_id));
        out.push_str(&format!(
            "route: {}\n",
            self.route.map(|r| r.as_str()).unwrap_or("-")
        ));
        out.push_str(&format!(
            "obligation: {}\n",
            self.obligation_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".into())
        ));
        out.push_str(&format!("chain state: {}\n", self.chain_state));
        out.push_str(&format!(
            "chain settlement tx: {}\n",
            self.chain_settlement_tx.as_deref().unwrap_or("-")
        ));
        out.push_str(&format!(
            "local request state: {}\n",
            self.local_request_state
        ));
        out.push_str(&format!(
            "local settlement state: {}\n",
            self.local_settlement_state
        ));
        out.push_str(&format!(
            "destination payout: {}\n",
            self.destination_payout
        ));
        out.push_str(&format!("refunds: {}\n", self.refunds));
        out.push_str(&format!("duplicates: {}\n", self.duplicates));
        out.push_str("\nverdict:\n\n");
        match &self.verdict {
            Verdict::SafeToReconcile(p) => out.push_str(&format!(
                "SAFE_TO_RECONCILE (settlement tx {} block {} nonce {} by {})\n",
                hex::encode(&p.chain_tx_hash),
                p.block_number,
                p.nonce,
                p.submitter.to_checksum_string()
            )),
            Verdict::AlreadyReconciled => out.push_str("ALREADY_RECONCILED\n"),
            Verdict::Refuse(reason) => out.push_str(&format!("REFUSE: {reason}\n")),
        }
        out
    }
}

/// Builds the proof. Read-only: touches no ledger row.
pub async fn prove<R>(
    rpc: &R,
    // `&mut` for Send-ness only (a shared `&Ledger` across an await is not
    // Send); nothing here writes.
    ledger: &mut Ledger,
    deployment: &VerifiedDeployment,
    submitter: EvmAddress,
    required_goldcoin_confirmations: i64,
    required_robinhood_confirmations: i64,
    request_id: i64,
) -> Result<ReconcileReport, ReconcileError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let mut report = ReconcileReport {
        request_id,
        route: None,
        obligation_index: None,
        chain_state: "-".into(),
        chain_settlement_tx: None,
        local_request_state: "-".into(),
        local_settlement_state: "-".into(),
        destination_payout: "-".into(),
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

    // ---- 1. request --------------------------------------------------
    let Some(request) = ledger.get_request(request_id)? else {
        refuse!("request {request_id} does not exist");
    };
    report.route = Some(request.direction);
    report.local_request_state = request.state.as_str().to_string();
    if !request.direction.source_is_robinhood() {
        refuse!(
            "{} is not a Robinhood-settlement route",
            request.direction.as_str()
        );
    }
    let route = match request.direction {
        Direction::RhnToGlc => crate::routes::Route::RhnToGlc,
        Direction::RhnToSol => crate::routes::Route::RhnToSol,
        other => refuse!("{} has no Robinhood settlement leg", other.as_str()),
    };
    let expected_contract = deployment.bridge_contract.to_bytes();
    match request.source_contract {
        Some(c) if c == expected_contract => {}
        Some(c) => refuse!(
            "request is recorded under contract 0x{} — not the configured 0x{}",
            hex::encode(&c),
            hex::encode(&expected_contract)
        ),
        None => refuse!("request records no source contract"),
    }
    let Some(obligation_index) = request.source_obligation_index else {
        refuse!("request names no source obligation");
    };
    report.obligation_index = Some(obligation_index);
    if request.state.is_terminal() && request.state != RequestState::Settled {
        refuse!(
            "request is terminal as {} — not behind a chain-side Settled",
            request.state.as_str()
        );
    }

    // ---- 2. chain ----------------------------------------------------
    let reader = BridgeReader::new(deployment.bridge_contract);
    let obligation = reader
        .obligation(rpc, obligation_index, EvmBlockTag::Latest)
        .await?;
    report.chain_state = obligation.status_name().to_string();
    let contract_request_id = auth::derive_request_id(
        ACTION_SETTLE,
        route,
        deployment.domain(),
        &auth::obligation_identity(obligation_index),
    )?;
    if obligation.status != OBLIGATION_STATUS_SETTLED {
        refuse!(
            "obligation {obligation_index} is {} on chain, not Settled",
            obligation.status_name()
        );
    }
    if !reader
        .request_executed(rpc, ACTION_SETTLE, contract_request_id, EvmBlockTag::Latest)
        .await?
    {
        refuse!(
            "requestExecuted(SETTLE, {}) is false: the contract does not report this settlement \
             as executed",
            hex::encode(&contract_request_id)
        );
    }
    let head = rpc.block_number().await?;
    let from_block = ledger
        .robinhood_observation_for_request(request_id)?
        .map(|o| o.observation.block_number)
        .unwrap_or(0);
    let logs = rpc
        .logs(&EvmLogFilter {
            from_block,
            to_block: head,
            address: deployment.bridge_contract,
            topic0: obligation_settled_topic(),
        })
        .await?;
    let mut index_word = [0u8; 32];
    index_word[24..].copy_from_slice(&obligation_index.to_be_bytes());
    let events: Vec<_> = logs
        .iter()
        .filter(|l| {
            !l.removed
                && l.address == deployment.bridge_contract
                && l.topics.len() == 3
                && l.topics[1] == index_word
                && l.topics[2] == contract_request_id
        })
        .collect();
    let event = match events.as_slice() {
        [one] => *one,
        [] => refuse!(
            "no ObligationSettled({obligation_index}, {}) event from the configured contract \
             between blocks {from_block} and {head}",
            hex::encode(&contract_request_id)
        ),
        many => refuse!(
            "{} ObligationSettled events for obligation {obligation_index} — expected exactly one",
            many.len()
        ),
    };
    report.chain_settlement_tx = Some(hex::encode(event.tx_hash.as_bytes()));
    let Some(receipt) = rpc.transaction_receipt(event.tx_hash).await? else {
        refuse!(
            "the node has no receipt for the settlement transaction {}",
            event.tx_hash
        );
    };
    if !receipt.success {
        refuse!("the settlement transaction {} REVERTED", event.tx_hash);
    }
    if receipt.block_number != event.block_number {
        refuse!(
            "receipt block {} disagrees with the event block {}",
            receipt.block_number,
            event.block_number
        );
    }
    if !receipt.logs.iter().any(|l| {
        l.address == deployment.bridge_contract
            && l.topics.first() == Some(&obligation_settled_topic())
    }) {
        refuse!("the receipt carries no ObligationSettled event from the bridge contract");
    }
    match receipt.from {
        Some(from) if from == submitter => {}
        Some(from) => refuse!(
            "the settlement was sent by {} — not the configured submitter {}",
            from.to_checksum_string(),
            submitter.to_checksum_string()
        ),
        None => refuse!("the node did not report the settlement transaction's sender"),
    }
    let Some(summary) = rpc.transaction_by_hash(event.tx_hash).await? else {
        refuse!("the node does not know transaction {}", event.tx_hash);
    };
    if summary.from != submitter {
        refuse!(
            "transaction sender {} is not the configured submitter",
            summary.from.to_checksum_string()
        );
    }
    if summary.to != Some(deployment.bridge_contract) {
        refuse!("the settlement transaction was not addressed to the configured contract");
    }
    let confirmations = if head >= receipt.block_number {
        (head - receipt.block_number + 1) as i64
    } else {
        0
    };
    if confirmations < required_robinhood_confirmations {
        refuse!(
            "the settlement is only {confirmations} deep; {required_robinhood_confirmations} \
             required"
        );
    }

    // ---- 3. local settlement row ------------------------------------
    let rows = ledger.robinhood_txs_for_request(request_id)?;
    let settlements: Vec<_> = rows
        .iter()
        .filter(|t| t.kind == RobinhoodTxKind::Settlement)
        .collect();
    let others: Vec<_> = rows
        .iter()
        .filter(|t| t.kind != RobinhoodTxKind::Settlement)
        .map(|t| format!("{} #{} ({})", t.kind.as_str(), t.id, t.state.as_str()))
        .collect();
    let same_id = ledger.robinhood_txs_with_contract_request_id(contract_request_id)?;
    let same_obligation =
        ledger.requests_for_robinhood_obligation(expected_contract, obligation_index)?;
    report.duplicates = format!(
        "settlement rows for request: {}; rows with this requestId: {}; requests claiming \
         obligation {obligation_index}: {:?}",
        settlements.len(),
        same_id.len(),
        same_obligation
    );
    let tx = match settlements.as_slice() {
        [one] => *one,
        [] => refuse!("no Settlement operation row exists for request {request_id}"),
        many => refuse!(
            "{} Settlement rows for request {request_id} — duplicate",
            many.len()
        ),
    };
    report.local_settlement_state = tx.state.as_str().to_string();
    if !others.is_empty() {
        refuse!(
            "request carries other Robinhood operations: {}",
            others.join(", ")
        );
    }
    if same_id.len() != 1 || same_id[0].id != tx.id {
        refuse!(
            "{} operation rows carry requestId {} — duplicate operation",
            same_id.len(),
            hex::encode(&contract_request_id)
        );
    }
    if same_obligation != vec![request_id] {
        refuse!(
            "obligation {obligation_index} is claimed by requests {:?} — duplicate mapping",
            same_obligation
        );
    }
    if tx.bridge_contract != expected_contract {
        refuse!("the operation row is bound to another contract");
    }
    if tx.chain_id != deployment.chain_id.get() {
        refuse!(
            "the operation row is bound to chain {} not {}",
            tx.chain_id,
            deployment.chain_id.get()
        );
    }
    if tx.action != ACTION_SETTLE || tx.contract_request_id != contract_request_id {
        refuse!("the operation row's (action, requestId) is not this settlement's");
    }
    if tx.obligation_index != Some(obligation_index) {
        refuse!(
            "the operation row names obligation {:?}",
            tx.obligation_index
        );
    }
    if tx.submitter.map(EvmAddress::from_bytes) != Some(submitter) {
        refuse!("the operation row was signed for a different submitter");
    }
    match tx.nonce {
        Some(n) if n == summary.nonce => {}
        Some(n) => refuse!(
            "the operation row holds nonce {n} but the landed transaction used nonce {}",
            summary.nonce
        ),
        None => refuse!("the operation row never allocated a nonce"),
    }
    if tx.raw_tx.is_none() {
        refuse!("the operation row has no signed transaction persisted");
    }

    // ---- 4. destination payout --------------------------------------
    match request.direction {
        Direction::RhnToGlc => {
            // Two reads of the same row: the snapshot carries txid /
            // state / confirmations, the full row the destination hash.
            let Some(payout) = ledger.get_goldcoin_payout(request_id)? else {
                refuse!("no Goldcoin payout row exists for request {request_id}");
            };
            let Some(full) = ledger.get_goldcoin_payout_full(request_id)? else {
                refuse!("no Goldcoin payout row exists for request {request_id}");
            };
            let Some(txid) = payout.txid else {
                refuse!("the Goldcoin payout was never broadcast");
            };
            let dest_txid = ledger.get_destination_txid(request_id)?.unwrap_or_default();
            report.destination_payout = format!(
                "goldcoin txid {} {} amount {} confirmations {}",
                hex::encode(&txid),
                payout.state,
                payout.payout_atomic,
                payout.confirmations
            );
            if dest_txid != txid.to_vec() {
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
            // Version-agnostic on purpose: the payout builder decoded the
            // same string under the configured network; equality of the
            // 20-byte hash is what binds the payout to the recipient.
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
            let claimants = ledger.goldcoin_payout_requests_with_txid(&txid)?;
            if claimants != vec![request_id] {
                refuse!("Goldcoin txid is claimed by requests {claimants:?}");
            }
        }
        Direction::RhnToSol => {
            let Some(sig) = ledger.get_destination_txid(request_id)? else {
                refuse!("the request carries no Solana release signature");
            };
            report.destination_payout = format!(
                "solana release {} ({})",
                hex::encode(&sig),
                request.state.as_str()
            );
        }
        _ => unreachable!("routes filtered above"),
    }

    // ---- 5. conflicts ------------------------------------------------
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

    // ---- verdict -----------------------------------------------------
    if request.state == RequestState::Settled && tx.state == RobinhoodTxState::Finalized {
        report.verdict = Verdict::AlreadyReconciled;
        return Ok(report);
    }
    if request.state != RequestState::DestinationConfirmed {
        refuse!(
            "request is {} — only DestinationConfirmed is behind a chain-side Settled",
            request.state.as_str()
        );
    }
    if matches!(tx.state, RobinhoodTxState::Reverted) {
        refuse!("the operation row records a revert");
    }
    report.verdict = Verdict::SafeToReconcile(SettlementProof {
        request_id,
        tx_id: tx.id,
        route: request.direction,
        obligation_index,
        contract_request_id,
        chain_tx_hash: *event.tx_hash.as_bytes(),
        block_number: receipt.block_number,
        block_hash: *receipt.block_hash.as_bytes(),
        confirmations,
        nonce: summary.nonce,
        submitter,
    });
    Ok(report)
}

/// The write. Only ever called with a proof [`prove`] produced.
pub fn apply(
    ledger: &mut Ledger,
    proof: &SettlementProof,
    actor: &str,
    now: i64,
) -> Result<ReconcileOutcome, LedgerError> {
    ledger.reconcile_robinhood_settlement_from_chain(
        &crate::ledger::ChainSettlementEvidence {
            tx_id: proof.tx_id,
            request_id: proof.request_id,
            chain_tx_hash: proof.chain_tx_hash,
            block_number: proof.block_number,
            block_hash: proof.block_hash,
            confirmations: proof.confirmations,
        },
        actor,
        now,
    )
}

#[cfg(test)]
mod tests;

/// Convenience for `EvmTxHash` display in reports.
pub fn tx_hash_hex(hash: &EvmTxHash) -> String {
    hex::encode(hash.as_bytes())
}
