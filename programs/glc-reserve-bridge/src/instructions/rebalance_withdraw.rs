//! **RETIRED.** The operator-initiated reserve withdrawal that accepted an
//! ARBITRARY destination token account.
//!
//! # Why it is gone
//!
//! This instruction required two factors — the admin's signature AND a
//! threshold attestation over the exact nonce/amount/destination/mint — and
//! it enforced the global pause, the protected minimum, and a per-nonce
//! replay guard. Every one of those checks worked as designed.
//!
//! It was still the path used to drain the reserve on 2026-09-02, because
//! the two factors were not independent in deployment: the admin keypair
//! and the credentials that reach the attestation signers were resident on
//! the same production host, and the signers were blind oracles that would
//! sign any bytes presented with a valid bearer token. With both factors
//! reachable from one shell, the only remaining bound on the withdrawal was
//! `BridgeConfig.protected_minimum` — which the same admin key can set to
//! zero via `set_limit`. The destination was unconstrained, so the funds
//! went to an account the operator controlled.
//!
//! The lesson encoded here is that "two factors" is a deployment property,
//! not a code property, and an instruction whose blast radius is "the
//! entire reserve, to anywhere" must not depend on it. The replacements
//! bound the blast radius in the program itself, where no host compromise
//! can reach:
//!
//! - [`crate::instructions::treasury_withdraw`] — destination must appear
//!   verbatim in the threshold-governed, timelocked
//!   [`crate::state::RebalancePolicy`] allowlist; amount is bounded by a
//!   dedicated per-withdrawal limit and a dedicated rolling limit; the
//!   policy revision is bound into the signed claim.
//! - [`crate::instructions::refund_withdraw`] — destination is DERIVED from
//!   the depositor's own `WithdrawalObligation`, so it can only ever return
//!   funds to the wallet that sent them.
//!
//! Between them there is no remaining code path from an operator-supplied
//! destination to the reserve.
//!
//! # Why the instruction still exists at all
//!
//! It fails closed with [`BridgeError::RebalanceWithdrawRetired`] rather
//! than being deleted outright, so that:
//!
//! - stale tooling, a stale attested plan file, or a replayed transaction
//!   from before the upgrade fails LOUDLY with an error naming its
//!   replacement, instead of failing with an opaque
//!   `InstructionFallbackNotFound` that reads like a deployment mistake;
//! - the negative test in `tests/incident_replay.rs` can present the exact
//!   transaction shape the incident used — real admin signature, real
//!   2-of-3 attestation, bridge genuinely paused, amount within the
//!   protected minimum — and prove no funds move.
//!
//! The account context is kept intact deliberately: the handler rejects
//! BEFORE any transfer, and Anchor unwinds the whole transaction on error,
//! so the `init` of the replay-guard PDA in this context never persists and
//! the nonce it names is not burned.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::ID as INSTRUCTIONS_SYSVAR_ID;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::constants::{
    SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_REBALANCE_WITHDRAWAL, SEED_RESERVE_AUTHORITY,
};
use crate::errors::BridgeError;
use crate::state::{AttestationKeySet, BridgeConfig, RebalanceWithdrawal};

#[derive(Accounts)]
#[instruction(nonce: u64)]
pub struct RebalanceWithdraw<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin,
        constraint = bridge_config.reserve_token_mint != Pubkey::default()
            @ BridgeError::ReserveNotConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

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

/// Always fails. See module docs.
///
/// The rejection is the FIRST statement in the handler, before any state is
/// read or written and long before any CPI, so there is no ordering in
/// which this instruction can partially execute. It ignores its arguments
/// entirely: there is no combination of nonce, amount or attestation epoch
/// that makes it succeed, and no attestation — however valid — that
/// authorizes it.
pub fn rebalance_withdraw(
    _ctx: Context<RebalanceWithdraw>,
    _nonce: u64,
    _amount: u64,
    _attestation_epoch: u64,
) -> Result<()> {
    Err(BridgeError::RebalanceWithdrawRetired.into())
}
