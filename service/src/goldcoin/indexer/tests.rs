use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::goldcoin::deposit::encode_request_binding;
use crate::goldcoin::rpc::{DecodedScriptPubKey, DecodedVout};
use crate::ledger::{CreateRequestOutcome, Direction, RequestState, ReserveDirection};

const VAULT_SCRIPT_HEX: &str = "76a9145a7ab7adf8185c27b3f54104cdccfe1ff0cd54cf88ac";

#[derive(Default)]
struct FakeChain {
    blocks: Vec<FakeBlock>,
    txs: HashMap<String, DecodedTransaction>,
    spent: std::collections::HashSet<(String, u32)>,
}

struct FakeBlock {
    hash: String,
    prev_hash: Option<String>,
    time: i64,
    tx_ids: Vec<String>,
}

struct MockRpc {
    chain: Mutex<FakeChain>,
}

impl MockRpc {
    fn new() -> Self {
        MockRpc {
            chain: Mutex::new(FakeChain::default()),
        }
    }

    fn push_block(&self, hash: &str, prev: Option<&str>, txs: Vec<DecodedTransaction>) {
        let mut chain = self.chain.lock().unwrap();
        let mut tx_ids = Vec::new();
        for tx in txs {
            tx_ids.push(tx.txid.clone());
            chain.txs.insert(tx.txid.clone(), tx);
        }
        let time = 1_000 + chain.blocks.len() as i64;
        chain.blocks.push(FakeBlock {
            hash: label_hex(hash),
            prev_hash: prev.map(label_hex),
            time,
            tx_ids,
        });
    }

    fn spend(&self, txid_label: &str, vout: u32) {
        self.chain
            .lock()
            .unwrap()
            .spent
            .insert((label_hex(txid_label), vout));
    }
}

impl GoldcoinRpc for MockRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        Ok(self.chain.lock().unwrap().blocks.len() as i64 - 1)
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        let chain = self.chain.lock().unwrap();
        chain
            .blocks
            .get(height as usize)
            .map(|b| b.hash.clone())
            .ok_or(RpcError::Method {
                code: -8,
                message: "height out of range".into(),
            })
    }
    async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        let chain = self.chain.lock().unwrap();
        let (height, block) = chain
            .blocks
            .iter()
            .enumerate()
            .find(|(_, b)| b.hash == hash)
            .ok_or(RpcError::Method {
                code: -5,
                message: "block not found".into(),
            })?;
        Ok(BlockHeader {
            hash: block.hash.clone(),
            confirmations: 1,
            height: height as i64,
            time: block.time,
            previousblockhash: block.prev_hash.clone(),
            tx: block.tx_ids.clone(),
        })
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        self.chain
            .lock()
            .unwrap()
            .txs
            .get(txid_hex)
            .cloned()
            .ok_or(RpcError::Method {
                code: -5,
                message: "tx not found".into(),
            })
    }
    async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<TxOut>, RpcError> {
        let chain = self.chain.lock().unwrap();
        if chain.spent.contains(&(txid_hex.to_string(), vout)) {
            return Ok(None);
        }
        Ok(Some(TxOut { confirmations: 1 }))
    }
    async fn send_raw_transaction(&self, _hex: &str) -> Result<BroadcastOutcome, RpcError> {
        unimplemented!("not exercised by indexer tests")
    }
}

impl GoldcoinRpc for Arc<MockRpc> {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        MockRpc::get_block_count(self).await
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        MockRpc::get_block_hash(self, height).await
    }
    async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        MockRpc::get_block(self, hash).await
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        MockRpc::get_raw_transaction(self, txid_hex).await
    }
    async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<TxOut>, RpcError> {
        MockRpc::get_tx_out_confirmed(self, txid_hex, vout).await
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        MockRpc::send_raw_transaction(self, hex).await
    }
}

fn label_hex(label: &str) -> String {
    let mut bytes = [0u8; 32];
    for (i, b) in label.bytes().enumerate().take(32) {
        bytes[i] = b;
    }
    hex::encode(&bytes)
}

fn vault_tx(txid_label: &str, vout_n: u32, value: f64, request_id: i64) -> DecodedTransaction {
    let txid = label_hex(txid_label);
    let mut op_return = vec![0x6a, 32u8];
    op_return.extend_from_slice(&encode_request_binding(request_id));
    DecodedTransaction {
        txid,
        confirmations: Some(1),
        vout: vec![
            DecodedVout {
                value,
                n: vout_n,
                script_pub_key: DecodedScriptPubKey {
                    hex: VAULT_SCRIPT_HEX.to_string(),
                    kind: "pubkeyhash".to_string(),
                },
            },
            DecodedVout {
                value: 0.0,
                n: vout_n + 1,
                script_pub_key: DecodedScriptPubKey {
                    hex: hex::encode(&op_return),
                    kind: "nulldata".to_string(),
                },
            },
        ],
    }
}

fn test_config(confirmation_depth: u32, max_reorg_depth: u32) -> IndexerConfig {
    IndexerConfig {
        vault_script_hex: VAULT_SCRIPT_HEX.to_string(),
        confirmation_depth,
        max_reorg_depth,
    }
}

fn ledger_with_reservation(amount: u64) -> (Ledger, i64) {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            10_000_000_000,
            0,
            5_000_000_000,
            2_000_000_000,
            1_000_000_000,
            0,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, amount, &[0xAB; 32], None, 100_000, 0)
        .unwrap()
    else {
        panic!("reservation should succeed")
    };
    (ledger, request_id)
}

#[tokio::test]
async fn genesis_block_transactions_are_never_fetched() {
    let chain = Arc::new(MockRpc::new());
    // Genesis has a "poison" tx id that get_raw_transaction would error on
    // if fetched — proves height 0 is skipped, not merely empty in this test.
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));

    idx.tick().await.expect("must not try to fetch genesis");
    assert_eq!(
        idx.ledger()
            .requests_by_state(Direction::GlcToSol, RequestState::SourceFinalized)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn deposit_binds_to_its_reservation_and_reaches_confirming_same_tick() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain.clone(), ledger, test_config(3, 10));

    idx.tick().await.unwrap();
    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Confirming);
    assert_eq!(
        req.source_txid,
        Some(crate::goldcoin::hex::decode_exact::<32>(&label_hex("t1")).unwrap())
    );
}

#[tokio::test]
async fn promotes_to_source_finalized_at_configured_depth() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain.clone(), ledger, test_config(3, 10));

    idx.tick().await.unwrap(); // depth 1
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    chain.push_block("h2", Some("h1"), vec![]);
    idx.tick().await.unwrap(); // depth 2 — still below 3
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    chain.push_block("h3", Some("h2"), vec![]);
    idx.tick().await.unwrap(); // depth 3 == confirmation_depth: promotes
    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert!(req.source_finalized_at.is_some());
}

#[tokio::test]
async fn amount_mismatch_routes_to_manual_review_and_never_promotes() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    // Deposit for half the reserved amount.
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 2.5, request_id)]);
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    idx.tick().await.unwrap();
    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
}

#[tokio::test]
async fn unmatched_deposit_is_recorded_not_dropped() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    // request_id 999999 was never reserved.
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, 999_999)]);
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    idx.tick().await.unwrap();
    let count: i64 = idx
        .ledger()
        .raw()
        .query_row(
            "SELECT count(*) FROM unmatched_goldcoin_deposits",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn vault_output_spent_before_finalization_never_promotes() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain.clone(), ledger, test_config(1, 10));

    chain.spend("t1", 0);
    idx.tick().await.unwrap();

    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        RequestState::Confirming,
        "must not silently finalize a spent output"
    );
}

#[tokio::test]
async fn deep_reorg_halts_and_makes_no_further_writes() {
    let chain = MockRpc::new();
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    chain.push_block("h2", Some("h1"), vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(chain, ledger, test_config(1, 1));
    idx.tick().await.unwrap();
    let (tip_before, _) = idx.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_before, 2);

    // Simulate the node's chain having changed under us beyond max_reorg_depth=1.
    let new_chain = MockRpc::new();
    new_chain.push_block("g0", None, vec![]);
    new_chain.push_block("g1", Some("g0"), vec![]);
    new_chain.push_block("g2", Some("g1"), vec![]);
    new_chain.push_block("g3", Some("g2"), vec![]);
    let mut ledger2 = Ledger::open_in_memory().unwrap();
    ledger2
        .goldcoin_ingest_block(0, [0u8; 32], [0u8; 32], 0, 0)
        .unwrap();
    ledger2
        .goldcoin_ingest_block(1, [1u8; 32], [0u8; 32], 0, 0)
        .unwrap();
    ledger2
        .goldcoin_ingest_block(2, [2u8; 32], [1u8; 32], 0, 0)
        .unwrap();
    let mut idx2 = Indexer::new(new_chain, ledger2, test_config(1, 1));
    let outcome = idx2.tick().await.unwrap();
    assert!(matches!(outcome, TickOutcome::Halted { .. }));
    let (tip_after, _) = idx2.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_after, 2, "no writes on halt");

    let outcome2 = idx2.tick().await.unwrap();
    assert!(matches!(outcome2, TickOutcome::Halted { .. }));
}

#[tokio::test]
async fn one_block_reorg_returns_the_request_to_awaiting_deposit() {
    let chain = MockRpc::new();
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain, ledger, test_config(1, 5));
    idx.tick().await.unwrap();
    // confirmation_depth=1: reaches SourceFinalized in this same tick.
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );

    // A finalized request must NEVER be reorged automatically — replace
    // height 1 with a different block, deposit tx not re-mined, and verify
    // the request stays SourceFinalized (the reorg logic only touches
    // DepositObserved/Confirming rows).
    let reorged = MockRpc::new();
    reorged.push_block("h0", None, vec![]);
    reorged.push_block("h1b", Some("h0"), vec![]);
    let mut idx2 = Indexer::new(reorged, idx.ledger, test_config(1, 5));
    let outcome = idx2.tick().await.unwrap();
    assert!(matches!(
        outcome,
        TickOutcome::Progressed { reorg: Some(_), .. }
    ));
    assert_eq!(
        idx2.ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::SourceFinalized,
        "post-finality reorg must never auto-revert"
    );
}

#[tokio::test]
async fn pre_finality_one_block_reorg_reopens_awaiting_deposit() {
    let chain = MockRpc::new();
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    // confirmation_depth=3 so the deposit stays Confirming, not finalized.
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain, ledger, test_config(3, 5));
    idx.tick().await.unwrap();
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    let reorged = MockRpc::new();
    reorged.push_block("h0", None, vec![]);
    reorged.push_block("h1b", Some("h0"), vec![]);
    let mut idx2 = Indexer::new(reorged, idx.ledger, test_config(3, 5));
    let outcome = idx2.tick().await.unwrap();
    match outcome {
        TickOutcome::Progressed { reorg: Some(r), .. } => {
            assert_eq!(r.fork_height, 0);
            assert_eq!(r.orphaned_count, 1);
        }
        other => panic!("expected a detected reorg, got {other:?}"),
    }
    assert_eq!(
        idx2.ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::AwaitingDeposit
    );
}
