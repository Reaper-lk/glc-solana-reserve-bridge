//! Adversarial replay of the 2026-09-02 reserve-withdrawal incident.
//!
//! These are the regression tests that matter most in this repository. Each
//! one reconstructs a step the attacker actually took — with genuine
//! credentials, genuine signatures and a genuinely paused bridge — and
//! asserts that it now fails and that no tokens move.
//!
//! # What the attacker had
//!
//! Root on the production bridge host, and therefore:
//!
//! - the admin keypair file, so `BridgeConfig.admin` signatures were real;
//! - the bearer tokens for the attestation signer endpoints, and the
//!   signers would sign any bytes presented with a valid token, so the
//!   2-of-3 threshold attestations were real signatures from real current
//!   attestation keys over the exact bytes being executed;
//! - the submitter keypair, so fees were paid.
//!
//! Every test below GRANTS the attacker all of that. `signers[0]` and
//! `signers[1]` really do sign, and `authority` really is the configured
//! admin. Nothing here relies on the attacker failing to obtain a
//! credential, because in the real incident they obtained all of them.
//!
//! # What stops it now
//!
//! Controls that live in the program and depend on no host, no credential
//! and no operator decision made at withdrawal time:
//!
//! - the destination allowlist, governed by threshold + timelock;
//! - a dedicated per-withdrawal ceiling;
//! - a dedicated rolling budget;
//! - the retirement of the unrestricted instruction entirely;
//! - class separation, so a refund approval is not a withdrawal approval.

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::{LimitField, PauseScope};
use glc_reserve_bridge::state::WithdrawalStatus;

const RESERVE: u64 = 1_000_000;
const PER_WITHDRAWAL_LIMIT: u64 = 100_000;
const ROLLING_LIMIT: u64 = 250_000;
const WINDOW_SECONDS: i64 = 86_400;

fn reserve_ata(mint: &Pubkey) -> Pubkey {
    get_associated_token_address(&reserve_authority_pda(), mint)
}

// ============================================================
// 1. The incident itself, step for step.
// ============================================================

/// The exact 2026-09-02 sequence: pause the bridge, create a withdrawal to
/// an arbitrary external token account, obtain the required signer
/// attestations, execute.
///
/// The attacker has the admin key AND two valid attestation signatures AND
/// the bridge is genuinely paused. The withdrawal is small enough to clear
/// every amount-based limit. It fails anyway, on the destination.
#[test]
fn compromised_admin_with_a_valid_threshold_cannot_withdraw_to_an_attacker_destination() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );

    // Step 1: the bridge is already globally paused (setup did this via the
    // real `set_paused` instruction, exactly as the attacker did).
    assert!(get_config(&svm).paused);

    // Step 2 & 3: build the withdrawal to an arbitrary external token
    // account and obtain genuine 2-of-3 attestations over its exact bytes.
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);
    let amount = 50_000; // comfortably inside every limit
    let message = treasury_withdraw_claim_message(0, 1, amount, &attacker_ata, &mint, 0);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);

    // Step 4 & 5: simulate and execute.
    let result = send_ixs(
        &mut svm,
        &[
            proof,
            treasury_withdraw_ix(&authority.pubkey(), &mint, &attacker_ata, 1, amount, 0),
        ],
        &authority,
        &[],
    );

    assert_bridge_error(result, BridgeError::DestinationNotAllowlisted);
    assert_eq!(
        token_balance(&svm, &attacker_ata),
        0,
        "not one atomic unit may reach an unallowlisted destination"
    );
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE);
}

/// The same attempt through the ORIGINAL instruction, which is what a
/// replayed pre-upgrade transaction or a stale `attested-plan.json` would
/// use. It fails closed with an error naming its replacement, and — the
/// part that matters — the nonce it named is not burned, so a legitimate
/// treasury withdrawal can still use it afterwards.
#[test]
fn the_retired_rebalance_withdraw_moves_nothing_and_burns_no_nonce() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);
    let amount = 50_000;

    // A perfectly-formed pre-upgrade transaction: valid admin, valid 2-of-3
    // over the OLD claim format, paused bridge, within the protected
    // minimum. This is byte-for-byte what used to succeed.
    let message = rebalance_withdraw_claim_message(0, 1, amount, &attacker_ata, &mint);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            rebalance_withdraw_ix(&authority.pubkey(), &mint, &attacker_ata, 1, amount, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::RebalanceWithdrawRetired);
    assert_eq!(token_balance(&svm, &attacker_ata), 0);
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE);

    // The failed attempt unwound completely: nonce 1 is still available to
    // a legitimate withdrawal.
    let legit = treasury_withdraw_claim_message(0, 1, amount, &treasury, &mint, 0);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &legit),
            treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, 1, amount, 0),
        ],
        &authority,
        &[],
    )
    .expect("the rejected attempt must not have consumed the nonce");
    assert_eq!(token_balance(&svm, &treasury), amount);
}

/// Even aimed at the LEGITIMATE treasury, the retired instruction is inert.
/// Retirement is unconditional — it is not a destination check wearing a
/// different name.
#[test]
fn the_retired_rebalance_withdraw_fails_even_toward_the_allowlisted_treasury() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let message = rebalance_withdraw_claim_message(0, 1, 1_000, &treasury, &mint);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            rebalance_withdraw_ix(&authority.pubkey(), &mint, &treasury, 1, 1_000, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::RebalanceWithdrawRetired);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

// ============================================================
// 2. Amount and velocity: the case where the destination is legitimate.
// ============================================================

/// If treasury custody is what went wrong, the allowlist cannot help. The
/// per-withdrawal limit is the control that does.
#[test]
fn an_over_limit_withdrawal_to_the_legitimate_treasury_fails() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );

    // The attacker's real goal: the whole reserve, in one transaction.
    let message = treasury_withdraw_claim_message(0, 1, RESERVE, &treasury, &mint, 0);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, 1, RESERVE, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalancePerWithdrawalLimit);
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE);
}

/// Falling back to many individually-legal withdrawals hits the rolling
/// budget. The reserve cannot be drained in one window regardless of how
/// the attacker splits it.
#[test]
fn a_rolling_limit_drain_of_individually_legal_withdrawals_fails() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );

    let mut taken = 0u64;
    let mut refused = false;
    for nonce in 1..=20u64 {
        let message =
            treasury_withdraw_claim_message(0, nonce, PER_WITHDRAWAL_LIMIT, &treasury, &mint, 0);
        let result = send_ixs(
            &mut svm,
            &[
                ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
                treasury_withdraw_ix(
                    &authority.pubkey(),
                    &mint,
                    &treasury,
                    nonce,
                    PER_WITHDRAWAL_LIMIT,
                    0,
                ),
            ],
            &authority,
            &[],
        );
        match result {
            Ok(_) => taken += PER_WITHDRAWAL_LIMIT,
            Err(_) => {
                refused = true;
                break;
            }
        }
    }

    assert!(refused, "the drain must be stopped, not merely slowed");
    assert!(
        taken <= ROLLING_LIMIT,
        "no more than the rolling budget may leave in one window (took {taken})"
    );
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE - taken);
    assert!(
        token_balance(&svm, &reserve_ata(&mint)) >= RESERVE - ROLLING_LIMIT,
        "at least the reserve minus one window's budget must survive"
    );
}

/// The pre-existing escalation the incident review flagged: a compromised
/// admin can set `protected_minimum` to zero on their own signature. That
/// remains true (it is an existing admin-immediate limit, deliberately not
/// changed by this patch), and it now buys the attacker nothing, because
/// the policy limits are not admin-editable.
#[test]
fn zeroing_the_protected_minimum_no_longer_unlocks_a_drain() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );

    // Still possible on the admin's signature alone.
    send(
        &mut svm,
        set_limit_ix(&authority.pubkey(), LimitField::ProtectedMinimum, 0),
        &authority,
        &[],
    )
    .expect("protected_minimum remains admin-editable (unchanged by this patch)");
    assert_eq!(get_config(&svm).protected_minimum, 0);

    // And still useless: the withdrawal limits are governed elsewhere.
    let message = treasury_withdraw_claim_message(0, 1, RESERVE, &treasury, &mint, 0);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, 1, RESERVE, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalancePerWithdrawalLimit);
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE);
}

/// Nor can the admin widen the per-transfer limit to escape: the treasury
/// ceiling is `RebalancePolicy.per_withdrawal_limit`, a different field in
/// a different account that `set_limit` cannot reach.
#[test]
fn raising_the_settlement_per_transfer_limit_does_not_raise_the_withdrawal_ceiling() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );

    send(
        &mut svm,
        set_limit_ix(&authority.pubkey(), LimitField::PerTransferLimit, u64::MAX),
        &authority,
        &[],
    )
    .expect("the settlement limit is admin-editable");
    send(
        &mut svm,
        set_limit_ix(
            &authority.pubkey(),
            LimitField::RollingVolumeLimit,
            u64::MAX,
        ),
        &authority,
        &[],
    )
    .expect("the settlement rolling limit is admin-editable");

    let message =
        treasury_withdraw_claim_message(0, 1, PER_WITHDRAWAL_LIMIT + 1, &treasury, &mint, 0);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            treasury_withdraw_ix(
                &authority.pubkey(),
                &mint,
                &treasury,
                1,
                PER_WITHDRAWAL_LIMIT + 1,
                0,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::ExceedsRebalancePerWithdrawalLimit);
}

// ============================================================
// 3. Class confusion: a refund approval is not a withdrawal approval.
// ============================================================

/// A refund attestation cannot authorize a treasury withdrawal. Without
/// class separation, the refund path — which legitimately pays arbitrary
/// public wallets — would be a way back to arbitrary destinations.
#[test]
fn a_refund_claim_cannot_authorize_a_treasury_withdrawal() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let depositor = Pubkey::new_unique();
    write_obligation(&mut svm, 1, &depositor, 5_000, WithdrawalStatus::Pending);

    // A genuine refund approval for obligation 1 …
    let refund_message =
        refund_withdraw_claim_message(0, 1, 5_000, &treasury, &mint, 1, &depositor);
    // … presented to `treasury_withdraw`.
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &refund_message),
            treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, 1, 5_000, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert_eq!(token_balance(&svm, &treasury), 0);
}

/// And the reverse: a treasury approval cannot authorize a refund.
#[test]
fn a_treasury_claim_cannot_authorize_a_refund() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let depositor = Pubkey::new_unique();
    let depositor_ata = create_ata(&mut svm, &depositor, &mint, 0);
    write_obligation(&mut svm, 1, &depositor, 5_000, WithdrawalStatus::Pending);

    let nonce = glc_reserve_bridge::constants::NONCE_DOMAIN_REFUND | 1;
    let treasury_message =
        treasury_withdraw_claim_message(0, nonce, 5_000, &depositor_ata, &mint, 0);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &treasury_message),
            refund_withdraw_ix(
                &authority.pubkey(),
                &mint,
                &depositor,
                &depositor_ata,
                nonce,
                5_000,
                0,
                1,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert_eq!(token_balance(&svm, &depositor_ata), 0);
}

/// The refund path itself cannot be turned into an arbitrary-destination
/// withdrawal: even with genuine attestations, funds only ever reach the
/// depositor's own ATA.
#[test]
fn the_refund_path_cannot_be_redirected_to_an_attacker() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let depositor = Pubkey::new_unique();
    create_ata(&mut svm, &depositor, &mint, 0);
    write_obligation(&mut svm, 1, &depositor, 5_000, WithdrawalStatus::Pending);

    let attacker = Keypair::new();
    let attacker_ata = create_ata(&mut svm, &attacker.pubkey(), &mint, 0);
    let nonce = glc_reserve_bridge::constants::NONCE_DOMAIN_REFUND | 1;

    let message =
        refund_withdraw_claim_message(0, nonce, 5_000, &attacker_ata, &mint, 1, &depositor);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            refund_withdraw_ix(
                &authority.pubkey(),
                &mint,
                &depositor,
                &attacker_ata,
                nonce,
                5_000,
                0,
                1,
            ),
        ],
        &authority,
        &[],
    );
    assert!(
        result.is_err(),
        "a refund destination is derived, never chosen"
    );
    assert_eq!(token_balance(&svm, &attacker_ata), 0);
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE);
}

// ============================================================
// 4. The one thing that still works: legitimate operations.
// ============================================================

/// The whole point of bounding rather than banning: after all of the above,
/// a legitimate operator can still move funds to the real treasury and
/// still refund a real depositor. A hardening patch that broke both would
/// have "stopped the attack" the way unplugging the server does.
#[test]
fn legitimate_treasury_withdrawal_and_refund_both_still_work() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let depositor = Pubkey::new_unique();
    let depositor_ata = create_ata(&mut svm, &depositor, &mint, 0);
    write_obligation(&mut svm, 1, &depositor, 5_000, WithdrawalStatus::Pending);

    let message = treasury_withdraw_claim_message(0, 1, 25_000, &treasury, &mint, 0);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            treasury_withdraw_ix(&authority.pubkey(), &mint, &treasury, 1, 25_000, 0),
        ],
        &authority,
        &[],
    )
    .expect("a legitimate treasury withdrawal still works");

    let nonce = glc_reserve_bridge::constants::NONCE_DOMAIN_REFUND | 1;
    let refund_msg =
        refund_withdraw_claim_message(0, nonce, 5_000, &depositor_ata, &mint, 1, &depositor);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &refund_msg),
            refund_withdraw_ix(
                &authority.pubkey(),
                &mint,
                &depositor,
                &depositor_ata,
                nonce,
                5_000,
                0,
                1,
            ),
        ],
        &authority,
        &[],
    )
    .expect("a legitimate refund still works");

    assert_eq!(token_balance(&svm, &treasury), 25_000);
    assert_eq!(token_balance(&svm, &depositor_ata), 5_000);
    assert_eq!(token_balance(&svm, &reserve_ata(&mint)), RESERVE - 30_000);
}

/// And the settlement path — the bridge's actual job — is untouched.
#[test]
fn the_settlement_path_is_unaffected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) = setup_paused_with_policy(
        &authority,
        RESERVE,
        PER_WITHDRAWAL_LIMIT,
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .expect("unpause");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let txid = [9u8; 32];
    let message = release_claim_message(0, &txid, 0, 1_000, &recipient.pubkey(), &mint);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            release_from_reserve_ix(
                &authority.pubkey(),
                &mint,
                &recipient.pubkey(),
                &recipient_ata,
                txid,
                0,
                1_000,
                0,
            ),
        ],
        &authority,
        &[],
    )
    .expect("settlement is untouched");
    assert_eq!(token_balance(&svm, &recipient_ata), 1_000);
}
