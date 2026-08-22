//! Integration tests for `rebalance_withdraw`: the intentional,
//! operator-initiated reserve withdrawal path. Covers every safety property
//! named in the brief: no single-admin-key authorization, unpaused-state
//! rejection, insufficient-threshold rejection, wrong-mint/wrong-token-
//! program destination rejection, replay rejection, and a genuine
//! successful withdrawal that preserves protected accounting and emits an
//! auditable record.

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;

const NONCE: u64 = 1;
const AMOUNT: u64 = 5_000;

/// A `setup_with_reserve` environment with the bridge already globally
/// paused — the precondition `rebalance_withdraw` requires before it will
/// even attempt authorization.
fn paused_setup(
    reserve_balance: u64,
) -> (
    litesvm::LiteSVM,
    Vec<Keypair>,
    solana_sdk::pubkey::Pubkey,
    Keypair,
) {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, reserve_balance);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .expect("pause should succeed");
    (svm, signers, mint, authority)
}

#[test]
fn happy_path_2_of_3_withdraws_preserves_accounting_and_records() {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    send_ixs(&mut svm, &[proof, withdraw], &authority, &[])
        .expect("rebalance withdrawal should succeed");

    assert_eq!(token_balance(&svm, &destination_ata), AMOUNT);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), 1_000_000 - AMOUNT);

    let record = get_rebalance_withdrawal(&svm, NONCE);
    assert_eq!(record.nonce, NONCE);
    assert_eq!(record.amount, AMOUNT);
    assert_eq!(record.destination, destination_ata);
    assert_eq!(record.admin, authority.pubkey());
}

#[test]
fn unauthorized_admin_signer_alone_is_rejected() {
    // A threshold-valid attestation proof is present, but the `admin`
    // account does not match `BridgeConfig.admin` — admin's identity is
    // independently checked, not inferred from the attestation.
    let (mut svm, signers, mint, _authority) = paused_setup(1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);
    let impostor = Keypair::new();
    svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix(
        &impostor.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, withdraw], &impostor, &[]);
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn admin_signature_alone_without_a_threshold_attestation_is_rejected() {
    // The real admin signs, but no ed25519 verification instruction
    // precedes the withdrawal at all — proves admin alone can never
    // authorize this instruction, matching `release_from_reserve`'s own
    // "MissingSignatureVerification" property.
    let (mut svm, _signers, mint, authority) = paused_setup(1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[withdraw], &authority, &[]);
    assert_bridge_error(result, BridgeError::MissingSignatureVerification);
}

#[test]
fn insufficient_threshold_is_rejected() {
    // Only 1 of the required 2 attestation signatures — the exact
    // "no single key (of any kind) can authorize a withdrawal" property.
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0]], &message);
    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
}

#[test]
fn unpaused_bridge_rejects_withdrawal() {
    // Same environment as the happy path, but WITHOUT the pause step —
    // proves the bridge-must-already-be-paused precondition is actually
    // enforced, not merely documented.
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert_bridge_error(result, BridgeError::BridgeNotPaused);
}

#[test]
fn wrong_mint_destination_is_rejected() {
    // The destination token account is real and valid, but for a
    // DIFFERENT mint than the reserve — Anchor's `token::mint = reserve_mint`
    // constraint must reject it, not silently accept a cross-mint
    // destination.
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000);
    let other_mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_mint(&mut svm, &other_mint, 1_000_000);
    let destination = Keypair::new();
    let wrong_mint_ata = create_ata(&mut svm, &destination.pubkey(), &other_mint, 0);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &wrong_mint_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &wrong_mint_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert!(
        result.is_err(),
        "a destination token account for the wrong mint must be rejected"
    );
}

#[test]
fn wrong_token_program_is_rejected() {
    // The reserve was configured under legacy SPL Token
    // (`setup_with_reserve`'s default); substituting Token-2022 as the
    // instruction's `token_program` must be rejected structurally, not
    // silently accepted — same property `release_from_reserve` has. The
    // reserve_token_account's `associated_token::token_program` constraint
    // is validated before `token_program`'s own `address` constraint (same
    // account-declaration-order effect the analogous
    // `release_rejects_a_token_program_that_does_not_match_the_configured_one`
    // test in token2022_adversarial.rs already documents), so the concrete
    // rejected error is a generic Anchor account-constraint violation, not
    // necessarily `BridgeError::WrongTokenProgram` — what matters, and
    // what's asserted here, is that it is rejected, not which of the two
    // equally-valid constraints catches it first.
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix_with_token_program(
        &authority.pubkey(),
        &mint,
        &anchor_spl::token_interface::spl_token_2022::ID,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert!(
        result.is_err(),
        "substituting the wrong token program must never succeed"
    );
}

#[test]
fn replay_of_the_same_nonce_is_rejected() {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000);
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );
    send_ixs(
        &mut svm,
        &[proof.clone(), withdraw.clone()],
        &authority,
        &[],
    )
    .expect("first withdrawal should succeed");

    // A second attempt with the exact same nonce — the withdrawal-record
    // PDA already exists, so account creation (`init`) fails, exactly the
    // same replay-guard mechanism as `DepositClaim`.
    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert!(result.is_err(), "replay of the same nonce must be rejected");
}

#[test]
fn successful_withdrawal_still_preserves_the_protected_minimum() {
    // The reserve holds exactly enough that a withdrawal of the full
    // amount would breach `protected_minimum` — proves an operator
    // withdrawal is bound by the identical floor a bridge settlement is,
    // not a separate, weaker check.
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let mut config = get_config(&svm);
    config.protected_minimum = 999_000;
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&config, &mut data).unwrap();
    let mut account = svm.get_account(&config_pda()).unwrap();
    account.data = data;
    svm.set_account(config_pda(), account).unwrap();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .expect("pause should succeed");

    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);
    // 1_000_000 reserve - 999_000 protected_minimum = 1_000 max releasable;
    // AMOUNT (5_000) exceeds that.
    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &destination_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw = rebalance_withdraw_ix(
        &authority.pubkey(),
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert_bridge_error(result, BridgeError::InsufficientReserveBalance);
}

#[test]
fn destination_cannot_be_the_reserve_account_itself() {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);

    let message = rebalance_withdraw_claim_message(0, NONCE, AMOUNT, &reserve_ata, &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let withdraw =
        rebalance_withdraw_ix(&authority.pubkey(), &mint, &reserve_ata, NONCE, AMOUNT, 0);

    let result = send_ixs(&mut svm, &[proof, withdraw], &authority, &[]);
    assert_bridge_error(result, BridgeError::RebalanceDestinationIsReserveItself);
}

#[test]
fn does_not_weaken_release_from_reserve_which_still_works_unpaused() {
    // `rebalance_withdraw` requires pause; `release_from_reserve` must
    // continue to require the OPPOSITE (unpaused) and remain otherwise
    // completely unaffected by this new instruction's existence.
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let txid = [0x77u8; 32];

    let message = release_claim_message(0, &txid, 0, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        txid,
        0,
        AMOUNT,
        0,
    );

    send_ixs(&mut svm, &[proof, release], &authority, &[])
        .expect("release_from_reserve must still work exactly as before, unpaused");
    assert_eq!(token_balance(&svm, &recipient_ata), AMOUNT);
}
