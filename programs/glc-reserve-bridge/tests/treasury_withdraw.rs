//! Integration tests for `treasury_withdraw` — the bounded replacement for
//! the retired `rebalance_withdraw`.
//!
//! Two categories, and both matter:
//!
//! 1. **Preserved invariants.** Every check the retired instruction
//!    performed is re-asserted here from scratch, not assumed to have
//!    carried over: global pause, admin identity, threshold sufficiency,
//!    epoch freshness, protected minimum, replay, mint and token-program
//!    pinning. A hardening patch that silently dropped one of these while
//!    adding new ones would be a net loss.
//! 2. **New bounds.** The allowlist, the nonce namespace, and the
//!    policy-version binding.
//!
//! The incident-shaped negative cases live in `incident_replay.rs`.

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::constants::{NONCE_DOMAIN_REFUND, WITHDRAWAL_CLASS_TREASURY};
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;

const RESERVE: u64 = 1_000_000;
const WITHDRAW_AMOUNT: u64 = 100_000;
const NONCE: u64 = 1;
const AMOUNT: u64 = 5_000;

type Env = (
    litesvm::LiteSVM,
    Vec<Keypair>,
    solana_sdk::pubkey::Pubkey,
    solana_sdk::pubkey::Pubkey,
    Keypair,
);

/// Paused bridge, funded reserve, one canonical allowlisted treasury.
fn env() -> Env {
    let authority = Keypair::new();
    let (svm, signers, mint, treasury) = setup_paused_with_policy(&authority, RESERVE);
    (svm, signers, mint, treasury, authority)
}

/// Builds the two-instruction transaction the on-chain instruction
/// requires: ed25519 proof immediately followed by the withdrawal.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn withdraw(
    svm: &mut litesvm::LiteSVM,
    signers: &[&Keypair],
    authority: &Keypair,
    mint: &solana_sdk::pubkey::Pubkey,
    destination: &solana_sdk::pubkey::Pubkey,
    nonce: u64,
    amount: u64,
    epoch: u64,
    policy_version: u64,
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    let message =
        treasury_withdraw_claim_message(epoch, nonce, amount, destination, mint, policy_version);
    let proof = ed25519_proof_ix(signers, &message);
    let ix = treasury_withdraw_ix(&authority.pubkey(), mint, destination, nonce, amount, epoch);
    send_ixs(svm, &[proof, ix], authority, &[])
}

// ---------------------------------------------------------- happy path --

#[test]
fn withdraws_to_the_allowlisted_treasury_and_records_the_class() {
    let (mut svm, signers, mint, treasury, authority) = env();

    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    )
    .expect("treasury withdrawal should succeed");

    assert_eq!(token_balance(&svm, &treasury), AMOUNT);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), RESERVE - AMOUNT);

    let record = get_rebalance_withdrawal(&svm, NONCE);
    assert_eq!(record.nonce, NONCE);
    assert_eq!(record.amount, AMOUNT);
    assert_eq!(record.destination, treasury);
    assert_eq!(record.admin, authority.pubkey());
    assert_eq!(
        record.class(),
        WITHDRAWAL_CLASS_TREASURY,
        "the record must identify which instruction wrote it"
    );
}

// -------------------------------------------------------- the allowlist --

/// The single most important test in this file: the incident's shape.
/// Everything is legitimate except the destination.
#[test]
fn destination_not_on_the_allowlist_is_rejected() {
    let (mut svm, signers, mint, _treasury, authority) = env();
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &attacker_ata,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::DestinationNotAllowlisted);

    assert_eq!(token_balance(&svm, &attacker_ata), 0);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), RESERVE);
}

/// The reserve vault itself can never be a destination, even before the
/// allowlist is consulted. `validate_rebalance_policy` already refuses to
/// allowlist it, so reaching the handler's own check requires the policy
/// to be wrong — this asserts the independent backstop exists.
#[test]
fn the_reserve_account_can_never_be_allowlisted() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .expect("pause");

    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    let message = initialize_rebalance_policy_message(0, &[reserve_ata]);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(&authority.pubkey(), &mint, vec![reserve_ata]),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::TreasuryDestinationIsReserveItself);
}

/// Fails closed when the policy account does not exist at all: no
/// allowlist means no authorized destination, never "no restriction".
#[test]
fn fails_closed_when_no_policy_has_been_initialized() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .expect("pause");

    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);
    assert!(!rebalance_policy_exists(&svm));

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &destination_ata,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert!(
        result.is_err(),
        "no policy must mean no withdrawal, not an unrestricted one"
    );
    assert_eq!(token_balance(&svm, &destination_ata), 0);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), RESERVE);
}

// ------------------------------------------------- no amount restriction --

/// An ordinary withdrawal to the allowlisted treasury is admitted. There
/// is no per-withdrawal ceiling, no rate limit and no rolling budget in
/// `RebalancePolicy` at all — not a disabled one, not a sentinel — so the
/// only thing an amount is checked against is `protected_minimum`.
#[test]
fn an_ordinary_amount_is_accepted() {
    let (mut svm, signers, mint, treasury, authority) = env();
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        WITHDRAW_AMOUNT,
        0,
        0,
    )
    .expect("an ordinary amount must be admitted");
    assert_eq!(token_balance(&svm, &treasury), WITHDRAW_AMOUNT);
}

/// Repeated withdrawals to the allowlisted treasury are never refused on
/// velocity grounds: nothing accumulates across calls, so the tenth is as
/// admissible as the first. A treasury operation that legitimately needs
/// many transactions must not stall partway through.
#[test]
fn repeated_withdrawals_are_never_rate_limited() {
    let (mut svm, signers, mint, treasury, authority) = env();

    for nonce in 1..=8u64 {
        withdraw(
            &mut svm,
            &[&signers[0], &signers[1]],
            &authority,
            &mint,
            &treasury,
            nonce,
            WITHDRAW_AMOUNT,
            0,
            0,
        )
        .unwrap_or_else(|e| panic!("withdrawal {nonce} should succeed: {e:?}"));
    }

    assert_eq!(token_balance(&svm, &treasury), 8 * WITHDRAW_AMOUNT);
}

/// A single withdrawal may move the ENTIRE reserve, down to
/// `protected_minimum`. This is the property the removed rolling budget
/// would have denied: treasury custody must be able to move everything
/// when it has to, and `protected_minimum` is the only floor.
#[test]
fn a_single_withdrawal_may_take_the_entire_available_reserve() {
    let (mut svm, signers, mint, treasury, authority) = env();

    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        RESERVE,
        0,
        0,
    )
    .expect("the whole reserve must be withdrawable in one call");

    assert_eq!(token_balance(&svm, &treasury), RESERVE);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), 0);
}

/// Removing the amount rules weakens nothing else: a full-reserve
/// withdrawal to a destination that is not allowlisted is still refused,
/// and moves nothing.
#[test]
fn the_treasury_allowlist_still_holds_for_a_full_reserve_withdrawal() {
    let (mut svm, signers, mint, _treasury, authority) = env();
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &attacker_ata,
        1,
        RESERVE,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::DestinationNotAllowlisted);
    assert_eq!(token_balance(&svm, &attacker_ata), 0);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), RESERVE);
}

/// Nor is the threshold requirement weakened: a full-reserve withdrawal
/// with only one of the two required attestations is refused.
#[test]
fn the_threshold_attestation_is_still_required_for_a_full_reserve_withdrawal() {
    let (mut svm, signers, mint, treasury, authority) = env();

    let result = withdraw(
        &mut svm,
        &[&signers[0]],
        &authority,
        &mint,
        &treasury,
        1,
        RESERVE,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

// ------------------------------------------------- preserved invariants --

#[test]
fn unpaused_bridge_rejects_withdrawal() {
    let (mut svm, signers, mint, treasury, authority) = env();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .expect("unpause");

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::BridgeNotPaused);
}

#[test]
fn admin_signature_alone_without_a_threshold_attestation_is_rejected() {
    let (mut svm, _signers, mint, treasury, authority) = env();
    let ix = treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, NONCE, AMOUNT, 0);
    let result = send(&mut svm, ix, &authority, &[]);
    assert_bridge_error(result, BridgeError::MissingSignatureVerification);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

#[test]
fn insufficient_threshold_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let result = withdraw(
        &mut svm,
        &[&signers[0]], // 1 of the required 2
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

#[test]
fn a_signer_who_is_not_a_current_attestation_key_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let outsider = Keypair::new();
    let result = withdraw(
        &mut svm,
        &[&signers[0], &outsider],
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::UnknownAttestationSigner);
}

#[test]
fn a_non_admin_signer_is_rejected_even_with_a_valid_attestation() {
    let (mut svm, signers, mint, treasury, _authority) = env();
    let impostor = Keypair::new();
    svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &impostor,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn stale_attestation_epoch_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        1, // the live epoch is 0
        0,
    );
    assert_bridge_error(result, BridgeError::StaleAttestationEpoch);
}

#[test]
fn replay_of_the_same_nonce_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    )
    .expect("first withdrawal");

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        AMOUNT,
        0,
        0,
    );
    assert!(result.is_err(), "the nonce PDA already exists");
    assert_eq!(token_balance(&svm, &treasury), AMOUNT);
}

#[test]
fn protected_minimum_is_preserved() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(&authority, RESERVE);

    send(
        &mut svm,
        set_limit_ix(
            &authority.pubkey(),
            glc_reserve_bridge::instructions::admin::LimitField::ProtectedMinimum,
            RESERVE - AMOUNT,
        ),
        &authority,
        &[],
    )
    .expect("set protected minimum");

    // Exactly down to the floor is permitted.
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        AMOUNT,
        0,
        0,
    )
    .expect("withdrawing exactly to the floor is allowed");

    // One atomic unit below it is not.
    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        2,
        1,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::InsufficientReserveBalance);
}

#[test]
fn zero_amount_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        0,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ZeroRebalanceAmount);
}

#[test]
fn wrong_token_program_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let message = treasury_withdraw_claim_message(0, NONCE, AMOUNT, &treasury, &mint, 0);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let ix = treasury_withdraw_ix_with_token_program(
        &authority.pubkey(),
        &mint,
        &anchor_spl::token_interface::spl_token_2022::ID,
        &treasury,
        NONCE,
        AMOUNT,
        0,
    );
    let result = send_ixs(&mut svm, &[proof, ix], &authority, &[]);
    assert!(
        result.is_err(),
        "the configured token program is pinned and cannot be substituted"
    );
}

// -------------------------------------------------- nonce namespace --

/// The refund namespace is reserved. A treasury withdrawal must never
/// consume a nonce from it, or the two classes could burn each other's
/// replay-guard slots.
#[test]
fn a_nonce_in_the_refund_namespace_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE_DOMAIN_REFUND | 7,
        AMOUNT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::WrongNonceNamespace);
}

// ------------------------------------------------ policy-version binding --

/// An attestation gathered under policy revision 0 must not be spendable
/// after governance moves the policy to revision 1 — otherwise an approval
/// naming a treasury that has since been removed would still work.
#[test]
fn an_attestation_for_an_older_policy_version_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();

    // Sign for version 0 while the live policy is version 0, but do not
    // submit yet.
    let stale_message = treasury_withdraw_claim_message(0, NONCE, AMOUNT, &treasury, &mint, 0);

    // Governance moves the policy to version 1 (the change itself is
    // immaterial to the test — only the version bump matters).
    let epoch = get_attestation_key_set(&svm).epoch;
    let propose_msg = propose_rebalance_policy_message(epoch, &[treasury]);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &propose_msg),
            propose_rebalance_policy_ix(&authority.pubkey(), &mint, vec![treasury]),
        ],
        &authority,
        &[],
    )
    .expect("propose");
    warp_seconds(&mut svm, 3_600);
    send(
        &mut svm,
        execute_rebalance_policy_ix(&authority.pubkey(), &mint),
        &authority,
        &[],
    )
    .expect("execute");
    assert_eq!(get_rebalance_policy(&svm).version, 1);

    // The stale approval no longer verifies: the program now expects
    // policy_version = 1 in the signed bytes.
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &stale_message);
    let ix = treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, NONCE, AMOUNT, 0);
    let result = send_ixs(&mut svm, &[proof, ix], &authority, &[]);
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

// ------------------------------------------ no regression elsewhere --

#[test]
fn does_not_weaken_release_from_reserve_which_still_works_unpaused() {
    let (mut svm, signers, mint, _treasury, authority) = env();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .expect("unpause");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let txid = [7u8; 32];
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
        .expect("the settlement path is untouched by this hardening");
    assert_eq!(token_balance(&svm, &recipient_ata), AMOUNT);
}
