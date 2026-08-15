//! Shared litesvm harness for all integration test files. Adapted from the
//! old bridge's `programs/glc-bridge/tests/common/mod.rs`
//! (docs/01-reuse-inventory.md: the litesvm setup/ed25519-precompile-builder
//! pattern is authority-agnostic and directly reusable) — same mechanics,
//! rewired to this program's accounts/instructions.
#![allow(dead_code)]

use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::{AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas};
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    bpf_loader_upgradeable::{self, UpgradeableLoaderState},
    ed25519_program,
    instruction::{Instruction, InstructionError},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};

use glc_reserve_bridge::constants::{
    GLC_DECIMALS, PROTOCOL_VERSION, SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG,
    SEED_DEPOSIT_CLAIM, SEED_GOVERNANCE_ACTION, SEED_RESERVE_AUTHORITY, SEED_ROLLING_VOLUME_WINDOW,
    SEED_WITHDRAWAL_OBLIGATION,
};
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::{LimitField, PauseScope};
use glc_reserve_bridge::state::{
    AttestationKeySet, BridgeConfig, DepositClaim, WithdrawalObligation,
};

// ---------------------------------------------------------------- harness --

pub fn program_bytes() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/glc_reserve_bridge.so"
    );
    std::fs::read(path)
        .expect("target/deploy/glc_reserve_bridge.so missing — run `anchor build` first")
}

pub fn programdata_address() -> Pubkey {
    Pubkey::find_program_address(
        &[glc_reserve_bridge::ID.as_ref()],
        &bpf_loader_upgradeable::id(),
    )
    .0
}

pub fn config_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_BRIDGE_CONFIG], &glc_reserve_bridge::ID).0
}

pub fn attestation_key_set_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_ATTESTATION_KEY_SET], &glc_reserve_bridge::ID).0
}

pub fn release_volume_window_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_ROLLING_VOLUME_WINDOW, &[0u8]],
        &glc_reserve_bridge::ID,
    )
    .0
}

pub fn deposit_volume_window_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_ROLLING_VOLUME_WINDOW, &[1u8]],
        &glc_reserve_bridge::ID,
    )
    .0
}

pub fn reserve_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_RESERVE_AUTHORITY], &glc_reserve_bridge::ID).0
}

pub fn governance_action_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_GOVERNANCE_ACTION], &glc_reserve_bridge::ID).0
}

pub fn claim_pda(txid: &[u8; 32], vout: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_DEPOSIT_CLAIM, txid.as_ref(), &vout.to_le_bytes()],
        &glc_reserve_bridge::ID,
    )
    .0
}

pub fn obligation_pda(index: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_WITHDRAWAL_OBLIGATION, &index.to_le_bytes()],
        &glc_reserve_bridge::ID,
    )
    .0
}

pub fn programdata_account(upgrade_authority: Option<Pubkey>, elf: &[u8]) -> Account {
    let mut data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: upgrade_authority,
    })
    .unwrap();
    data.resize(UpgradeableLoaderState::size_of_programdata_metadata(), 0);
    data.extend_from_slice(elf);
    Account {
        lamports: 10_000_000_000,
        data,
        owner: bpf_loader_upgradeable::id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// Fresh VM with the program installed as upgradeable and `authority` as its
/// upgrade authority (funded).
pub fn setup(authority: &Keypair) -> LiteSVM {
    let mut svm = LiteSVM::new();
    svm.airdrop(&authority.pubkey(), 100_000_000_000).unwrap();

    let elf = program_bytes();
    svm.set_account(
        programdata_address(),
        programdata_account(Some(authority.pubkey()), &elf),
    )
    .unwrap();
    let program_state = bincode::serialize(&UpgradeableLoaderState::Program {
        programdata_address: programdata_address(),
    })
    .unwrap();
    svm.set_account(
        glc_reserve_bridge::ID,
        Account {
            lamports: 1_000_000_000,
            data: program_state,
            owner: bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: 0,
        },
    )
    .unwrap();
    svm
}

pub fn keys(n: usize) -> Vec<Keypair> {
    (0..n).map(|_| Keypair::new()).collect()
}

pub fn pubkeys(ks: &[Keypair]) -> Vec<Pubkey> {
    ks.iter().map(|k| k.pubkey()).collect()
}

#[allow(clippy::result_large_err)]
pub fn send(
    svm: &mut LiteSVM,
    ix: Instruction,
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    send_ixs(svm, &[ix], payer, extra_signers)
}

#[allow(clippy::result_large_err)]
pub fn send_ixs(
    svm: &mut LiteSVM,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&payer.pubkey()),
        &signers,
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx)
}

pub fn expected_code(e: BridgeError) -> u32 {
    match anchor_lang::error::Error::from(e) {
        anchor_lang::error::Error::AnchorError(ae) => ae.error_code_number,
        _ => unreachable!(),
    }
}

#[track_caller]
pub fn assert_bridge_error(
    result: Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata>,
    expected: BridgeError,
) {
    let err = result.expect_err("transaction should have failed").err;
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(code, expected_code(expected), "wrong custom error code")
        }
        other => panic!("expected custom program error, got {other:?}"),
    }
}

pub fn warp_seconds(svm: &mut LiteSVM, seconds: i64) {
    let mut clock = svm.get_sysvar::<anchor_lang::solana_program::clock::Clock>();
    clock.unix_timestamp += seconds;
    svm.set_sysvar(&clock);
    svm.expire_blockhash();
}

// ------------------------------------------------------------- initialize --

pub const DEFAULT_TEST_TIMELOCK: i64 = 3_600;
pub const DEFAULT_MIN_TRANSFER: u64 = 100;
pub const DEFAULT_PER_TRANSFER_LIMIT: u64 = 1_000_000_000;
pub const DEFAULT_PROTECTED_MINIMUM: u64 = 0;
pub const DEFAULT_ROLLING_VOLUME_LIMIT: u64 = 1_000_000_000;
pub const DEFAULT_ROLLING_WINDOW_SECONDS: i64 = 3_600;

#[allow(clippy::too_many_arguments)]
pub fn initialize_ix_full(
    authority: &Pubkey,
    program_data: Pubkey,
    attestation_keys: Vec<Pubkey>,
    threshold: u8,
    per_transfer_limit: u64,
    protected_minimum: u64,
    rolling_volume_limit: u64,
) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::Initialize {
            authority: *authority,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            release_volume_window: release_volume_window_pda(),
            deposit_volume_window: deposit_volume_window_pda(),
            program: glc_reserve_bridge::ID,
            program_data,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::Initialize {
            attestation_keys,
            threshold,
            governance_timelock_seconds: DEFAULT_TEST_TIMELOCK,
            min_transfer_amount: DEFAULT_MIN_TRANSFER,
            per_transfer_limit,
            protected_minimum,
            rolling_volume_limit,
            rolling_window_seconds: DEFAULT_ROLLING_WINDOW_SECONDS,
        }
        .data(),
    }
}

pub fn initialize_ix(
    authority: &Pubkey,
    program_data: Pubkey,
    attestation_keys: Vec<Pubkey>,
    threshold: u8,
) -> Instruction {
    initialize_ix_full(
        authority,
        program_data,
        attestation_keys,
        threshold,
        DEFAULT_PER_TRANSFER_LIMIT,
        DEFAULT_PROTECTED_MINIMUM,
        DEFAULT_ROLLING_VOLUME_LIMIT,
    )
}

/// Initialized bridge with `authority` as upgrade authority/admin and the
/// given attestation keypairs at `threshold`.
pub fn setup_initialized_with(
    authority: &Keypair,
    attestation_keys: Vec<Pubkey>,
    threshold: u8,
) -> LiteSVM {
    let mut svm = setup(authority);
    let ix = initialize_ix(
        &authority.pubkey(),
        programdata_address(),
        attestation_keys,
        threshold,
    );
    send(&mut svm, ix, authority, &[]).expect("initialize should succeed");
    svm
}

/// The approved trust model's starting point: 3 attestation keys, 2-of-3
/// threshold (docs/02-trust-model.md).
pub fn setup_initialized_two_of_three(authority: &Keypair) -> (LiteSVM, Vec<Keypair>) {
    let signers = keys(3);
    let svm = setup_initialized_with(authority, pubkeys(&signers), 2);
    (svm, signers)
}

pub fn get_config(svm: &LiteSVM) -> BridgeConfig {
    let account = svm.get_account(&config_pda()).expect("config must exist");
    BridgeConfig::try_deserialize(&mut account.data.as_slice()).unwrap()
}

pub fn get_attestation_key_set(svm: &LiteSVM) -> AttestationKeySet {
    let account = svm
        .get_account(&attestation_key_set_pda())
        .expect("attestation key set must exist");
    AttestationKeySet::try_deserialize(&mut account.data.as_slice()).unwrap()
}

// -------------------------------------------------------------------- SPL --

/// Writes a packed, initialized SPL token account at `address` with a
/// chosen starting `amount` — used to pre-create ATAs (at the derived ATA
/// address) and to fund the reserve/user accounts directly, without
/// invoking the token or ATA programs (same fabrication approach the old
/// bridge's harness used).
pub fn write_token_account(
    svm: &mut LiteSVM,
    address: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    amount: u64,
) {
    let state = spl_token::state::Account {
        mint: *mint,
        owner: *wallet,
        amount,
        delegate: None.into(),
        state: spl_token::state::AccountState::Initialized,
        is_native: None.into(),
        delegated_amount: 0,
        close_authority: None.into(),
    };
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(state, &mut data).unwrap();
    svm.set_account(
        *address,
        Account {
            lamports: 10_000_000,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// Pre-creates the wallet's ATA for `mint` at the given starting `amount`
/// and returns its address.
pub fn create_ata(svm: &mut LiteSVM, wallet: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
    let ata = get_associated_token_address(wallet, mint);
    write_token_account(svm, &ata, wallet, mint, amount);
    ata
}

/// Writes a packed SPL mint account at `address` — the existing Solana GLC
/// mint this bridge holds reserves of is never created by this program
/// (docs/12-management-decisions.md item 10), so tests fabricate one
/// directly rather than running a create-mint instruction that doesn't
/// exist in this program's surface.
pub fn write_mint(svm: &mut LiteSVM, address: &Pubkey, supply: u64) {
    write_mint_with_decimals(svm, address, supply, GLC_DECIMALS)
}

/// Same as [`write_mint`], but with an explicit `decimals` rather than
/// this test suite's own fixture default — see
/// `release_from_reserve::release_uses_the_real_mints_decimals_not_a_hardcoded_constant`,
/// which pins that the program reads decimals from the mint account
/// itself (docs/16-p0-checkpoint.md: the real production Solana GLC mint
/// uses 6 decimals, not the 8 this program's `transfer_checked` calls
/// used to hardcode).
pub fn write_mint_with_decimals(svm: &mut LiteSVM, address: &Pubkey, supply: u64, decimals: u8) {
    let state = spl_token::state::Mint {
        mint_authority: None.into(),
        supply,
        decimals,
        is_initialized: true,
        freeze_authority: None.into(),
    };
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    spl_token::state::Mint::pack(state, &mut data).unwrap();
    svm.set_account(
        *address,
        Account {
            lamports: 10_000_000,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

pub fn token_balance(svm: &LiteSVM, token_account: &Pubkey) -> u64 {
    let account = svm.get_account(token_account).expect("token account");
    spl_token::state::Account::unpack(&account.data)
        .unwrap()
        .amount
}

pub fn get_claim(svm: &LiteSVM, txid: &[u8; 32], vout: u32) -> DepositClaim {
    let account = svm
        .get_account(&claim_pda(txid, vout))
        .expect("claim must exist");
    DepositClaim::try_deserialize(&mut account.data.as_slice()).unwrap()
}

pub fn get_obligation(svm: &LiteSVM, index: u64) -> WithdrawalObligation {
    let account = svm
        .get_account(&obligation_pda(index))
        .expect("obligation must exist");
    WithdrawalObligation::try_deserialize(&mut account.data.as_slice()).unwrap()
}

/// Sets up an initialized bridge (2-of-3), a fabricated reserve mint, and a
/// reserve vault funded at `reserve_balance`. Returns (svm, signers, mint).
pub fn setup_with_reserve(
    authority: &Keypair,
    reserve_balance: u64,
) -> (LiteSVM, Vec<Keypair>, Pubkey) {
    let (mut svm, signers) = setup_initialized_two_of_three(authority);
    let mint = Pubkey::new_unique();
    write_mint(&mut svm, &mint, reserve_balance);

    // Patch BridgeConfig directly to record the reserve mint/authority bump
    // — equivalent in effect to `initialize_reserve_vault`, but avoids
    // depending on litesvm's ATA-program CPI path purely for account
    // bookkeeping that `write_token_account` already fabricates below.
    let mut config = get_config(&svm);
    config.reserve_token_mint = mint;
    config.reserve_token_program = spl_token::ID;
    let (_, bump) =
        Pubkey::find_program_address(&[SEED_RESERVE_AUTHORITY], &glc_reserve_bridge::ID);
    config.reserve_authority_bump = bump;
    let mut data = Vec::new();
    config.try_serialize(&mut data).unwrap();
    let mut account = svm.get_account(&config_pda()).unwrap();
    account.data = data;
    svm.set_account(config_pda(), account).unwrap();

    let reserve_ata = create_ata(&mut svm, &reserve_authority_pda(), &mint, reserve_balance);
    let _ = reserve_ata;
    (svm, signers, mint)
}

/// Same as [`setup_with_reserve`], but with an explicit mint `decimals`
/// rather than this test suite's own fixture default.
pub fn setup_with_reserve_and_decimals(
    authority: &Keypair,
    reserve_balance: u64,
    decimals: u8,
) -> (LiteSVM, Vec<Keypair>, Pubkey) {
    let (mut svm, signers) = setup_initialized_two_of_three(authority);
    let mint = Pubkey::new_unique();
    write_mint_with_decimals(&mut svm, &mint, reserve_balance, decimals);

    let mut config = get_config(&svm);
    config.reserve_token_mint = mint;
    config.reserve_token_program = spl_token::ID;
    let (_, bump) =
        Pubkey::find_program_address(&[SEED_RESERVE_AUTHORITY], &glc_reserve_bridge::ID);
    config.reserve_authority_bump = bump;
    let mut data = Vec::new();
    config.try_serialize(&mut data).unwrap();
    let mut account = svm.get_account(&config_pda()).unwrap();
    account.data = data;
    svm.set_account(config_pda(), account).unwrap();

    let reserve_ata = create_ata(&mut svm, &reserve_authority_pda(), &mint, reserve_balance);
    let _ = reserve_ata;
    (svm, signers, mint)
}

// -------------------------------------------------------------- ed25519 --

/// Builds an ed25519-precompile instruction: one shared message, one entry
/// per signer, all offsets self-referential (`u16::MAX`), matching what a
/// production attestation-signer client will produce.
pub fn ed25519_proof_ix(signers: &[&Keypair], message: &[u8]) -> Instruction {
    ed25519_proof_ix_with_index(signers, message, u16::MAX)
}

pub fn ed25519_proof_ix_with_index(
    signers: &[&Keypair],
    message: &[u8],
    ix_index: u16,
) -> Instruction {
    let n = signers.len();
    let entries_end = 2 + n * 14;
    let keys_end = entries_end + n * 32;
    let sigs_end = keys_end + n * 64;
    let msg_off = sigs_end;
    let mut data = vec![0u8; msg_off + message.len()];
    data[0] = n as u8;
    data[1] = 0;
    for (i, kp) in signers.iter().enumerate() {
        let base = 2 + i * 14;
        let pk_off = (entries_end + i * 32) as u16;
        let sig_off = (keys_end + i * 64) as u16;
        data[base..base + 2].copy_from_slice(&sig_off.to_le_bytes());
        data[base + 2..base + 4].copy_from_slice(&ix_index.to_le_bytes());
        data[base + 4..base + 6].copy_from_slice(&pk_off.to_le_bytes());
        data[base + 6..base + 8].copy_from_slice(&ix_index.to_le_bytes());
        data[base + 8..base + 10].copy_from_slice(&(msg_off as u16).to_le_bytes());
        data[base + 10..base + 12].copy_from_slice(&(message.len() as u16).to_le_bytes());
        data[base + 12..base + 14].copy_from_slice(&ix_index.to_le_bytes());
        let pk_pos = entries_end + i * 32;
        data[pk_pos..pk_pos + 32].copy_from_slice(kp.pubkey().as_ref());
        let sig = kp.sign_message(message);
        let sig_pos = keys_end + i * 64;
        data[sig_pos..sig_pos + 64].copy_from_slice(sig.as_ref());
    }
    data[msg_off..].copy_from_slice(message);
    Instruction {
        program_id: ed25519_program::id(),
        accounts: vec![],
        data,
    }
}

pub fn release_claim_message(
    epoch: u64,
    txid: &[u8; 32],
    vout: u32,
    amount: u64,
    recipient: &Pubkey,
    reserve_mint: &Pubkey,
) -> Vec<u8> {
    glc_reserve_bridge_shared::claim::release_claim_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        txid,
        vout,
        amount,
        &recipient.to_bytes(),
        &reserve_mint.to_bytes(),
    )
    .to_vec()
}

// -------------------------------------------------- release_from_reserve --

#[allow(clippy::too_many_arguments)]
pub fn release_from_reserve_ix(
    submitter: &Pubkey,
    reserve_mint: &Pubkey,
    recipient: &Pubkey,
    recipient_token_account: &Pubkey,
    txid: [u8; 32],
    vout: u32,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_token_account =
        get_associated_token_address(&reserve_authority_pda(), reserve_mint);
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ReleaseFromReserve {
            submitter: *submitter,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            deposit_claim: claim_pda(&txid, vout),
            release_volume_window: release_volume_window_pda(),
            reserve_mint: *reserve_mint,
            reserve_authority: reserve_authority_pda(),
            reserve_token_account,
            recipient: *recipient,
            recipient_token_account: *recipient_token_account,
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: spl_token::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ReleaseFromReserve {
            txid,
            vout,
            amount,
            attestation_epoch,
        }
        .data(),
    }
}

// ----------------------------------------------------- deposit_to_reserve --

pub fn deposit_to_reserve_ix(
    user: &Pubkey,
    reserve_mint: &Pubkey,
    user_token_account: &Pubkey,
    obligation_index: u64,
    amount: u64,
    glc_address: Vec<u8>,
) -> Instruction {
    let reserve_token_account =
        get_associated_token_address(&reserve_authority_pda(), reserve_mint);
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::DepositToReserve {
            user: *user,
            bridge_config: config_pda(),
            deposit_volume_window: deposit_volume_window_pda(),
            reserve_mint: *reserve_mint,
            user_token_account: *user_token_account,
            reserve_authority: reserve_authority_pda(),
            reserve_token_account,
            withdrawal_obligation: obligation_pda(obligation_index),
            token_program: spl_token::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::DepositToReserve {
            amount,
            glc_address,
        }
        .data(),
    }
}

// ------------------------------------------------------------------ admin --

pub fn admin_config_metas(admin: &Pubkey) -> Vec<solana_sdk::instruction::AccountMeta> {
    glc_reserve_bridge::accounts::AdminConfig {
        admin: *admin,
        bridge_config: config_pda(),
    }
    .to_account_metas(None)
}

pub fn set_paused_ix(admin: &Pubkey, scope: PauseScope, paused: bool) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: admin_config_metas(admin),
        data: glc_reserve_bridge::instruction::SetPaused { scope, paused }.data(),
    }
}

pub fn set_limit_ix(admin: &Pubkey, field: LimitField, new_value: u64) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: admin_config_metas(admin),
        data: glc_reserve_bridge::instruction::SetLimit { field, new_value }.data(),
    }
}

// ------------------------------------------------------------ governance --

pub fn rotation_message(epoch: u64, proposed_keys: &[Pubkey], threshold: u8) -> Vec<u8> {
    let raw: Vec<[u8; 32]> = proposed_keys.iter().map(|v| v.to_bytes()).collect();
    let commitment = anchor_lang::solana_program::hash::hash(
        &glc_reserve_bridge_shared::governance::rotation_params(threshold, &raw),
    )
    .to_bytes();
    glc_reserve_bridge_shared::governance::governance_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        glc_reserve_bridge_shared::governance::ACTION_PROPOSE_ROTATION,
        &commitment,
    )
    .to_vec()
}

pub fn propose_rotation_ix(
    proposer: &Pubkey,
    proposed_keys: Vec<Pubkey>,
    threshold: u8,
) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ProposeGovernanceAction {
            proposer: *proposer,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            pending_action: governance_action_pda(),
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ProposeAttestationKeyRotation {
            keys: proposed_keys,
            threshold,
        }
        .data(),
    }
}

pub fn execute_rotation_ix(executor: &Pubkey) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ExecuteGovernanceAction {
            executor: *executor,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            pending_action: governance_action_pda(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ExecuteAttestationKeyRotation {}.data(),
    }
}
