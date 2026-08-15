//! Hand-decoded account layouts for `glc-reserve-bridge`, and PDA
//! derivation. No `anchor-lang` dependency (owner decision R1, repeated
//! from the old bridge — see `rpc.rs` module docs): Borsh's integer/`Vec`
//! encoding is a fixed, well-known wire format, decoded here exactly as the
//! old bridge decoded its own on-chain accounts by hand. Layouts must stay
//! byte-for-byte in sync with `programs/glc-reserve-bridge/src/state.rs`
//! (that file is the single source of truth; this is a deliberate,
//! documented duplication across the workspace boundary, same as the old
//! bridge's `solana::rpc` decoders were to `glc_bridge::state`).

use solana_sdk::pubkey::Pubkey;

use super::rpc::{SolanaRpc, SolanaRpcError};

/// Must match `declare_id!` in `programs/glc-reserve-bridge/src/lib.rs`.
/// This is the DEV/LOCAL deploy id generated at scaffold time
/// (docs/07-implementation-plan.md Phase 2) — a production id is a distinct
/// later decision (docs/12-management-decisions.md).
pub const PROGRAM_ID: Pubkey = solana_sdk::pubkey!("BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY");

const SEED_BRIDGE_CONFIG: &[u8] = b"bridge_config";
const SEED_ATTESTATION_KEY_SET: &[u8] = b"attestation_key_set";
const SEED_RESERVE_AUTHORITY: &[u8] = b"reserve_authority";
const SEED_WITHDRAWAL_OBLIGATION: &[u8] = b"withdrawal_obligation";
const SEED_DEPOSIT_CLAIM: &[u8] = b"deposit_claim";
const SEED_ROLLING_VOLUME_WINDOW: &[u8] = b"rolling_volume_window";

const DISCRIMINATOR_LEN: usize = 8;

pub fn bridge_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_BRIDGE_CONFIG], &PROGRAM_ID).0
}

pub fn attestation_key_set_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_ATTESTATION_KEY_SET], &PROGRAM_ID).0
}

pub fn reserve_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[SEED_RESERVE_AUTHORITY], &PROGRAM_ID).0
}

pub fn withdrawal_obligation_pda(index: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_WITHDRAWAL_OBLIGATION, &index.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

pub fn deposit_claim_pda(txid: &[u8; 32], vout: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_DEPOSIT_CLAIM, txid, &vout.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

/// `direction`: `0` = release (GlcToSol), `1` = deposit (SolToGlc) — matches
/// `programs/glc-reserve-bridge/src/instructions/initialize.rs`'s seed
/// convention.
pub fn rolling_volume_window_pda(direction: u8) -> Pubkey {
    Pubkey::find_program_address(&[SEED_ROLLING_VOLUME_WINDOW, &[direction]], &PROGRAM_ID).0
}

/// Associated Token Account address for `(owner, mint)` under whichever SPL
/// token program actually owns `mint` — legacy SPL Token or Token-2022;
/// the ATA address itself is a PDA seeded in part by the token program id,
/// so the two programs derive different addresses for the same
/// `(owner, mint)` pair (docs/18-token-2022-support.md). Callers must pass
/// the real owning program — `BridgeConfigSnapshot.reserve_token_program`
/// once the vault is configured, or the program
/// [`verify_reserve_mint_token_program`] just verified during onboarding —
/// never assume `spl_token::ID`.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address_with_program_id(
        owner,
        mint,
        token_program,
    )
}

/// Decoded subset of `BridgeConfig` (state.rs layout, after the 8-byte
/// Anchor discriminator) — only the fields the indexer/reconciliation
/// actually consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfigSnapshot {
    pub paused: bool,
    pub release_paused: bool,
    pub deposit_paused: bool,
    pub reserve_token_mint: Pubkey,
    /// Whichever SPL token program `initialize_reserve_vault` recorded as
    /// actually owning `reserve_token_mint` — legacy SPL Token or
    /// Token-2022 (docs/18-token-2022-support.md). `Pubkey::default()`
    /// before the vault is configured, same sentinel convention as
    /// `reserve_token_mint`.
    pub reserve_token_program: Pubkey,
    pub obligation_count: u64,
    pub protected_minimum: u64,
    pub min_transfer_amount: u64,
    pub per_transfer_limit: u64,
}

pub fn decode_bridge_config(data: &[u8]) -> Result<BridgeConfigSnapshot, SolanaRpcError> {
    let body = data
        .get(DISCRIMINATOR_LEN..)
        .ok_or_else(|| SolanaRpcError::Malformed("account shorter than discriminator".into()))?;
    // protocol_version(1) admin(32) pending_admin(Option<Pubkey>, BORSH
    // VARIABLE-LENGTH: 1-byte tag, then 32 bytes ONLY if Some — never a
    // fixed 33-byte slot. Decoding this as fixed-size was a real bug this
    // workspace's own unit tests could not catch (their fake byte
    // fixtures shared the same wrong assumption); caught only by Phase 6
    // real-node testing against an actual on-chain-produced account.
    let mut off = 1 + 32;
    let pending_admin_tag = *body
        .get(off)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated pending_admin tag".into()))?;
    off += 1;
    if pending_admin_tag != 0 {
        off += 32;
    }
    let paused = read_bool(body, off)?;
    off += 1;
    let release_paused = read_bool(body, off)?;
    off += 1;
    let deposit_paused = read_bool(body, off)?;
    off += 1;
    off += 1; // bump
    let reserve_token_mint = read_pubkey(body, off)?;
    off += 32;
    let reserve_token_program = read_pubkey(body, off)?;
    off += 32;
    off += 1; // reserve_authority_bump
    let obligation_count = read_u64(body, off)?;
    off += 8;
    off += 8; // governance_timelock_seconds
    let min_transfer_amount = read_u64(body, off)?;
    off += 8;
    let per_transfer_limit = read_u64(body, off)?;
    off += 8;
    let protected_minimum = read_u64(body, off)?;
    Ok(BridgeConfigSnapshot {
        paused,
        release_paused,
        deposit_paused,
        reserve_token_mint,
        reserve_token_program,
        obligation_count,
        protected_minimum,
        min_transfer_amount,
        per_transfer_limit,
    })
}

/// Decoded `AttestationKeySet` (state.rs layout, after discriminator):
/// `epoch: u64, threshold: u8, bump: u8, keys: Vec<Pubkey>, reserved: [u8; 32]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationKeySetSnapshot {
    pub epoch: u64,
    pub threshold: u8,
    pub keys: Vec<Pubkey>,
}

pub fn decode_attestation_key_set(
    data: &[u8],
) -> Result<AttestationKeySetSnapshot, SolanaRpcError> {
    let body = data
        .get(DISCRIMINATOR_LEN..)
        .ok_or_else(|| SolanaRpcError::Malformed("account shorter than discriminator".into()))?;
    let epoch = read_u64(body, 0)?;
    let threshold = *body
        .get(8)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated threshold".into()))?;
    // offset 9 = bump (1 byte), offset 10 = Vec<Pubkey> borsh length prefix (u32 LE).
    let len_bytes = body
        .get(10..14)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated keys length".into()))?;
    let key_count = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
    let keys_start: usize = 14;
    let keys_end = keys_start
        .checked_add(
            key_count
                .checked_mul(32)
                .ok_or_else(|| SolanaRpcError::Malformed("key count overflow".into()))?,
        )
        .ok_or_else(|| SolanaRpcError::Malformed("key range overflow".into()))?;
    let keys_bytes = body
        .get(keys_start..keys_end)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated keys".into()))?;
    #[allow(clippy::chunks_exact_to_as_chunks)]
    // `as_chunks` isn't available on this workspace's pinned stable toolchain
    let keys = keys_bytes
        .chunks_exact(32)
        .map(|c| Pubkey::try_from(c).unwrap())
        .collect();
    Ok(AttestationKeySetSnapshot {
        epoch,
        threshold,
        keys,
    })
}

/// Decoded `WithdrawalObligation` (state.rs layout, after discriminator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalObligationSnapshot {
    pub index: u64,
    pub amount: u64,
    pub requester: Pubkey,
    pub glc_address: Vec<u8>,
    pub status: u8,
}

pub fn decode_withdrawal_obligation(
    data: &[u8],
) -> Result<WithdrawalObligationSnapshot, SolanaRpcError> {
    let body = data
        .get(DISCRIMINATOR_LEN..)
        .ok_or_else(|| SolanaRpcError::Malformed("account shorter than discriminator".into()))?;
    let index = read_u64(body, 0)?;
    let amount = read_u64(body, 8)?;
    let requester = read_pubkey(body, 16)?;
    let glc_address_full = body
        .get(48..48 + 64)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated glc_address".into()))?;
    let glc_address_len = *body
        .get(112)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated glc_address_len".into()))?
        as usize;
    let status = *body
        .get(113)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated status".into()))?;
    let len = glc_address_len.min(64);
    Ok(WithdrawalObligationSnapshot {
        index,
        amount,
        requester,
        glc_address: glc_address_full[..len].to_vec(),
        status,
    })
}

/// Decodes an SPL Token account's `amount` field (offset 64, 8 bytes LE;
/// the raw SPL layout, not an Anchor account — no discriminator).
pub fn decode_token_account_amount(data: &[u8]) -> Result<u64, SolanaRpcError> {
    read_u64(data, 64)
}

/// Boxed out of [`TokenProgramError::UnexpectedOwner`] purely to keep
/// `Result<_, TokenProgramError>` small (clippy's `result_large_err`) —
/// this is the rare, terminal, non-hot-path error branch, so the extra
/// indirection costs nothing that matters.
#[derive(Debug)]
pub struct UnexpectedOwnerDetails {
    pub mint: Pubkey,
    pub actual: Pubkey,
    pub basics: Option<MintBasics>,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenProgramError {
    #[error("configured reserve_token_mint {0} does not exist on-chain")]
    MintNotFound(Pubkey),
    #[error(
        "configured reserve_token_mint {mint} is owned by {actual}, which is neither the legacy \
         SPL Token program ({spl_token}) nor Token-2022 ({token_2022}) — this bridge holds \
         reserves of the existing token directly and cannot operate against an account owned by \
         any other program. Mint fields as decoded anyway, for diagnostics: {basics:?}",
        mint = .0.mint, actual = .0.actual, basics = .0.basics,
        spl_token = spl_token::ID, token_2022 = spl_token_2022::ID,
    )]
    UnexpectedOwner(Box<UnexpectedOwnerDetails>),
    #[error(
        "configured reserve_token_mint {mint} is a Token-2022 mint carrying extension(s) that \
         are not explicitly reviewed and supported: {unsupported:?}. Only MetadataPointer and \
         TokenMetadata (docs/18-token-2022-support.md's extension policy) are accepted; anything \
         that could alter transfer/accounting behavior (transfer fees, transfer hooks, permanent \
         delegation, confidential transfers, non-transferability, interest-bearing behavior, \
         account restrictions) is rejected fail-closed until independently reviewed and added to \
         that policy."
    )]
    UnsupportedExtensions {
        mint: Pubkey,
        unsupported: Vec<spl_token_2022::extension::ExtensionType>,
    },
    #[error(
        "could not parse Token-2022 extension TLV data on reserve_token_mint {mint}: {source}"
    )]
    UnreadableExtensions {
        mint: Pubkey,
        source: SolanaRpcError,
    },
    #[error("could not read reserve_token_mint from Solana: {0}")]
    Rpc(#[from] SolanaRpcError),
}

/// Extensions on the reserve mint that are explicitly reviewed and
/// accepted — kept in sync with `programs/glc-reserve-bridge/src/
/// token_extensions.rs`'s `SUPPORTED_MINT_EXTENSIONS` (a deliberate,
/// documented duplication across the workspace boundary, same discipline
/// as this module's account-layout decoders — see module docs). This is
/// diagnostic/fail-fast only: the on-chain program is the actual
/// enforcement authority and re-checks on every reserve-touching call, not
/// just here at startup/onboarding.
pub const SUPPORTED_MINT_EXTENSIONS: &[spl_token_2022::extension::ExtensionType] = &[
    spl_token_2022::extension::ExtensionType::MetadataPointer,
    spl_token_2022::extension::ExtensionType::TokenMetadata,
];

/// Enumerates every Token-2022 extension type present on a mint's raw
/// account data. A legacy SPL Token mint (no TLV extension data at all)
/// always yields an empty vec.
pub fn mint_extension_types(
    mint: &Pubkey,
    data: &[u8],
) -> Result<Vec<spl_token_2022::extension::ExtensionType>, TokenProgramError> {
    use spl_token_2022::extension::{BaseStateWithExtensions, StateWithExtensions};
    let state = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(data).map_err(|e| {
        TokenProgramError::UnreadableExtensions {
            mint: *mint,
            source: SolanaRpcError::Malformed(format!("mint base/TLV unpack failed: {e}")),
        }
    })?;
    state
        .get_extension_types()
        .map_err(|e| TokenProgramError::UnreadableExtensions {
            mint: *mint,
            source: SolanaRpcError::Malformed(format!("extension type enumeration failed: {e}")),
        })
}

/// The base SPL Mint fields this bridge cares about — decimals, supply,
/// and whether either authority is still live. Decoded from the fixed,
/// well-known 82-byte `spl_token::state::Mint` layout, which Token-2022
/// mints also start with byte-for-byte (extension data is appended after
/// byte 82) — so this decodes correctly regardless of which token program
/// actually owns the account, which is what lets
/// [`verify_reserve_mint_token_program`] report real decimals/authority
/// state even when it's about to reject the mint for being the wrong
/// program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintBasics {
    pub decimals: u8,
    pub supply: u64,
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
    /// Which SPL token program actually owns this mint — legacy SPL Token
    /// or Token-2022. Not decoded from the mint's own data (mint accounts
    /// don't record their own owning program in their data); this is the
    /// account's on-chain `owner` field, carried alongside the decoded
    /// fields so callers never need a second round trip to learn it.
    pub token_program: Pubkey,
}

pub fn decode_mint_basics(data: &[u8]) -> Result<MintBasics, SolanaRpcError> {
    let mint_authority = if read_u32(data, 0)? != 0 {
        Some(read_pubkey(data, 4)?)
    } else {
        None
    };
    let supply = read_u64(data, 36)?;
    let decimals = *data
        .get(44)
        .ok_or_else(|| SolanaRpcError::Malformed("truncated decimals".into()))?;
    let freeze_authority = if read_u32(data, 46)? != 0 {
        Some(read_pubkey(data, 50)?)
    } else {
        None
    };
    Ok(MintBasics {
        decimals,
        supply,
        mint_authority,
        freeze_authority,
        // Not decoded from the mint's own bytes — always overwritten by
        // the caller once it independently knows the account's on-chain
        // `owner`. See `MintBasics::token_program`'s doc comment.
        token_program: Pubkey::default(),
    })
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, SolanaRpcError> {
    let b = data
        .get(offset..offset + 4)
        .ok_or_else(|| SolanaRpcError::Malformed(format!("truncated u32 at {offset}")))?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

/// Verifies the configured reserve mint is owned by a supported SPL token
/// program — legacy SPL Token or Token-2022 (docs/18-token-2022-support.md;
/// the canonical Solana GLC mint,
/// `Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump`, is Token-2022) — and, if
/// it is a Token-2022 mint, that every extension it carries is on the
/// explicitly reviewed allowlist ([`SUPPORTED_MINT_EXTENSIONS`]). Never
/// creates, mints, burns, or wraps anything — this only reads and
/// classifies an existing account.
///
/// This is a diagnostic/fail-fast check, not the enforcement boundary: the
/// on-chain program's own `address`/`InterfaceAccount` constraints and
/// `crate::token_extensions` (on the on-chain side) are what actually
/// reject a bad mint or program at transfer time, on every call, not just
/// once here. Running the same checks here means a misconfigured
/// `reserve_token_mint` fails at service startup/onboarding with a clear,
/// specific message, before any indexer/orchestrator wiring or on-chain
/// transaction is ever attempted.
///
/// On success, returns the mint's real [`MintBasics`] (including which
/// program owns it) — decimals in particular is exactly the value
/// `transfer_checked` will be validated against on-chain
/// (`release_from_reserve`/`deposit_to_reserve` both read it from the
/// mint account directly, never a hardcoded constant), so there is
/// nothing further to "configure" or drift out of sync here.
pub async fn verify_reserve_mint_token_program(
    rpc: &impl SolanaRpc,
    mint: &Pubkey,
) -> Result<MintBasics, TokenProgramError> {
    let account = rpc
        .get_account(mint)
        .await?
        .ok_or(TokenProgramError::MintNotFound(*mint))?;
    let token_program = if account.owner == spl_token::ID {
        spl_token::ID
    } else if account.owner == spl_token_2022::ID {
        spl_token_2022::ID
    } else {
        return Err(TokenProgramError::UnexpectedOwner(Box::new(
            UnexpectedOwnerDetails {
                mint: *mint,
                actual: account.owner,
                basics: decode_mint_basics(&account.data)
                    .ok()
                    .map(|basics| MintBasics {
                        token_program: account.owner,
                        ..basics
                    }),
            },
        )));
    };

    if token_program == spl_token_2022::ID {
        let types = mint_extension_types(mint, &account.data)?;
        let unsupported: Vec<_> = types
            .into_iter()
            .filter(|t| !SUPPORTED_MINT_EXTENSIONS.contains(t))
            .collect();
        if !unsupported.is_empty() {
            return Err(TokenProgramError::UnsupportedExtensions {
                mint: *mint,
                unsupported,
            });
        }
    }

    let basics = decode_mint_basics(&account.data)?;
    Ok(MintBasics {
        token_program,
        ..basics
    })
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, SolanaRpcError> {
    let b = data
        .get(offset..offset + 8)
        .ok_or_else(|| SolanaRpcError::Malformed(format!("truncated u64 at {offset}")))?;
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}

fn read_bool(data: &[u8], offset: usize) -> Result<bool, SolanaRpcError> {
    Ok(*data
        .get(offset)
        .ok_or_else(|| SolanaRpcError::Malformed(format!("truncated bool at {offset}")))?
        != 0)
}

fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey, SolanaRpcError> {
    let b = data
        .get(offset..offset + 32)
        .ok_or_else(|| SolanaRpcError::Malformed(format!("truncated pubkey at {offset}")))?;
    Ok(Pubkey::new_from_array(b.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bridge_config_bytes(
        paused: bool,
        release_paused: bool,
        deposit_paused: bool,
        obligation_count: u64,
    ) -> Vec<u8> {
        let mut v = vec![0u8; DISCRIMINATOR_LEN];
        v.push(1); // protocol_version
        v.extend_from_slice(&[0u8; 32]); // admin
        v.push(0); // pending_admin tag (None) — Borsh variable-length: no payload bytes follow
        v.push(paused as u8);
        v.push(release_paused as u8);
        v.push(deposit_paused as u8);
        v.push(7); // bump
        v.extend_from_slice(&[9u8; 32]); // reserve_token_mint
        v.extend_from_slice(&[6u8; 32]); // reserve_token_program
        v.push(3); // reserve_authority_bump
        v.extend_from_slice(&obligation_count.to_le_bytes());
        v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
        v.extend_from_slice(&100u64.to_le_bytes()); // min_transfer_amount
        v.extend_from_slice(&1_000_000u64.to_le_bytes()); // per_transfer_limit
        v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
        v.extend_from_slice(&2_000_000u64.to_le_bytes()); // rolling_volume_limit
        v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
        v
    }

    #[test]
    fn decodes_bridge_config_matching_the_real_layout() {
        let bytes = fake_bridge_config_bytes(false, true, false, 42);
        let snap = decode_bridge_config(&bytes).unwrap();
        assert!(!snap.paused);
        assert!(snap.release_paused);
        assert!(!snap.deposit_paused);
        assert_eq!(snap.obligation_count, 42);
        assert_eq!(snap.reserve_token_mint, Pubkey::new_from_array([9u8; 32]));
        assert_eq!(
            snap.reserve_token_program,
            Pubkey::new_from_array([6u8; 32])
        );
        assert_eq!(snap.min_transfer_amount, 100);
        assert_eq!(snap.per_transfer_limit, 1_000_000);
        assert_eq!(snap.protected_minimum, 500);
    }

    /// Regression for a real bug Phase 6 real-node testing caught: Borsh
    /// encodes `Option<Pubkey>` as *variable-length* (1-byte tag, then 32
    /// bytes only if `Some`), never a fixed 33-byte slot. The decoder
    /// previously assumed fixed-size, which happened to pass every unit
    /// test because the fake byte fixtures shared the same wrong
    /// assumption — only a real on-chain-produced account (where
    /// `pending_admin` is genuinely `None`, i.e. 1 byte, not 33) exposed
    /// it. This test pins the `Some` case specifically, since `None` alone
    /// can't distinguish fixed-size-with-zeroed-payload from truly
    /// variable-length.
    #[test]
    fn decodes_bridge_config_when_pending_admin_is_some() {
        let mut v = vec![0u8; DISCRIMINATOR_LEN];
        v.push(1); // protocol_version
        v.extend_from_slice(&[0u8; 32]); // admin
        v.push(1); // pending_admin tag (Some)
        v.extend_from_slice(&[3u8; 32]); // pending_admin pubkey payload
        v.push(0); // paused
        v.push(0); // release_paused
        v.push(0); // deposit_paused
        v.push(7); // bump
        v.extend_from_slice(&[9u8; 32]); // reserve_token_mint
        v.extend_from_slice(&[6u8; 32]); // reserve_token_program
        v.push(3); // reserve_authority_bump
        v.extend_from_slice(&42u64.to_le_bytes()); // obligation_count
        v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
        v.extend_from_slice(&100u64.to_le_bytes()); // min_transfer_amount
        v.extend_from_slice(&1_000_000u64.to_le_bytes()); // per_transfer_limit
        v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
        v.extend_from_slice(&2_000_000u64.to_le_bytes()); // rolling_volume_limit
        v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds

        let snap = decode_bridge_config(&v).unwrap();
        assert_eq!(snap.reserve_token_mint, Pubkey::new_from_array([9u8; 32]));
        assert_eq!(snap.obligation_count, 42);
        assert_eq!(snap.per_transfer_limit, 1_000_000);
        assert_eq!(snap.protected_minimum, 500);
    }

    fn fake_withdrawal_obligation_bytes(
        index: u64,
        amount: u64,
        glc_address: &[u8],
        status: u8,
    ) -> Vec<u8> {
        let mut v = vec![0u8; DISCRIMINATOR_LEN];
        v.extend_from_slice(&index.to_le_bytes());
        v.extend_from_slice(&amount.to_le_bytes());
        v.extend_from_slice(&[5u8; 32]); // requester
        let mut addr = [0u8; 64];
        addr[..glc_address.len()].copy_from_slice(glc_address);
        v.extend_from_slice(&addr);
        v.push(glc_address.len() as u8);
        v.push(status);
        v.extend_from_slice(&11u64.to_le_bytes()); // requested_at_slot
        v.push(1); // protocol_version
        v.push(2); // bump
        v.extend_from_slice(&[0u8; 48]); // reserved
        v
    }

    #[test]
    fn decodes_withdrawal_obligation_matching_the_real_layout() {
        let addr = b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
        let bytes = fake_withdrawal_obligation_bytes(9, 12345, addr, 0);
        let snap = decode_withdrawal_obligation(&bytes).unwrap();
        assert_eq!(snap.index, 9);
        assert_eq!(snap.amount, 12345);
        assert_eq!(snap.requester, Pubkey::new_from_array([5u8; 32]));
        assert_eq!(snap.glc_address, addr.to_vec());
        assert_eq!(snap.status, 0);
    }

    #[test]
    fn rejects_truncated_accounts_rather_than_panicking() {
        assert!(decode_bridge_config(&[0u8; 8]).is_err());
        assert!(decode_withdrawal_obligation(&[0u8; 8]).is_err());
    }

    #[test]
    fn pda_derivation_is_deterministic() {
        assert_eq!(bridge_config_pda(), bridge_config_pda());
        assert_eq!(withdrawal_obligation_pda(5), withdrawal_obligation_pda(5));
        assert_ne!(withdrawal_obligation_pda(5), withdrawal_obligation_pda(6));
    }

    // ---------------------------------------- verify_reserve_mint_token_program --

    struct FakeMintOwnerRpc {
        owner: Option<Pubkey>,
        data: Vec<u8>,
    }

    impl FakeMintOwnerRpc {
        fn new(owner: Option<Pubkey>) -> Self {
            FakeMintOwnerRpc {
                owner,
                data: vec![0u8; 82],
            }
        }
        fn with_data(owner: Option<Pubkey>, data: Vec<u8>) -> Self {
            FakeMintOwnerRpc { owner, data }
        }
    }

    /// Builds a real 82-byte `spl_token::state::Mint` layout — the same
    /// fixed-size `COption<Pubkey>` (4-byte tag + 32-byte value, always
    /// both present) encoding real mint accounts use, distinct from this
    /// program's own Borsh `Option<Pubkey>` (variable-length) elsewhere in
    /// this file.
    fn fake_mint_bytes(
        decimals: u8,
        supply: u64,
        mint_authority: Option<Pubkey>,
        freeze_authority: Option<Pubkey>,
    ) -> Vec<u8> {
        let mut v = vec![0u8; 82];
        if let Some(a) = mint_authority {
            v[0..4].copy_from_slice(&1u32.to_le_bytes());
            v[4..36].copy_from_slice(a.as_ref());
        }
        v[36..44].copy_from_slice(&supply.to_le_bytes());
        v[44] = decimals;
        v[45] = 1; // is_initialized
        if let Some(a) = freeze_authority {
            v[46..50].copy_from_slice(&1u32.to_le_bytes());
            v[50..82].copy_from_slice(a.as_ref());
        }
        v
    }

    impl SolanaRpc for FakeMintOwnerRpc {
        async fn get_account(
            &self,
            _pubkey: &Pubkey,
        ) -> Result<Option<solana_sdk::account::Account>, SolanaRpcError> {
            Ok(self.owner.map(|owner| solana_sdk::account::Account {
                lamports: 1,
                data: self.data.clone(),
                owner,
                executable: false,
                rent_epoch: 0,
            }))
        }
        async fn get_multiple_accounts(
            &self,
            _pubkeys: &[Pubkey],
        ) -> Result<Vec<Option<solana_sdk::account::Account>>, SolanaRpcError> {
            unimplemented!()
        }
        async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
            unimplemented!()
        }
        async fn get_latest_blockhash(&self) -> Result<solana_sdk::hash::Hash, SolanaRpcError> {
            unimplemented!()
        }
        async fn send_transaction(
            &self,
            _tx: &solana_sdk::transaction::Transaction,
        ) -> Result<solana_sdk::signature::Signature, SolanaRpcError> {
            unimplemented!()
        }
        async fn get_signature_status(
            &self,
            _signature: &solana_sdk::signature::Signature,
        ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
            unimplemented!()
        }
        async fn is_blockhash_valid(
            &self,
            _blockhash: &solana_sdk::hash::Hash,
        ) -> Result<bool, SolanaRpcError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn accepts_a_mint_owned_by_the_legacy_spl_token_program_and_reports_its_basics() {
        let rpc = FakeMintOwnerRpc::with_data(
            Some(spl_token::ID),
            fake_mint_bytes(6, 978_182_574_793_857, None, None),
        );
        let mint = Pubkey::new_unique();
        let basics = verify_reserve_mint_token_program(&rpc, &mint)
            .await
            .unwrap();
        assert_eq!(basics.decimals, 6);
        assert_eq!(basics.supply, 978_182_574_793_857);
        assert_eq!(basics.mint_authority, None);
        assert_eq!(basics.freeze_authority, None);
        assert_eq!(basics.token_program, spl_token::ID);
    }

    #[tokio::test]
    async fn decodes_live_authorities_when_present() {
        let mint_authority = Pubkey::new_unique();
        let freeze_authority = Pubkey::new_unique();
        let rpc = FakeMintOwnerRpc::with_data(
            Some(spl_token::ID),
            fake_mint_bytes(9, 42, Some(mint_authority), Some(freeze_authority)),
        );
        let basics = verify_reserve_mint_token_program(&rpc, &Pubkey::new_unique())
            .await
            .unwrap();
        assert_eq!(basics.mint_authority, Some(mint_authority));
        assert_eq!(basics.freeze_authority, Some(freeze_authority));
    }

    /// Builds a real, on-chain-shaped Token-2022 mint buffer carrying
    /// exactly `extensions` — same construction pattern as
    /// `programs/glc-reserve-bridge/src/token_extensions/tests.rs`'s
    /// `build_mint_bytes` (spl-token-2022's own test suite uses the
    /// identical pattern in `src/offchain.rs`).
    fn fake_token2022_mint_bytes(
        decimals: u8,
        supply: u64,
        extensions: &[spl_token_2022::extension::ExtensionType],
    ) -> Vec<u8> {
        use solana_sdk::program_option::COption;
        use solana_sdk::program_pack::Pack;
        use spl_token_2022::extension::metadata_pointer::MetadataPointer;
        use spl_token_2022::extension::transfer_fee::TransferFeeConfig;
        use spl_token_2022::extension::{
            BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
        };
        use spl_token_2022::state::Mint as Token2022Mint;

        let len = if extensions.is_empty() {
            Token2022Mint::LEN
        } else {
            ExtensionType::try_calculate_account_len::<Token2022Mint>(extensions).unwrap()
        };
        let mut data = vec![0u8; len];
        let mut state =
            StateWithExtensionsMut::<Token2022Mint>::unpack_uninitialized(&mut data).unwrap();
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
        state.base.supply = supply;
        state.base.decimals = decimals;
        state.base.is_initialized = true;
        state.base.freeze_authority = COption::None;
        state.pack_base();
        state.init_account_type().unwrap();
        data
    }

    #[tokio::test]
    async fn accepts_a_token_2022_mint_with_only_reviewed_supported_extensions() {
        // Matches the canonical GLC mint's actual, verified extension set
        // (docs/18-token-2022-support.md): Token-2022, decimals 6, only
        // MetadataPointer/TokenMetadata, both authorities renounced.
        let rpc = FakeMintOwnerRpc::with_data(
            Some(spl_token_2022::ID),
            fake_token2022_mint_bytes(
                6,
                978_182_574_793_857,
                &[spl_token_2022::extension::ExtensionType::MetadataPointer],
            ),
        );
        let mint = Pubkey::new_unique();
        let basics = verify_reserve_mint_token_program(&rpc, &mint)
            .await
            .unwrap();
        assert_eq!(basics.decimals, 6);
        assert_eq!(basics.supply, 978_182_574_793_857);
        assert_eq!(basics.token_program, spl_token_2022::ID);
    }

    #[tokio::test]
    async fn rejects_a_token_2022_mint_carrying_an_unsupported_extension() {
        let rpc = FakeMintOwnerRpc::with_data(
            Some(spl_token_2022::ID),
            fake_token2022_mint_bytes(
                6,
                1_000,
                &[spl_token_2022::extension::ExtensionType::TransferFeeConfig],
            ),
        );
        let mint = Pubkey::new_unique();
        let err = verify_reserve_mint_token_program(&rpc, &mint)
            .await
            .unwrap_err();
        match err {
            TokenProgramError::UnsupportedExtensions {
                mint: m,
                unsupported,
            } => {
                assert_eq!(m, mint);
                assert_eq!(
                    unsupported,
                    vec![spl_token_2022::extension::ExtensionType::TransferFeeConfig]
                );
            }
            other => panic!("expected UnsupportedExtensions, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_a_mint_owned_by_some_other_unrelated_program() {
        let not_a_token_program = Pubkey::new_unique();
        let rpc = FakeMintOwnerRpc::new(Some(not_a_token_program));
        let mint = Pubkey::new_unique();
        let err = verify_reserve_mint_token_program(&rpc, &mint)
            .await
            .unwrap_err();
        match err {
            TokenProgramError::UnexpectedOwner(details) => {
                assert_eq!(details.mint, mint);
                assert_eq!(details.actual, not_a_token_program);
            }
            other => panic!("expected UnexpectedOwner, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_a_mint_that_does_not_exist_on_chain() {
        let rpc = FakeMintOwnerRpc::new(None);
        let mint = Pubkey::new_unique();
        let err = verify_reserve_mint_token_program(&rpc, &mint)
            .await
            .unwrap_err();
        assert!(matches!(err, TokenProgramError::MintNotFound(m) if m == mint));
    }
}
