//! The Solana-side chain-terminal reconciliation, on the exact shapes of
//! requests 4119 / 4256 (completion landed) and 4105 (release landed),
//! and on every way the proof must refuse.

use std::collections::HashMap;
use std::sync::Mutex;

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

use super::*;
use crate::ledger::{ReconcileOutcome, RequestState};
use crate::solana::accounts::PROGRAM_ID;
use crate::solana::rpc::{SimulationOutcome, SolanaRpc};

const MINT_DECIMALS: u8 = 6;
const REQUIRED_GOLDCOIN_DEPTH: i64 = 6;
/// 50,000 GLC gross, 47,000 net (600 bps) — requests 4119/4256.
const GROSS: u64 = 5_000_000_000_000;
const NET: u64 = 4_700_000_000_000;
const FEE: u64 = 300_000_000_000;
const GROSS_MINT_UNITS: u64 = 50_000_000_000; // 6-dp
const OBLIGATION: u64 = 4001;
const PAYOUT_TXID: [u8; 32] = [0x9a; 32];
const REQUESTER: [u8; 32] = [0x6b; 32];
const GLC_DEST: &str = "DsxPiSHC25QCcWVs7JdNsksapVAM5JhH6G";

struct MockRpc {
    accounts: Mutex<HashMap<Pubkey, Account>>,
    statuses: Mutex<HashMap<Signature, Option<Result<(), String>>>>,
}

impl MockRpc {
    fn new() -> MockRpc {
        let mint = Pubkey::new_from_array([0xaa; 32]);
        let mut accounts = HashMap::new();
        accounts.insert(
            crate::solana::accounts::bridge_config_pda(),
            fake_bridge_config_account(mint),
        );
        accounts.insert(mint, fake_mint_account(MINT_DECIMALS));
        MockRpc {
            accounts: Mutex::new(accounts),
            statuses: Mutex::new(HashMap::new()),
        }
    }
    fn set(&self, k: Pubkey, a: Account) {
        self.accounts.lock().unwrap().insert(k, a);
    }
    fn remove(&self, k: &Pubkey) {
        self.accounts.lock().unwrap().remove(k);
    }
}

impl SolanaRpc for MockRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        Ok(self.accounts.lock().unwrap().get(pubkey).cloned())
    }
    async fn get_multiple_accounts(
        &self,
        _: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        Ok(1)
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        Ok(Hash::new_unique())
    }
    async fn send_transaction(&self, _: &Transaction) -> Result<Signature, SolanaRpcError> {
        panic!("the reconciliation must never send a transaction")
    }
    async fn simulate_transaction(
        &self,
        _: &Transaction,
    ) -> Result<SimulationOutcome, SolanaRpcError> {
        panic!("the reconciliation must never simulate a transaction")
    }
    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        Ok(self
            .statuses
            .lock()
            .unwrap()
            .get(signature)
            .cloned()
            .unwrap_or(None))
    }
    async fn is_blockhash_valid(&self, _: &Hash) -> Result<bool, SolanaRpcError> {
        Ok(true)
    }
}

fn account(data: Vec<u8>, owner: Pubkey) -> Account {
    Account {
        lamports: 1,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

fn fake_mint_account(decimals: u8) -> Account {
    let mut data = vec![0u8; 82];
    data[44] = decimals;
    account(data, spl_token::ID)
}

fn fake_bridge_config_account(reserve_mint: Pubkey) -> Account {
    let mut data = vec![0u8; 8];
    data.push(1);
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    data.push(0);
    data.push(0);
    data.push(0);
    data.push(0);
    data.push(0);
    data.extend_from_slice(reserve_mint.as_ref());
    data.extend_from_slice(spl_token::ID.as_ref());
    data.push(0);
    data.extend_from_slice(&5000u64.to_le_bytes());
    data.extend_from_slice(&3600i64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&10_000_000u64.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&20_000_000u64.to_le_bytes());
    data.extend_from_slice(&3600i64.to_le_bytes());
    account(data, PROGRAM_ID)
}

/// `WithdrawalObligation` bytes — with the payout record a `Completed`
/// obligation carries.
fn obligation_account(
    index: u64,
    amount: u64,
    requester: [u8; 32],
    glc_address: &str,
    status: u8,
    payout_record: Option<([u8; 32], u64)>,
) -> Account {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(&requester);
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address.as_bytes());
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(status);
    v.extend_from_slice(&11u64.to_le_bytes());
    v.push(1);
    v.push(2);
    let mut reserved = [0u8; 48];
    if let Some((txid, height)) = payout_record {
        reserved[..32].copy_from_slice(&txid);
        reserved[32..40].copy_from_slice(&height.to_le_bytes());
    }
    v.extend_from_slice(&reserved);
    account(v, PROGRAM_ID)
}

/// `DepositClaim` bytes (the release replay guard).
fn claim_account(txid: [u8; 32], vout: u32, amount: u64, recipient: Pubkey) -> Account {
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&txid);
    v.extend_from_slice(&vout.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(recipient.as_ref());
    v.extend_from_slice(&0u64.to_le_bytes());
    v.push(1);
    v.extend_from_slice(&446_509_363u64.to_le_bytes());
    v.push(1);
    v.extend_from_slice(&[0u8; 16]);
    account(v, PROGRAM_ID)
}

fn ledger() -> Ledger {
    let mut l = Ledger::open_in_memory().unwrap();
    l.configure_reserve(
        crate::ledger::ReserveDirection::GoldcoinReserve,
        NET,
        0,
        NET,
        NET / 2,
        NET / 4,
        100,
    )
    .unwrap();
    l.conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, reserved_liquidity = ?1,
                pending_obligations = ?1 WHERE direction = 'GoldcoinReserve'",
            [NET as i64],
        )
        .unwrap();
    l.configure_reserve(
        crate::ledger::ReserveDirection::SolanaReserve,
        NET,
        0,
        NET,
        NET / 2,
        NET / 4,
        100,
    )
    .unwrap();
    l.conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, reserved_liquidity = ?1,
                pending_obligations = ?1 WHERE direction = 'SolanaReserve'",
            [NET as i64],
        )
        .unwrap();
    l
}

/// Request 4119's local shape: SolToGlc, DestinationConfirmed, one
/// Confirmed Goldcoin payout row whose txid is the request's
/// destination_txid.
fn seed_4119(ledger: &Ledger) -> i64 {
    let dest_hash = crate::goldcoin::address::base58check_decode(GLC_DEST)
        .unwrap()
        .1;
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, destination_txid, destination_confirmations)
             VALUES ('SolToGlc', 'DestinationConfirmed', ?1, 600, ?2, ?3, ?3, ?4, ?5, 100,
                     'solana', X'01', ?6, 1, 100, ?7, 6)",
            rusqlite::params![
                GROSS as i64,
                FEE as i64,
                NET as i64,
                GLC_DEST.as_bytes(),
                &REQUESTER[..],
                OBLIGATION as i64,
                &PAYOUT_TXID[..],
            ],
        )
        .unwrap();
    let id = ledger.conn_for_tests().last_insert_rowid();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, txid, state, built_at, confirmations, onchain_completion_signature,
                 onchain_completion_submitted_at, completion_submissions)
             VALUES (?1, X'00', ?2, 0, 0, ?3, ?4, 'Confirmed', 100, 328, ?5, 200, 123)",
            rusqlite::params![id, NET as i64, &dest_hash[..], &PAYOUT_TXID[..], &[7u8; 64][..]],
        )
        .unwrap();
    id
}

/// Request 4105's local shape: RhnToSol, DestinationSubmitted with the
/// release signature, (source_txid, source_vout) naming the claim.
fn seed_4105(ledger: &Ledger, recipient: Pubkey, sig: [u8; 64]) -> (i64, [u8; 32], u32) {
    let txid = [0x2c; 32];
    let vout = 3u32;
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index, source_txid,
                 source_vout, source_confirmations, source_finalized_at, destination_txid)
             VALUES ('RhnToSol', 'DestinationSubmitted', ?1, 300, ?2, ?3, ?3, ?4, ?5, 100,
                     'robinhood', ?6, 41, ?7, ?8, 1, 100, ?9)",
            rusqlite::params![
                2_000_000_000_000i64,
                60_000_000_000i64,
                1_940_000_000_000i64,
                recipient.as_ref(),
                &[0x24u8; 20][..],
                &[0xbau8; 20][..],
                &txid[..],
                vout,
                &sig[..],
            ],
        )
        .unwrap();
    let id = ledger.conn_for_tests().last_insert_rowid();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
             VALUES (?1, 'SourceFinalized', 'DestinationSubmitted', 150, NULL, 'system')",
            [id],
        )
        .unwrap();
    (id, txid, vout)
}

async fn prove_for(rpc: &MockRpc, ledger: &mut Ledger, id: i64) -> Report {
    prove(rpc, ledger, REQUIRED_GOLDCOIN_DEPTH, id)
        .await
        .unwrap()
}

fn refusal(r: &Report) -> String {
    match &r.verdict {
        Verdict::Refuse(x) => x.clone(),
        other => panic!("expected a refusal, got {other:?}\n{}", r.render()),
    }
}

fn completed_4119(rpc: &MockRpc) {
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(
            OBLIGATION,
            GROSS_MINT_UNITS,
            REQUESTER,
            GLC_DEST,
            crate::solana::accounts::WITHDRAWAL_STATUS_COMPLETED,
            Some((PAYOUT_TXID, 2_589_769)),
        ),
    );
}

/// The exact 4119 (and 4256) pattern: completion landed, ledger stale.
#[tokio::test]
async fn request_4119_completion_landed_is_reconciled_once_from_the_obligations_payout_record() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    completed_4119(&rpc);

    let report = prove_for(&rpc, &mut ledger, id).await;
    let Verdict::SafeToReconcile(proof) = report.verdict.clone() else {
        panic!("{}", report.render())
    };
    assert_eq!(proof.shape, Shape::CompletionLanded);
    assert_eq!(proof.obligation_index, Some(OBLIGATION));
    assert_eq!(report.chain_state, "Completed");
    assert!(report.render().contains("SAFE_TO_RECONCILE"));
    // Dry run wrote nothing.
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::DestinationConfirmed
    );

    let receipt =
        crate::admin_api::audited_solana_reconcile_request(&mut ledger, &proof, "cli:ops").unwrap();
    assert_eq!(receipt.old_value.as_deref(), Some("DestinationConfirmed"));
    let request = ledger.get_request(id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::Settled);
    assert_eq!(
        ledger.get_goldcoin_payout(id).unwrap().unwrap().state,
        "Completed"
    );
    let log = ledger.state_log(id).unwrap();
    assert_eq!(log.last().unwrap().1, RequestState::Settled);
    assert_eq!(
        log.last().unwrap().3.as_deref(),
        Some(Ledger::CHAIN_TERMINAL_RECONCILIATION_REASON)
    );
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        (balance, reserved, pending),
        (0, 0, 0),
        "normal completion accounting"
    );

    // Second execution: ALREADY_RECONCILED, zero writes.
    let log_len = log.len();
    let again = prove_for(&rpc, &mut ledger, id).await;
    assert_eq!(again.verdict, Verdict::AlreadyReconciled);
    assert_eq!(
        apply(&mut ledger, &proof, "cli:ops", 9_999).unwrap(),
        ReconcileOutcome::AlreadyReconciled
    );
    assert_eq!(ledger.state_log(id).unwrap().len(), log_len);
}

/// 4256 is the same shape with different identifiers — proven as a
/// second, independent row in the same ledger, and the two never cross.
#[tokio::test]
async fn request_4256_pattern_reconciles_independently_of_4119() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let a = seed_4119(&ledger);
    completed_4119(&rpc);
    // 4256: obligation #4121, another payout txid, another requester.
    let txid_b = [0x48u8; 32];
    let requester_b = [0x11u8; 32];
    let dest_b = "EDu9vSdWc9vp72jcx8Cfah4jBeikyz6PSa";
    let dest_hash_b = crate::goldcoin::address::base58check_decode(dest_b)
        .unwrap()
        .1;
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, destination_txid, destination_confirmations)
             VALUES ('SolToGlc', 'DestinationConfirmed', ?1, 600, ?2, ?3, ?3, ?4, ?5, 100,
                     'solana', X'01', 4121, 1, 100, ?6, 6)",
            rusqlite::params![
                GROSS as i64,
                FEE as i64,
                NET as i64,
                dest_b.as_bytes(),
                &requester_b[..],
                &txid_b[..]
            ],
        )
        .unwrap();
    let b = ledger.conn_for_tests().last_insert_rowid();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts (request_id, commitment_hash, payout_atomic, change_atomic,
                fee_atomic, dest_p2pkh_hash, txid, state, built_at, confirmations)
             VALUES (?1, X'00', ?2, 0, 0, ?3, ?4, 'Confirmed', 100, 300)",
            rusqlite::params![b, NET as i64, &dest_hash_b[..], &txid_b[..]],
        )
        .unwrap();
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(4121),
        obligation_account(
            4121,
            GROSS_MINT_UNITS,
            requester_b,
            dest_b,
            2,
            Some((txid_b, 2_590_098)),
        ),
    );
    for id in [a, b] {
        let report = prove_for(&rpc, &mut ledger, id).await;
        let Verdict::SafeToReconcile(proof) = report.verdict.clone() else {
            panic!("{}", report.render())
        };
        assert_eq!(
            apply(&mut ledger, &proof, "cli:ops", 500).unwrap(),
            ReconcileOutcome::Reconciled
        );
    }
    assert_eq!(
        ledger.get_request(a).unwrap().unwrap().state,
        RequestState::Settled
    );
    assert_eq!(
        ledger.get_request(b).unwrap().unwrap().state,
        RequestState::Settled
    );
}

/// The exact 4105 pattern: release finalized, signature aged out of the
/// status cache, claim PDA present with the request's amount and
/// recipient.
#[tokio::test]
async fn request_4105_release_landed_is_reconciled_to_destination_confirmed_once() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let recipient = Pubkey::new_from_array([0xf0; 32]);
    let sig = [0x69u8; 64];
    let (id, txid, vout) = seed_4105(&ledger, recipient, sig);
    // Aged out: the node reports nothing for the signature. Claim present.
    rpc.set(
        crate::solana::accounts::deposit_claim_pda(&txid, vout),
        claim_account(txid, vout, 19_400_000_000, recipient),
    );
    let report = prove_for(&rpc, &mut ledger, id).await;
    let Verdict::SafeToReconcile(proof) = report.verdict.clone() else {
        panic!("{}", report.render())
    };
    assert_eq!(proof.shape, Shape::ReleaseLanded);
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::DestinationSubmitted,
        "dry run wrote nothing"
    );
    let receipt =
        crate::admin_api::audited_solana_reconcile_request(&mut ledger, &proof, "cli:ops").unwrap();
    assert_eq!(receipt.old_value.as_deref(), Some("DestinationSubmitted"));
    let request = ledger.get_request(id).unwrap().unwrap();
    // RhnToSol does not settle on release: the unchanged settler now
    // authorizes executeSettlement for V2 obligation #41.
    assert_eq!(request.state, RequestState::DestinationConfirmed);
    let log = ledger.state_log(id).unwrap();
    assert_eq!(log.last().unwrap().1, RequestState::DestinationConfirmed);
    assert_eq!(
        log.last().unwrap().3.as_deref(),
        Some(Ledger::CHAIN_TERMINAL_RECONCILIATION_REASON)
    );
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(
        (balance, reserved, pending),
        (
            NET - 1_940_000_000_000,
            NET - 1_940_000_000_000,
            NET - 1_940_000_000_000
        )
    );
    // Claim already reconciled: second run is a no-op.
    let log_len = log.len();
    let again = prove_for(&rpc, &mut ledger, id).await;
    assert_eq!(again.verdict, Verdict::AlreadyReconciled);
    assert_eq!(
        apply(&mut ledger, &proof, "cli:ops", 1).unwrap(),
        ReconcileOutcome::AlreadyReconciled
    );
    assert_eq!(ledger.state_log(id).unwrap().len(), log_len);
}

#[tokio::test]
async fn release_with_no_claim_pda_is_refused_as_not_executed() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let recipient = Pubkey::new_from_array([0xf0; 32]);
    let (id, _, _) = seed_4105(&ledger, recipient, [0x69u8; 64]);
    let report = prove_for(&rpc, &mut ledger, id).await;
    assert!(
        refusal(&report).contains("no release claim exists"),
        "{}",
        report.render()
    );
    // A failed signature is refused outright.
    let l2 = Ledger::open_in_memory().unwrap();
    let (id2, txid, vout) = seed_4105(&l2, recipient, [0x70u8; 64]);
    ledger = l2;
    rpc.statuses.lock().unwrap().insert(
        Signature::from([0x70u8; 64]),
        Some(Err("custom program error".into())),
    );
    rpc.set(
        crate::solana::accounts::deposit_claim_pda(&txid, vout),
        claim_account(txid, vout, 19_400_000_000, recipient),
    );
    let report = prove_for(&rpc, &mut ledger, id2).await;
    assert!(
        refusal(&report).contains("FAILED on chain"),
        "{}",
        report.render()
    );
}

#[tokio::test]
async fn release_with_wrong_recipient_or_amount_is_refused() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let recipient = Pubkey::new_from_array([0xf0; 32]);
    let (id, txid, vout) = seed_4105(&ledger, recipient, [0x69u8; 64]);
    rpc.set(
        crate::solana::accounts::deposit_claim_pda(&txid, vout),
        claim_account(
            txid,
            vout,
            19_400_000_000,
            Pubkey::new_from_array([0xf1; 32]),
        ),
    );
    assert!(
        refusal(&prove_for(&rpc, &mut ledger, id).await).contains("not the request's recipient")
    );
    rpc.set(
        crate::solana::accounts::deposit_claim_pda(&txid, vout),
        claim_account(txid, vout, 19_400_000_001, recipient),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("release amount"));
}

#[tokio::test]
async fn wrong_obligation_is_refused() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    // The account at #4001's PDA holds another index.
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(
            4002,
            GROSS_MINT_UNITS,
            REQUESTER,
            GLC_DEST,
            2,
            Some((PAYOUT_TXID, 1)),
        ),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("holds obligation #4002"));
    // Pending obligation: not completed.
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(OBLIGATION, GROSS_MINT_UNITS, REQUESTER, GLC_DEST, 0, None),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("not Completed"));
    // Missing account.
    rpc.remove(&crate::solana::accounts::withdrawal_obligation_pda(
        OBLIGATION,
    ));
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("does not exist"));
}

#[tokio::test]
async fn wrong_recipient_is_refused() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(
            OBLIGATION,
            GROSS_MINT_UNITS,
            REQUESTER,
            "EDu9vSdWc9vp72jcx8Cfah4jBeikyz6PSa",
            2,
            Some((PAYOUT_TXID, 1)),
        ),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("Goldcoin destination"));
    // Wrong requester too.
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(
            OBLIGATION,
            GROSS_MINT_UNITS,
            [0x99; 32],
            GLC_DEST,
            2,
            Some((PAYOUT_TXID, 1)),
        ),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("requester"));
}

#[tokio::test]
async fn wrong_amount_is_refused() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(
            OBLIGATION,
            GROSS_MINT_UNITS - 1,
            REQUESTER,
            GLC_DEST,
            2,
            Some((PAYOUT_TXID, 1)),
        ),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("obligation amount"));
    completed_4119(&rpc);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE goldcoin_payouts SET payout_atomic = payout_atomic - 1 WHERE request_id = ?1",
            [id],
        )
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("payout amount"));
}

#[tokio::test]
async fn wrong_payout_tx_is_refused() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    rpc.set(
        crate::solana::accounts::withdrawal_obligation_pda(OBLIGATION),
        obligation_account(
            OBLIGATION,
            GROSS_MINT_UNITS,
            REQUESTER,
            GLC_DEST,
            2,
            Some(([0x55; 32], 1)),
        ),
    );
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("recorded payout txid"));
}

#[tokio::test]
async fn missing_or_duplicate_payout_is_refused() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    completed_4119(&rpc);
    // Duplicate: another request's payout row carries the same txid.
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                source_contract, source_obligation_index)
             VALUES ('SolToGlc', 'DestinationConfirmed', 1, 0, 0, 1, 1, X'00', 1, 'solana', X'01', 9999)",
            [],
        )
        .unwrap();
    let other = ledger.conn_for_tests().last_insert_rowid();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts (request_id, commitment_hash, payout_atomic, change_atomic,
                fee_atomic, dest_p2pkh_hash, txid, state, built_at, confirmations)
             VALUES (?1, X'00', 1, 0, 0, X'00', ?2, 'Confirmed', 100, 300)",
            rusqlite::params![other, &PAYOUT_TXID[..]],
        )
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("claimed by requests"));
    ledger
        .conn_for_tests()
        .execute(
            "DELETE FROM goldcoin_payouts WHERE request_id = ?1",
            [other],
        )
        .unwrap();
    // Missing.
    ledger
        .conn_for_tests()
        .execute("DELETE FROM goldcoin_payouts WHERE request_id = ?1", [id])
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("no Goldcoin payout row"));
}

#[tokio::test]
async fn refund_or_closure_is_refused_before_any_chain_read() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    completed_4119(&rpc);
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO request_closures (request_id, disposition, reference, note, actor, closed_at,
                from_state, manual_review_disposition)
             VALUES (?1, 'refunded_out_of_band', 'x', 'n', 'a', 1, 'ManualReview', 'normal')",
            [id],
        )
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("request closure"));
    ledger
        .conn_for_tests()
        .execute("DELETE FROM request_closures WHERE request_id = ?1", [id])
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                destination_token_account, reserve_mint, token_program, manual_review_reason, note,
                created_by, state, attestation_epoch, created_at)
             VALUES (?1, ?2, 1, 1, ?3, ?3, ?3, ?3, 'r', 'n', 'a', 'Pending', 0, 1)",
            rusqlite::params![id, OBLIGATION as i64, &[0u8; 32][..]],
        )
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("solana refund"));
}

#[tokio::test]
async fn a_request_of_another_route_or_state_has_no_shape() {
    let rpc = MockRpc::new();
    let mut ledger = ledger();
    let id = seed_4119(&ledger);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'ManualReview' WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await).contains("only DestinationConfirmed"));
    ledger
        .conn_for_tests()
        .execute("UPDATE bridge_requests SET direction = 'GlcToSol', state = 'DestinationSubmitted' WHERE id = ?1", [id])
        .unwrap();
    assert!(refusal(&prove_for(&rpc, &mut ledger, id).await)
        .contains("no Solana-side reconciliation shape"));
    assert!(refusal(&prove_for(&rpc, &mut ledger, 424_242).await).contains("does not exist"));
}
