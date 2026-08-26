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
use crate::ledger::{CreateRequestOutcome, ResumeManualReviewOutcome, SolFoldOutcome};
use crate::signing::attestation::DevAttestationSigner;
use crate::signing::goldcoin_vault::DevVaultSigner;

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
    /// What `list_unspent` reports — empty unless a test explicitly seeds
    /// it via `set_unspent`, matching the real chain read the orchestrator's
    /// `tick_vault_utxos` phase now performs every tick.
    unspent: Vec<crate::goldcoin::rpc::ListUnspentEntry>,
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

    fn set_unspent(&self, entries: Vec<crate::goldcoin::rpc::ListUnspentEntry>) {
        self.chain.lock().unwrap().unspent = entries;
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
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<crate::goldcoin::rpc::ListUnspentEntry>, RpcError> {
        Ok(self.chain.lock().unwrap().unspent.clone())
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
    async fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> Result<Vec<crate::goldcoin::rpc::ListUnspentEntry>, RpcError> {
        MockGoldcoinRpc::list_unspent(self, min_conf, addresses).await
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
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        let stored_accounts = self.accounts.lock().unwrap();
        Ok(pubkeys
            .iter()
            .map(|pk| {
                stored_accounts.get(pk).cloned().map(|data| Account {
                    lamports: 1,
                    data,
                    owner: accounts::PROGRAM_ID,
                    executable: false,
                    rent_epoch: 0,
                })
            })
            .collect())
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
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!("not exercised by orchestrator tests")
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
    async fn simulate_transaction(
        &self,
        tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        MockSolanaRpc::simulate_transaction(self, tx).await
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
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin tag (None) — Borsh variable-length: no payload bytes follow
    v.push(0);
    v.push(0);
    v.push(0);
    v.push(7);
    v.extend_from_slice(&reserve_token_mint);
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3);
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v.extend_from_slice(&100u64.to_le_bytes());
    v.extend_from_slice(&1_000_000u64.to_le_bytes());
    v.extend_from_slice(&500u64.to_le_bytes());
    v.extend_from_slice(&2_000_000u64.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md); registered as a fake mint account wherever a test
/// exercises a path that now reads decimals live (release building,
/// Goldcoin payout building).
const TEST_SOLANA_DECIMALS: u8 = 6;

/// A minimal, real 82-byte `spl_token::state::Mint`-shaped buffer — see
/// the matching helper in `signing::attestation::tests`.
fn fake_mint_bytes(decimals: u8) -> Vec<u8> {
    let mut v = vec![0u8; 82];
    v[44] = decimals;
    v[45] = 1; // is_initialized
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

pub(crate) fn attestation_signers() -> Vec<Box<dyn AttestationSigner>> {
    vec![
        Box::new(DevAttestationSigner::generate()),
        Box::new(DevAttestationSigner::generate()),
        Box::new(DevAttestationSigner::generate()),
    ]
}

pub(crate) fn vault_and_signers() -> (MultisigVault, Vec<Box<dyn VaultSigner>>) {
    let signers = vec![
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap();
    let boxed: Vec<Box<dyn VaultSigner>> = signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect();
    (vault, boxed)
}

pub(crate) fn base_config() -> OrchestratorConfig {
    OrchestratorConfig {
        attestation_threshold: 2,
        vault_threshold: 2,
        required_goldcoin_confirmations: 6,
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        reconciliation_tolerance: 0,
        vault_min_confirmations: 1,
        goldcoin_network: Network::Testnet,
        signer_timeout: std::time::Duration::from_secs(5),
    }
}

pub(crate) fn indexer_config() -> IndexerConfig {
    IndexerConfig {
        vault_script_hex: "51".to_string(),
        confirmation_depth: 6,
        max_reorg_depth: 6,
        initial_checkpoint: None,
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
    vault_signers: Vec<Box<dyn VaultSigner>>,
    attestation_signers: Vec<Box<dyn AttestationSigner>>,
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
        0,
    )
}

/// Real gross/fee/net breakdown for a GlcToSol request, matching what
/// `api::create_glc_to_sol_transfer` computes (docs/20-bridge-fee.md) —
/// needed here (not the zero-fee shortcut `ledger::tests` uses) because
/// the orchestrator's own attestation path recomputes and strictly
/// verifies the fee against `amount_conversion::BRIDGE_FEE_BPS`.
fn glc_to_sol_amounts(gross: u64, solana_decimals: u8) -> crate::ledger::RequestAmounts {
    let fb =
        crate::amount_conversion::compute_fee(crate::amount_conversion::CanonicalAtomic(gross))
            .unwrap();
    let net_destination = fb.net.to_solana(solana_decimals).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: net_destination.0,
    }
}

/// Real gross/fee/net breakdown for a SolToGlc obligation, matching what
/// `solana::indexer::tick` computes (docs/20-bridge-fee.md).
fn sol_to_glc_amounts(amount: u64, solana_decimals: u8) -> crate::ledger::RequestAmounts {
    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(solana_decimals)
        .unwrap();
    let fb = crate::amount_conversion::compute_fee(gross_canonical).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
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
            .create_request(
                Direction::GlcToSol,
                glc_to_sol_amounts(500_000, TEST_SOLANA_DECIMALS),
                &recipient,
                None,
                3600,
                0,
            )
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
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
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
    assert!(!orchestrator.goldcoin_indexer_status().is_halted());
    assert_eq!(orchestrator.goldcoin_indexer_status().last_tick_unix(), 10);
    assert_eq!(orchestrator.solana_indexer_status().last_tick_unix(), 10);
    let records = orchestrator.ledger().all_attestation_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request_id, request_id);
    assert_eq!(records[0].action_type, "release");
    assert_eq!(
        records[0].message_hash,
        Sha256::digest(&records[0].canonical_message).to_vec()
    );
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
    // Settled liquidity is the NET destination payout, after the 6% bridge
    // fee (docs/20-bridge-fee.md): 500_000 gross - 30_000 fee = 470_000
    // canonical, /100 to the reserve mint's 6-decimal precision = 4_700.
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::SolanaReserve)
            .unwrap(),
        4_700
    );
}

#[tokio::test]
async fn sol_to_glc_payout_settles_across_three_ticks() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    // `fold_sol_deposit`'s 500_000 is Solana-native (6 decimals); the real
    // Goldcoin payout `rederive_plan` builds converts it to Goldcoin-native
    // canonical and takes the 6% bridge fee (docs/20-bridge-fee.md):
    // 500_000 * 100 = 50_000_000 gross, minus 3_000_000 fee = 47_000_000 net.
    // Fund the vault (and configure the GoldcoinReserve balance/
    // reconciliation fixture) with enough real Goldcoin-atomic headroom to
    // cover that net payout.
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                0,
            )
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
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
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
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
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
    let records = orchestrator.ledger().all_attestation_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].action_type, "completion");
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
    // Settled liquidity is the NET Goldcoin-native destination payout,
    // after the 6% bridge fee (docs/20-bridge-fee.md).
    assert_eq!(
        orchestrator
            .ledger()
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        goldcoin_payout_atomic
    );
}

/// Proves coin selection fails closed, at the real orchestrator-tick level,
/// when the vault has no real spendable UTXO yet — the request must park
/// safely (stay `SourceFinalized`, no `goldcoin_payouts` row, no panic, no
/// stuck tick) and then settle automatically, with no operator action, the
/// very next tick after a real UTXO actually becomes available.
#[tokio::test]
async fn sol_to_glc_payout_parks_safely_and_later_succeeds_once_the_vault_has_mature_funds() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let vault_script_pubkey_hex = vault.script_pubkey_hex();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // Capacity accounting believes this much is backed (matching the
        // real admission-time cached balance) even though no real spendable
        // UTXO exists yet — exactly the gap automatic coin selection must
        // fail closed against rather than fabricate.
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        request_id
    };

    // No `set_unspent` call: the vault genuinely has zero spendable UTXOs.
    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
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
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::clone(&goldcoin_rpc),
        Arc::clone(&solana_rpc),
        vault,
        vault_signers,
        attestation_signers,
    );

    // Tick 1: coin selection has nothing to select from — must fail
    // closed, park the request, and keep the tick itself healthy.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 0);
    assert!(
        !report.errors.is_empty(),
        "insufficient liquidity must be recorded, not silently swallowed"
    );
    assert!(
        orchestrator
            .ledger()
            .get_goldcoin_payout(request_id)
            .unwrap()
            .is_none(),
        "no payout row may exist until a real spendable UTXO is actually available"
    );
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::SourceFinalized,
        "the request must stay parked for automatic retry, never move to a stuck or wrong state"
    );

    // The vault now genuinely receives (and matures) a real UTXO — no
    // operator command of any kind.
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault_script_pubkey_hex,
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);

    // Tick 2: the same still-`SourceFinalized` request is retried
    // automatically and now succeeds.
    let report = orchestrator.tick(20).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
}

/// Proves `glc-admin resume-manual-review` genuinely restores normal
/// processing end to end: a request parked in `ManualReview` by
/// `fold_sol_deposit` (admission closed at the time) is resumed via
/// `Ledger::resume_manual_review_sol_to_glc`, and the real `Orchestrator`
/// tick loop then builds, signs, and broadcasts its Goldcoin payout exactly
/// as it would have for a normal `SourceFinalized` request — with admission
/// left closed throughout, since resuming never re-admits anything new.
#[tokio::test]
async fn resumed_manual_review_request_processes_normally_with_admission_still_closed() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();

        ledger
            .set_admission(
                ReserveDirection::GoldcoinReserve,
                true,
                Some("closing before the deposit arrives"),
            )
            .unwrap();

        let SolFoldOutcome::FoldedManualReview { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!("expected admission-closed to route to ManualReview")
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();

        let outcome = ledger
            .resume_manual_review_sol_to_glc(request_id, "verified, safe to resume", 0)
            .unwrap();
        assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
        assert_eq!(
            ledger.get_request(request_id).unwrap().unwrap().state,
            RequestState::SourceFinalized
        );
        // Admission stays closed — resuming never re-admits anything new.
        assert!(ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());

        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
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
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
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

    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    assert!(orchestrator
        .ledger()
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// Proves the two admission-control requirements end to end through the
/// real `Orchestrator` tick loop (docs/09-runbook.md "Admission control
/// (Solana->Goldcoin)"): (1) a request already `SourceFinalized` BEFORE
/// admission was closed still gets built, signed, and broadcast normally —
/// payout processing has never been gated by either `paused` or
/// `admission_closed`, and closing admission must not change that; (2) a
/// brand-new fold attempted WHILE admission is closed is routed to
/// `ManualReview` instead, even though the reserve is not paused and has
/// ample capacity.
#[tokio::test]
async fn admission_closed_blocks_new_folds_but_never_already_accepted_processing() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: vault.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
            .unwrap();

        // Accepted BEFORE admission is closed.
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();

        // Close admission — the exact operator action this feature adds.
        ledger
            .set_admission(
                ReserveDirection::GoldcoinReserve,
                true,
                Some("draining backlog, not accepting new transfers"),
            )
            .unwrap();

        // A brand-new obligation observed WHILE admission is closed must
        // be refused (routed to ManualReview), even though the reserve
        // is not paused and there is no capacity shortfall.
        assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
        let new_outcome = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(1_000, TEST_SOLANA_DECIMALS),
                [2u8; 32],
                dest_addr.as_bytes(),
                0,
            )
            .unwrap();
        assert!(
            matches!(new_outcome, SolFoldOutcome::FoldedManualReview { .. }),
            "expected a new fold while admission is closed to land in ManualReview, got {new_outcome:?}"
        );

        request_id
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
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
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
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

    // The already-accepted request still gets built, signed, and broadcast
    // this tick — payout processing is unaffected by admission being
    // closed.
    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");

    // Admission remains closed — nothing in the tick loop reopens it.
    assert!(orchestrator
        .ledger()
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// Step 4 end-to-end: a SolToGlc payout is built, signed, and broadcast by
/// the real `Orchestrator` tick loop spending a UTXO that lives at a
/// per-request DERIVED deposit address (never the legacy static vault) —
/// exercising the full production wiring (`Ledger::available_vault_utxos`,
/// `signing::goldcoin_vault::rederive_plan`'s per-input resolution, and
/// `Orchestrator::build_and_broadcast_payout`'s per-input assemble loop),
/// not just the signing module in isolation.
#[tokio::test]
async fn watched_goldcoin_addresses_includes_the_root_vault_and_every_derived_deposit_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
        for _ in 0..2 {
            let CreateRequestOutcome::Reserved { request_id } = ledger
                .create_request(
                    Direction::GlcToSol,
                    crate::ledger::RequestAmounts {
                        gross_atomic: 1,
                        fee_bps: 0,
                        fee_atomic: 0,
                        net_atomic: 1,
                        net_destination_atomic: 1,
                    },
                    &[0xABu8; 32],
                    None,
                    100_000,
                    0,
                )
                .unwrap()
            else {
                panic!("reservation should succeed")
            };
            let derived = crate::goldcoin::derivation::derive_request_vault(
                &vault,
                request_id,
                Network::Testnet,
            )
            .unwrap();
            ledger
                .set_glc_to_sol_deposit_address(
                    request_id,
                    derived.address(),
                    &derived.script_pubkey_hex(),
                    &derived.redeem_script_hex(),
                )
                .unwrap();
        }
    }

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    let orchestrator = build_orchestrator(
        &db_path,
        goldcoin_rpc,
        solana_rpc,
        vault.clone(),
        vault_signers,
        attestation_signers(),
    );

    let addresses = orchestrator.watched_goldcoin_addresses().unwrap();
    assert_eq!(
        addresses.len(),
        3,
        "the root vault plus both derived deposit addresses: {addresses:?}"
    );
    assert!(addresses.contains(&vault.address().to_string()));
    let all_deposit_addresses = orchestrator
        .ledger()
        .all_glc_to_sol_deposit_addresses()
        .unwrap();
    assert_eq!(all_deposit_addresses.len(), 2);
    for addr in &all_deposit_addresses {
        assert!(addresses.contains(addr));
    }
}

#[tokio::test]
async fn sol_to_glc_payout_spends_a_derived_address_utxo_end_to_end() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, vault_signers) = vault_and_signers();
    let mint = [7u8; 32];
    let goldcoin_payout_atomic = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let utxo_amount = goldcoin_payout_atomic + 100_000;

    let (request_id, derived_script_pubkey_hex) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                utxo_amount,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();

        // An ordinary GlcToSol reservation gets a unique derived deposit
        // address — exactly what `api::BridgeApi::create_glc_to_sol_
        // transfer` does in production — and its address receives the
        // UTXO the payout below will spend. This is a DIFFERENT request
        // from (and, realistically, a different direction than) the
        // SolToGlc `request_id` whose payout is being settled.
        let CreateRequestOutcome::Reserved {
            request_id: funding_request_id,
        } = ledger
            .create_request(
                Direction::GlcToSol,
                crate::ledger::RequestAmounts {
                    gross_atomic: 1,
                    fee_bps: 0,
                    fee_atomic: 0,
                    net_atomic: 1,
                    net_destination_atomic: 1,
                },
                &[0xABu8; 32],
                None,
                100_000,
                0,
            )
            .unwrap()
        else {
            panic!("reservation should succeed")
        };
        let derived = crate::goldcoin::derivation::derive_request_vault(
            &vault,
            funding_request_id,
            Network::Testnet,
        )
        .unwrap();
        ledger
            .set_glc_to_sol_deposit_address(
                funding_request_id,
                derived.address(),
                &derived.script_pubkey_hex(),
                &derived.redeem_script_hex(),
            )
            .unwrap();

        let utxo = VaultUtxo {
            txid: [0xCCu8; 32],
            vout: 0,
            amount_atomic: utxo_amount,
            script_pubkey_hex: derived.script_pubkey_hex(),
        };
        ledger
            .sync_vault_utxos(&[(utxo, 10, derived.script_pubkey_hex())], 1, 0)
            .unwrap();

        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000, TEST_SOLANA_DECIMALS),
                [1u8; 32],
                dest_addr.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .goldcoin_ingest_block(100, [1u8; 32], [0u8; 32], 1000, 0)
            .unwrap();
        (request_id, derived.script_pubkey_hex())
    };

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    goldcoin_rpc.set_known_tip(100, crate::goldcoin::hex::encode(&[1u8; 32]));
    // The mocked node reports this UTXO at the DERIVED address's
    // scriptPubKey — never the legacy vault's.
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: derived_script_pubkey_hex,
        amount: utxo_amount as f64 / 100_000_000.0,
        confirmations: 10,
        solvable: true,
    }]);
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
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
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

    let report = orchestrator.tick(10).await;
    assert_eq!(report.payouts_built, 1, "errors: {:?}", report.errors);
    let payout = orchestrator
        .ledger()
        .get_goldcoin_payout(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
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
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
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
    let reconciliation = report.solana_reconciliation.unwrap().unwrap();
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

#[tokio::test]
async fn reconciliation_breach_pauses_the_goldcoin_reserve_without_aborting_the_tick() {
    // Regression coverage for the gap found in the post-Phase-6 audit:
    // `tick_reconciliation` used to cover SolanaReserve only, so a
    // discrepancy between the vault's real Goldcoin balance and the
    // ledger's bookkeeping would never trigger a pause. GoldcoinReserve
    // must get exactly the same automatic breach detection as
    // SolanaReserve — this mirrors
    // `reconciliation_breach_pauses_the_solana_reserve_without_aborting_the_tick`
    // with the two directions' roles swapped, keeping the Solana side
    // healthy so this test isolates the Goldcoin path.
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
    }
    // GoldcoinReserve's configured total_reserve_balance is 10_000_000
    // (see configure_both_reserves); the mock's `list_unspent` reports no
    // UTXOs at all by default, an unexplained drop to 0.

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    // Solana side matches its configured balance exactly, so only the
    // Goldcoin side is under test here.
    solana_rpc.set_account(ata, fake_token_account_bytes(10_000_000));

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
    let solana_reconciliation = report.solana_reconciliation.unwrap().unwrap();
    assert_eq!(
        solana_reconciliation.classification,
        crate::reconciliation::Classification::WithinTolerance,
        "Solana side must stay healthy so this test isolates the Goldcoin path"
    );
    // Caught by the EARLIER, admission-gating reconciliation pass
    // (`Orchestrator::tick`'s own comment explains why it runs first) —
    // by the time the pre-existing end-of-tick pass below runs again, its
    // own "before" baseline has already been refreshed to match the still
    // point at 0.0 GLC (with this test's protected_minimum=0 and no
    // pending obligations, the hard invariant trivially holds too, so
    // that second pass reports WithinTolerance on its own turn — expected,
    // not a bug: the breach was already found and paused, and nothing
    // silently un-pauses it (reconciliation never auto-unpauses).
    let goldcoin_pre_admission_reconciliation = report
        .goldcoin_pre_admission_reconciliation
        .unwrap()
        .unwrap();
    assert_eq!(
        goldcoin_pre_admission_reconciliation.classification,
        crate::reconciliation::Classification::Breach
    );
    assert!(goldcoin_pre_admission_reconciliation.auto_paused);
    assert!(orchestrator
        .ledger()
        .is_paused(ReserveDirection::GoldcoinReserve)
        .unwrap());
    assert!(
        !orchestrator
            .ledger()
            .is_paused(ReserveDirection::SolanaReserve)
            .unwrap(),
        "a Goldcoin-side breach must not pause the unrelated Solana direction"
    );
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

/// Production-shaped regression for the admission-freshness incident:
/// several near-10,000-GLC-gross SolToGlc obligations arrive together
/// (all newly observed in the same `solana_indexer.tick()` batch), against
/// a `GoldcoinReserve` whose CACHED `total_reserve_balance` is stale
/// relative to the live, freshly-read mature balance (69,942.41205717
/// GLC — the exact production figure from the incident this fixes) by
/// exactly the value of an already-`Broadcast` (not yet settled) Goldcoin
/// payout — the same mechanism the real incident involved: a payout's own
/// change output goes immature the moment it broadcasts, mechanically
/// dropping observed mature balance by more than the cached figure yet
/// reflects. That gap is fully explained by
/// `pending_destination_settlement_amount`'s in-flight accounting
/// (`Classification::InFlightExplained`, not a breach), so the pre-
/// admission pass refreshes the balance and moves on without pausing
/// anything — protected_minimum is 20,000 GLC, so:
///
///   - against the STALE cached balance (89,942.41205717 GLC), available
///     looks like 69,942.41205717 GLC — comfortably enough for all six
///     9,400 GLC net obligations (6 * 9,400 = 56,400 <=
///     69,942.41205717): every one would have been admitted.
///   - against the FRESH balance this fix surfaces before admission runs
///     (69,942.41205717 GLC), available is only 49,942.41205717 GLC —
///     enough for exactly five (5 * 9,400 = 47,000 <= 49,942.41205717)
///     but not six (6 * 9,400 = 56,400 > 49,942.41205717).
///
/// Desired behavior per the incident follow-up: park/reject only the one
/// request that would cross the floor, never admit it and then discover
/// the breach only after the fact via auto-pause of the whole direction.
#[tokio::test]
async fn several_near_10k_requests_arriving_together_park_only_the_one_that_does_not_fit() {
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");

    const PROTECTED_MINIMUM: u64 = 20_000 * 100_000_000;
    // The exact production figure from the incident this fixes.
    const FRESH_LIVE_BALANCE: u64 = 6_994_241_205_717; // 69,942.41205717 GLC
                                                       // An already-broadcast payout's full input value (never settled yet),
                                                       // explaining the entire gap between the stale cache and live reality.
    const IN_FLIGHT_BROADCAST_VALUE: u64 = 20_000 * 100_000_000;
    const STALE_CACHED_BALANCE: u64 = FRESH_LIVE_BALANCE + IN_FLIGHT_BROADCAST_VALUE;
    const NUM_REQUESTS: u64 = 6;
    // 10,000 GLC gross (Solana-native, 6 decimals) -> 9,400 GLC net after
    // the 6% bridge fee.
    const GROSS_SOLANA_ATOMIC: u64 = 10_000_000_000;

    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                STALE_CACHED_BALANCE,
                PROTECTED_MINIMUM,
                90_000 * 100_000_000,
                50_000 * 100_000_000,
                30_000 * 100_000_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();

        // A phantom, already-settling SolToGlc request (id 1) whose
        // Goldcoin payout has already broadcast but not yet settled —
        // the real-world cause of the stale-vs-live gap above. Inserted
        // directly: this is a fixed, pre-existing fact this test starts
        // from, not something exercised through the normal build/sign/
        // broadcast pipeline.
        let conn = ledger.raw();
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at)
             VALUES (1, 'SolToGlc', 'SettlementAuthorized', ?1, X'AA', 0)",
            [IN_FLIGHT_BROADCAST_VALUE as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, broadcast_at)
             VALUES (1, X'AB', ?1, 0, 0, X'CD', 'Broadcast', 0, 0)",
            [IN_FLIGHT_BROADCAST_VALUE as i64],
        )
        .unwrap();
    }

    let goldcoin_rpc = Arc::new(MockGoldcoinRpc::new());
    let (vault, _vault_signers) = vault_and_signers();
    goldcoin_rpc.set_unspent(vec![crate::goldcoin::rpc::ListUnspentEntry {
        txid: crate::goldcoin::hex::encode(&[0xCCu8; 32]),
        vout: 0,
        script_pub_key: vault.script_pubkey_hex(),
        amount: FRESH_LIVE_BALANCE as f64 / 100_000_000.0,
        confirmations: 20,
        solvable: true,
    }]);

    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, NUM_REQUESTS),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    solana_rpc.set_account(ata, fake_token_account_bytes(10_000_000));
    solana_rpc.set_account(
        Pubkey::new_from_array(mint),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    for i in 0..NUM_REQUESTS {
        solana_rpc.set_account(
            accounts::withdrawal_obligation_pda(i),
            fake_withdrawal_obligation_bytes(i, GROSS_SOLANA_ATOMIC, dest_addr.as_bytes()),
        );
    }

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

    // The pre-admission pass sees a real drop (stale cache vs. live
    // reality) but it's fully explained by the phantom request's
    // already-broadcast payout, so it refreshes total_reserve_balance to
    // the fresh, lower figure — which is what admission then sees — and
    // reports InFlightExplained, not a breach.
    let pre_admission = report
        .goldcoin_pre_admission_reconciliation
        .unwrap()
        .unwrap();
    assert_eq!(
        pre_admission.classification,
        crate::reconciliation::Classification::InFlightExplained
    );

    // The six real obligations folded this tick get ids 2..=7 (id 1 is the
    // phantom inserted above).
    let mut finalized = 0u32;
    let mut manual_review = 0u32;
    for i in 2..=(NUM_REQUESTS as i64 + 1) {
        match orchestrator.ledger().get_request(i).unwrap().unwrap().state {
            RequestState::SourceFinalized => finalized += 1,
            RequestState::ManualReview => manual_review += 1,
            other => panic!("request {i}: unexpected state {other:?}"),
        }
    }
    assert_eq!(
        finalized, 5,
        "exactly five of the six should fit against the fresh balance"
    );
    assert_eq!(
        manual_review, 1,
        "exactly one should be parked, not silently admitted"
    );

    // The whole direction must NOT be paused — only the one oversized
    // request was parked, matching "park/reject only that new request
    // instead of admitting it and then auto-pausing the whole direction."
    assert!(
        !orchestrator
            .ledger()
            .is_paused(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "admitting only what fits must never itself trigger an auto-pause"
    );

    // The invariant genuinely holds for what was actually admitted.
    orchestrator
        .ledger()
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        orchestrator
            .ledger()
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap()
            .2, // reserved_liquidity
        5 * 940_000_000_000,
        "five admissions at 9,400 GLC net each"
    );
}

#[tokio::test]
async fn goldcoin_reconciliation_pause_survives_a_simulated_crash_and_restart() {
    // Crash/restart companion to the breach test above: a pause set by one
    // orchestrator instance must still be in force after that instance is
    // dropped (simulated crash) and a fresh one is rebuilt against the
    // same on-disk ledger — pauses are never auto-cleared, including
    // across a restart (docs/09-runbook.md).
    let mint = [7u8; 32];
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_both_reserves(&mut ledger);
    }

    let solana_rpc = Arc::new(MockSolanaRpc::new());
    solana_rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes(mint, 0),
    );
    let reserve_authority = accounts::reserve_authority_pda();
    let ata = accounts::associated_token_address(
        &reserve_authority,
        &Pubkey::new_from_array(mint),
        &spl_token::ID,
    );
    solana_rpc.set_account(ata, fake_token_account_bytes(10_000_000));

    // A fresh vault/signer set per orchestrator instance is fine here:
    // this test only exercises reconciliation/pause bookkeeping, never
    // vault-signed payout construction, so the two instances do not need
    // to share the same vault identity.
    let (vault1, vault_signers1) = vault_and_signers();
    let mut orchestrator = build_orchestrator(
        &db_path,
        Arc::new(MockGoldcoinRpc::new()), // empty list_unspent -> unexplained drop
        Arc::clone(&solana_rpc),
        vault1,
        vault_signers1,
        attestation_signers(),
    );
    orchestrator.tick(10).await;
    assert!(orchestrator
        .ledger()
        .is_paused(ReserveDirection::GoldcoinReserve)
        .unwrap());
    drop(orchestrator); // simulated crash

    let (vault2, vault_signers2) = vault_and_signers();
    let mut restarted = build_orchestrator(
        &db_path,
        Arc::new(MockGoldcoinRpc::new()),
        solana_rpc,
        vault2,
        vault_signers2,
        attestation_signers(),
    );
    assert!(
        restarted
            .ledger()
            .is_paused(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "pause must survive a restart"
    );
    let outcome = Ledger::open(&db_path)
        .unwrap()
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 1_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 1_000,
                net_destination_atomic: 1_000,
            },
            &[1u8; 32],
            None,
            3600,
            20,
        )
        .unwrap();
    assert_eq!(
        outcome,
        CreateRequestOutcome::Paused,
        "a restarted orchestrator must still fail closed on the paused direction, \
         never silently resuming settlement"
    );

    // A further tick observes the same (still-zero) balance it just cached,
    // so this tick's own delta is now zero — no *new* breach — but the
    // pause from the first tick is never lifted regardless: reconciliation
    // has no code path that clears a pause, only operator action does.
    let report = restarted.tick(20).await;
    assert_eq!(
        report
            .goldcoin_reconciliation
            .unwrap()
            .unwrap()
            .classification,
        crate::reconciliation::Classification::WithinTolerance,
        "the cache converged to the observed balance, so this tick sees no new drop"
    );
    assert!(restarted
        .ledger()
        .is_paused(ReserveDirection::GoldcoinReserve)
        .unwrap());
}
