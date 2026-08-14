use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::signature::Keypair;
use solana_sdk::transaction::Transaction as SolanaTx;

use super::*;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::indexer::IndexerConfig;
use crate::goldcoin::rpc::{BlockHeader, DecodedTransaction, RpcError};
use crate::ledger::{CreateRequestOutcome, SolFoldOutcome};

// -------------------------------------------------------------- mock RPCs --

#[derive(Default)]
struct FakeGoldcoinChain {
    /// Present once a payout tx has been "mined": txid_hex -> confirmations.
    mined: HashMap<String, i64>,
    broadcasts: Vec<String>,
    /// Matches whatever height/hash a test seeded directly into the shared
    /// ledger via `Ledger::goldcoin_ingest_block` (to fake a chain tip for
    /// payout-confirmation polling) — the goldcoin indexer, sharing that
    /// same on-disk ledger, will otherwise try to verify that tip against
    /// this mock via `find_fork_point` and needs to find it here too.
    known_tip: Option<(i64, String)>,
}

struct MockGoldcoinRpc {
    chain: Mutex<FakeGoldcoinChain>,
}

impl MockGoldcoinRpc {
    fn new() -> Self {
        MockGoldcoinRpc {
            chain: Mutex::new(FakeGoldcoinChain::default()),
        }
    }

    fn set_confirmations(&self, txid_hex: &str, confirmations: i64) {
        self.chain
            .lock()
            .unwrap()
            .mined
            .insert(txid_hex.to_string(), confirmations);
    }

    fn set_known_tip(&self, height: i64, hash_hex: String) {
        self.chain.lock().unwrap().known_tip = Some((height, hash_hex));
    }
}

impl GoldcoinRpc for MockGoldcoinRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        Ok(-1) // empty chain: the indexer tick is a harmless no-op
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        match &self.chain.lock().unwrap().known_tip {
            Some((h, hash)) if *h == height => Ok(hash.clone()),
            _ => Err(RpcError::Method {
                code: -8,
                message: "height out of range".into(),
            }),
        }
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("no blocks in this mock chain")
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        let confirmations = self.chain.lock().unwrap().mined.get(txid_hex).copied();
        Ok(DecodedTransaction {
            txid: txid_hex.to_string(),
            vout: Vec::new(),
            confirmations,
        })
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<crate::goldcoin::rpc::TxOut>, RpcError> {
        unimplemented!("not exercised by orchestrator tests")
    }
    async fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> Result<crate::goldcoin::rpc::BroadcastOutcome, RpcError> {
        self.chain.lock().unwrap().broadcasts.push(hex.to_string());
        Ok(crate::goldcoin::rpc::BroadcastOutcome::Accepted {
            txid: "mock".to_string(),
        })
    }
}

impl GoldcoinRpc for Arc<MockGoldcoinRpc> {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        MockGoldcoinRpc::get_block_count(self).await
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        MockGoldcoinRpc::get_block_hash(self, height).await
    }
    async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        MockGoldcoinRpc::get_block(self, hash).await
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        MockGoldcoinRpc::get_raw_transaction(self, txid_hex).await
    }
    async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<crate::goldcoin::rpc::TxOut>, RpcError> {
        MockGoldcoinRpc::get_tx_out_confirmed(self, txid_hex, vout).await
    }
    async fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> Result<crate::goldcoin::rpc::BroadcastOutcome, RpcError> {
        MockGoldcoinRpc::send_raw_transaction(self, hex).await
    }
}

#[derive(Default)]
struct MockSolanaRpc {
    accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
    statuses: Mutex<HashMap<Signature, Result<(), String>>>,
    sent: Mutex<Vec<SolanaTx>>,
}

impl MockSolanaRpc {
    fn new() -> Self {
        MockSolanaRpc::default()
    }

    fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
        self.accounts.lock().unwrap().insert(pubkey, data);
    }

    fn set_status(&self, signature: Signature, status: Result<(), String>) {
        self.statuses.lock().unwrap().insert(signature, status);
    }
}

impl SolanaRpc for MockSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(pubkey)
            .cloned()
            .map(|data| Account {
                lamports: 1,
                data,
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }))
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!("not exercised by orchestrator tests")
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        Ok(1) // the solana indexer's own progress tracking; not asserted on by these tests
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        Ok(Hash::new_unique())
    }
    async fn send_transaction(&self, tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        let signature = tx.signatures[0];
        self.sent.lock().unwrap().push(tx.clone());
        Ok(signature)
    }
    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        Ok(self.statuses.lock().unwrap().get(signature).cloned())
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised by orchestrator tests")
    }
}

impl SolanaRpc for Arc<MockSolanaRpc> {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        MockSolanaRpc::get_account(self, pubkey).await
    }
    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        MockSolanaRpc::get_multiple_accounts(self, pubkeys).await
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        MockSolanaRpc::get_slot(self).await
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        MockSolanaRpc::get_latest_blockhash(self).await
    }
    async fn send_transaction(&self, tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        MockSolanaRpc::send_transaction(self, tx).await
    }
    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        MockSolanaRpc::get_signature_status(self, signature).await
    }
    async fn is_blockhash_valid(&self, blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        MockSolanaRpc::is_blockhash_valid(self, blockhash).await
    }
}

// -------------------------------------------------------------- fixtures --

fn fake_attestation_key_set_bytes(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&epoch.to_le_bytes());
    v.push(threshold);
    v.push(1);
    v.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        v.extend_from_slice(k.as_ref());
    }
    v.extend_from_slice(&[0u8; 32]);
    v
}

fn fake_bridge_config_bytes(reserve_token_mint: [u8; 32], obligation_count: u64) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1);
    v.extend_from_slice(&[0u8; 32]);
    v.push(0);
    v.extend_from_slice(&[0u8; 32]);
    v.push(0);
    v.push(0);
    v.push(0);
    v.push(7);
    v.extend_from_slice(&reserve_token_mint);
    v.push(3);
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v.extend_from_slice(&100u64.to_le_bytes());
    v.extend_from_slice(&1_000_000u64.to_le_bytes());
    v.extend_from_slice(&500u64.to_le_bytes());
    v.extend_from_slice(&2_000_000u64.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v.extend_from_slice(&[0u8; 32]);
    v
}

fn fake_withdrawal_obligation_bytes(index: u64, amount: u64, glc_address: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(&[5u8; 32]);
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address);
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(0);
    v.extend_from_slice(&11u64.to_le_bytes());
    v.push(1);
    v.push(2);
    v.extend_from_slice(&[0u8; 48]);
    v
}

fn fake_token_account_bytes(amount: u64) -> Vec<u8> {
    let mut v = vec![0u8; 72];
    v[64..72].copy_from_slice(&amount.to_le_bytes());
    v
}

fn attestation_signers() -> Vec<DevAttestationSigner> {
    vec![
        DevAttestationSigner::generate(),
        DevAttestationSigner::generate(),
        DevAttestationSigner::generate(),
    ]
}

fn vault_and_signers() -> (MultisigVault, Vec<DevVaultSigner>) {
    let signers = vec![
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(signers.iter().map(|s| s.pubkey).collect(), 2).unwrap();
    (vault, signers)
}

fn base_config() -> OrchestratorConfig {
    OrchestratorConfig {
        attestation_threshold: 2,
        vault_threshold: 2,
        required_goldcoin_confirmations: 6,
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        reconciliation_tolerance: 0,
    }
}

fn indexer_config() -> IndexerConfig {
    IndexerConfig {
        vault_script_hex: "51".to_string(),
        confirmation_depth: 6,
        max_reorg_depth: 6,
    }
}

/// Opens three independent connections onto the SAME database file — the
/// same "concurrent operators" concurrency model this ledger is designed
/// for (WAL mode + `BEGIN IMMEDIATE` transactions is the real safety
/// boundary, not a single shared in-process handle — see
/// `docs/06-schema.md`), used here in-process so the indexers' writes and
/// the orchestrator's own reads/writes are genuinely visible to each
/// other, exactly as they must be across real, separate processes in
/// production. Using `Ledger::open_in_memory` for more than one of these
/// would silently give each component an isolated database and mask
/// exactly this class of wiring bug.
#[allow(clippy::too_many_arguments)]
fn build_orchestrator(
    db_path: &std::path::Path,
    goldcoin_rpc: Arc<MockGoldcoinRpc>,
    solana_rpc: Arc<MockSolanaRpc>,
    vault: MultisigVault,
    vault_signers: Vec<DevVaultSigner>,
    attestation_signers: Vec<DevAttestationSigner>,
) -> Orchestrator<Arc<MockGoldcoinRpc>, Arc<MockSolanaRpc>> {
    let goldcoin_indexer = Indexer::new(
        Arc::clone(&goldcoin_rpc),
        Ledger::open(db_path).unwrap(),
        indexer_config(),
    );
    let solana_indexer =
        SolanaIndexer::new(Arc::clone(&solana_rpc), Ledger::open(db_path).unwrap());
    let ledger = Ledger::open(db_path).unwrap();
    Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        ledger,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers,
        Keypair::new(),
        base_config(),
    )
}

fn configure_both_reserves(ledger: &mut Ledger) {
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
            .unwrap();
    }
}

// ----------------------------------------------------------------- tests --

#[tokio::test]
async fn glc_to_sol_release_settles_across_two_ticks() {
    let mint = [7u8; 32];
    let recipient = [9u8; 32];

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(Direction::GlcToSol, 500_000, &recipient, None, 3600, 0)
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAAu8; 32], 2, 500_000, 10, [0u8; 32], 0)
            .unwrap();
        ledger.mark_glc_source_finalized(request_id, 0).unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            5,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    let report = orchestrator.tick(10).await;
    assert_eq!(report.releases_submitted, 1, "errors: {:?}", report.errors);
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::DestinationSubmitted
    );

    let destination_txid = orchestrator
        .ledger()
        .get_destination_txid(request_id)
        .unwrap()
        .unwrap();
    let signature = Signature::from(<[u8; 64]>::try_from(destination_txid).unwrap());
    solana_rpc.set_status(signature, Ok(()));

    let report = orchestrator.tick(20).await;
    assert_eq!(report.releases_confirmed, 1, "errors: {:?}", report.errors);
    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(req.state, RequestState::Settled);
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::SolanaReserve)
            .unwrap(),
        500_000
    );
}

#[tokio::test]
async fn sol_to_glc_payout_settles_across_three_ticks() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: 600_000,
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(0, 500_000, [1u8; 32], dest_addr.as_bytes(), 0)
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let attestation_signers = attestation_signers();
    solana_rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(
            9,
            2,
            &attestation_signers
                .iter()
                .map(|s| s.pubkey())
                .collect::<Vec<_>>(),
        ),
    );
    solana_rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_withdrawal_obligation_bytes(0, 500_000, dest_addr.as_bytes()),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: build, independently sign, and broadcast the payout.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    let txid = payout.txid.unwrap();

    // Tick 2: the payout mines with enough confirmations, and — within the
    // same sweep — the now-Confirmed payout's completion is independently
    // attested and submitted to Solana.
    goldcoin_rpc.set_confirmations(&crate::goldcoin::hex::encode(&txid), 6);
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_confirmed, 1, "errors: {:?}", report.errors);
    assert_eq!(
        report.completions_submitted, 1,
        "errors: {:?}",
        report.errors
    );
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Confirmed");
    assert_eq!(payout.mined_height, Some(95)); // tip 100 - confirmations 6 + 1
    let completion_sig = Signature::from(payout.onchain_completion_signature.unwrap());
    solana_rpc.set_status(completion_sig, Ok(()));

    // Tick 3: the completion confirms and the request settles.
    let report = orchestrator.tick(30).await;
    assert_eq!(
        report.completions_confirmed, 1,
        "errors: {:?}",
        report.errors
    );
    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(req.state, RequestState::Settled);
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        500_000
    );
}

#[tokio::test]
async fn reconciliation_breach_pauses_the_solana_reserve_without_aborting_the_tick() {
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
    }
    // protected_minimum for SolanaReserve is 1_000_000 (see configure_both_reserves);
    // an observed balance of 0 is a hard invariant breach.

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(&reserve_authority, &Pubkey::new_from_array(mint));
    solana_rpc.set_account(ata, fake_token_account_bytes(0));

    let (vault, vault_signers) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault,
        vault_signers,
        attestation_signers(),
    );

    let report = orchestrator.tick(10).await;
    let reconciliation = report.reconciliation.unwrap().unwrap();
    assert_eq!(
        reconciliation.classification,
        crate::reconciliation::Classification::Breach
    );
    assert!(reconciliation.auto_paused);
    assert!(orchestrator
        .ledger()
        .is_paused(ReserveDirection::SolanaReserve)
        .unwrap());
    // The tick did not abort: reservation expiry still ran (no error recorded for it).
    assert!(
        !report
            .errors
            .iter()
            .any(|e| e.contains("expire_reservations")),
        "a reconciliation breach must not stop the rest of the tick from running: {:?}",
        report.errors
    );
}
