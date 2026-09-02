//! ManualReview refund: returns one specific `WithdrawalObligation`'s
//! deposit to the wallet that made it.
//!
//! Carved out of the retired [`crate::instructions::rebalance_withdraw`]
//! alongside [`crate::instructions::treasury_withdraw`]. The two classes
//! were always semantically distinct — one moves operator funds to an
//! operator destination, the other returns a user's own funds to that same
//! user — but they shared one unrestricted instruction, so the program
//! could not tell them apart and had to permit the union of what both
//! needed. That union was "any destination", and it is what the 2026-09-02
//! incident used.
//!
//! # Why this class cannot use an allowlist, and does not need one
//!
//! A refund's destination is, by definition, a member of the public: the
//! depositor. No fixed list can contain it. So instead of constraining the
//! destination to a set, this instruction DERIVES it — it must be the
//! canonical associated token account of the obligation's own recorded
//! `requester`, for the configured reserve mint under the configured token
//! program.
//!
//! That derivation is enforced structurally, by Anchor's
//! `associated_token::{mint, authority, token_program}` constraints, not by
//! a runtime comparison the handler could forget. The `authority` in that
//! derivation is an account address-pinned to `withdrawal_obligation
//! .requester`, a value written by `deposit_to_reserve` at the moment the
//! user's own transfer landed and never mutable afterwards.
//!
//! The result is a destination bound as tightly as an allowlist entry,
//! without a list: an operator running this instruction chooses WHICH
//! obligation to refund, and nothing else. They cannot choose where the
//! money goes, because the depositor already did, by depositing.
//!
//! # What is bounded, and what deliberately is not
//!
//! There is no amount limit here — as with `treasury_withdraw`, and for a
//! reason specific to this path. A refund is bounded by something strictly
//! tighter than any configurable number:
//!
//! - `amount` must equal `obligation.amount` EXACTLY. Not "at most": a
//!   partial refund is not a refund, and an over-refund would be a
//!   withdrawal wearing a refund's clothes.
//! - the obligation must still be `Pending`, so an obligation already paid
//!   out on Goldcoin can never also be refunded on Solana.
//! - the per-nonce replay guard means one nonce, one refund; and the
//!   off-chain ledger derives that nonce deterministically from the request
//!   id (`Ledger::solana_refund_nonce`), so one request maps to one nonce
//!   forever, even across a database restore.
//!
//! Applying an amount cap or a rolling budget on top would add no security
//! — the ceiling is already "the sum of what users actually deposited and
//! did not receive" — and would introduce a failure mode where legitimate
//! refunds queue behind a quota during exactly the kind of incident that
//! generates them.
//!
//! # Known residual gap (deliberately not closed in this patch)
//!
//! This instruction does NOT mark the obligation as refunded, so on-chain
//! nothing prevents a second refund of the same obligation under a
//! different nonce. Today that is prevented off-chain by the ledger's
//! `solana_refunds` primary key, exactly as it was before this patch — the
//! guarantee is unchanged, not weakened.
//!
//! Closing it properly means adding a `WithdrawalStatus::Refunded` variant,
//! which changes a wire value that several off-chain decoders match on
//! (`service::solana::refund`, `accounts`, `indexer`,
//! `manual_review_settle`). That is a correct change and it is recommended,
//! but it is a settlement-path change, and this patch is scoped to the
//! withdrawal path. Tracked in `docs/29-reserve-withdrawal-hardening.md`
//! as follow-up F-8.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    get_instruction_relative, ID as INSTRUCTIONS_SYSVAR_ID,
};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use glc_reserve_bridge_shared::claim::refund_withdraw_claim_message;

use crate::constants::{
    NONCE_DOMAIN_REFUND, SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_REBALANCE_WITHDRAWAL,
    SEED_RESERVE_AUTHORITY, SEED_WITHDRAWAL_OBLIGATION, WITHDRAWAL_CLASS_REFUND,
};
use crate::errors::BridgeError;
use crate::events::RebalanceWithdrawalExecuted;
use crate::limits::enforce_protected_minimum;
use crate::state::{
    AttestationKeySet, BridgeConfig, RebalanceWithdrawal, WithdrawalObligation, WithdrawalStatus,
};
use crate::token_extensions::{validate_mint_extensions, validate_token_account_extensions};
use crate::verification::count_unique_attestation_signers;

#[derive(Accounts)]
#[instruction(nonce: u64, amount: u64, attestation_epoch: u64, obligation_index: u64)]
pub struct RefundWithdraw<'info> {
    /// Co-authorizes this specific refund and pays the withdrawal record's
    /// rent. As everywhere else in this program, the signature alone
    /// confers no authority.
    #[account(mut)]
    pub admin: Signer<'info>,

    /// Not `mut`: this instruction never writes to `BridgeConfig`.
    #[account(
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin,
        constraint = bridge_config.reserve_token_mint != Pubkey::default()
            @ BridgeError::ReserveNotConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// The deposit being returned. Seeded directly from the instruction's
    /// own `obligation_index` argument, so the caller cannot substitute a
    /// different obligation than the one their attestation covers: the
    /// index is in the signed claim, and it is the seed.
    #[account(
        seeds = [SEED_WITHDRAWAL_OBLIGATION, &obligation_index.to_le_bytes()],
        bump = withdrawal_obligation.bump,
    )]
    pub withdrawal_obligation: Account<'info, WithdrawalObligation>,

    /// CHECK: the original depositor. Not read as data — it exists solely
    /// so `destination_token_account`'s `associated_token::authority`
    /// constraint has an account to derive from, and it is address-pinned
    /// to the obligation's own immutable `requester` field. This is the
    /// hinge of the whole instruction: the destination is derived from
    /// this, and this is derived from what the depositor actually did.
    #[account(address = withdrawal_obligation.requester)]
    pub requester: UncheckedAccount<'info>,

    /// Existence of this account is the on-chain replay guard. Shares one
    /// PDA namespace with `treasury_withdraw`; the classes are kept
    /// disjoint inside it by [`NONCE_DOMAIN_REFUND`].
    #[account(
        init,
        payer = admin,
        space = RebalanceWithdrawal::SPACE,
        seeds = [SEED_REBALANCE_WITHDRAWAL, &nonce.to_le_bytes()],
        bump
    )]
    pub rebalance_withdrawal: Account<'info, RebalanceWithdrawal>,

    #[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]
    pub reserve_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: data-less PDA, sole authority over the reserve token account.
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
        associated_token::token_program = token_program,
    )]
    pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The depositor's canonical ATA — DERIVED, never supplied as a free
    /// choice. `associated_token::` rather than the retired instruction's
    /// permissive `token::`: this constraint recomputes the address from
    /// (requester, reserve mint, reserve token program) and rejects any
    /// account that is not it, so there is no destination an operator can
    /// name here that the depositor does not already own.
    ///
    /// The account must already exist. Creating it is the submitting
    /// tooling's job (`service::solana::refund` prepends an idempotent ATA
    /// creation instruction), deliberately kept out of this program: an
    /// instruction that both creates accounts and moves reserve funds has
    /// a larger surface than one that only moves them.
    #[account(
        mut,
        associated_token::mint = reserve_mint,
        associated_token::authority = requester,
        associated_token::token_program = token_program,
    )]
    pub destination_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn refund_withdraw(
    ctx: Context<RefundWithdraw>,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
    obligation_index: u64,
) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;
    let obligation = &ctx.accounts.withdrawal_obligation;
    let destination = ctx.accounts.destination_token_account.key();
    let reserve_token_account = ctx.accounts.reserve_token_account.key();

    // Preserved from the retired instruction: the bridge must already be
    // globally paused. Refunds inherited this precondition and keep it —
    // the incident review declined to relax any existing pause requirement.
    require!(config.paused, BridgeError::BridgeNotPaused);
    require!(
        attestation_epoch == key_set.epoch,
        BridgeError::StaleAttestationEpoch
    );
    require!(amount > 0, BridgeError::ZeroRebalanceAmount);

    // Namespace separation, mirroring `treasury_withdraw`'s inverse check:
    // a refund must consume a nonce from the refund half of the space, the
    // half `Ledger::solana_refund_nonce` derives into. This makes the
    // previously conventional split structural.
    require!(
        nonce & NONCE_DOMAIN_REFUND != 0,
        BridgeError::WrongNonceNamespace
    );

    // The obligation must be an unsettled deposit. `Broadcast` and
    // `Completed` both mean a Goldcoin payout is already in flight or
    // done; refunding either would pay the same deposit twice, on two
    // chains.
    require!(
        obligation.status == WithdrawalStatus::Pending,
        BridgeError::ObligationNotPending
    );

    // Exactly the deposited amount — see module docs on why this is `==`
    // and not `<=`.
    require!(
        amount == obligation.amount,
        BridgeError::RefundAmountMismatch
    );

    // Re-reviewed on every call, same discipline as every other
    // reserve-touching instruction.
    validate_mint_extensions(&ctx.accounts.reserve_mint.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.reserve_token_account.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.destination_token_account.to_account_info())?;

    // Protected accounting: unchanged, and it applies to refunds exactly as
    // it applies to settlements. A reserve too depleted to refund without
    // breaching its floor is a situation for the operator to resolve
    // deliberately, not one for this instruction to resolve by ignoring the
    // floor.
    let reserve_balance_before = ctx.accounts.reserve_token_account.amount;
    enforce_protected_minimum(reserve_balance_before, config.protected_minimum, amount)?;

    // The threshold attestation, over a claim that binds the obligation
    // index and the requester in addition to the usual nonce/amount/
    // destination/mint. A treasury approval can never satisfy this (byte 57
    // differs, and the lengths differ), and a refund approval can never
    // satisfy `treasury_withdraw`.
    let verification_ix =
        get_instruction_relative(-1, &ctx.accounts.instructions_sysvar.to_account_info())
            .map_err(|_| BridgeError::MissingSignatureVerification)?;
    require!(
        verification_ix.program_id == anchor_lang::solana_program::ed25519_program::ID,
        BridgeError::MissingSignatureVerification
    );
    let expected_message = refund_withdraw_claim_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        nonce,
        amount,
        &destination.to_bytes(),
        &config.reserve_token_mint.to_bytes(),
        &reserve_token_account.to_bytes(),
        obligation_index,
        &obligation.requester.to_bytes(),
    );
    let signer_count =
        count_unique_attestation_signers(&verification_ix.data, &expected_message, &key_set.keys)?;
    require!(
        signer_count >= usize::from(key_set.threshold),
        BridgeError::InsufficientSignatures
    );

    let bump = config.reserve_authority_bump;
    let seeds: &[&[u8]] = &[SEED_RESERVE_AUTHORITY, &[bump]];
    let signer_seeds: &[&[&[u8]]] = &[seeds];
    let cpi_accounts = TransferChecked {
        from: ctx.accounts.reserve_token_account.to_account_info(),
        mint: ctx.accounts.reserve_mint.to_account_info(),
        to: ctx.accounts.destination_token_account.to_account_info(),
        authority: ctx.accounts.reserve_authority.to_account_info(),
    };
    let cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        cpi_accounts,
        signer_seeds,
    );
    token_interface::transfer_checked(cpi_ctx, amount, ctx.accounts.reserve_mint.decimals)?;

    let admin_key = ctx.accounts.admin.key();
    let protocol_version = ctx.accounts.bridge_config.protocol_version;
    let record = &mut ctx.accounts.rebalance_withdrawal;
    record.nonce = nonce;
    record.amount = amount;
    record.destination = destination;
    record.admin = admin_key;
    record.attestation_epoch = attestation_epoch;
    record.protocol_version = protocol_version;
    record.slot_created = Clock::get()?.slot;
    record.bump = ctx.bumps.rebalance_withdrawal;
    record.reserved = [0u8; 16];
    record.reserved[0] = WITHDRAWAL_CLASS_REFUND;

    let reserve_balance_after = reserve_balance_before
        .checked_sub(amount)
        .ok_or(BridgeError::ArithmeticUnderflow)?;

    msg!(
        "refund_withdraw: obligation {} returned to depositor ATA {}",
        obligation_index,
        destination
    );
    emit!(RebalanceWithdrawalExecuted {
        nonce,
        destination,
        amount,
        attestation_epoch,
        admin: admin_key,
        reserve_balance_after,
        class: WITHDRAWAL_CLASS_REFUND,
    });
    Ok(())
}
