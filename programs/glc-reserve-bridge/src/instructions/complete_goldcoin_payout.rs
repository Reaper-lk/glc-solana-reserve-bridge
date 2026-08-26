//! `record_goldcoin_completion` — records that a withdrawal obligation was
//! paid on Goldcoin. Replaces the old bridge's `complete_withdrawal`
//! (docs/01-reuse-inventory.md: same mechanism and rationale).
//!
//! # What this closes
//!
//! Without this instruction, nothing could advance `WithdrawalStatus` past
//! `Pending`, so the only record distinguishing a paid obligation from an
//! unpaid one would live in the off-chain service's local database —
//! exactly the asymmetry docs/02-trust-model.md names: the Solana ->
//! Goldcoin direction has no on-chain replay guard of its own, because
//! Goldcoin has no program layer. This instruction is the best available
//! mitigation: it makes the *completion* fact reconstructable from Solana
//! chain state, verified by the same threshold attestation as a release,
//! even though the *payout itself* happened off-chain and cannot be
//! verified by this program directly.
//!
//! # Authorization reuses the audited path verbatim
//!
//! Exactly as `release_from_reserve`: the instruction immediately before
//! this one must be the ed25519 precompile carrying >= threshold unique
//! current attestation-key signatures over the canonical completion
//! message. Each signer independently verified the Goldcoin payout against
//! its own chain read before signing (constraint 3, 4) — this program
//! cannot verify that itself, since it cannot observe Goldcoin.
//!
//! # Completion is terminal, deliberately
//!
//! There is no reversal path. One would let anyone able to force a
//! Goldcoin reorg *un-complete* an obligation and induce a second payout —
//! strictly worse than the problem it solves. A deep reorg after
//! completion is a halt-and-reconcile incident requiring operator
//! judgement (docs/10-threat-model.md's "post-finality reorg" section),
//! not an automatic on-chain state change.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_lang::solana_program::sysvar::instructions::get_instruction_relative;

use glc_reserve_bridge_shared::claim::goldcoin_completion_message;

use crate::constants::{SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_WITHDRAWAL_OBLIGATION};
use crate::errors::BridgeError;
use crate::events::ObligationCompleted;
use crate::state::{AttestationKeySet, BridgeConfig, WithdrawalObligation, WithdrawalStatus};
use crate::verification::count_unique_attestation_signers;

#[derive(Accounts)]
#[instruction(index: u64)]
pub struct CompleteGoldcoinPayout<'info> {
    /// Pays the transaction fee. Confers NO authority: the threshold
    /// attestation is what authorizes, and this account is not consulted
    /// for it.
    #[account(mut)]
    pub submitter: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// The obligation being completed. Address-pinned to the PDA its own
    /// index derives, so a caller cannot substitute a different record.
    #[account(
        mut,
        seeds = [SEED_WITHDRAWAL_OBLIGATION, &index.to_le_bytes()],
        bump = obligation.bump,
    )]
    pub obligation: Account<'info, WithdrawalObligation>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

pub fn record_goldcoin_completion(
    ctx: Context<CompleteGoldcoinPayout>,
    index: u64,
    payout_txid: [u8; 32],
    payout_height: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;

    require!(!config.paused, BridgeError::BridgeGloballyPaused);
    require!(amount > 0, BridgeError::ZeroAmount);
    require!(
        attestation_epoch == key_set.epoch,
        BridgeError::StaleAttestationEpoch
    );

    // Terminal-state guard. The status field IS the replay guard for this
    // direction — the closest analog available to a claim PDA's `init`
    // constraint, given Goldcoin has no program layer of its own
    // (docs/02-trust-model.md asymmetry note).
    let obligation = &ctx.accounts.obligation;
    require!(
        obligation.status != WithdrawalStatus::Completed,
        BridgeError::ObligationAlreadyCompleted
    );
    require!(
        obligation.status == WithdrawalStatus::Pending,
        BridgeError::ObligationNotPending
    );
    require!(
        obligation.payout_record_is_unset(),
        BridgeError::PayoutRecordAlreadySet
    );
    require!(payout_txid != [0u8; 32], BridgeError::ZeroPayoutTxid);
    require!(payout_height > 0, BridgeError::ZeroPayoutHeight);

    let verification_ix =
        get_instruction_relative(-1, &ctx.accounts.instructions_sysvar.to_account_info())
            .map_err(|_| BridgeError::MissingSignatureVerification)?;
    require!(
        verification_ix.program_id == anchor_lang::solana_program::ed25519_program::ID,
        BridgeError::MissingSignatureVerification
    );

    // The destination is committed by hashing the address bytes exactly as
    // stored, so the program never needs to interpret an address format
    // (same reasoning as the old bridge's ADR-0018 D6).
    let len = usize::from(obligation.glc_address_len);
    require!(
        len >= 1 && len <= obligation.glc_address.len(),
        BridgeError::InvalidGlcAddressLength
    );
    let dest_commitment: [u8; 32] = hash(&obligation.glc_address[..len]).to_bytes();

    // `amount` here is the NET Goldcoin amount actually paid out (the
    // gross deposit minus the off-chain bridge fee) — the fact each
    // attestation signer independently verified against its own real
    // Goldcoin chain read before signing, not `obligation.amount` (the
    // GROSS Solana-side deposit, which is 6% larger and was never what
    // moved on Goldcoin). This program has no fee policy of its own
    // (docs/20-bridge-fee.md: the fee is off-chain policy, not an
    // on-chain rule), so it cannot derive the net figure itself — it is a
    // caller-supplied argument, exactly like `release_from_reserve`'s own
    // `amount` argument, verified only by requiring it to be covered by
    // the already-checked threshold signature, never trusted on its own.
    let expected_message = goldcoin_completion_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        index,
        &payout_txid,
        payout_height,
        amount,
        &dest_commitment,
    );
    let signer_count =
        count_unique_attestation_signers(&verification_ix.data, &expected_message, &key_set.keys)?;
    require!(
        signer_count >= usize::from(key_set.threshold),
        BridgeError::InsufficientSignatures
    );

    let obligation = &mut ctx.accounts.obligation;
    obligation.set_payout_record(&payout_txid, payout_height);
    // Status last: every guard above reads it.
    obligation.status = WithdrawalStatus::Completed;

    emit!(ObligationCompleted {
        index,
        payout_txid,
        payout_height,
        amount,
        attestation_epoch: key_set.epoch,
    });

    Ok(())
}
