//! ManualReview -> Goldcoin L1 settlement recovery, end to end.
//!
//! The point of this file is the claim the feature rests on: a recovered
//! request goes through the **existing** payout pipeline, not a second
//! implementation. So the test drives the real lifecycle functions —
//! `rederive_plan` / `independently_sign` / `record_goldcoin_payout_*` /
//! `mark_goldcoin_completion_confirmed` — exactly as
//! `goldcoin_payout_lifecycle.rs` drives them for a normally-admitted
//! request, and asserts a recovered one is indistinguishable at the end.
//!
//! Chain-side verification is covered in
//! `solana::manual_review_settle::tests`; the ledger rules in
//! `ledger::tests`. This file covers the join between recovery and
//! settlement, plus the capacity race against normal admission.

use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::multisig;
use glc_reserve_bridge_service::goldcoin::payout;
use glc_reserve_bridge_service::goldcoin::payout::{PayoutPolicy, ZeroConfChangeMode};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{
    Ledger, RequestState, ReserveDirection, ResumeManualReviewOutcome, SolFoldOutcome,
};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    independently_sign, DevLedgerPayoutSource, DevVaultSigner,
};

const DEST_ADDR: &str = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
const TEST_SOLANA_DECIMALS: u8 = 6;
const TEST_SIGNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn test_policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth: 0,
        zero_conf_change_mode: ZeroConfChangeMode::DepthLimited,
        zero_conf_change_recursive_chain_limit: 20,
    }
}

fn setup_vault() -> (MultisigVault, [DevVaultSigner; 3]) {
    let signers = [
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
    (vault, signers)
}

fn sol_to_glc_amounts(amount: u64) -> glc_reserve_bridge_service::ledger::RequestAmounts {
    let gross = glc_reserve_bridge_service::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let fee = glc_reserve_bridge_service::amount_conversion::compute_fee(gross).unwrap();
    glc_reserve_bridge_service::ledger::RequestAmounts {
        gross_atomic: fee.gross.0,
        fee_bps: fee.fee_bps,
        fee_atomic: fee.fee.0,
        net_atomic: fee.net.0,
        net_destination_atomic: fee.net.0,
    }
}

fn configure_and_fund(ledger: &mut Ledger, vault: &MultisigVault, utxo_amount: u64) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            200_000_000,
            0,
            100_000_000,
            50_000_000,
            20_000_000,
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
        txid: [0xDDu8; 32],
        vout: 0,
        amount_atomic: utxo_amount,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
        .unwrap();
}

/// Parks a SolToGlc request in ManualReview the realistic way — admission
/// closed at the moment the deposit was observed.
fn park(ledger: &mut Ledger, obligation: u64, amount: u64, tag: u8, dest: &str) -> i64 {
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let outcome = ledger
        .fold_sol_deposit(
            obligation,
            sol_to_glc_amounts(amount),
            [tag; 32],
            dest.as_bytes(),
            0,
        )
        .unwrap();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    match outcome {
        SolFoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a ManualReview park, got {other:?}"),
    }
}

/// The REAL payout path — copied verbatim from
/// `goldcoin_payout_lifecycle.rs` so a recovered request is driven by
/// exactly the same calls a normally-admitted one is.
async fn build_sign_and_authorize(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    signers: &[DevVaultSigner; 3],
    request_id: i64,
    now: i64,
) -> [u8; 32] {
    let source = DevLedgerPayoutSource { ledger };
    let (p0, plan, unsigned_tx) = independently_sign(
        &signers[0],
        vault,
        &source,
        request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let (p1, plan1, _) = independently_sign(
        &signers[1],
        vault,
        &source,
        request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(plan, plan1, "independent re-derivation must agree");

    let sighash = unsigned_tx.sighash_all(0, &vault.redeem_script());
    let script_sig = multisig::assemble(vault, &sighash, &[p0, p1]).unwrap();
    let mut signed_tx = unsigned_tx.clone();
    signed_tx.inputs[0].script_sig = script_sig;
    payout::verify_payout_tx(&signed_tx, &plan).unwrap();

    let unsigned_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&unsigned_tx.serialize());
    ledger
        .record_goldcoin_payout_built(request_id, &plan, [0x99u8; 32], &unsigned_hex, now)
        .unwrap();
    let signed_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&signed_tx.serialize());
    ledger
        .reserve_vault_utxos(request_id, &plan.inputs, 0, now)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, &signed_hex, now)
        .unwrap();
    signed_tx.txid()
}

/// THE test for this feature: a request recovered out of ManualReview
/// settles all the way to `Settled` through the normal pipeline, with
/// exactly one payout row and correct accounting — indistinguishable
/// from a request that was never parked.
#[tokio::test]
async fn a_recovered_request_settles_through_the_normal_pipeline_to_settled() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_and_fund(&mut ledger, &vault, 200_000_000);

    let request_id = park(&mut ledger, 0, 500_000, 1, DEST_ADDR);
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    let before = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();

    // Recover it. This is the ONLY recovery-specific step in the whole
    // test; everything after is the normal pipeline.
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(request_id, "recover for L1", "cli:test", 5)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    // Capacity reserved through the same counters normal admission uses.
    let after_recovery = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(after_recovery.2, before.2 + req.net_destination_atomic);
    assert_eq!(after_recovery.3, before.3 + req.net_destination_atomic);

    // --- from here on: the existing pipeline, untouched ---
    let txid = build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SettlementAuthorized
    );
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, 20)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationSubmitted
    );
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, 30)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationConfirmed
    );
    ledger
        .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 35)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, 40)
        .unwrap();

    // Terminal, and accounted exactly like any other settlement.
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    assert_eq!(
        ledger
            .settled_liquidity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        req.net_destination_atomic,
        "a recovered request must settle the same NET amount as any other"
    );
    let end = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(end.2, before.2, "reserved_liquidity returns to baseline");
    assert_eq!(end.3, before.3, "pending_obligations returns to baseline");
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();

    // Exactly one payout, ever — asserted through the public API: the
    // row exists, and a second build for the same request is refused by
    // the PRIMARY KEY boundary that guards against double-pay.
    assert!(ledger.get_goldcoin_payout(request_id).unwrap().is_some());
    let second = ledger.record_goldcoin_payout_built(
        request_id,
        &glc_reserve_bridge_service::goldcoin::payout::PayoutPlan {
            inputs: vec![],
            input_contexts: vec![],
            dest_p2pkh_hash: [0u8; 20],
            payout_atomic: 1,
            change_outputs: vec![],
            vault_script_pubkey: vec![],
            fee_atomic: 0,
        },
        [0u8; 32],
        "00",
        50,
    );
    assert!(
        second.is_err(),
        "a second payout for a recovered request must be structurally impossible"
    );

    // Re-running recovery on a SETTLED request is the idempotent no-op,
    // not an error — the state-log proof of the earlier recovery makes it
    // unambiguous — and critically it reserves nothing a second time.
    let snapshot = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(request_id, "again", "cli:test", 50)
            .unwrap(),
        ResumeManualReviewOutcome::AlreadyResumed {
            state: RequestState::Settled
        }
    );
    assert_eq!(
        ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        snapshot,
        "a repeat recovery on a settled request must reserve nothing"
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled,
        "Settled is terminal and irreversible"
    );

    // And it can never be refunded either.
    let verified = glc_reserve_bridge_service::ledger::VerifiedRefundInputs {
        obligation_index: 0,
        amount_solana_atomic: 500_000,
        gross_canonical_atomic: req.gross_amount_atomic,
        requester: [1u8; 32],
        destination_token_account: [0xDD; 32],
        reserve_mint: [0xEE; 32],
        token_program: [0xFF; 32],
    };
    assert!(
        ledger
            .begin_solana_refund(request_id, &verified, "try refund", "cli:test", 60)
            .is_err(),
        "a settled request must never be refundable"
    );
}

/// Recovery and normal admission contend for ONE counter under SQLite's
/// write lock. With headroom for exactly one of them, exactly one must
/// win — never both, and the reserve invariant must hold either way.
#[test]
fn recovery_and_normal_admission_never_both_take_the_same_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.sqlite3");

    // Headroom for exactly one transfer of this size.
    let one = sol_to_glc_amounts(500_000).net_destination_atomic;
    let parked_id;
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                // balance = protected_minimum + exactly one payout
                1_000 + one,
                1_000,
                1_000 + one,
                1_000 + one,
                2_000,
                0,
            )
            .unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000_000,
                1_000,
                5_000_000_000,
                2_000_000_000,
                10_000,
                0,
            )
            .unwrap();
        parked_id = park(&mut ledger, 0, 500_000, 1, DEST_ADDR);
    }

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    // Thread A: recover the parked request.
    let recover = {
        let path = path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut ledger = Ledger::open(&path).unwrap();
            barrier.wait();
            ledger.resume_manual_review_sol_to_glc(parked_id, "recover", "cli:test", 10)
        })
    };
    // Thread B: a brand-new deposit arrives and wants the same capacity.
    let fold = {
        let path = path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut ledger = Ledger::open(&path).unwrap();
            let amounts = sol_to_glc_amounts(500_000);
            barrier.wait();
            ledger.fold_sol_deposit(1, amounts, [2u8; 32], b"GLCotherRecipient22", 10)
        })
    };

    let recovered = recover.join().unwrap();
    let folded = fold.join().unwrap().unwrap();

    let ledger = Ledger::open(&path).unwrap();
    let (balance, protected, reserved, _pending) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();

    // Whoever lost must have been refused cleanly, not silently
    // over-committed: at most ONE of the two holds the capacity.
    let recovery_won = recovered.is_ok();
    let fold_won = matches!(folded, SolFoldOutcome::FoldedFinalized { .. });
    assert!(
        !(recovery_won && fold_won),
        "both a recovery and a new fold took the same capacity — over-commitment"
    );
    assert!(
        recovery_won || fold_won,
        "one of them should have fitted: reserved={reserved} balance={balance}"
    );

    // The invariant holds regardless of who won.
    // These are u64, so express the invariant as an ADDITION — a
    // subtraction here would underflow rather than report a breach, and
    // `>= 0` would be vacuously true.
    assert!(
        balance >= protected + reserved,
        "reserve over-committed: balance {balance} < protected {protected} + reserved {reserved}"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        reserved, one,
        "exactly one transfer's worth of capacity may be held"
    );
}
