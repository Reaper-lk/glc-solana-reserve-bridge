use std::collections::HashMap;
use std::sync::Mutex;

use solana_sdk::account::Account;

use super::*;
use crate::ledger::{CreateRequestOutcome, ReserveDirection, SolFoldOutcome};
use crate::solana::rpc::SolanaRpcError;

struct MockRpc {
    accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
}

impl MockRpc {
    fn new() -> Self {
        MockRpc {
            accounts: Mutex::new(HashMap::new()),
        }
    }

    fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
        self.accounts.lock().unwrap().insert(pubkey, data);
    }
}

impl SolanaRpc for MockRpc {
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
        unimplemented!("not exercised by attestation tests")
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn get_latest_blockhash(&self) -> Result<solana_sdk::hash::Hash, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn send_transaction(
        &self,
        _tx: &solana_sdk::transaction::Transaction,
    ) -> Result<Signature, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn is_blockhash_valid(
        &self,
        _blockhash: &solana_sdk::hash::Hash,
    ) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
}

fn fake_attestation_key_set_bytes(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Vec<u8> {
    let mut v = vec![0u8; 8]; // discriminator
    v.extend_from_slice(&epoch.to_le_bytes());
    v.push(threshold);
    v.push(1); // bump
    v.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        v.extend_from_slice(k.as_ref());
    }
    v.extend_from_slice(&[0u8; 32]); // reserved
    v
}

fn fake_bridge_config_bytes(reserve_token_mint: [u8; 32], obligation_count: u64) -> Vec<u8> {
    let mut v = vec![0u8; 8]; // discriminator
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin tag (None) — Borsh variable-length: no payload bytes follow
    v.push(0); // paused
    v.push(0); // release_paused
    v.push(0); // deposit_paused
    v.push(7); // bump
    v.extend_from_slice(&reserve_token_mint);
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3); // reserve_authority_bump
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
    v.extend_from_slice(&100u64.to_le_bytes()); // min_transfer_amount
    v.extend_from_slice(&1_000_000u64.to_le_bytes()); // per_transfer_limit
    v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
    v.extend_from_slice(&2_000_000u64.to_le_bytes()); // rolling_volume_limit
    v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
    v
}

fn fake_withdrawal_obligation_bytes(index: u64, amount: u64, glc_address: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 8]; // discriminator
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(&[5u8; 32]); // requester
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address);
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(0); // status = Pending
    v.extend_from_slice(&11u64.to_le_bytes()); // requested_at_slot
    v.push(1); // protocol_version
    v.push(2); // bump
    v.extend_from_slice(&[0u8; 48]); // reserved
    v
}

fn ledger_with_both_reserves() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
            .unwrap();
    }
    ledger
}

fn finalized_glc_to_sol_request(ledger: &mut Ledger, amount: u64, recipient: [u8; 32]) -> i64 {
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, amount, &recipient, None, 3600, 0)
        .unwrap()
    else {
        panic!("expected capacity to be reserved")
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAAu8; 32], 2, amount, 10, [0u8; 32], 0)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 0).unwrap();
    request_id
}

fn confirmed_sol_to_glc_payout(ledger: &mut Ledger, amount: u64, dest_addr: &str) -> (i64, u64) {
    let utxo = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic: amount + 100_000,
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, "51".to_string())], 1, 0)
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amount, [7u8; 32], dest_addr.as_bytes(), 0)
        .unwrap()
    else {
        panic!("expected the deposit to fold directly to SourceFinalized")
    };

    let plan = crate::goldcoin::payout::PayoutPlan {
        inputs: vec![],
        dest_p2pkh_hash: [0u8; 20],
        payout_atomic: amount,
        change_atomic: 0,
        vault_script_pubkey: vec![],
        fee_atomic: 0,
    };
    ledger
        .record_goldcoin_payout_built(request_id, &plan, [0u8; 32], "00", 0)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, "00", 0)
        .unwrap();
    ledger
        .record_goldcoin_payout_broadcast(request_id, [0xBBu8; 32], 0)
        .unwrap();
    // tip_height 105, confirmations 6 => mined_height = 100.
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 105, 6, 0)
        .unwrap();
    (request_id, 0)
}

#[tokio::test]
async fn independently_attests_a_release_matching_the_golden_layout() {
    let mut ledger = ledger_with_both_reserves();
    let recipient = [9u8; 32];
    let request_id = finalized_glc_to_sol_request(&mut ledger, 500_000, recipient);

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(5, 2, &[signer.pubkey(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );

    let (pubkey, signature, message) =
        independently_attest_release(&signer, &ledger, &rpc, request_id)
            .await
            .unwrap();

    let expected = release_claim_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        5,
        &[0xAAu8; 32],
        2,
        500_000,
        &recipient,
        &[7u8; 32],
    );
    assert_eq!(message, expected);
    assert_eq!(pubkey, signer.pubkey());
    assert!(signature.verify(pubkey.as_ref(), &message));
}

#[tokio::test]
async fn two_independent_signers_re_derive_the_identical_release_message() {
    let mut ledger = ledger_with_both_reserves();
    let request_id = finalized_glc_to_sol_request(&mut ledger, 250_000, [3u8; 32]);
    let rpc = MockRpc::new();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(1, 2, &[Pubkey::new_unique(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([4u8; 32], 0),
    );

    let signer_a = DevAttestationSigner::generate();
    let signer_b = DevAttestationSigner::generate();
    let (pk_a, sig_a, msg_a) = independently_attest_release(&signer_a, &ledger, &rpc, request_id)
        .await
        .unwrap();
    let (pk_b, sig_b, msg_b) = independently_attest_release(&signer_b, &ledger, &rpc, request_id)
        .await
        .unwrap();

    assert_eq!(
        msg_a, msg_b,
        "independent re-derivation must be deterministic"
    );
    assert_ne!(pk_a, pk_b);
    assert!(sig_a.verify(pk_a.as_ref(), &msg_a));
    assert!(sig_b.verify(pk_b.as_ref(), &msg_b));
}

#[tokio::test]
async fn refuses_to_attest_a_release_that_is_not_yet_source_finalized() {
    let mut ledger = ledger_with_both_reserves();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[1u8; 32], None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result = independently_attest_release(&signer, &ledger, &rpc, request_id).await;
    assert!(matches!(
        result,
        Err(AttestationError::NotSourceFinalized(
            _,
            RequestState::AwaitingDeposit
        ))
    ));
}

#[tokio::test]
async fn refuses_to_attest_a_release_for_a_sol_to_glc_request() {
    let mut ledger = ledger_with_both_reserves();
    let (request_id, _) =
        confirmed_sol_to_glc_payout(&mut ledger, 100_000, "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef");
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result = independently_attest_release(&signer, &ledger, &rpc, request_id).await;
    assert!(matches!(
        result,
        Err(AttestationError::WrongDirectionForRelease(_))
    ));
}

#[tokio::test]
async fn refuses_to_attest_a_release_for_a_request_that_does_not_exist() {
    let ledger = ledger_with_both_reserves();
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result = independently_attest_release(&signer, &ledger, &rpc, 999).await;
    assert!(matches!(
        result,
        Err(AttestationError::RequestNotFound(999))
    ));
}

#[tokio::test]
async fn independently_attests_a_completion_matching_the_golden_layout() {
    let mut ledger = ledger_with_both_reserves();
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (request_id, obligation_index) =
        confirmed_sol_to_glc_payout(&mut ledger, 500_000, dest_addr);

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(9, 2, &[signer.pubkey()]),
    );
    rpc.set_account(
        accounts::withdrawal_obligation_pda(obligation_index),
        fake_withdrawal_obligation_bytes(obligation_index, 500_000, dest_addr.as_bytes()),
    );

    let (pubkey, signature, message) =
        independently_attest_completion(&signer, &ledger, &rpc, request_id)
            .await
            .unwrap();

    let dest_commitment: [u8; 32] = Sha256::digest(dest_addr.as_bytes())
        .as_slice()
        .try_into()
        .unwrap();
    let expected = goldcoin_completion_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        9,
        obligation_index,
        &[0xBBu8; 32],
        100,
        500_000,
        &dest_commitment,
    );
    assert_eq!(message, expected);
    assert!(signature.verify(pubkey.as_ref(), &message));
}

#[tokio::test]
async fn refuses_to_attest_completion_before_the_goldcoin_payout_is_confirmed() {
    let mut ledger = ledger_with_both_reserves();
    let utxo = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic: 600_000,
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, "51".to_string())], 1, 0)
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            500_000,
            [7u8; 32],
            b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result = independently_attest_completion(&signer, &ledger, &rpc, request_id).await;
    assert!(matches!(result, Err(AttestationError::PayoutNotFound(_))));
}

#[tokio::test]
async fn refuses_to_attest_completion_when_onchain_amount_disagrees_with_the_recorded_payout() {
    let mut ledger = ledger_with_both_reserves();
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (request_id, obligation_index) =
        confirmed_sol_to_glc_payout(&mut ledger, 500_000, dest_addr);

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(1, 2, &[signer.pubkey()]),
    );
    // On-chain obligation disagrees with the recorded payout amount.
    rpc.set_account(
        accounts::withdrawal_obligation_pda(obligation_index),
        fake_withdrawal_obligation_bytes(obligation_index, 999_999, dest_addr.as_bytes()),
    );

    let result = independently_attest_completion(&signer, &ledger, &rpc, request_id).await;
    assert!(matches!(
        result,
        Err(AttestationError::ObligationAmountMismatch {
            onchain: 999_999,
            recorded: 500_000,
            ..
        })
    ));
}
