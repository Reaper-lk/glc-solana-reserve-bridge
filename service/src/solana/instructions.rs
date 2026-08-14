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
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;

use super::accounts::{self, PROGRAM_ID};

fn discriminator(instruction_name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("global:{instruction_name}"));
    hash[..8].try_into().unwrap()
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
                accounts::associated_token_address(&reserve_authority, reserve_mint),
                false,
            ),
            AccountMeta::new_readonly(*recipient, false),
            AccountMeta::new(
                accounts::associated_token_address(recipient, reserve_mint),
                false,
            ),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

/// Builds the `record_goldcoin_completion` instruction. Same
/// immediately-preceded-by-the-ed25519-proof requirement as
/// [`release_from_reserve`].
pub fn record_goldcoin_completion(
    submitter: &Pubkey,
    index: u64,
    payout_txid: [u8; 32],
    payout_height: u64,
    attestation_epoch: u64,
) -> Instruction {
    let mut data = discriminator("record_goldcoin_completion").to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    data.extend_from_slice(&payout_txid);
    data.extend_from_slice(&payout_height.to_le_bytes());
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
        let ix = release_from_reserve(&submitter, &mint, &recipient, txid, 3, 500_000, 7);
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
        let ix = release_from_reserve(&submitter, &mint, &recipient, [0u8; 32], 0, 1, 0);
        assert_eq!(ix.accounts.len(), 13);
        assert_eq!(ix.accounts[0].pubkey, submitter);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accounts::bridge_config_pda());
        assert_eq!(ix.accounts[11].pubkey, spl_token::ID);
        assert_eq!(ix.accounts[12].pubkey, solana_sdk::system_program::ID);
    }

    #[test]
    fn record_goldcoin_completion_encodes_args_in_declared_order() {
        let submitter = Pubkey::new_unique();
        let payout_txid = [0xCDu8; 32];
        let ix = record_goldcoin_completion(&submitter, 9, payout_txid, 12345, 2);
        assert_eq!(&ix.data[0..8], discriminator("record_goldcoin_completion"));
        assert_eq!(&ix.data[8..16], &9u64.to_le_bytes());
        assert_eq!(&ix.data[16..48], &payout_txid);
        assert_eq!(&ix.data[48..56], &12345u64.to_le_bytes());
        assert_eq!(&ix.data[56..64], &2u64.to_le_bytes());
    }

    #[test]
    fn record_goldcoin_completion_has_five_accounts() {
        let submitter = Pubkey::new_unique();
        let ix = record_goldcoin_completion(&submitter, 0, [0u8; 32], 1, 0);
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
}
