//! Chain-verification tests for ManualReview -> L1 settlement recovery.
//!
//! The ledger-side rules are already covered by `ledger::tests` (they are
//! the real `resume_manual_review_sol_to_glc`, trialled and rolled back).
//! What is tested here is the one thing this module adds: proving the
//! original on-chain deposit still matches the stored request, and
//! failing closed when it does not or cannot be read.

use std::collections::HashMap;
use std::sync::Mutex;

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

use super::*;
use crate::ledger::{Direction, Ledger, RequestAmounts, RequestState, ReserveDirection};
use crate::solana::accounts::PROGRAM_ID;
use crate::solana::rpc::{SimulationOutcome, SolanaRpcError};

struct MockRpc {
    accounts: HashMap<Pubkey, Account>,
    fail_reads: bool,
    sent: Mutex<Vec<Transaction>>,
}

impl SolanaRpc for MockRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        if self.fail_reads {
            return Err(SolanaRpcError::Transport("connection refused".into()));
        }
        Ok(self.accounts.get(pubkey).cloned())
    }
    async fn get_multiple_accounts(
        &self,
        _p: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!("not exercised")
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        Ok(1)
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        Ok(Hash::new_unique())
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature, SolanaRpcError> {
        // Recovery must never broadcast anything. Recorded so tests can
        // assert emptiness.
        self.sent.lock().unwrap().push(tx.clone());
        Ok(tx.signatures[0])
    }
    async fn simulate_transaction(
        &self,
        _tx: &Transaction,
    ) -> Result<SimulationOutcome, SolanaRpcError> {
        unimplemented!("not exercised")
    }
    async fn get_signature_status(
        &self,
        _s: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!("not exercised")
    }
    async fn is_blockhash_valid(&self, _h: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised")
    }
}

const MINT_DECIMALS: u8 = 6;
const DEPOSIT_NATIVE: u64 = 500_000;
const GROSS_CANONICAL: u64 = 50_000_000; // widened by the 2-decimal gap

fn fake_mint_account(decimals: u8) -> Account {
    let mut data = vec![0u8; 82];
    data[44] = decimals;
    Account {
        lamports: 1,
        data,
        owner: Pubkey::new_unique(),
        executable: false,
        rent_epoch: 0,
    }
}

fn fake_bridge_config_account(reserve_mint: Pubkey) -> Account {
    let mut data = vec![0u8; 8];
    data.push(1); // protocol_version
    data.extend_from_slice(Pubkey::new_unique().as_ref()); // admin
    data.push(0); // pending_admin = None
    data.push(0); // paused
    data.push(0); // release_paused
    data.push(0); // deposit_paused
    data.push(0); // bump
    data.extend_from_slice(reserve_mint.as_ref());
    data.extend_from_slice(Pubkey::new_unique().as_ref()); // token program
    data.push(0); // reserve_authority_bump
    data.extend_from_slice(&10u64.to_le_bytes()); // obligation_count
    data.extend_from_slice(&3600i64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&10_000_000u64.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // protected_minimum
    data.extend_from_slice(&20_000_000u64.to_le_bytes());
    data.extend_from_slice(&3600i64.to_le_bytes());
    Account {
        lamports: 1,
        data,
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

fn fake_obligation_account(index: u64, amount: u64, requester: Pubkey, status: u8) -> Account {
    let mut data = vec![0u8; 8];
    data.extend_from_slice(&index.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(requester.as_ref());
    data.extend_from_slice(&[0u8; 64]); // glc_address
    data.push(32); // glc_address_len
    data.push(status);
    data.extend_from_slice(&[0u8; 64]);
    Account {
        lamports: 1,
        data,
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

fn amounts() -> RequestAmounts {
    RequestAmounts {
        gross_atomic: GROSS_CANONICAL,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: GROSS_CANONICAL,
        net_destination_atomic: GROSS_CANONICAL,
    }
}

/// A parked SolToGlc request plus a chain view that agrees with it.
fn fixture(status: u8, onchain_amount: u64, requester: Pubkey) -> (MockRpc, Ledger, i64) {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for d in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .configure_reserve(
                d,
                1_000_000_000,
                1_000,
                500_000_000,
                200_000_000,
                100_000,
                0,
            )
            .unwrap();
    }
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("park"))
        .unwrap();
    let crate::ledger::SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(), requester.to_bytes(), b"GLCdest1111", 1_000)
        .unwrap()
    else {
        panic!("expected a park")
    };
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopen"))
        .unwrap();

    let reserve_mint = Pubkey::new_unique();
    let mut chain = HashMap::new();
    chain.insert(
        accounts::bridge_config_pda(),
        fake_bridge_config_account(reserve_mint),
    );
    chain.insert(reserve_mint, fake_mint_account(MINT_DECIMALS));
    chain.insert(
        accounts::withdrawal_obligation_pda(0),
        fake_obligation_account(0, onchain_amount, requester, status),
    );
    let rpc = MockRpc {
        accounts: chain,
        fail_reads: false,
        sent: Mutex::new(Vec::new()),
    };
    (rpc, ledger, request_id)
}

#[tokio::test]
async fn a_matching_obligation_verifies_and_the_dry_run_would_settle() {
    let requester = Pubkey::new_unique();
    let (rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE, requester);

    let report = dry_run_settle(&rpc, &mut ledger, id, 5_000).await.unwrap();
    let v = report.chain.as_ref().expect("chain proof");
    assert_eq!(v.requester, requester);
    assert_eq!(v.onchain_amount, DEPOSIT_NATIVE);
    assert_eq!(v.expected_amount, DEPOSIT_NATIVE);
    assert_eq!(v.status, 0);
    assert!(report.would_settle, "ledger verdict: {:?}", report.ledger);

    // Strictly read-only: nothing persisted, nothing broadcast.
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert!(rpc.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_obligation_fails_closed() {
    let requester = Pubkey::new_unique();
    let (mut rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE, requester);
    rpc.accounts.remove(&accounts::withdrawal_obligation_pda(0));

    let report = dry_run_settle(&rpc, &mut ledger, id, 5_000).await.unwrap();
    let err = report.chain.unwrap_err();
    assert!(err.contains("does not exist"), "got: {err}");
    assert!(!report.would_settle);
}

#[tokio::test]
async fn a_non_pending_obligation_fails_closed() {
    let requester = Pubkey::new_unique();
    for status in [1u8, 2u8] {
        let (rpc, mut ledger, id) = fixture(status, DEPOSIT_NATIVE, requester);
        let report = dry_run_settle(&rpc, &mut ledger, id, 5_000).await.unwrap();
        let err = report.chain.unwrap_err();
        assert!(
            err.contains("settlement evidence"),
            "status {status}: {err}"
        );
        assert!(!report.would_settle);
    }
}

#[tokio::test]
async fn a_requester_mismatch_fails_closed() {
    let requester = Pubkey::new_unique();
    let (mut rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE, requester);
    // Chain says a DIFFERENT depositor than the ledger recorded.
    rpc.accounts.insert(
        accounts::withdrawal_obligation_pda(0),
        fake_obligation_account(0, DEPOSIT_NATIVE, Pubkey::new_unique(), 0),
    );
    let report = dry_run_settle(&rpc, &mut ledger, id, 5_000).await.unwrap();
    let err = report.chain.unwrap_err();
    assert!(err.contains("requester"), "got: {err}");
    assert!(!report.would_settle);
}

#[tokio::test]
async fn an_amount_mismatch_fails_closed() {
    let requester = Pubkey::new_unique();
    let (rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE + 1, requester);
    let report = dry_run_settle(&rpc, &mut ledger, id, 5_000).await.unwrap();
    let err = report.chain.unwrap_err();
    assert!(err.contains("deposited amount"), "got: {err}");
    assert!(!report.would_settle);
}

#[tokio::test]
async fn an_unreachable_rpc_fails_closed_and_never_re_admits() {
    let requester = Pubkey::new_unique();
    let (mut rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE, requester);
    rpc.fail_reads = true;

    let report = dry_run_settle(&rpc, &mut ledger, id, 5_000).await.unwrap();
    assert!(report.chain.is_err(), "an unreadable chain must not verify");
    assert!(!report.would_settle);

    // And execute refuses outright rather than proceeding on the ledger
    // checks alone.
    let err = execute_settle(&rpc, &mut ledger, id, "note", "cli:test")
        .await
        .unwrap_err();
    assert!(err.contains("connection refused"), "got: {err}");
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a failed chain proof must leave the request parked"
    );
}

#[tokio::test]
async fn execute_re_admits_into_the_normal_pipeline_and_is_idempotent() {
    let requester = Pubkey::new_unique();
    let (rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE, requester);
    let before = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();

    let outcome = execute_settle(&rpc, &mut ledger, id, "recover for L1", "cli:test")
        .await
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);

    // The request is now exactly what the NORMAL payout pipeline selects.
    let req = ledger.get_request(id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert!(ledger
        .requests_by_state(Direction::SolToGlc, RequestState::SourceFinalized)
        .unwrap()
        .iter()
        .any(|r| r.id == id));

    // Capacity reserved through the same counters normal admission uses.
    let after = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(after.2, before.2 + GROSS_CANONICAL, "reserved_liquidity");
    assert_eq!(after.3, before.3 + GROSS_CANONICAL, "pending_obligations");

    // Audited.
    let audit = ledger
        .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
        .unwrap();
    assert!(audit.iter().any(|a| a.action == "resume_manual_review"));

    // Re-running is a safe no-op — no second reservation.
    let again = execute_settle(&rpc, &mut ledger, id, "again", "cli:test")
        .await
        .unwrap();
    assert!(matches!(
        again,
        ResumeManualReviewOutcome::AlreadyResumed { .. }
    ));
    let after2 = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(after2, after, "a repeat must not reserve again");

    // Nothing was ever broadcast by recovery itself.
    assert!(rpc.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn candidate_listing_excludes_non_whitelisted_and_refunded_requests() {
    let requester = Pubkey::new_unique();
    let (_rpc, mut ledger, id) = fixture(0, DEPOSIT_NATIVE, requester);
    assert_eq!(list_candidates(&ledger).unwrap().len(), 1);

    // A non-whitelisted reason drops out.
    ledger
        .raw()
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'deposit_spent_before_finalized'
             WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(list_candidates(&ledger).unwrap().is_empty());

    // Restore, then start a refund lifecycle — also drops out.
    ledger
        .raw()
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'admission_closed_at_fold'
             WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert_eq!(list_candidates(&ledger).unwrap().len(), 1);
    let request = ledger.get_request(id).unwrap().unwrap();
    let verified = crate::ledger::VerifiedRefundInputs {
        obligation_index: request.source_obligation_index.unwrap(),
        amount_solana_atomic: DEPOSIT_NATIVE,
        gross_canonical_atomic: request.gross_amount_atomic,
        requester: request.requester.unwrap(),
        destination_token_account: [0xDD; 32],
        reserve_mint: [0xEE; 32],
        token_program: [0xFF; 32],
    };
    ledger
        .begin_solana_refund(id, &verified, "refunding instead", "cli:test", 6_000)
        .unwrap();
    assert!(
        list_candidates(&ledger).unwrap().is_empty(),
        "a request in a refund lifecycle is never a recovery candidate"
    );
}
