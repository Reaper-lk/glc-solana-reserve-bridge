//! Integration tests for `reset_rolling_volume_window`: the admin-gated
//! administrative override that lets an operator manually reopen one
//! direction's rolling 24h volume window after maintenance/refill, without
//! editing SQLite or fabricating a timestamp by hand
//! (docs/09-runbook.md's rolling-volume-window maintenance sequence).

mod common;

use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;
use glc_reserve_bridge::state::Direction;

const GLC_ADDR: &[u8] = b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

/// An initialized, reserve-funded bridge with the global pause already
/// engaged — the precondition `reset_rolling_volume_window` requires
/// before it will even attempt authorization.
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

/// Drives one real SolToGlc deposit through `deposit_to_reserve`, moving
/// real volume into the DEPOSIT window — the same real path the rolling
/// window is actually populated by in production, not a hand-fabricated
/// account value.
fn deposit(svm: &mut litesvm::LiteSVM, mint: &solana_sdk::pubkey::Pubkey, index: u64, amount: u64) {
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(svm, &user.pubkey(), mint, amount);
    let ix = deposit_to_reserve_ix(
        &user.pubkey(),
        mint,
        &user_ata,
        index,
        amount,
        GLC_ADDR.to_vec(),
    );
    send(svm, ix, &user, &[]).expect("deposit should succeed");
}

/// Drives one real GlcToSol release through `release_from_reserve`, moving
/// real volume into the RELEASE window.
fn release(
    svm: &mut litesvm::LiteSVM,
    signers: &[Keypair],
    mint: &solana_sdk::pubkey::Pubkey,
    authority: &Keypair,
    txid: [u8; 32],
    amount: u64,
) {
    let recipient = Keypair::new();
    let recipient_ata = create_ata(svm, &recipient.pubkey(), mint, 0);
    let message = release_claim_message(0, &txid, 0, amount, &recipient.pubkey(), mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let ix = release_from_reserve_ix(
        &authority.pubkey(),
        mint,
        &recipient.pubkey(),
        &recipient_ata,
        txid,
        0,
        amount,
        0,
    );
    send_ixs(svm, &[proof, ix], authority, &[]).expect("release should succeed");
}

#[test]
fn valid_admin_reset_of_glc_to_sol_release_window() {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000_000);
    // Unpause release only long enough to move real volume through it,
    // then re-pause globally before resetting — mirrors the real
    // maintenance sequence (pause -> ... -> reset -> ... -> unpause).
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();
    release(&mut svm, &signers, &mint, &authority, [0x11; 32], 5_000);
    let before = get_release_volume_window(&svm);
    assert_eq!(
        before.window_total, 5_000,
        "test setup: release volume must have moved"
    );

    // A fresh blockhash only (clock untouched): the second `pause(true)`
    // below is otherwise byte-identical to the one `paused_setup` already
    // sent, which litesvm would reject as `AlreadyProcessed` rather than a
    // genuine second pause.
    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("admin reset should succeed");

    let after = get_release_volume_window(&svm);
    assert_eq!(
        after.window_total, 0,
        "used volume must be zero after reset"
    );
    assert_eq!(
        after.window_start,
        svm.get_sysvar::<anchor_lang::solana_program::clock::Clock>()
            .unix_timestamp,
        "a fresh window must start from the current on-chain clock"
    );
    assert_eq!(
        after.direction,
        Direction::GoldcoinToSolana,
        "the account's own direction tag must be unchanged by the reset"
    );
}

#[test]
fn valid_admin_reset_of_sol_to_glc_deposit_window() {
    let (mut svm, _signers, mint, authority) = paused_setup(1_000_000_000);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();
    deposit(&mut svm, &mint, 0, 5_000);
    let before = get_deposit_volume_window(&svm);
    assert_eq!(
        before.window_total, 5_000,
        "test setup: deposit volume must have moved"
    );

    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::SolanaToGoldcoin),
        &authority,
        &[],
    )
    .expect("admin reset should succeed");

    let after = get_deposit_volume_window(&svm);
    assert_eq!(after.window_total, 0);
    assert_eq!(
        after.direction,
        Direction::SolanaToGoldcoin,
        "the account's own direction tag must be unchanged by the reset"
    );
}

#[test]
fn non_admin_signer_is_rejected() {
    let (mut svm, _signers, _mint, _authority) = paused_setup(1_000_000_000);
    let impostor = Keypair::new();
    svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let result = send(
        &mut svm,
        reset_rolling_volume_window_ix(&impostor.pubkey(), Direction::GoldcoinToSolana),
        &impostor,
        &[],
    );
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn reset_rejected_while_global_pause_is_false() {
    let authority = Keypair::new();
    let (mut svm, _signers, _mint) = setup_with_reserve(&authority, 1_000_000_000);
    // Deliberately NOT paused — `setup_with_reserve` leaves the bridge
    // unpaused by default (matching `initialize`'s own `paused = false`).

    let result = send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::BridgeNotPaused);
}

#[test]
fn reset_does_not_require_the_individual_direction_to_already_be_paused() {
    // Global pause is engaged (the actual precondition), but the
    // direction-specific `release_paused`/`deposit_paused` flags are left
    // at their default `false` — the reset must still succeed.
    let (mut svm, _signers, _mint, authority) = paused_setup(1_000_000_000);
    let config = get_config(&svm);
    assert!(
        !config.release_paused,
        "test setup: directional flag must be false"
    );

    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("reset must not require the individual direction to also be paused");
}

#[test]
fn resetting_one_direction_does_not_alter_the_other() {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000_000);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();
    deposit(&mut svm, &mint, 0, 3_000);
    release(&mut svm, &signers, &mint, &authority, [0x22; 32], 4_000);
    let deposit_before = get_deposit_volume_window(&svm);
    assert_eq!(deposit_before.window_total, 3_000);

    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("release reset should succeed");

    let release_after = get_release_volume_window(&svm);
    assert_eq!(
        release_after.window_total, 0,
        "release window must be reset"
    );
    let deposit_after = get_deposit_volume_window(&svm);
    assert_eq!(
        deposit_after.window_total, 3_000,
        "the untouched deposit window must be completely unaffected"
    );
    assert_eq!(deposit_after.window_start, deposit_before.window_start);
}

#[test]
fn rolling_limit_and_reserve_accounting_remain_unchanged() {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000_000);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();
    release(&mut svm, &signers, &mint, &authority, [0x33; 32], 5_000);
    let config_before = get_config(&svm);
    let reserve_balance_before = token_balance(
        &svm,
        &anchor_spl::associated_token::get_associated_token_address(
            &reserve_authority_pda(),
            &mint,
        ),
    );

    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("reset should succeed");

    let config_after = get_config(&svm);
    assert_eq!(
        config_after.rolling_volume_limit, config_before.rolling_volume_limit,
        "rolling_volume_limit must be untouched by a window reset"
    );
    assert_eq!(
        config_after.per_transfer_limit,
        config_before.per_transfer_limit
    );
    assert_eq!(
        config_after.protected_minimum,
        config_before.protected_minimum
    );
    assert_eq!(
        config_after.min_transfer_amount,
        config_before.min_transfer_amount
    );
    let reserve_balance_after = token_balance(
        &svm,
        &anchor_spl::associated_token::get_associated_token_address(
            &reserve_authority_pda(),
            &mint,
        ),
    );
    assert_eq!(
        reserve_balance_after, reserve_balance_before,
        "reserve balance must be untouched by a window reset"
    );
}

#[test]
fn remaining_capacity_returns_to_the_full_configured_rolling_limit_and_quota_is_no_longer_exhausted(
) {
    let (mut svm, signers, mint, authority) = paused_setup(1_000_000_000);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();
    // Exhaust the release window completely — a subsequent release of any
    // size would be refused for exceeding the rolling volume limit, the
    // exact "quota_exhausted" condition.
    release(
        &mut svm,
        &signers,
        &mint,
        &authority,
        [0x44; 32],
        DEFAULT_ROLLING_VOLUME_LIMIT,
    );
    let exhausted = get_release_volume_window(&svm);
    let config = get_config(&svm);
    assert_eq!(
        exhausted.window_total, config.rolling_volume_limit,
        "test setup: the window must be fully exhausted"
    );

    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("reset should succeed");

    let after = get_release_volume_window(&svm);
    let remaining = config.rolling_volume_limit - after.window_total;
    assert_eq!(
        remaining, config.rolling_volume_limit,
        "remaining must return to the full configured rolling_volume_limit"
    );
    assert!(
        after.window_total < config.rolling_volume_limit,
        "quota_exhausted (window_total >= rolling_volume_limit) must become false"
    );
}

#[test]
fn subsequent_volume_is_counted_normally_from_the_new_reset_state() {
    // Funded well above the rolling limit: the first release exhausts the
    // WINDOW, not the reserve, so the second (post-reset) release has
    // real balance to draw from.
    let (mut svm, signers, mint, authority) =
        paused_setup(DEFAULT_ROLLING_VOLUME_LIMIT + 1_000_000);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();
    release(
        &mut svm,
        &signers,
        &mint,
        &authority,
        [0x55; 32],
        DEFAULT_ROLLING_VOLUME_LIMIT,
    );

    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, true),
        &authority,
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .unwrap();
    svm.expire_blockhash();
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .unwrap();

    // Without the reset, this would exceed the rolling volume limit
    // (the window would already be fully exhausted) and be refused.
    release(&mut svm, &signers, &mint, &authority, [0x66; 32], 1_000);
    let window = get_release_volume_window(&svm);
    assert_eq!(
        window.window_total, 1_000,
        "volume after a reset must be counted from zero, not appended to the old total"
    );
}

#[test]
fn repeated_reset_while_globally_paused_is_deterministic() {
    let (mut svm, _signers, _mint, authority) = paused_setup(1_000_000_000);

    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("first reset should succeed");
    let first = get_release_volume_window(&svm);

    // A fresh blockhash only — the clock is untouched, so `window_start`
    // below must come out identical; without this the second,
    // byte-identical instruction would be rejected as `AlreadyProcessed`
    // rather than genuinely re-executed.
    svm.expire_blockhash();
    send(
        &mut svm,
        reset_rolling_volume_window_ix(&authority.pubkey(), Direction::GoldcoinToSolana),
        &authority,
        &[],
    )
    .expect("a second reset while still paused should also succeed, deterministically");
    let second = get_release_volume_window(&svm);

    assert_eq!(first.window_total, 0);
    assert_eq!(second.window_total, 0);
    assert_eq!(
        first.window_start, second.window_start,
        "the clock did not advance between calls in this test, so both resets must \
         produce the identical window_start — no hidden accumulation or drift"
    );
}
