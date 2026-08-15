//! Adversarial Token-2022 test matrix (Task 4, docs/18-token-2022-support.md):
//! `initialize_reserve_vault`'s own validation (never exercised by
//! `setup_with_reserve`'s config-patching shortcut used elsewhere in this
//! test suite) and the pinned-token-program invariant it establishes.
//! Uses real, TLV-correct Token-2022 mint/account bytes
//! (`common::write_token2022_mint`/`write_token2022_token_account`) against
//! litesvm's bundled, genuinely-executable `spl_token_2022` program
//! (`LiteSVM::new()` -> `with_spl_programs()`), not a legacy-shaped stand-in.

mod common;

use anchor_spl::token_interface::spl_token_2022;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;

const SUPPLY: u64 = 1_000_000_000;
const DECIMALS: u8 = 6;

#[test]
fn initializes_with_a_real_token_2022_mint_carrying_only_supported_extensions() {
    let authority = Keypair::new();
    let (mut svm, _signers) = setup_initialized_two_of_three(&authority);

    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(
        &mut svm,
        &mint,
        SUPPLY,
        DECIMALS,
        &[spl_token_2022::extension::ExtensionType::MetadataPointer],
    );

    let ix = initialize_reserve_vault_ix(&authority.pubkey(), &mint, &spl_token_2022::ID);
    send(&mut svm, ix, &authority, &[]).expect("Token-2022 vault init with a benign extension");

    let config = get_config(&svm);
    assert_eq!(config.reserve_token_mint, mint);
    assert_eq!(config.reserve_token_program, spl_token_2022::ID);
}

#[test]
fn initializes_with_a_real_legacy_spl_mint_and_records_the_legacy_program() {
    let authority = Keypair::new();
    let (mut svm, _signers) = setup_initialized_two_of_three(&authority);

    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_mint_with_decimals(&mut svm, &mint, SUPPLY, DECIMALS);

    let ix = initialize_reserve_vault_ix(&authority.pubkey(), &mint, &anchor_spl::token::ID);
    send(&mut svm, ix, &authority, &[]).expect("legacy SPL Token vault init");

    let config = get_config(&svm);
    assert_eq!(config.reserve_token_mint, mint);
    assert_eq!(
        config.reserve_token_program,
        anchor_spl::token::ID,
        "the bridge supports EITHER legitimate SPL token program, whichever the admin's real \
         mint actually belongs to — never assumed"
    );
}

#[test]
fn rejects_a_token_2022_mint_carrying_an_unsupported_extension() {
    let authority = Keypair::new();
    let (mut svm, _signers) = setup_initialized_two_of_three(&authority);

    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(
        &mut svm,
        &mint,
        SUPPLY,
        DECIMALS,
        &[spl_token_2022::extension::ExtensionType::TransferFeeConfig],
    );

    let ix = initialize_reserve_vault_ix(&authority.pubkey(), &mint, &spl_token_2022::ID);
    let result = send(&mut svm, ix, &authority, &[]);
    assert_bridge_error(result, BridgeError::UnsupportedTokenExtension);
}

#[test]
fn rejects_a_token_2022_mint_carrying_both_a_supported_and_an_unsupported_extension() {
    // A benign extension being present must not mask an unsafe one on the
    // same mint — the canonical GLC mint's real extension set alone is not
    // sufficient to pass; every extension present must be reviewed.
    let authority = Keypair::new();
    let (mut svm, _signers) = setup_initialized_two_of_three(&authority);

    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(
        &mut svm,
        &mint,
        SUPPLY,
        DECIMALS,
        &[
            spl_token_2022::extension::ExtensionType::MetadataPointer,
            spl_token_2022::extension::ExtensionType::TransferFeeConfig,
        ],
    );

    let ix = initialize_reserve_vault_ix(&authority.pubkey(), &mint, &spl_token_2022::ID);
    let result = send(&mut svm, ix, &authority, &[]);
    assert_bridge_error(result, BridgeError::UnsupportedTokenExtension);
}

#[test]
fn rejects_vault_init_when_signer_is_not_admin() {
    let authority = Keypair::new();
    let (mut svm, _signers) = setup_initialized_two_of_three(&authority);
    let not_admin = Keypair::new();
    svm.airdrop(&not_admin.pubkey(), 10_000_000_000).unwrap();

    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(&mut svm, &mint, SUPPLY, DECIMALS, &[]);

    let ix = initialize_reserve_vault_ix(&not_admin.pubkey(), &mint, &spl_token_2022::ID);
    let result = send(&mut svm, ix, &not_admin, &[]);
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn rejects_reinitializing_an_already_configured_vault() {
    let authority = Keypair::new();
    let (mut svm, _signers) = setup_initialized_two_of_three(&authority);

    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(&mut svm, &mint, SUPPLY, DECIMALS, &[]);
    let ix = initialize_reserve_vault_ix(&authority.pubkey(), &mint, &spl_token_2022::ID);
    send(&mut svm, ix, &authority, &[]).expect("first vault init must succeed");

    let other_mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(&mut svm, &other_mint, SUPPLY, DECIMALS, &[]);
    let ix2 = initialize_reserve_vault_ix(&authority.pubkey(), &other_mint, &spl_token_2022::ID);
    let result = send(&mut svm, ix2, &authority, &[]);
    assert_bridge_error(result, BridgeError::ReserveAlreadyConfigured);
}

/// Sets up a fully real, on-chain-configured Token-2022 reserve vault (via
/// the actual `initialize_reserve_vault` instruction, not the config-patch
/// shortcut) with `reserve_balance` already funded, and a real user ATA
/// holding `user_balance`. Returns `(svm, signers, mint, user, user_ata)`.
fn setup_real_token2022_reserve(
    authority: &Keypair,
    reserve_balance: u64,
    user: &Keypair,
    user_balance: u64,
) -> (litesvm::LiteSVM, Vec<Keypair>, solana_sdk::pubkey::Pubkey) {
    let (mut svm, signers) = setup_initialized_two_of_three(authority);
    let mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(
        &mut svm,
        &mint,
        SUPPLY,
        DECIMALS,
        &[spl_token_2022::extension::ExtensionType::MetadataPointer],
    );
    let ix = initialize_reserve_vault_ix(&authority.pubkey(), &mint, &spl_token_2022::ID);
    send(&mut svm, ix, authority, &[]).expect("Token-2022 vault init");

    write_token2022_token_account(
        &mut svm,
        &token2022_ata_address(&reserve_authority_pda(), &mint),
        &reserve_authority_pda(),
        &mint,
        reserve_balance,
    );
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    create_token2022_ata(&mut svm, &user.pubkey(), &mint, user_balance);

    (svm, signers, mint)
}

#[test]
fn token_2022_deposit_and_release_settle_1_to_1_end_to_end() {
    let authority = Keypair::new();
    let user = Keypair::new();
    let (mut svm, signers, mint) =
        setup_real_token2022_reserve(&authority, 1_000_000, &user, 5_000);

    let user_ata = token2022_ata_address(&user.pubkey(), &mint);
    let deposit_ix = deposit_to_reserve_ix_with_token_program(
        &user.pubkey(),
        &mint,
        &spl_token_2022::ID,
        &user_ata,
        0,
        5_000,
        b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef".to_vec(),
    );
    send(&mut svm, deposit_ix, &user, &[]).expect("real Token-2022 deposit must succeed");
    assert_eq!(token2022_balance(&svm, &user_ata), 0);
    let reserve_ata = token2022_ata_address(&reserve_authority_pda(), &mint);
    assert_eq!(token2022_balance(&svm, &reserve_ata), 1_000_000 + 5_000);

    let recipient = Keypair::new();
    let recipient_ata = create_token2022_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let txid = [0x11u8; 32];
    let vout = 0u32;
    let amount = 3_000u64;
    let message = release_claim_message(0, &txid, vout, amount, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&signers.iter().take(2).collect::<Vec<_>>(), &message);
    let release_ix = release_from_reserve_ix_with_token_program(
        &authority.pubkey(),
        &mint,
        &spl_token_2022::ID,
        &recipient.pubkey(),
        &recipient_ata,
        txid,
        vout,
        amount,
        0,
    );
    send_ixs(&mut svm, &[proof, release_ix], &authority, &[])
        .expect("real Token-2022 release must succeed");
    assert_eq!(token2022_balance(&svm, &recipient_ata), amount);
}

#[test]
fn release_rejects_a_token_program_that_does_not_match_the_configured_one() {
    let authority = Keypair::new();
    let recipient = Keypair::new();
    let (mut svm, signers, mint) =
        setup_real_token2022_reserve(&authority, 1_000_000, &recipient, 0);
    let recipient_ata = token2022_ata_address(&recipient.pubkey(), &mint);

    let txid = [0x22u8; 32];
    let vout = 0u32;
    let amount = 3_000u64;
    let message = release_claim_message(0, &txid, vout, amount, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&signers.iter().take(2).collect::<Vec<_>>(), &message);
    // The reserve was configured under Token-2022; substituting the OTHER
    // legitimate SPL program must be rejected, not silently accepted.
    let release_ix = release_from_reserve_ix_with_token_program(
        &authority.pubkey(),
        &mint,
        &anchor_spl::token::ID,
        &recipient.pubkey(),
        &recipient_ata,
        txid,
        vout,
        amount,
        0,
    );
    let result = send_ixs(&mut svm, &[proof, release_ix], &authority, &[]);
    assert!(
        result.is_err(),
        "substituting the wrong token program must never succeed"
    );
}

#[test]
fn deposit_rejects_a_token_program_that_does_not_match_the_configured_one() {
    let authority = Keypair::new();
    let user = Keypair::new();
    let (mut svm, _signers, mint) =
        setup_real_token2022_reserve(&authority, 1_000_000, &user, 5_000);
    let user_ata = token2022_ata_address(&user.pubkey(), &mint);

    let deposit_ix = deposit_to_reserve_ix_with_token_program(
        &user.pubkey(),
        &mint,
        &anchor_spl::token::ID,
        &user_ata,
        0,
        5_000,
        b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef".to_vec(),
    );
    let result = send(&mut svm, deposit_ix, &user, &[]);
    assert!(
        result.is_err(),
        "substituting the wrong token program must never succeed"
    );
}

#[test]
fn release_rejects_the_wrong_reserve_mint() {
    let authority = Keypair::new();
    let recipient = Keypair::new();
    let (mut svm, signers, mint) =
        setup_real_token2022_reserve(&authority, 1_000_000, &recipient, 0);

    // A second, entirely real, valid Token-2022 mint — never configured as
    // this bridge's reserve.
    let wrong_mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(&mut svm, &wrong_mint, SUPPLY, DECIMALS, &[]);
    let recipient_ata = create_token2022_ata(&mut svm, &recipient.pubkey(), &wrong_mint, 0);

    let txid = [0x33u8; 32];
    let vout = 0u32;
    let amount = 1_000u64;
    let message = release_claim_message(0, &txid, vout, amount, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&signers.iter().take(2).collect::<Vec<_>>(), &message);
    let release_ix = release_from_reserve_ix_with_token_program(
        &authority.pubkey(),
        &wrong_mint,
        &spl_token_2022::ID,
        &recipient.pubkey(),
        &recipient_ata,
        txid,
        vout,
        amount,
        0,
    );
    let result = send_ixs(&mut svm, &[proof, release_ix], &authority, &[]);
    assert!(
        result.is_err(),
        "a real but unconfigured mint must never be accepted as the reserve mint"
    );
}

#[test]
fn deposit_rejects_a_token_account_belonging_to_a_different_mint() {
    let authority = Keypair::new();
    let user = Keypair::new();
    let (mut svm, _signers, mint) =
        setup_real_token2022_reserve(&authority, 1_000_000, &user, 5_000);

    // A real Token-2022 account, but for a DIFFERENT mint than the reserve.
    let other_mint = solana_sdk::pubkey::Pubkey::new_unique();
    write_token2022_mint(&mut svm, &other_mint, SUPPLY, DECIMALS, &[]);
    let wrong_user_ata = create_token2022_ata(&mut svm, &user.pubkey(), &other_mint, 5_000);

    let deposit_ix = deposit_to_reserve_ix_with_token_program(
        &user.pubkey(),
        &mint,
        &spl_token_2022::ID,
        &wrong_user_ata,
        0,
        5_000,
        b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef".to_vec(),
    );
    let result = send(&mut svm, deposit_ix, &user, &[]);
    assert!(
        result.is_err(),
        "a token account for a different mint must never be accepted as the reserve token account"
    );
}
