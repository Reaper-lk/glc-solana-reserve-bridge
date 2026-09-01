//! **Isolated devnet rehearsal of the full ManualReview refund
//! lifecycle** (docs/09-runbook.md "ManualReview refunds
//! (Solana->Goldcoin)"), against a real Solana validator running the real
//! on-chain program — not a mock.
//!
//! # Isolation
//!
//! Everything here is throwaway and ephemeral, created fresh per run and
//! destroyed on drop:
//!
//! - an ephemeral `solana-test-validator` on a random high port, with its
//!   own temp ledger directory (`support::LocalValidator`), the program
//!   baked into genesis. **A private, single-node chain** — strictly more
//!   isolated than public devnet, which is a shared network that would
//!   retain deployed artifacts. Nothing here can reach mainnet or any
//!   public cluster: the RPC URL is `127.0.0.1:<random port>`.
//! - a freshly generated admin/upgrade-authority keypair, a freshly
//!   generated submitter, a freshly generated depositor, and three
//!   freshly generated in-process attestation signers
//!   (`DevAttestationSigner`) — **no production keypair, no remote signer
//!   endpoint, and no auth token is loaded or contacted anywhere in this
//!   file.**
//! - a freshly created, valueless Token-2022 mint (never the production
//!   reserve mint) and a reserve vault ATA derived from it.
//! - a temp-directory SQLite ledger — never `/var/lib/glc-bridge/ledger.db`.
//!
//! # Gating
//!
//! Skips (never fails) unless `GLC_RESERVE_BRIDGE_SO` points at a built
//! program and `solana-test-validator` is on `PATH`, matching the
//! established discipline in `support::phase6_prereqs`.

mod support;

use std::path::PathBuf;
use std::str::FromStr;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic, SolanaAtomic};
use glc_reserve_bridge_service::ledger::{
    AdminAuditFilter, Direction, Ledger, RequestAmounts, RequestState, ReserveDirection,
    SolanaRefundState,
};
use glc_reserve_bridge_service::signing::attestation::DevAttestationSigner;
use glc_reserve_bridge_service::signing::signers::AttestationSigner;
use glc_reserve_bridge_service::solana::accounts::{self, PROGRAM_ID};
use glc_reserve_bridge_service::solana::confirm::ConfirmPolicy;
use glc_reserve_bridge_service::solana::indexer::SolanaIndexer;
use glc_reserve_bridge_service::solana::instructions::{self, LimitField, PauseScope};
use glc_reserve_bridge_service::solana::refund::{self, RefundExecuteOutcome};
use glc_reserve_bridge_service::solana::rpc::SolanaRpc;

const MINT_DECIMALS: u8 = 6;
/// 12.5 GLC in the throwaway mint's own 6-decimal units.
const DEPOSIT_NATIVE: u64 = 12_500_000;
/// Reserve is funded well above the deposit so the protected minimum is a
/// real constraint rather than an artifact of an empty vault.
const RESERVE_FUNDING_NATIVE: u64 = 500_000_000;
const PROTECTED_MINIMUM_NATIVE: u64 = 100_000_000;

fn prereqs() -> Option<PathBuf> {
    let so = std::env::var("GLC_RESERVE_BRIDGE_SO")
        .ok()
        .map(PathBuf::from)?;
    if !so.exists() {
        return None;
    }
    which_test_validator()?;
    Some(so)
}

fn which_test_validator() -> Option<()> {
    std::env::var("PATH").ok().and_then(|path| {
        path.split(':')
            .map(|d| std::path::Path::new(d).join("solana-test-validator"))
            .find(|p| p.exists())
            .map(|_| ())
    })
}

/// `finalized` lags `confirmed` by ~32 slots on a fresh single-node
/// validator, and the indexer reads `BridgeConfig` at `finalized` by
/// design. Wait for the deposit to actually be visible there before
/// ticking, or the tick correctly reports `NoNewObligations`.
async fn wait_for_finalized_obligation_count(
    rpc: &glc_reserve_bridge_service::solana::rpc::RealSolanaRpc,
    expected: u64,
) {
    for _ in 0..200 {
        if let Ok(Some(account)) = rpc.get_account(&accounts::bridge_config_pda()).await {
            if let Ok(cfg) = accounts::decode_bridge_config(&account.data) {
                if cfg.obligation_count >= expected {
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    panic!("obligation_count never reached {expected} at finalized commitment");
}

/// Mirrors what `glc-admin onchain-pause` itself guarantees: it confirms
/// via `solana::confirm::confirm_transaction`, which polls
/// `get_signature_status` and only reports success at FINALIZED
/// commitment. The refund path reads `BridgeConfig` at finalized too, so
/// the real operator sequence never races. The blocking test client used
/// for setup here confirms at `confirmed`, so the rehearsal must wait
/// explicitly to reproduce production ordering.
async fn wait_for_finalized_pause(
    rpc: &glc_reserve_bridge_service::solana::rpc::RealSolanaRpc,
    expected: bool,
) {
    for _ in 0..200 {
        if let Ok(Some(account)) = rpc.get_account(&accounts::bridge_config_pda()).await {
            if let Ok(cfg) = accounts::decode_bridge_config(&account.data) {
                if cfg.paused == expected {
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    panic!("BridgeConfig.paused never reached {expected} at finalized commitment");
}

fn token_2022_id() -> Pubkey {
    Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap()
}

/// The canonical amounts a real fold computes, so the ledger row carries
/// genuine fee-adjusted values rather than a zero-fee shortcut.
fn real_amounts(deposit_native: u64) -> RequestAmounts {
    let gross_canonical = SolanaAtomic(deposit_native)
        .to_canonical(MINT_DECIMALS)
        .expect("widening to canonical is always exact");
    let fee = compute_fee(gross_canonical).expect("fee");
    RequestAmounts {
        gross_atomic: fee.gross.0,
        fee_bps: fee.fee_bps,
        fee_atomic: fee.fee.0,
        net_atomic: fee.net.0,
        // Destination for SolToGlc is Goldcoin-native; canonical IS
        // Goldcoin-native, so net carries over unchanged.
        net_destination_atomic: fee.net.0,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn devnet_refund_rehearsal_full_lifecycle() {
    let Some(so_path) = prereqs() else {
        println!(
            "skipping: set GLC_RESERVE_BRIDGE_SO to a built program and put \
             solana-test-validator on PATH"
        );
        return;
    };

    // ------------------------------------------------------- 1. setup --
    println!("\n===== 1. ISOLATED ENVIRONMENT =====");
    let admin = Keypair::new();
    let submitter = Keypair::new();
    let depositor = Keypair::new();
    let attestation_keypairs: Vec<Keypair> = (0..3).map(|_| Keypair::new()).collect();
    let attestation_pubkeys: Vec<Pubkey> =
        attestation_keypairs.iter().map(|k| k.pubkey()).collect();

    let validator = support::LocalValidator::start(&so_path, &PROGRAM_ID, &admin.pubkey());
    let client = validator.blocking_client();
    let rpc = validator.real_rpc();
    println!(
        "validator RPC        = {} (ephemeral, private)",
        validator.rpc_url()
    );
    println!("program id           = {PROGRAM_ID}");
    println!("admin (throwaway)    = {}", admin.pubkey());
    println!("submitter (throwaway)= {}", submitter.pubkey());
    println!("depositor (throwaway)= {}", depositor.pubkey());
    for (i, k) in attestation_pubkeys.iter().enumerate() {
        println!("attestation key {i}    = {k}");
    }

    support::airdrop(&client, &admin.pubkey(), 100_000_000_000);
    support::airdrop(&client, &submitter.pubkey(), 100_000_000_000);
    support::airdrop(&client, &depositor.pubkey(), 100_000_000_000);

    let token_program = token_2022_id();
    let mint = support::create_throwaway_token2022_mint(&client, &admin, MINT_DECIMALS);
    println!(
        "throwaway mint       = {} (Token-2022, {MINT_DECIMALS} dec)",
        mint.pubkey()
    );
    assert_ne!(
        mint.pubkey().to_string(),
        "Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump",
        "SAFETY: the rehearsal must never use the production reserve mint"
    );

    support::bootstrap_program(
        &client,
        &admin,
        &attestation_pubkeys,
        2,
        &mint.pubkey(),
        &token_program,
    );

    // A real, nonzero protected minimum so the floor genuinely binds.
    let set_min = instructions::set_limit(
        &admin.pubkey(),
        LimitField::ProtectedMinimum,
        PROTECTED_MINIMUM_NATIVE,
    );
    let bh = client.get_latest_blockhash().unwrap();
    client
        .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
            &[set_min],
            Some(&admin.pubkey()),
            &[&admin],
            bh,
        ))
        .expect("set protected_minimum");

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_vault =
        accounts::associated_token_address(&reserve_authority, &mint.pubkey(), &token_program);
    support::mint_to(
        &client,
        &admin,
        &mint.pubkey(),
        &token_program,
        &reserve_vault,
        &admin,
        RESERVE_FUNDING_NATIVE,
    );
    support::wait_for_finalized_balance(&rpc, &reserve_vault, RESERVE_FUNDING_NATIVE).await;
    println!("reserve authority PDA= {reserve_authority}");
    println!("reserve vault ATA    = {reserve_vault}");
    println!("reserve funded       = {RESERVE_FUNDING_NATIVE} native");
    println!("protected minimum    = {PROTECTED_MINIMUM_NATIVE} native");

    // The depositor's own ATA, funded with the tokens they will deposit.
    let depositor_ata = support::create_ata(
        &client,
        &admin,
        &depositor.pubkey(),
        &mint.pubkey(),
        &token_program,
    );
    support::mint_to(
        &client,
        &admin,
        &mint.pubkey(),
        &token_program,
        &depositor_ata,
        &admin,
        DEPOSIT_NATIVE,
    );
    let depositor_balance_start = support::token_balance(&client, &depositor_ata);
    println!("depositor ATA        = {depositor_ata}");
    println!("depositor balance    = {depositor_balance_start} native (pre-deposit)");
    assert_eq!(depositor_balance_start, DEPOSIT_NATIVE);

    // ---------------------------------------------- 2. real deposit --
    println!("\n===== 2. SolToGlc DEPOSIT (real on-chain deposit_to_reserve) =====");
    let glc_address = b"GLCtestDepositorAddress1111111111";
    let deposit_ix = instructions::deposit_to_reserve(
        &depositor.pubkey(),
        &mint.pubkey(),
        &token_program,
        0, // obligation index 0 — the first deposit on this chain
        DEPOSIT_NATIVE,
        glc_address,
    );
    let bh = client.get_latest_blockhash().unwrap();
    let deposit_sig = client
        .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
            &[deposit_ix],
            Some(&depositor.pubkey()),
            &[&depositor],
            bh,
        ))
        .expect("deposit_to_reserve");
    println!("deposit signature    = {deposit_sig}");
    println!(
        "depositor balance    = {} native (post-deposit)",
        support::token_balance(&client, &depositor_ata)
    );
    assert_eq!(support::token_balance(&client, &depositor_ata), 0);

    let obligation_pda = accounts::withdrawal_obligation_pda(0);
    println!("obligation PDA       = {obligation_pda}");

    // The indexer reads at `finalized`; wait for the deposit to be
    // visible there (and for the vault to reflect it) before ticking.
    wait_for_finalized_obligation_count(&rpc, 1).await;
    let vault_after_deposit = RESERVE_FUNDING_NATIVE + DEPOSIT_NATIVE;
    support::wait_for_finalized_balance(&rpc, &reserve_vault, vault_after_deposit).await;
    println!("vault after deposit  = {vault_after_deposit} native (finalized)");

    // ------------------------------- 3. normal fold into ManualReview --
    println!("\n===== 3. NORMAL APPLICATION PATH -> ManualReview =====");
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("rehearsal-ledger.sqlite3");
    println!("ledger (throwaway)   = {}", db_path.display());
    assert!(
        !db_path.starts_with("/var/lib/glc-bridge"),
        "SAFETY: never the production ledger"
    );
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                // The real, finalized on-chain vault balance — so the
                // ledger book and the chain agree at the baseline and
                // reconciliation is meaningful.
                vault_after_deposit,
                PROTECTED_MINIMUM_NATIVE,
                RESERVE_FUNDING_NATIVE,
                RESERVE_FUNDING_NATIVE / 2,
                PROTECTED_MINIMUM_NATIVE * 2,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                // Sized in CANONICAL 8-decimal units, comfortably above
                // the net destination amount of a 12.5 GLC transfer
                // (1,212,500,000 canonical after the 3% fee) so the
                // step-13 resume is exercising the refund guard, not a
                // capacity shortfall.
                ReserveDirection::GoldcoinReserve,
                100_000_000_000,
                1_000_000_000,
                50_000_000_000,
                20_000_000_000,
                5_000_000_000,
                0,
            )
            .unwrap();
        // The realistic cause: admission closed at the moment the deposit
        // was observed. The indexer folds it to ManualReview, never drops it.
        ledger
            .set_admission(
                ReserveDirection::GoldcoinReserve,
                true,
                Some("rehearsal: admission closed before the deposit was observed"),
            )
            .unwrap();
    }

    let mut indexer = SolanaIndexer::new(validator.real_rpc(), Ledger::open(&db_path).unwrap());
    let outcome = indexer.tick().await.expect("indexer tick");
    println!("indexer tick         = {outcome:?}");

    let request_id = {
        let ledger = Ledger::open(&db_path).unwrap();
        let parked = ledger
            .requests_by_state(Direction::SolToGlc, RequestState::ManualReview)
            .unwrap();
        assert_eq!(parked.len(), 1, "exactly one parked request expected");
        let r = &parked[0];
        println!("request id           = {}", r.id);
        println!("request state        = {:?}", r.state);
        println!("manual review reason = {:?}", r.manual_review_note);
        println!("source obligation    = {:?}", r.source_obligation_index);
        println!(
            "requester (from chain)= {}",
            Pubkey::from(r.requester.unwrap())
        );
        assert_eq!(
            r.manual_review_note.as_deref(),
            Some("admission_closed_at_fold")
        );
        assert_eq!(r.requester.unwrap(), depositor.pubkey().to_bytes());
        r.id
    };

    // ------------------------------------------------- 4. DRY RUN --
    println!("\n===== 4. DRY RUN (read-only) =====");
    let (state_before_dry, refund_before_dry, audit_before_dry) = {
        let l = Ledger::open(&db_path).unwrap();
        (
            l.get_request(request_id).unwrap().unwrap().state,
            l.get_solana_refund(request_id).unwrap().is_some(),
            l.list_admin_audit(&AdminAuditFilter::default())
                .unwrap()
                .len(),
        )
    };
    let reserve_before_dry = support::token_balance(&client, &reserve_vault);

    {
        let ledger = Ledger::open(&db_path).unwrap();
        let report = refund::dry_run_refund(&rpc, &ledger, request_id)
            .await
            .expect("dry run");
        let plan = report.plan.as_ref().expect("plan");
        println!("identified requester = {}", plan.requester);
        println!("derived destination  = {}", plan.destination_token_account);
        println!(
            "refund amount        = {} native",
            plan.amount_solana_atomic
        );
        println!("nonce                = {:#x}", plan.nonce);
        for c in &report.checks {
            println!("  [{}] {}", if c.ok { "PASS" } else { "FAIL" }, c.name);
        }
        // The runbook's procedure is dry-run FIRST, then pause. So at
        // this point every request-level check must pass while the pause
        // is correctly reported as not yet engaged — and the verdict must
        // distinguish those two things rather than calling the request
        // ineligible.
        assert!(
            report.eligible_ignoring_pause,
            "every request-level check must pass before the pause is engaged"
        );
        assert!(
            !report.pause_engaged,
            "the bridge is not paused yet at this point"
        );
        assert!(
            !report.would_execute,
            "executing right now would correctly refuse: the pause is not engaged"
        );

        // 4a. requester identified correctly.
        assert_eq!(plan.requester, depositor.pubkey());
        // 4b. canonical Token-2022 ATA derived correctly — and it is
        //     exactly the account the deposit came from.
        assert_eq!(plan.destination_token_account, depositor_ata);
        assert_eq!(
            plan.destination_token_account,
            spl_associated_token_account::get_associated_token_address_with_program_id(
                &depositor.pubkey(),
                &mint.pubkey(),
                &token_program,
            )
        );
        // 4c. exact gross amount, no fee deducted.
        assert_eq!(plan.amount_solana_atomic, DEPOSIT_NATIVE);
        let expected_amounts = real_amounts(DEPOSIT_NATIVE);
        assert!(
            expected_amounts.fee_atomic > 0,
            "the request really does carry a nonzero bridge fee..."
        );
        assert_eq!(
            plan.amount_solana_atomic,
            CanonicalAtomic(expected_amounts.gross_atomic)
                .to_solana(MINT_DECIMALS)
                .unwrap()
                .0,
            "...and the refund is still the full GROSS, not the fee-adjusted net"
        );
    }

    // 4d/4e/4f: nothing contacted, sent, or mutated.
    let (state_after_dry, refund_after_dry, audit_after_dry) = {
        let l = Ledger::open(&db_path).unwrap();
        (
            l.get_request(request_id).unwrap().unwrap().state,
            l.get_solana_refund(request_id).unwrap().is_some(),
            l.list_admin_audit(&AdminAuditFilter::default())
                .unwrap()
                .len(),
        )
    };
    assert_eq!(state_before_dry, state_after_dry, "dry run mutated state");
    assert_eq!(
        refund_before_dry, refund_after_dry,
        "dry run created a refund row"
    );
    assert_eq!(
        audit_before_dry, audit_after_dry,
        "dry run wrote audit rows"
    );
    assert_eq!(
        reserve_before_dry,
        support::token_balance(&client, &reserve_vault),
        "dry run moved funds"
    );
    assert!(
        rpc.get_account(&accounts::rebalance_withdrawal_pda(
            Ledger::solana_refund_nonce(request_id).unwrap()
        ))
        .await
        .unwrap()
        .is_none(),
        "dry run must not create the on-chain refund record"
    );
    println!("no signer contacted (none passed), no tx sent, no DB mutation: CONFIRMED");

    // ------------------------------------------------ 5. global pause --
    println!("\n===== 5. GLOBAL PAUSE =====");
    let pause_ix = instructions::set_paused(&admin.pubkey(), PauseScope::Global, true);
    let bh = client.get_latest_blockhash().unwrap();
    let pause_sig = client
        .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
            &[pause_ix],
            Some(&admin.pubkey()),
            &[&admin],
            bh,
        ))
        .expect("global pause");
    println!("pause signature      = {pause_sig}");
    wait_for_finalized_pause(&rpc, true).await;
    println!("pause finalized      = true");

    // ---------------------------------------------------- 6/7. execute --
    println!("\n===== 6/7. EXECUTE REFUND =====");
    let reserve_before = support::token_balance(&client, &reserve_vault);
    let depositor_before = support::token_balance(&client, &depositor_ata);
    let ledger_book_before = {
        let l = Ledger::open(&db_path).unwrap();
        l.reserve_snapshot(ReserveDirection::SolanaReserve).unwrap()
    };
    println!("reserve balance (chain, before)  = {reserve_before}");
    println!("depositor balance (chain, before)= {depositor_before}");
    println!("ledger book (before)             = {ledger_book_before:?}");

    let signers: Vec<Box<dyn AttestationSigner>> = attestation_keypairs
        .iter()
        .map(|k| {
            Box::new(DevAttestationSigner {
                keypair: k.insecure_clone(),
            }) as Box<dyn AttestationSigner>
        })
        .collect();

    let mut ledger = Ledger::open(&db_path).unwrap();
    let outcome = refund::execute_refund(
        &rpc,
        &mut ledger,
        &signers,
        &admin,
        &submitter,
        request_id,
        "devnet rehearsal: refunding a deposit parked by closed admission",
        "cli:rehearsal",
        ConfirmPolicy::default(),
    )
    .await
    .expect("execute refund");
    let refund_sig = match &outcome {
        RefundExecuteOutcome::Confirmed { signature } => signature.clone(),
        other => panic!("expected Confirmed, got {other:?}"),
    };
    println!("refund signature     = {refund_sig}");
    println!("outcome              = {outcome:?}");

    // 7. finalized: execute_refund only returns Confirmed after
    //    confirm_transaction observed finalized commitment. Re-verify
    //    independently.
    let sig_parsed = solana_sdk::signature::Signature::from_str(&refund_sig).unwrap();
    assert_eq!(
        rpc.get_signature_status(&sig_parsed).await.unwrap(),
        Some(Ok(())),
        "refund tx must be observable at finalized commitment"
    );
    println!("finalized            = CONFIRMED");

    // ------------------------------------ 8/16. balances and accounting --
    println!("\n===== 8/16. BALANCES AND RESERVE ACCOUNTING =====");
    let reserve_after = support::token_balance(&client, &reserve_vault);
    let depositor_after = support::token_balance(&client, &depositor_ata);
    let ledger_book_after = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    println!("reserve balance (chain, after)   = {reserve_after}");
    println!("depositor balance (chain, after) = {depositor_after}");
    println!("ledger book (after)              = {ledger_book_after:?}");
    assert_eq!(
        depositor_after,
        depositor_before + DEPOSIT_NATIVE,
        "depositor must receive EXACTLY the deposited amount"
    );
    assert_eq!(
        reserve_after,
        reserve_before - DEPOSIT_NATIVE,
        "reserve must decrease by exactly the refunded amount"
    );
    assert!(
        reserve_after >= PROTECTED_MINIMUM_NATIVE,
        "protected minimum preserved"
    );
    // The cached book was debited by the same amount, in the same
    // transaction that marked the request Refunded.
    assert_eq!(
        ledger_book_after.0,
        ledger_book_before.0 - DEPOSIT_NATIVE,
        "cached total_reserve_balance must be debited exactly once"
    );
    // Goldcoin-side reservation counters untouched (a fold-time park
    // never held any).
    let gc = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        (gc.2, gc.3),
        (0, 0),
        "no Goldcoin liquidity was ever reserved or released"
    );

    // ------------------------------------------ 9/10. state and record --
    println!("\n===== 9/10. REQUEST STATE AND REFUND ROW =====");
    let request = ledger.get_request(request_id).unwrap().unwrap();
    println!("request state        = {:?}", request.state);
    assert_eq!(request.state, RequestState::Refunded);
    // Original evidence preserved.
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("admission_closed_at_fold")
    );
    assert_eq!(request.source_obligation_index, Some(0));

    let refunds = ledger.list_solana_refunds(false).unwrap();
    assert_eq!(refunds.len(), 1, "exactly one refund record");
    let row = &refunds[0];
    println!(
        "refund row           = request={} state={} nonce={:#x} amount={} obligation=#{}",
        row.request_id,
        row.state.as_str(),
        row.nonce,
        row.amount_solana_atomic,
        row.obligation_index
    );
    println!("  requester          = {}", Pubkey::from(row.requester));
    println!(
        "  destination        = {}",
        Pubkey::from(row.destination_token_account)
    );
    println!("  signature          = {:?}", row.refund_signature);
    println!("  reason             = {}", row.manual_review_reason);
    println!("  created_by         = {}", row.created_by);
    assert_eq!(row.state, SolanaRefundState::Confirmed);
    assert_eq!(row.amount_solana_atomic, DEPOSIT_NATIVE);
    assert_eq!(row.requester, depositor.pubkey().to_bytes());
    assert_eq!(row.destination_token_account, depositor_ata.to_bytes());
    assert_eq!(row.refund_signature.as_deref(), Some(refund_sig.as_str()));

    // 8b. On-chain replay-guard record exists and matches.
    let nonce_pda = accounts::rebalance_withdrawal_pda(row.nonce);
    let nonce_account = rpc
        .get_account(&nonce_pda)
        .await
        .unwrap()
        .expect("nonce PDA");
    let record = refund::decode_rebalance_withdrawal(&nonce_account.data).unwrap();
    println!("on-chain refund PDA  = {nonce_pda}");
    println!("  record             = {record:?}");
    assert_eq!(record.nonce, row.nonce);
    assert_eq!(record.amount, DEPOSIT_NATIVE);
    assert_eq!(record.destination, depositor_ata);

    // -------------------------------- 11/14. idempotent re-execution --
    println!("\n===== 11/14. RERUN (idempotency) =====");
    let rerun = refund::execute_refund(
        &rpc,
        &mut ledger,
        &signers,
        &admin,
        &submitter,
        request_id,
        "devnet rehearsal: rerun must be a no-op",
        "cli:rehearsal",
        ConfirmPolicy::default(),
    )
    .await
    .expect("rerun");
    println!("rerun outcome        = {rerun:?}");
    assert!(matches!(
        rerun,
        RefundExecuteOutcome::AlreadyRefunded { .. }
    ));
    assert_eq!(
        support::token_balance(&client, &reserve_vault),
        reserve_after,
        "rerun must move no funds"
    );
    assert_eq!(
        support::token_balance(&client, &depositor_ata),
        depositor_after,
        "rerun must not pay the depositor twice"
    );
    assert_eq!(ledger.list_solana_refunds(false).unwrap().len(), 1);
    println!("no second transfer   = CONFIRMED");

    // ------------------------------------------- 12. resume rejected --
    println!("\n===== 12. resume-manual-review REJECTED =====");
    let resume_err = ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "try to resume a refunded request",
            "operator",
            9_999,
        )
        .unwrap_err();
    println!("resume result        = {resume_err}");
    assert!(resume_err.to_string().contains("refund lifecycle"));

    // ---------------------------- 13. auto-resume skips, others drain --
    println!("\n===== 13. AUTO-RESUME: refunded skipped, others still eligible =====");
    // A second, genuinely eligible parked request from a different wallet.
    let other_id = {
        let outcome = ledger
            .fold_sol_deposit(
                // A high, synthetic index that can never collide with a
                // real on-chain obligation index in this rehearsal.
                9_999,
                real_amounts(DEPOSIT_NATIVE),
                [0x5A; 32],
                b"GLCotherRecipientAddress222222",
                10_000,
            )
            .unwrap();
        match outcome {
            glc_reserve_bridge_service::ledger::SolFoldOutcome::FoldedManualReview {
                request_id,
            } => request_id,
            other => panic!("expected a parked fold, got {other:?}"),
        }
    };
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // The refunded request is not even a candidate: it is no longer in
    // ManualReview, which is the only state auto-resume selects.
    let candidates = ledger
        .requests_by_state(Direction::SolToGlc, RequestState::ManualReview)
        .unwrap();
    let candidate_ids: Vec<i64> = candidates.iter().map(|r| r.id).collect();
    println!("auto-resume candidates = {candidate_ids:?}");
    assert!(
        !candidate_ids.contains(&request_id),
        "refunded must not be a candidate"
    );
    assert!(candidate_ids.contains(&other_id));
    // And the other one genuinely resumes.
    let other_outcome = ledger
        .resume_manual_review_sol_to_glc(other_id, "auto-resume equivalent", "auto-resume", 11_000)
        .unwrap();
    println!("other request resume   = {other_outcome:?}");
    assert_eq!(
        ledger.get_request(other_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Refunded,
        "the refunded request is untouched while others drain"
    );

    // --------------------- 15. recovery from RefundBroadcast (real) --
    println!("\n===== 15. RECOVERY FROM RefundBroadcast (real chain) =====");
    // A third deposit, parked, whose refund is deliberately left in
    // RefundBroadcast with a signature/blockhash that never landed —
    // exactly the crash-after-record-before-send shape. Recovery must
    // rebuild under the SAME nonce and complete, never double-pay.
    let depositor2 = Keypair::new();
    support::airdrop(&client, &depositor2.pubkey(), 100_000_000_000);
    let depositor2_ata = support::create_ata(
        &client,
        &admin,
        &depositor2.pubkey(),
        &mint.pubkey(),
        &token_program,
    );
    support::mint_to(
        &client,
        &admin,
        &mint.pubkey(),
        &token_program,
        &depositor2_ata,
        &admin,
        DEPOSIT_NATIVE,
    );
    // Deposits are paused globally, so lift only to make the deposit,
    // then re-pause (the refund itself still requires the pause).
    let bh = client.get_latest_blockhash().unwrap();
    client
        .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
            &[instructions::set_paused(
                &admin.pubkey(),
                PauseScope::Global,
                false,
            )],
            Some(&admin.pubkey()),
            &[&admin],
            bh,
        ))
        .unwrap();
    wait_for_finalized_pause(&rpc, false).await;
    let bh = client.get_latest_blockhash().unwrap();
    client
        .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
            &[instructions::deposit_to_reserve(
                &depositor2.pubkey(),
                &mint.pubkey(),
                &token_program,
                1, // obligation_count is 1 after the first deposit
                DEPOSIT_NATIVE,
                b"GLCsecondDepositorAddr3333333",
            )],
            Some(&depositor2.pubkey()),
            &[&depositor2],
            bh,
        ))
        .expect("second deposit");
    let bh = client.get_latest_blockhash().unwrap();
    client
        .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
            &[instructions::set_paused(
                &admin.pubkey(),
                PauseScope::Global,
                true,
            )],
            Some(&admin.pubkey()),
            &[&admin],
            bh,
        ))
        .unwrap();
    wait_for_finalized_pause(&rpc, true).await;

    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("park the next one"),
        )
        .unwrap();
    wait_for_finalized_obligation_count(&rpc, 2).await;
    let mut indexer2 = SolanaIndexer::new(validator.real_rpc(), Ledger::open(&db_path).unwrap());
    indexer2.tick().await.expect("second indexer tick");
    let recovery_id = {
        let l = Ledger::open(&db_path).unwrap();
        let parked = l
            .requests_by_state(Direction::SolToGlc, RequestState::ManualReview)
            .unwrap();
        let r = parked
            .iter()
            .find(|r| r.source_obligation_index == Some(1))
            .expect("the second deposit must be parked");
        r.id
    };
    println!("recovery request id  = {recovery_id}");

    // Drive it to RefundBroadcast with a transaction that never existed.
    {
        let plan =
            refund::build_refund_plan(&rpc, &ledger.get_request(recovery_id).unwrap().unwrap())
                .await
                .expect("plan");
        glc_reserve_bridge_service::admin_api::audited_begin_solana_refund(
            &mut ledger,
            recovery_id,
            &refund::verified_inputs(&plan),
            "rehearsal: simulate a crash after recording the broadcast",
            "cli:rehearsal",
        )
        .unwrap();
        let phantom_sig = solana_sdk::signature::Signature::from([0x5C; 64]);
        let phantom_hash = solana_sdk::hash::Hash::new_unique();
        ledger
            .record_solana_refund_broadcast(
                recovery_id,
                &phantom_sig.to_string(),
                &phantom_hash.to_string(),
                plan.attestation_epoch,
                12_000,
            )
            .unwrap();
        println!("simulated stuck state = RefundBroadcast, tx {phantom_sig} (never landed)");
        assert_eq!(
            ledger.get_request(recovery_id).unwrap().unwrap().state,
            RequestState::RefundBroadcast
        );
    }

    let d2_before = support::token_balance(&client, &depositor2_ata);
    let recovered = refund::execute_refund(
        &rpc,
        &mut ledger,
        &signers,
        &admin,
        &submitter,
        recovery_id,
        "rehearsal: recovery rerun",
        "cli:rehearsal",
        ConfirmPolicy::default(),
    )
    .await
    .expect("recovery");
    let recovered_sig = match &recovered {
        RefundExecuteOutcome::Confirmed { signature } => signature.clone(),
        other => panic!("expected recovery to complete, got {other:?}"),
    };
    println!("recovered signature  = {recovered_sig}");
    let recovery_row = ledger.get_solana_refund(recovery_id).unwrap().unwrap();
    assert_eq!(recovery_row.state, SolanaRefundState::Confirmed);
    assert_eq!(
        recovery_row.nonce,
        Ledger::solana_refund_nonce(recovery_id).unwrap(),
        "recovery must reuse the SAME nonce"
    );
    assert_eq!(
        support::token_balance(&client, &depositor2_ata),
        d2_before + DEPOSIT_NATIVE,
        "the second depositor is paid exactly once"
    );
    assert_eq!(
        ledger.get_request(recovery_id).unwrap().unwrap().state,
        RequestState::Refunded
    );
    println!("recovery             = CONFIRMED, same nonce, paid exactly once");

    // ----------------------------------------- 17. audit / state log --
    println!("\n===== 17. AUDIT AND STATE LOG =====");
    for entry in ledger.state_log(request_id).unwrap() {
        println!(
            "state log #{request_id}: {:?} -> {:?} at {} ({:?})",
            entry.0, entry.1, entry.2, entry.3
        );
    }
    let audit = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    for row in audit.iter().filter(|r| r.action.starts_with("refund_")) {
        println!(
            "audit #{}: {} target={:?} {:?} -> {:?} outcome={:?} note={:?} actor={}",
            row.id,
            row.action,
            row.target,
            row.old_value,
            row.new_value,
            row.outcome,
            row.note,
            row.actor
        );
    }
    let actions: Vec<&str> = audit
        .iter()
        .filter(|r| r.target.as_deref() == Some(request_id.to_string().as_str()))
        .map(|r| r.action.as_str())
        .collect();
    assert!(actions.contains(&"refund_begin"));
    assert!(actions.contains(&"refund_broadcast"));
    assert!(actions.contains(&"refund_confirm"));

    println!("\n===== REHEARSAL COMPLETE: ALL ASSERTIONS PASSED =====");
    println!("deposit tx           = {deposit_sig}");
    println!("refund tx            = {refund_sig}");
    println!("recovery refund tx   = {recovered_sig}");
}
