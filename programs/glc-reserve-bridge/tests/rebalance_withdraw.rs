//! Retirement tests for `rebalance_withdraw`.
//!
//! This file used to be the integration suite for the operator-withdrawal
//! path. That instruction accepted an arbitrary destination token account
//! and was the path taken during the 2026-09-02 reserve withdrawal, so it
//! now fails closed unconditionally. Its former positive coverage moved to
//! `treasury_withdraw.rs` and `refund_withdraw.rs`; the adversarial replay
//! of the incident itself lives in `incident_replay.rs`.
//!
//! What remains here is the narrow question this file's name still asks:
//! **is the retired instruction genuinely inert, under every input?** A
//! retirement that only held for the inputs someone thought to try would
//! be worse than no retirement, because the fail-closed error message
//! invites the reader to stop checking.

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;

const RESERVE: u64 = 1_000_000;
const AMOUNT: u64 = 5_000;

/// A paused, funded bridge WITHOUT a rebalance policy — deliberately, so
/// these tests prove the retirement is unconditional rather than a policy
/// lookup failing by luck.
fn paused_setup() -> (
    litesvm::LiteSVM,
    Vec<Keypair>,
    solana_sdk::pubkey::Pubkey,
    Keypair,
) {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .expect("pause should succeed");
    (svm, signers, mint, authority)
}

/// The former happy path — every precondition satisfied — now fails, and
/// the reserve is untouched.
#[test]
fn the_former_happy_path_now_fails_closed() {
    let (mut svm, signers, mint, authority) = paused_setup();
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, 1, AMOUNT, &destination_ata, &mint);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            rebalance_withdraw_ix(&authority.pubkey(), &mint, &destination_ata, 1, AMOUNT, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::RebalanceWithdrawRetired);

    assert_eq!(token_balance(&svm, &destination_ata), 0);
    let reserve = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve), RESERVE);
}

/// The rejection is unconditional across the argument space: no nonce, no
/// amount and no attestation epoch makes it succeed. In particular it does
/// NOT depend on the amount being large, the destination being unusual, or
/// a policy being absent.
#[test]
fn no_combination_of_arguments_succeeds() {
    let (mut svm, signers, mint, authority) = paused_setup();
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let cases: [(u64, u64, u64); 5] = [
        (0, 1, 0),              // smallest everything
        (1, AMOUNT, 0),         // the ordinary case
        (u64::MAX, RESERVE, 0), // the whole reserve
        (7, AMOUNT, 1),         // a stale epoch
        (1 << 63, AMOUNT, 0),   // a refund-namespace nonce
    ];
    for (nonce, amount, epoch) in cases {
        let message =
            rebalance_withdraw_claim_message(epoch, nonce, amount, &destination_ata, &mint);
        let result = send_ixs(
            &mut svm,
            &[
                ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
                rebalance_withdraw_ix(
                    &authority.pubkey(),
                    &mint,
                    &destination_ata,
                    nonce,
                    amount,
                    epoch,
                ),
            ],
            &authority,
            &[],
        );
        assert!(
            result.is_err(),
            "nonce={nonce} amount={amount} epoch={epoch} must not succeed"
        );
    }

    assert_eq!(token_balance(&svm, &destination_ata), 0);
    let reserve = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve), RESERVE);
}

/// The retirement is checked BEFORE any state is touched, so a rejected
/// call leaves no replay-guard PDA behind. Otherwise stale tooling could
/// silently burn nonces a legitimate operator later needs.
#[test]
fn a_rejected_call_creates_no_withdrawal_record() {
    let (mut svm, signers, mint, authority) = paused_setup();
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);

    let message = rebalance_withdraw_claim_message(0, 42, AMOUNT, &destination_ata, &mint);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            rebalance_withdraw_ix(&authority.pubkey(), &mint, &destination_ata, 42, AMOUNT, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::RebalanceWithdrawRetired);

    assert!(
        svm.get_account(&rebalance_withdrawal_pda(42))
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "no record may be created by a retired instruction"
    );
}

/// Also fails when the bridge is NOT paused — the retirement short-circuits
/// ahead of every other check, so it can never be mistaken for one of them.
#[test]
fn fails_the_same_way_whether_or_not_the_bridge_is_paused() {
    let (mut svm, signers, mint, authority) = paused_setup();
    let destination = Keypair::new();
    let destination_ata = create_ata(&mut svm, &destination.pubkey(), &mint, 0);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .expect("unpause");

    let message = rebalance_withdraw_claim_message(0, 1, AMOUNT, &destination_ata, &mint);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            rebalance_withdraw_ix(&authority.pubkey(), &mint, &destination_ata, 1, AMOUNT, 0),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::RebalanceWithdrawRetired);
}
