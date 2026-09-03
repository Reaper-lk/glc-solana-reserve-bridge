//! Integration tests for `refund_withdraw` — the ManualReview refund path,
//! carved out of the retired `rebalance_withdraw`.
//!
//! The property under test throughout is that the operator chooses WHICH
//! obligation to refund and nothing else: the destination is derived from
//! the obligation's own recorded depositor, and the amount must equal the
//! obligation exactly. There is no input to this instruction that redirects
//! funds.

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::constants::{NONCE_DOMAIN_REFUND, WITHDRAWAL_CLASS_REFUND};
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;
use glc_reserve_bridge::state::WithdrawalStatus;

const RESERVE: u64 = 1_000_000;
const OBLIGATION_INDEX: u64 = 3;
const DEPOSIT: u64 = 42_000;

/// The production nonce shape: `Ledger::solana_refund_nonce(request_id)`.
fn refund_nonce(request_id: u64) -> u64 {
    NONCE_DOMAIN_REFUND | request_id
}

struct Env {
    svm: litesvm::LiteSVM,
    signers: Vec<Keypair>,
    mint: solana_sdk::pubkey::Pubkey,
    authority: Keypair,
    depositor: solana_sdk::pubkey::Pubkey,
    depositor_ata: solana_sdk::pubkey::Pubkey,
}

/// Paused bridge with a funded reserve, an initialized policy (which this
/// path does not consult, deliberately — asserted below), and one
/// `Pending` obligation from a real depositor whose ATA already exists.
fn env() -> Env {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) = setup_paused_with_policy(&authority, RESERVE);
    let depositor = Pubkey::new_unique();
    let depositor_ata = create_ata(&mut svm, &depositor, &mint, 0);
    write_obligation(
        &mut svm,
        OBLIGATION_INDEX,
        &depositor,
        DEPOSIT,
        WithdrawalStatus::Pending,
    );
    Env {
        svm,
        signers,
        mint,
        authority,
        depositor,
        depositor_ata,
    }
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn refund(
    env: &mut Env,
    signer_indices: &[usize],
    destination: &Pubkey,
    requester: &Pubkey,
    nonce: u64,
    amount: u64,
    epoch: u64,
    obligation_index: u64,
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    let message = refund_withdraw_claim_message(
        epoch,
        nonce,
        amount,
        destination,
        &env.mint,
        obligation_index,
        requester,
    );
    let signers: Vec<&Keypair> = signer_indices.iter().map(|i| &env.signers[*i]).collect();
    let proof = ed25519_proof_ix(&signers, &message);
    let ix = refund_withdraw_ix(
        &env.authority.pubkey(),
        &env.mint,
        requester,
        destination,
        nonce,
        amount,
        epoch,
        obligation_index,
    );
    let authority = env.authority.insecure_clone();
    send_ixs(&mut env.svm, &[proof, ix], &authority, &[])
}

// ---------------------------------------------------------- happy path --

#[test]
fn refunds_the_depositor_and_records_the_class() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);

    refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    )
    .expect("refund should succeed");

    assert_eq!(token_balance(&env.svm, &destination), DEPOSIT);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &env.mint);
    assert_eq!(token_balance(&env.svm, &reserve_ata), RESERVE - DEPOSIT);

    let record = get_rebalance_withdrawal(&env.svm, refund_nonce(7));
    assert_eq!(record.amount, DEPOSIT);
    assert_eq!(record.destination, destination);
    assert_eq!(record.class(), WITHDRAWAL_CLASS_REFUND);
}

/// A large refund still works: the obligation amount is the bound, and a
/// refund is not subject to the treasury policy at all.
#[test]
fn a_refund_larger_than_a_treasury_withdrawal_amount_still_works() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) = setup_paused_with_policy(&authority, RESERVE);
    let depositor = Pubkey::new_unique();
    let depositor_ata = create_ata(&mut svm, &depositor, &mint, 0);
    let big = 500_000u64;
    write_obligation(
        &mut svm,
        OBLIGATION_INDEX,
        &depositor,
        big,
        WithdrawalStatus::Pending,
    );
    let mut env = Env {
        svm,
        signers,
        mint,
        authority,
        depositor,
        depositor_ata,
    };

    refund(
        &mut env,
        &[0, 1],
        &depositor_ata,
        &depositor,
        refund_nonce(1),
        big,
        0,
        OBLIGATION_INDEX,
    )
    .expect("a refund is bounded by its obligation, not by the treasury policy");
    assert_eq!(token_balance(&env.svm, &depositor_ata), big);
}

// -------------------------------------------------- destination binding --

/// The core property: an operator cannot redirect a refund. Any account
/// other than the depositor's canonical ATA is rejected structurally by
/// Anchor's `associated_token::authority` constraint.
#[test]
fn a_destination_other_than_the_depositors_ata_is_rejected() {
    let mut env = env();
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut env.svm, &attacker.pubkey(), &env.mint, 0);
    let requester = env.depositor;

    let result = refund(
        &mut env,
        &[0, 1],
        &attacker_ata,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert!(
        result.is_err(),
        "the destination is derived from the obligation, not chosen"
    );
    assert_eq!(token_balance(&env.svm, &attacker_ata), 0);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &env.mint);
    assert_eq!(token_balance(&env.svm, &reserve_ata), RESERVE);
}

/// Nor can the operator substitute a different `requester` account to make
/// some other wallet's ATA look canonical: `requester` is address-pinned to
/// the obligation's own field.
#[test]
fn a_substituted_requester_is_rejected() {
    let mut env = env();
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut env.svm, &attacker.pubkey(), &env.mint, 0);

    let result = refund(
        &mut env,
        &[0, 1],
        &attacker_ata,
        &attacker.pubkey(), // consistent ATA, wrong requester
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert!(
        result.is_err(),
        "requester is pinned to the obligation's recorded depositor"
    );
    assert_eq!(token_balance(&env.svm, &attacker_ata), 0);
}

// ------------------------------------------------------ obligation binding --

#[test]
fn an_amount_other_than_the_obligations_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);

    for wrong in [DEPOSIT - 1, DEPOSIT + 1] {
        let result = refund(
            &mut env,
            &[0, 1],
            &destination,
            &requester,
            refund_nonce(7),
            wrong,
            0,
            OBLIGATION_INDEX,
        );
        assert_bridge_error(result, BridgeError::RefundAmountMismatch);
    }
    assert_eq!(token_balance(&env.svm, &destination), 0);
}

#[test]
fn an_obligation_that_is_not_pending_is_rejected() {
    for status in [WithdrawalStatus::Broadcast, WithdrawalStatus::Completed] {
        let mut env = env();
        let (destination, requester) = (env.depositor_ata, env.depositor);
        write_obligation(&mut env.svm, OBLIGATION_INDEX, &requester, DEPOSIT, status);

        let result = refund(
            &mut env,
            &[0, 1],
            &destination,
            &requester,
            refund_nonce(7),
            DEPOSIT,
            0,
            OBLIGATION_INDEX,
        );
        assert_bridge_error(result, BridgeError::ObligationNotPending);
        assert_eq!(token_balance(&env.svm, &destination), 0);
    }
}

/// The obligation index is both a seed and a signed field, so an
/// attestation covering one obligation cannot be pointed at another.
#[test]
fn an_attestation_for_a_different_obligation_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    write_obligation(
        &mut env.svm,
        OBLIGATION_INDEX + 1,
        &requester,
        DEPOSIT,
        WithdrawalStatus::Pending,
    );

    // Sign for obligation 3, submit against obligation 4.
    let message = refund_withdraw_claim_message(
        0,
        refund_nonce(7),
        DEPOSIT,
        &destination,
        &env.mint,
        OBLIGATION_INDEX,
        &requester,
    );
    let signers: Vec<&Keypair> = vec![&env.signers[0], &env.signers[1]];
    let proof = ed25519_proof_ix(&signers, &message);
    let ix = refund_withdraw_ix(
        &env.authority.pubkey(),
        &env.mint,
        &requester,
        &destination,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX + 1,
    );
    let authority = env.authority.insecure_clone();
    let result = send_ixs(&mut env.svm, &[proof, ix], &authority, &[]);
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
}

// ------------------------------------------------- preserved invariants --

#[test]
fn unpaused_bridge_rejects_refund() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    let authority = env.authority.insecure_clone();
    send(
        &mut env.svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .expect("unpause");

    let result = refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert_bridge_error(result, BridgeError::BridgeNotPaused);
}

#[test]
fn insufficient_threshold_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    let result = refund(
        &mut env,
        &[0],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
    assert_eq!(token_balance(&env.svm, &destination), 0);
}

#[test]
fn a_non_admin_signer_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let message = refund_withdraw_claim_message(
        0,
        refund_nonce(7),
        DEPOSIT,
        &destination,
        &env.mint,
        OBLIGATION_INDEX,
        &requester,
    );
    let signers: Vec<&Keypair> = vec![&env.signers[0], &env.signers[1]];
    let proof = ed25519_proof_ix(&signers, &message);
    let ix = refund_withdraw_ix(
        &impostor.pubkey(),
        &env.mint,
        &requester,
        &destination,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    let result = send_ixs(&mut env.svm, &[proof, ix], &impostor, &[]);
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn protected_minimum_is_preserved() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    let authority = env.authority.insecure_clone();
    send(
        &mut env.svm,
        set_limit_ix(
            &authority.pubkey(),
            glc_reserve_bridge::instructions::admin::LimitField::ProtectedMinimum,
            RESERVE - DEPOSIT + 1,
        ),
        &authority,
        &[],
    )
    .expect("set protected minimum");

    let result = refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert_bridge_error(result, BridgeError::InsufficientReserveBalance);
}

#[test]
fn replay_of_the_same_nonce_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    )
    .expect("first refund");

    let result = refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert!(result.is_err(), "the nonce PDA already exists");
    assert_eq!(token_balance(&env.svm, &destination), DEPOSIT);
}

#[test]
fn stale_attestation_epoch_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    let result = refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        refund_nonce(7),
        DEPOSIT,
        1,
        OBLIGATION_INDEX,
    );
    assert_bridge_error(result, BridgeError::StaleAttestationEpoch);
}

// --------------------------------------------------- nonce namespace --

/// A refund must consume a nonce from the refund half of the space — the
/// half `Ledger::solana_refund_nonce` derives into. A treasury-namespace
/// nonce is refused, so the two classes can never collide on a
/// replay-guard slot.
#[test]
fn a_nonce_outside_the_refund_namespace_is_rejected() {
    let mut env = env();
    let (destination, requester) = (env.depositor_ata, env.depositor);
    let result = refund(
        &mut env,
        &[0, 1],
        &destination,
        &requester,
        7, // no high bit
        DEPOSIT,
        0,
        OBLIGATION_INDEX,
    );
    assert_bridge_error(result, BridgeError::WrongNonceNamespace);
}
