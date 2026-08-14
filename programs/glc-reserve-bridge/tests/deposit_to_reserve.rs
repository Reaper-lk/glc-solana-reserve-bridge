//! Integration tests for `deposit_to_reserve`: the Solana-deposit ->
//! reserve path (the leg of the bridge that has no on-chain replay guard of
//! its own — docs/02-trust-model.md's asymmetry note — so what CAN be
//! verified on-chain, the deposit itself, is tested thoroughly here).

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::{LimitField, PauseScope};
use glc_reserve_bridge::state::WithdrawalStatus;

const AMOUNT: u64 = 5_000;
const GLC_ADDR: &[u8] = b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

#[test]
fn happy_path_deposits_and_records_obligation() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let user = Keypair::new();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT);

    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let ix = deposit_to_reserve_ix(
        &user.pubkey(),
        &mint,
        &user_ata,
        0,
        AMOUNT,
        GLC_ADDR.to_vec(),
    );
    send(&mut svm, ix, &user, &[]).expect("deposit should succeed");

    assert_eq!(token_balance(&svm, &user_ata), 0);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), 1_000_000 + AMOUNT);

    let obligation = get_obligation(&svm, 0);
    assert_eq!(obligation.amount, AMOUNT);
    assert_eq!(obligation.requester, user.pubkey());
    assert_eq!(obligation.status, WithdrawalStatus::Pending);
    assert_eq!(&obligation.glc_address[..GLC_ADDR.len()], GLC_ADDR);

    let config = get_config(&svm);
    assert_eq!(
        config.obligation_count, 1,
        "counter must advance exactly once"
    );
}

#[test]
fn second_deposit_gets_the_next_sequential_index() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT * 2);

    for expected_index in 0..2u64 {
        let ix = deposit_to_reserve_ix(
            &user.pubkey(),
            &mint,
            &user_ata,
            expected_index,
            AMOUNT,
            GLC_ADDR.to_vec(),
        );
        send(&mut svm, ix, &user, &[]).expect("deposit should succeed");
        let obligation = get_obligation(&svm, expected_index);
        assert_eq!(obligation.index, expected_index);
    }
}

#[test]
fn zero_length_address_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT);

    let ix = deposit_to_reserve_ix(&user.pubkey(), &mint, &user_ata, 0, AMOUNT, vec![]);
    let result = send(&mut svm, ix, &user, &[]);
    assert_bridge_error(result, BridgeError::InvalidGlcAddressLength);
}

#[test]
fn amount_above_per_transfer_limit_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let set_limit = set_limit_ix(
        &authority.pubkey(),
        LimitField::PerTransferLimit,
        AMOUNT - 1,
    );
    send(&mut svm, set_limit, &authority, &[]).expect("set_limit should succeed");

    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT);

    let ix = deposit_to_reserve_ix(
        &user.pubkey(),
        &mint,
        &user_ata,
        0,
        AMOUNT,
        GLC_ADDR.to_vec(),
    );
    let result = send(&mut svm, ix, &user, &[]);
    assert_bridge_error(result, BridgeError::ExceedsPerTransferLimit);
}

#[test]
fn globally_paused_bridge_rejects_deposit() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let pause = set_paused_ix(&authority.pubkey(), PauseScope::Global, true);
    send(&mut svm, pause, &authority, &[]).expect("pause should succeed");

    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT);
    let ix = deposit_to_reserve_ix(
        &user.pubkey(),
        &mint,
        &user_ata,
        0,
        AMOUNT,
        GLC_ADDR.to_vec(),
    );
    let result = send(&mut svm, ix, &user, &[]);
    assert_bridge_error(result, BridgeError::BridgeGloballyPaused);
}

#[test]
fn directionally_paused_deposit_leg_rejects_deposit() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let pause = set_paused_ix(&authority.pubkey(), PauseScope::Deposit, true);
    send(&mut svm, pause, &authority, &[]).expect("directional pause should succeed");

    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT);
    let ix = deposit_to_reserve_ix(
        &user.pubkey(),
        &mint,
        &user_ata,
        0,
        AMOUNT,
        GLC_ADDR.to_vec(),
    );
    let result = send(&mut svm, ix, &user, &[]);
    assert_bridge_error(result, BridgeError::DepositDirectionPaused);
}

#[test]
fn insufficient_user_balance_fails_at_the_token_program_not_a_partial_transfer() {
    // No custom program logic guards this — the SPL token program itself
    // refuses an over-amount transfer. Asserting it here documents that the
    // reserve model relies on that guarantee rather than re-implementing it.
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, AMOUNT - 1);

    let ix = deposit_to_reserve_ix(
        &user.pubkey(),
        &mint,
        &user_ata,
        0,
        AMOUNT,
        GLC_ADDR.to_vec(),
    );
    let result = send(&mut svm, ix, &user, &[]);
    assert!(result.is_err(), "over-balance transfer must fail");
    assert_eq!(
        token_balance(&svm, &user_ata),
        AMOUNT - 1,
        "balance untouched"
    );
}
