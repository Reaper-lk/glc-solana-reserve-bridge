//! End-to-end tests of the refund pipeline against an in-memory
//! [`SolanaRpc`] and a real (in-memory SQLite) [`Ledger`] — the same
//! mock-layer approach `glc-rebalance-withdraw-solana`'s own tests use,
//! extended with the ledger lifecycle so idempotency, crash recovery,
//! and audit linkage are exercised for real, not asserted by hand.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

use super::*;
use crate::ledger::{AdminAuditFilter, RequestAmounts, RequestState};
use crate::signing::attestation::DevAttestationSigner;
use crate::solana::rpc::{SimulationOutcome, SolanaRpcError};

struct MockRpc {
    accounts: Mutex<HashMap<Pubkey, Account>>,
    statuses: Mutex<HashMap<Signature, Result<(), String>>>,
    invalid_blockhashes: Mutex<HashSet<Hash>>,
    simulate_err: Option<String>,
    /// When true (the default), `send_transaction` immediately marks the
    /// sent signature finalized-successful, so `confirm_transaction`
    /// resolves on its first poll.
    auto_finalize_sends: bool,
    sent: Mutex<Vec<Transaction>>,
    /// How many times `bridge_config` has been read this run.
    bridge_config_reads: Mutex<u32>,
    /// When set, every `get_signature_status` / `is_blockhash_valid`
    /// call fails as a transport error — the "RPC temporarily cannot
    /// determine the transaction's fate" case.
    status_rpc_down: bool,
    /// When set, `get_account` for this exact pubkey fails as a transport
    /// error.
    fail_account_reads_for: Option<Pubkey>,
    /// When true, `send_transaction` fails WITHOUT recording the send —
    /// the broadcast-failed-after-recording case, which is also how the
    /// record-before-send ordering is proven.
    fail_sends: bool,
    /// Swaps `bridge_config` for this account once
    /// `bridge_config_reads` exceeds the given count — lets a test change
    /// live chain state BETWEEN the pipeline's own reads, which is the
    /// only way to isolate the last-instant pre-simulation re-check from
    /// the earlier precondition check.
    swap_bridge_config_after_reads: Option<(u32, Account)>,
}

impl MockRpc {
    fn new(accounts: HashMap<Pubkey, Account>) -> Self {
        MockRpc {
            accounts: Mutex::new(accounts),
            statuses: Mutex::new(HashMap::new()),
            invalid_blockhashes: Mutex::new(HashSet::new()),
            simulate_err: None,
            auto_finalize_sends: true,
            sent: Mutex::new(Vec::new()),
            status_rpc_down: false,
            fail_account_reads_for: None,
            fail_sends: false,
            bridge_config_reads: Mutex::new(0),
            swap_bridge_config_after_reads: None,
        }
    }
    fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

impl SolanaRpc for MockRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        if self.fail_account_reads_for == Some(*pubkey) {
            return Err(SolanaRpcError::Transport("connection refused".into()));
        }
        if *pubkey == accounts::bridge_config_pda() {
            let mut reads = self.bridge_config_reads.lock().unwrap();
            *reads += 1;
            if let Some((after, account)) = &self.swap_bridge_config_after_reads {
                if *reads > *after {
                    return Ok(Some(account.clone()));
                }
            }
        }
        Ok(self.accounts.lock().unwrap().get(pubkey).cloned())
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!("not exercised by refund tests")
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        Ok(1)
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        Ok(Hash::new_unique())
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature, SolanaRpcError> {
        if self.fail_sends {
            return Err(SolanaRpcError::Transport("send failed".into()));
        }
        self.sent.lock().unwrap().push(tx.clone());
        if self.auto_finalize_sends {
            self.statuses
                .lock()
                .unwrap()
                .insert(tx.signatures[0], Ok(()));
        }
        Ok(tx.signatures[0])
    }
    async fn simulate_transaction(
        &self,
        _tx: &Transaction,
    ) -> Result<SimulationOutcome, SolanaRpcError> {
        Ok(SimulationOutcome {
            err: self.simulate_err.clone(),
            logs: vec!["Program log: simulated".to_string()],
            units_consumed: Some(9_999),
        })
    }
    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        if self.status_rpc_down {
            return Err(SolanaRpcError::Transport("connection refused".into()));
        }
        Ok(self.statuses.lock().unwrap().get(signature).cloned())
    }
    async fn is_blockhash_valid(&self, blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        if self.status_rpc_down {
            return Err(SolanaRpcError::Transport("connection refused".into()));
        }
        Ok(!self.invalid_blockhashes.lock().unwrap().contains(blockhash))
    }
}

// ------------------------------------------------------- account fixtures --

fn fake_token_account(mint: Pubkey, owner_program: Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    Account {
        lamports: 1,
        data,
        owner: owner_program,
        executable: false,
        rent_epoch: 0,
    }
}

/// Matches `accounts::decode_mint_basics`'s offsets: decimals at 44.
fn fake_mint_account(decimals: u8, owner_program: Pubkey) -> Account {
    let mut data = vec![0u8; 82];
    data[44] = decimals;
    Account {
        lamports: 1,
        data,
        owner: owner_program,
        executable: false,
        rent_epoch: 0,
    }
}

/// Matches `accounts::decode_bridge_config`'s exact byte offsets.
fn fake_bridge_config_account(
    paused: bool,
    reserve_mint: Pubkey,
    token_program: Pubkey,
    protected_minimum: u64,
) -> Account {
    let mut data = vec![0u8; 8]; // discriminator
    data.push(1); // protocol_version
    data.extend_from_slice(Pubkey::new_unique().as_ref()); // admin
    data.push(0); // pending_admin tag (None)
    data.push(paused as u8);
    data.push(0); // release_paused
    data.push(0); // deposit_paused
    data.push(0); // bump
    data.extend_from_slice(reserve_mint.as_ref());
    data.extend_from_slice(token_program.as_ref());
    data.push(0); // reserve_authority_bump
    data.extend_from_slice(&100u64.to_le_bytes()); // obligation_count
    data.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
    data.extend_from_slice(&1u64.to_le_bytes()); // min_transfer_amount
    data.extend_from_slice(&10_000_000u64.to_le_bytes()); // per_transfer_limit
    data.extend_from_slice(&protected_minimum.to_le_bytes());
    data.extend_from_slice(&20_000_000u64.to_le_bytes()); // rolling_volume_limit
    data.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
    Account {
        lamports: 1,
        data,
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// Matches `accounts::decode_attestation_key_set`'s exact byte offsets.
fn fake_attestation_key_set_account(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Account {
    let mut data = vec![0u8; 8]; // discriminator
    data.extend_from_slice(&epoch.to_le_bytes());
    data.push(threshold);
    data.push(0); // bump
    data.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        data.extend_from_slice(k.as_ref());
    }
    Account {
        lamports: 1,
        data,
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// Matches `accounts::decode_withdrawal_obligation`'s offsets.
fn fake_obligation_account(index: u64, amount: u64, requester: Pubkey, status: u8) -> Account {
    let mut data = vec![0u8; 8]; // discriminator
    data.extend_from_slice(&index.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(requester.as_ref());
    data.extend_from_slice(&[0u8; 64]); // glc_address
    data.push(32); // glc_address_len
    data.push(status);
    data.extend_from_slice(&[0u8; 64]); // slot/version/bump/reserved padding
    Account {
        lamports: 1,
        data,
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

/// Matches `decode_rebalance_withdrawal`'s offsets (state.rs layout).
fn fake_rebalance_withdrawal_account(nonce: u64, amount: u64, destination: Pubkey) -> Account {
    let mut data = vec![0u8; 8]; // discriminator
    data.extend_from_slice(&nonce.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(destination.as_ref());
    data.extend_from_slice(Pubkey::new_unique().as_ref()); // admin
    data.extend_from_slice(&0u64.to_le_bytes()); // attestation_epoch
    data.extend_from_slice(&[0u8; 32]); // version/slot/bump/reserved
    Account {
        lamports: 1,
        data,
        owner: PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    }
}

// ------------------------------------------------------------ the fixture --

const OBLIGATION_INDEX: u64 = 7;
const AMOUNT_NATIVE: u64 = 500_000; // 6-decimal units
const GROSS_CANONICAL: u64 = 50_000_000; // widened by the 2-decimal gap
const MINT_DECIMALS: u8 = 6;

struct Fixture {
    rpc: MockRpc,
    ledger: Ledger,
    request_id: i64,
    signers: Vec<Box<dyn AttestationSigner>>,
    admin: Keypair,
    submitter: Keypair,
    reserve_mint: Pubkey,
    token_program: Pubkey,
    requester: Pubkey,
    destination: Pubkey,
    nonce: u64,
}

fn fee_free(gross: u64) -> RequestAmounts {
    RequestAmounts {
        gross_atomic: gross,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: gross,
        net_destination_atomic: gross,
    }
}

/// A consistent world: a fold-parked SolToGlc request in the ledger whose
/// stored gross/requester/obligation exactly match the on-chain
/// obligation fixture, a paused bridge, 2-of-3 dev attestation signers
/// matching the on-chain key set, and a funded reserve.
fn fixture() -> Fixture {
    let requester_keypair = Keypair::new();
    let requester = requester_keypair.pubkey();
    let reserve_mint = Pubkey::new_unique();
    let token_program = Pubkey::new_unique();

    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::SolanaReserve,
            10_000_000,
            100_000,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            10_000_000,
            100_000,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    ledger
        .set_admission(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            true,
            Some("closing for the refund tests"),
        )
        .unwrap();
    let crate::ledger::SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(
            OBLIGATION_INDEX,
            fee_free(GROSS_CANONICAL),
            requester.to_bytes(),
            &[9u8; 32],
            1_000,
        )
        .unwrap()
    else {
        panic!("expected admission-closed to park the fold")
    };

    let signer_keypairs: Vec<Keypair> = (0..3).map(|_| Keypair::new()).collect();
    let signer_pubkeys: Vec<Pubkey> = signer_keypairs.iter().map(|k| k.pubkey()).collect();
    let signers: Vec<Box<dyn AttestationSigner>> = signer_keypairs
        .into_iter()
        .map(|keypair| Box::new(DevAttestationSigner { keypair }) as Box<dyn AttestationSigner>)
        .collect();

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_token_account =
        accounts::associated_token_address(&reserve_authority, &reserve_mint, &token_program);
    let destination = accounts::associated_token_address(&requester, &reserve_mint, &token_program);

    let mut chain = HashMap::new();
    chain.insert(
        accounts::bridge_config_pda(),
        fake_bridge_config_account(true, reserve_mint, token_program, 100_000),
    );
    chain.insert(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_account(0, 2, &signer_pubkeys),
    );
    chain.insert(
        accounts::withdrawal_obligation_pda(OBLIGATION_INDEX),
        fake_obligation_account(OBLIGATION_INDEX, AMOUNT_NATIVE, requester, 0),
    );
    chain.insert(
        reserve_mint,
        fake_mint_account(MINT_DECIMALS, token_program),
    );
    chain.insert(
        reserve_token_account,
        fake_token_account(reserve_mint, token_program, 10_000_000),
    );
    // The destination ATA deliberately does NOT exist: the execute path
    // must handle a closed/never-created ATA via the idempotent create.

    let nonce = Ledger::solana_refund_nonce(request_id).unwrap();
    Fixture {
        rpc: MockRpc::new(chain),
        ledger,
        request_id,
        signers,
        admin: Keypair::new(),
        submitter: Keypair::new(),
        reserve_mint,
        token_program,
        requester,
        destination,
        nonce,
    }
}

async fn run_execute(f: &mut Fixture) -> Result<RefundExecuteOutcome, String> {
    execute_refund(
        &f.rpc,
        &mut f.ledger,
        &f.signers,
        &f.admin,
        &f.submitter,
        f.request_id,
        "refund test note",
        "cli:test",
        ConfirmPolicy::default(),
    )
    .await
}

// ------------------------------------------------------------------- tests --

#[tokio::test]
async fn dry_run_is_eligible_and_provably_touches_nothing() {
    let f = fixture();
    let report = dry_run_refund(&f.rpc, &f.ledger, f.request_id)
        .await
        .unwrap();
    assert!(report.would_execute, "checks: {:#?}", report.checks);
    let plan = report.plan.as_ref().unwrap();
    assert_eq!(plan.amount_solana_atomic, AMOUNT_NATIVE);
    assert_eq!(plan.requester, f.requester);
    assert_eq!(plan.destination_token_account, f.destination);
    assert_eq!(plan.nonce, f.nonce);
    assert!(!plan.destination_exists);
    // Strict read-only: nothing sent, nothing written.
    assert_eq!(f.rpc.sent_count(), 0);
    assert!(f.ledger.get_solana_refund(f.request_id).unwrap().is_none());
    assert_eq!(
        f.ledger.get_request(f.request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert!(f
        .ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn destination_is_the_program_id_aware_ata_of_the_original_requester() {
    let f = fixture();
    // The Token-2022 correctness property: the ATA derivation is seeded
    // by the CONFIGURED token program (Token-2022 for the real mint) —
    // legacy SPL Token would derive a different address entirely.
    assert_eq!(
        f.destination,
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &f.requester,
            &f.reserve_mint,
            &f.token_program,
        )
    );
    let report = dry_run_refund(&f.rpc, &f.ledger, f.request_id)
        .await
        .unwrap();
    assert_eq!(
        report.plan.unwrap().destination_token_account,
        f.destination
    );
}

#[tokio::test]
async fn execute_creates_exactly_one_correct_transaction_and_settles_the_lifecycle() {
    let mut f = fixture();
    let outcome = run_execute(&mut f).await.unwrap();
    let RefundExecuteOutcome::Confirmed { signature } = outcome else {
        panic!("expected Confirmed, got {outcome:?}")
    };
    assert_eq!(f.rpc.sent_count(), 1, "exactly one transaction");

    let sent = f.rpc.sent.lock().unwrap();
    let tx = &sent[0];
    let msg = &tx.message;
    assert_eq!(msg.instructions.len(), 3);
    // Instruction 0: idempotent ATA create for the requester's ATA.
    let ata_program: Pubkey = spl_associated_token_account::id();
    assert_eq!(
        msg.account_keys[msg.instructions[0].program_id_index as usize],
        ata_program
    );
    // Instruction 1: the ed25519 proof; instruction 2: rebalance_withdraw.
    assert_eq!(
        msg.account_keys[msg.instructions[1].program_id_index as usize],
        solana_sdk::ed25519_program::ID
    );
    assert_eq!(
        msg.account_keys[msg.instructions[2].program_id_index as usize],
        PROGRAM_ID
    );
    // The withdraw instruction's args: nonce, amount, epoch after the
    // 8-byte discriminator.
    let data = &msg.instructions[2].data;
    assert_eq!(&data[8..16], &f.nonce.to_le_bytes());
    assert_eq!(&data[16..24], &AMOUNT_NATIVE.to_le_bytes());
    drop(sent);

    // Lifecycle: request Refunded, refund Confirmed, book debited once.
    let request = f.ledger.get_request(f.request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::Refunded);
    let refund = f.ledger.get_solana_refund(f.request_id).unwrap().unwrap();
    assert_eq!(refund.state, crate::ledger::SolanaRefundState::Confirmed);
    assert_eq!(refund.refund_signature.as_deref(), Some(signature.as_str()));
    let (sol_balance, _, _, _) = f
        .ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(sol_balance, 10_000_000 - AMOUNT_NATIVE);

    // Full audit linkage: begin, broadcast (with the tx signature), and
    // confirm rows all present, all targeting this request.
    let audit = f
        .ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    let actions: Vec<&str> = audit.iter().map(|r| r.action.as_str()).collect();
    assert!(actions.contains(&"refund_begin"), "{actions:?}");
    assert!(actions.contains(&"refund_broadcast"), "{actions:?}");
    assert!(actions.contains(&"refund_confirm"), "{actions:?}");
    for row in &audit {
        assert_eq!(
            row.target.as_deref(),
            Some(f.request_id.to_string().as_str())
        );
        assert_eq!(row.actor, "cli:test");
    }
    let broadcast_row = audit
        .iter()
        .find(|r| r.action == "refund_broadcast")
        .unwrap();
    assert!(
        broadcast_row
            .new_value
            .as_deref()
            .unwrap()
            .contains(&signature),
        "the broadcast audit row must carry the transaction signature"
    );
}

#[tokio::test]
async fn rerun_after_refunded_is_an_idempotent_no_op() {
    let mut f = fixture();
    run_execute(&mut f).await.unwrap();
    assert_eq!(f.rpc.sent_count(), 1);
    let outcome = run_execute(&mut f).await.unwrap();
    let RefundExecuteOutcome::AlreadyRefunded { signature } = outcome else {
        panic!("expected AlreadyRefunded, got {outcome:?}")
    };
    assert!(signature.is_some());
    assert_eq!(f.rpc.sent_count(), 1, "no second transaction, ever");
}

#[tokio::test]
async fn rerun_after_a_landed_but_unrecorded_broadcast_finalizes_without_a_second_tx() {
    let mut f = fixture();
    // Simulate: a previous run recorded + broadcast, the transaction
    // LANDED (nonce PDA exists on-chain), then the process crashed before
    // the confirm write. The signature status cache has since forgotten
    // the transaction (None) — the PDA alone must be enough.
    let plan = build_refund_plan(
        &f.rpc,
        &f.ledger.get_request(f.request_id).unwrap().unwrap(),
    )
    .await
    .unwrap();
    audited_begin_solana_refund(
        &mut f.ledger,
        f.request_id,
        &verified_inputs(&plan),
        "note",
        "cli:test",
    )
    .unwrap();
    let old_sig = Signature::from([7u8; 64]);
    let old_hash = Hash::new_unique();
    f.ledger
        .record_solana_refund_broadcast(
            f.request_id,
            &old_sig.to_string(),
            &old_hash.to_string(),
            0,
            2_000,
        )
        .unwrap();
    f.rpc.accounts.lock().unwrap().insert(
        plan.nonce_pda,
        fake_rebalance_withdrawal_account(plan.nonce, AMOUNT_NATIVE, f.destination),
    );

    let outcome = run_execute(&mut f).await.unwrap();
    let RefundExecuteOutcome::Confirmed { signature } = outcome else {
        panic!("expected Confirmed, got {outcome:?}")
    };
    assert_eq!(signature, old_sig.to_string());
    assert_eq!(f.rpc.sent_count(), 0, "recovery must not send anything");
    assert_eq!(
        f.ledger.get_request(f.request_id).unwrap().unwrap().state,
        RequestState::Refunded
    );
}

#[tokio::test]
async fn rerun_after_a_dead_broadcast_rebuilds_under_the_same_nonce() {
    let mut f = fixture();
    let plan = build_refund_plan(
        &f.rpc,
        &f.ledger.get_request(f.request_id).unwrap().unwrap(),
    )
    .await
    .unwrap();
    audited_begin_solana_refund(
        &mut f.ledger,
        f.request_id,
        &verified_inputs(&plan),
        "note",
        "cli:test",
    )
    .unwrap();
    let dead_sig = Signature::from([8u8; 64]);
    let dead_hash = Hash::new_unique();
    f.ledger
        .record_solana_refund_broadcast(
            f.request_id,
            &dead_sig.to_string(),
            &dead_hash.to_string(),
            0,
            2_000,
        )
        .unwrap();
    // The recorded broadcast is POSITIVELY dead: blockhash expired, no
    // status, no nonce PDA.
    f.rpc.invalid_blockhashes.lock().unwrap().insert(dead_hash);

    let outcome = run_execute(&mut f).await.unwrap();
    let RefundExecuteOutcome::Confirmed { signature } = outcome else {
        panic!("expected Confirmed, got {outcome:?}")
    };
    assert_ne!(signature, dead_sig.to_string());
    assert_eq!(f.rpc.sent_count(), 1, "exactly one rebuild");
    // The rebuild reused the SAME nonce — the on-chain replay guard key.
    let sent = f.rpc.sent.lock().unwrap();
    let data = &sent[0].message.instructions[2].data;
    assert_eq!(&data[8..16], &f.nonce.to_le_bytes());
    drop(sent);
    let refund = f.ledger.get_solana_refund(f.request_id).unwrap().unwrap();
    assert_eq!(refund.refund_signature.as_deref(), Some(signature.as_str()));
}

#[tokio::test]
async fn a_still_landable_broadcast_is_never_raced_by_a_second_transfer() {
    let mut f = fixture();
    let plan = build_refund_plan(
        &f.rpc,
        &f.ledger.get_request(f.request_id).unwrap().unwrap(),
    )
    .await
    .unwrap();
    audited_begin_solana_refund(
        &mut f.ledger,
        f.request_id,
        &verified_inputs(&plan),
        "note",
        "cli:test",
    )
    .unwrap();
    let inflight_sig = Signature::from([9u8; 64]);
    let inflight_hash = Hash::new_unique(); // still valid in the mock
    f.ledger
        .record_solana_refund_broadcast(
            f.request_id,
            &inflight_sig.to_string(),
            &inflight_hash.to_string(),
            0,
            2_000,
        )
        .unwrap();

    // With the recorded blockhash still landable and no status yet, the
    // pipeline must WAIT (bounded), not construct another transfer. The
    // short policy makes the bounded wait return TimedOut quickly.
    let short_policy = ConfirmPolicy {
        deadline: std::time::Duration::from_millis(50),
        poll_interval: std::time::Duration::from_millis(10),
    };
    let err = execute_refund(
        &f.rpc,
        &mut f.ledger,
        &f.signers,
        &f.admin,
        &f.submitter,
        f.request_id,
        "refund test note",
        "cli:test",
        short_policy,
    )
    .await
    .unwrap_err();
    assert!(err.contains("undetermined"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0, "no concurrent second transfer");
}

#[tokio::test]
async fn an_unpaused_bridge_blocks_execute_before_anything_begins() {
    let mut f = fixture();
    f.rpc.accounts.lock().unwrap().insert(
        accounts::bridge_config_pda(),
        fake_bridge_config_account(false, f.reserve_mint, f.token_program, 100_000),
    );
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("paused"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
    assert!(
        f.ledger.get_solana_refund(f.request_id).unwrap().is_none(),
        "nothing must begin while the pause precondition fails"
    );
}

#[tokio::test]
async fn the_pause_is_rechecked_immediately_before_simulation() {
    // Paused when the plan is built and when the first precondition
    // check runs, then UNPAUSED before the pre-simulation re-check: only
    // the last-instant check can catch this, and it must.
    let mut f = fixture();
    let unpaused = fake_bridge_config_account(false, f.reserve_mint, f.token_program, 100_000);
    // Read 1 = build_refund_plan (paused, passes); read 2 = the fresh
    // pre-simulation check inside broadcast_and_confirm (unpaused).
    f.rpc.swap_bridge_config_after_reads = Some((1, unpaused));

    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("paused"), "got: {err}");
    assert!(
        *f.rpc.bridge_config_reads.lock().unwrap() >= 2,
        "the pipeline must re-read bridge_config immediately before simulating"
    );
    assert_eq!(f.rpc.sent_count(), 0, "nothing may broadcast once unpaused");
    // The lifecycle began (audited) but never broadcast — cleanly
    // resumable once the operator re-pauses.
    let refund = f.ledger.get_solana_refund(f.request_id).unwrap().unwrap();
    assert_eq!(refund.state, crate::ledger::SolanaRefundState::Pending);
    assert!(refund.refund_signature.is_none());
}

#[tokio::test]
async fn simulation_failure_blocks_broadcast_and_stays_resumable() {
    let mut f = fixture();
    f.rpc.simulate_err = Some("custom program error: 0x1770".to_string());
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("simulation FAILED"), "got: {err}");
    assert_eq!(
        f.rpc.sent_count(),
        0,
        "a failed simulation never broadcasts"
    );
    // The lifecycle began (audited intent) and is cleanly resumable.
    let refund = f.ledger.get_solana_refund(f.request_id).unwrap().unwrap();
    assert_eq!(refund.state, crate::ledger::SolanaRefundState::Pending);
    assert!(refund.refund_signature.is_none());

    // Fix the cause and rerun: completes with exactly one transaction.
    f.rpc.simulate_err = None;
    let outcome = run_execute(&mut f).await.unwrap();
    assert!(matches!(outcome, RefundExecuteOutcome::Confirmed { .. }));
    assert_eq!(f.rpc.sent_count(), 1);
}

#[tokio::test]
async fn a_wrong_mint_destination_account_fails_closed() {
    let mut f = fixture();
    let wrong_mint = Pubkey::new_unique();
    f.rpc.accounts.lock().unwrap().insert(
        f.destination,
        fake_token_account(wrong_mint, f.token_program, 0),
    );
    let report = dry_run_refund(&f.rpc, &f.ledger, f.request_id)
        .await
        .unwrap();
    let err = report.plan.unwrap_err();
    assert!(err.contains("mint"), "got: {err}");
    assert!(!report.would_execute);
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("mint"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
}

#[tokio::test]
async fn a_wrong_token_program_destination_account_fails_closed() {
    let mut f = fixture();
    let wrong_program = Pubkey::new_unique();
    f.rpc.accounts.lock().unwrap().insert(
        f.destination,
        fake_token_account(f.reserve_mint, wrong_program, 0),
    );
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("program"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
}

#[tokio::test]
async fn an_onchain_completed_obligation_fails_closed() {
    let mut f = fixture();
    f.rpc.accounts.lock().unwrap().insert(
        accounts::withdrawal_obligation_pda(OBLIGATION_INDEX),
        fake_obligation_account(OBLIGATION_INDEX, AMOUNT_NATIVE, f.requester, 2),
    );
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("settlement evidence"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
}

#[tokio::test]
async fn a_mismatched_onchain_amount_fails_closed() {
    let mut f = fixture();
    f.rpc.accounts.lock().unwrap().insert(
        accounts::withdrawal_obligation_pda(OBLIGATION_INDEX),
        fake_obligation_account(OBLIGATION_INDEX, AMOUNT_NATIVE + 1, f.requester, 0),
    );
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("deposited amount"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
}

#[tokio::test]
async fn an_insufficient_onchain_reserve_fails_closed() {
    let mut f = fixture();
    let reserve_token_account = accounts::associated_token_address(
        &accounts::reserve_authority_pda(),
        &f.reserve_mint,
        &f.token_program,
    );
    // amount 500_000 + protected 100_000 > 400_000.
    f.rpc.accounts.lock().unwrap().insert(
        reserve_token_account,
        fake_token_account(f.reserve_mint, f.token_program, 400_000),
    );
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("protected_minimum"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
}

#[tokio::test]
async fn a_nonce_pda_with_no_refund_row_refuses_and_never_transfers() {
    // The database-restored-from-old-backup scenario: chain says this
    // request's refund already happened, the database has no record.
    let mut f = fixture();
    let nonce_pda = accounts::rebalance_withdrawal_pda(f.nonce);
    f.rpc.accounts.lock().unwrap().insert(
        nonce_pda,
        fake_rebalance_withdrawal_account(f.nonce, AMOUNT_NATIVE, f.destination),
    );
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("already exists"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
    assert!(f.ledger.get_solana_refund(f.request_id).unwrap().is_none());
}

#[test]
fn decode_rebalance_withdrawal_roundtrip() {
    let destination = Pubkey::new_unique();
    let account = fake_rebalance_withdrawal_account(42, 1_234, destination);
    let record = decode_rebalance_withdrawal(&account.data).unwrap();
    assert_eq!(record.nonce, 42);
    assert_eq!(record.amount, 1_234);
    assert_eq!(record.destination, destination);
    assert!(decode_rebalance_withdrawal(&[0u8; 4]).is_err());
}

#[tokio::test]
async fn a_dry_run_on_an_already_refunded_request_reports_it_as_terminal() {
    let mut f = fixture();
    run_execute(&mut f).await.unwrap();
    let report = dry_run_refund(&f.rpc, &f.ledger, f.request_id)
        .await
        .unwrap();
    assert!(report.already_refunded);
    assert!(
        report.would_execute,
        "a rerun is a safe no-op, not a refusal"
    );
    assert_eq!(
        report.refund.unwrap().state,
        crate::ledger::SolanaRefundState::Confirmed
    );
    assert_eq!(
        report.request.state,
        RequestState::Refunded,
        "the dry run reports the terminal state without changing it"
    );
    assert_eq!(f.rpc.sent_count(), 1, "the dry run sent nothing further");
}

/// Recovery case D: the database says `RefundBroadcast` and the RPC
/// temporarily cannot determine the transaction's fate. This MUST fail
/// closed — no second transfer may ever be constructed while the
/// recorded one's outcome is unknown.
#[tokio::test]
async fn an_indeterminate_rpc_during_recovery_fails_closed_and_never_rebuilds() {
    let mut f = fixture();
    let plan = build_refund_plan(
        &f.rpc,
        &f.ledger.get_request(f.request_id).unwrap().unwrap(),
    )
    .await
    .unwrap();
    audited_begin_solana_refund(
        &mut f.ledger,
        f.request_id,
        &verified_inputs(&plan),
        "note",
        "cli:test",
    )
    .unwrap();
    let sig = Signature::from([11u8; 64]);
    let hash = Hash::new_unique();
    f.ledger
        .record_solana_refund_broadcast(f.request_id, &sig.to_string(), &hash.to_string(), 0, 2_000)
        .unwrap();

    // The nonce PDA is absent from the mock's account map AND status/
    // blockhash reads fail: the transaction's fate is genuinely unknown.
    f.rpc.status_rpc_down = true;

    let err = run_execute(&mut f).await.unwrap_err();
    assert!(
        err.contains("connection refused") || err.contains("undetermined"),
        "an indeterminate RPC must surface as an error, got: {err}"
    );
    assert_eq!(
        f.rpc.sent_count(),
        0,
        "FAIL CLOSED: no second transfer may be constructed while the recorded refund's \
         outcome is unknown"
    );
    // State is untouched and still resumable once the RPC recovers.
    let refund = f.ledger.get_solana_refund(f.request_id).unwrap().unwrap();
    assert_eq!(refund.state, crate::ledger::SolanaRefundState::Broadcast);
    assert_eq!(
        refund.refund_signature.as_deref(),
        Some(sig.to_string().as_str())
    );
    assert_eq!(
        f.ledger.get_request(f.request_id).unwrap().unwrap().state,
        RequestState::RefundBroadcast
    );
}

/// The same fail-closed rule at the very first recovery read: if the
/// nonce-PDA lookup itself fails, nothing may be concluded or rebuilt.
#[tokio::test]
async fn an_indeterminate_nonce_pda_read_during_recovery_fails_closed() {
    let mut f = fixture();
    let plan = build_refund_plan(
        &f.rpc,
        &f.ledger.get_request(f.request_id).unwrap().unwrap(),
    )
    .await
    .unwrap();
    audited_begin_solana_refund(
        &mut f.ledger,
        f.request_id,
        &verified_inputs(&plan),
        "note",
        "cli:test",
    )
    .unwrap();
    let sig = Signature::from([12u8; 64]);
    let hash = Hash::new_unique();
    f.ledger
        .record_solana_refund_broadcast(f.request_id, &sig.to_string(), &hash.to_string(), 0, 2_000)
        .unwrap();

    f.rpc.fail_account_reads_for = Some(plan.nonce_pda);
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("connection refused"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0, "FAIL CLOSED: nothing rebuilt");
    assert_eq!(
        f.ledger
            .get_solana_refund(f.request_id)
            .unwrap()
            .unwrap()
            .state,
        crate::ledger::SolanaRefundState::Broadcast
    );
}

/// Ordering proof for the one window that matters: the database must
/// already record the broadcast BEFORE the irreversible send happens.
/// Demonstrated by failing the send — if the record is present
/// afterwards, it can only have been committed beforehand. There is
/// therefore no crash point at which the chain has a transfer the
/// database knows nothing about.
#[tokio::test]
async fn the_broadcast_is_recorded_before_the_send_so_no_crash_can_hide_a_transfer() {
    let mut f = fixture();
    f.rpc.fail_sends = true;

    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("after the intent was recorded"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0, "the send did not succeed");

    // The durable record exists anyway — written before the send.
    let refund = f.ledger.get_solana_refund(f.request_id).unwrap().unwrap();
    assert_eq!(refund.state, crate::ledger::SolanaRefundState::Broadcast);
    assert!(refund.refund_signature.is_some());
    assert!(refund.recent_blockhash.is_some());
    assert_eq!(
        f.ledger.get_request(f.request_id).unwrap().unwrap().state,
        RequestState::RefundBroadcast
    );
    // And the audit trail carries it too.
    let audit = f
        .ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    assert!(audit.iter().any(|r| r.action == "refund_broadcast"));
}

/// Item 11 of the production-safety review: a real DRY RUN rendered
/// exactly as `glc-admin refund-manual-review` prints it, against
/// mock-only data. Contacts no production RPC and no production signer:
/// every account comes from this file's own in-memory fixture.
#[tokio::test]
async fn printable_dry_run_against_mock_data_only() {
    let f = fixture();
    let report = dry_run_refund(&f.rpc, &f.ledger, f.request_id)
        .await
        .unwrap();
    let plan = report.plan.as_ref().unwrap();

    println!("Refund review for request {}:", report.request.id);
    println!("  state                     = {:?}", report.request.state);
    println!(
        "  manual review reason      = {}",
        report
            .db_checks
            .manual_review_reason
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  gross deposited (canonical, 8 dec) = {}",
        report.request.gross_amount_atomic
    );
    println!(
        "  original deposit          = WithdrawalObligation #{} at {}",
        plan.obligation_index, plan.obligation_pda
    );
    println!("  original sender (owner)   = {}", plan.requester);
    println!(
        "  refund destination        = {} ({})",
        plan.destination_token_account,
        if plan.destination_exists {
            "exists"
        } else {
            "missing — created idempotently at execute"
        }
    );
    println!("  reserve mint              = {}", plan.reserve_mint);
    println!("  token program             = {}", plan.token_program);
    println!(
        "  refund amount (native, {} dec) = {} — exact gross deposit; no fee applies",
        plan.mint_decimals, plan.amount_solana_atomic
    );
    println!(
        "  refund nonce              = {:#x} (PDA {})",
        plan.nonce, plan.nonce_pda
    );
    println!("  reserve balance (before)  = {}", plan.reserve_balance);
    println!(
        "  reserve balance (after)   = {}",
        plan.reserve_balance
            .saturating_sub(plan.amount_solana_atomic)
    );
    println!("  protected minimum         = {}", plan.protected_minimum);
    println!("  bridge globally paused    = {}", plan.bridge_paused);
    println!(
        "  attestation               = {} of {} keys, epoch {}",
        plan.attestation_threshold,
        plan.attestation_keys.len(),
        plan.attestation_epoch
    );
    println!(
        "  Goldcoin payout exists    = {}",
        !report.db_checks.no_goldcoin_payout
    );
    println!("  prior refund              = none");
    println!("\n  Safety checks:");
    for c in &report.checks {
        println!(
            "    [{}] {}{}",
            if c.ok { "PASS" } else { "FAIL" },
            c.name,
            if c.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", c.detail)
            }
        );
    }
    println!(
        "\n  overall: {}",
        if report.would_execute {
            "ELIGIBLE"
        } else {
            "NOT ELIGIBLE"
        }
    );

    assert!(report.would_execute);
    assert_eq!(f.rpc.sent_count(), 0, "a dry run broadcasts nothing");
}

/// The dry-run verdict must separate "this request is refundable" from
/// "the operator has engaged the pause yet" — the runbook's procedure is
/// dry-run first, then pause, so a not-yet-paused bridge must never make
/// an otherwise-eligible request read as NOT ELIGIBLE.
#[tokio::test]
async fn an_unpaused_bridge_reports_eligible_pending_pause_not_ineligible() {
    let mut f = fixture();
    f.rpc.accounts.lock().unwrap().insert(
        accounts::bridge_config_pda(),
        fake_bridge_config_account(false, f.reserve_mint, f.token_program, 100_000),
    );
    let report = dry_run_refund(&f.rpc, &f.ledger, f.request_id)
        .await
        .unwrap();

    assert!(
        report.eligible_ignoring_pause,
        "the request itself is refundable: {:#?}",
        report.checks
    );
    assert!(!report.pause_engaged);
    assert!(!report.would_execute, "executing now would refuse");
    // Exactly one check is a pause precondition, and it is the only
    // failing one.
    let failing: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.name)
        .collect();
    assert_eq!(failing.len(), 1, "only the pause should fail: {failing:?}");
    assert!(report
        .checks
        .iter()
        .filter(|c| c.is_execute_precondition)
        .all(|c| !c.ok));
    // And execution still genuinely refuses — the report is informational,
    // the enforcement is separate.
    let err = run_execute(&mut f).await.unwrap_err();
    assert!(err.contains("paused"), "got: {err}");
    assert_eq!(f.rpc.sent_count(), 0);
}
