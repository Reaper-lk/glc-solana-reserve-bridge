use super::*;
use anchor_lang::solana_program::program_option::COption;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_spl::token_interface::spl_token_2022::extension::immutable_owner::ImmutableOwner;
use anchor_spl::token_interface::spl_token_2022::extension::memo_transfer::MemoTransfer;
use anchor_spl::token_interface::spl_token_2022::extension::metadata_pointer::MetadataPointer;
use anchor_spl::token_interface::spl_token_2022::extension::transfer_fee::{
    TransferFeeAmount, TransferFeeConfig,
};
use anchor_spl::token_interface::spl_token_2022::extension::{
    BaseStateWithExtensionsMut, StateWithExtensionsMut,
};
use anchor_spl::token_interface::spl_token_2022::state::AccountState;

/// Builds a real, on-chain-shaped Token-2022 mint buffer carrying exactly
/// `extensions`, matching the construction pattern spl-token-2022 itself
/// uses in its own tests (`src/offchain.rs`): allocate a buffer sized by
/// `try_calculate_account_len`, `init_extension` each requested type, fill
/// in the base `Mint` fields, then `pack_base`/`init_account_type`.
fn build_mint_bytes(extensions: &[ExtensionType]) -> Vec<u8> {
    let len = if extensions.is_empty() {
        SplMint::LEN
    } else {
        ExtensionType::try_calculate_account_len::<SplMint>(extensions).unwrap()
    };
    let mut data = vec![0u8; len];
    let mut state = StateWithExtensionsMut::<SplMint>::unpack_uninitialized(&mut data).unwrap();
    for ext in extensions {
        match ext {
            ExtensionType::MetadataPointer => {
                state.init_extension::<MetadataPointer>(true).unwrap();
            }
            ExtensionType::TransferFeeConfig => {
                state.init_extension::<TransferFeeConfig>(true).unwrap();
            }
            other => panic!("test helper does not support building extension {other:?}"),
        }
    }
    state.base.mint_authority = COption::None;
    state.base.supply = 0;
    state.base.decimals = 6;
    state.base.is_initialized = true;
    state.base.freeze_authority = COption::None;
    state.pack_base();
    state.init_account_type().unwrap();
    data
}

/// Same construction pattern as [`build_mint_bytes`], for a token account.
fn build_token_account_bytes(extensions: &[ExtensionType]) -> Vec<u8> {
    let len = if extensions.is_empty() {
        SplTokenAccount::LEN
    } else {
        ExtensionType::try_calculate_account_len::<SplTokenAccount>(extensions).unwrap()
    };
    let mut data = vec![0u8; len];
    let mut state =
        StateWithExtensionsMut::<SplTokenAccount>::unpack_uninitialized(&mut data).unwrap();
    for ext in extensions {
        match ext {
            ExtensionType::ImmutableOwner => {
                state.init_extension::<ImmutableOwner>(true).unwrap();
            }
            ExtensionType::MemoTransfer => {
                state.init_extension::<MemoTransfer>(true).unwrap();
            }
            ExtensionType::TransferFeeAmount => {
                state.init_extension::<TransferFeeAmount>(true).unwrap();
            }
            other => panic!("test helper does not support building extension {other:?}"),
        }
    }
    state.base.mint = Pubkey::new_unique();
    state.base.owner = Pubkey::new_unique();
    state.base.amount = 0;
    state.base.delegate = COption::None;
    state.base.state = AccountState::Initialized;
    state.base.is_native = COption::None;
    state.base.delegated_amount = 0;
    state.base.close_authority = COption::None;
    state.pack_base();
    state.init_account_type().unwrap();
    data
}

#[test]
fn classify_matches_documented_supported_set() {
    assert_eq!(
        classify(ExtensionType::MetadataPointer),
        ExtensionSafety::Supported
    );
    assert_eq!(
        classify(ExtensionType::TokenMetadata),
        ExtensionSafety::Supported
    );
    assert_eq!(
        classify(ExtensionType::ImmutableOwner),
        ExtensionSafety::Supported
    );
}

#[test]
fn classify_marks_value_affecting_extensions_unsafe() {
    // The exact categories Task 2 calls out by name.
    for ext in [
        ExtensionType::TransferFeeConfig,
        ExtensionType::TransferFeeAmount,
        ExtensionType::TransferHook,
        ExtensionType::TransferHookAccount,
        ExtensionType::PermanentDelegate,
        ExtensionType::ConfidentialTransferMint,
        ExtensionType::ConfidentialTransferAccount,
        ExtensionType::ConfidentialTransferFeeConfig,
        ExtensionType::ConfidentialTransferFeeAmount,
        ExtensionType::ConfidentialMintBurn,
        ExtensionType::NonTransferable,
        ExtensionType::NonTransferableAccount,
        ExtensionType::InterestBearingConfig,
        ExtensionType::DefaultAccountState,
    ] {
        assert_eq!(
            classify(ext),
            ExtensionSafety::Unsafe,
            "{ext:?} must classify as Unsafe"
        );
    }
}

#[test]
fn classify_marks_reviewed_neutral_extensions_irrelevant() {
    for ext in [
        ExtensionType::MintCloseAuthority,
        ExtensionType::MemoTransfer,
        ExtensionType::CpiGuard,
        ExtensionType::GroupPointer,
        ExtensionType::TokenGroup,
        ExtensionType::GroupMemberPointer,
        ExtensionType::TokenGroupMember,
    ] {
        assert_eq!(
            classify(ext),
            ExtensionSafety::Irrelevant,
            "{ext:?} must classify as Irrelevant"
        );
    }
}

#[test]
fn mint_with_no_extensions_passes() {
    let data = build_mint_bytes(&[]);
    assert!(validate_mint_extension_bytes(&data).is_ok());
}

#[test]
fn mint_with_metadata_pointer_passes() {
    // Matches the canonical GLC mint's actual, verified extension set.
    let data = build_mint_bytes(&[ExtensionType::MetadataPointer]);
    assert!(validate_mint_extension_bytes(&data).is_ok());
}

#[test]
fn mint_with_transfer_fee_config_is_rejected() {
    let data = build_mint_bytes(&[ExtensionType::TransferFeeConfig]);
    let err = validate_mint_extension_bytes(&data).unwrap_err();
    assert_eq!(err, Error::from(BridgeError::UnsupportedTokenExtension));
}

#[test]
fn mint_with_supported_and_unsafe_extension_is_rejected() {
    // A benign extension being present must not mask an unsafe one on the
    // same mint.
    let data = build_mint_bytes(&[
        ExtensionType::MetadataPointer,
        ExtensionType::TransferFeeConfig,
    ]);
    let err = validate_mint_extension_bytes(&data).unwrap_err();
    assert_eq!(err, Error::from(BridgeError::UnsupportedTokenExtension));
}

#[test]
fn token_account_with_no_extensions_passes() {
    let data = build_token_account_bytes(&[]);
    assert!(validate_token_account_extension_bytes(&data).is_ok());
}

#[test]
fn token_account_with_immutable_owner_passes() {
    // Matches every real Token-2022 associated token account.
    let data = build_token_account_bytes(&[ExtensionType::ImmutableOwner]);
    assert!(validate_token_account_extension_bytes(&data).is_ok());
}

#[test]
fn token_account_with_transfer_fee_amount_is_rejected() {
    let data = build_token_account_bytes(&[ExtensionType::TransferFeeAmount]);
    let err = validate_token_account_extension_bytes(&data).unwrap_err();
    assert_eq!(err, Error::from(BridgeError::UnsupportedTokenExtension));
}

#[test]
fn token_account_with_unallowlisted_irrelevant_extension_is_rejected() {
    // `MemoTransfer` classifies as Irrelevant in `classify`, but the
    // active allowlist enforced by `validate_token_account_extension_bytes`
    // only contains `ImmutableOwner` — irrelevant-but-unreviewed-for-this-
    // account-type extensions still fail closed, they are not silently
    // treated as supported.
    let data = build_token_account_bytes(&[ExtensionType::MemoTransfer]);
    let err = validate_token_account_extension_bytes(&data).unwrap_err();
    assert_eq!(err, Error::from(BridgeError::UnsupportedTokenExtension));
}
