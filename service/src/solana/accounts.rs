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

use super::rpc::SolanaRpcError;

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

/// Decoded subset of `BridgeConfig` (state.rs layout, after the 8-byte
/// Anchor discriminator) — only the fields the indexer/reconciliation
/// actually consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfigSnapshot {
    pub paused: bool,
    pub release_paused: bool,
    pub deposit_paused: bool,
    pub reserve_token_mint: Pubkey,
    pub obligation_count: u64,
    pub protected_minimum: u64,
    pub per_transfer_limit: u64,
}

pub fn decode_bridge_config(data: &[u8]) -> Result<BridgeConfigSnapshot, SolanaRpcError> {
    let body = data
        .get(DISCRIMINATOR_LEN..)
        .ok_or_else(|| SolanaRpcError::Malformed("account shorter than discriminator".into()))?;
    // protocol_version(1) admin(32) pending_admin(1+32) paused(1) release_paused(1) deposit_paused(1) bump(1)
    let mut off = 1 + 32 + 33;
    let paused = read_bool(body, off)?;
    off += 1;
    let release_paused = read_bool(body, off)?;
    off += 1;
    let deposit_paused = read_bool(body, off)?;
    off += 1;
    off += 1; // bump
    let reserve_token_mint = read_pubkey(body, off)?;
    off += 32;
    off += 1; // reserve_authority_bump
    let obligation_count = read_u64(body, off)?;
    off += 8;
    off += 8; // governance_timelock_seconds
    off += 8; // min_transfer_amount
    let per_transfer_limit = read_u64(body, off)?;
    off += 8;
    let protected_minimum = read_u64(body, off)?;
    Ok(BridgeConfigSnapshot {
        paused,
        release_paused,
        deposit_paused,
        reserve_token_mint,
        obligation_count,
        protected_minimum,
        per_transfer_limit,
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
        v.push(0); // pending_admin tag (None)
        v.extend_from_slice(&[0u8; 32]); // pending_admin payload (still allocated)
        v.push(paused as u8);
        v.push(release_paused as u8);
        v.push(deposit_paused as u8);
        v.push(7); // bump
        v.extend_from_slice(&[9u8; 32]); // reserve_token_mint
        v.push(3); // reserve_authority_bump
        v.extend_from_slice(&obligation_count.to_le_bytes());
        v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
        v.extend_from_slice(&100u64.to_le_bytes()); // min_transfer_amount
        v.extend_from_slice(&1_000_000u64.to_le_bytes()); // per_transfer_limit
        v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
        v.extend_from_slice(&2_000_000u64.to_le_bytes()); // rolling_volume_limit
        v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
        v.extend_from_slice(&[0u8; 32]); // reserved
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
}
