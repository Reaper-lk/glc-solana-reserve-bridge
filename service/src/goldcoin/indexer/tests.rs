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
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<crate::goldcoin::rpc::ListUnspentEntry>, RpcError> {
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
    async fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> Result<Vec<crate::goldcoin::rpc::ListUnspentEntry>, RpcError> {
        MockRpc::list_unspent(self, min_conf, addresses).await
    }
}

fn label_hex_bytes(label: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, b) in label.bytes().enumerate().take(32) {
        bytes[i] = b;
    }
    bytes
}

fn label_hex(label: &str) -> String {
    hex::encode(&label_hex_bytes(label))
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
        initial_checkpoint: None,
    }
}

fn test_config_with_checkpoint(
    confirmation_depth: u32,
    max_reorg_depth: u32,
    checkpoint: InitialCheckpoint,
) -> IndexerConfig {
    IndexerConfig {
        initial_checkpoint: Some(checkpoint),
        ..test_config(confirmation_depth, max_reorg_depth)
    }
}

/// Same real, node-verified 2-of-3 redeem script used by
/// `goldcoin::vault::tests` and `api::tests` — a fixed "root vault" so
/// per-request deposit addresses in these tests are derived exactly the
/// way `BridgeApi::create_glc_to_sol_transfer` derives them in production.
const TEST_ROOT_REDEEM_SCRIPT: &str = "5221028e7147e643d67093dc8ca6a8fb888f1a452dddc62de991c7ed72080d65a421e42102f1c88ca7176c3ffee952ee6fae697991b257b6d53c3bc88e81cfe99adbcdbee5210256220bb7865197a40c4590ac80f12ef18e9063eac2eff92c4476ec27034042f953ae";

fn test_root_vault() -> crate::goldcoin::vault::MultisigVault {
    crate::goldcoin::vault::MultisigVault::from_redeem_script_hex(
        TEST_ROOT_REDEEM_SCRIPT,
        crate::goldcoin::address::Network::Testnet,
    )
    .unwrap()
}

/// Derives `request_id`'s unique deposit address/script exactly the way
/// the API does, and persists it — mirroring
/// `BridgeApi::create_glc_to_sol_transfer`'s `Reserved` arm so indexer
/// tests exercise the real derive-then-persist-then-match pipeline rather
/// than a hand-rolled stand-in scriptPubKey.
fn assign_deposit_address(ledger: &mut Ledger, request_id: i64) -> String {
    let derived = crate::goldcoin::derivation::derive_request_vault(
        &test_root_vault(),
        request_id,
        crate::goldcoin::address::Network::Testnet,
    )
    .unwrap();
    let script_pubkey_hex = derived.script_pubkey_hex();
    ledger
        .set_glc_to_sol_deposit_address(
            request_id,
            derived.address(),
            &script_pubkey_hex,
            &derived.redeem_script_hex(),
        )
        .unwrap();
    script_pubkey_hex
}

/// A transaction paying `value` GLC to `script_pub_key_hex`, with no
/// OP_RETURN at all — exactly what an ordinary Goldcoin wallet produces
/// when sending to a per-request deposit address (the whole point of this
/// feature: no wallet-side OP_RETURN support required).
fn direct_tx(
    txid_label: &str,
    vout_n: u32,
    value: f64,
    script_pub_key_hex: &str,
) -> DecodedTransaction {
    DecodedTransaction {
        txid: label_hex(txid_label),
        confirmations: Some(1),
        vout: vec![DecodedVout {
            value,
            n: vout_n,
            script_pub_key: DecodedScriptPubKey {
                hex: script_pub_key_hex.to_string(),
                kind: "scripthash".to_string(),
            },
        }],
    }
}

/// A transaction with `amounts_atomic.len()` outputs, ALL paying
/// `VAULT_SCRIPT_HEX` with no OP_RETURN at all — exactly the shape
/// `glc-admin split-vault-utxo` produces (`goldcoin::split::
/// build_unsigned_split_tx`): one vault-owned UTXO split into several
/// smaller vault-owned ones, never carrying a request-binding OP_RETURN
/// since it isn't a deposit at all.
fn split_output_tx(txid_label: &str, amounts_atomic: &[u64]) -> DecodedTransaction {
    DecodedTransaction {
        txid: label_hex(txid_label),
        confirmations: Some(1),
        vout: amounts_atomic
            .iter()
            .enumerate()
            .map(|(n, &amount)| DecodedVout {
                value: amount as f64 / 100_000_000.0,
                n: n as u32,
                script_pub_key: DecodedScriptPubKey {
                    hex: VAULT_SCRIPT_HEX.to_string(),
                    kind: "scripthash".to_string(),
                },
            })
            .collect(),
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
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: amount,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: amount,
                net_destination_atomic: amount,
            },
            &[0xAB; 32],
            None,
            100_000,
            0,
        )
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

/// Regression for the vault-split-indexing production incident
/// (docs/09-runbook.md "Vault UTXO splitting"): a real 67,270 GLC UTXO
/// split into 6 chunks (12,500 GLC target) produced six vault-owned
/// outputs with no OP_RETURN — indistinguishable, before this fix, from an
/// unexplained deposit. Uses the exact production amounts (pinned in
/// `goldcoin::split::tests` as a golden vector) so this test fails if the
/// distribution formula or the matching logic ever drifts from what
/// actually happened on chain.
#[tokio::test]
async fn six_output_vault_split_is_recognized_and_never_recorded_unmatched() {
    use crate::goldcoin::coin::VaultUtxo;
    use crate::goldcoin::split::SplitPlan;

    const SOURCE_AMOUNT: u64 = 67_270 * 100_000_000;
    const FEE_ATOMIC: u64 = 51_300;
    const CHUNK_TARGET: u64 = 12_500 * 100_000_000;
    const LARGER: u64 = 1_121_166_658_117; // 11,211.66658117 GLC
    const SMALLER: u64 = 1_121_166_658_116; // 11,211.66658116 GLC
    let output_amounts = vec![LARGER, LARGER, LARGER, LARGER, SMALLER, SMALLER];
    assert_eq!(
        output_amounts.iter().sum::<u64>() + FEE_ATOMIC,
        SOURCE_AMOUNT,
        "test setup: golden vector must itself conserve value"
    );

    let mut ledger = Ledger::open_in_memory().unwrap();
    let plan = SplitPlan {
        source: VaultUtxo {
            txid: label_hex_bytes("source-utxo"),
            vout: 1,
            amount_atomic: SOURCE_AMOUNT,
            script_pubkey_hex: VAULT_SCRIPT_HEX.to_string(),
        },
        vault_script_pubkey: vec![0xAA], // not inspected by this fix
        output_amounts,
        fee_atomic: FEE_ATOMIC,
    };
    let split_id = ledger
        .record_vault_utxo_split_built(&plan, CHUNK_TARGET, "deadbeef", "test split", 0)
        .unwrap();
    ledger
        .record_vault_utxo_split_signed(split_id, "deadbeef", 0)
        .unwrap();
    let split_txid = label_hex_bytes("split-tx");
    ledger
        .record_vault_utxo_split_broadcast(split_id, split_txid, 0)
        .unwrap();

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block(
        "h1",
        Some("h0"),
        vec![split_output_tx(
            "split-tx",
            &[LARGER, LARGER, LARGER, LARGER, SMALLER, SMALLER],
        )],
    );
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
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "all six recognized split outputs must never be recorded as unmatched"
    );

    // Idempotent on rescan/restart: ticking again over the same chain must
    // not create anything either.
    idx.tick().await.unwrap();
    let count_again: i64 = idx
        .ledger()
        .raw()
        .query_row(
            "SELECT count(*) FROM unmatched_goldcoin_deposits",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert_eq!(count_again, 0);
}

/// "Exact match only" — a genuinely wrong amount on an otherwise-real
/// split transaction must still be recorded unmatched, never waved through
/// just because the txid belongs to a known split.
#[tokio::test]
async fn a_tampered_split_output_amount_is_still_recorded_unmatched() {
    use crate::goldcoin::coin::VaultUtxo;
    use crate::goldcoin::split::SplitPlan;

    const SOURCE_AMOUNT: u64 = 67_270 * 100_000_000;
    const FEE_ATOMIC: u64 = 51_300;
    const LARGER: u64 = 1_121_166_658_117;
    const SMALLER: u64 = 1_121_166_658_116;

    let mut ledger = Ledger::open_in_memory().unwrap();
    let plan = SplitPlan {
        source: VaultUtxo {
            txid: label_hex_bytes("source-utxo"),
            vout: 1,
            amount_atomic: SOURCE_AMOUNT,
            script_pubkey_hex: VAULT_SCRIPT_HEX.to_string(),
        },
        vault_script_pubkey: vec![0xAA],
        output_amounts: vec![LARGER, LARGER, LARGER, LARGER, SMALLER, SMALLER],
        fee_atomic: FEE_ATOMIC,
    };
    let split_id = ledger
        .record_vault_utxo_split_built(&plan, 12_500 * 100_000_000, "deadbeef", "test split", 0)
        .unwrap();
    ledger
        .record_vault_utxo_split_signed(split_id, "deadbeef", 0)
        .unwrap();
    let split_txid = label_hex_bytes("split-tx");
    ledger
        .record_vault_utxo_split_broadcast(split_id, split_txid, 0)
        .unwrap();

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    // vout 0 is off by one atomic unit from the persisted plan.
    chain.push_block(
        "h1",
        Some("h0"),
        vec![split_output_tx(
            "split-tx",
            &[LARGER + 1, LARGER, LARGER, LARGER, SMALLER, SMALLER],
        )],
    );
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    idx.tick().await.unwrap();

    let rows: Vec<(i64, i64)> = {
        let ledger = idx.ledger();
        let mut stmt = ledger
            .raw()
            .prepare("SELECT vout, amount_atomic FROM unmatched_goldcoin_deposits")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![(0, (LARGER + 1) as i64)],
        "only the tampered output should be recorded unmatched; the other five, being exact matches, must not be"
    );
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

    // Must not silently finalize a spent output, AND must not strand the
    // request in Confirming forever either — fail closed to ManualReview
    // with an explicit, distinguishable reason (the previous behavior,
    // pinned here until this fix, was to warn and leave it in Confirming
    // permanently with its reservation held and no operator-visible
    // terminal state).
    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        RequestState::ManualReview,
        "must fail closed to ManualReview, not stay stranded in Confirming"
    );
    assert_eq!(
        req.manual_review_note.as_deref(),
        Some("deposit_spent_before_finalized")
    );

    // Idempotent: a second tick against the same still-spent output must
    // not error or double-transition.
    idx.tick().await.unwrap();
    let req_again = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req_again.state, RequestState::ManualReview);
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
    // height 1 with a different block, deposit tx not re-mined. Since
    // height 1 is this request's own `source_block_height`, this is now
    // detected as a dedicated post-finality reorg: the tick halts before
    // any rollback/reindex, both reserves are paused, and a distinct
    // audit event is recorded — never the routine
    // `Progressed { reorg: Some(_) }` path (docs/22-production-
    // readiness-review.md, docs/10-threat-model.md).
    let reorged = MockRpc::new();
    reorged.push_block("h0", None, vec![]);
    reorged.push_block("h1b", Some("h0"), vec![]);
    let mut idx2 = Indexer::new(reorged, idx.ledger, test_config(1, 5));
    idx2.ledger_mut()
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000_000_000,
            0,
            5_000_000_000,
            2_000_000_000,
            1_000_000_000,
            0,
        )
        .unwrap();
    let outcome = idx2.tick().await.unwrap();
    assert!(
        matches!(
            outcome,
            TickOutcome::PostFinalityReorgHalted {
                fork_height: 0,
                old_tip_height: 1,
                ..
            }
        ),
        "expected a dedicated post-finality halt, got {outcome:?}"
    );
    assert_eq!(
        idx2.ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::SourceFinalized,
        "post-finality reorg must never auto-revert"
    );
    assert!(
        idx2.ledger()
            .is_paused(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "a post-finality reorg must pause the Goldcoin reserve"
    );
    assert!(
        idx2.ledger()
            .is_paused(ReserveDirection::SolanaReserve)
            .unwrap(),
        "a post-finality reorg must pause the Solana reserve too — it undermines confidence \
         in the whole Goldcoin-side ledger, not just this one direction"
    );
    assert_eq!(
        idx2.ledger().post_finality_reorg_event_count().unwrap(),
        1,
        "a dedicated audit event, distinct from the routine goldcoin_reorg_events table"
    );

    // The next tick reports the same halt without touching the database
    // or the network again.
    let outcome2 = idx2.tick().await.unwrap();
    assert!(matches!(
        outcome2,
        TickOutcome::PostFinalityReorgHalted { .. }
    ));
    assert_eq!(
        idx2.ledger().post_finality_reorg_event_count().unwrap(),
        1,
        "a repeated tick within the same process must not record a duplicate event"
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

// ------------------------------------------------- initial checkpoint --
//
// docs/09-runbook.md "Goldcoin indexer initial checkpoint": a brand-new
// ledger against an already-tall production chain would otherwise need a
// many-hours full resync from height 0. These tests cover the safe
// bootstrap mechanism that skips that resync — see
// `Indexer::bootstrap_from_checkpoint_or_genesis`'s own docs for the
// exact fail-closed contract each test below is pinning.

fn checkpoint(height: i64, hash: &str, acknowledged: bool) -> InitialCheckpoint {
    InitialCheckpoint {
        height,
        hash: label_hex(hash),
        operator_acknowledged_no_prior_deposits: acknowledged,
    }
}

#[tokio::test]
async fn checkpoint_bootstrap_on_empty_ledger_starts_at_checkpoint_height_inclusive() {
    let chain = Arc::new(MockRpc::new());
    // h0..h5 — a deposit strictly BEFORE the checkpoint (h2, height 2) and
    // one AT the checkpoint height itself (h3, height 3): the checkpoint
    // means "before this height", not "at or before" (requirement 4).
    let (mut ledger, before_request) = ledger_with_reservation(500_000_000);
    // A second reservation on the same ledger/direction so both requests
    // exist before any tick runs.
    let CreateRequestOutcome::Reserved {
        request_id: at_request,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 500_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 500_000_000,
                net_destination_atomic: 500_000_000,
            },
            &[0xCD; 32],
            None,
            100_000,
            0,
        )
        .unwrap()
    else {
        panic!("second reservation should succeed")
    };

    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    chain.push_block(
        "h2",
        Some("h1"),
        vec![vault_tx("before", 0, 5.0, before_request)],
    );
    chain.push_block("h3", Some("h2"), vec![vault_tx("at", 0, 5.0, at_request)]);
    chain.push_block("h4", Some("h3"), vec![]);
    chain.push_block("h5", Some("h4"), vec![]);

    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(3, "h3", true)),
    );

    let outcome = idx.tick().await.expect("valid checkpoint must succeed");
    assert!(matches!(outcome, TickOutcome::Progressed { .. }));

    let (tip_height, _) = idx.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_height, 5, "must reach the live tip in this same tick");
    assert_eq!(
        idx.ledger().goldcoin_block_hash_at(3).unwrap(),
        Some(crate::goldcoin::hex::decode_exact::<32>(&label_hex("h3")).unwrap()),
        "the checkpoint height itself must be recorded with its real hash"
    );
    assert!(
        idx.ledger().goldcoin_block_hash_at(2).unwrap().is_none(),
        "heights strictly before the checkpoint must never be recorded"
    );

    // Strictly before the checkpoint: never scanned, request untouched.
    assert_eq!(
        idx.ledger()
            .get_request(before_request)
            .unwrap()
            .unwrap()
            .state,
        RequestState::AwaitingDeposit,
        "a deposit strictly before the checkpoint must be treated as outside supported history"
    );
    // AT the checkpoint height: scanned normally, same as any other block.
    assert_eq!(
        idx.ledger().get_request(at_request).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "a deposit AT the checkpoint height must be indexed normally, not excluded"
    );
}

#[tokio::test]
async fn checkpoint_with_wrong_hash_is_rejected_and_writes_nothing() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(1, "not-the-real-hash", true)),
    );

    let err = idx.tick().await.expect_err("wrong hash must be rejected");
    assert!(
        matches!(err, IndexerError::CheckpointHashMismatch { height: 1, .. }),
        "expected CheckpointHashMismatch, got {err:?}"
    );
    assert!(
        idx.ledger().goldcoin_chain_tip().unwrap().is_none(),
        "a rejected checkpoint must leave the ledger completely untouched"
    );
}

#[tokio::test]
async fn checkpoint_above_the_live_tip_is_rejected() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    // Live tip is height 1 (2 blocks pushed); checkpoint claims height 5.
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(5, "h5", true)),
    );

    let err = idx
        .tick()
        .await
        .expect_err("a checkpoint above the live tip must be rejected");
    assert!(
        matches!(err, IndexerError::CheckpointAboveTip { height: 5, tip: 1 }),
        "expected CheckpointAboveTip{{height: 5, tip: 1}}, got {err:?}"
    );
    assert!(idx.ledger().goldcoin_chain_tip().unwrap().is_none());
}

#[tokio::test]
async fn checkpoint_with_negative_height_is_rejected_never_falls_back_to_zero() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(-1, "h0", true)),
    );

    let err = idx
        .tick()
        .await
        .expect_err("negative height must be rejected");
    assert!(matches!(err, IndexerError::InvalidCheckpointConfig(_)));
    assert!(
        idx.ledger().goldcoin_chain_tip().unwrap().is_none(),
        "must never silently start at height 0 instead of erroring"
    );
}

#[tokio::test]
async fn checkpoint_with_malformed_hash_is_rejected_never_falls_back_to_zero() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(
        chain,
        ledger,
        IndexerConfig {
            initial_checkpoint: Some(InitialCheckpoint {
                height: 0,
                hash: "not-64-hex-chars".to_string(),
                operator_acknowledged_no_prior_deposits: true,
            }),
            ..test_config(1, 10)
        },
    );

    let err = idx
        .tick()
        .await
        .expect_err("malformed hex hash must be rejected");
    assert!(matches!(err, IndexerError::InvalidCheckpointConfig(_)));
    assert!(idx.ledger().goldcoin_chain_tip().unwrap().is_none());
}

#[tokio::test]
async fn checkpoint_without_operator_acknowledgement_is_rejected_never_falls_back_to_zero() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    // Otherwise perfectly valid — height 0, correct hash — but
    // acknowledged=false must still fail closed. This is the one check
    // that exists precisely because Goldcoin 0.15 has no `scantxoutset`
    // to verify the claim automatically.
    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(0, "h0", false)),
    );

    let err = idx
        .tick()
        .await
        .expect_err("missing operator acknowledgement must be rejected");
    assert!(matches!(err, IndexerError::InvalidCheckpointConfig(_)));
    assert!(idx.ledger().goldcoin_chain_tip().unwrap().is_none());
}

#[tokio::test]
async fn existing_ledger_ignores_the_configured_checkpoint() {
    // Ledger already has indexed blocks (height 0) — the persisted
    // cursor/reorg logic must win unconditionally; the checkpoint config
    // below is deliberately bogus (a hash that would fail verification if
    // it were ever consulted) to prove it truly is never touched.
    let chain = MockRpc::new();
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .goldcoin_ingest_block(
            0,
            crate::goldcoin::hex::decode_exact::<32>(&label_hex("h0")).unwrap(),
            [0u8; 32],
            1_000,
            0,
        )
        .unwrap();

    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(1, "this-hash-is-wrong", true)),
    );

    let outcome = idx
        .tick()
        .await
        .expect("a bogus checkpoint must be silently irrelevant once the ledger is non-empty");
    assert!(matches!(outcome, TickOutcome::Progressed { .. }));
    let (tip_height, _) = idx.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_height, 1);
}

#[tokio::test]
async fn restart_after_checkpoint_continues_from_the_persisted_tip_not_the_checkpoint_again() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    chain.push_block("h2", Some("h1"), vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    let cp = checkpoint(1, "h1", true);
    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, cp.clone()),
    );
    idx.tick().await.unwrap();
    let (tip_height, _) = idx.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_height, 2);

    // Simulate a process restart: a fresh `Indexer` over the SAME ledger
    // (now non-empty) and, deliberately, the SAME checkpoint config still
    // present — it must be ignored exactly as
    // `existing_ledger_ignores_the_configured_checkpoint` proves, so
    // restarting never re-verifies or re-applies it.
    let new_chain = Arc::new(MockRpc::new());
    new_chain.push_block("h0", None, vec![]);
    new_chain.push_block("h1", Some("h0"), vec![]);
    new_chain.push_block("h2", Some("h1"), vec![]);
    new_chain.push_block("h3", Some("h2"), vec![]);
    let mut idx2 = Indexer::new(
        new_chain,
        idx.ledger,
        test_config_with_checkpoint(1, 10, cp),
    );
    let outcome = idx2.tick().await.unwrap();
    assert!(matches!(outcome, TickOutcome::Progressed { .. }));
    let (tip_after, _) = idx2.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(
        tip_after, 3,
        "must continue forward from the persisted tip (2 -> 3), not re-bootstrap at height 1"
    );
}

#[tokio::test]
async fn reorg_detection_still_works_after_a_checkpoint_bootstrap() {
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![]);
    chain.push_block("h2", Some("h1"), vec![]);
    let ledger = Ledger::open_in_memory().unwrap();
    let mut idx = Indexer::new(
        chain,
        ledger,
        test_config_with_checkpoint(1, 10, checkpoint(1, "h1", true)),
    );
    idx.tick().await.unwrap();
    let (tip_height, _) = idx.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_height, 2);

    // The live chain now diverges at height 2 (a routine, shallow reorg
    // well within max_reorg_depth=10) — must be detected and rolled back
    // exactly as it would be with no checkpoint ever having been
    // involved.
    let reorged = MockRpc::new();
    reorged.push_block("h0", None, vec![]);
    reorged.push_block("h1", Some("h0"), vec![]);
    reorged.push_block("h2b", Some("h1"), vec![]);
    let mut idx2 = Indexer::new(
        reorged,
        idx.ledger,
        test_config_with_checkpoint(1, 10, checkpoint(1, "h1", true)),
    );
    let outcome = idx2.tick().await.unwrap();
    match outcome {
        TickOutcome::Progressed { reorg: Some(r), .. } => {
            assert_eq!(r.fork_height, 1);
            assert_eq!(r.orphaned_count, 0);
        }
        other => panic!("expected a detected reorg, got {other:?}"),
    }
    let (tip_after, _) = idx2.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_after, 2);
}

#[tokio::test]
async fn no_checkpoint_configured_retains_height_zero_start_for_dev_test_compatibility() {
    // Same shape as `genesis_block_transactions_are_never_fetched` above,
    // asserted here explicitly under this feature's own test group: an
    // absent `initial_checkpoint` must behave completely unchanged from
    // before this feature existed.
    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    let (ledger, request_id) = ledger_with_reservation(500_000_000);
    chain.push_block("h1", Some("h0"), vec![vault_tx("t1", 0, 5.0, request_id)]);
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    assert!(idx.config.initial_checkpoint.is_none());

    idx.tick().await.unwrap();
    let (tip_height, _) = idx.ledger().goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip_height, 1);
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

// ---------------------------------------------------------------------
// Per-request deposit address attribution (Step 3): the indexer must
// attribute a deposit by destination scriptPubKey -> request mapping,
// with no OP_RETURN and no amount-based attribution, while the legacy
// static-vault + OP_RETURN path keeps working unchanged for old requests.
// ---------------------------------------------------------------------

#[tokio::test]
async fn two_requests_get_different_deposit_addresses() {
    let (mut ledger, request_a) = ledger_with_reservation(500_000_000);
    let CreateRequestOutcome::Reserved {
        request_id: request_b,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 500_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 500_000_000,
                net_destination_atomic: 500_000_000,
            },
            &[0xAB; 32],
            None,
            100_000,
            0,
        )
        .unwrap()
    else {
        panic!("reservation should succeed")
    };
    assert_ne!(request_a, request_b);

    let script_a = assign_deposit_address(&mut ledger, request_a);
    let script_b = assign_deposit_address(&mut ledger, request_b);
    assert_ne!(script_a, script_b);
}

#[tokio::test]
async fn payment_to_address_a_matches_only_request_a() {
    let (mut ledger, request_a) = ledger_with_reservation(500_000_000);
    let CreateRequestOutcome::Reserved {
        request_id: request_b,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 300_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 300_000_000,
                net_destination_atomic: 300_000_000,
            },
            &[0xAB; 32],
            None,
            100_000,
            0,
        )
        .unwrap()
    else {
        panic!("reservation should succeed")
    };
    let script_a = assign_deposit_address(&mut ledger, request_a);
    let _script_b = assign_deposit_address(&mut ledger, request_b);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![direct_tx("t1", 0, 5.0, &script_a)]);
    let mut idx = Indexer::new(chain, ledger, test_config(3, 10));
    idx.tick().await.unwrap();

    assert_eq!(
        idx.ledger().get_request(request_a).unwrap().unwrap().state,
        RequestState::Confirming
    );
    // Request B never received a payment — must remain untouched.
    assert_eq!(
        idx.ledger().get_request(request_b).unwrap().unwrap().state,
        RequestState::AwaitingDeposit
    );
}

#[tokio::test]
async fn payment_to_address_b_matches_only_request_b() {
    let (mut ledger, request_a) = ledger_with_reservation(500_000_000);
    let CreateRequestOutcome::Reserved {
        request_id: request_b,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 300_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 300_000_000,
                net_destination_atomic: 300_000_000,
            },
            &[0xAB; 32],
            None,
            100_000,
            0,
        )
        .unwrap()
    else {
        panic!("reservation should succeed")
    };
    let _script_a = assign_deposit_address(&mut ledger, request_a);
    let script_b = assign_deposit_address(&mut ledger, request_b);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![direct_tx("t1", 0, 3.0, &script_b)]);
    let mut idx = Indexer::new(chain, ledger, test_config(3, 10));
    idx.tick().await.unwrap();

    assert_eq!(
        idx.ledger().get_request(request_b).unwrap().unwrap().state,
        RequestState::Confirming
    );
    assert_eq!(
        idx.ledger().get_request(request_a).unwrap().unwrap().state,
        RequestState::AwaitingDeposit
    );
}

#[tokio::test]
async fn payment_to_an_unknown_address_is_recorded_unmatched() {
    let (mut ledger, request_id) = ledger_with_reservation(500_000_000);
    let _ = assign_deposit_address(&mut ledger, request_id);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    // Some other, never-assigned scriptPubKey — not the legacy vault
    // script and not any request's derived address.
    chain.push_block(
        "h1",
        Some("h0"),
        vec![direct_tx(
            "t1",
            0,
            5.0,
            "76a914deadbeefdeadbeefdeadbeefdeadbeefdead88ac",
        )],
    );
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    idx.tick().await.unwrap();

    // A payment to a script that is neither the legacy vault script nor
    // any known per-request deposit script isn't a bridge deposit at all
    // (indistinguishable from any other unrelated Goldcoin transaction) —
    // the real request must never be credited for it.
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::AwaitingDeposit
    );
}

#[tokio::test]
async fn exact_requested_amount_to_deposit_address_reaches_confirming() {
    let (mut ledger, request_id) = ledger_with_reservation(500_000_000);
    let script = assign_deposit_address(&mut ledger, request_id);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![direct_tx("t1", 0, 5.0, &script)]);
    let mut idx = Indexer::new(chain, ledger, test_config(3, 10));
    idx.tick().await.unwrap();

    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Confirming);
    assert_eq!(req.gross_amount_atomic, 500_000_000);
}

#[tokio::test]
async fn underpayment_to_deposit_address_routes_to_manual_review() {
    let (mut ledger, request_id) = ledger_with_reservation(500_000_000);
    let script = assign_deposit_address(&mut ledger, request_id);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    // Half the reserved amount — same fail-closed policy the legacy
    // OP_RETURN path already enforces (`amount_mismatch_routes_to_manual_
    // review_and_never_promotes`), unchanged by this feature.
    chain.push_block("h1", Some("h0"), vec![direct_tx("t1", 0, 2.5, &script)]);
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    idx.tick().await.unwrap();

    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
}

#[tokio::test]
async fn overpayment_to_deposit_address_routes_to_manual_review() {
    let (mut ledger, request_id) = ledger_with_reservation(500_000_000);
    let script = assign_deposit_address(&mut ledger, request_id);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    // Double the reserved amount — the exact-amount policy fails closed
    // in both directions, never silently accepting "at least enough".
    chain.push_block("h1", Some("h0"), vec![direct_tx("t1", 0, 10.0, &script)]);
    let mut idx = Indexer::new(chain, ledger, test_config(1, 10));
    idx.tick().await.unwrap();

    let req = idx.ledger().get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
}

#[tokio::test]
async fn restart_and_rescan_of_an_address_based_deposit_is_idempotent() {
    let (mut ledger, request_id) = ledger_with_reservation(500_000_000);
    let script = assign_deposit_address(&mut ledger, request_id);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block("h1", Some("h0"), vec![direct_tx("t1", 0, 5.0, &script)]);
    let mut idx = Indexer::new(chain.clone(), ledger, test_config(3, 10));
    idx.tick().await.unwrap();
    assert_eq!(
        idx.ledger().get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    // Simulate a process restart: a brand-new `Indexer` over the SAME
    // already-populated ledger, re-ticking against a chain that hasn't
    // moved. Forward indexing has nothing new to do (chain tip unchanged),
    // and `record_glc_deposit_observed` is idempotent on (txid, vout) —
    // either way, the state must be unchanged, never double-processed or
    // errored.
    let mut idx2 = Indexer::new(chain, idx.ledger, test_config(3, 10));
    idx2.tick().await.unwrap();
    assert_eq!(
        idx2.ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Confirming
    );
}

#[tokio::test]
async fn legacy_op_return_and_address_based_deposits_coexist_in_the_same_block() {
    let (mut ledger, legacy_request) = ledger_with_reservation(500_000_000);
    let CreateRequestOutcome::Reserved {
        request_id: new_request,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 300_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 300_000_000,
                net_destination_atomic: 300_000_000,
            },
            &[0xAB; 32],
            None,
            100_000,
            0,
        )
        .unwrap()
    else {
        panic!("reservation should succeed")
    };
    let new_script = assign_deposit_address(&mut ledger, new_request);

    let chain = Arc::new(MockRpc::new());
    chain.push_block("h0", None, vec![]);
    chain.push_block(
        "h1",
        Some("h0"),
        vec![
            vault_tx("legacy", 0, 5.0, legacy_request),
            direct_tx("modern", 0, 3.0, &new_script),
        ],
    );
    let mut idx = Indexer::new(chain, ledger, test_config(3, 10));
    idx.tick().await.unwrap();

    assert_eq!(
        idx.ledger()
            .get_request(legacy_request)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Confirming
    );
    assert_eq!(
        idx.ledger()
            .get_request(new_request)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Confirming
    );
}
