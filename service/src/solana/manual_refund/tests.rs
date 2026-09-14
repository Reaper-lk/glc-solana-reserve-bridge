//! The out-of-band Solana refund: export eligibility (ledger + chain),
//! the transaction shape, landed-transaction verification, and the
//! import's per-request verdicts — against a mock cluster.

use std::collections::HashMap;
use std::sync::Mutex;

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

use super::*;
use crate::ledger::{Ledger, RequestState};
use crate::solana::accounts::{PROGRAM_ID, WITHDRAWAL_STATUS_COMPLETED, WITHDRAWAL_STATUS_PENDING};
use crate::solana::rpc::{SimulationOutcome, SolanaRpc, SolanaRpcError};

const DECIMALS: u8 = 6;
const GROSS: u64 = 5_000_000_000_000; // 50,000 GLC, canonical 8dp
const FEE: u64 = 300_000_000_000;
const NET: u64 = 4_700_000_000_000;
const MINT_UNITS: u64 = 50_000_000_000; // 6dp
const NOW: i64 = 1_789_400_000;

fn mint() -> Pubkey {
    Pubkey::new_from_array([0xaa; 32])
}
fn token_program() -> Pubkey {
    spl_token_2022::ID
}

struct MockRpc {
    accounts: Mutex<HashMap<Pubkey, Account>>,
    /// Stored as JSON: the response type is not `Clone`.
    txs: Mutex<HashMap<Signature, serde_json::Value>>,
}

impl MockRpc {
    fn new() -> MockRpc {
        let mut accounts = HashMap::new();
        accounts.insert(
            accounts::bridge_config_pda(),
            fake_bridge_config_account(mint(), token_program()),
        );
        accounts.insert(mint(), fake_mint_account(DECIMALS, token_program()));
        MockRpc {
            accounts: Mutex::new(accounts),
            txs: Mutex::new(HashMap::new()),
        }
    }
    fn set(&self, k: Pubkey, a: Account) {
        self.accounts.lock().unwrap().insert(k, a);
    }
    fn put_tx(&self, sig: Signature, tx: EncodedConfirmedTransactionWithStatusMeta) {
        self.txs
            .lock()
            .unwrap()
            .insert(sig, serde_json::to_value(tx).unwrap());
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
        panic!("export/import must never send a transaction")
    }
    async fn simulate_transaction(
        &self,
        _: &Transaction,
    ) -> Result<SimulationOutcome, SolanaRpcError> {
        panic!("export/import must never simulate a transaction")
    }
    async fn get_signature_status(
        &self,
        _: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        Ok(None)
    }
    async fn is_blockhash_valid(&self, _: &Hash) -> Result<bool, SolanaRpcError> {
        Ok(true)
    }
}

impl SolanaTransactionLookup for MockRpc {
    async fn get_finalized_transaction(
        &self,
        signature: &Signature,
    ) -> Result<Option<EncodedConfirmedTransactionWithStatusMeta>, SolanaRpcError> {
        Ok(self
            .txs
            .lock()
            .unwrap()
            .get(signature)
            .map(|v| serde_json::from_value(v.clone()).unwrap()))
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

fn fake_mint_account(decimals: u8, owner: Pubkey) -> Account {
    let mut data = vec![0u8; 82];
    data[44] = decimals;
    account(data, owner)
}

fn fake_bridge_config_account(reserve_mint: Pubkey, program: Pubkey) -> Account {
    let mut data = vec![0u8; 8];
    data.push(1);
    data.extend_from_slice(Pubkey::new_unique().as_ref());
    data.push(0);
    data.push(0);
    data.push(0);
    data.push(0);
    data.push(0);
    data.extend_from_slice(reserve_mint.as_ref());
    data.extend_from_slice(program.as_ref());
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

fn obligation_account(index: u64, amount: u64, requester: Pubkey, status: u8) -> Account {
    let glc_address = "DsxPiSHC25QCcWVs7JdNsksapVAM5JhH6G";
    let mut v = vec![0u8; 8];
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(requester.as_ref());
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address.as_bytes());
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(status);
    v.extend_from_slice(&11u64.to_le_bytes());
    v.push(1);
    v.push(2);
    v.extend_from_slice(&[0u8; 48]);
    account(v, PROGRAM_ID)
}

/// Raw SPL token account bytes: mint, owner, amount.
fn token_account(mint: Pubkey, owner: Pubkey, amount: u64, program: Pubkey) -> Account {
    let mut v = Vec::with_capacity(165);
    v.extend_from_slice(mint.as_ref());
    v.extend_from_slice(owner.as_ref());
    v.extend_from_slice(&amount.to_le_bytes());
    v.resize(165, 0);
    account(v, program)
}

fn seed_parked(ledger: &Ledger, direction: &str, requester: Pubkey, obligation: u64) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, manual_review_note)
             VALUES (?1, 'ManualReview', ?2, 600, ?3, ?4, ?4, X'00', ?5, 100,
                     'solana', X'01', ?6, 1, 100, 'liquidity_buffer_low_at_fold')",
            rusqlite::params![
                direction,
                GROSS as i64,
                FEE as i64,
                NET as i64,
                requester.as_ref(),
                obligation as i64
            ],
        )
        .unwrap();
    ledger.conn_for_tests().last_insert_rowid()
}

fn seed_robinhood_parked(ledger: &Ledger) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at,
                 source_chain, source_contract, source_obligation_index, source_confirmations)
             VALUES ('RhnToGlc', 'ManualReview', 2000000000000, 300, 60000000000, 1940000000000,
                     1940000000000, X'00', 100, 'robinhood', ?1, 7, 1)",
            rusqlite::params![&[0xbau8; 20][..]],
        )
        .unwrap();
    ledger.conn_for_tests().last_insert_rowid()
}

/// One eligible request on both ledger and chain.
fn eligible(rpc: &MockRpc, ledger: &Ledger, requester: Pubkey, obligation: u64) -> i64 {
    rpc.set(
        accounts::withdrawal_obligation_pda(obligation),
        obligation_account(obligation, MINT_UNITS, requester, WITHDRAWAL_STATUS_PENDING),
    );
    seed_parked(ledger, "SolToGlc", requester, obligation)
}

// ------------------------------------------------------------- helpers --

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A signed refund transaction for `request_id`, exactly as the sender
/// builds it.
fn signed_refund(wallet: &Keypair, recipient: &Pubkey, amount: u64, memo: &str) -> Transaction {
    let ixs = build_refund_instructions(
        &wallet.pubkey(),
        recipient,
        &mint(),
        &token_program(),
        DECIMALS,
        amount,
        memo,
    )
    .unwrap();
    Transaction::new_signed_with_payer(&ixs, Some(&wallet.pubkey()), &[wallet], Hash::new_unique())
}

fn token_balance_json(index: usize, amount: u64, mint: &Pubkey) -> serde_json::Value {
    serde_json::json!({
        "accountIndex": index,
        "mint": mint.to_string(),
        "uiTokenAmount": {
            "amount": amount.to_string(),
            "decimals": DECIMALS,
            "uiAmount": null,
            "uiAmountString": amount.to_string(),
        },
        "programId": token_program().to_string(),
    })
}

/// Renders `tx` the way `getTransaction` (base64 encoding) returns it.
fn landed(
    tx: &Transaction,
    err: Option<serde_json::Value>,
    dest: &Pubkey,
    pre: Option<u64>,
    post: Option<u64>,
    meta_mint: &Pubkey,
) -> EncodedConfirmedTransactionWithStatusMeta {
    let dest_index = tx
        .message
        .account_keys
        .iter()
        .position(|k| k == dest)
        .unwrap();
    let pre_balances: Vec<serde_json::Value> = pre
        .map(|a| vec![token_balance_json(dest_index, a, meta_mint)])
        .unwrap_or_default();
    let post_balances: Vec<serde_json::Value> = post
        .map(|a| vec![token_balance_json(dest_index, a, meta_mint)])
        .unwrap_or_default();
    let status = match &err {
        None => serde_json::json!({"Ok": null}),
        Some(e) => serde_json::json!({"Err": e}),
    };
    let v = serde_json::json!({
        "slot": 446_600_000u64,
        "blockTime": NOW - 30,
        "transaction": [base64(&bincode::serialize(tx).unwrap()), "base64"],
        "meta": {
            "err": err,
            "status": status,
            "fee": 5000,
            "preBalances": [],
            "postBalances": [],
            "preTokenBalances": pre_balances,
            "postTokenBalances": post_balances,
        }
    });
    serde_json::from_value(v).unwrap()
}

fn results_for(
    batch: &RefundBatch,
    wallet: &Pubkey,
    sigs: &[(i64, &Transaction)],
) -> RefundResults {
    let mut r = RefundResults::new_for(batch, wallet, NOW);
    for (id, tx) in sigs {
        let e = r.results.iter_mut().find(|e| e.request_id == *id).unwrap();
        e.status = ResultStatus::Finalized;
        e.tx_signature = Some(tx.signatures[0].to_string());
        e.recent_blockhash = Some(tx.message.recent_blockhash.to_string());
        e.slot = Some(446_600_000);
        e.submitted_at = Some(NOW - 60);
        e.finalized_at = Some(NOW - 20);
    }
    r
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

// ------------------------------------------------------------------ pure --

#[test]
fn amount_and_stamp_formatting() {
    assert_eq!(format_amount(50_000_000_000, 6), "50000.000000");
    assert_eq!(format_amount(1, 6), "0.000001");
    assert_eq!(format_amount(2_074_080, 9), "0.002074080");
    assert_eq!(format_amount(7, 0), "7");
    assert_eq!(utc_stamp(0), "19700101T000000Z");
    assert_eq!(utc_stamp(1_789_400_000), "20260914T153320Z");
    assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
}

#[test]
fn digest_is_order_independent_and_tamper_evident() {
    let e = |id: i64, amt: &str| RefundBatchEntry {
        request_id: id,
        route: "SolToGlc".into(),
        state: "ManualReview".into(),
        manual_review_reason: None,
        created_at: 1,
        source_obligation_index: 1,
        recipient: "r".into(),
        recipient_token_account: "a".into(),
        recipient_token_account_exists: true,
        mint: "m".into(),
        amount_atomic: amt.into(),
        amount_display: "x".into(),
        amount_canonical_atomic: "y".into(),
        memo: "z".into(),
    };
    let a = batch_digest(&[e(1, "10"), e(2, "20")]);
    let b = batch_digest(&[e(2, "20"), e(1, "10")]);
    assert_eq!(a, b);
    assert_ne!(a, batch_digest(&[e(1, "10"), e(2, "21")]));
}

#[test]
fn memo_names_batch_and_request() {
    assert_eq!(memo_text("mrb-x", 4361), "glc-manual-refund:mrb-x:4361");
}

// ------------------------------------------------------- expectation --

#[test]
fn chain_expectation_derives_recipient_and_amount_from_the_obligation() {
    let rpc = MockRpc::new();
    let ledger = Ledger::open_in_memory().unwrap();
    let requester = Pubkey::new_unique();
    let id = eligible(&rpc, &ledger, requester, 4001);
    let req = ledger.get_request(id).unwrap().unwrap();
    let exp = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap();
    assert_eq!(exp.requester, requester);
    assert_eq!(exp.amount_atomic, MINT_UNITS);
    assert_eq!(exp.amount_canonical_atomic, GROSS);
    assert_eq!(exp.mint, mint());
    assert_eq!(exp.token_program, token_program());
    assert_eq!(
        exp.recipient_token_account,
        accounts::associated_token_address(&requester, &mint(), &token_program())
    );
    assert!(!exp.recipient_token_account_exists);
    rpc.set(
        exp.recipient_token_account,
        token_account(mint(), requester, 5, token_program()),
    );
    let exp = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap();
    assert!(exp.recipient_token_account_exists);
}

#[test]
fn chain_expectation_refuses_every_disagreement() {
    let rpc = MockRpc::new();
    let ledger = Ledger::open_in_memory().unwrap();
    let requester = Pubkey::new_unique();
    let id = eligible(&rpc, &ledger, requester, 4001);
    let req = ledger.get_request(id).unwrap().unwrap();

    // Configured mint differs from the on-chain reserve mint.
    let e = rt()
        .block_on(chain_expectation(&rpc, &req, &Pubkey::new_unique()))
        .unwrap_err();
    assert!(e.contains("not the on-chain reserve mint"), "{e}");
    // Obligation requester differs from the stored one.
    rpc.set(
        accounts::withdrawal_obligation_pda(4001),
        obligation_account(
            4001,
            MINT_UNITS,
            Pubkey::new_unique(),
            WITHDRAWAL_STATUS_PENDING,
        ),
    );
    let e = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap_err();
    assert!(e.contains("requester does not match"), "{e}");
    // Amount differs.
    rpc.set(
        accounts::withdrawal_obligation_pda(4001),
        obligation_account(4001, MINT_UNITS - 1, requester, WITHDRAWAL_STATUS_PENDING),
    );
    let e = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap_err();
    assert!(
        e.contains("does not equal the on-chain deposited amount"),
        "{e}"
    );
    // Obligation already Completed — a competing settlement.
    rpc.set(
        accounts::withdrawal_obligation_pda(4001),
        obligation_account(4001, MINT_UNITS, requester, WITHDRAWAL_STATUS_COMPLETED),
    );
    let e = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap_err();
    assert!(e.contains("not Pending"), "{e}");
    // Obligation missing.
    rpc.accounts
        .lock()
        .unwrap()
        .remove(&accounts::withdrawal_obligation_pda(4001));
    let e = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap_err();
    assert!(e.contains("does not exist"), "{e}");
    // Recipient ATA exists but holds another mint.
    rpc.set(
        accounts::withdrawal_obligation_pda(4001),
        obligation_account(4001, MINT_UNITS, requester, WITHDRAWAL_STATUS_PENDING),
    );
    rpc.set(
        accounts::associated_token_address(&requester, &mint(), &token_program()),
        token_account(Pubkey::new_unique(), requester, 0, token_program()),
    );
    let e = rt()
        .block_on(chain_expectation(&rpc, &req, &mint()))
        .unwrap_err();
    assert!(e.contains("has mint"), "{e}");
}

// ------------------------------------------------------------- export --

#[test]
fn export_includes_eligible_and_reports_every_exclusion() {
    let rpc = MockRpc::new();
    let ledger = Ledger::open_in_memory().unwrap();
    let r1 = Pubkey::new_unique();
    let r2 = Pubkey::new_unique();
    let a = eligible(&rpc, &ledger, r1, 4001);
    let b = eligible(&rpc, &ledger, r2, 4002);
    // Existing ATA for b.
    rpc.set(
        accounts::associated_token_address(&r2, &mint(), &token_program()),
        token_account(mint(), r2, 0, token_program()),
    );
    let robinhood = seed_robinhood_parked(&ledger);
    // Chain says Completed for this one.
    let r3 = Pubkey::new_unique();
    let completed = seed_parked(&ledger, "SolToGlc", r3, 4003);
    rpc.set(
        accounts::withdrawal_obligation_pda(4003),
        obligation_account(4003, MINT_UNITS, r3, WITHDRAWAL_STATUS_COMPLETED),
    );
    // Not in ManualReview at all: never a candidate of the default set.
    let settled = seed_parked(&ledger, "SolToGlc", Pubkey::new_unique(), 4004);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'Settled' WHERE id = ?1",
            [settled],
        )
        .unwrap();

    let out = rt()
        .block_on(export_backlog(
            &rpc,
            &ledger,
            None,
            &mint(),
            "http://rpc",
            "/tmp/ledger.db",
            "cli:test",
            NOW,
        ))
        .unwrap();
    assert_eq!(out.manual_review_total, 4);
    let batch = &out.batch;
    batch.verify().unwrap();
    assert_eq!(batch.request_count, 2);
    assert_eq!(batch.excluded_count, 2);
    assert_eq!(batch.mint_decimals, DECIMALS);
    assert_eq!(batch.token_program, token_program().to_string());
    assert_eq!(batch.total_amount_atomic, (2 * MINT_UNITS).to_string());
    assert_eq!(batch.total_amount_display, "100000.000000");
    // One missing ATA (a), one existing (b).
    assert_eq!(
        batch.estimated_sol_lamports,
        2 * BASE_FEE_LAMPORTS + ata_rent_lamports(&token_program())
    );
    let ea = batch.requests.iter().find(|e| e.request_id == a).unwrap();
    assert_eq!(ea.recipient, r1.to_string());
    assert_eq!(ea.amount_atomic, MINT_UNITS.to_string());
    assert_eq!(ea.amount_display, "50000.000000");
    assert_eq!(ea.amount_canonical_atomic, GROSS.to_string());
    assert_eq!(ea.source_obligation_index, 4001);
    assert!(!ea.recipient_token_account_exists);
    assert_eq!(ea.memo, memo_text(&batch.batch_id, a));
    let eb = batch.requests.iter().find(|e| e.request_id == b).unwrap();
    assert!(eb.recipient_token_account_exists);
    let xr = batch
        .excluded
        .iter()
        .find(|x| x.request_id == robinhood)
        .unwrap();
    assert!(xr.reason.contains("not Solana-sourced"), "{}", xr.reason);
    assert_eq!(xr.route, "RhnToGlc");
    let xc = batch
        .excluded
        .iter()
        .find(|x| x.request_id == completed)
        .unwrap();
    assert!(xc.reason.contains("not Pending"), "{}", xc.reason);
    assert!(batch.batch_id.starts_with("mrb-20260914T"));

    // Explicit ids: a non-ManualReview id is reported, not silently dropped.
    let out = rt()
        .block_on(export_backlog(
            &rpc,
            &ledger,
            Some(vec![a, settled, 999_999]),
            &mint(),
            "http://rpc",
            "/tmp/ledger.db",
            "cli:test",
            NOW,
        ))
        .unwrap();
    assert_eq!(out.batch.request_count, 1);
    assert_eq!(out.batch.excluded_count, 2);
    // Ledger untouched.
    for id in [a, b, robinhood, completed] {
        assert_eq!(
            ledger.get_request(id).unwrap().unwrap().state,
            RequestState::ManualReview
        );
    }
}

#[test]
fn a_tampered_batch_file_is_refused() {
    let rpc = MockRpc::new();
    let ledger = Ledger::open_in_memory().unwrap();
    eligible(&rpc, &ledger, Pubkey::new_unique(), 4001);
    let mut batch = rt()
        .block_on(export_backlog(
            &rpc,
            &ledger,
            None,
            &mint(),
            "u",
            "l",
            "a",
            NOW,
        ))
        .unwrap()
        .batch;
    batch.verify().unwrap();
    batch.requests[0].amount_atomic = "1".into();
    assert!(batch.verify().unwrap_err().contains("digest"));
}

// --------------------------------------------------- landed transaction --

fn expectation(request_id: i64, requester: Pubkey) -> RefundExpectation {
    RefundExpectation {
        request_id,
        obligation_index: 4001,
        requester,
        mint: mint(),
        token_program: token_program(),
        mint_decimals: DECIMALS,
        recipient_token_account: accounts::associated_token_address(
            &requester,
            &mint(),
            &token_program(),
        ),
        recipient_token_account_exists: false,
        amount_atomic: MINT_UNITS,
        amount_canonical_atomic: GROSS,
    }
}

#[test]
fn the_refund_transaction_decodes_and_verifies() {
    let wallet = Keypair::new();
    let requester = Pubkey::new_unique();
    let exp = expectation(4361, requester);
    let memo = memo_text("mrb-t", 4361);
    let tx = signed_refund(&wallet, &requester, MINT_UNITS, &memo);
    assert_eq!(tx.message.instructions.len(), 3);
    let sig = tx.signatures[0];
    let landed_tx = landed(
        &tx,
        None,
        &exp.recipient_token_account,
        None,
        Some(MINT_UNITS),
        &mint(),
    );
    let l = decode_landed_refund(&sig, &landed_tx).unwrap();
    assert_eq!(l.memo, memo);
    assert_eq!(l.amount, MINT_UNITS);
    assert_eq!(l.decimals, DECIMALS);
    assert_eq!(l.mint, mint());
    assert_eq!(l.fee_payer, wallet.pubkey());
    assert_eq!(l.authority, wallet.pubkey());
    assert_eq!(l.destination_token_account, exp.recipient_token_account);
    assert_eq!(l.destination_delta, Some(MINT_UNITS));
    assert_eq!(l.slot, 446_600_000);
    verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap();

    // With a pre-balance too.
    let landed_tx = landed(
        &tx,
        None,
        &exp.recipient_token_account,
        Some(7),
        Some(7 + MINT_UNITS),
        &mint(),
    );
    let l = decode_landed_refund(&sig, &landed_tx).unwrap();
    verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap();
}

#[test]
fn landed_verification_refuses_every_mismatch() {
    let wallet = Keypair::new();
    let requester = Pubkey::new_unique();
    let exp = expectation(4361, requester);
    let ok_memo = memo_text("mrb-t", 4361);
    let dest = exp.recipient_token_account;

    // On-chain failure.
    let tx = signed_refund(&wallet, &requester, MINT_UNITS, &ok_memo);
    let e = decode_landed_refund(
        &tx.signatures[0],
        &landed(
            &tx,
            Some(serde_json::json!("InsufficientFundsForFee")),
            &dest,
            None,
            None,
            &mint(),
        ),
    )
    .unwrap_err();
    assert!(e.contains("FAILED on chain"), "{e}");
    // Wrong signature asked for.
    let e = decode_landed_refund(
        &Signature::default(),
        &landed(&tx, None, &dest, None, Some(MINT_UNITS), &mint()),
    )
    .unwrap_err();
    assert!(e.contains("first signature"), "{e}");
    // Memo names another request.
    let other = signed_refund(&wallet, &requester, MINT_UNITS, &memo_text("mrb-t", 4362));
    let l = decode_landed_refund(
        &other.signatures[0],
        &landed(&other, None, &dest, None, Some(MINT_UNITS), &mint()),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("memo"), "{e}");
    // Wrong batch.
    let e = verify_landed_refund(&l, &exp, "mrb-other", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("memo"), "{e}");
    // Wrong amount.
    let short = signed_refund(&wallet, &requester, MINT_UNITS - 1, &ok_memo);
    let l = decode_landed_refund(
        &short.signatures[0],
        &landed(&short, None, &dest, None, Some(MINT_UNITS - 1), &mint()),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("amount is"), "{e}");
    // Wrong recipient.
    let stranger = Pubkey::new_unique();
    let wrong = signed_refund(&wallet, &stranger, MINT_UNITS, &ok_memo);
    let wrong_dest = accounts::associated_token_address(&stranger, &mint(), &token_program());
    let l = decode_landed_refund(
        &wrong.signatures[0],
        &landed(&wrong, None, &wrong_dest, None, Some(MINT_UNITS), &mint()),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("destination token account"), "{e}");
    // Another wallet sent it.
    let l = decode_landed_refund(
        &tx.signatures[0],
        &landed(&tx, None, &dest, None, Some(MINT_UNITS), &mint()),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &Pubkey::new_unique()).unwrap_err();
    assert!(e.contains("refund wallet"), "{e}");
    // Balance meta disagrees with the instruction.
    let l = decode_landed_refund(
        &tx.signatures[0],
        &landed(&tx, None, &dest, Some(0), Some(MINT_UNITS - 5), &mint()),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("rose by"), "{e}");
    // No balance meta at all: refused, not assumed.
    let l = decode_landed_refund(
        &tx.signatures[0],
        &landed(&tx, None, &dest, None, None, &mint()),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("no destination token-balance meta"), "{e}");
    // Meta names a different mint.
    let l = decode_landed_refund(
        &tx.signatures[0],
        &landed(
            &tx,
            None,
            &dest,
            None,
            Some(MINT_UNITS),
            &Pubkey::new_unique(),
        ),
    )
    .unwrap();
    let e = verify_landed_refund(&l, &exp, "mrb-t", &wallet.pubkey()).unwrap_err();
    assert!(e.contains("meta names mint"), "{e}");
    // A transaction with an extra, foreign instruction is not a refund.
    let mut ixs = build_refund_instructions(
        &wallet.pubkey(),
        &requester,
        &mint(),
        &token_program(),
        DECIMALS,
        MINT_UNITS,
        &ok_memo,
    )
    .unwrap();
    ixs.push(solana_sdk::system_instruction::transfer(
        &wallet.pubkey(),
        &stranger,
        1,
    ));
    let extra = Transaction::new_signed_with_payer(
        &ixs,
        Some(&wallet.pubkey()),
        &[&wallet],
        Hash::new_unique(),
    );
    let e = decode_landed_refund(
        &extra.signatures[0],
        &landed(&extra, None, &dest, None, Some(MINT_UNITS), &mint()),
    )
    .unwrap_err();
    assert!(e.contains("invokes program"), "{e}");
    // No memo at all.
    let ixs = vec![build_refund_instructions(
        &wallet.pubkey(),
        &requester,
        &mint(),
        &token_program(),
        DECIMALS,
        MINT_UNITS,
        &ok_memo,
    )
    .unwrap()[2]
        .clone()];
    let bare = Transaction::new_signed_with_payer(
        &ixs,
        Some(&wallet.pubkey()),
        &[&wallet],
        Hash::new_unique(),
    );
    let e = decode_landed_refund(
        &bare.signatures[0],
        &landed(&bare, None, &dest, None, Some(MINT_UNITS), &mint()),
    )
    .unwrap_err();
    assert!(e.contains("no memo"), "{e}");
}

// ------------------------------------------------------------- import --

#[test]
fn import_verifies_on_chain_then_records_and_closes_idempotently() {
    let rpc = MockRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let wallet = Keypair::new();
    let r1 = Pubkey::new_unique();
    let r2 = Pubkey::new_unique();
    let a = eligible(&rpc, &ledger, r1, 4001);
    let b = eligible(&rpc, &ledger, r2, 4002);
    let batch = rt()
        .block_on(export_backlog(
            &rpc,
            &ledger,
            None,
            &mint(),
            "u",
            "l",
            "cli:x",
            NOW,
        ))
        .unwrap()
        .batch;
    let tx_a = signed_refund(&wallet, &r1, MINT_UNITS, &memo_text(&batch.batch_id, a));
    let dest_a = accounts::associated_token_address(&r1, &mint(), &token_program());
    rpc.put_tx(
        tx_a.signatures[0],
        landed(&tx_a, None, &dest_a, None, Some(MINT_UNITS), &mint()),
    );
    // b: the sender recorded it failed — stays ManualReview.
    let mut results = results_for(&batch, &wallet.pubkey(), &[(a, &tx_a)]);
    {
        let eb = results
            .results
            .iter_mut()
            .find(|e| e.request_id == b)
            .unwrap();
        eb.status = ResultStatus::Failed;
        eb.error = Some("expired before it landed".into());
    }

    // Dry run: verdicts, zero writes.
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &results,
            &mint(),
            false,
            "import",
            "cli:x",
            NOW,
        ))
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].verdict, ImportVerdict::WouldImport);
    assert!(matches!(&rows[1].verdict, ImportVerdict::Skipped(s) if s.contains("failed")));
    assert_eq!(
        ledger.get_request(a).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert!(ledger.list_manual_solana_refunds(10).unwrap().is_empty());
    assert!(ledger
        .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
        .unwrap()
        .is_empty());

    // Execute.
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &results,
            &mint(),
            true,
            "import",
            "cli:x",
            NOW,
        ))
        .unwrap();
    let audit_id = match &rows[0].verdict {
        ImportVerdict::Imported { audit_id } => *audit_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(&rows[1].verdict, ImportVerdict::Skipped(_)));
    let req = ledger.get_request(a).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Closed);
    let closure = ledger.request_closure(a).unwrap().unwrap();
    assert_eq!(
        closure.disposition,
        crate::ledger::ClosureDisposition::RefundedOutOfBand
    );
    assert_eq!(closure.reference, tx_a.signatures[0].to_string());
    let row = ledger.get_manual_solana_refund(a).unwrap().unwrap();
    assert_eq!(row.batch_id, batch.batch_id);
    assert_eq!(row.refund_wallet, wallet.pubkey().to_bytes());
    assert_eq!(row.recipient, r1.to_bytes());
    assert_eq!(row.amount_atomic, MINT_UNITS);
    assert_eq!(row.amount_canonical_atomic, GROSS);
    assert_eq!(row.slot, 446_600_000);
    assert_eq!(row.block_time, Some(NOW - 30));
    assert_eq!(row.imported_by, "cli:x");
    let audit = ledger
        .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
        .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].id, audit_id);
    assert_eq!(audit[0].action, "manual_refund_import");
    assert!(audit[0]
        .new_value
        .as_deref()
        .unwrap()
        .contains(&tx_a.signatures[0].to_string()));
    // b untouched.
    assert_eq!(
        ledger.get_request(b).unwrap().unwrap().state,
        RequestState::ManualReview
    );

    // Second import: ALREADY_IMPORTED, zero writes.
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &results,
            &mint(),
            true,
            "again",
            "cli:x",
            NOW + 5,
        ))
        .unwrap();
    assert_eq!(rows[0].verdict, ImportVerdict::AlreadyImported);
    assert_eq!(ledger.list_manual_solana_refunds(10).unwrap().len(), 1);
    assert_eq!(
        ledger
            .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn import_refuses_unproven_or_mismatched_results_without_writing() {
    let rpc = MockRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let wallet = Keypair::new();
    let r1 = Pubkey::new_unique();
    let a = eligible(&rpc, &ledger, r1, 4001);
    let batch = rt()
        .block_on(export_backlog(
            &rpc,
            &ledger,
            None,
            &mint(),
            "u",
            "l",
            "cli:x",
            NOW,
        ))
        .unwrap()
        .batch;
    let tx_a = signed_refund(&wallet, &r1, MINT_UNITS, &memo_text(&batch.batch_id, a));
    let dest_a = accounts::associated_token_address(&r1, &mint(), &token_program());
    let refused = |rows: &[ImportRow], needle: &str| match &rows[0].verdict {
        ImportVerdict::Refused(r) => assert!(r.contains(needle), "{r}"),
        other => panic!("expected Refused({needle}), got {other:?}"),
    };

    // Signature unknown to the cluster: not proven.
    let results = results_for(&batch, &wallet.pubkey(), &[(a, &tx_a)]);
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &results,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    refused(&rows, "not known at finalized commitment");

    // Manifest amount edited.
    rpc.put_tx(
        tx_a.signatures[0],
        landed(&tx_a, None, &dest_a, None, Some(MINT_UNITS), &mint()),
    );
    let mut edited = results.clone();
    edited.results[0].amount_atomic = "1".into();
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &edited,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    refused(&rows, "authoritative amount");

    // Manifest recipient edited.
    let mut edited = results.clone();
    edited.results[0].recipient = Pubkey::new_unique().to_string();
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &edited,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    refused(&rows, "requester");

    // Wallet claimed differs from the fee payer on chain.
    let mut edited = results.clone();
    let other = Pubkey::new_unique().to_string();
    edited.refund_wallet = other.clone();
    edited.results[0].refund_wallet = other;
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &edited,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    refused(&rows, "refund wallet");

    // A landed transaction whose memo names another request.
    let tx_other = signed_refund(&wallet, &r1, MINT_UNITS, &memo_text(&batch.batch_id, a + 1));
    rpc.put_tx(
        tx_other.signatures[0],
        landed(&tx_other, None, &dest_a, None, Some(MINT_UNITS), &mint()),
    );
    let wrong = results_for(&batch, &wallet.pubkey(), &[(a, &tx_other)]);
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &wrong,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    refused(&rows, "memo");

    // The request changed since export (a payout row appeared).
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, confirmations)
             VALUES (?1, X'00', 1, 0, 0, X'00', 'Built', 100, 0)",
            [a],
        )
        .unwrap();
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &results,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    refused(&rows, "payout row");

    // Through all of that: nothing written.
    assert_eq!(
        ledger.get_request(a).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert!(ledger.list_manual_solana_refunds(10).unwrap().is_empty());
    assert!(ledger.request_closure(a).unwrap().is_none());
    assert!(ledger
        .list_admin_audit(&crate::ledger::AdminAuditFilter::default())
        .unwrap()
        .is_empty());

    // A duplicate signature inside one file is refused for both entries.
    let b = eligible(&rpc, &ledger, Pubkey::new_unique(), 4002);
    let batch2 = rt()
        .block_on(export_backlog(
            &rpc,
            &ledger,
            Some(vec![b]),
            &mint(),
            "u",
            "l",
            "cli:x",
            NOW,
        ))
        .unwrap()
        .batch;
    let mut dup = results_for(&batch2, &wallet.pubkey(), &[]);
    dup.results.push(dup.results[0].clone());
    for e in dup.results.iter_mut() {
        e.status = ResultStatus::Finalized;
        e.tx_signature = Some(tx_a.signatures[0].to_string());
    }
    let rows = rt()
        .block_on(import_results(
            &rpc,
            &mut ledger,
            &dup,
            &mint(),
            true,
            "n",
            "cli:x",
            NOW,
        ))
        .unwrap();
    assert!(rows
        .iter()
        .all(|r| matches!(&r.verdict, ImportVerdict::Refused(m) if m.contains("more than once"))));
}

// ------------------------------------------------ refund return proof --

fn seed_refund_pending(ledger: &Ledger, requester: Pubkey, obligation: u64) -> i64 {
    let id = seed_parked(ledger, "SolToGlc", requester, obligation);
    let nonce = Ledger::solana_refund_nonce(id).unwrap() as i64;
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO solana_refunds
                (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                 destination_token_account, reserve_mint, token_program, manual_review_reason,
                 note, created_by, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?8, ?6, ?7, 'r', 'n', 'cli:x', 'Pending', 1)",
            rusqlite::params![
                id,
                obligation as i64,
                nonce,
                MINT_UNITS as i64,
                requester.as_ref(),
                mint().as_ref(),
                token_program().as_ref(),
                &[0x22u8; 32][..]
            ],
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'RefundPending' WHERE id = ?1",
            [id],
        )
        .unwrap();
    id
}

#[test]
fn refund_return_proof_requires_an_absent_nonce_pda_and_a_pending_obligation() {
    let rpc = MockRpc::new();
    let ledger = Ledger::open_in_memory().unwrap();
    let requester = Pubkey::new_unique();
    rpc.set(
        accounts::withdrawal_obligation_pda(4017),
        obligation_account(4017, MINT_UNITS, requester, WITHDRAWAL_STATUS_PENDING),
    );
    let id = seed_refund_pending(&ledger, requester, 4017);
    let request = ledger.get_request(id).unwrap().unwrap();
    let refund = ledger.get_solana_refund(id).unwrap().unwrap();

    let proof = rt()
        .block_on(prove_refund_never_landed(
            &rpc,
            &request,
            &refund,
            &mint(),
            NOW,
        ))
        .unwrap();
    assert_eq!(proof.request_id, id);
    assert_eq!(proof.nonce, refund.nonce);
    assert!(proof.nonce_pda_absent && proof.obligation_pending);

    // The nonce PDA exists: something landed under this refund's nonce.
    rpc.set(
        accounts::rebalance_withdrawal_pda(refund.nonce),
        account(vec![0u8; 8], PROGRAM_ID),
    );
    let e = rt()
        .block_on(prove_refund_never_landed(
            &rpc,
            &request,
            &refund,
            &mint(),
            NOW,
        ))
        .unwrap_err();
    assert!(e.contains("EXISTS"), "{e}");
    rpc.accounts
        .lock()
        .unwrap()
        .remove(&accounts::rebalance_withdrawal_pda(refund.nonce));

    // The obligation is no longer Pending.
    rpc.set(
        accounts::withdrawal_obligation_pda(4017),
        obligation_account(4017, MINT_UNITS, requester, WITHDRAWAL_STATUS_COMPLETED),
    );
    let e = rt()
        .block_on(prove_refund_never_landed(
            &rpc,
            &request,
            &refund,
            &mint(),
            NOW,
        ))
        .unwrap_err();
    assert!(e.contains("not Pending"), "{e}");

    // The obligation disagrees with the refund row.
    rpc.set(
        accounts::withdrawal_obligation_pda(4017),
        obligation_account(4017, MINT_UNITS - 1, requester, WITHDRAWAL_STATUS_PENDING),
    );
    let e = rt()
        .block_on(prove_refund_never_landed(
            &rpc,
            &request,
            &refund,
            &mint(),
            NOW,
        ))
        .unwrap_err();
    assert!(e.contains("disagrees"), "{e}");

    // The obligation is missing: ambiguous, refused.
    rpc.accounts
        .lock()
        .unwrap()
        .remove(&accounts::withdrawal_obligation_pda(4017));
    let e = rt()
        .block_on(prove_refund_never_landed(
            &rpc,
            &request,
            &refund,
            &mint(),
            NOW,
        ))
        .unwrap_err();
    assert!(e.contains("does not exist"), "{e}");

    // Wrong configured mint.
    rpc.set(
        accounts::withdrawal_obligation_pda(4017),
        obligation_account(4017, MINT_UNITS, requester, WITHDRAWAL_STATUS_PENDING),
    );
    let e = rt()
        .block_on(prove_refund_never_landed(
            &rpc,
            &request,
            &refund,
            &Pubkey::new_unique(),
            NOW,
        ))
        .unwrap_err();
    assert!(e.contains("reserve mint"), "{e}");
}
