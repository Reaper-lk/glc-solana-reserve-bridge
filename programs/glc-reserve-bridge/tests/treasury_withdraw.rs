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
//! 2. **New bounds.** The allowlist, the dedicated per-withdrawal limit,
//!    the dedicated rolling limit, the nonce namespace, and the
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
const PER_WITHDRAWAL_LIMIT: u64 = 100_000;
const ROLLING_LIMIT: u64 = 250_000;
const WINDOW_SECONDS: i64 = 86_400;
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
    let (svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
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

    // The rolling budget is consumed by exactly the amount withdrawn.
    let policy = get_rebalance_policy(&svm);
    assert_eq!(policy.window_total, AMOUNT);
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

/// A rejected withdrawal must not consume any of the rolling budget —
/// otherwise an attacker who cannot steal could still deny service by
/// exhausting the operator's own withdrawal capacity with attempts that
/// were themselves refused.
#[test]
fn a_rejected_withdrawal_consumes_no_rolling_budget() {
    let (mut svm, signers, mint, _treasury, authority) = env();
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);

    for nonce in 1..=3u64 {
        let result = withdraw(
            &mut svm,
            &[&signers[0], &signers[1]],
            &authority,
            &mint,
            &attacker_ata,
            nonce,
            PER_WITHDRAWAL_LIMIT,
            0,
            0,
        );
        assert_bridge_error(result, BridgeError::DestinationNotAllowlisted);
    }

    let policy = get_rebalance_policy(&svm);
    assert_eq!(policy.window_total, 0);
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
    let message = initialize_rebalance_policy_message(
        0,
        &[reserve_ata],
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![reserve_ata],
                PER_WITHDRAWAL_LIMIT,
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
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

// ------------------------------------------------------------- limits --

#[test]
fn amount_exactly_at_the_per_withdrawal_limit_is_accepted() {
    let (mut svm, signers, mint, treasury, authority) = env();
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        PER_WITHDRAWAL_LIMIT,
        0,
        0,
    )
    .expect("exactly the limit is within the limit");
    assert_eq!(token_balance(&svm, &treasury), PER_WITHDRAWAL_LIMIT);
}

#[test]
fn one_atomic_unit_above_the_per_withdrawal_limit_is_rejected() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        NONCE,
        PER_WITHDRAWAL_LIMIT + 1,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalancePerWithdrawalLimit);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

/// The velocity bound: repeated within-limit withdrawals to the LEGITIMATE
/// treasury still cannot exceed the rolling budget inside one window. This
/// is the control that contains a compromise of the treasury custody
/// itself, where the destination check cannot help.
#[test]
fn rolling_limit_stops_a_drain_of_individually_legal_withdrawals() {
    let (mut svm, signers, mint, treasury, authority) = env();

    // ROLLING_LIMIT / PER_WITHDRAWAL_LIMIT = 2.5, so two full-size
    // withdrawals fit and the third must not.
    for nonce in 1..=2u64 {
        withdraw(
            &mut svm,
            &[&signers[0], &signers[1]],
            &authority,
            &mint,
            &treasury,
            nonce,
            PER_WITHDRAWAL_LIMIT,
            0,
            0,
        )
        .unwrap_or_else(|e| panic!("withdrawal {nonce} should succeed: {e:?}"));
    }
    assert_eq!(token_balance(&svm, &treasury), 2 * PER_WITHDRAWAL_LIMIT);
    assert_eq!(
        get_rebalance_policy(&svm).window_total,
        2 * PER_WITHDRAWAL_LIMIT
    );

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        3,
        PER_WITHDRAWAL_LIMIT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalanceRollingLimit);
    assert_eq!(
        token_balance(&svm, &treasury),
        2 * PER_WITHDRAWAL_LIMIT,
        "nothing may move once the budget is exhausted"
    );

    // The remaining headroom (50_000) is still spendable — the budget is a
    // cap, not a hard stop after N withdrawals.
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        4,
        ROLLING_LIMIT - 2 * PER_WITHDRAWAL_LIMIT,
        0,
        0,
    )
    .expect("the exact remaining headroom is spendable");
    assert_eq!(get_rebalance_policy(&svm).window_total, ROLLING_LIMIT);
}

/// Drains the full rolling budget in per-withdrawal-limit-sized pieces —
/// the budget can only ever be spent through withdrawals that are
/// individually legal, so exhausting it takes several.
fn exhaust_rolling_budget(
    svm: &mut litesvm::LiteSVM,
    signers: &[&Keypair],
    authority: &Keypair,
    mint: &solana_sdk::pubkey::Pubkey,
    treasury: &solana_sdk::pubkey::Pubkey,
) {
    let mut spent = 0u64;
    let mut nonce = 1u64;
    while spent < ROLLING_LIMIT {
        let amount = PER_WITHDRAWAL_LIMIT.min(ROLLING_LIMIT - spent);
        withdraw(svm, signers, authority, mint, treasury, nonce, amount, 0, 0)
            .unwrap_or_else(|e| panic!("withdrawal {nonce} of {amount} should succeed: {e:?}"));
        spent += amount;
        nonce += 1;
    }
    assert_eq!(get_rebalance_policy(svm).window_total, ROLLING_LIMIT);
}

#[test]
fn rolling_budget_refills_only_after_the_window_elapses() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let refs: Vec<&Keypair> = vec![&signers[0], &signers[1]];
    exhaust_rolling_budget(&mut svm, &refs, &authority, &mint, &treasury);

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        100,
        1,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalanceRollingLimit);

    warp_seconds(&mut svm, WINDOW_SECONDS);
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        101,
        AMOUNT,
        0,
        0,
    )
    .expect("a fresh window permits withdrawals again");
    assert_eq!(get_rebalance_policy(&svm).window_total, AMOUNT);
}

/// `reset_rolling_volume_window` is admin-gated. It must have no effect on
/// the withdrawal budget, or a compromised admin could refill the very
/// limit that exists to contain them.
#[test]
fn admin_cannot_reset_the_withdrawal_budget_via_reset_rolling_volume_window() {
    let (mut svm, signers, mint, treasury, authority) = env();
    let refs: Vec<&Keypair> = vec![&signers[0], &signers[1]];
    exhaust_rolling_budget(&mut svm, &refs, &authority, &mint, &treasury);

    for direction in [
        glc_reserve_bridge::state::Direction::GoldcoinToSolana,
        glc_reserve_bridge::state::Direction::SolanaToGoldcoin,
    ] {
        send(
            &mut svm,
            reset_rolling_volume_window_ix(&authority.pubkey(), direction),
            &authority,
            &[],
        )
        .expect("resetting a settlement window is still allowed");
    }

    assert_eq!(
        get_rebalance_policy(&svm).window_total,
        ROLLING_LIMIT,
        "the withdrawal budget must be untouched by settlement-window resets"
    );
    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        100,
        1,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalanceRollingLimit);
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
    let (mut svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, RESERVE, RESERVE, WINDOW_SECONDS);

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

    // Governance moves the policy to version 1 (same allowlist, tighter
    // per-withdrawal limit — the change itself is immaterial to the test).
    let epoch = get_attestation_key_set(&svm).epoch;
    let propose_msg = propose_rebalance_policy_message(
        epoch,
        &[treasury],
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &propose_msg),
            propose_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                PER_WITHDRAWAL_LIMIT,
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
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

// ------------------------ no effective per-withdrawal cap (uncapped mode) --
//
// A deployment may legitimately want NO independent per-withdrawal ceiling —
// the rolling budget alone being the real limit. That is expressible today,
// without a sentinel and without weakening anything, by setting
// `per_withdrawal_limit == rolling_limit`.
//
// Why that is exactly equivalent to "disabled", not an approximation:
// the rolling check is `window_total + amount <= rolling_limit` with
// `window_total >= 0`, so it already implies `amount <= rolling_limit`.
// Setting the per-withdrawal ceiling to the same value therefore makes
// that ceiling NON-BINDING: every amount it would reject
// (`amount > rolling_limit`) is an amount the rolling budget rejects too,
// so it can never change the OUTCOME of a withdrawal — only which of the
// two errors is reported, since it happens to be evaluated first. It is
// not unreachable, and these tests deliberately pin that distinction
// rather than overstating it. What matters operationally is the pair of
// properties below: any amount up to the entire budget succeeds in ONE
// transaction, and no combination of withdrawals exceeds the budget.
//
// Deliberately NOT done with a `per_withdrawal_limit == 0` "unlimited"
// sentinel. `validate_rebalance_policy` refuses zero on every limit, and
// `treasury_withdraw` independently re-checks `per_withdrawal_limit > 0`
// and refuses the whole policy as invalid otherwise. Both exist so that a
// zeroed or partially-written policy account fails CLOSED. Teaching zero
// to mean "unlimited" would inverted that: the same corrupted account
// would then read as "no ceiling" — a fail-open direction on exactly the
// state that most warrants suspicion.

const UNCAPPED: u64 = ROLLING_LIMIT;

/// `per_withdrawal_limit == rolling_limit`: no independent single-
/// withdrawal ceiling.
fn uncapped_env() -> Env {
    let authority = Keypair::new();
    let (svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, UNCAPPED, ROLLING_LIMIT, WINDOW_SECONDS);
    (svm, signers, mint, treasury, authority)
}

/// The whole point: one withdrawal may take the ENTIRE rolling budget in a
/// single transaction. Under the default configuration this exact amount
/// would be refused by the per-withdrawal ceiling.
#[test]
fn with_no_per_withdrawal_cap_a_single_withdrawal_may_take_the_entire_budget() {
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();

    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        ROLLING_LIMIT,
        0,
        0,
    )
    .expect("a single withdrawal of the full rolling budget must succeed");

    assert_eq!(token_balance(&svm, &treasury), ROLLING_LIMIT);
    assert_eq!(get_rebalance_policy(&svm).window_total, ROLLING_LIMIT);
}

/// And the rolling cap is still the real limit: having taken the whole
/// budget in one go, nothing more moves inside the window — not even one
/// atomic unit.
#[test]
fn with_no_per_withdrawal_cap_the_rolling_limit_still_cannot_be_exceeded() {
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();

    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        ROLLING_LIMIT,
        0,
        0,
    )
    .expect("first withdrawal takes the whole budget");

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
    assert_bridge_error(result, BridgeError::ExceedsRebalanceRollingLimit);
    assert_eq!(
        token_balance(&svm, &treasury),
        ROLLING_LIMIT,
        "the refused withdrawal moved nothing"
    );
}

/// A request LARGER than the whole budget is refused, moves nothing, and
/// consumes no budget.
///
/// It reports `ExceedsRebalancePerWithdrawalLimit` rather than the rolling
/// error purely because the per-withdrawal check is evaluated first
/// (`treasury_withdraw` step 10 vs step 12). With the two limits equal the
/// two checks reject exactly the same set of amounts, so this is a
/// difference in the error label only — never in whether the withdrawal is
/// permitted. Asserted explicitly so the equivalence is documented by a
/// test rather than assumed.
#[test]
fn with_no_per_withdrawal_cap_an_oversized_request_is_refused_and_moves_nothing() {
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        ROLLING_LIMIT + 1,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalancePerWithdrawalLimit);
    assert_eq!(token_balance(&svm, &treasury), 0);
    assert_eq!(get_rebalance_policy(&svm).window_total, 0);
}

/// The equivalence stated above, proven directly: with
/// `per_withdrawal_limit == rolling_limit`, the largest amount a single
/// withdrawal may move is the budget itself — exactly what the rolling
/// limit alone would allow. There is no smaller independent ceiling.
#[test]
fn with_no_per_withdrawal_cap_the_largest_single_withdrawal_is_the_whole_budget() {
    // Exactly the budget: permitted.
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();
    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        ROLLING_LIMIT,
        0,
        0,
    )
    .expect("the full budget must be movable in one withdrawal");
    assert_eq!(token_balance(&svm, &treasury), ROLLING_LIMIT);

    // One unit more, from a fresh policy: refused.
    let (mut svm2, signers2, mint2, treasury2, authority2) = uncapped_env();
    let result = withdraw(
        &mut svm2,
        &[&signers2[0], &signers2[1]],
        &authority2,
        &mint2,
        &treasury2,
        1,
        ROLLING_LIMIT + 1,
        0,
        0,
    );
    assert!(result.is_err(), "one unit above the budget must be refused");
    assert_eq!(token_balance(&svm2, &treasury2), 0);
}

/// Splitting the same total across many withdrawals does not get more out:
/// the budget is a sum, so the cap holds however the demand is shaped.
#[test]
fn with_no_per_withdrawal_cap_splitting_a_drain_does_not_defeat_the_rolling_limit() {
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();

    let half = ROLLING_LIMIT / 2;
    for nonce in 1..=2u64 {
        withdraw(
            &mut svm,
            &[&signers[0], &signers[1]],
            &authority,
            &mint,
            &treasury,
            nonce,
            half,
            0,
            0,
        )
        .unwrap_or_else(|e| panic!("withdrawal {nonce} should succeed: {e:?}"));
    }

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        3,
        1,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalanceRollingLimit);
    assert_eq!(token_balance(&svm, &treasury), 2 * half);
}

/// Removing the per-withdrawal ceiling does not touch the allowlist. A
/// full-budget withdrawal to a non-allowlisted destination still fails on
/// the allowlist, and consumes no budget.
#[test]
fn with_no_per_withdrawal_cap_the_treasury_allowlist_still_holds() {
    let (mut svm, signers, mint, _treasury, authority) = uncapped_env();
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);

    let result = withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &attacker_ata,
        1,
        ROLLING_LIMIT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::DestinationNotAllowlisted);
    assert_eq!(token_balance(&svm, &attacker_ata), 0);
    assert_eq!(get_rebalance_policy(&svm).window_total, 0);
}

/// Nor does it weaken the threshold requirement: a full-budget withdrawal
/// with only one of the two required attestations is refused.
#[test]
fn with_no_per_withdrawal_cap_the_threshold_attestation_is_still_required() {
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();

    let result = withdraw(
        &mut svm,
        &[&signers[0]],
        &authority,
        &mint,
        &treasury,
        1,
        ROLLING_LIMIT,
        0,
        0,
    );
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
    assert_eq!(token_balance(&svm, &treasury), 0);
    assert_eq!(get_rebalance_policy(&svm).window_total, 0);
}

/// The budget still ages out on its own schedule, and the next window
/// starts empty — the uncapped configuration changes the ceiling on a
/// single withdrawal, never the window's own accounting.
#[test]
fn with_no_per_withdrawal_cap_the_budget_still_ages_out_normally() {
    let (mut svm, signers, mint, treasury, authority) = uncapped_env();

    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        1,
        ROLLING_LIMIT,
        0,
        0,
    )
    .expect("first window's budget is taken in one withdrawal");

    warp_seconds(&mut svm, WINDOW_SECONDS);

    withdraw(
        &mut svm,
        &[&signers[0], &signers[1]],
        &authority,
        &mint,
        &treasury,
        2,
        ROLLING_LIMIT,
        0,
        0,
    )
    .expect("a fresh window permits a fresh full-budget withdrawal");

    assert_eq!(token_balance(&svm, &treasury), 2 * ROLLING_LIMIT);
}

/// A zero `per_withdrawal_limit` is still refused outright. This pins the
/// decision NOT to overload zero as "unlimited": the uncapped
/// configuration is `per_withdrawal_limit == rolling_limit`, and zero
/// remains an invalid policy that fails closed.
#[test]
fn zero_per_withdrawal_limit_is_still_refused_and_does_not_mean_unlimited() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let treasury = create_ata(
        &mut svm,
        &solana_sdk::pubkey::Pubkey::new_unique(),
        &mint,
        0,
    );

    let message =
        initialize_rebalance_policy_message(0, &[treasury], 0, ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                0,
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::ZeroAmount);
    assert!(!rebalance_policy_exists(&svm));
}
