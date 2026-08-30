//! End-to-end (mock-signer, file-backed ledger) test of the Solana->Goldcoin
//! payout lifecycle: SourceFinalized -> (select/reserve UTXOs, build,
//! independently sign) -> SettlementAuthorized -> DestinationSubmitted ->
//! DestinationConfirmed -> Settled. Covers restart recovery and adversarial
//! cases at each step, per the same discipline as
//! `restart_recovery.rs`/`adversarial.rs` (docs/07-implementation-plan.md
//! Phase 3).

use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::multisig;
use glc_reserve_bridge_service::goldcoin::payout;
use glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, ReserveDirection, SolFoldOutcome};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    independently_sign, DevLedgerPayoutSource, DevVaultSigner, IndependentPayoutSource,
};

const DEST_ADDR: &str = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
/// A second, distinct, VALID Goldcoin testnet P2PKH address — used
/// wherever a test folds two obligations in the same tick and needs them
/// to be independent under the SolToGlc recipient rate limit (a different
/// recipient is unaffected by another recipient's own 24h window).
fn second_dest_addr() -> String {
    glc_reserve_bridge_service::goldcoin::address::encode_p2pkh(&[0x42u8; 20], Network::Testnet)
}
/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md) — every `fold_sol_deposit` amount below is Solana-native
/// and gets converted to Goldcoin-native atomic units by `rederive_plan`.
const TEST_SOLANA_DECIMALS: u8 = 6;
const TEST_SIGNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A production-scale change-fanout target relative to this file's
/// test-scale amounts means every test naturally gets exactly one change
/// output, identical to the pre-fan-out behavior these tests were
/// originally written against, EXCEPT the tests below that deliberately
/// size their own UTXOs/dust-threshold to exercise fan-out directly.
fn test_policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth: 0,
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

/// The real NET Goldcoin-native payout for a 500_000 (Solana-native)
/// `fold_sol_deposit` amount, after the 3% bridge fee (docs/20-bridge-
/// fee.md), plus headroom — every test below funds the vault with exactly
/// this so a real payout for that amount succeeds.
fn net_goldcoin_payout_for_500_000_solana_native() -> u64 {
    let gross_canonical = glc_reserve_bridge_service::amount_conversion::SolanaAtomic(500_000)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    glc_reserve_bridge_service::amount_conversion::compute_fee(gross_canonical)
        .unwrap()
        .net
        .0
}

fn test_utxo_amount() -> u64 {
    net_goldcoin_payout_for_500_000_solana_native() + 100_000
}

/// Real gross/fee/net breakdown for `fold_sol_deposit`'s `amount`
/// (Solana-native), mirroring `solana::indexer::tick`'s own conversion
/// (docs/20-bridge-fee.md).
fn sol_to_glc_amounts(amount: u64) -> glc_reserve_bridge_service::ledger::RequestAmounts {
    let gross_canonical = glc_reserve_bridge_service::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let fb = glc_reserve_bridge_service::amount_conversion::compute_fee(gross_canonical).unwrap();
    glc_reserve_bridge_service::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

fn configure_and_fund(ledger: &mut Ledger, vault: &MultisigVault, utxo_amount: u64) {
    // GoldcoinReserve capacity must cover the canonical (8-decimal) scale
    // of Solana-native `fold_sol_deposit` amounts once correctly converted
    // (docs/20-bridge-fee.md); some tests below fold two 500_000
    // (Solana-native) deposits, so this must cover at least 2x the net
    // payout for each.
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

/// Drives the full lifecycle for `request_id` against `ledger`, up through
/// `record_goldcoin_payout_signed` (SettlementAuthorized). Returns the
/// assembled signed transaction hex and computed txid.
async fn build_sign_and_authorize(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    signers: &[DevVaultSigner; 3],
    request_id: i64,
    now: i64,
) -> (String, [u8; 32]) {
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
    payout::verify_payout_tx(&signed_tx, &plan).unwrap(); // signing must not have altered inputs/outputs

    let commitment = [0x99u8; 32]; // placeholder commitment hash for this phase
    let unsigned_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&unsigned_tx.serialize());
    ledger
        .record_goldcoin_payout_built(request_id, &plan, commitment, &unsigned_hex, now)
        .unwrap();

    let signed_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&signed_tx.serialize());
    // reserve the vault UTXOs the plan selected, matching what a real
    // orchestrator would do at build time.
    ledger
        .reserve_vault_utxos(request_id, &plan.inputs, 0, now)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, &signed_hex, now)
        .unwrap();

    (signed_hex, signed_tx.txid())
}

#[tokio::test]
async fn full_lifecycle_reaches_settled_with_exact_fee_adjusted_accounting() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_and_fund(&mut ledger, &vault, test_utxo_amount());
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(500_000),
            [1u8; 32],
            DEST_ADDR.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!()
    };

    let (_, txid) = build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::SettlementAuthorized
    );

    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, 20)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::DestinationSubmitted
    );

    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, 30)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::DestinationConfirmed
    );

    ledger
        .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 35)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, 40)
        .unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        glc_reserve_bridge_service::ledger::RequestState::Settled
    );

    let settled = ledger
        .settled_liquidity(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        settled,
        net_goldcoin_payout_for_500_000_solana_native(),
        "exact fee-adjusted accounting: settled_liquidity must equal the NET amount actually \
         released (gross minus the 3% bridge fee, docs/20-bridge-fee.md), no more, no less"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[tokio::test]
async fn rederive_plan_combines_several_small_utxos_and_computes_correct_change() {
    // Neither UTXO alone covers the payout, so automatic coin selection must
    // combine both (the "several small UTXOs" case) — proves `change_atomic`
    // reconciles exactly, end to end through the real ledger/plan path, not
    // just at `coin::finalize`'s own unit-test level.
    let (vault, _signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
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

    let payout_atomic = net_goldcoin_payout_for_500_000_solana_native();
    let utxo_a = VaultUtxo {
        txid: [0xA1u8; 32],
        vout: 0,
        amount_atomic: payout_atomic / 2 + 50_000,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    let utxo_b = VaultUtxo {
        txid: [0xA2u8; 32],
        vout: 0,
        amount_atomic: payout_atomic / 2 + 50_000,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    ledger
        .sync_vault_utxos(
            &[
                (utxo_a.clone(), 10, vault.script_pubkey_hex()),
                (utxo_b.clone(), 10, vault.script_pubkey_hex()),
            ],
            1,
            0,
        )
        .unwrap();

    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(500_000),
            [1u8; 32],
            DEST_ADDR.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!()
    };

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let plan = source
        .rederive_plan(request_id, &vault, &test_policy(), Network::Testnet)
        .unwrap();

    assert_eq!(
        plan.inputs.len(),
        2,
        "neither UTXO alone covers the payout, so both must be selected"
    );
    let total_selected: u64 = plan.inputs.iter().map(|u| u.amount_atomic).sum();
    assert_eq!(total_selected, utxo_a.amount_atomic + utxo_b.amount_atomic);
    assert_eq!(plan.payout_atomic, payout_atomic);
    assert_eq!(
        plan.total_change_atomic() + plan.payout_atomic + plan.fee_atomic,
        total_selected,
        "change must exactly reconcile total selected minus payout minus network fee"
    );
    assert_eq!(
        plan.change_outputs.len(),
        1,
        "leftover here is tiny relative to the fanout target, so exactly one change output"
    );
    assert!(
        plan.total_change_atomic() > 0,
        "combining two UTXOs must leave real change here"
    );
}

#[tokio::test]
async fn rederive_plan_folds_sub_dust_change_into_the_fee() {
    let (vault, _signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
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

    let payout_atomic = net_goldcoin_payout_for_500_000_solana_native();
    let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
    let fee_with_change = coin::fee_for(1, 2, 1000, input_bytes);
    let dust_threshold = 1000u64;
    // Sized so the only candidate UTXO leaves exactly `dust_threshold - 1`
    // of raw change — below the dust floor, so it must fold entirely into
    // the fee rather than being emitted as a stranded/uneconomical output.
    let utxo_amount = payout_atomic + fee_with_change + (dust_threshold - 1);
    let utxo = VaultUtxo {
        txid: [0xB1u8; 32],
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
            sol_to_glc_amounts(500_000),
            [1u8; 32],
            DEST_ADDR.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!()
    };

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let policy = PayoutPolicy {
        dust_threshold,
        ..test_policy()
    };
    let plan = source
        .rederive_plan(request_id, &vault, &policy, Network::Testnet)
        .unwrap();

    assert_eq!(
        plan.change_outputs,
        Vec::<u64>::new(),
        "sub-dust change must never be emitted as an output"
    );
    assert_eq!(
        plan.fee_atomic,
        utxo_amount - payout_atomic,
        "the folded dust must land entirely in the recorded fee, not vanish"
    );
    let tx = payout::build_unsigned_tx(&plan);
    assert_eq!(
        tx.outputs.len(),
        1,
        "no change output must be built when change folds to zero"
    );
}

#[tokio::test]
async fn automatic_payout_using_the_only_sufficient_utxo_still_leaves_the_reserve_invariant_intact()
{
    // The vault's only UTXO is far larger than this payout needs — the
    // "giant UTXO used when it is the only sufficient option" case, driven
    // all the way through real settlement, proving the oversized-UTXO path
    // never compromises the reserve invariant (docs/05-reserve-accounting.md)
    // even though it necessarily creates a large immature change output.
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_and_fund(&mut ledger, &vault, test_utxo_amount() * 3);
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(500_000),
            [1u8; 32],
            DEST_ADDR.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!()
    };

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let plan = source
        .rederive_plan(request_id, &vault, &test_policy(), Network::Testnet)
        .unwrap();
    assert_eq!(
        plan.inputs.len(),
        1,
        "only one (oversized) UTXO exists; it must still be used, not refused"
    );
    assert!(
        plan.total_change_atomic() > 0,
        "consuming an oversized UTXO must produce real change"
    );

    let (_, txid) = build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, 20)
        .unwrap();
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, 30)
        .unwrap();
    ledger
        .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 35)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, 40)
        .unwrap();

    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::Settled
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[tokio::test]
async fn broadcast_is_idempotent_across_a_restart() {
    let (vault, signers) = setup_vault();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let (request_id, txid) = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure_and_fund(&mut ledger, &vault, test_utxo_amount());
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000),
                [1u8; 32],
                DEST_ADDR.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        let (_, txid) =
            build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
        ledger
            .record_goldcoin_payout_broadcast(request_id, txid, 20)
            .unwrap();
        (request_id, txid)
        // Crash here: broadcast recorded, process exits.
    };

    // Restart: the orchestrator, unsure whether its broadcast call
    // completed before the crash, tries again.
    let mut ledger = Ledger::open(&path).unwrap();
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, 25)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::DestinationSubmitted,
        "replayed broadcast must not re-transition or error"
    );
}

#[tokio::test]
async fn vault_utxo_reservation_survives_restart_and_is_never_double_spent() {
    let (vault, signers) = setup_vault();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let (request_id_a, request_id_b) = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure_and_fund(&mut ledger, &vault, test_utxo_amount()); // exactly one UTXO
        let SolFoldOutcome::FoldedFinalized { request_id: a } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000),
                [1u8; 32],
                DEST_ADDR.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        let SolFoldOutcome::FoldedFinalized { request_id: b } = ledger
            .fold_sol_deposit(
                1,
                sol_to_glc_amounts(500_000),
                [2u8; 32],
                second_dest_addr().as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        build_sign_and_authorize(&mut ledger, &vault, &signers, a, 10).await;
        (a, b)
    };

    // Restart, then attempt to build a payout for `b`, which would need the
    // SAME (only) vault UTXO already reserved by `a`.
    let ledger = Ledger::open(&path).unwrap();
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let result = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_id_b,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await;
    assert!(result.is_err(), "the only vault UTXO is already reserved by request {request_id_a}; request {request_id_b} must fail closed, not double-spend it");
}

#[tokio::test]
async fn settlement_authorized_survives_restart_and_confirmation_flow_still_proceeds() {
    let (vault, signers) = setup_vault();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let (request_id, txid) = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure_and_fund(&mut ledger, &vault, test_utxo_amount());
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000),
                [1u8; 32],
                DEST_ADDR.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        let (_, txid) =
            build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
        (request_id, txid)
        // Crash here: signed and authorized, never broadcast.
    };

    let mut ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::SettlementAuthorized,
        "authorization must survive a restart"
    );
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, 50)
        .unwrap();
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, 60)
        .unwrap();
    ledger
        .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 65)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, 70)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::Settled
    );
}

#[tokio::test]
async fn mark_completed_is_idempotent() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_and_fund(&mut ledger, &vault, test_utxo_amount());
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(500_000),
            [1u8; 32],
            DEST_ADDR.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    let (_, txid) = build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, 20)
        .unwrap();
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, 30)
        .unwrap();
    ledger
        .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 35)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, 40)
        .unwrap();
    // Idempotent replay (e.g. an orchestrator retry after a crash) must not
    // double-credit settled_liquidity.
    ledger
        .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 45)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, 50)
        .unwrap();
    let settled = ledger
        .settled_liquidity(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(
        settled,
        net_goldcoin_payout_for_500_000_solana_native(),
        "double-completion must never double-credit settled liquidity"
    );
    // The SolToGlc bridge fee accrues to the SOURCE (Solana) side
    // (docs/20-bridge-fee.md) — idempotent replay of the completion must
    // not double-credit it either.
    let expected_fee = {
        let gross_canonical = glc_reserve_bridge_service::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap();
        glc_reserve_bridge_service::amount_conversion::compute_fee(gross_canonical)
            .unwrap()
            .fee
            .0
    };
    assert_eq!(
        ledger
            .accrued_fees(ReserveDirection::SolanaReserve)
            .unwrap(),
        expected_fee,
        "double-completion must never double-credit accrued fees"
    );
}

#[tokio::test]
async fn accrued_fees_survive_a_restart() {
    let (vault, signers) = setup_vault();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let expected_fee = {
        let gross_canonical = glc_reserve_bridge_service::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap();
        glc_reserve_bridge_service::amount_conversion::compute_fee(gross_canonical)
            .unwrap()
            .fee
            .0
    };
    {
        let mut ledger = Ledger::open(&path).unwrap();
        configure_and_fund(&mut ledger, &vault, test_utxo_amount());
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                0,
                sol_to_glc_amounts(500_000),
                [1u8; 32],
                DEST_ADDR.as_bytes(),
                0,
            )
            .unwrap()
        else {
            panic!()
        };
        let (_, txid) =
            build_sign_and_authorize(&mut ledger, &vault, &signers, request_id, 10).await;
        ledger
            .record_goldcoin_payout_broadcast(request_id, txid, 20)
            .unwrap();
        ledger
            .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, 30)
            .unwrap();
        ledger
            .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], 35)
            .unwrap();
        ledger
            .mark_goldcoin_completion_confirmed(request_id, 40)
            .unwrap();
        assert_eq!(
            ledger
                .accrued_fees(ReserveDirection::SolanaReserve)
                .unwrap(),
            expected_fee
        );
        // `ledger` dropped here — simulates process exit.
    }

    // Restart: reopen against the same file.
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger
            .accrued_fees(ReserveDirection::SolanaReserve)
            .unwrap(),
        expected_fee,
        "accrued fee accounting must survive a restart, exactly like every other settlement field"
    );
}

#[tokio::test]
async fn a_single_signers_partial_alone_can_never_authorize_a_payout() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_and_fund(&mut ledger, &vault, test_utxo_amount());
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(500_000),
            [1u8; 32],
            DEST_ADDR.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!()
    };

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let (p0, plan, unsigned_tx) = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let sighash = unsigned_tx.sighash_all(0, &vault.redeem_script());
    let result = multisig::assemble(&vault, &sighash, &[p0]);
    assert!(
        result.is_err(),
        "no code path in this crate can turn a single partial signature into an authorized payout"
    );
    let _ = plan;
}
