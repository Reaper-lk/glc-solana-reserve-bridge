//! Out-of-band recovery against the mock node and an in-memory ledger,
//! driven by the EXACT log the V1 custody contract emitted for the
//! incident deposit (obligation #30, tx `0x85dfdd39…`, block 60706447).

use super::*;

use crate::evm::EvmU256;
use crate::ledger::{Direction, RequestState, ReserveDirection, RobinhoodFinality};
use crate::robinhood::rpc::{EvmRawLog, EvmReceipt};
use crate::robinhood::testkit::{block_hash, deposit_log, DepositParams, MockNode, BRIDGE};

const NOW: i64 = 1_790_000_000;
const DEPTH: u64 = 12;
const SOLANA_DECIMALS: u8 = 6;

/// The V1 contract the incident deposit was made to; the config under
/// test names it, exactly as `config-v1.toml` does.
const V1: EvmAddress = EvmAddress::from_bytes([
    0x17, 0x53, 0xdd, 0xa0, 0x25, 0x6a, 0x2c, 0xb1, 0x0b, 0x44, 0x49, 0x7a, 0xce, 0xa9, 0x65, 0x0a,
    0x14, 0x22, 0xf4, 0x40,
]);
const INCIDENT_BLOCK: u64 = 60_706_447;
const INCIDENT_TX: [u8; 32] = [
    0x85, 0xdf, 0xdd, 0x39, 0x42, 0xcf, 0x1e, 0xac, 0x22, 0x7e, 0x78, 0xb5, 0xd4, 0x12, 0x9d, 0xe0,
    0x80, 0xbd, 0xbe, 0x91, 0x2a, 0xac, 0x99, 0xa3, 0x90, 0xa3, 0xd1, 0xfd, 0x1f, 0x21, 0x84, 0xc9,
];
const DEPOSITOR_5376: [u8; 20] = [
    0x53, 0x76, 0xd8, 0xff, 0x23, 0xb3, 0xea, 0xee, 0xef, 0x9f, 0xbe, 0x78, 0x57, 0xeb, 0xe4, 0x48,
    0x61, 0x91, 0xf5, 0x3b,
];
/// The 32-byte Solana pubkey the user named: `6sMX5pDw7cBaVohMadPEo9SksjbKjBKtvRBDpdtvVw8A`.
const SOL_DESTINATION: [u8; 32] = [
    0x57, 0x30, 0xac, 0xf5, 0x97, 0x9d, 0xd7, 0xca, 0x59, 0x04, 0x23, 0xc0, 0x5e, 0xee, 0x41, 0xea,
    0x5d, 0x31, 0xc5, 0x67, 0x3e, 0x6a, 0x8b, 0x59, 0x0c, 0x00, 0x6f, 0xc8, 0xc4, 0xac, 0x3d, 0x47,
];

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The incident log, byte for byte as the chain returned it: topics
/// `[DepositCreated, 0x1e, depositor, 0x04]` and
/// `abi.encode(135e18, 13500000000, bytes(<32-byte pubkey>))`.
fn incident_log(block_hash_bytes: [u8; 32]) -> EvmRawLog {
    let topics = [
        "da95cbec0e8506ff53e79872faa19696b138e68538e0228071f9675305a9b537",
        "000000000000000000000000000000000000000000000000000000000000001e",
        "0000000000000000000000005376d8ff23b3eaeeef9fbe7857ebe4486191f53b",
        "0000000000000000000000000000000000000000000000000000000000000004",
    ]
    .iter()
    .map(|t| {
        let v = hex(t);
        let mut w = [0u8; 32];
        w.copy_from_slice(&v);
        w
    })
    .collect();
    let data = hex(
        "000000000000000000000000000000000000000000000007518058bd45bc0000\
         0000000000000000000000000000000000000000000000000000000324a9a700\
         0000000000000000000000000000000000000000000000000000000000000060\
         0000000000000000000000000000000000000000000000000000000000000020\
         5730acf5979dd7ca590423c05eee41ea5d31c5673e6a8b590c006fc8c4ac3d47",
    );
    EvmRawLog {
        address: V1,
        topics,
        data,
        block_number: INCIDENT_BLOCK,
        block_hash: EvmBlockHash::from_bytes(block_hash_bytes),
        tx_hash: EvmTxHash::from_bytes(INCIDENT_TX),
        log_index: 40,
        removed: false,
    }
}

/// A node standing in for Robinhood chain at a head deep enough past the
/// incident block, with the receipt for the incident transaction. The
/// mock answers `block_by_number(n)` with `block_hash(n, 0)`, so a log
/// that names that hash is canonical and one that names another is a
/// reorg.
fn node_with_receipt(logs: Vec<EvmRawLog>, success: bool) -> MockNode {
    let node = MockNode::new(V1);
    node.with(|s| {
        s.head = INCIDENT_BLOCK + DEPTH + 5;
        s.receipts.insert(
            INCIDENT_TX,
            EvmReceipt {
                tx_hash: EvmTxHash::from_bytes(INCIDENT_TX),
                success,
                block_number: INCIDENT_BLOCK,
                block_hash: EvmBlockHash::from_bytes(block_hash(INCIDENT_BLOCK, 0)),
                gas_used: 136_850,
                logs,
            },
        );
    });
    node
}

fn v1_indexer_config(node: &MockNode) -> RobinhoodIndexerConfig {
    let mut cfg = node.indexer_config();
    cfg.bridge_contract = V1;
    cfg.confirmation_depth = DEPTH;
    cfg
}

fn ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().expect("an in-memory ledger");
    // The destination reserve exists (so the fold can read its gates) —
    // and is irrelevant to the outcome, because the route is folded CLOSED.
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000_000_000,
            0,
            1_000_000_000_000,
            500_000_000_000,
            250_000_000_000,
            NOW,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            1_000_000_000_000,
            0,
            1_000_000_000_000,
            500_000_000_000,
            250_000_000_000,
            NOW,
        )
        .unwrap();
    ledger
}

fn inputs() -> RecoverInputs {
    RecoverInputs {
        fee_bps: 300,
        source_minimum: crate::min_transfer::SOURCE_MINIMUM_CANONICAL,
        solana_decimals: Some(SOLANA_DECIMALS),
        goldcoin_network: crate::goldcoin::address::Network::Testnet,
    }
}

async fn run(node: &MockNode, ledger: &mut Ledger) -> Result<Recovered, RecoverError> {
    recover(
        node,
        ledger,
        &v1_indexer_config(node),
        EvmTxHash::from_bytes(INCIDENT_TX),
        inputs(),
        NOW,
    )
    .await
}

// =====================================================================
// The incident deposit, recovered exactly once, into ManualReview
// =====================================================================

#[tokio::test]
async fn the_v1_rhn_to_sol_deposit_is_decoded_recorded_final_and_parked_in_manual_review() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], true);
    let mut ledger = ledger();

    let recovered = run(&node, &mut ledger).await.expect("recovers");

    // Decoded from the log, not from arguments.
    assert_eq!(recovered.contract, V1);
    assert_eq!(recovered.obligation_index, 30);
    assert_eq!(recovered.route, Route::RhnToSol);
    assert_eq!(recovered.depositor, EvmAddress::from_bytes(DEPOSITOR_5376));
    assert_eq!(
        recovered.destination,
        SOL_DESTINATION.to_vec(),
        "the Solana pubkey, verbatim"
    );
    assert_eq!(
        recovered.amount_canonical.0, 13_500_000_000,
        "135 GLC canonical"
    );
    assert_eq!(recovered.block_number, INCIDENT_BLOCK);
    assert_eq!(recovered.observation, RobinhoodObservationOutcome::Recorded);

    // The observation is Final under its durable V1 identity.
    let row = ledger
        .robinhood_observation_by_source(V1.to_bytes(), 30)
        .unwrap()
        .expect("recorded");
    assert_eq!(row.finality, RobinhoodFinality::Final);
    assert_eq!(row.observation.tx_hash, INCIDENT_TX);
    assert_eq!(row.observation.log_index, 40);
    assert_eq!(
        EvmU256::from_be_bytes(row.observation.amount_robinhood_atomic),
        EvmU256::from_u128(135 * 10u128.pow(18))
    );

    // Exactly one request, in ManualReview, holding no capacity, with
    // the decoded destination and amounts.
    let FoldOutcome::FoldedManualReview { request_id } = recovered.fold else {
        panic!("expected a parked request, got {:?}", recovered.fold);
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::RhnToSol);
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(request.gross_amount_atomic, 13_500_000_000);
    assert_eq!(request.fee_bps, 300);
    assert_eq!(request.net_amount_atomic, 13_095_000_000, "135 GLC less 3%");
    assert_eq!(request.recipient, SOL_DESTINATION.to_vec());
    assert_eq!(request.source_obligation_index, Some(30));
    assert_eq!(
        ledger
            .requests_by_state(Direction::RhnToSol, RequestState::ManualReview)
            .unwrap()
            .len(),
        1
    );
    let (_, _, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(
        (reserved, pending),
        (0, 0),
        "a parked deposit reserves nothing"
    );

    // No scan anchor was written and the scanner's cursor is untouched.
    assert!(ledger.robinhood_scan_cursor().unwrap().is_none());
}

/// Recovery is idempotent at both layers: the observation is found, the
/// fold reports the existing request, and nothing new appears.
#[tokio::test]
async fn a_second_recovery_of_the_same_deposit_creates_nothing() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], true);
    let mut ledger = ledger();
    let first = run(&node, &mut ledger).await.unwrap();
    let second = run(&node, &mut ledger).await.unwrap();

    assert_eq!(
        second.observation,
        RobinhoodObservationOutcome::AlreadyRecorded
    );
    assert_eq!(
        second.fold,
        FoldOutcome::AlreadyFolded {
            request_id: first.request_id()
        }
    );
    assert_eq!(ledger.robinhood_observations().unwrap().len(), 1);
    assert_eq!(
        ledger
            .requests_by_state(Direction::RhnToSol, RequestState::ManualReview)
            .unwrap()
            .len(),
        1
    );
}

// =====================================================================
// Wrong contract, wrong shape, not final, reorged, reverted: refused
// =====================================================================

/// A receipt whose DepositCreated came from a contract other than the
/// configured one (here: the log says V1, the config names another
/// bridge) is refused and nothing is recorded — the configured contract
/// is the only one this recovers for.
#[tokio::test]
async fn a_deposit_log_from_another_contract_is_ignored_and_refused() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], true);
    let mut ledger = ledger();
    let mut cfg = v1_indexer_config(&node);
    cfg.bridge_contract = BRIDGE; // not V1
    let err = recover(
        &node,
        &mut ledger,
        &cfg,
        EvmTxHash::from_bytes(INCIDENT_TX),
        inputs(),
        NOW,
    )
    .await
    .expect_err("wrong contract");
    let detail = err.to_string();
    assert!(matches!(err, RecoverError::NoDepositLog { .. }), "{detail}");
    assert!(
        detail.contains(&V1.to_checksum_string()),
        "names the other address: {detail}"
    );
    assert!(ledger.robinhood_observations().unwrap().is_empty());
}

/// Non-deposit logs in the same receipt (the token's `Transfer`) are
/// skipped, not decoded: only the configured contract's DepositCreated
/// counts.
#[tokio::test]
async fn foreign_logs_in_the_receipt_are_skipped() {
    let mut transfer = incident_log(block_hash(INCIDENT_BLOCK, 0));
    transfer.address = crate::robinhood::testkit::TOKEN;
    transfer.topics = vec![[0xdd; 32], [0x01; 32], [0x02; 32]];
    transfer.data = vec![0; 32];
    transfer.log_index = 39;
    let node = node_with_receipt(
        vec![transfer, incident_log(block_hash(INCIDENT_BLOCK, 0))],
        true,
    );
    let mut ledger = ledger();
    let recovered = run(&node, &mut ledger)
        .await
        .expect("recovers past the Transfer log");
    assert_eq!(recovered.obligation_index, 30);
}

#[tokio::test]
async fn two_deposit_logs_in_one_receipt_are_refused() {
    let mut second = incident_log(block_hash(INCIDENT_BLOCK, 0));
    second.log_index = 41;
    let node = node_with_receipt(
        vec![incident_log(block_hash(INCIDENT_BLOCK, 0)), second],
        true,
    );
    let mut ledger = ledger();
    let err = run(&node, &mut ledger).await.expect_err("two deposits");
    assert!(
        matches!(err, RecoverError::MultipleDepositLogs { count: 2, .. }),
        "{err}"
    );
    assert!(ledger.robinhood_observations().unwrap().is_empty());
}

#[tokio::test]
async fn a_reverted_transaction_is_refused() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], false);
    let mut ledger = ledger();
    let err = run(&node, &mut ledger).await.expect_err("reverted");
    assert!(matches!(err, RecoverError::Reverted { .. }), "{err}");
    assert!(ledger.robinhood_observations().unwrap().is_empty());
}

#[tokio::test]
async fn an_unmined_transaction_is_refused() {
    let node = MockNode::new(V1);
    node.with(|s| s.head = INCIDENT_BLOCK + 100);
    let mut ledger = ledger();
    let err = run(&node, &mut ledger).await.expect_err("no receipt");
    assert!(matches!(err, RecoverError::NotMined { .. }), "{err}");
}

#[tokio::test]
async fn a_deposit_shallower_than_the_confirmation_depth_is_refused() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], true);
    node.with(|s| s.head = INCIDENT_BLOCK + DEPTH - 1);
    let mut ledger = ledger();
    let err = run(&node, &mut ledger).await.expect_err("too shallow");
    assert!(
        matches!(
            err,
            RecoverError::NotFinal {
                required: DEPTH,
                ..
            }
        ),
        "{err}"
    );
    assert!(ledger.robinhood_observations().unwrap().is_empty());
}

/// The log names a block hash the chain no longer holds at that height:
/// a reorged deposit is not recorded.
#[tokio::test]
async fn a_log_whose_block_is_no_longer_canonical_is_refused() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 1))], true);
    let mut ledger = ledger();
    let err = run(&node, &mut ledger).await.expect_err("reorged");
    assert!(matches!(err, RecoverError::Reorged { .. }), "{err}");
    assert!(ledger.robinhood_observations().unwrap().is_empty());
}

// =====================================================================
// V1 history is untouched, and the other inbound route works too
// =====================================================================

/// Existing V1 observations and requests (here: an earlier RhnToGlc
/// deposit already folded) are not modified by recovering a later one.
#[tokio::test]
async fn existing_v1_rows_are_untouched() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], true);
    let mut ledger = ledger();

    // Prior V1 history: obligation #2 (RhnToGlc), observed and folded
    // through the normal path.
    let mut earlier = DepositParams::valid(2, 59_542_597, 100_00000000);
    earlier.depositor = EvmAddress::from_bytes(DEPOSITOR_5376);
    let log = deposit_log(&earlier);
    let prior = RobinhoodDepositObservation {
        source_contract: V1.to_bytes(),
        obligation_index: 2,
        route: Route::RhnToGlc,
        depositor: DEPOSITOR_5376,
        destination: log.data[..4].to_vec(),
        amount_robinhood_atomic: EvmU256::from_u128(100 * 10u128.pow(18)).to_be_bytes(),
        amount_canonical_atomic: 100_00000000,
        tx_hash: log.tx_hash.to_bytes(),
        log_index: 0,
        block_number: 59_542_597,
        block_hash: log.block_hash.to_bytes(),
    };
    ledger
        .robinhood_record_final_observation(&prior, NOW - 1000)
        .unwrap();
    let before = ledger.robinhood_observations().unwrap();
    assert_eq!(before.len(), 1);

    run(&node, &mut ledger).await.expect("recovers #30");

    let after = ledger.robinhood_observations().unwrap();
    assert_eq!(after.len(), 2);
    let still = after
        .iter()
        .find(|r| r.observation.obligation_index == 2)
        .unwrap();
    assert_eq!(still.observation, before[0].observation);
    assert_eq!(still.finality, before[0].finality);
    assert_eq!(still.finalized_at, before[0].finalized_at);
}

/// The other inbound route goes through the Goldcoin-destination fold,
/// also closed, also ManualReview.
#[tokio::test]
async fn an_rhn_to_glc_deposit_is_parked_through_the_goldcoin_fold() {
    let mut params = DepositParams::valid(29, INCIDENT_BLOCK, 300_00000000);
    params.tx_seed = 29;
    params.destination = crate::goldcoin::address::encode_p2pkh(
        &[0x42; 20],
        crate::goldcoin::address::Network::Testnet,
    )
    .into_bytes();
    let mut log = deposit_log(&params);
    log.address = V1;
    log.tx_hash = EvmTxHash::from_bytes(INCIDENT_TX);
    let node = node_with_receipt(vec![log], true);
    let mut ledger = ledger();
    let recovered = run(&node, &mut ledger).await.expect("recovers");
    assert_eq!(recovered.route, Route::RhnToGlc);
    assert_eq!(recovered.obligation_index, 29);
    let FoldOutcome::FoldedManualReview { request_id } = recovered.fold else {
        panic!("{:?}", recovered.fold)
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::RhnToGlc);
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(request.gross_amount_atomic, 300_00000000);
}

/// A Solana-bound deposit cannot be folded without the reserve mint's
/// decimals; the observation is still recorded (it is a fact), and a
/// rerun with the decimals supplied folds it.
#[tokio::test]
async fn rhn_to_sol_without_solana_decimals_records_but_refuses_to_fold() {
    let node = node_with_receipt(vec![incident_log(block_hash(INCIDENT_BLOCK, 0))], true);
    let mut ledger = ledger();
    let mut without = inputs();
    without.solana_decimals = None;
    let err = recover(
        &node,
        &mut ledger,
        &v1_indexer_config(&node),
        EvmTxHash::from_bytes(INCIDENT_TX),
        without,
        NOW,
    )
    .await
    .expect_err("no decimals");
    assert!(
        matches!(err, RecoverError::Fold(FoldError::UnsupportedRoute { .. })),
        "{err}"
    );
    assert_eq!(
        ledger.robinhood_observations().unwrap().len(),
        1,
        "the fact is kept"
    );
    assert!(ledger
        .requests_by_state(Direction::RhnToSol, RequestState::ManualReview)
        .unwrap()
        .is_empty());

    let recovered = run(&node, &mut ledger).await.expect("folds on rerun");
    assert_eq!(
        recovered.observation,
        RobinhoodObservationOutcome::AlreadyRecorded
    );
    assert!(matches!(
        recovered.fold,
        FoldOutcome::FoldedManualReview { .. }
    ));
}
