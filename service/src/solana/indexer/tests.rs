use std::collections::HashMap;
use std::sync::Mutex;

use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

use super::*;
use crate::ledger::ReserveDirection;

struct MockRpc {
    accounts: Mutex<HashMap<Pubkey, Account>>,
    slot: Mutex<u64>,
}

impl MockRpc {
    fn new() -> Self {
        MockRpc {
            accounts: Mutex::new(HashMap::new()),
            slot: Mutex::new(1),
        }
    }

    fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
        self.accounts.lock().unwrap().insert(
            pubkey,
            Account {
                lamports: 1,
                data,
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    fn set_slot(&self, slot: u64) {
        *self.slot.lock().unwrap() = slot;
    }
}

impl SolanaRpc for MockRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        Ok(self.accounts.lock().unwrap().get(pubkey).cloned())
    }
    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        let map = self.accounts.lock().unwrap();
        Ok(pubkeys.iter().map(|k| map.get(k).cloned()).collect())
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        Ok(*self.slot.lock().unwrap())
    }
    async fn get_latest_blockhash(&self) -> Result<solana_sdk::hash::Hash, SolanaRpcError> {
        unimplemented!("not exercised by indexer tests")
    }
    async fn send_transaction(
        &self,
        _tx: &solana_sdk::transaction::Transaction,
    ) -> Result<solana_sdk::signature::Signature, SolanaRpcError> {
        unimplemented!("not exercised by indexer tests")
    }
    async fn get_signature_status(
        &self,
        _signature: &solana_sdk::signature::Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!("not exercised by indexer tests")
    }
    async fn is_blockhash_valid(
        &self,
        _blockhash: &solana_sdk::hash::Hash,
    ) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised by indexer tests")
    }
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md); `fake_bridge_config`'s `reserve_token_mint` is always
/// `[7u8; 32]`, so any test that reaches obligation processing must
/// register a fake mint account there for `tick`'s live decimals read
/// (docs/20-bridge-fee.md).
const TEST_SOLANA_DECIMALS: u8 = 6;

/// A minimal, real 82-byte `spl_token::state::Mint`-shaped buffer — see
/// the matching helper in `signing::attestation::tests`.
fn fake_mint_bytes(decimals: u8) -> Vec<u8> {
    let mut v = vec![0u8; 82];
    v[44] = decimals;
    v[45] = 1; // is_initialized
    v
}

fn fake_bridge_config(obligation_count: u64) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin tag (None) — Borsh variable-length: no payload bytes follow
    v.push(0); // paused
    v.push(0); // release_paused
    v.push(0); // deposit_paused
    v.push(1); // bump
    v.extend_from_slice(&[7u8; 32]); // reserve_token_mint
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(2); // reserve_authority_bump
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v.extend_from_slice(&100u64.to_le_bytes());
    v.extend_from_slice(&1_000_000u64.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes()); // protected_minimum
    v.extend_from_slice(&10_000_000u64.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v
}

fn fake_obligation(index: u64, amount: u64, glc_address: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(&[5u8; 32]); // requester
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address);
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(0); // status = Pending
    v.extend_from_slice(&11u64.to_le_bytes());
    v.push(1);
    v.push(2);
    v.extend_from_slice(&[0u8; 48]);
    v
}

fn ledger_ready() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    // GoldcoinReserve capacity must cover the canonical (8-decimal) scale
    // of a Solana-native obligation `amount` (6 decimals) once correctly
    // converted (docs/20-bridge-fee.md) — a 500_000 Solana-native deposit
    // widens to 50_000_000 canonical before the fee is even taken, well
    // beyond the pre-fee-round 10_000_000 fixture.
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            100_000_000,
            0,
            50_000_000,
            20_000_000,
            10_000_000,
            0,
        )
        .unwrap();
    ledger
}

#[tokio::test]
async fn no_change_in_obligation_count_is_a_no_op() {
    let rpc = MockRpc::new();
    rpc.set_account(accounts::bridge_config_pda(), fake_bridge_config(0));
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());
    let outcome = idx.tick().await.unwrap();
    assert_eq!(outcome, SolanaTickOutcome::NoNewObligations);
}

#[tokio::test]
async fn new_obligation_folds_directly_to_source_finalized() {
    let rpc = MockRpc::new();
    rpc.set_account(accounts::bridge_config_pda(), fake_bridge_config(1));
    rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_obligation(0, 500_000, b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef"),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());

    let outcome = idx.tick().await.unwrap();
    assert_eq!(outcome, SolanaTickOutcome::Folded { count: 1 });
    let reqs = idx
        .ledger()
        .requests_by_state(
            crate::ledger::Direction::SolToGlc,
            crate::ledger::RequestState::SourceFinalized,
        )
        .unwrap();
    assert_eq!(reqs.len(), 1);
    // 500_000 Solana-native (6 decimals) widens to 50_000_000 canonical
    // gross; the 1% bridge fee (docs/20-bridge-fee.md) is 500_000,
    // leaving 49_500_000 net.
    assert_eq!(reqs[0].gross_amount_atomic, 50_000_000);
    assert_eq!(reqs[0].fee_amount_atomic, 500_000);
    assert_eq!(reqs[0].net_amount_atomic, 49_500_000);
    assert_eq!(idx.ledger().last_synced_obligation_count().unwrap(), 1);
}

#[tokio::test]
async fn tick_is_idempotent_across_a_simulated_restart() {
    let rpc = MockRpc::new();
    rpc.set_account(accounts::bridge_config_pda(), fake_bridge_config(1));
    rpc.set_account(
        accounts::withdrawal_obligation_pda(0),
        fake_obligation(0, 500_000, b"addr"),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());
    idx.tick().await.unwrap();

    // "Restart": run tick again against the same (unchanged) chain state.
    let outcome = idx.tick().await.unwrap();
    assert_eq!(
        outcome,
        SolanaTickOutcome::NoNewObligations,
        "already-synced state must not re-fold"
    );
    let count = idx
        .ledger()
        .requests_by_state(
            crate::ledger::Direction::SolToGlc,
            crate::ledger::RequestState::SourceFinalized,
        )
        .unwrap()
        .len();
    assert_eq!(count, 1, "no duplicate fold");
}

#[tokio::test]
async fn multiple_new_obligations_are_all_folded_in_one_tick() {
    let rpc = MockRpc::new();
    rpc.set_account(accounts::bridge_config_pda(), fake_bridge_config(3));
    for i in 0..3u64 {
        rpc.set_account(
            accounts::withdrawal_obligation_pda(i),
            fake_obligation(i, 100_000, b"addr"),
        );
    }
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());
    let outcome = idx.tick().await.unwrap();
    assert_eq!(outcome, SolanaTickOutcome::Folded { count: 3 });
}

#[tokio::test]
async fn missing_obligation_account_errors_and_does_not_advance_cursor() {
    let rpc = MockRpc::new();
    rpc.set_account(accounts::bridge_config_pda(), fake_bridge_config(1));
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    // Deliberately do NOT set the obligation account — simulates an RPC
    // node lagging behind its own reported finalized state.
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());
    let result = idx.tick().await;
    assert!(matches!(
        result,
        Err(SolanaIndexerError::MissingObligationAccount(0))
    ));
    assert_eq!(
        idx.ledger().last_synced_obligation_count().unwrap(),
        0,
        "cursor must not advance past a gap"
    );
}

#[tokio::test]
async fn obligation_count_going_backward_is_a_hard_error_not_a_no_op() {
    let rpc = MockRpc::new();
    rpc.set_account(accounts::bridge_config_pda(), fake_bridge_config(5));
    rpc.set_slot(1);
    for i in 0..5u64 {
        rpc.set_account(
            accounts::withdrawal_obligation_pda(i),
            fake_obligation(i, 1_000, b"addr"),
        );
    }
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());
    idx.tick().await.unwrap();
    assert_eq!(idx.ledger().last_synced_obligation_count().unwrap(), 5);

    // Simulate a corrupted/rolled-back RPC view reporting fewer obligations.
    let rpc2 = MockRpc::new();
    rpc2.set_account(accounts::bridge_config_pda(), fake_bridge_config(2));
    let mut idx2 = SolanaIndexer::new(rpc2, idx.ledger);
    let result = idx2.tick().await;
    assert!(matches!(
        result,
        Err(SolanaIndexerError::StaleOrInconsistentChainState {
            last_synced: 5,
            observed: 2
        })
    ));
}

#[tokio::test]
async fn uninitialized_bridge_config_is_a_hard_error_not_treated_as_zero_obligations() {
    let rpc = MockRpc::new();
    let mut idx = SolanaIndexer::new(rpc, ledger_ready());
    let result = idx.tick().await;
    assert!(matches!(result, Err(SolanaIndexerError::NotInitialized(_))));
}
