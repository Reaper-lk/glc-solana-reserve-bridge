//! Explicit Token-2022 extension classification (docs/18-token-2022-
//! support.md). The canonical Solana GLC mint is a Token-2022 mint
//! (verified read-only against mainnet — docs/16-p0-checkpoint.md/
//! docs/17-p1-checkpoint.md); Token-2022's extension mechanism is a real
//!, general-purpose way for a mint or token account to carry behavior
//! this program was never written to account for — a transfer fee, a
//! transfer hook invoked via CPI, a permanent delegate that can move
//! funds without the owner's signature, non-transferability, and more.
//! Any of those would silently break the 1:1 reserve invariant this
//! bridge depends on if simply ignored.
//!
//! # The rule: allowlist, not denylist
//!
//! Every extension type Token-2022 currently defines is classified below
//! as either explicitly SUPPORTED (safe, reviewed, does not affect
//! transferred amounts or who can transfer) or UNSUPPORTED (everything
//! else, including anything not yet reviewed). `validate_mint_extensions`/
//! `validate_token_account_extensions` reject any account carrying an
//! UNSUPPORTED extension. A brand-new extension type Token-2022 adds in
//! the future is UNSUPPORTED by construction — it is rejected, not
//! silently allowed, until someone explicitly reviews and adds it to the
//! allowlist below (Task 2's explicit instruction: do not assume future
//! extension changes are harmless).
//!
//! # Checked on every instruction that touches the reserve mint, not just once
//!
//! A mint's extension *type set* is fixed at mint creation for most
//! extensions, but some extension authorities can still change behavior
//! within an already-present extension (e.g. a `TransferFeeConfig`
//! authority can update the fee rate). Re-running this check on every
//! `deposit_to_reserve`/`release_from_reserve` call, not only at
//! `initialize_reserve_vault`, means a mint that somehow gained an
//! unsupported extension after this bridge started using it would still
//! be caught before the next transfer, not just at onboarding time.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::spl_token_2022::extension::{
    BaseStateWithExtensions, ExtensionType, StateWithExtensions,
};
use anchor_spl::token_interface::spl_token_2022::state::{
    Account as SplTokenAccount, Mint as SplMint,
};

use crate::errors::BridgeError;

/// Extensions allowed on the reserve mint. Both are metadata-only,
/// carry no authority over transfers or balances, and are exactly what
/// the canonical Solana GLC mint actually has (verified read-only against
/// mainnet, both frozen — `authority`/`updateAuthority: null`).
///
/// - `MetadataPointer` — points at the account holding this mint's
///   on-chain metadata (here, the mint itself). Purely informational.
/// - `TokenMetadata` — the on-chain name/symbol/URI. Purely
///   informational; never consulted by transfer logic.
const SUPPORTED_MINT_EXTENSIONS: &[ExtensionType] =
    &[ExtensionType::MetadataPointer, ExtensionType::TokenMetadata];

/// Extensions allowed on a token account interacting with the reserve
/// (the reserve/user/recipient ATAs).
///
/// - `ImmutableOwner` — the standard, expected default every Token-2022
///   associated token account carries: it only prevents the account's
///   *ownership* from ever being reassigned via `SetAuthority`, and has
///   no effect on balances, transfer amounts, or who can initiate a
///   transfer with a valid signature/PDA authority. Strictly safer than
///   its absence, never a risk to the 1:1 invariant.
const SUPPORTED_TOKEN_ACCOUNT_EXTENSIONS: &[ExtensionType] = &[ExtensionType::ImmutableOwner];

/// Every extension type Token-2022 6.0.0 defines is named explicitly
/// below, one per match arm, rather than relying on the trailing wildcard
/// to classify most of them — so the classification itself is reviewable
/// as a flat, explicit list (Task 2: "Do not assume future mint/account
/// extension changes are harmless"). This function is never called by
/// production code (the allowlists above already do the real
/// enforcement); it exists purely so that list is reviewable and
/// unit-testable on its own. The trailing wildcard exists only to catch a
/// future `spl-token-2022` upgrade that adds a variant nobody has
/// reviewed yet here — it fails that variant closed as `Unsafe`, the same
/// outcome `validate_extension_types`'s allowlist check already produces
/// for any type not explicitly listed as supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionSafety {
    /// Explicitly reviewed and allowed — see `SUPPORTED_MINT_EXTENSIONS`/
    /// `SUPPORTED_TOKEN_ACCOUNT_EXTENSIONS` for exactly which.
    Supported,
    /// Reviewed and confirmed to have no bearing on transfer/accounting
    /// semantics for either a mint or a token account, but not currently
    /// present on the canonical GLC mint or expected on its accounts —
    /// listed for completeness, not currently reachable by the allowlists
    /// above (which only need to distinguish supported from unsupported).
    Irrelevant,
    /// Could alter transfer amounts, who can transfer, or accounting —
    /// rejected unconditionally.
    Unsafe,
}

pub fn classify(extension: ExtensionType) -> ExtensionSafety {
    use ExtensionSafety::*;
    match extension {
        // ---- Reviewed and supported (see doc comments above) ----
        ExtensionType::MetadataPointer => Supported,
        ExtensionType::TokenMetadata => Supported,
        ExtensionType::ImmutableOwner => Supported,

        // ---- Reviewed: changes transferred amounts or accounting; the
        // exact class of risk this module exists to reject ----
        ExtensionType::TransferFeeConfig => Unsafe,
        ExtensionType::TransferFeeAmount => Unsafe,
        ExtensionType::ConfidentialTransferMint => Unsafe,
        ExtensionType::ConfidentialTransferAccount => Unsafe,
        ExtensionType::ConfidentialTransferFeeConfig => Unsafe,
        ExtensionType::ConfidentialTransferFeeAmount => Unsafe,
        ExtensionType::ConfidentialMintBurn => Unsafe,
        ExtensionType::NonTransferable => Unsafe,
        ExtensionType::NonTransferableAccount => Unsafe,
        ExtensionType::InterestBearingConfig => Unsafe,
        ExtensionType::TransferHook => Unsafe,
        ExtensionType::TransferHookAccount => Unsafe,
        ExtensionType::PermanentDelegate => Unsafe,
        ExtensionType::DefaultAccountState => Unsafe,

        // ---- Reviewed: does not itself change transfer amounts or
        // accounting, but not something this bridge currently relies on
        // or expects to see on the reserve mint/its accounts, so not
        // added to the active allowlist without a specific reason to ----
        ExtensionType::MintCloseAuthority => Irrelevant,
        ExtensionType::MemoTransfer => Irrelevant,
        ExtensionType::CpiGuard => Irrelevant,
        ExtensionType::GroupPointer => Irrelevant,
        ExtensionType::TokenGroup => Irrelevant,
        ExtensionType::GroupMemberPointer => Irrelevant,
        ExtensionType::TokenGroupMember => Irrelevant,
        ExtensionType::Uninitialized => Irrelevant,

        // Any variant not named above — including ones added to a future
        // spl-token-2022 release after this module was last reviewed —
        // fails closed as unsafe rather than silently passing.
        #[allow(unreachable_patterns)]
        _ => Unsafe,
    }
}

fn validate_extension_types(
    extension_types: &[ExtensionType],
    allowlist: &[ExtensionType],
) -> Result<()> {
    for ext in extension_types {
        if !allowlist.contains(ext) {
            msg!(
                "reserve mint/token account carries unsupported Token-2022 extension: {:?}",
                ext
            );
            return Err(error!(BridgeError::UnsupportedTokenExtension));
        }
    }
    Ok(())
}

/// Validates every extension present on `mint_account_info`'s raw data
/// against [`SUPPORTED_MINT_EXTENSIONS`]. A legacy SPL Token mint (no TLV
/// extension data at all) always passes trivially — this only ever
/// rejects something a real extension is actually present and not
/// reviewed.
pub fn validate_mint_extensions(mint_account_info: &AccountInfo) -> Result<()> {
    let data = mint_account_info.try_borrow_data()?;
    validate_mint_extension_bytes(&data)
}

/// Same as [`validate_mint_extensions`], for a token account
/// (reserve/user/recipient ATA) against
/// [`SUPPORTED_TOKEN_ACCOUNT_EXTENSIONS`].
pub fn validate_token_account_extensions(token_account_info: &AccountInfo) -> Result<()> {
    let data = token_account_info.try_borrow_data()?;
    validate_token_account_extension_bytes(&data)
}

/// Byte-buffer-only core of [`validate_mint_extensions`], split out so
/// unit tests can exercise it directly against hand-built TLV buffers
/// without constructing a Solana `AccountInfo`.
fn validate_mint_extension_bytes(data: &[u8]) -> Result<()> {
    let state = StateWithExtensions::<SplMint>::unpack(data)
        .map_err(|_| error!(BridgeError::UnreadableTokenState))?;
    let extension_types = state
        .get_extension_types()
        .map_err(|_| error!(BridgeError::UnreadableTokenState))?;
    validate_extension_types(&extension_types, SUPPORTED_MINT_EXTENSIONS)
}

/// Byte-buffer-only core of [`validate_token_account_extensions`]; see
/// [`validate_mint_extension_bytes`].
fn validate_token_account_extension_bytes(data: &[u8]) -> Result<()> {
    let state = StateWithExtensions::<SplTokenAccount>::unpack(data)
        .map_err(|_| error!(BridgeError::UnreadableTokenState))?;
    let extension_types = state
        .get_extension_types()
        .map_err(|_| error!(BridgeError::UnreadableTokenState))?;
    validate_extension_types(&extension_types, SUPPORTED_TOKEN_ACCOUNT_EXTENSIONS)
}

#[cfg(test)]
mod tests;
