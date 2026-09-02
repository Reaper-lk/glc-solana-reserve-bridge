//! Hand-built Solana instruction encoders for `release_from_reserve`,
//! `record_goldcoin_completion`, and `set_paused` — no
//! `anchor-lang`/on-chain-crate dependency (docs/01-reuse-inventory.md
//! owner decision R1, repeated from the old bridge for the same reason:
//! this workspace's dependency graph must stay independent of the SBF
//! build).
//!
//! Account ordering and the 8-byte discriminator scheme must stay
//! byte-for-byte in sync with `programs/glc-reserve-bridge/src/
//! instructions/{release_from_reserve,complete_goldcoin_payout,admin}.rs`
//! (the single source of truth) and Anchor's standard discriminator
//! convention (`sha256("global:<snake_case_instruction_name>")[..8]`,
//! unchanged across recent Anchor versions).

use sha2::{Digest, Sha256};
// `solana_sdk::bpf_loader_upgradeable` is deprecated in favor of a
// dedicated `solana-loader-v3-interface` crate; not worth a new dependency
// for the one function this module needs (`get_program_data_address`),
// which is unchanged and still correct.
#[allow(deprecated)]
use solana_sdk::bpf_loader_upgradeable;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;

use super::accounts::{self, PROGRAM_ID};

fn discriminator(instruction_name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("global:{instruction_name}"));
    hash[..8].try_into().unwrap()
}

/// Idempotent creation of the recipient's CANONICAL associated token
/// account, via the Associated Token Program's `CreateIdempotent`
/// instruction: a no-op when the canonical ATA already exists and
/// initialized; creates it (rent funded by `payer`) when it does not;
/// FAILS if the canonical address is occupied by an account with a
/// different mint/owner — never "adopts" an arbitrary token account. The
/// ATA address is derived inside the ATA program from
/// `(owner, token_program, mint)` — the exact same program-id-aware
/// derivation `accounts::associated_token_address` uses — so this can
/// only ever touch the canonical account `release_from_reserve`'s
/// `associated_token::` constraints will then verify.
///
/// Placed FIRST in the release transaction (before the ed25519 proof +
/// `release_from_reserve` pair, whose relative -1 adjacency is
/// unaffected), so ATA creation and the release are ATOMIC: either the
/// recipient's ATA exists and the funds land in it, or nothing happened
/// at all — there is no partial state for a retry to trip over, and a
/// retry simply carries the same idempotent instruction again.
///
/// The on-chain program deliberately does NOT `init_if_needed` this
/// account (release_from_reserve.rs: a stranger's release transaction
/// must not charge rent on the recipient's behalf). That on-chain
/// precondition stands for arbitrary direct callers; THIS service, as
/// the bridge operator's own submitter, funding the one-time rent for
/// its own recipient is an explicit product decision, made here
/// off-chain without weakening the on-chain rule.
pub fn create_recipient_ata_idempotent(
    payer: &Pubkey,
    recipient: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        payer,
        recipient,
        reserve_mint,
        token_program,
    )
}

/// Builds the `release_from_reserve` instruction. Must be placed
/// immediately after the corresponding
/// [`crate::solana::ed25519::build_attestation_proof`] instruction in the
/// same transaction — the on-chain program looks at `instructions_sysvar`
/// to find it at relative position -1.
#[allow(clippy::too_many_arguments)]
pub fn release_from_reserve(
    submitter: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    recipient: &Pubkey,
    txid: [u8; 32],
    vout: u32,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    let mut data = discriminator("release_from_reserve").to_vec();
    data.extend_from_slice(&txid);
    data.extend_from_slice(&vout.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&attestation_epoch.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*submitter, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::deposit_claim_pda(&txid, vout), false),
            AccountMeta::new(accounts::rolling_volume_window_pda(0), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(*recipient, false),
            AccountMeta::new(
                accounts::associated_token_address(recipient, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds the `record_goldcoin_completion` instruction. Same
/// immediately-preceded-by-the-ed25519-proof requirement as
/// [`release_from_reserve`].
#[allow(clippy::too_many_arguments)]
pub fn record_goldcoin_completion(
    submitter: &Pubkey,
    index: u64,
    payout_txid: [u8; 32],
    payout_height: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let mut data = discriminator("record_goldcoin_completion").to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    data.extend_from_slice(&payout_txid);
    data.extend_from_slice(&payout_height.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&attestation_epoch.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*submitter, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::withdrawal_obligation_pda(index), false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
        ],
        data,
    }
}

/// Builds the one-time `initialize` instruction (`programs/glc-reserve-
/// bridge/src/instructions/initialize.rs`). Only the program's upgrade
/// authority may call this — enforced on-chain by matching `authority`
/// against the loader-v3 `ProgramData` account's `upgrade_authority`
/// field, which is why `program`/`program_data` are required accounts
/// even though this instruction never modifies them. Used by this
/// workspace's Phase 6 real-node rehearsal bootstrap and, later, by a real
/// launch's one-time setup — never by the orchestrator's steady-state
/// loop.
#[allow(clippy::too_many_arguments)]
pub fn initialize(
    authority: &Pubkey,
    attestation_keys: &[Pubkey],
    threshold: u8,
    governance_timelock_seconds: i64,
    min_transfer_amount: u64,
    per_transfer_limit: u64,
    protected_minimum: u64,
    rolling_volume_limit: u64,
    rolling_window_seconds: i64,
    upgrade_timelock_seconds: i64,
) -> Instruction {
    let mut data = discriminator("initialize").to_vec();
    data.extend_from_slice(&(attestation_keys.len() as u32).to_le_bytes());
    for key in attestation_keys {
        data.extend_from_slice(key.as_ref());
    }
    data.push(threshold);
    data.extend_from_slice(&governance_timelock_seconds.to_le_bytes());
    data.extend_from_slice(&min_transfer_amount.to_le_bytes());
    data.extend_from_slice(&per_transfer_limit.to_le_bytes());
    data.extend_from_slice(&protected_minimum.to_le_bytes());
    data.extend_from_slice(&rolling_volume_limit.to_le_bytes());
    data.extend_from_slice(&rolling_window_seconds.to_le_bytes());
    data.extend_from_slice(&upgrade_timelock_seconds.to_le_bytes());

    let program_data = bpf_loader_upgradeable::get_program_data_address(&PROGRAM_ID);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*authority, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
            AccountMeta::new(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::rolling_volume_window_pda(0), false),
            AccountMeta::new(accounts::rolling_volume_window_pda(1), false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(program_data, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds the one-time `initialize_reserve_vault` instruction
/// (`programs/glc-reserve-bridge/src/instructions/reserve_vault.rs`).
/// `admin` must be `BridgeConfig.admin` (set by `initialize`). Binds
/// whatever `reserve_mint` is supplied — this workspace never creates or
/// assumes a mint (docs/12-management-decisions.md item 10); the caller
/// supplies a throwaway mint in Phase 6 rehearsal, a real one at launch.
/// `token_program` must be whichever SPL token program actually owns
/// `reserve_mint` (legacy SPL Token or Token-2022 —
/// [`accounts::verify_reserve_mint_token_program`] determines this); the
/// on-chain instruction records it into
/// `BridgeConfig.reserve_token_program` and pins every later
/// deposit/release instruction's `token_program` account to it
/// (docs/18-token-2022-support.md).
pub fn initialize_reserve_vault(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let data = discriminator("initialize_reserve_vault").to_vec();
    let reserve_authority = accounts::reserve_authority_pda();
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(spl_associated_token_account::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds the `deposit_to_reserve` instruction (`programs/glc-reserve-
/// bridge/src/instructions/deposit_to_reserve.rs`) — the Solana->Goldcoin
/// leg's trigger: the user's own signed SPL transfer into the reserve,
/// atomically paired with a `WithdrawalObligation` record. No attestation
/// needed (the user moves only their own tokens). `obligation_index` must
/// be the CURRENT live `BridgeConfig.obligation_count` — the caller reads
/// it fresh immediately before submitting (same "never trust a cached
/// value" discipline as everywhere else in this workspace); a stale index
/// derives the wrong PDA and the instruction fails closed rather than
/// silently colliding.
pub fn deposit_to_reserve(
    user: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    obligation_index: u64,
    amount: u64,
    glc_address: &[u8],
) -> Instruction {
    let mut data = discriminator("deposit_to_reserve").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&(glc_address.len() as u32).to_le_bytes());
    data.extend_from_slice(glc_address);

    let reserve_authority = accounts::reserve_authority_pda();
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
            AccountMeta::new(accounts::rolling_volume_window_pda(1), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new(
                accounts::associated_token_address(user, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new(accounts::withdrawal_obligation_pda(obligation_index), false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Mirrors `programs/glc-reserve-bridge/src/instructions/admin.rs`'s
/// `PauseScope` — a fieldless enum, so its Borsh encoding is exactly its
/// variant index as a single byte, in declaration order (Anchor/Borsh
/// convention, unchanged across recent versions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseScope {
    Global = 0,
    Release = 1,
    Deposit = 2,
}

/// Builds the `set_paused` instruction — admin-gated-immediate per
/// docs/12-management-decisions.md/IMPLEMENTATION_LOG.md's Phase 2 scoping
/// decision, NOT threshold-gated (unlike attestation-key rotation).
/// `admin` must be the on-chain `BridgeConfig.admin` signer.
pub fn set_paused(admin: &Pubkey, scope: PauseScope, paused: bool) -> Instruction {
    let mut data = discriminator("set_paused").to_vec();
    data.push(scope as u8);
    data.push(paused as u8);

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
        ],
        data,
    }
}

/// Builds the `transfer_admin` instruction — step 1 of the two-step admin
/// handover (`programs/glc-reserve-bridge/src/instructions/admin.rs`).
///
/// Nothing changes on chain until `new_admin` calls [`accept_admin`]: the
/// two-step shape exists so a typoed or unreachable key cannot brick
/// governance, and it is why routine admin rotation needs no separate
/// recovery mechanism. `admin` must be the CURRENT on-chain
/// `BridgeConfig.admin` signer.
pub fn transfer_admin(admin: &Pubkey, new_admin: &Pubkey) -> Instruction {
    let mut data = discriminator("transfer_admin").to_vec();
    data.extend_from_slice(new_admin.as_ref());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
        ],
        data,
    }
}

/// Builds the `accept_admin` instruction — step 2 of the handover. Only
/// the key named by a prior `transfer_admin` may sign this, and signing it
/// is what actually moves `BridgeConfig.admin`.
pub fn accept_admin(new_admin: &Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*new_admin, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
        ],
        data: discriminator("accept_admin").to_vec(),
    }
}

/// Mirrors `programs/glc-reserve-bridge/src/instructions/admin.rs`'s
/// `LimitField` — a fieldless enum, same single-byte Borsh encoding as
/// [`PauseScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField {
    MinTransferAmount = 0,
    PerTransferLimit = 1,
    ProtectedMinimum = 2,
    RollingVolumeLimit = 3,
}

/// Builds the `set_limit` instruction — admin-gated-immediate, same
/// `AdminConfig` accounts shape as [`set_paused`] (module docs there
/// explain why this is an interim posture, not threshold-gated). `admin`
/// must be the on-chain `BridgeConfig.admin` signer.
pub fn set_limit(admin: &Pubkey, field: LimitField, new_value: u64) -> Instruction {
    let mut data = discriminator("set_limit").to_vec();
    data.push(field as u8);
    data.extend_from_slice(&new_value.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
        ],
        data,
    }
}

/// Mirrors `programs/glc-reserve-bridge/src/state.rs`'s `Direction` — a
/// fieldless enum, same single-byte Borsh encoding as [`PauseScope`]/
/// [`LimitField`]. Named `RollingWindowDirection` rather than `Direction`
/// (which would shadow-collide with `crate::ledger::Direction`, a
/// completely different, off-chain-only enum already imported at every
/// call site that uses this module) — this one exists purely to select
/// which on-chain `RollingVolumeWindow` PDA `reset_rolling_volume_window`
/// targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollingWindowDirection {
    /// Goldcoin deposit confirmed -> Solana reserve release ("glc-to-sol").
    GoldcoinToSolana = 0,
    /// Solana deposit confirmed -> Goldcoin reserve release ("sol-to-glc").
    SolanaToGoldcoin = 1,
}

/// Builds the `reset_rolling_volume_window` instruction — admin-gated,
/// same `AdminConfig`-style authorization as [`set_paused`]/[`set_limit`],
/// PLUS an on-chain requirement (checked in the program, not here) that
/// `BridgeConfig.paused` already be `true`. `admin` must be the on-chain
/// `BridgeConfig.admin` signer. The single `RollingVolumeWindow` PDA
/// implied by `direction` is the only account besides `admin`/
/// `bridge_config` this instruction touches.
pub fn reset_rolling_volume_window(
    admin: &Pubkey,
    direction: RollingWindowDirection,
) -> Instruction {
    let mut data = discriminator("reset_rolling_volume_window").to_vec();
    data.push(direction as u8);

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new(accounts::rolling_volume_window_pda(direction as u8), false),
        ],
        data,
    }
}

/// Builds the `rebalance_withdraw` instruction — an intentional,
/// operator-initiated reserve withdrawal (`programs/glc-reserve-bridge/
/// src/instructions/rebalance_withdraw.rs`). Must be placed immediately
/// after the corresponding
/// [`crate::solana::ed25519::build_attestation_proof`] instruction in the
/// same transaction, exactly like [`release_from_reserve`] — the on-chain
/// program looks at `instructions_sysvar` to find it at relative
/// position -1. `admin` must be the on-chain `BridgeConfig.admin` signer
/// AND pays the `rebalance_withdrawal` record's rent (writable, unlike
/// [`set_paused`]'s read-only admin signer).
/// Builds the `treasury_withdraw` instruction — an operator-initiated
/// reserve withdrawal to an ALLOWLISTED treasury token account
/// (`programs/glc-reserve-bridge/src/instructions/treasury_withdraw.rs`).
/// Must be placed immediately after the ed25519 proof instruction.
///
/// `destination_token_account` must appear verbatim in the on-chain
/// `RebalancePolicy` allowlist. This builder does not check that — the
/// program does, authoritatively — but callers should check it first so a
/// bad destination is refused before any custody domain is asked for a
/// signature. See `solana::accounts::RebalancePolicySnapshot::is_allowlisted`.
///
/// `admin` co-signs AND pays the `rebalance_withdrawal` record's rent
/// (writable), same as the retired `rebalance_withdraw` did.
#[allow(clippy::too_many_arguments)]
pub fn treasury_withdraw(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    let mut data = discriminator("treasury_withdraw").to_vec();
    data.extend_from_slice(&nonce.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&attestation_epoch.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new_readonly(accounts::rebalance_policy_pda(), false),
            AccountMeta::new(accounts::rebalance_withdrawal_pda(nonce), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new(*destination_token_account, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds the `refund_withdraw` instruction — returns one
/// `WithdrawalObligation`'s deposit to the wallet that made it
/// (`programs/glc-reserve-bridge/src/instructions/refund_withdraw.rs`).
/// Must be placed immediately after the ed25519 proof instruction.
///
/// `destination_token_account` is not a free choice: the program derives
/// it from `(requester, reserve_mint, token_program)` via Anchor's
/// `associated_token::` constraints and rejects anything else. Callers
/// must pass exactly `accounts::associated_token_address(requester,
/// reserve_mint, token_program)`; passing anything else produces a
/// transaction that cannot succeed.
///
/// `nonce` must have `accounts::NONCE_DOMAIN_REFUND` set — use
/// `Ledger::solana_refund_nonce(request_id)`.
#[allow(clippy::too_many_arguments)]
pub fn refund_withdraw(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    requester: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
    obligation_index: u64,
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    let mut data = discriminator("refund_withdraw").to_vec();
    data.extend_from_slice(&nonce.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&attestation_epoch.to_le_bytes());
    data.extend_from_slice(&obligation_index.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new_readonly(accounts::withdrawal_obligation_pda(obligation_index), false),
            AccountMeta::new_readonly(*requester, false),
            AccountMeta::new(accounts::rebalance_withdrawal_pda(nonce), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new(*destination_token_account, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// **RETIRED — builds a transaction that always fails.**
///
/// The on-chain `rebalance_withdraw` instruction now returns
/// `RebalanceWithdrawRetired` before doing anything. This builder is kept
/// only so tests can construct the exact transaction shape the 2026-09-02
/// incident used and prove it no longer moves funds. Production code must
/// use [`treasury_withdraw`] or [`refund_withdraw`].
#[deprecated(
    note = "rebalance_withdraw is retired on chain; use treasury_withdraw or refund_withdraw"
)]
pub fn rebalance_withdraw(
    admin: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    destination_token_account: &Pubkey,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    let mut data = discriminator("rebalance_withdraw").to_vec();
    data.extend_from_slice(&nonce.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&attestation_epoch.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::rebalance_withdrawal_pda(nonce), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new(*destination_token_account, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

// ------------------------------------------------- rebalance policy (governance) --
//
// The four `RebalancePolicy` instructions. None of them is admin-gated:
// authorization is a threshold attestation over a GOVERNANCE message
// (`glc_reserve_bridge_shared::governance`), never `BridgeConfig.admin`'s
// signature. The signer these builders take is a fee payer / rent
// recipient and confers no authority whatsoever — which is exactly why
// the allowlist survives a compromised admin key.
//
// Every builder below mirrors the account order in
// `programs/glc-reserve-bridge/src/instructions/rebalance_policy.rs`
// verbatim; the tests at the bottom of this module pin that ordering.

/// Borsh encoding of a `Vec<Pubkey>` instruction argument: a `u32` LE
/// length prefix followed by each 32-byte key, in order. Order is
/// significant — it is the order the allowlist is stored in and the order
/// `governance::rebalance_policy_params` commits to.
fn encode_pubkey_vec(keys: &[Pubkey]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + keys.len() * 32);
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        out.extend_from_slice(k.as_ref());
    }
    out
}

/// Builds `initialize_rebalance_policy` — the ONE-TIME creation of the
/// treasury allowlist
/// (`programs/glc-reserve-bridge/src/instructions/rebalance_policy.rs`).
/// Must be placed immediately after the ed25519 proof instruction.
///
/// `payer` funds the policy account's rent and signs for that reason
/// alone. It is deliberately NOT required to be the admin: tying creation
/// of the allowlist to the admin key would put it inside the blast radius
/// the allowlist exists to contain.
///
/// The account is created with Anchor `init`, so a second call against an
/// existing policy fails at account creation — there is no "re-initialize"
/// path, by design. Later changes go through
/// [`propose_rebalance_policy`] and its governance timelock.
pub fn initialize_rebalance_policy(
    payer: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    treasuries: &[Pubkey],
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    let mut data = discriminator("initialize_rebalance_policy").to_vec();
    data.extend_from_slice(&encode_pubkey_vec(treasuries));

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::rebalance_policy_pda(), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new_readonly(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds `propose_rebalance_policy` — queues a REPLACEMENT policy behind
/// the governance timelock. Must be placed immediately after the ed25519
/// proof instruction.
///
/// The policy being replaced must already exist; a proposal against a
/// never-created policy is refused rather than treated as an implicit
/// initialization, which keeps the two approvals (action `0x09` vs `0x07`)
/// strictly apart.
pub fn propose_rebalance_policy(
    proposer: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
    treasuries: &[Pubkey],
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    let mut data = discriminator("propose_rebalance_policy").to_vec();
    data.extend_from_slice(&encode_pubkey_vec(treasuries));

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*proposer, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new_readonly(accounts::rebalance_policy_pda(), false),
            AccountMeta::new(accounts::pending_rebalance_policy_pda(), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new_readonly(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds `execute_rebalance_policy` — applies a queued replacement once
/// its timelock has elapsed.
///
/// PERMISSIONLESS and carries NO ed25519 proof: the threshold attestation
/// collected at proposal time was the authorization and the delay was the
/// safeguard, so this instruction takes no attestation and must NOT be
/// preceded by a proof instruction. `executor` receives the closed pending
/// account's rent.
pub fn execute_rebalance_policy(
    executor: &Pubkey,
    reserve_mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let reserve_authority = accounts::reserve_authority_pda();
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*executor, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::rebalance_policy_pda(), false),
            AccountMeta::new(accounts::pending_rebalance_policy_pda(), false),
            AccountMeta::new_readonly(*reserve_mint, false),
            AccountMeta::new_readonly(reserve_authority, false),
            AccountMeta::new_readonly(
                accounts::associated_token_address(&reserve_authority, reserve_mint, token_program),
                false,
            ),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: discriminator("execute_rebalance_policy").to_vec(),
    }
}

/// Builds `cancel_rebalance_policy` — discards the pending policy change
/// and frees the singleton slot. Must be placed immediately after the
/// ed25519 proof instruction.
///
/// Requires a FRESH threshold proof binding the specific `eta` being
/// cancelled (`governance::cancel_params`), so a cancel signature cannot
/// be replayed against a later re-proposal. `canceller` receives the
/// closed account's rent and confers no authority.
pub fn cancel_rebalance_policy(canceller: &Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*canceller, true),
            AccountMeta::new_readonly(accounts::bridge_config_pda(), false),
            AccountMeta::new_readonly(accounts::attestation_key_set_pda(), false),
            AccountMeta::new(accounts::pending_rebalance_policy_pda(), false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
        ],
        data: discriminator("cancel_rebalance_policy").to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_is_deterministic_and_action_specific() {
        assert_eq!(
            discriminator("release_from_reserve"),
            discriminator("release_from_reserve")
        );
        assert_ne!(
            discriminator("release_from_reserve"),
            discriminator("record_goldcoin_completion")
        );
    }

    #[test]
    fn release_from_reserve_encodes_args_in_declared_order() {
        let submitter = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let txid = [0xABu8; 32];
        let ix = release_from_reserve(
            &submitter,
            &mint,
            &spl_token::ID,
            &recipient,
            txid,
            3,
            500_000,
            7,
        );
        assert_eq!(&ix.data[0..8], discriminator("release_from_reserve"));
        assert_eq!(&ix.data[8..40], &txid);
        assert_eq!(&ix.data[40..44], &3u32.to_le_bytes());
        assert_eq!(&ix.data[44..52], &500_000u64.to_le_bytes());
        assert_eq!(&ix.data[52..60], &7u64.to_le_bytes());
        assert_eq!(ix.data.len(), 60);
    }

    #[test]
    fn release_from_reserve_has_thirteen_accounts_in_the_declared_order() {
        let submitter = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let ix = release_from_reserve(
            &submitter,
            &mint,
            &spl_token::ID,
            &recipient,
            [0u8; 32],
            0,
            1,
            0,
        );
        assert_eq!(ix.accounts.len(), 13);
        assert_eq!(ix.accounts[0].pubkey, submitter);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert_eq!(ix.accounts[11].pubkey, spl_token::ID);
        assert_eq!(ix.accounts[12].pubkey, solana_sdk::system_program::ID);
    }

    #[test]
    fn release_from_reserve_derives_atas_and_pins_the_supplied_token_program() {
        // Token-2022 (docs/18-token-2022-support.md) derives DIFFERENT ATA
        // addresses than legacy SPL Token for the same (owner, mint) — the
        // ATA PDA's seeds include the token program id. Passing the wrong
        // program here must not silently reuse the legacy addresses.
        let submitter = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let reserve_authority = accounts::reserve_authority_pda();
        let ix = release_from_reserve(
            &submitter,
            &mint,
            &spl_token_2022::ID,
            &recipient,
            [0u8; 32],
            0,
            1,
            0,
        );
        assert_eq!(ix.accounts[11].pubkey, spl_token_2022::ID);
        assert_eq!(
            ix.accounts[7].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token_2022::ID)
        );
        assert_ne!(
            ix.accounts[7].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token::ID)
        );
        assert_eq!(
            ix.accounts[9].pubkey,
            accounts::associated_token_address(&recipient, &mint, &spl_token_2022::ID)
        );
    }

    #[test]
    fn record_goldcoin_completion_encodes_args_in_declared_order() {
        let submitter = Pubkey::new_unique();
        let payout_txid = [0xCDu8; 32];
        let ix = record_goldcoin_completion(&submitter, 9, payout_txid, 12345, 500_000, 2);
        assert_eq!(&ix.data[0..8], discriminator("record_goldcoin_completion"));
        assert_eq!(&ix.data[8..16], &9u64.to_le_bytes());
        assert_eq!(&ix.data[16..48], &payout_txid);
        assert_eq!(&ix.data[48..56], &12345u64.to_le_bytes());
        assert_eq!(&ix.data[56..64], &500_000u64.to_le_bytes());
        assert_eq!(&ix.data[64..72], &2u64.to_le_bytes());
    }

    #[test]
    fn record_goldcoin_completion_has_five_accounts() {
        let submitter = Pubkey::new_unique();
        let ix = record_goldcoin_completion(&submitter, 0, [0u8; 32], 1, 1, 0);
        assert_eq!(ix.accounts.len(), 5);
        assert_eq!(
            ix.accounts[3].pubkey,
            accounts::withdrawal_obligation_pda(0)
        );
    }

    #[test]
    fn set_paused_encodes_scope_and_flag_in_declared_order() {
        let admin = Pubkey::new_unique();
        let ix = set_paused(&admin, PauseScope::Release, true);
        assert_eq!(&ix.data[0..8], discriminator("set_paused"));
        assert_eq!(ix.data[8], PauseScope::Release as u8);
        assert_eq!(ix.data[9], 1);
        assert_eq!(ix.data.len(), 10);
    }

    #[test]
    fn set_paused_scope_discriminants_match_declaration_order() {
        assert_eq!(PauseScope::Global as u8, 0);
        assert_eq!(PauseScope::Release as u8, 1);
        assert_eq!(PauseScope::Deposit as u8, 2);
    }

    #[test]
    fn set_paused_has_two_accounts_admin_signer_then_bridge_config() {
        let admin = Pubkey::new_unique();
        let ix = set_paused(&admin, PauseScope::Global, false);
        assert_eq!(ix.accounts.len(), 2);
        assert_eq!(ix.accounts[0].pubkey, admin);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert!(ix.accounts[1].is_writable);
    }

    #[test]
    fn set_limit_encodes_field_and_new_value_in_declared_order() {
        let admin = Pubkey::new_unique();
        let ix = set_limit(&admin, LimitField::MinTransferAmount, 99_000_000);
        assert_eq!(&ix.data[0..8], discriminator("set_limit"));
        assert_eq!(ix.data[8], LimitField::MinTransferAmount as u8);
        assert_eq!(&ix.data[9..17], &99_000_000u64.to_le_bytes());
        assert_eq!(ix.data.len(), 17);
    }

    #[test]
    fn set_limit_field_discriminants_match_declaration_order() {
        assert_eq!(LimitField::MinTransferAmount as u8, 0);
        assert_eq!(LimitField::PerTransferLimit as u8, 1);
        assert_eq!(LimitField::ProtectedMinimum as u8, 2);
        assert_eq!(LimitField::RollingVolumeLimit as u8, 3);
    }

    #[test]
    fn set_limit_has_two_accounts_admin_signer_then_bridge_config() {
        let admin = Pubkey::new_unique();
        let ix = set_limit(&admin, LimitField::MinTransferAmount, 1);
        assert_eq!(ix.accounts.len(), 2);
        assert_eq!(ix.accounts[0].pubkey, admin);
        assert!(ix.accounts[0].is_signer);
        assert!(!ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert!(ix.accounts[1].is_writable);
    }

    #[test]
    fn reset_rolling_volume_window_encodes_direction_in_declared_order() {
        let admin = Pubkey::new_unique();
        let ix = reset_rolling_volume_window(&admin, RollingWindowDirection::SolanaToGoldcoin);
        assert_eq!(&ix.data[0..8], discriminator("reset_rolling_volume_window"));
        assert_eq!(ix.data[8], RollingWindowDirection::SolanaToGoldcoin as u8);
        assert_eq!(ix.data.len(), 9);
    }

    #[test]
    fn reset_rolling_volume_window_direction_discriminants_match_declaration_order() {
        assert_eq!(RollingWindowDirection::GoldcoinToSolana as u8, 0);
        assert_eq!(RollingWindowDirection::SolanaToGoldcoin as u8, 1);
    }

    #[test]
    fn reset_rolling_volume_window_has_three_accounts_admin_config_window() {
        let admin = Pubkey::new_unique();
        let ix = reset_rolling_volume_window(&admin, RollingWindowDirection::GoldcoinToSolana);
        assert_eq!(ix.accounts.len(), 3);
        assert_eq!(ix.accounts[0].pubkey, admin);
        assert!(ix.accounts[0].is_signer);
        assert!(!ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert!(!ix.accounts[1].is_writable);
        assert_eq!(
            ix.accounts[2].pubkey,
            accounts::rolling_volume_window_pda(0)
        );
        assert!(ix.accounts[2].is_writable);
    }

    #[test]
    fn reset_rolling_volume_window_targets_the_pda_matching_the_given_direction() {
        let admin = Pubkey::new_unique();
        let release = reset_rolling_volume_window(&admin, RollingWindowDirection::GoldcoinToSolana);
        let deposit = reset_rolling_volume_window(&admin, RollingWindowDirection::SolanaToGoldcoin);
        assert_eq!(
            release.accounts[2].pubkey,
            accounts::rolling_volume_window_pda(0)
        );
        assert_eq!(
            deposit.accounts[2].pubkey,
            accounts::rolling_volume_window_pda(1)
        );
        assert_ne!(release.accounts[2].pubkey, deposit.accounts[2].pubkey);
    }

    #[test]
    fn initialize_encodes_attestation_keys_and_scalar_args_in_declared_order() {
        let authority = Pubkey::new_unique();
        let keys = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let ix = initialize(
            &authority, &keys, 2, 3600, 100, 1_000_000, 500, 2_000_000, 3600, 7200,
        );
        assert_eq!(&ix.data[0..8], discriminator("initialize"));
        assert_eq!(&ix.data[8..12], &3u32.to_le_bytes());
        assert_eq!(&ix.data[12..44], keys[0].as_ref());
        assert_eq!(&ix.data[44..76], keys[1].as_ref());
        assert_eq!(&ix.data[76..108], keys[2].as_ref());
        assert_eq!(ix.data[108], 2); // threshold
        assert_eq!(&ix.data[109..117], &3600i64.to_le_bytes());
        assert_eq!(&ix.data[117..125], &100u64.to_le_bytes());
        assert_eq!(&ix.data[125..133], &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[133..141], &500u64.to_le_bytes());
        assert_eq!(&ix.data[141..149], &2_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[149..157], &3600i64.to_le_bytes());
        assert_eq!(&ix.data[157..165], &7200i64.to_le_bytes()); // upgrade_timelock_seconds
        assert_eq!(ix.data.len(), 165);
    }

    #[test]
    fn initialize_has_eight_accounts_authority_signer_first() {
        let authority = Pubkey::new_unique();
        let ix = initialize(&authority, &[], 0, 1, 1, 1, 1, 1, 1, 1);
        assert_eq!(ix.accounts.len(), 8);
        assert_eq!(ix.accounts[0].pubkey, authority);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert_eq!(ix.accounts[2].pubkey, accounts::attestation_key_set_pda());
        assert_eq!(
            ix.accounts[3].pubkey,
            accounts::rolling_volume_window_pda(0)
        );
        assert_eq!(
            ix.accounts[4].pubkey,
            accounts::rolling_volume_window_pda(1)
        );
        assert_eq!(ix.accounts[5].pubkey, PROGRAM_ID);
        assert_eq!(
            ix.accounts[6].pubkey,
            bpf_loader_upgradeable::get_program_data_address(&PROGRAM_ID)
        );
        assert_eq!(ix.accounts[7].pubkey, solana_sdk::system_program::ID);
    }

    #[test]
    fn initialize_reserve_vault_has_eight_accounts_and_derives_the_ata() {
        let admin = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ix = initialize_reserve_vault(&admin, &mint, &spl_token::ID);
        assert_eq!(&ix.data[..], discriminator("initialize_reserve_vault"));
        assert_eq!(ix.accounts.len(), 8);
        assert_eq!(ix.accounts[0].pubkey, admin);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert_eq!(ix.accounts[2].pubkey, mint);
        let reserve_authority = accounts::reserve_authority_pda();
        assert_eq!(ix.accounts[3].pubkey, reserve_authority);
        assert_eq!(
            ix.accounts[4].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token::ID)
        );
        assert_eq!(ix.accounts[5].pubkey, spl_token::ID);
        assert_eq!(ix.accounts[6].pubkey, spl_associated_token_account::ID);
        assert_eq!(ix.accounts[7].pubkey, solana_sdk::system_program::ID);
    }

    #[test]
    fn initialize_reserve_vault_pins_a_token_2022_program_and_ata_when_supplied() {
        let admin = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ix = initialize_reserve_vault(&admin, &mint, &spl_token_2022::ID);
        let reserve_authority = accounts::reserve_authority_pda();
        assert_eq!(ix.accounts[5].pubkey, spl_token_2022::ID);
        assert_eq!(
            ix.accounts[4].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token_2022::ID)
        );
        assert_ne!(
            ix.accounts[4].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token::ID)
        );
    }

    #[test]
    fn deposit_to_reserve_encodes_amount_and_glc_address_in_declared_order() {
        let user = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let addr = b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
        let ix = deposit_to_reserve(&user, &mint, &spl_token::ID, 7, 500_000, addr);
        assert_eq!(&ix.data[0..8], discriminator("deposit_to_reserve"));
        assert_eq!(&ix.data[8..16], &500_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..20], &(addr.len() as u32).to_le_bytes());
        assert_eq!(&ix.data[20..20 + addr.len()], addr);
        assert_eq!(ix.data.len(), 20 + addr.len());
    }

    #[test]
    fn deposit_to_reserve_has_ten_accounts_user_signer_first_and_derives_the_obligation_pda() {
        let user = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ix = deposit_to_reserve(&user, &mint, &spl_token::ID, 3, 1, b"addr");
        assert_eq!(ix.accounts.len(), 10);
        assert_eq!(ix.accounts[0].pubkey, user);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert_eq!(
            ix.accounts[2].pubkey,
            accounts::rolling_volume_window_pda(1)
        );
        assert_eq!(ix.accounts[3].pubkey, mint);
        assert_eq!(
            ix.accounts[4].pubkey,
            accounts::associated_token_address(&user, &mint, &spl_token::ID)
        );
        let reserve_authority = accounts::reserve_authority_pda();
        assert_eq!(ix.accounts[5].pubkey, reserve_authority);
        assert_eq!(
            ix.accounts[6].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token::ID)
        );
        assert_eq!(
            ix.accounts[7].pubkey,
            accounts::withdrawal_obligation_pda(3)
        );
        assert_eq!(ix.accounts[8].pubkey, spl_token::ID);
        assert_eq!(ix.accounts[9].pubkey, solana_sdk::system_program::ID);
    }

    #[test]
    fn deposit_to_reserve_pins_a_token_2022_program_and_atas_when_supplied() {
        let user = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ix = deposit_to_reserve(&user, &mint, &spl_token_2022::ID, 3, 1, b"addr");
        let reserve_authority = accounts::reserve_authority_pda();
        assert_eq!(ix.accounts[8].pubkey, spl_token_2022::ID);
        assert_eq!(
            ix.accounts[4].pubkey,
            accounts::associated_token_address(&user, &mint, &spl_token_2022::ID)
        );
        assert_eq!(
            ix.accounts[6].pubkey,
            accounts::associated_token_address(&reserve_authority, &mint, &spl_token_2022::ID)
        );
    }

    /// docs/22-production-readiness-review.md P0-6: every builder in this
    /// file sets `Instruction { program_id: PROGRAM_ID, .. }` — pinned
    /// directly here (distinct from the account-derivation assertions
    /// above, which would pass identically against any consistent value)
    /// so a regression back to the program's old scaffold/dev id is
    /// caught even if every other test here somehow still passed.
    #[test]
    fn every_builder_targets_the_deployed_mainnet_program_id() {
        let expected = solana_sdk::pubkey!("6tmLSP2j2thito2RpByqgfKHuVRSLcNd9c5FkrLJMjja");
        assert_eq!(PROGRAM_ID, expected);
        let admin = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        assert_eq!(
            initialize(&admin, &[], 0, 1, 1, 1, 1, 1, 1, 1).program_id,
            expected
        );
        assert_eq!(
            initialize_reserve_vault(&admin, &mint, &spl_token::ID).program_id,
            expected
        );
        assert_eq!(
            release_from_reserve(&admin, &mint, &spl_token::ID, &admin, [0u8; 32], 0, 1, 0)
                .program_id,
            expected
        );
        assert_eq!(
            record_goldcoin_completion(&admin, 0, [0u8; 32], 1, 1, 0).program_id,
            expected
        );
        assert_eq!(
            deposit_to_reserve(&admin, &mint, &spl_token::ID, 0, 1, b"addr").program_id,
            expected
        );
        assert_eq!(
            set_paused(&admin, PauseScope::Global, false).program_id,
            expected
        );
        assert_eq!(
            set_limit(&admin, LimitField::MinTransferAmount, 1).program_id,
            expected
        );
        assert_eq!(
            reset_rolling_volume_window(&admin, RollingWindowDirection::GoldcoinToSolana)
                .program_id,
            expected
        );
    }
}
