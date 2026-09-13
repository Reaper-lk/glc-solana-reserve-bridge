//! Chain/ledger obligation reconciliation for one `GlcRobinhoodBridge`
//! deployment: every obligation the contract holds, set against the
//! ledger row that claims it, with every disagreement named.
//!
//! # What "disagreement" means
//!
//! An obligation's on-chain status is the contract's own record of what
//! happened to one user's deposit: `Pending` (in custody, refundable),
//! `Settled` (paid out on the destination network, no longer refundable),
//! `Refunded` (principal returned), `Abandoned` (closed administratively,
//! principal retained). The ledger holds the same story from this
//! service's side, as the request's state plus its `robinhood_
//! transactions` rows. The two are written by different actors at
//! different moments, and every path that moves value already re-reads
//! the chain immediately before acting — so a disagreement here is never
//! a live race. It is one of exactly these:
//!
//! - **the chain moved and the ledger did not** — an obligation reached a
//!   terminal status through a transaction this service did not record
//!   (a predecessor deployment, a hand-assembled governance transaction,
//!   a database restore that lost the row's later history);
//! - **the ledger moved and the chain did not** — a row claims `Settled`
//!   or `Refunded` while the obligation is still `Pending`, meaning the
//!   refund path the contract offers is still OPEN for a deposit the
//!   ledger believes is closed (the double-payment shape);
//! - **a deposit the ledger never saw** — an obligation with no row at
//!   all (the V1 #29/#30 shape of 2026-09-12: deposits on a contract the
//!   indexer was no longer watching).
//!
//! # What this never does
//!
//! It reads. Its RPC bound is [`EvmCallRpc`] alone. It writes nothing to
//! the ledger, proposes nothing, and repairs nothing: a finding is an
//! input to a human decision (docs/09-runbook.md), and the tooling that
//! acts on one (`robinhood-refund`, `manual-review-process`,
//! `robinhood-recover-deposit`) carries its own predicates.
//!
//! # Predecessor contracts
//!
//! The audit is against ONE contract, named by the caller. The
//! configured deployment is the usual subject; a predecessor that still
//! holds obligations (V1 held 1.56M GLC and six `Pending` obligations
//! when this was written) is audited by naming it explicitly. Ledger
//! rows whose `source_contract` is neither the audited contract nor
//! absent are reported as [`Verdict::ForeignRow`] so an operator sees
//! that a second audit, against that contract, is owed.

use std::collections::BTreeMap;

use crate::evm::EvmAddress;
use crate::ledger::{
    BridgeRequest, ClosureDisposition, Ledger, LedgerError, RequestState, RobinhoodTxKind,
    RobinhoodTxState,
};

use super::calls::{
    BridgeReader, ContractReadError, Obligation, OBLIGATION_STATUS_ABANDONED,
    OBLIGATION_STATUS_PENDING, OBLIGATION_STATUS_REFUNDED, OBLIGATION_STATUS_SETTLED,
};
use super::rpc::{EvmBlockTag, EvmCallRpc};

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Read(#[from] ContractReadError),
}

/// The ledger's view of one obligation, if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSide {
    pub request_id: i64,
    pub direction: &'static str,
    pub state: RequestState,
    /// The `Settlement` row's state, if a settlement was ever begun.
    pub settlement: Option<RobinhoodTxState>,
    /// The `Refund` row's state, if a refund was ever begun.
    pub refund: Option<RobinhoodTxState>,
    /// The recorded closure, when `state` is `Closed` (schema v32).
    pub closure: Option<ClosureDisposition>,
}

/// What one obligation's two records say about each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// The two records agree.
    Consistent,
    /// The chain is terminal and the ledger's own operation that made it
    /// so is still moving through its receipt states — the ledger will
    /// catch up on its own. Reported, not a mismatch.
    InFlight,
    /// The obligation reached a terminal status on chain, and the ledger
    /// still holds the request OPEN with no operation of its own that
    /// explains it. Someone or something else closed this deposit.
    ChainTerminalLedgerOpen,
    /// The ledger claims a terminal outcome the chain does not show: the
    /// obligation is still `Pending`, so the contract's refund path is
    /// still open for a deposit the ledger believes is finished.
    LedgerTerminalChainPending,
    /// Both are terminal, and they disagree about HOW (the ledger says
    /// refunded, the chain says settled, or the reverse).
    TerminalDisagreement,
    /// The contract holds an obligation the ledger has no row for.
    Unobserved,
    /// A ledger row for THIS contract names an obligation index the
    /// contract does not have (index >= obligationCount).
    LedgerRowWithoutObligation,
    /// A ledger row whose `source_contract` is a different deployment.
    /// Not a disagreement about this contract — a pointer to another
    /// audit that is owed.
    ForeignRow,
    /// The ledger closed the request as `retained_per_terms` but the
    /// contract still holds the obligation `Pending`: the chain-side
    /// close-out (`executeAbandonment`) is still owed, and until it
    /// lands the contract's refund path is open for a principal the
    /// operator decided to retain.
    ClosedChainCloseoutOwed,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Consistent => "consistent",
            Verdict::InFlight => "in_flight",
            Verdict::ChainTerminalLedgerOpen => "chain_terminal_ledger_open",
            Verdict::LedgerTerminalChainPending => "ledger_terminal_chain_pending",
            Verdict::TerminalDisagreement => "terminal_disagreement",
            Verdict::Unobserved => "unobserved",
            Verdict::LedgerRowWithoutObligation => "ledger_row_without_obligation",
            Verdict::ForeignRow => "foreign_row",
            Verdict::ClosedChainCloseoutOwed => "closed_chain_closeout_owed",
        }
    }

    /// Whether this verdict is a disagreement an operator must look at.
    /// `Consistent` and `InFlight` are not; `ForeignRow` is not a
    /// disagreement about the audited contract, but it is still
    /// surfaced as needing a second audit.
    pub fn is_mismatch(self) -> bool {
        !matches!(
            self,
            Verdict::Consistent | Verdict::InFlight | Verdict::ForeignRow
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub obligation_index: u64,
    /// `None` only for [`Verdict::LedgerRowWithoutObligation`] and
    /// [`Verdict::ForeignRow`].
    pub chain: Option<Obligation>,
    pub ledger: Option<LedgerSide>,
    pub verdict: Verdict,
    /// For a foreign row: the contract the row actually names.
    pub foreign_contract: Option<EvmAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObligationAuditReport {
    pub contract: EvmAddress,
    pub block: EvmBlockTag,
    pub obligation_count: u64,
    /// Every obligation on the contract plus every ledger row that
    /// claims this contract, index order; foreign rows last.
    pub findings: Vec<Finding>,
}

impl ObligationAuditReport {
    pub fn mismatches(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.verdict.is_mismatch())
    }

    pub fn mismatch_count(&self) -> usize {
        self.mismatches().count()
    }

    pub fn foreign_rows(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.verdict == Verdict::ForeignRow)
    }

    pub fn is_clean(&self) -> bool {
        self.mismatch_count() == 0
    }

    /// Per-verdict counts, for a one-line summary or a gauge.
    pub fn tally(&self) -> BTreeMap<&'static str, usize> {
        let mut t = BTreeMap::new();
        for f in &self.findings {
            *t.entry(f.verdict.as_str()).or_insert(0) += 1;
        }
        t
    }
}

/// Audits `contract` against `ledger` at `block`.
///
/// Reads `obligationCount()` once and `obligation(i)` for every index,
/// all at the same block tag, so the chain side is one consistent
/// picture. Ledger rows are read after, from the live database.
///
/// Takes `&mut Ledger` only so the future is `Send` (a `&Ledger` held
/// across an await is not); nothing here writes.
pub async fn audit<R: EvmCallRpc>(
    rpc: &R,
    ledger: &mut Ledger,
    contract: EvmAddress,
    block: EvmBlockTag,
) -> Result<ObligationAuditReport, AuditError> {
    let reader = BridgeReader::new(contract);
    let obligation_count = reader.obligation_count(rpc, block).await?;
    let mut chain: BTreeMap<u64, Obligation> = BTreeMap::new();
    for index in 0..obligation_count {
        chain.insert(index, reader.obligation(rpc, index, block).await?);
    }

    let contract_bytes = contract.to_bytes();
    let mut ours: BTreeMap<u64, LedgerSide> = BTreeMap::new();
    let mut foreign: Vec<Finding> = Vec::new();
    for request in ledger.robinhood_sourced_requests()? {
        let Some(index) = request.source_obligation_index else {
            continue;
        };
        let recorded = request.source_contract.as_deref().unwrap_or(&[]);
        if recorded != contract_bytes {
            foreign.push(Finding {
                obligation_index: index,
                chain: None,
                ledger: Some(ledger_side(ledger, &request)?),
                verdict: Verdict::ForeignRow,
                foreign_contract: EvmAddress::try_from_slice(recorded).ok(),
            });
            continue;
        }
        // The unique index on (chain, contract, index) makes a second row
        // for the same obligation impossible, so a plain insert suffices.
        ours.insert(index, ledger_side(ledger, &request)?);
    }

    let mut findings = Vec::with_capacity(chain.len() + foreign.len());
    for (index, obligation) in &chain {
        let ledger_side = ours.remove(index);
        let verdict = match &ledger_side {
            None => Verdict::Unobserved,
            Some(l) => classify(obligation, l),
        };
        findings.push(Finding {
            obligation_index: *index,
            chain: Some(*obligation),
            ledger: ledger_side,
            verdict,
            foreign_contract: None,
        });
    }
    // Rows that claim this contract but name an index it has not reached.
    for (index, l) in ours {
        findings.push(Finding {
            obligation_index: index,
            chain: None,
            ledger: Some(l),
            verdict: Verdict::LedgerRowWithoutObligation,
            foreign_contract: None,
        });
    }
    findings.extend(foreign);

    Ok(ObligationAuditReport {
        contract,
        block,
        obligation_count,
        findings,
    })
}

fn ledger_side(ledger: &Ledger, request: &BridgeRequest) -> Result<LedgerSide, LedgerError> {
    Ok(LedgerSide {
        request_id: request.id,
        direction: request.direction.as_str(),
        state: request.state,
        settlement: ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request.id)?
            .map(|t| t.state),
        refund: ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Refund, request.id)?
            .map(|t| t.state),
        closure: ledger.request_closure(request.id)?.map(|c| c.disposition),
    })
}

/// Whether a `robinhood_transactions` row in this state means the chain
/// may legitimately be ahead of the request's state: the operation was
/// broadcast (or is being confirmed) and the request will follow once
/// the receipt is deep enough.
fn explains_chain_terminal(state: Option<RobinhoodTxState>) -> bool {
    matches!(
        state,
        Some(
            RobinhoodTxState::Broadcast
                | RobinhoodTxState::Included
                | RobinhoodTxState::Finalized
                | RobinhoodTxState::Signed
        )
    )
}

/// The verdict for one obligation with both records present.
pub fn classify(chain: &Obligation, ledger: &LedgerSide) -> Verdict {
    let ledger_settled = ledger.state == RequestState::Settled;
    let ledger_refunded = ledger.state == RequestState::Refunded;
    // A closure (schema v32) is the ledger's explicit record of an
    // outcome this service did not produce itself; each disposition
    // agrees with exactly the chain status it describes.
    if ledger.state == RequestState::Closed {
        return match (ledger.closure, chain.status) {
            (Some(ClosureDisposition::RefundedOutOfBand), OBLIGATION_STATUS_REFUNDED)
            | (Some(ClosureDisposition::RetainedPerTerms), OBLIGATION_STATUS_ABANDONED)
            | (Some(ClosureDisposition::ReconciledToChain), OBLIGATION_STATUS_SETTLED)
            | (Some(ClosureDisposition::ReconciledToChain), OBLIGATION_STATUS_REFUNDED)
            | (Some(ClosureDisposition::ReconciledToChain), OBLIGATION_STATUS_ABANDONED) => {
                Verdict::Consistent
            }
            (Some(ClosureDisposition::RetainedPerTerms), OBLIGATION_STATUS_PENDING) => {
                Verdict::ClosedChainCloseoutOwed
            }
            (_, OBLIGATION_STATUS_PENDING) => Verdict::LedgerTerminalChainPending,
            _ => Verdict::TerminalDisagreement,
        };
    }
    match chain.status {
        OBLIGATION_STATUS_PENDING => {
            if ledger_settled || ledger_refunded {
                Verdict::LedgerTerminalChainPending
            } else {
                Verdict::Consistent
            }
        }
        OBLIGATION_STATUS_SETTLED => {
            if ledger_settled {
                Verdict::Consistent
            } else if ledger_refunded {
                Verdict::TerminalDisagreement
            } else if explains_chain_terminal(ledger.settlement) {
                Verdict::InFlight
            } else {
                Verdict::ChainTerminalLedgerOpen
            }
        }
        OBLIGATION_STATUS_REFUNDED => {
            if ledger_refunded {
                Verdict::Consistent
            } else if ledger_settled {
                Verdict::TerminalDisagreement
            } else if explains_chain_terminal(ledger.refund) {
                Verdict::InFlight
            } else {
                Verdict::ChainTerminalLedgerOpen
            }
        }
        OBLIGATION_STATUS_ABANDONED => {
            // An abandonment the ledger has not recorded as a
            // `retained_per_terms` closure is something an operator did
            // outside this service and must be looked at.
            if ledger_settled || ledger_refunded {
                Verdict::TerminalDisagreement
            } else {
                Verdict::ChainTerminalLedgerOpen
            }
        }
        // `None` (an unwritten slot below obligationCount — impossible on
        // a well-formed contract) or an unknown wire value: never treated
        // as agreement.
        _ => Verdict::ChainTerminalLedgerOpen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::EvmU256;
    use crate::robinhood::testkit::{MockNode, BRIDGE, DEPOSITOR};

    fn ledger() -> Ledger {
        Ledger::open_in_memory().expect("an in-memory ledger")
    }

    fn row(ledger: &Ledger, contract: &EvmAddress, index: u64, state: &str) -> i64 {
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                     net_amount_atomic, net_destination_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index,
                     source_confirmations, source_finalized_at, manual_review_note)
                 VALUES ('RhnToGlc', ?1, 100, 300, 3, 97, 97, X'6162', 100,
                         'robinhood', ?2, ?3, 1, 100, 'parked')",
                rusqlite::params![state, &contract.to_bytes()[..], index as i64],
            )
            .unwrap();
        ledger.conn_for_tests().last_insert_rowid()
    }

    fn obligation(node: &MockNode, index: u64, status: u8) {
        node.with(|s| {
            s.contract.obligation_count = s.contract.obligation_count.max(index + 1);
            s.contract.obligations.insert(
                index,
                Obligation {
                    depositor: DEPOSITOR,
                    status,
                    route: 0x02,
                    amount: EvmU256::from_u128(1),
                },
            );
        });
    }

    fn side(
        state: RequestState,
        settlement: Option<RobinhoodTxState>,
        refund: Option<RobinhoodTxState>,
    ) -> LedgerSide {
        LedgerSide {
            request_id: 1,
            direction: "RhnToGlc",
            state,
            settlement,
            refund,
            closure: None,
        }
    }

    fn closed(disposition: ClosureDisposition) -> LedgerSide {
        LedgerSide {
            closure: Some(disposition),
            ..side(RequestState::Closed, None, None)
        }
    }

    fn chain(status: u8) -> Obligation {
        Obligation {
            depositor: DEPOSITOR,
            status,
            route: 0x02,
            amount: EvmU256::from_u128(1),
        }
    }

    #[test]
    fn classification_table() {
        use RequestState as S;
        use RobinhoodTxState as T;
        let cases: Vec<(u8, LedgerSide, Verdict)> = vec![
            (
                OBLIGATION_STATUS_PENDING,
                side(S::ManualReview, None, None),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                side(S::DestinationConfirmed, Some(T::Authorized), None),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                side(S::RefundPending, None, Some(T::Authorized)),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                side(S::Settled, Some(T::Finalized), None),
                Verdict::LedgerTerminalChainPending,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                side(S::Refunded, None, Some(T::Finalized)),
                Verdict::LedgerTerminalChainPending,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                side(S::Settled, Some(T::Finalized), None),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                side(S::DestinationConfirmed, Some(T::Broadcast), None),
                Verdict::InFlight,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                side(S::DestinationConfirmed, Some(T::Included), None),
                Verdict::InFlight,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                side(S::DestinationConfirmed, Some(T::Authorizing), None),
                Verdict::ChainTerminalLedgerOpen,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                side(S::ManualReview, None, None),
                Verdict::ChainTerminalLedgerOpen,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                side(S::Refunded, None, Some(T::Finalized)),
                Verdict::TerminalDisagreement,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                side(S::Refunded, None, Some(T::Finalized)),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                side(S::RefundPending, None, Some(T::Broadcast)),
                Verdict::InFlight,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                side(S::ManualReview, None, None),
                Verdict::ChainTerminalLedgerOpen,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                side(S::Settled, Some(T::Finalized), None),
                Verdict::TerminalDisagreement,
            ),
            (
                OBLIGATION_STATUS_ABANDONED,
                side(S::ManualReview, None, None),
                Verdict::ChainTerminalLedgerOpen,
            ),
            (
                OBLIGATION_STATUS_ABANDONED,
                side(S::Settled, Some(T::Finalized), None),
                Verdict::TerminalDisagreement,
            ),
            (
                0,
                side(S::ManualReview, None, None),
                Verdict::ChainTerminalLedgerOpen,
            ),
            (
                9,
                side(S::ManualReview, None, None),
                Verdict::ChainTerminalLedgerOpen,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                closed(ClosureDisposition::RefundedOutOfBand),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                closed(ClosureDisposition::RefundedOutOfBand),
                Verdict::LedgerTerminalChainPending,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                closed(ClosureDisposition::RefundedOutOfBand),
                Verdict::TerminalDisagreement,
            ),
            (
                OBLIGATION_STATUS_ABANDONED,
                closed(ClosureDisposition::RetainedPerTerms),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                closed(ClosureDisposition::RetainedPerTerms),
                Verdict::ClosedChainCloseoutOwed,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                closed(ClosureDisposition::RetainedPerTerms),
                Verdict::TerminalDisagreement,
            ),
            (
                OBLIGATION_STATUS_SETTLED,
                closed(ClosureDisposition::ReconciledToChain),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_REFUNDED,
                closed(ClosureDisposition::ReconciledToChain),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_ABANDONED,
                closed(ClosureDisposition::ReconciledToChain),
                Verdict::Consistent,
            ),
            (
                OBLIGATION_STATUS_PENDING,
                closed(ClosureDisposition::ReconciledToChain),
                Verdict::LedgerTerminalChainPending,
            ),
        ];
        for (status, l, want) in cases {
            assert_eq!(
                classify(&chain(status), &l),
                want,
                "status {status} / {:?}",
                l.state
            );
        }
    }

    #[tokio::test]
    async fn the_v1_v2_shape_is_named_from_both_sides() {
        // The audited contract holds #29 Pending; the ledger holds a row
        // for #29 on THIS contract (consistent), a row for #30 on a
        // predecessor (foreign), and the contract holds #31 nobody
        // observed (unobserved).
        let node = MockNode::new(BRIDGE);
        let ledger = ledger();
        let v1 = EvmAddress::try_from_slice(&[0x17; 20]).unwrap();
        obligation(&node, 29, OBLIGATION_STATUS_PENDING);
        obligation(&node, 30, OBLIGATION_STATUS_SETTLED);
        obligation(&node, 31, OBLIGATION_STATUS_PENDING);
        row(&ledger, &BRIDGE, 29, "ManualReview");
        row(&ledger, &v1, 30, "ManualReview");
        row(&ledger, &BRIDGE, 30, "Settled");
        row(&ledger, &BRIDGE, 40, "ManualReview");

        let mut ledger = ledger;
        let report = audit(&node, &mut ledger, BRIDGE, EvmBlockTag::Latest)
            .await
            .expect("an audit of a healthy node");
        assert_eq!(report.obligation_count, 32);
        let by_index: BTreeMap<(u64, Verdict), &Finding> = report
            .findings
            .iter()
            .map(|f| ((f.obligation_index, f.verdict), f))
            .collect();
        assert!(by_index.contains_key(&(29, Verdict::Consistent)));
        assert!(by_index.contains_key(&(30, Verdict::Consistent)));
        assert!(by_index.contains_key(&(31, Verdict::Unobserved)));
        assert!(by_index.contains_key(&(40, Verdict::LedgerRowWithoutObligation)));
        let foreign = by_index[&(30, Verdict::ForeignRow)];
        assert_eq!(foreign.foreign_contract, Some(v1));
        // #31 plus the 29 slots below #29 that nobody observed, plus the
        // row for #40: every one is a finding, none is silently skipped.
        assert_eq!(report.tally()["unobserved"], 30);
        assert_eq!(report.mismatch_count(), 31, "{:?}", report.tally());
        assert!(!report.is_clean());
        assert!(by_index.contains_key(&(0, Verdict::Unobserved)));
    }

    #[tokio::test]
    async fn a_ledger_that_matches_the_chain_is_clean() {
        let node = MockNode::new(BRIDGE);
        let ledger = ledger();
        obligation(&node, 0, OBLIGATION_STATUS_SETTLED);
        obligation(&node, 1, OBLIGATION_STATUS_PENDING);
        row(&ledger, &BRIDGE, 0, "Settled");
        row(&ledger, &BRIDGE, 1, "ManualReview");
        let mut ledger = ledger;
        let report = audit(&node, &mut ledger, BRIDGE, EvmBlockTag::Latest)
            .await
            .unwrap();
        assert!(report.is_clean(), "{:?}", report.tally());
        assert_eq!(report.tally()["consistent"], 2);
    }

    #[tokio::test]
    async fn a_failed_read_is_an_error_not_a_clean_report() {
        let node = MockNode::new(BRIDGE);
        let ledger = ledger();
        obligation(&node, 0, OBLIGATION_STATUS_PENDING);
        node.fail_calls("endpoint down");
        let mut ledger = ledger;
        let err = audit(&node, &mut ledger, BRIDGE, EvmBlockTag::Latest)
            .await
            .expect_err("no report without a chain read");
        assert!(matches!(err, AuditError::Read(_)), "{err}");
    }
}
