//! Operator-initiated reserve withdrawal to an ALLOWLISTED treasury token
//! account — the bounded replacement for the retired
//! [`crate::instructions::rebalance_withdraw`], whose destination was
//! unconstrained (see that module's docs for the incident that retired it).
//!
//! # The authorization stack, in the order it is enforced
//!
//! Every check the retired instruction performed is preserved here
//! unchanged, and four are added. Nothing was traded away to make room:
//!
//! | # | check | source |
//! |---|-------|--------|
//! | 1 | bridge is GLOBALLY PAUSED | preserved |
//! | 2 | `admin` signed | preserved |
//! | 3 | attestation epoch is current | preserved |
//! | 4 | `amount > 0` | preserved |
//! | 5 | **nonce is in the treasury namespace** | NEW |
//! | 6 | **on-chain policy exists and is internally valid** | NEW |
//! | 7 | **destination is verbatim in the policy allowlist** | NEW |
//! | 8 | destination is not the reserve account itself | preserved |
//! | 9 | mint/token-account extensions re-reviewed | preserved |
//! | 10 | **amount within the dedicated per-withdrawal limit** | NEW |
//! | 11 | `protected_minimum` preserved | preserved |
//! | 12 | **amount within the dedicated rolling limit** | NEW |
//! | 13 | threshold attestation over the canonical claim | preserved |
//! | 14 | per-nonce replay guard (`init` on the record PDA) | preserved |
//!
//! # Why the allowlist is the load-bearing change
//!
//! Checks 1–4 and 13 were all satisfied during the incident: the pause was
//! real, the admin signature was real, and the attestations were genuine
//! signatures from genuine current attestation keys over the exact bytes
//! being executed. They failed to stop anything because both factors were
//! reachable from one compromised host.
//!
//! Check 7 does not depend on any host, any credential, or any operator
//! decision made at withdrawal time. The set of addresses the reserve can
//! pay out to is fixed in advance, on chain, by a threshold of attestation
//! keys plus a public timelock. An attacker holding the admin key and every
//! signer credential simultaneously still cannot name a destination that is
//! not already on that list — the most they can do is move reserve funds to
//! the operator's own treasury, which is loud, bounded and reversible in a
//! way an anonymous external account is not.
//!
//! Checks 10 and 12 bound what a single compromised approval can cost even
//! when the destination IS legitimate — the case where the treasury custody
//! itself is what went wrong.
//!
//! # What the attestation signers are approving
//!
//! [`glc_reserve_bridge_shared::claim::treasury_withdraw_claim_message`]
//! binds the nonce, the amount, the destination, the reserve mint, the
//! source reserve token account, and — new to this family — the
//! [`RebalancePolicy::version`] the withdrawal is being authorized under.
//! The policy binding is what stops an approval gathered while a treasury
//! was still allowlisted from being held and replayed after governance
//! removed it.
//!
//! Signers are expected to parse those bytes and refuse anything that does
//! not match their OWN independently-held view of the treasury and the
//! limits — see `docs/28-signer-policy.md`. This program cannot enforce
//! that, which is precisely why the allowlist is also enforced here.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    get_instruction_relative, ID as INSTRUCTIONS_SYSVAR_ID,
};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use glc_reserve_bridge_shared::claim::treasury_withdraw_claim_message;

use crate::constants::{
    NONCE_DOMAIN_REFUND, SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_REBALANCE_POLICY,
    SEED_REBALANCE_WITHDRAWAL, SEED_RESERVE_AUTHORITY, WITHDRAWAL_CLASS_TREASURY,
};
use crate::errors::BridgeError;
use crate::events::RebalanceWithdrawalExecuted;
use crate::limits::{enforce_and_record_rebalance_volume, enforce_protected_minimum};
use crate::state::{AttestationKeySet, BridgeConfig, RebalancePolicy, RebalanceWithdrawal};
use crate::token_extensions::{validate_mint_extensions, validate_token_account_extensions};
use crate::verification::count_unique_attestation_signers;

#[derive(Accounts)]
#[instruction(nonce: u64)]
pub struct TreasuryWithdraw<'info> {
    /// Co-authorizes this specific withdrawal and pays the withdrawal
    /// record's rent. Signature alone confers no authority — a valid
    /// threshold attestation AND an allowlisted destination are both
    /// additionally required (module docs above).
    #[account(mut)]
    pub admin: Signer<'info>,

    /// Not `mut`: this instruction never writes to `BridgeConfig`. The
    /// retired instruction requested write access it did not use; a
    /// withdrawal has no business being able to modify governance state,
    /// so the capability is dropped rather than carried forward.
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

    /// The treasury allowlist and the dedicated limits. `mut` because the
    /// rolling-window fields are recorded here on success.
    ///
    /// If this account does not exist, Anchor's own deserialization fails
    /// the instruction before the handler runs — the fail-closed default
    /// this design wants: no policy means no allowlisted destination means
    /// no authorized withdrawal. There is deliberately no "policy absent
    /// implies unrestricted" branch anywhere in this file.
    #[account(mut, seeds = [SEED_REBALANCE_POLICY], bump = rebalance_policy.bump)]
    pub rebalance_policy: Account<'info, RebalancePolicy>,

    /// Existence of this account is the on-chain replay guard: reusing a
    /// `nonce` fails right here at `init`, exactly like `DepositClaim`.
    /// Shares one PDA namespace with `refund_withdraw`; the two classes are
    /// kept disjoint inside it by [`NONCE_DOMAIN_REFUND`].
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

    /// The withdrawal's destination. The mint/token-program constraints
    /// below are necessary but nowhere near sufficient — they were exactly
    /// what the retired instruction relied on, and they admit every token
    /// account of the reserve mint in existence. The real check is the
    /// allowlist lookup in the handler.
    #[account(
        mut,
        token::mint = reserve_mint,
        token::token_program = token_program,
    )]
    pub destination_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn treasury_withdraw(
    ctx: Context<TreasuryWithdraw>,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;
    let destination = ctx.accounts.destination_token_account.key();
    let reserve_token_account = ctx.accounts.reserve_token_account.key();

    // (1) The bridge must ALREADY be paused — a separate, independently
    // logged/audited action (`set_paused`). Preserved exactly as the
    // retired instruction had it; the incident review explicitly declined
    // to relax it, even though a compromised admin can satisfy it, because
    // it remains a real barrier to an accidental withdrawal and a real
    // signal in the transaction log.
    require!(config.paused, BridgeError::BridgeNotPaused);

    // (3)(4)
    require!(
        attestation_epoch == key_set.epoch,
        BridgeError::StaleAttestationEpoch
    );
    require!(amount > 0, BridgeError::ZeroRebalanceAmount);

    // (5) Namespace separation, now structural rather than conventional: a
    // treasury withdrawal may never consume a nonce from the refund
    // namespace, so neither class can burn the other's replay-guard slot.
    require!(
        nonce & NONCE_DOMAIN_REFUND == 0,
        BridgeError::WrongNonceNamespace
    );

    // (6) The policy account exists (Anchor proved that above) — but its
    // CONTENTS are re-validated here rather than trusted. A policy that
    // somehow failed to satisfy its own invariants must stop a withdrawal,
    // not silently permit one: `treasury_count == 0` would make the
    // allowlist check below vacuously false, but a zero `rolling_limit`
    // would make the budget check vacuously true, which is the direction
    // that loses money.
    let policy = &ctx.accounts.rebalance_policy;
    require!(
        policy.treasury_count >= 1
            && usize::from(policy.treasury_count) <= crate::constants::MAX_TREASURY_DESTINATIONS
            && policy.rolling_limit > 0
            && policy.rolling_window_seconds > 0,
        BridgeError::InvalidRebalancePolicy
    );

    // (7) THE check. Exact address equality against the governed allowlist.
    require!(
        policy.is_allowlisted(&destination),
        BridgeError::DestinationNotAllowlisted
    );

    // (8) Preserved. Redundant given a valid policy — `validate_rebalance_
    // policy` already refuses to allowlist the reserve account — but kept
    // as an independent backstop that does not depend on the policy having
    // been written correctly.
    require!(
        destination != reserve_token_account,
        BridgeError::RebalanceDestinationIsReserveItself
    );

    // (9) Re-reviewed on every call, not just at vault setup — same
    // discipline as `release_from_reserve`.
    validate_mint_extensions(&ctx.accounts.reserve_mint.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.reserve_token_account.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.destination_token_account.to_account_info())?;

    // (11) Protected accounting (constraint 6): identical function,
    // identical live-balance read, as `release_from_reserve` — an operator
    // withdrawal can no more breach `protected_minimum` than a bridge
    // settlement can.
    let reserve_balance_before = ctx.accounts.reserve_token_account.amount;
    enforce_protected_minimum(reserve_balance_before, config.protected_minimum, amount)?;

    // (13) The threshold attestation: the instruction directly before this
    // one must be the ed25519 precompile carrying >= threshold unique
    // current attestation-key signatures over exactly the canonical
    // treasury-withdrawal claim bytes. Same verification mechanism and same
    // threshold as `release_from_reserve`, over a message that can never be
    // confused with a release, a completion, a refund, or the retired
    // rebalance claim (distinct action byte AND distinct total length).
    //
    // Deliberately verified BEFORE the rolling window is recorded below:
    // an unauthorized attempt must not consume any of the withdrawal
    // budget. (The transaction would unwind on error regardless; the
    // ordering makes that independent of Anchor's unwinding.)
    let verification_ix =
        get_instruction_relative(-1, &ctx.accounts.instructions_sysvar.to_account_info())
            .map_err(|_| BridgeError::MissingSignatureVerification)?;
    require!(
        verification_ix.program_id == anchor_lang::solana_program::ed25519_program::ID,
        BridgeError::MissingSignatureVerification
    );
    let expected_message = treasury_withdraw_claim_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        nonce,
        amount,
        &destination.to_bytes(),
        &config.reserve_token_mint.to_bytes(),
        &reserve_token_account.to_bytes(),
        policy.version,
    );
    let signer_count =
        count_unique_attestation_signers(&verification_ix.data, &expected_message, &key_set.keys)?;
    require!(
        signer_count >= usize::from(key_set.threshold),
        BridgeError::InsufficientSignatures
    );

    // (12) Dedicated rolling budget, recorded only now that everything
    // else has passed.
    let now = Clock::get()?.unix_timestamp;
    enforce_and_record_rebalance_volume(&mut ctx.accounts.rebalance_policy, amount, now)?;

    let policy_version = ctx.accounts.rebalance_policy.version;
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

    let record = &mut ctx.accounts.rebalance_withdrawal;
    record.nonce = nonce;
    record.amount = amount;
    record.destination = destination;
    record.admin = ctx.accounts.admin.key();
    record.attestation_epoch = attestation_epoch;
    record.protocol_version = ctx.accounts.bridge_config.protocol_version;
    record.slot_created = Clock::get()?.slot;
    record.bump = ctx.bumps.rebalance_withdrawal;
    record.reserved = [0u8; 16];
    record.reserved[0] = WITHDRAWAL_CLASS_TREASURY;

    // Safe: `enforce_protected_minimum` above already proved
    // `reserve_balance_before >= amount + protected_minimum >= amount`.
    let reserve_balance_after = reserve_balance_before
        .checked_sub(amount)
        .ok_or(BridgeError::ArithmeticUnderflow)?;

    msg!(
        "treasury_withdraw: {} atomic units to allowlisted treasury {} under policy version {}",
        amount,
        destination,
        policy_version
    );
    emit!(RebalanceWithdrawalExecuted {
        nonce,
        destination: record.destination,
        amount,
        attestation_epoch: record.attestation_epoch,
        admin: record.admin,
        reserve_balance_after,
        class: WITHDRAWAL_CLASS_TREASURY,
    });
    Ok(())
}
