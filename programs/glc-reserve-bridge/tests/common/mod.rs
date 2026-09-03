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
use anchor_spl::token_interface::spl_token_2022;
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
    GOLDCOIN_DECIMALS, PROTOCOL_VERSION, SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG,
    SEED_DEPOSIT_CLAIM, SEED_GOVERNANCE_ACTION, SEED_PENDING_REBALANCE_POLICY,
    SEED_PENDING_UPGRADE, SEED_REBALANCE_POLICY, SEED_REBALANCE_WITHDRAWAL, SEED_RESERVE_AUTHORITY,
    SEED_ROLLING_VOLUME_WINDOW, SEED_UPGRADE_AUTHORITY, SEED_WITHDRAWAL_OBLIGATION,
};
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::{LimitField, PauseScope};
use glc_reserve_bridge::state::{
    AttestationKeySet, BridgeConfig, DepositClaim, Direction, PendingRebalancePolicy,
    RebalancePolicy, RebalanceWithdrawal, RollingVolumeWindow, WithdrawalObligation,
    WithdrawalStatus,
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

pub fn upgrade_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_UPGRADE_AUTHORITY], &glc_reserve_bridge::ID).0
}

pub fn rebalance_policy_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_REBALANCE_POLICY], &glc_reserve_bridge::ID).0
}

pub fn pending_rebalance_policy_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_PENDING_REBALANCE_POLICY], &glc_reserve_bridge::ID).0
}

pub fn pending_upgrade_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_PENDING_UPGRADE], &glc_reserve_bridge::ID).0
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

pub fn rebalance_withdrawal_pda(nonce: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_REBALANCE_WITHDRAWAL, &nonce.to_le_bytes()],
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

/// A BPF-loader-v3 Buffer account carrying real ELF bytes, for exercising
/// the real `bpf_loader_upgradeable::upgrade` CPI in `execute_upgrade`
/// tests — the same fabrication approach as `programdata_account`, just
/// for the `Buffer` variant.
pub fn buffer_account(authority: Option<Pubkey>, elf: &[u8]) -> Account {
    let mut data = bincode::serialize(&UpgradeableLoaderState::Buffer {
        authority_address: authority,
    })
    .unwrap();
    data.resize(UpgradeableLoaderState::size_of_buffer_metadata(), 0);
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
pub const DEFAULT_UPGRADE_TIMELOCK: i64 = 3_600;

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
            upgrade_timelock_seconds: DEFAULT_UPGRADE_TIMELOCK,
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
    write_mint_with_decimals(svm, address, supply, GOLDCOIN_DECIMALS)
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

// --------------------------------------------------------- Token-2022 --

/// Writes a real, TLV-correct Token-2022 mint at `address`, carrying
/// exactly `extensions` — litesvm bundles the real `spl_token_2022`
/// program by default (`LiteSVM::new()` -> `with_spl_programs()`), so an
/// account built this way is genuinely well-formed input to
/// `anchor_spl::token_interface`/`crate::token_extensions`, not merely a
/// legacy-shaped stand-in relabeled with a different owner. Same
/// construction pattern as `service/src/solana/accounts.rs`'s and
/// `src/token_extensions/tests.rs`'s equivalents.
pub fn write_token2022_mint(
    svm: &mut LiteSVM,
    address: &Pubkey,
    supply: u64,
    decimals: u8,
    extensions: &[spl_token_2022::extension::ExtensionType],
) {
    use spl_token_2022::extension::metadata_pointer::MetadataPointer;
    use spl_token_2022::extension::transfer_fee::TransferFeeConfig;
    use spl_token_2022::extension::{
        BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
    };
    use spl_token_2022::solana_program::program_option::COption;
    use spl_token_2022::state::Mint as Token2022Mint;

    let len = if extensions.is_empty() {
        Token2022Mint::LEN
    } else {
        ExtensionType::try_calculate_account_len::<Token2022Mint>(extensions).unwrap()
    };
    let mut data = vec![0u8; len];
    let mut state = StateWithExtensionsMut::<Token2022Mint>::unpack_uninitialized(&mut data)
        .expect("unpack uninitialized Token-2022 mint buffer");
    for ext in extensions {
        match ext {
            ExtensionType::MetadataPointer => {
                state.init_extension::<MetadataPointer>(true).unwrap();
            }
            ExtensionType::TransferFeeConfig => {
                state.init_extension::<TransferFeeConfig>(true).unwrap();
            }
            other => panic!("write_token2022_mint does not support building extension {other:?}"),
        }
    }
    state.base.mint_authority = COption::None;
    state.base.supply = supply;
    state.base.decimals = decimals;
    state.base.is_initialized = true;
    state.base.freeze_authority = COption::None;
    state.pack_base();
    state.init_account_type().unwrap();

    svm.set_account(
        *address,
        Account {
            lamports: 10_000_000,
            data,
            owner: spl_token_2022::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// Writes a real, TLV-correct Token-2022 token account (carrying the
/// standard `ImmutableOwner` extension every real Token-2022 ATA has) at
/// `address` — see [`write_token2022_mint`].
pub fn write_token2022_token_account(
    svm: &mut LiteSVM,
    address: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    amount: u64,
) {
    use spl_token_2022::extension::immutable_owner::ImmutableOwner;
    use spl_token_2022::extension::{
        BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
    };
    use spl_token_2022::solana_program::program_option::COption;
    use spl_token_2022::state::{Account as Token2022Account, AccountState};

    let len = ExtensionType::try_calculate_account_len::<Token2022Account>(&[
        ExtensionType::ImmutableOwner,
    ])
    .unwrap();
    let mut data = vec![0u8; len];
    let mut state = StateWithExtensionsMut::<Token2022Account>::unpack_uninitialized(&mut data)
        .expect("unpack uninitialized Token-2022 account buffer");
    state.init_extension::<ImmutableOwner>(true).unwrap();
    state.base.mint = *mint;
    state.base.owner = *wallet;
    state.base.amount = amount;
    state.base.delegate = COption::None;
    state.base.state = AccountState::Initialized;
    state.base.is_native = COption::None;
    state.base.delegated_amount = 0;
    state.base.close_authority = COption::None;
    state.pack_base();
    state.init_account_type().unwrap();

    svm.set_account(
        *address,
        Account {
            lamports: 10_000_000,
            data,
            owner: spl_token_2022::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// The Token-2022 ATA address for `(wallet, mint)` — distinct from
/// [`get_associated_token_address`] (legacy-only) because the ATA PDA's
/// seeds include the owning token program.
pub fn token2022_ata_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address_with_program_id(
        wallet,
        mint,
        &spl_token_2022::ID,
    )
}

/// Pre-creates a real Token-2022 ATA for `wallet`/`mint` at `amount` and
/// returns its address — see [`write_token2022_token_account`].
pub fn create_token2022_ata(
    svm: &mut LiteSVM,
    wallet: &Pubkey,
    mint: &Pubkey,
    amount: u64,
) -> Pubkey {
    let ata = token2022_ata_address(wallet, mint);
    write_token2022_token_account(svm, &ata, wallet, mint, amount);
    ata
}

/// Builds the `initialize_reserve_vault` instruction directly (not via
/// `setup_with_reserve`'s config-patching shortcut — this is the real
/// instruction path, needed to test its own validation).
pub fn initialize_reserve_vault_ix(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    let reserve_token_account =
        anchor_spl::associated_token::get_associated_token_address_with_program_id(
            &reserve_authority,
            reserve_mint,
            token_program,
        );
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::InitializeReserveVault {
            admin: *admin,
            bridge_config: config_pda(),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account,
            token_program: *token_program,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: solana_sdk::system_program::ID,
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::InitializeReserveVault {}.data(),
    }
}

pub fn token_balance(svm: &LiteSVM, token_account: &Pubkey) -> u64 {
    let account = svm.get_account(token_account).expect("token account");
    spl_token::state::Account::unpack(&account.data)
        .unwrap()
        .amount
}

/// Same as [`token_balance`], for a real Token-2022 account (whose data is
/// longer than the legacy fixed 165-byte layout once any extension — even
/// just `ImmutableOwner` — is present).
pub fn token2022_balance(svm: &LiteSVM, token_account: &Pubkey) -> u64 {
    use spl_token_2022::extension::StateWithExtensions;
    let account = svm.get_account(token_account).expect("token account");
    StateWithExtensions::<spl_token_2022::state::Account>::unpack(&account.data)
        .unwrap()
        .base
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

pub fn get_release_volume_window(svm: &LiteSVM) -> RollingVolumeWindow {
    let account = svm
        .get_account(&release_volume_window_pda())
        .expect("release volume window must exist");
    RollingVolumeWindow::try_deserialize(&mut account.data.as_slice()).unwrap()
}

pub fn get_deposit_volume_window(svm: &LiteSVM) -> RollingVolumeWindow {
    let account = svm
        .get_account(&deposit_volume_window_pda())
        .expect("deposit volume window must exist");
    RollingVolumeWindow::try_deserialize(&mut account.data.as_slice()).unwrap()
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

// ------------------------------------------------------ rebalance_withdraw --

pub fn rebalance_withdraw_claim_message(
    epoch: u64,
    nonce: u64,
    amount: u64,
    destination: &Pubkey,
    reserve_mint: &Pubkey,
) -> Vec<u8> {
    glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        nonce,
        amount,
        &destination.to_bytes(),
        &reserve_mint.to_bytes(),
    )
    .to_vec()
}

#[allow(clippy::too_many_arguments)]
pub fn rebalance_withdraw_ix(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_token_account =
        get_associated_token_address(&reserve_authority_pda(), reserve_mint);
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::RebalanceWithdraw {
            admin: *admin,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            rebalance_withdrawal: rebalance_withdrawal_pda(nonce),
            reserve_mint: *reserve_mint,
            reserve_authority: reserve_authority_pda(),
            reserve_token_account,
            destination_token_account: *destination_token_account,
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: spl_token::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::RebalanceWithdraw {
            nonce,
            amount,
            attestation_epoch,
        }
        .data(),
    }
}

pub fn get_rebalance_withdrawal(svm: &LiteSVM, nonce: u64) -> RebalanceWithdrawal {
    let account = svm.get_account(&rebalance_withdrawal_pda(nonce)).unwrap();
    RebalanceWithdrawal::try_deserialize(&mut account.data.as_slice()).unwrap()
}

/// Same as [`rebalance_withdraw_ix`], but with an explicit `token_program`
/// — needed to exercise the reserve's pinned-token-program constraint
/// (`address = bridge_config.reserve_token_program`), same reasoning as
/// [`release_from_reserve_ix_with_token_program`].
#[allow(clippy::too_many_arguments)]
pub fn rebalance_withdraw_ix_with_token_program(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    let reserve_token_account =
        anchor_spl::associated_token::get_associated_token_address_with_program_id(
            &reserve_authority,
            reserve_mint,
            token_program,
        );
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::RebalanceWithdraw {
            admin: *admin,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            rebalance_withdrawal: rebalance_withdrawal_pda(nonce),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account,
            destination_token_account: *destination_token_account,
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: *token_program,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::RebalanceWithdraw {
            nonce,
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

/// Same as [`release_from_reserve_ix`], but with an explicit
/// `token_program` and Token-2022-aware ATA derivation — needed to
/// exercise the reserve's pinned-token-program constraint (`address =
/// bridge_config.reserve_token_program`) and Token-2022 settlement paths.
#[allow(clippy::too_many_arguments)]
pub fn release_from_reserve_ix_with_token_program(
    submitter: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    recipient: &Pubkey,
    recipient_token_account: &Pubkey,
    txid: [u8; 32],
    vout: u32,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    let reserve_token_account =
        anchor_spl::associated_token::get_associated_token_address_with_program_id(
            &reserve_authority,
            reserve_mint,
            token_program,
        );
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ReleaseFromReserve {
            submitter: *submitter,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            deposit_claim: claim_pda(&txid, vout),
            release_volume_window: release_volume_window_pda(),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account,
            recipient: *recipient,
            recipient_token_account: *recipient_token_account,
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: *token_program,
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

/// Same as [`deposit_to_reserve_ix`], but with an explicit `token_program`
/// and Token-2022-aware ATA derivation — see
/// [`release_from_reserve_ix_with_token_program`].
pub fn deposit_to_reserve_ix_with_token_program(
    user: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    user_token_account: &Pubkey,
    obligation_index: u64,
    amount: u64,
    glc_address: Vec<u8>,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    let reserve_token_account =
        anchor_spl::associated_token::get_associated_token_address_with_program_id(
            &reserve_authority,
            reserve_mint,
            token_program,
        );
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::DepositToReserve {
            user: *user,
            bridge_config: config_pda(),
            deposit_volume_window: deposit_volume_window_pda(),
            reserve_mint: *reserve_mint,
            user_token_account: *user_token_account,
            reserve_authority,
            reserve_token_account,
            withdrawal_obligation: obligation_pda(obligation_index),
            token_program: *token_program,
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

// ------------------------------------------------------- complete_goldcoin_payout --

/// The same `sha256` commitment `record_goldcoin_completion` computes
/// on-chain from `WithdrawalObligation.glc_address[..glc_address_len]` —
/// used here to build the exact message a real off-chain attestation
/// signer would sign.
pub fn glc_dest_commitment(glc_address: &[u8]) -> [u8; 32] {
    anchor_lang::solana_program::hash::hash(glc_address).to_bytes()
}

#[allow(clippy::too_many_arguments)]
pub fn goldcoin_completion_message(
    epoch: u64,
    obligation_index: u64,
    payout_txid: &[u8; 32],
    payout_height: u64,
    amount: u64,
    dest_commitment: &[u8; 32],
) -> Vec<u8> {
    glc_reserve_bridge_shared::claim::goldcoin_completion_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        obligation_index,
        payout_txid,
        payout_height,
        amount,
        dest_commitment,
    )
    .to_vec()
}

#[allow(clippy::too_many_arguments)]
pub fn complete_goldcoin_payout_ix(
    submitter: &Pubkey,
    index: u64,
    payout_txid: [u8; 32],
    payout_height: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::CompleteGoldcoinPayout {
            submitter: *submitter,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            obligation: obligation_pda(index),
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::RecordGoldcoinCompletion {
            index,
            payout_txid,
            payout_height,
            amount,
            attestation_epoch,
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

/// PDA for the `RollingVolumeWindow` the given `direction` maps to —
/// `GoldcoinToSolana` (release) at index 0, `SolanaToGoldcoin` (deposit)
/// at index 1, matching `initialize.rs`'s own assignment.
pub fn rolling_volume_window_pda_for(direction: Direction) -> Pubkey {
    match direction {
        Direction::GoldcoinToSolana => release_volume_window_pda(),
        Direction::SolanaToGoldcoin => deposit_volume_window_pda(),
    }
}

pub fn reset_rolling_volume_window_ix(admin: &Pubkey, direction: Direction) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ResetRollingVolumeWindow {
            admin: *admin,
            bridge_config: config_pda(),
            rolling_volume_window: rolling_volume_window_pda_for(direction),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ResetRollingVolumeWindow { direction }.data(),
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

// -------------------------------------------------- upgrade timelock --

pub fn accept_upgrade_authority_ix(current_authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::AcceptUpgradeAuthority {
            current_upgrade_authority: *current_authority,
            bridge_config: config_pda(),
            upgrade_authority_pda: upgrade_authority_pda(),
            program: glc_reserve_bridge::ID,
            program_data: programdata_address(),
            bpf_loader_upgradeable_program: bpf_loader_upgradeable::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::AcceptUpgradeAuthority {}.data(),
    }
}

pub fn propose_upgrade_ix(admin: &Pubkey, buffer_address: Pubkey) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ProposeUpgrade {
            admin: *admin,
            bridge_config: config_pda(),
            pending_upgrade: pending_upgrade_pda(),
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ProposeUpgrade { buffer_address }.data(),
    }
}

pub fn cancel_upgrade_ix(admin: &Pubkey) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::CancelUpgrade {
            admin: *admin,
            bridge_config: config_pda(),
            pending_upgrade: pending_upgrade_pda(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::CancelUpgrade {}.data(),
    }
}

pub fn execute_upgrade_ix(executor: &Pubkey, buffer_address: Pubkey) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ExecuteUpgrade {
            executor: *executor,
            bridge_config: config_pda(),
            pending_upgrade: pending_upgrade_pda(),
            upgrade_authority_pda: upgrade_authority_pda(),
            program: glc_reserve_bridge::ID,
            program_data: programdata_address(),
            buffer: buffer_address,
            rent: solana_sdk::sysvar::rent::id(),
            clock: solana_sdk::sysvar::clock::id(),
            bpf_loader_upgradeable_program: bpf_loader_upgradeable::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ExecuteUpgrade {}.data(),
    }
}

/// The real, live `ProgramData.upgrade_authority_address` — read directly
/// from loader-owned account state, not this program's own bookkeeping,
/// so a test can prove the CPI actually changed what the runtime believes
/// rather than merely that our instruction returned success.
pub fn get_programdata_upgrade_authority(svm: &LiteSVM) -> Option<Pubkey> {
    let account = svm
        .get_account(&programdata_address())
        .expect("programdata account must exist");
    match bincode::deserialize(&account.data).unwrap() {
        UpgradeableLoaderState::ProgramData {
            upgrade_authority_address,
            ..
        } => upgrade_authority_address,
        other => panic!("expected ProgramData, got {other:?}"),
    }
}

pub fn get_pending_upgrade(svm: &LiteSVM) -> glc_reserve_bridge::state::PendingProgramUpgrade {
    let account = svm
        .get_account(&pending_upgrade_pda())
        .expect("pending upgrade must exist");
    glc_reserve_bridge::state::PendingProgramUpgrade::try_deserialize(&mut account.data.as_slice())
        .unwrap()
}

// ============================================================================
// Reserve withdrawal hardening (2026-09-02): rebalance policy,
// treasury_withdraw, refund_withdraw.
// ============================================================================

pub fn get_rebalance_policy(svm: &LiteSVM) -> RebalancePolicy {
    let account = svm
        .get_account(&rebalance_policy_pda())
        .expect("rebalance policy account missing");
    RebalancePolicy::try_deserialize(&mut account.data.as_slice()).unwrap()
}

pub fn get_pending_rebalance_policy(svm: &LiteSVM) -> PendingRebalancePolicy {
    let account = svm
        .get_account(&pending_rebalance_policy_pda())
        .expect("pending rebalance policy account missing");
    PendingRebalancePolicy::try_deserialize(&mut account.data.as_slice()).unwrap()
}

pub fn rebalance_policy_exists(svm: &LiteSVM) -> bool {
    svm.get_account(&rebalance_policy_pda())
        .map(|a| !a.data.is_empty())
        .unwrap_or(false)
}

pub fn pending_rebalance_policy_exists(svm: &LiteSVM) -> bool {
    svm.get_account(&pending_rebalance_policy_pda())
        .map(|a| !a.data.is_empty())
        .unwrap_or(false)
}

/// The governance message a policy INITIALIZATION must be attested over.
pub fn initialize_rebalance_policy_message(epoch: u64, treasuries: &[Pubkey]) -> Vec<u8> {
    rebalance_policy_governance_message(
        epoch,
        treasuries,
        glc_reserve_bridge_shared::governance::ACTION_INITIALIZE_REBALANCE_POLICY,
    )
}

/// The governance message a policy UPDATE PROPOSAL must be attested over.
pub fn propose_rebalance_policy_message(epoch: u64, treasuries: &[Pubkey]) -> Vec<u8> {
    rebalance_policy_governance_message(
        epoch,
        treasuries,
        glc_reserve_bridge_shared::governance::ACTION_PROPOSE_REBALANCE_POLICY,
    )
}

fn rebalance_policy_governance_message(epoch: u64, treasuries: &[Pubkey], action: u8) -> Vec<u8> {
    let raw: Vec<[u8; 32]> = treasuries.iter().map(|t| t.to_bytes()).collect();
    let commitment = anchor_lang::solana_program::hash::hash(
        &glc_reserve_bridge_shared::governance::rebalance_policy_params(&raw),
    )
    .to_bytes();
    glc_reserve_bridge_shared::governance::governance_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        action,
        &commitment,
    )
    .to_vec()
}

/// The governance message a policy-update CANCELLATION must be attested
/// over. Binds the exact `eta` being cancelled.
pub fn cancel_rebalance_policy_message(epoch: u64, pending_eta: i64) -> Vec<u8> {
    let commitment = anchor_lang::solana_program::hash::hash(
        &glc_reserve_bridge_shared::governance::cancel_params(
            glc_reserve_bridge_shared::governance::ACTION_PROPOSE_REBALANCE_POLICY,
            pending_eta,
        ),
    )
    .to_bytes();
    glc_reserve_bridge_shared::governance::governance_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        glc_reserve_bridge_shared::governance::ACTION_CANCEL_REBALANCE_POLICY,
        &commitment,
    )
    .to_vec()
}

pub fn initialize_rebalance_policy_ix(
    payer: &Pubkey,
    reserve_mint: &Pubkey,
    treasuries: Vec<Pubkey>,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::InitializeRebalancePolicy {
            payer: *payer,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            rebalance_policy: rebalance_policy_pda(),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account: get_associated_token_address(&reserve_authority, reserve_mint),
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: spl_token::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::InitializeRebalancePolicy { treasuries }.data(),
    }
}

pub fn propose_rebalance_policy_ix(
    proposer: &Pubkey,
    reserve_mint: &Pubkey,
    treasuries: Vec<Pubkey>,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ProposeRebalancePolicy {
            proposer: *proposer,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            rebalance_policy: rebalance_policy_pda(),
            pending_rebalance_policy: pending_rebalance_policy_pda(),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account: get_associated_token_address(&reserve_authority, reserve_mint),
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: spl_token::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ProposeRebalancePolicy { treasuries }.data(),
    }
}

pub fn execute_rebalance_policy_ix(executor: &Pubkey, reserve_mint: &Pubkey) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::ExecuteRebalancePolicy {
            executor: *executor,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            rebalance_policy: rebalance_policy_pda(),
            pending_rebalance_policy: pending_rebalance_policy_pda(),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account: get_associated_token_address(&reserve_authority, reserve_mint),
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::ExecuteRebalancePolicy {}.data(),
    }
}

pub fn cancel_rebalance_policy_ix(canceller: &Pubkey) -> Instruction {
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::CancelRebalancePolicy {
            canceller: *canceller,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            pending_rebalance_policy: pending_rebalance_policy_pda(),
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::CancelRebalancePolicy {}.data(),
    }
}

// ---------------------------------------------------- treasury_withdraw --

#[allow(clippy::too_many_arguments)]
pub fn treasury_withdraw_claim_message(
    epoch: u64,
    nonce: u64,
    amount: u64,
    destination: &Pubkey,
    reserve_mint: &Pubkey,
    policy_version: u64,
) -> Vec<u8> {
    let reserve_token_account =
        get_associated_token_address(&reserve_authority_pda(), reserve_mint);
    glc_reserve_bridge_shared::claim::treasury_withdraw_claim_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        nonce,
        amount,
        &destination.to_bytes(),
        &reserve_mint.to_bytes(),
        &reserve_token_account.to_bytes(),
        policy_version,
    )
    .to_vec()
}

pub fn treasury_withdraw_ix(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    treasury_withdraw_ix_with_token_program(
        admin,
        reserve_mint,
        &spl_token::ID,
        destination_token_account,
        nonce,
        amount,
        attestation_epoch,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn treasury_withdraw_ix_with_token_program(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    let reserve_token_account =
        anchor_spl::associated_token::get_associated_token_address_with_program_id(
            &reserve_authority,
            reserve_mint,
            token_program,
        );
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::TreasuryWithdraw {
            admin: *admin,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            rebalance_policy: rebalance_policy_pda(),
            rebalance_withdrawal: rebalance_withdrawal_pda(nonce),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account,
            destination_token_account: *destination_token_account,
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: *token_program,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::TreasuryWithdraw {
            nonce,
            amount,
            attestation_epoch,
        }
        .data(),
    }
}

// ------------------------------------------------------ refund_withdraw --

#[allow(clippy::too_many_arguments)]
pub fn refund_withdraw_claim_message(
    epoch: u64,
    nonce: u64,
    amount: u64,
    destination: &Pubkey,
    reserve_mint: &Pubkey,
    obligation_index: u64,
    requester: &Pubkey,
) -> Vec<u8> {
    let reserve_token_account =
        get_associated_token_address(&reserve_authority_pda(), reserve_mint);
    glc_reserve_bridge_shared::claim::refund_withdraw_claim_message(
        PROTOCOL_VERSION,
        &glc_reserve_bridge::ID.to_bytes(),
        epoch,
        nonce,
        amount,
        &destination.to_bytes(),
        &reserve_mint.to_bytes(),
        &reserve_token_account.to_bytes(),
        obligation_index,
        &requester.to_bytes(),
    )
    .to_vec()
}

#[allow(clippy::too_many_arguments)]
pub fn refund_withdraw_ix(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    requester: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
    obligation_index: u64,
) -> Instruction {
    let reserve_authority = reserve_authority_pda();
    Instruction {
        program_id: glc_reserve_bridge::ID,
        accounts: glc_reserve_bridge::accounts::RefundWithdraw {
            admin: *admin,
            bridge_config: config_pda(),
            attestation_key_set: attestation_key_set_pda(),
            withdrawal_obligation: obligation_pda(obligation_index),
            requester: *requester,
            rebalance_withdrawal: rebalance_withdrawal_pda(nonce),
            reserve_mint: *reserve_mint,
            reserve_authority,
            reserve_token_account: get_associated_token_address(&reserve_authority, reserve_mint),
            destination_token_account: *destination_token_account,
            instructions_sysvar: anchor_lang::solana_program::sysvar::instructions::ID,
            token_program: spl_token::ID,
            system_program: solana_sdk::system_program::id(),
        }
        .to_account_metas(None),
        data: glc_reserve_bridge::instruction::RefundWithdraw {
            nonce,
            amount,
            attestation_epoch,
            obligation_index,
        }
        .data(),
    }
}

/// Fabricates a `WithdrawalObligation` directly, the same
/// patch-the-account technique `setup_with_reserve` uses for
/// `BridgeConfig`: it avoids routing every refund test through a full
/// `deposit_to_reserve` (which would drag in the deposit direction's own
/// pause flags, dust floor and rolling window, none of which is under test
/// here) while producing a byte-identical account.
pub fn write_obligation(
    svm: &mut LiteSVM,
    index: u64,
    requester: &Pubkey,
    amount: u64,
    status: WithdrawalStatus,
) {
    let (_, bump) = Pubkey::find_program_address(
        &[SEED_WITHDRAWAL_OBLIGATION, &index.to_le_bytes()],
        &glc_reserve_bridge::ID,
    );
    let mut glc_address = [0u8; 64];
    let addr = b"GTestGoldcoinAddressForRefundTests1";
    glc_address[..addr.len()].copy_from_slice(addr);
    let obligation = WithdrawalObligation {
        index,
        amount,
        requester: *requester,
        glc_address,
        glc_address_len: addr.len() as u8,
        status,
        requested_at_slot: 1,
        protocol_version: PROTOCOL_VERSION,
        bump,
        reserved: [0u8; 48],
    };
    let mut data = Vec::new();
    obligation.try_serialize(&mut data).unwrap();
    let account = Account {
        lamports: 10_000_000,
        data,
        owner: glc_reserve_bridge::ID,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(obligation_pda(index), account).unwrap();
}

/// `setup_with_reserve` plus a globally-paused bridge and an initialized
/// `RebalancePolicy` naming exactly ONE canonical treasury — the shape
/// production starts in (decision 2 of the 2026-09-02 hardening brief).
///
/// Returns `(svm, attestation signers, reserve mint, treasury token
/// account)`.
pub fn setup_paused_with_policy(
    authority: &Keypair,
    reserve_balance: u64,
) -> (LiteSVM, Vec<Keypair>, Pubkey, Pubkey) {
    let (mut svm, signers, mint) = setup_with_reserve(authority, reserve_balance);

    // The bridge must already be globally paused before any withdrawal is
    // attempted — preserved precondition, so every test here exercises the
    // real production sequence rather than a relaxed one.
    send_ixs(
        &mut svm,
        &[set_paused_ix(&authority.pubkey(), PauseScope::Global, true)],
        authority,
        &[],
    )
    .expect("pause");

    let treasury_owner = Pubkey::new_unique();
    let treasury = create_ata(&mut svm, &treasury_owner, &mint, 0);

    let epoch = get_attestation_key_set(&svm).epoch;
    let message = initialize_rebalance_policy_message(epoch, &[treasury]);
    let signer_refs: Vec<&Keypair> = signers.iter().take(2).collect();
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&signer_refs, &message),
            initialize_rebalance_policy_ix(&authority.pubkey(), &mint, vec![treasury]),
        ],
        authority,
        &[],
    )
    .expect("initialize rebalance policy");

    (svm, signers, mint, treasury)
}
