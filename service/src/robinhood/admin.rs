//! Operator inspection and recovery for the Robinhood leg.
//!
//! # What this module is for
//!
//! `docs/32-robinhood-settlement-phase-f.md` §13.G names what Phase F
//! deliberately left out: an entry point for `robinhood::begin_refund`
//! (implemented, tested, and with no production caller), controls for
//! requests parked in `ManualReview`, visibility into
//! `robinhood_transactions` and the submitter's nonce, and a way to clear
//! a halted indexer. This is that.
//!
//! # The shape every operation here takes
//!
//! **Inspect first, act second, and never act on anything an operator
//! typed.** Each recovery path below is a pair: a read-only assessment
//! that names every check individually as PASS or FAIL, and an execution
//! that re-runs the same checks against fresh state under the write lock.
//! The assessment is not a precondition for the execution — it is a
//! preview of it, and the execution never trusts that it ran.
//!
//! That mirrors what `glc-admin`'s existing recovery commands already do
//! for the Solana and Goldcoin legs (`refund-manual-review`,
//! `manual-review-settle`, `refund-glc-manual-review`): a strict
//! read-only dry run by default, `--execute` for the real thing, and no
//! flag anywhere that overrides a refused check.
//!
//! # What is deliberately NOT here
//!
//! - **No force-complete.** There is no function that marks a request
//!   Settled, moves it past a state it did not earn, or writes a
//!   completion without the chain evidence the settlement engine
//!   requires. A stuck operation is diagnosed, not overridden.
//! - **No balance movement.** Nothing here transfers, mints, sweeps or
//!   rebalances. The one value-moving path is a refund, and a refund
//!   chooses neither its recipient nor its amount — both are read from
//!   the obligation on-chain (`super::refund`).
//! - **No nonce rewrite.** The submitter's nonce state is READ here.
//!   Nothing sets, resets, skips or reallocates one: the allocator is the
//!   ledger's own maximum inside the same write transaction that stores
//!   it, and an operator editing that would reintroduce exactly the
//!   duplicate-broadcast window the design removes.
//! - **No refund recipient or amount parameter.** There is no argument to
//!   supply one, on any function here or on any CLI command that calls
//!   them.
//! - **No abandonment.** The on-chain path that closes an obligation
//!   while retaining a depositor's principal has no representation in
//!   this service and gains none here.
//!
//! # Everything is a projection
//!
//! Every view below is built from already-persisted rows plus, where a
//! function says so, a live chain read. Nothing is computed twice: the
//! eligibility checks call the same `Ledger` methods `begin_refund`
//! itself calls, so a listing can never claim a refund is available that
//! the execution would then refuse.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::{EvmAddress, EvmU256};
use crate::ledger::{
    Direction, Ledger, LedgerError, RequestState, RobinhoodHalt, RobinhoodHaltReason, RobinhoodTx,
    RobinhoodTxKind, RobinhoodTxState,
};
use crate::routes::Route;

// ===================================================================
// Named checks
// ===================================================================

/// One named safety check, and whether it holds.
///
/// The same shape `solana::refund`'s dry run already reports, for the
/// same reason: an operator needs to see WHICH condition refused, not a
/// single opaque verdict. `detail` states the observed values, so a
/// failing check explains itself without a second query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    pub fn fail(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            ok: false,
            detail: detail.into(),
        }
    }

    pub fn of(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
        Check {
            name,
            ok,
            detail: detail.into(),
        }
    }
}

/// Whether every check in a list holds. An EMPTY list is not eligible:
/// it means nothing was evaluated, and a verdict derived from no evidence
/// must never read as approval.
pub fn all_pass(checks: &[Check]) -> bool {
    !checks.is_empty() && checks.iter().all(|c| c.ok)
}

// ===================================================================
// ManualReview queue
// ===================================================================

/// One `RhnToGlc` request parked in `ManualReview`, with everything an
/// operator needs to choose between resuming and refunding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualReviewItem {
    pub request_id: i64,
    /// The Robinhood obligation this deposit created on-chain. Present on
    /// every folded `RhnToGlc` request; its absence is itself a finding.
    pub obligation_index: Option<u64>,
    pub reason: Option<String>,
    /// Canonical 8-decimal units, like every other ledger amount.
    pub gross_amount_atomic: u64,
    pub net_amount_atomic: u64,
    pub created_at: i64,
    /// The Goldcoin address the payout would go to, as recorded at fold.
    pub destination: String,
    /// Whether a refund or a settlement operation already exists for this
    /// request. The two are mutually exclusive, so at most one is true —
    /// and either being true means the decision has already been made.
    pub has_refund: bool,
    pub has_settlement: bool,
    /// The state of whichever operation exists, if one does.
    pub operation_state: Option<RobinhoodTxState>,
}

/// Every `RhnToGlc` request currently in `ManualReview`.
///
/// Deliberately scoped to the one inbound direction: a `GlcToRhn` request
/// parks for Goldcoin-side reasons and is served by the existing Goldcoin
/// tooling, and listing it here would invite an operator to reach for a
/// Robinhood refund for a deposit that never touched Robinhood.
pub fn manual_review_queue(ledger: &Ledger) -> Result<Vec<ManualReviewItem>, LedgerError> {
    let mut out = Vec::new();
    for request in ledger.requests_by_state(Direction::RhnToGlc, RequestState::ManualReview)? {
        let refund = ledger.get_robinhood_tx_for(RobinhoodTxKind::Refund, request.id)?;
        let settlement = ledger.get_robinhood_tx_for(RobinhoodTxKind::Settlement, request.id)?;
        let operation_state = refund.as_ref().or(settlement.as_ref()).map(|tx| tx.state);
        out.push(ManualReviewItem {
            request_id: request.id,
            obligation_index: request.source_obligation_index,
            reason: request.manual_review_note.clone(),
            gross_amount_atomic: request.gross_amount_atomic,
            net_amount_atomic: request.net_amount_atomic,
            created_at: request.created_at,
            destination: String::from_utf8_lossy(&request.recipient).into_owned(),
            has_refund: refund.is_some(),
            has_settlement: settlement.is_some(),
            operation_state,
        });
    }
    Ok(out)
}

// ===================================================================
// Transaction / nonce inspection
// ===================================================================

/// One Robinhood operation, projected for display.
///
/// Byte arrays are rendered as hex here rather than handed out raw, so no
/// caller has to decide how to spell a transaction hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxView {
    pub id: i64,
    pub kind: RobinhoodTxKind,
    pub state: RobinhoodTxState,
    /// The bridge request settled; `None` for a treasury withdrawal.
    pub request_id: Option<i64>,
    /// The rebalance request settled; `Some` exactly for a treasury
    /// withdrawal.
    pub rebalance_request_id: Option<i64>,
    /// `None` for a treasury withdrawal, which binds no route.
    pub route: Option<Route>,
    pub chain_id: u64,
    pub contract_request_id: String,
    pub obligation_index: Option<u64>,
    pub recipient: Option<String>,
    /// Robinhood 18-decimal atomic units, as a decimal string. `None` for
    /// a settlement, which moves nothing.
    pub amount_robinhood_atomic: Option<String>,
    pub signer_epoch: u64,
    pub expiry: u64,
    pub auth_digest: String,
    /// How many of the required signatures have been collected. Below
    /// [`super::SIGNER_THRESHOLD`] means the quorum did not form — the
    /// single most common reason an operation sits in `Authorizing`.
    pub signatures_collected: usize,
    pub signers: Vec<String>,
    pub submitter: Option<String>,
    pub nonce: Option<u64>,
    pub tx_hash: Option<String>,
    pub gas_limit: Option<u64>,
    pub fee_summary: Option<String>,
    /// Whether the signed bytes are persisted. A broadcast that is
    /// re-sent must send THESE bytes, never reconstructed ones.
    pub has_raw_tx: bool,
    pub broadcast_attempts: i64,
    pub replacement_attempts: i64,
    pub first_broadcast_at: Option<i64>,
    pub last_broadcast_at: Option<i64>,
    pub receipt_status: Option<i64>,
    pub receipt_block_number: Option<i64>,
    pub confirmations: i64,
    pub finalized_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn hex_bytes(bytes: &[u8]) -> String {
    crate::evm::hex::encode_lower(bytes)
}

/// Projects one stored operation, reading its signatures alongside.
pub fn tx_view(ledger: &Ledger, tx: &RobinhoodTx) -> Result<TxView, LedgerError> {
    let signatures = ledger.robinhood_auth_signatures(tx.id)?;
    Ok(TxView {
        id: tx.id,
        kind: tx.kind,
        state: tx.state,
        request_id: tx.request_id,
        rebalance_request_id: tx.rebalance_request_id,
        route: tx.route,
        chain_id: tx.chain_id,
        contract_request_id: hex_bytes(&tx.contract_request_id),
        obligation_index: tx.obligation_index,
        recipient: tx
            .recipient
            .map(|r| EvmAddress::from_bytes(r).to_checksum_string()),
        amount_robinhood_atomic: tx.amount_robinhood.map(|a| {
            // Rendered from the stored 256-bit word. A value too large
            // for the amount model is reported as its hex word rather
            // than silently truncated — it should be impossible, and
            // hiding it would be worse than showing it oddly.
            let word = EvmU256::from_be_bytes(a);
            match RobinhoodAtomic::try_from_u256(word) {
                Ok(amount) => amount.to_string(),
                Err(_) => word.to_word_hex(),
            }
        }),
        signer_epoch: tx.signer_epoch,
        expiry: tx.expiry,
        auth_digest: hex_bytes(&tx.auth_digest),
        signatures_collected: signatures.len(),
        signers: signatures
            .iter()
            .map(|s| EvmAddress::from_bytes(s.signer).to_checksum_string())
            .collect(),
        submitter: tx
            .submitter
            .map(|s| EvmAddress::from_bytes(s).to_checksum_string()),
        nonce: tx.nonce,
        tx_hash: tx.tx_hash.map(|h| hex_bytes(&h)),
        gas_limit: tx.gas_limit,
        fee_summary: tx.fee_summary.clone(),
        has_raw_tx: tx.raw_tx.is_some(),
        broadcast_attempts: tx.broadcast_attempts,
        replacement_attempts: tx.replacement_attempts,
        first_broadcast_at: tx.first_broadcast_at,
        last_broadcast_at: tx.last_broadcast_at,
        receipt_status: tx.receipt_status,
        receipt_block_number: tx.receipt_block_number,
        confirmations: tx.confirmations,
        finalized_at: tx.finalized_at,
        failure_reason: tx.failure_reason.clone(),
        created_at: tx.created_at,
        updated_at: tx.updated_at,
    })
}

/// Every operation recorded for one bridge request — at most one of each
/// kind, by the `ux_robinhood_tx_operation` unique index.
pub fn txs_for_request(ledger: &Ledger, request_id: i64) -> Result<Vec<TxView>, LedgerError> {
    let mut out = Vec::new();
    for kind in RobinhoodTxKind::ALL {
        if !kind.settles_a_bridge_request() {
            continue;
        }
        if let Some(tx) = ledger.get_robinhood_tx_for(kind, request_id)? {
            out.push(tx_view(ledger, &tx)?);
        }
    }
    Ok(out)
}

/// One treasury-withdrawal operation, for `glc-admin
/// robinhood-treasury-withdraw-status` and the admin API. Read-only;
/// amounts as decimal strings in BOTH units so a reader can never
/// mistake one for the other.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TreasuryWithdrawalView {
    pub operation_id: i64,
    pub rebalance_id: i64,
    pub state: String,
    pub rebalance_state: String,
    /// Robinhood 18-decimal atomic units.
    pub amount_atomic: String,
    /// Canonical 8-decimal atomic units, from the rebalance request.
    pub amount_canonical_atomic: String,
    pub amount_glc: String,
    /// The contract's `TREASURY` the authorization named.
    pub destination: String,
    pub contract_request_id: String,
    pub signer_epoch: u64,
    pub signatures_collected: usize,
    pub signers: Vec<String>,
    pub nonce: Option<u64>,
    pub tx_hash: Option<String>,
    pub receipt_status: Option<i64>,
    pub receipt_block_number: Option<i64>,
    pub confirmations: i64,
    pub finalized_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub rebalance_failure_reason: Option<String>,
    pub broadcast_attempts: i64,
    pub replacement_attempts: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Treasury-withdrawal operations, newest first, optionally narrowed to
/// one rebalance request or one operation.
pub fn treasury_withdrawal_views(
    ledger: &Ledger,
    rebalance_id: Option<i64>,
    operation_id: Option<i64>,
) -> Result<Vec<TreasuryWithdrawalView>, LedgerError> {
    let rows: Vec<RobinhoodTx> = match (rebalance_id, operation_id) {
        (Some(id), _) => ledger
            .get_robinhood_tx_for_rebalance(id)?
            .into_iter()
            .collect(),
        (None, Some(id)) => ledger
            .get_robinhood_tx(id)?
            .filter(|tx| tx.kind == RobinhoodTxKind::TreasuryWithdraw)
            .into_iter()
            .collect(),
        (None, None) => ledger.robinhood_treasury_withdrawals()?,
    };
    let mut out = Vec::with_capacity(rows.len());
    for tx in rows {
        let view = tx_view(ledger, &tx)?;
        let rebalance_id = tx.rebalance_request_id.unwrap_or(0);
        let rebalance = ledger.get_rebalance(rebalance_id)?;
        let canonical = rebalance.as_ref().map(|r| r.amount_atomic).unwrap_or(0);
        out.push(TreasuryWithdrawalView {
            operation_id: tx.id,
            rebalance_id,
            state: tx.state.as_str().to_string(),
            rebalance_state: rebalance
                .as_ref()
                .map(|r| r.state.as_str().to_string())
                .unwrap_or_else(|| "missing".to_string()),
            amount_atomic: view
                .amount_robinhood_atomic
                .clone()
                .unwrap_or_else(|| "0".to_string()),
            amount_canonical_atomic: canonical.to_string(),
            amount_glc: crate::chain_policy::human::format_glc(canonical),
            destination: view.recipient.clone().unwrap_or_default(),
            contract_request_id: view.contract_request_id.clone(),
            signer_epoch: tx.signer_epoch,
            signatures_collected: view.signatures_collected,
            signers: view.signers.clone(),
            nonce: tx.nonce,
            tx_hash: view.tx_hash.clone(),
            receipt_status: tx.receipt_status,
            receipt_block_number: tx.receipt_block_number,
            confirmations: tx.confirmations,
            finalized_at: tx.finalized_at,
            failure_reason: tx.failure_reason.clone(),
            rebalance_failure_reason: rebalance.and_then(|r| r.failure_reason),
            broadcast_attempts: tx.broadcast_attempts,
            replacement_attempts: tx.replacement_attempts,
            created_at: tx.created_at,
            updated_at: tx.updated_at,
        });
    }
    Ok(out)
}

/// Every operation that is neither finalized nor terminally failed —
/// the working set an operator is actually asked about.
pub fn open_operations(ledger: &Ledger) -> Result<Vec<TxView>, LedgerError> {
    let mut out = Vec::new();
    for state in [
        RobinhoodTxState::Authorizing,
        RobinhoodTxState::Authorized,
        RobinhoodTxState::Signed,
        RobinhoodTxState::Broadcast,
        RobinhoodTxState::Included,
    ] {
        for tx in ledger.robinhood_txs_in_state(state)? {
            out.push(tx_view(ledger, &tx)?);
        }
    }
    out.sort_by_key(|t| t.id);
    Ok(out)
}

/// Operations that have stopped and will not resume without a human:
/// reverted on-chain, or moved to `ManualReview` by a post-broadcast
/// contradiction.
pub fn stalled_operations(ledger: &Ledger) -> Result<Vec<TxView>, LedgerError> {
    let mut out = Vec::new();
    for state in [RobinhoodTxState::Reverted, RobinhoodTxState::ManualReview] {
        for tx in ledger.robinhood_txs_in_state(state)? {
            out.push(tx_view(ledger, &tx)?);
        }
    }
    out.sort_by_key(|t| t.id);
    Ok(out)
}

/// The submitter's nonce and broadcast picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitterState {
    pub submitter: String,
    pub chain_id: u64,
    /// The highest nonce this ledger has allocated, across operations in
    /// EVERY state — the same figure
    /// [`Ledger::allocate_robinhood_nonce`] reads to pick the next one,
    /// so this reports the number the allocator will actually act on.
    /// `None` when none has ever been allocated.
    pub highest_allocated_nonce: Option<u64>,
    /// The last `eth_getTransactionCount(..., "pending")` this service
    /// recorded, and when. A RECONCILIATION INPUT and a floor, never the
    /// allocator — see `super::submitter`.
    pub observed_pending_nonce: Option<u64>,
    pub observed_at: Option<i64>,
    /// Operations holding a nonce that has not reached a receipt: the set
    /// that must resolve before the picture is clean.
    pub in_flight: Vec<TxView>,
}

/// Reads the submitter's nonce state. Performs NO chain read and writes
/// nothing — `observed_pending_nonce` is whatever the settlement loop
/// last recorded, which is the honest answer to "what does this service
/// believe".
pub fn submitter_state(
    ledger: &Ledger,
    submitter: EvmAddress,
    chain_id: u64,
) -> Result<SubmitterState, LedgerError> {
    let observed = ledger.evm_submitter_nonce(submitter.to_bytes(), chain_id)?;
    let mut in_flight: Vec<TxView> = open_operations(ledger)?
        .into_iter()
        .filter(|t| t.nonce.is_some() && t.chain_id == chain_id)
        .collect();
    in_flight.sort_by_key(|t| t.nonce);
    // Read from the ledger rather than derived from `in_flight`: a
    // finalized operation's nonce is still spent, and a maximum taken
    // over unresolved rows alone would under-report it the moment they
    // resolve — showing a nonce gap that does not exist.
    let highest_allocated_nonce = ledger.highest_robinhood_nonce(submitter.to_bytes(), chain_id)?;
    Ok(SubmitterState {
        submitter: submitter.to_checksum_string(),
        chain_id,
        highest_allocated_nonce,
        observed_pending_nonce: observed.map(|(n, _)| n),
        observed_at: observed.map(|(_, at)| at),
        in_flight,
    })
}

// ===================================================================
// Refund eligibility
// ===================================================================

/// The ledger-side verdict on refunding one request.
///
/// "Ledger-side" is the important qualifier: it evaluates everything that
/// can be decided from persisted state, and says so explicitly about the
/// half it cannot. The obligation's on-chain status, its depositor and
/// its principal are read from the contract by
/// [`super::refund::begin_refund`] itself, immediately before the
/// authorization is built — so this assessment can say "the ledger has no
/// objection", never "this will succeed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundAssessment {
    pub request_id: i64,
    pub obligation_index: Option<u64>,
    /// Canonical 8-decimal gross. Shown for context ONLY: the refunded
    /// amount is the obligation's own on-chain principal, which this
    /// service does not choose and does not read from here.
    pub gross_amount_atomic: u64,
    pub checks: Vec<Check>,
    /// True when every ledger-side check holds. Not a promise: the chain
    /// half is still ahead.
    pub ledger_eligible: bool,
    /// Set when a refund has already been begun — the operation to look
    /// at rather than a second one to start.
    pub existing_refund: Option<TxView>,
}

/// Assesses a refund without writing anything or contacting any chain.
///
/// Every check below mirrors one `begin_refund` performs, and calls the
/// same `Ledger` method it calls, so the two cannot drift into
/// disagreement.
pub fn refund_assessment(
    ledger: &Ledger,
    request_id: i64,
) -> Result<RefundAssessment, LedgerError> {
    let mut checks = Vec::new();

    let Some(request) = ledger.get_request(request_id)? else {
        return Ok(RefundAssessment {
            request_id,
            obligation_index: None,
            gross_amount_atomic: 0,
            checks: vec![Check::fail("request_exists", "no such bridge request")],
            ledger_eligible: false,
            existing_refund: None,
        });
    };

    // An already-begun refund is not a failure — it is the answer.
    let existing = ledger.get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)?;
    let existing_refund = match &existing {
        Some(tx) => Some(tx_view(ledger, tx)?),
        None => None,
    };

    checks.push(Check::of(
        "direction_is_rhn_to_glc",
        request.direction == Direction::RhnToGlc,
        format!(
            "request is {} (only a Robinhood-sourced deposit can be refunded on Robinhood)",
            request.direction.as_str()
        ),
    ));
    checks.push(Check::of(
        "state_is_manual_review",
        request.state == RequestState::ManualReview,
        format!(
            "state is {} (a deposit is refunded from ManualReview, after a human has decided \
             against completing it)",
            request.state.as_str()
        ),
    ));
    checks.push(Check::of(
        "names_an_obligation",
        request.source_obligation_index.is_some(),
        match request.source_obligation_index {
            Some(index) => format!("obligation {index}"),
            None => "no source_obligation_index recorded".to_string(),
        },
    ));

    // Mutual exclusion with settlement, the failure that loses real
    // money. Checked against the SAME table `begin_refund` checks.
    let settlement = ledger.get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)?;
    checks.push(Check::of(
        "no_settlement_exists",
        settlement.is_none(),
        match &settlement {
            Some(tx) => format!(
                "a settlement operation exists (id {}, state {}) — its obligation was closed as \
                 paid out on Goldcoin, and refunding now would return principal the bridge \
                 already delivered",
                tx.id,
                tx.state.as_str()
            ),
            None => "no settlement operation recorded".to_string(),
        },
    ));

    // And against a different table: a Goldcoin payout that reached the
    // chain means the depositor was paid.
    let payout = ledger.get_goldcoin_payout(request_id)?;
    let paid_out = payout.as_ref().is_some_and(|p| p.txid.is_some());
    checks.push(Check::of(
        "no_confirmed_goldcoin_payout",
        !paid_out,
        if paid_out {
            "a Goldcoin payout transaction exists — the depositor has been paid, so the \
             principal is not theirs to have back"
                .to_string()
        } else {
            "no Goldcoin payout transaction".to_string()
        },
    ));

    checks.push(Check::of(
        "no_refund_already_begun",
        existing.is_none(),
        match &existing_refund {
            Some(tx) => format!(
                "refund operation {} already exists in state {} — resume it rather than \
                 beginning a second",
                tx.id,
                tx.state.as_str()
            ),
            None => "no refund operation recorded".to_string(),
        },
    ));

    // Stated as a check so it appears in the operator's list rather than
    // only in prose: the chain half has not been evaluated here.
    checks.push(Check::pass(
        "chain_checks_deferred",
        "the obligation's on-chain status, depositor and principal are read from the contract \
         at execution time and are NOT evaluated by this assessment",
    ));

    let ledger_eligible = all_pass(&checks);
    Ok(RefundAssessment {
        request_id,
        obligation_index: request.source_obligation_index,
        gross_amount_atomic: request.gross_amount_atomic,
        checks,
        ledger_eligible,
        existing_refund,
    })
}

/// The mirror assessment for settlement: whether this request is one the
/// engine could still settle, or whether that door is closed.
///
/// Settlement and refund are opposite, irreversible answers to the same
/// question, so an operator choosing between them needs both verdicts
/// side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementAssessment {
    pub request_id: i64,
    pub checks: Vec<Check>,
    pub ledger_eligible: bool,
    pub existing_settlement: Option<TxView>,
}

pub fn settlement_assessment(
    ledger: &Ledger,
    request_id: i64,
) -> Result<SettlementAssessment, LedgerError> {
    let mut checks = Vec::new();
    let Some(request) = ledger.get_request(request_id)? else {
        return Ok(SettlementAssessment {
            request_id,
            checks: vec![Check::fail("request_exists", "no such bridge request")],
            ledger_eligible: false,
            existing_settlement: None,
        });
    };
    let existing = ledger.get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)?;
    let existing_settlement = match &existing {
        Some(tx) => Some(tx_view(ledger, tx)?),
        None => None,
    };
    let refund = ledger.get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)?;

    checks.push(Check::of(
        "direction_is_rhn_to_glc",
        request.direction == Direction::RhnToGlc,
        request.direction.as_str().to_string(),
    ));
    checks.push(Check::of(
        "no_refund_exists",
        refund.is_none(),
        match &refund {
            Some(tx) => format!(
                "a refund operation exists (id {}, state {}) — a refunded deposit is never \
                 settled",
                tx.id,
                tx.state.as_str()
            ),
            None => "no refund operation recorded".to_string(),
        },
    ));
    // The load-bearing ordering property: a settlement asserts the
    // depositor has been paid on Goldcoin, so it is only reachable once
    // the Goldcoin payout is CONFIRMED.
    checks.push(Check::of(
        "goldcoin_payout_confirmed",
        request.state == RequestState::DestinationConfirmed
            || request.state == RequestState::Settled,
        format!(
            "state is {} — a settlement is only authorized from DestinationConfirmed, because \
             it is an irreversible statement that the depositor has been paid on Goldcoin",
            request.state.as_str()
        ),
    ));
    let ledger_eligible = all_pass(&checks);
    Ok(SettlementAssessment {
        request_id,
        checks,
        ledger_eligible,
        existing_settlement,
    })
}

// ===================================================================
// Indexer halt
// ===================================================================

/// The halt picture: the halt itself plus the context that decides
/// whether clearing it is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaltState {
    pub halt: Option<RobinhoodHalt>,
    /// The durable scan cursor — where the indexer would resume.
    pub cursor_block: Option<u64>,
    pub cursor_block_hash: Option<String>,
    /// How many retained anchors the reorg walk still has to work with.
    pub retained_anchors: usize,
    pub observations: crate::ledger::RobinhoodObservationSummary,
    /// Finalized observations that have been folded into a bridge
    /// request. These are the ones a post-finality reorg would have
    /// invalidated, and the reason clearing such a halt needs a human's
    /// explicit acknowledgement.
    pub folded_final_observations: usize,
}

pub fn halt_state(ledger: &Ledger) -> Result<HaltState, LedgerError> {
    let cursor = ledger.robinhood_scan_cursor()?;
    let anchors = ledger.robinhood_scan_anchors_desc()?;
    let observations = ledger.robinhood_observation_summary()?;
    // Derived rather than queried: `RobinhoodObservationRow` does not
    // expose `folded_request_id`, and the two counts the ledger DOES
    // publish give the same answer — every Final observation is either
    // still unfolded or has become a bridge request.
    //
    // Saturating: the two reads are separate statements and a fold
    // committing between them would make the subtraction negative for an
    // instant. That is a stale count, not a fault, and reporting zero is
    // the conservative direction for a figure whose only use is to make
    // an operator look harder.
    let unfolded = ledger.unfolded_final_robinhood_observations()?.len();
    let folded_final_observations = usize::try_from(observations.finalized)
        .unwrap_or(0)
        .saturating_sub(unfolded);
    Ok(HaltState {
        halt: ledger.robinhood_halt()?,
        cursor_block: cursor.map(|(b, _)| b),
        cursor_block_hash: cursor.map(|(_, h)| hex_bytes(&h)),
        retained_anchors: anchors.len(),
        observations,
        folded_final_observations,
    })
}

/// What an operator must state before a halt is cleared.
///
/// Deliberately a struct of explicit acknowledgements rather than one
/// `--force`. A halt means the indexer recorded, or was about to record,
/// something it could not stand behind; clearing it is a claim that the
/// underlying condition is gone. Each field below is a distinct claim,
/// and the operator has to make the right one.
#[derive(Debug, Clone, Default)]
pub struct HaltClearance {
    /// The reason the operator believes they are clearing. Must equal the
    /// stored one.
    ///
    /// This is the check that stops "make the red light go away": an
    /// operator who has diagnosed a chain-id mismatch and repointed the
    /// endpoint will name `ChainIdMismatch`, and if the stored halt is
    /// actually a post-finality reorg they find that out here rather than
    /// after the indexer re-halts.
    pub expect_reason: Option<RobinhoodHaltReason>,
    /// Required for the two reorg reasons: an explicit statement that the
    /// operator has reviewed the finalized observations a reorg may have
    /// invalidated.
    pub acknowledge_orphaned_finality: bool,
    /// Required when the endpoint's identity was the problem: an explicit
    /// statement that the endpoint now reports the configured chain id.
    /// The CLI sets this only after re-reading it from the chain.
    pub endpoint_reverified: bool,
}

/// Why a halt clearance was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HaltClearError {
    #[error("the Robinhood indexer is not halted — there is nothing to clear")]
    NotHalted,
    #[error(
        "this deployment's halt is {stored}, but the clearance names {named}. Clearing a halt \
         is a claim that its specific cause is gone; naming a different one means the cause has \
         not been diagnosed"
    )]
    ReasonMismatch {
        stored: &'static str,
        named: &'static str,
    },
    #[error(
        "clearing a {reason} halt requires naming the reason explicitly — a halt must never be \
         cleared by an operator who has not said which one they are clearing"
    )]
    ReasonNotNamed { reason: &'static str },
    #[error(
        "this halt means a block the indexer called irreversible was not. {folded} finalized \
         observation(s) have already been folded into bridge requests and may rest on orphaned \
         blocks; clearing requires reviewing them and acknowledging explicitly"
    )]
    OrphanedFinalityNotAcknowledged { folded: usize },
    #[error(
        "this halt means the endpoint was not the configured network. Clearing without \
         re-verifying its chain id would simply re-halt on the next tick"
    )]
    EndpointNotReverified,
    #[error(
        "{count} Robinhood operation(s) are still in flight. The indexer's view of the chain is \
         what a broadcast is verified against, so a halt is not cleared while an unresolved \
         operation depends on it — resolve those first"
    )]
    OperationsInFlight { count: usize },
}

/// Assesses a halt clearance without performing it.
///
/// Returns the checks in the same shape every other assessment here uses,
/// so an operator sees the same PASS/FAIL list before and after.
pub fn halt_clear_assessment(
    ledger: &Ledger,
    clearance: &HaltClearance,
) -> Result<Vec<Check>, LedgerError> {
    let state = halt_state(ledger)?;
    let in_flight = open_operations(ledger)?;
    Ok(halt_clear_checks(&state, &in_flight, clearance))
}

fn halt_clear_checks(
    state: &HaltState,
    in_flight: &[TxView],
    clearance: &HaltClearance,
) -> Vec<Check> {
    let mut checks = Vec::new();

    let Some(halt) = &state.halt else {
        checks.push(Check::fail(
            "indexer_is_halted",
            "the indexer is not halted — there is nothing to clear",
        ));
        return checks;
    };
    checks.push(Check::pass(
        "indexer_is_halted",
        format!(
            "halted at {} for {}: {}",
            halt.halted_at,
            halt.reason.as_str(),
            halt.detail
        ),
    ));

    match clearance.expect_reason {
        None => checks.push(Check::fail(
            "reason_named",
            format!(
                "the stored halt is {} and the clearance names nothing — a halt must never be \
                 cleared by an operator who has not said which one they are clearing",
                halt.reason.as_str()
            ),
        )),
        Some(named) => checks.push(Check::of(
            "reason_named",
            named == halt.reason,
            format!(
                "clearance names {}, stored halt is {}",
                named.as_str(),
                halt.reason.as_str()
            ),
        )),
    }

    // An unresolved broadcast is verified against the indexer's view of
    // the chain. Clearing a halt underneath one changes what that view
    // is.
    checks.push(Check::of(
        "no_operations_in_flight",
        in_flight.is_empty(),
        if in_flight.is_empty() {
            "no Robinhood operation is in flight".to_string()
        } else {
            format!(
                "{} operation(s) in flight: {}",
                in_flight.len(),
                in_flight
                    .iter()
                    .map(|t| format!("#{} ({})", t.id, t.state.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    ));

    match halt.reason {
        RobinhoodHaltReason::PostFinalityReorg
        | RobinhoodHaltReason::ReorgBeyondRetainedAnchors => {
            checks.push(Check::of(
                "orphaned_finality_acknowledged",
                clearance.acknowledge_orphaned_finality,
                format!(
                    "{} finalized observation(s) have been folded into bridge requests and may \
                     rest on orphaned blocks; the operator must review and acknowledge them",
                    state.folded_final_observations
                ),
            ));
        }
        RobinhoodHaltReason::ChainIdMismatch | RobinhoodHaltReason::UnexpectedContractRoute => {
            checks.push(Check::of(
                "endpoint_reverified",
                clearance.endpoint_reverified,
                "the endpoint must be re-read and confirmed to be the configured network and \
                 contract before this halt is cleared, or the next tick simply re-halts"
                    .to_string(),
            ));
        }
        RobinhoodHaltReason::ObservationConflict => {
            // Nothing extra to acknowledge beyond naming the reason: the
            // conflicting rows are already in the observation table and
            // the indexer will re-halt on the next tick if the conflict
            // has not been resolved at its source. Stated as a passing
            // check so the operator sees it was considered.
            checks.push(Check::pass(
                "conflict_is_self_reasserting",
                "two events claim one durable identity and disagree; if that is still true, the \
                 next tick halts again — clearing is safe but may not be effective",
            ));
        }
    }

    checks
}

/// Clears the halt, re-running every check under the same read.
///
/// The assessment above is a preview, not a precondition: this function
/// evaluates the checks itself and refuses on any failure, so a clearance
/// can never be performed on the strength of an assessment taken earlier
/// against different state.
pub fn clear_halt(
    ledger: &mut Ledger,
    clearance: &HaltClearance,
    now: i64,
) -> Result<Vec<Check>, HaltClearErrorOrLedger> {
    let state = halt_state(ledger)?;
    let in_flight = open_operations(ledger)?;
    let checks = halt_clear_checks(&state, &in_flight, clearance);

    let Some(halt) = &state.halt else {
        return Err(HaltClearError::NotHalted.into());
    };
    if !in_flight.is_empty() {
        return Err(HaltClearError::OperationsInFlight {
            count: in_flight.len(),
        }
        .into());
    }
    match clearance.expect_reason {
        None => {
            return Err(HaltClearError::ReasonNotNamed {
                reason: halt.reason.as_str(),
            }
            .into())
        }
        Some(named) if named != halt.reason => {
            return Err(HaltClearError::ReasonMismatch {
                stored: halt.reason.as_str(),
                named: named.as_str(),
            }
            .into())
        }
        Some(_) => {}
    }
    match halt.reason {
        RobinhoodHaltReason::PostFinalityReorg
        | RobinhoodHaltReason::ReorgBeyondRetainedAnchors
            if !clearance.acknowledge_orphaned_finality =>
        {
            return Err(HaltClearError::OrphanedFinalityNotAcknowledged {
                folded: state.folded_final_observations,
            }
            .into())
        }
        RobinhoodHaltReason::ChainIdMismatch | RobinhoodHaltReason::UnexpectedContractRoute
            if !clearance.endpoint_reverified =>
        {
            return Err(HaltClearError::EndpointNotReverified.into())
        }
        _ => {}
    }

    ledger.robinhood_clear_halt(now)?;
    Ok(checks)
}

/// Either a refused clearance or a storage failure.
#[derive(Debug, thiserror::Error)]
pub enum HaltClearErrorOrLedger {
    #[error(transparent)]
    Refused(#[from] HaltClearError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

// ===================================================================
// Route status
// ===================================================================

/// One route's availability, decomposed into the gates that decide it.
///
/// # Why the decomposition rather than one boolean
///
/// `crate::routes::RouteGate` is a three-place AND, and the public API
/// deliberately collapses it to one cause-agnostic message for end users
/// ([`crate::routes::RouteGateError::UNAVAILABLE_MESSAGE`]). An OPERATOR
/// debugging a route that will not open needs the opposite: which gate
/// refused. This type is that view, and it exists only behind the
/// authenticated admin surface and the operator CLI.
///
/// `health_reason` is separate from `disabled_reason` on purpose. A route
/// can be enabled at every gate and still not be usable — a halted
/// indexer, a chain-id disagreement, an unformable signer quorum. Merging
/// the two would tell an operator to go looking at configuration when the
/// problem is the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStatus {
    pub route: &'static str,
    pub source_chain: &'static str,
    pub destination_chain: &'static str,
    /// Whether settlement machinery exists for this route at all
    /// (`Route::as_direction().is_some()`). `false` means structurally
    /// inert in this build, not merely switched off.
    pub implemented: bool,
    /// The service-side three-place AND: config, ledger, adapter.
    pub service_enabled: bool,
    /// The contract's own `routeEnabled(route)`, when a live read
    /// supplied one. `None` means not read — never assumed either way.
    pub contract_route_enabled: Option<bool>,
    /// Enabled by BOTH the service and the contract, and healthy. The
    /// only field a caller should treat as "a transfer could happen".
    pub effective_available: bool,
    /// Which gate refused, in operator terms. `None` when enabled.
    pub disabled_reason: Option<String>,
    /// Why the route is unusable despite being enabled. `None` when
    /// healthy.
    pub health_reason: Option<String>,
}

/// Everything that can make an enabled Robinhood route unusable.
///
/// Passed in rather than read here so that one evaluation serves the
/// health endpoint, the admin API and the CLI identically — a second
/// implementation is how two surfaces come to disagree about whether a
/// route is up.
#[derive(Debug, Clone, Default)]
pub struct RobinhoodReadiness {
    /// A `[robinhood.settlement]` section is present AND its startup
    /// preflight passed.
    pub deployment_verified: bool,
    pub signers_available: usize,
    pub signers_required: usize,
    pub halted: Option<RobinhoodHaltReason>,
    /// The indexer's last observed chain id disagreed with the
    /// configured one.
    pub chain_id_disagrees: bool,
    /// The indexer has never completed a tick against the endpoint.
    pub never_connected: bool,
    /// The local `RobinhoodReserve` pause. A paused reserve cannot be
    /// drawn against, so a `GlcToRhn` payout cannot be funded — the route
    /// is enabled and unusable, which is a HEALTH fact rather than a
    /// configuration one.
    pub reserve_paused: bool,
    /// `[reserve.robinhood]` is absent, so the reserve has no
    /// `reserve_ledger` row at all. Distinct from `reserve_paused`:
    /// "does not exist" and "exists and is paused" are different
    /// operator problems, and the fail-closed default is the former.
    pub reserve_unconfigured: bool,
}

impl RobinhoodReadiness {
    pub fn signer_quorum_available(&self) -> bool {
        self.signers_required > 0 && self.signers_available >= self.signers_required
    }

    /// The first reason this leg is unusable, or `None`.
    ///
    /// Ordered most-fundamental first, so an operator is pointed at the
    /// root cause rather than at a symptom of it: a deployment that never
    /// verified has no signer set to have a quorum of.
    pub fn health_reason(&self) -> Option<String> {
        if !self.deployment_verified {
            return Some(
                "no verified Robinhood deployment in this process: either no \
                 [robinhood.settlement] section, or its startup preflight did not pass"
                    .to_string(),
            );
        }
        if let Some(reason) = self.halted {
            return Some(format!(
                "the Robinhood indexer is HALTED ({}) and is observing nothing",
                reason.as_str()
            ));
        }
        if self.chain_id_disagrees {
            return Some(
                "the endpoint's chain id disagrees with the configured one — this is not the \
                 network this deployment settles on"
                    .to_string(),
            );
        }
        if !self.signer_quorum_available() {
            return Some(format!(
                "{} of the {} authorization signers a quorum requires are available — no \
                 Robinhood operation can be authorized",
                self.signers_available, self.signers_required
            ));
        }
        if self.reserve_unconfigured {
            return Some(
                "no [reserve.robinhood] section: the Robinhood reserve has no reserve_ledger \
                 row, so nothing can be reserved against it and no Robinhood settlement can \
                 pass admission"
                    .to_string(),
            );
        }
        if self.reserve_paused {
            return Some(
                "the RobinhoodReserve is PAUSED locally — it cannot be drawn against".to_string(),
            );
        }
        if self.never_connected {
            return Some(
                "the Robinhood endpoint has not answered a tick yet in this process".to_string(),
            );
        }
        None
    }
}

/// Builds the per-route status for all four Robinhood routes.
///
/// `contract_route_enabled` is a lookup the caller supplies from a live
/// read, or one that always returns `None` when no chain read was made.
/// It is never defaulted to `true`: a route this service cannot confirm
/// the contract has opened is not reported as open.
pub fn route_status<F>(
    ledger: &Ledger,
    gate: &crate::routes::RouteGate,
    readiness: &RobinhoodReadiness,
    mut contract_route_enabled: F,
) -> Vec<RouteStatus>
where
    F: FnMut(Route) -> Option<bool>,
{
    let health_reason = readiness.health_reason();
    Route::ALL
        .iter()
        .filter(|r| {
            matches!(
                r,
                Route::GlcToRhn | Route::RhnToGlc | Route::SolToRhn | Route::RhnToSol
            )
        })
        .map(|route| {
            let route = *route;
            let service_enabled = gate.is_enabled(ledger, route);
            let disabled_reason = match gate.ensure_enabled(ledger, route) {
                Ok(()) => None,
                Err(crate::routes::RouteGateError::Disabled {
                    disabled_by,
                    reason,
                    ..
                }) => Some(format!("{}: {reason}", disabled_by.as_str())),
                Err(e) => Some(e.to_string()),
            };
            let contract = contract_route_enabled(route);
            // Availability requires BOTH gates to say yes and nothing to
            // be wrong. An unread contract flag is not a yes.
            let effective_available = service_enabled
                && contract == Some(true)
                && health_reason.is_none()
                && route.as_direction().is_some();
            RouteStatus {
                route: route.as_str(),
                source_chain: route.source_chain().as_str(),
                destination_chain: route.destination_chain().as_str(),
                implemented: route.as_direction().is_some(),
                service_enabled,
                contract_route_enabled: contract,
                effective_available,
                disabled_reason,
                health_reason: health_reason.clone(),
            }
        })
        .collect()
}

// ===================================================================
// Reserve reporting
// ===================================================================

/// The Robinhood reserve, as a THIRD independent reserve.
///
/// Never netted against the Goldcoin or Solana reserve, and never
/// presented alongside them as one figure: they are different physical
/// pools on different chains, and a combined number would imply a
/// fungibility that does not exist.
///
/// Ledger amounts are canonical 8-decimal units, like every other reserve
/// row — NOT Robinhood's native 18 decimals. At 18 decimals one whole GLC
/// is 10^18, so an `i64` column would overflow on a single real transfer;
/// the two units differ by an exact factor of 10^10 and every amount
/// crossing the boundary must be an exact multiple of it. The on-chain
/// figures below are in the contract's own 18-decimal units and are
/// labelled as such rather than converted, because a rolling limit is not
/// an amount this service ever pays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveReport {
    /// Canonical 8dp, from `reserve_ledger`.
    pub balance_atomic: u64,
    pub protected_minimum_atomic: u64,
    pub reserved_liquidity_atomic: u64,
    /// Liquidity committed to SourceFinalized-or-later requests — the
    /// pending OUTBOUND obligations this reserve owes.
    pub pending_obligations_atomic: u64,
    pub accrued_fees_atomic: u64,
    /// `balance - protected_minimum - reserved`, signed: a negative value
    /// is itself diagnostic and is not clamped.
    pub available_capacity_atomic: i64,
    pub invariant_holds: bool,
    pub paused: bool,
    /// The on-chain half, when a live read supplied it.
    pub onchain: Option<OnchainReserveReport>,
}

/// The contract's own view of the same reserve, in Robinhood 18-decimal
/// atomic units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnchainReserveReport {
    /// The bridge contract's ERC-20 balance.
    pub token_balance: EvmU256,
    /// The protected floor plus every unsettled depositor's principal:
    /// GLC that is physically present but is not the bridge's to pay out.
    pub encumbered_reserve: EvmU256,
    pub protected_min_reserve: EvmU256,
    /// Rolling limits and what remains of each direction's CURRENT
    /// bucket. Per DIRECTION, shared by both routes on that side — see
    /// [`super::calls::BridgeLimits`].
    pub inbound_rolling_limit: EvmU256,
    pub inbound_used: EvmU256,
    pub inbound_remaining: EvmU256,
    pub inbound_window_resets_at: u64,
    pub outbound_rolling_limit: EvmU256,
    pub outbound_used: EvmU256,
    pub outbound_remaining: EvmU256,
    pub outbound_window_resets_at: u64,
}

/// Reads the Robinhood reserve's ledger side.
///
/// `None` when `[reserve.robinhood]` was never configured, which is every
/// production config today: an unconfigured reserve has no
/// `reserve_ledger` row at all, so nothing can be reserved against it.
/// The fail-closed default is "this reserve does not exist", not "this
/// reserve is empty" — and returning `Some(zeroes)` would say the latter.
pub fn reserve_report(ledger: &Ledger, now: i64) -> Result<Option<ReserveReport>, LedgerError> {
    use crate::ledger::ReserveDirection;
    // "Configured" is detected through the ledger's own typed error
    // rather than a separate existence probe: `reserve_snapshot` already
    // distinguishes "no row" from every other failure, and a second
    // probe could answer differently from the read that follows it.
    let snapshot =
        match crate::ops::reserve_health::check(ledger, ReserveDirection::RobinhoodReserve, now) {
            Ok(snapshot) => snapshot,
            Err(LedgerError::ReserveNotInitialized(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
    Ok(Some(ReserveReport {
        balance_atomic: snapshot.total_reserve_balance,
        protected_minimum_atomic: snapshot.protected_minimum,
        reserved_liquidity_atomic: snapshot.reserved_liquidity,
        pending_obligations_atomic: snapshot.pending_obligations,
        accrued_fees_atomic: snapshot.accrued_fees,
        available_capacity_atomic: snapshot.confirmed_admission_headroom,
        invariant_holds: snapshot.invariant_holds,
        paused: snapshot.paused,
        onchain: None,
    }))
}

#[cfg(test)]
mod tests;
