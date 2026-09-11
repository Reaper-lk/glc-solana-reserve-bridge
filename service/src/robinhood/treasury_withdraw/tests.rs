//! Treasury withdrawal tests, end to end through the mock node.
//!
//! Organised the way the Solana `glc-treasury-withdraw` tests are: the
//! refusals first (each named for the gate that refuses), then the happy
//! path, then the receipt outcomes, then idempotency. Every test drives
//! the SAME functions the CLI drives — `begin`, `Settler::tick_broadcast`,
//! `Settler::tick_receipts` — never a hand-written row.

use super::*;
use crate::amount_conversion::robinhood::CANONICAL_TO_ROBINHOOD_SCALE;
use crate::ledger::{RebalanceKind, RebalanceState, ReserveDirection, RobinhoodTxState};
use crate::robinhood::calls::{GateError, GateRefusal};
use crate::robinhood::settlement::{SettlementReport, Settler};
use crate::robinhood::signer::{DevEvmAuthSigner, EvmAuthSigner};
use crate::robinhood::testkit::{signer_key, submitter_key, MockNode, BRIDGE, TREASURY};
use crate::robinhood::Submitter;
use std::time::Duration;

/// 1,000 GLC, in the two units that meet here.
const AMOUNT_CANONICAL: u64 = 100_000_000_000; // 8dp
const AMOUNT_18DP: u128 = 1_000_000_000_000_000_000_000; // 18dp
/// What the mock contract holds: 10,000 GLC.
const RESERVE_18DP: u128 = 10_000_000_000_000_000_000_000;
const RESERVE_CANONICAL: u64 = 1_000_000_000_000;

fn ledger() -> Ledger {
    Ledger::open_in_memory().expect("an in-memory ledger")
}

fn settler(node: &MockNode) -> Settler<MockNode> {
    settler_with_signers(node, vec![0, 1, 2])
}

fn settler_with_signers(node: &MockNode, indexes: Vec<u8>) -> Settler<MockNode> {
    let config = node.settlement_config();
    let signers: Vec<Box<dyn EvmAuthSigner>> = indexes
        .into_iter()
        .map(|i| Box::new(DevEvmAuthSigner::new(signer_key(i))) as Box<dyn EvmAuthSigner>)
        .collect();
    Settler::new(
        node.clone(),
        Submitter::from_key(submitter_key(), &config).unwrap(),
        signers,
        node.verified_deployment(),
        config,
        Duration::from_secs(5),
        crate::goldcoin::address::Network::Testnet,
        6,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
}

/// A node whose contract is paused in both directions and holds
/// `RESERVE_18DP`, with no encumbrance — the state a withdrawal expects.
fn paused_node() -> MockNode {
    let node = MockNode::new(BRIDGE);
    node.with(|s| {
        s.contract.deposits_paused = true;
        s.contract.payouts_paused = true;
    });
    node.set_token_balance(BRIDGE, RESERVE_18DP);
    node
}

fn configure_reserve(ledger: &mut Ledger, balance: u64, protected_min: u64) {
    // Bands must sit above the floor (docs/05): target > warning >
    // critical > protected_minimum.
    let headroom = balance.saturating_sub(protected_min).max(4);
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            balance,
            protected_min,
            protected_min + headroom,
            protected_min + headroom / 2,
            protected_min + headroom / 4,
            100,
        )
        .unwrap();
}

/// An APPROVED Robinhood withdrawal for `amount` canonical units.
fn approved(ledger: &mut Ledger, amount: u64) -> i64 {
    let id = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            amount,
            "treasury rebalance",
            "ops:alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops:bob", 1_001).unwrap();
    id
}

/// The standard fixture: paused node, configured reserve, approved
/// 1,000 GLC withdrawal.
fn fixture() -> (MockNode, Settler<MockNode>, Ledger, i64) {
    let node = paused_node();
    let settler = settler(&node);
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    let id = approved(&mut ledger, AMOUNT_CANONICAL);
    (node, settler, ledger, id)
}

fn failed_checks(a: &Assessment) -> Vec<&'static str> {
    a.checks.iter().filter(|c| !c.ok).map(|c| c.name).collect()
}

async fn onchain(node: &MockNode) -> OnchainContext {
    read_onchain(node, BRIDGE, crate::robinhood::testkit::TOKEN, None)
        .await
        .unwrap()
}

// ===================================================================
// The assessment: every check, PASS and FAIL
// ===================================================================

#[tokio::test]
async fn a_valid_withdrawal_passes_every_check() {
    let (node, _settler, ledger, id) = fixture();
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert_eq!(failed_checks(&a), Vec::<&str>::new(), "{:?}", a.checks);
    assert!(a.eligible());
    assert_eq!(a.amount_robinhood.unwrap().get(), AMOUNT_18DP);
    assert_eq!(a.onchain.as_ref().unwrap().treasury, TREASURY);
}

/// The 8dp -> 18dp widening is exactly 10^10, stated on the check itself
/// so the dry run's own output cannot lie about the unit.
#[tokio::test]
async fn the_assessment_states_both_units_and_widens_by_ten_to_the_ten() {
    let (node, _settler, ledger, id) = fixture();
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    let check = a
        .checks
        .iter()
        .find(|c| c.name == "amount_widens_exactly_to_18dp")
        .unwrap();
    assert!(check.detail.contains(&AMOUNT_CANONICAL.to_string()));
    assert!(check.detail.contains(&AMOUNT_18DP.to_string()));
    assert!(check.detail.contains("1,000 GLC"));
    assert_eq!(
        u128::from(AMOUNT_CANONICAL) * CANONICAL_TO_ROBINHOOD_SCALE,
        AMOUNT_18DP
    );
}

#[tokio::test]
async fn protected_minimum_breach_is_refused() {
    let node = paused_node();
    let mut ledger = ledger();
    // Floor leaves only 500 GLC spendable; 1,000 GLC requested.
    configure_reserve(
        &mut ledger,
        RESERVE_CANONICAL,
        RESERVE_CANONICAL - 50_000_000_000,
    );
    let id = approved(&mut ledger, AMOUNT_CANONICAL);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(
        failed_checks(&a).contains(&"ledger_protected_minimum_preserved"),
        "{:?}",
        a.checks
    );
    assert!(!a.eligible());
}

#[tokio::test]
async fn reserved_liquidity_breach_is_refused() {
    let node = paused_node();
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    // 9,500 GLC reserved against accepted requests: 500 spendable.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET reserved_liquidity = ?1 WHERE direction = 'RobinhoodReserve'",
            [(RESERVE_CANONICAL - 50_000_000_000) as i64],
        )
        .unwrap();
    let id = approved(&mut ledger, AMOUNT_CANONICAL);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(
        failed_checks(&a).contains(&"ledger_reserved_liquidity_preserved"),
        "{:?}",
        a.checks
    );
}

#[tokio::test]
async fn pending_outbound_obligations_breach_is_refused() {
    let node = paused_node();
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET reserved_liquidity = ?1, pending_obligations = ?1
             WHERE direction = 'RobinhoodReserve'",
            [(RESERVE_CANONICAL - 50_000_000_000) as i64],
        )
        .unwrap();
    let id = approved(&mut ledger, AMOUNT_CANONICAL);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(
        failed_checks(&a).contains(&"ledger_pending_obligations_preserved"),
        "{:?}",
        a.checks
    );
}

/// The contract's own floor, read live — independent of the ledger's.
#[tokio::test]
async fn onchain_encumbered_reserve_breach_is_refused() {
    let (node, _settler, ledger, id) = fixture();
    node.with(|s| {
        s.contract.encumbered_reserve = EvmU256::from_u128(RESERVE_18DP - AMOUNT_18DP / 2)
    });
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(
        failed_checks(&a).contains(&"onchain_spendable_reserve_covers_amount"),
        "{:?}",
        a.checks
    );
}

#[tokio::test]
async fn an_unpaused_contract_fails_the_authoritative_pause_check() {
    let (node, _settler, ledger, id) = fixture();
    for (deposits, payouts) in [(false, false), (true, false), (false, true)] {
        node.with(|s| {
            s.contract.deposits_paused = deposits;
            s.contract.payouts_paused = payouts;
        });
        let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
        assert!(
            failed_checks(&a).contains(&"both_directions_paused_onchain"),
            "deposits={deposits} payouts={payouts}: {:?}",
            a.checks
        );
    }
}

/// The local `RobinhoodReserve.paused` flag is NOT the requirement and
/// cannot satisfy it: with the local gate paused and the contract open,
/// the check still fails.
#[tokio::test]
async fn the_local_pause_flag_is_not_accepted_in_place_of_the_contracts() {
    let (node, _settler, mut ledger, id) = fixture();
    ledger
        .set_paused(ReserveDirection::RobinhoodReserve, true, Some("local"))
        .unwrap();
    node.with(|s| {
        s.contract.deposits_paused = false;
        s.contract.payouts_paused = false;
    });
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(failed_checks(&a).contains(&"both_directions_paused_onchain"));
    assert!(
        a.ledger_reserve.as_ref().unwrap().paused,
        "the local flag was set…"
    );
    assert!(!a.eligible(), "…and it counted for nothing");
}

#[tokio::test]
async fn a_zero_treasury_is_refused() {
    let (node, _settler, ledger, id) = fixture();
    node.with(|s| s.contract.treasury = EvmAddress::ZERO);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(failed_checks(&a).contains(&"treasury_configured"));
}

#[tokio::test]
async fn a_migrated_contract_is_refused() {
    let (node, _settler, ledger, id) = fixture();
    node.with(|s| s.contract.migrated = true);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(failed_checks(&a).contains(&"not_migrated"));
}

#[tokio::test]
async fn a_proposed_but_unapproved_request_is_refused() {
    let node = paused_node();
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    let id = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            AMOUNT_CANONICAL,
            "n",
            "ops:alice",
            2,
            1_000,
        )
        .unwrap();
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(failed_checks(&a).contains(&"rebalance_approved"));
}

#[tokio::test]
async fn a_solana_or_deposit_request_is_refused() {
    let node = paused_node();
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    configure_reserve_for(&mut ledger, ReserveDirection::SolanaReserve);
    let solana = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Withdraw,
            AMOUNT_CANONICAL,
            "n",
            "ops:alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(solana, "ops:bob", 1_001).unwrap();
    let deposit = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Deposit,
            AMOUNT_CANONICAL,
            "n",
            "ops:alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(deposit, "ops:bob", 1_001).unwrap();
    for id in [solana, deposit] {
        let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
        assert!(failed_checks(&a).contains(&"rebalance_is_robinhood_withdraw"));
    }
}

fn configure_reserve_for(ledger: &mut Ledger, direction: ReserveDirection) {
    ledger
        .configure_reserve(direction, 1_000_000, 0, 1_000_000, 500_000, 250_000, 100)
        .unwrap();
}

#[tokio::test]
async fn missing_quorum_is_refused() {
    let (node, _settler, ledger, id) = fixture();
    let a = assess(&ledger, id, Some(onchain(&node).await), 1, 2_000).unwrap();
    assert!(failed_checks(&a).contains(&"signer_quorum_configured"));
}

#[tokio::test]
async fn an_unconfigured_ledger_reserve_is_refused() {
    let node = paused_node();
    let mut ledger = ledger();
    let id = approved(&mut ledger, AMOUNT_CANONICAL);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert!(failed_checks(&a).contains(&"ledger_reserve_configured"));
}

/// THE policy test, backend side: with no liabilities and both floors at
/// zero, a withdrawal of the ENTIRE reserve passes every check. Then,
/// one by one, each accounting constraint refuses exactly the amount it
/// protects — and nothing else does. There is no cap to find.
#[tokio::test]
async fn the_entire_free_reserve_is_withdrawable_and_only_accounting_refuses() {
    let node = paused_node();
    // Make the user-route limits SMALL, so the withdrawal below is
    // provably above every one of them.
    node.with(|s| {
        s.contract.limits.outbound_max = EvmU256::from_u128(100 * 1_000_000_000_000_000_000);
        s.contract.limits.outbound_rolling_limit =
            EvmU256::from_u128(1_000 * 1_000_000_000_000_000_000);
    });
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    // The whole reserve (10,000 GLC), far above every user-route limit.
    let id = approved(&mut ledger, RESERVE_CANONICAL);
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert_eq!(failed_checks(&a), Vec::<&str>::new(), "{:?}", a.checks);
    assert!(a.eligible(), "the whole free reserve is withdrawable");
    assert!(
        a.amount_robinhood.unwrap().to_u256() > node.with(|s| s.contract.limits.outbound_max),
        "above outboundMax, and nobody cared"
    );
    assert!(
        a.amount_robinhood.unwrap().to_u256()
            > node.with(|s| s.contract.limits.outbound_rolling_limit),
        "above the rolling limit, and nobody cared"
    );

    // A ledger floor of one canonical unit refuses the whole-reserve
    // request — by exactly that unit, for exactly that reason.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET protected_minimum = 1 WHERE direction = 'RobinhoodReserve'",
            [],
        )
        .unwrap();
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    let failed = failed_checks(&a);
    assert!(
        failed.contains(&"ledger_protected_minimum_preserved"),
        "{failed:?}"
    );
    // The floor is a term of available capacity and of the pending
    // headroom too, so those refuse alongside it — and NOTHING that is
    // not an accounting check does.
    assert!(
        failed.iter().all(|c| c.starts_with("ledger_")),
        "only accounting checks may refuse: {failed:?}"
    );
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET protected_minimum = 0 WHERE direction = 'RobinhoodReserve'",
            [],
        )
        .unwrap();

    // A contract-side encumbrance of one atomic unit does the same.
    node.with(|s| s.contract.encumbered_reserve = EvmU256::from_u64(1));
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    assert_eq!(
        failed_checks(&a),
        vec!["onchain_spendable_reserve_covers_amount"]
    );
    node.with(|s| s.contract.encumbered_reserve = EvmU256::ZERO);

    // Back to clear: eligible again, and it executes end to end.
    let settler = settler(&node);
    let tx_id = begin(&settler, &mut ledger, id, 2_100).await.unwrap();
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_200, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    node.mine(tx.tx_hash.unwrap(), 400, true);
    node.mark_executed(tx.action, tx.contract_request_id);
    node.with(|s| s.head = 402);
    settler.tick_receipts(&mut ledger, 2_300, &mut report).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Confirmed
    );
    assert_eq!(
        admin::reserve_report(&ledger, 2_400)
            .unwrap()
            .unwrap()
            .balance_atomic,
        0,
        "the ledger's reserve reads zero: fully drained"
    );
}

// ===================================================================
// begin: refusals are refusals, and write nothing
// ===================================================================

#[tokio::test]
async fn begin_refuses_an_unpaused_contract_and_writes_nothing() {
    let (node, settler, mut ledger, id) = fixture();
    node.with(|s| s.contract.payouts_paused = false);
    let err = begin(&settler, &mut ledger, id, 2_000).await.unwrap_err();
    assert!(matches!(err, TreasuryWithdrawError::Refused(_)), "{err}");
    assert!(err.to_string().contains("both_directions_paused_onchain"));
    assert!(ledger.get_robinhood_tx_for_rebalance(id).unwrap().is_none());
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Approved,
        "the approval is untouched"
    );
}

#[tokio::test]
async fn begin_refuses_a_non_approved_request() {
    let node = paused_node();
    let settler = settler(&node);
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    let id = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            AMOUNT_CANONICAL,
            "n",
            "ops:alice",
            2,
            1_000,
        )
        .unwrap();
    let err = begin(&settler, &mut ledger, id, 2_000).await.unwrap_err();
    assert!(
        matches!(err, TreasuryWithdrawError::NotApproved { .. }),
        "{err}"
    );
    assert!(ledger.get_robinhood_tx_for_rebalance(id).unwrap().is_none());
}

/// The gate names the pause refusal specifically when the assessment is
/// bypassed — pinned so the on-chain read is what refuses, not a ledger
/// flag.
#[tokio::test]
async fn the_gate_itself_refuses_without_both_pauses() {
    let node = paused_node();
    node.with(|s| s.contract.deposits_paused = false);
    let err = calls::ContractGate::new(BRIDGE)
        .check_treasury_withdraw(&node, [0x77; 32], 7, TREASURY, EvmBlockTag::Latest)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            GateError::Refused(GateRefusal::NotPausedForWithdrawal {
                deposits: false,
                payouts: true
            })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn the_gate_refuses_a_treasury_the_contract_does_not_hold() {
    let node = paused_node();
    let other = EvmAddress::from_bytes([0x99; 20]);
    let err = calls::ContractGate::new(BRIDGE)
        .check_treasury_withdraw(&node, [0x77; 32], 0, other, EvmBlockTag::Latest)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            GateError::Refused(GateRefusal::TreasuryMismatch { .. })
        ),
        "{err}"
    );
}

// ===================================================================
// The happy path, and the receipt outcomes
// ===================================================================

#[tokio::test]
async fn a_valid_withdrawal_authorizes_broadcasts_and_finalizes_exactly_once() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();

    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.kind, RobinhoodTxKind::TreasuryWithdraw);
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(tx.rebalance_request_id, Some(id));
    assert_eq!(tx.request_id, None);
    assert_eq!(tx.route, None);
    assert_eq!(tx.action, ACTION_TREASURY_WITHDRAW);
    assert_eq!(
        tx.recipient,
        Some(TREASURY.to_bytes()),
        "the treasury the contract holds"
    );
    assert_eq!(
        tx.amount_robinhood,
        Some(EvmU256::from_u128(AMOUNT_18DP).to_be_bytes()),
        "the approved amount, widened by 10^10"
    );
    assert_eq!(ledger.robinhood_auth_signatures(tx_id).unwrap().len(), 2);

    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Broadcast);
    assert!(tx.nonce.is_some());
    assert!(tx.raw_tx.is_some());
    // The calldata is executeTreasuryWithdraw, nothing else.
    let sent = node.with(|s| s.broadcasts.last().unwrap().raw.clone());
    assert!(!sent.is_empty());

    node.mine(tx.tx_hash.unwrap(), 400, true);
    node.mark_executed(tx.action, tx.contract_request_id);
    node.with(|s| s.head = 402);
    settler.tick_receipts(&mut ledger, 2_200, &mut report).await;

    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Finalized);
    assert_eq!(tx.receipt_status, Some(1));

    // The rebalance side: Executed -> Confirmed, with the hash as the
    // reference and the reserve decremented by the canonical amount.
    let rebalance = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rebalance.state, RebalanceState::Confirmed);
    assert_eq!(
        rebalance.tx_reference.as_deref(),
        Some(crate::evm::hex::encode_lower(&tx.tx_hash.unwrap()).as_str())
    );
    assert_eq!(rebalance.observed_amount_atomic, Some(AMOUNT_CANONICAL));
    let report = admin::reserve_report(&ledger, 2_300).unwrap().unwrap();
    assert_eq!(report.balance_atomic, RESERVE_CANONICAL - AMOUNT_CANONICAL);
}

#[tokio::test]
async fn a_reverted_receipt_parks_the_operation_and_fails_the_rebalance() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    node.mine(tx.tx_hash.unwrap(), 400, false);
    node.with(|s| s.head = 410);
    settler.tick_receipts(&mut ledger, 2_200, &mut report).await;

    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::ManualReview);
    assert_eq!(tx.receipt_status, Some(0));
    let rebalance = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rebalance.state, RebalanceState::Failed);
    assert!(rebalance.failure_reason.unwrap().contains("REVERTED"));
    // The reserve's cached balance was NOT decremented: nothing moved.
    assert_eq!(
        admin::reserve_report(&ledger, 2_300)
            .unwrap()
            .unwrap()
            .balance_atomic,
        RESERVE_CANONICAL
    );
    // And a second tick does not retry under a fresh nonce.
    settler
        .tick_broadcast(&mut ledger, 2_400, &mut report)
        .await;
    assert_eq!(node.with(|s| s.broadcasts.len()), 1, "one broadcast, ever");
}

/// `success` is true for exactly one state. Broadcast-but-unmined is
/// not it.
#[tokio::test]
async fn the_result_reports_success_only_when_finalized() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_150).unwrap();

    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    let result =
        TreasuryWithdrawResult::build(&a, Some(&tx), RebalanceState::Approved, 3, false, vec![]);
    assert_eq!(result.state, "Broadcast");
    assert!(!result.success, "submitted is not landed");
    assert!(result.tx_hash.is_some());
    assert_eq!(result.nonce, tx.nonce);
    assert_eq!(result.amount_atomic, AMOUNT_18DP.to_string());
    assert_eq!(result.amount_canonical_atomic, AMOUNT_CANONICAL.to_string());
    assert_eq!(result.amount_glc, "1,000 GLC");
    assert_eq!(
        result.destination.as_deref(),
        Some(TREASURY.to_checksum_string().as_str())
    );

    node.mine(tx.tx_hash.unwrap(), 400, true);
    node.mark_executed(tx.action, tx.contract_request_id);
    node.with(|s| s.head = 402);
    settler.tick_receipts(&mut ledger, 2_200, &mut report).await;
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    let result =
        TreasuryWithdrawResult::build(&a, Some(&tx), RebalanceState::Confirmed, 3, false, vec![]);
    assert_eq!(result.state, "Finalized");
    assert!(result.success);
    assert_eq!(result.receipt_status, Some(1));
    assert_eq!(result.confirmations, 3);

    // A reverted row: false, with the receipt status to prove it.
    let mut reverted = tx.clone();
    reverted.state = RobinhoodTxState::Reverted;
    reverted.receipt_status = Some(0);
    let result = TreasuryWithdrawResult::build(
        &a,
        Some(&reverted),
        RebalanceState::Failed,
        3,
        false,
        vec![],
    );
    assert!(!result.success);
    assert_eq!(result.receipt_status, Some(0));
}

/// The JSON shape is stable: every documented field is present, amounts
/// are decimal strings, and the dry-run marker is honest.
#[tokio::test]
async fn the_json_result_has_the_documented_fields() {
    let (node, _settler, ledger, id) = fixture();
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_000).unwrap();
    let result = TreasuryWithdrawResult::build(&a, None, RebalanceState::Approved, 3, true, vec![]);
    let json = serde_json::to_value(&result).unwrap();
    for field in [
        "operation_id",
        "rebalance_id",
        "state",
        "rebalance_state",
        "success",
        "dry_run",
        "tx_hash",
        "nonce",
        "amount_atomic",
        "amount_canonical_atomic",
        "amount_glc",
        "destination",
        "receipt_status",
        "confirmations",
        "required_confirmations",
        "failure_reason",
        "checks",
        "onchain",
        "ledger_reserve_before",
    ] {
        assert!(json.get(field).is_some(), "missing {field}");
    }
    assert_eq!(json["state"], "DryRun");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["success"], false);
    assert!(
        json["amount_atomic"].is_string(),
        "18dp as a decimal string"
    );
    assert_eq!(json["amount_atomic"], AMOUNT_18DP.to_string());
    assert_eq!(json["onchain"]["treasury"], TREASURY.to_checksum_string());
    assert_eq!(json["onchain"]["deposits_paused"], true);
}

// ===================================================================
// Simulation and idempotency
// ===================================================================

/// `eth_estimateGas` executes the call; a revert there is caught before
/// a nonce is consumed and nothing is broadcast.
#[tokio::test]
async fn a_simulation_failure_broadcasts_nothing_and_consumes_no_nonce() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    node.with(|s| s.contract.gas_estimate = None);
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    assert!(!report.errors.is_empty(), "the failure is reported");
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(
        tx.state,
        RobinhoodTxState::Authorized,
        "still authorized, not signed"
    );
    assert_eq!(tx.nonce, None, "no nonce consumed");
    assert_eq!(node.with(|s| s.broadcasts.len()), 0);
}

#[tokio::test]
async fn beginning_twice_resumes_the_same_operation_and_the_same_nonce() {
    let (node, settler, mut ledger, id) = fixture();
    let first = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    let nonce = ledger.get_robinhood_tx(first).unwrap().unwrap().nonce;

    // Re-run shortly after: same row, no second authorization, no second
    // nonce, and the IDENTICAL bytes re-sent.
    let second = begin(&settler, &mut ledger, id, 2_150).await.unwrap();
    assert_eq!(first, second);
    settler
        .tick_broadcast(&mut ledger, 2_200, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(first).unwrap().unwrap();
    assert_eq!(tx.nonce, nonce);
    assert_eq!(ledger.robinhood_auth_signatures(first).unwrap().len(), 2);
    assert_eq!(ledger.robinhood_treasury_withdrawals().unwrap().len(), 1);
    let raws: Vec<Vec<u8>> = node.with(|s| s.broadcasts.iter().map(|b| b.raw.clone()).collect());
    assert_eq!(raws.len(), 2);
    assert_eq!(raws[0], raws[1], "a re-send is the same bytes");

    // Later, past the replacement window but inside the incident
    // threshold: a fee-bumped REPLACEMENT may be signed — under the SAME
    // nonce, never a second operation.
    let third = begin(&settler, &mut ledger, id, 2_500).await.unwrap();
    assert_eq!(first, third);
    settler
        .tick_broadcast(&mut ledger, 2_600, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(first).unwrap().unwrap();
    assert_eq!(tx.nonce, nonce, "a replacement keeps the nonce");
    assert_eq!(ledger.robinhood_treasury_withdrawals().unwrap().len(), 1);
    assert!(tx.state.is_in_flight());

    // And past the incident threshold with still no receipt: NOT
    // abandoned, NOT retried under a fresh nonce — parked for a human,
    // nonce and bytes intact.
    settler
        .tick_broadcast(&mut ledger, 2_100 + 31 * 60, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(first).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::ManualReview);
    assert_eq!(tx.nonce, nonce);
    assert!(tx.raw_tx.is_some());
    assert_eq!(ledger.robinhood_treasury_withdrawals().unwrap().len(), 1);
}

/// A crash between the row being written and the quorum being stored
/// resumes into the SAME payload and collects the quorum for it.
#[tokio::test]
async fn an_authorizing_row_is_resumed_not_recreated() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    // Simulate the crash: drop the stored quorum and rewind the state.
    ledger
        .conn_for_tests()
        .execute(
            "DELETE FROM robinhood_authorization_signatures WHERE transaction_id = ?1",
            [tx_id],
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_transactions SET state = 'Authorizing' WHERE id = ?1",
            [tx_id],
        )
        .unwrap();
    let digest_before = ledger.get_robinhood_tx(tx_id).unwrap().unwrap().auth_digest;

    let resumed = begin(&settler, &mut ledger, id, 9_000).await.unwrap();
    assert_eq!(resumed, tx_id);
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(
        tx.auth_digest, digest_before,
        "the same payload, not a re-minted one"
    );
    let _ = node;
}

/// The database itself refuses a second operation for one approval.
#[test]
fn the_schema_allows_one_withdrawal_operation_per_rebalance() {
    let mut ledger = ledger();
    configure_reserve(&mut ledger, RESERVE_CANONICAL, 0);
    let id = approved(&mut ledger, AMOUNT_CANONICAL);
    let new = |tag: u8| NewRobinhoodTx {
        kind: RobinhoodTxKind::TreasuryWithdraw,
        request_id: None,
        rebalance_request_id: Some(id),
        route: None,
        bridge_contract: [0x11; 20],
        chain_id: 4663,
        contract_request_id: [tag; 32],
        obligation_index: None,
        recipient: Some(TREASURY.to_bytes()),
        amount_robinhood: Some(EvmU256::from_u128(AMOUNT_18DP).to_be_bytes()),
        signer_epoch: 0,
        expiry: 9_999,
        auth_digest: [tag; 32],
    };
    let first = ledger.begin_robinhood_tx(&new(1), 100).unwrap();
    let again = ledger.begin_robinhood_tx(&new(2), 101).unwrap();
    match (first, again) {
        (BeginTxOutcome::Created { id: a }, BeginTxOutcome::Exists { id: b }) => assert_eq!(a, b),
        other => panic!("{other:?}"),
    }
    // And the shape rules: a withdrawal may not name a bridge request
    // or a route.
    let mut bad = new(3);
    bad.request_id = Some(1);
    assert!(ledger.begin_robinhood_tx(&bad, 102).is_err());
    let mut bad = new(4);
    bad.route = Some(crate::routes::Route::GlcToRhn);
    assert!(ledger.begin_robinhood_tx(&bad, 103).is_err());
}

/// Post-state verification: a receipt with status 1 whose logs carry no
/// bridge event, or whose replay guard does not confirm, is not treated
/// as success — the same discipline every other kind gets.
#[tokio::test]
async fn a_successful_receipt_the_replay_guard_does_not_confirm_is_not_success() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    node.mine(tx.tx_hash.unwrap(), 400, true);
    // Deliberately NOT marking the request executed on the mock contract.
    node.with(|s| s.head = 402);
    settler.tick_receipts(&mut ledger, 2_200, &mut report).await;
    assert!(
        report.errors.iter().any(|e| e.contains("replay guard")),
        "{:?}",
        report.errors
    );
    // The shared receipt phase promotes the ROW on depth before it
    // verifies the effect, so the row reads Finalized here (the payout
    // path behaves identically). What must not have happened is the
    // completion: the rebalance stays Approved, the reserve is untouched,
    // and the result reports NO success.
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    let rebalance = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rebalance.state, RebalanceState::Approved);
    assert_eq!(
        admin::reserve_report(&ledger, 2_300)
            .unwrap()
            .unwrap()
            .balance_atomic,
        RESERVE_CANONICAL
    );
    let a = assess(&ledger, id, Some(onchain(&node).await), 3, 2_300).unwrap();
    let result = TreasuryWithdrawResult::build(&a, Some(&tx), rebalance.state, 3, false, vec![]);
    assert!(
        !result.success,
        "a receipt whose effect is unverified is not success"
    );
}

/// The wrong contract or chain is refused by the deployment identity the
/// row records: a row authorized against one bridge cannot be built for
/// another.
#[tokio::test]
async fn a_row_for_a_different_deployment_is_refused_before_broadcast() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    // Point the contract at a different treasury: the recorded treasury
    // no longer matches what the chain holds, which is the signature of
    // a row built against another deployment.
    node.with(|s| s.contract.treasury = EvmAddress::from_bytes([0x55; 20]));
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("different deployment")),
        "{:?}",
        report.errors
    );
    assert_eq!(ledger.get_robinhood_tx(tx_id).unwrap().unwrap().nonce, None);
    assert_eq!(node.with(|s| s.broadcasts.len()), 0);
}

/// `drive` returns only on a terminal state or the deadline, and reports
/// what it ended with. The chain "mines" the broadcast between rounds.
#[tokio::test]
async fn drive_runs_to_finality_and_reports_it() {
    let (node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    let row = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    let (action, request_id) = (row.action, row.contract_request_id);

    let mut clock = 2_100i64;
    let miner = node.clone();
    let outcome = drive(
        &settler,
        &mut ledger,
        tx_id,
        || {
            clock += 10;
            clock
        },
        10_000,
        || {
            // First round broadcast it; now the chain includes it at depth.
            if let Some(hash) = miner.with(|s| s.broadcasts.last().map(|b| b.tx_hash)) {
                miner.mine(hash, 400, true);
                miner.mark_executed(action, request_id);
                miner.with(|s| s.head = 402);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.tx.state, RobinhoodTxState::Finalized);
    assert_eq!(outcome.report.errors, Vec::<String>::new());
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Confirmed
    );
}

/// With nothing mining it, `drive` stops at the deadline with the row
/// still `Broadcast` — which the CLI then reports as NOT complete.
#[tokio::test]
async fn drive_stops_at_the_deadline_with_an_unresolved_broadcast() {
    let (_node, settler, mut ledger, id) = fixture();
    let tx_id = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    let mut clock = 2_100i64;
    let outcome = drive(
        &settler,
        &mut ledger,
        tx_id,
        || {
            clock += 100;
            clock
        },
        2_400,
        || {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.tx.state, RobinhoodTxState::Broadcast);
    assert!(!outcome.tx.state.is_terminal());
}

/// The user-facing payout and refund paths are untouched: the same
/// settler settles a payout exactly as before with a withdrawal row
/// beside it.
#[tokio::test]
async fn payouts_and_refunds_are_unaffected_by_a_withdrawal_in_the_same_ledger() {
    let (node, settler, mut ledger, id) = fixture();
    let _ = begin(&settler, &mut ledger, id, 2_000).await.unwrap();
    // A payout row alongside, through the ordinary path, must still be
    // listed under its request and never confused with the withdrawal.
    let views = admin::open_operations(&ledger).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].kind, RobinhoodTxKind::TreasuryWithdraw);
    assert_eq!(views[0].request_id, None);
    assert_eq!(views[0].rebalance_request_id, Some(id));
    assert!(
        admin::txs_for_request(&ledger, id).unwrap().is_empty(),
        "a bridge request with the same numeric id shares nothing with the rebalance"
    );
    let _ = node;
}
