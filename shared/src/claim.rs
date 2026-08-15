//! Canonical attestation-signed messages for the reserve bridge.
//!
//! This is THE byte layout the internal threshold attestation signers sign
//! and the on-chain program verifies (docs/02-trust-model.md, docs/12
//! item 1 — approved: 2-of-3 internal threshold custody, NOT federation).
//! Single source of truth for both the program and the off-chain service,
//! which is why it lives in this shared crate. Any change to a layout here
//! invalidates every outstanding signature and is a protocol-version event.
//!
//! Adapted from the old bridge's `shared::claim` module (see
//! docs/01-reuse-inventory.md): identical domain-separation discipline and
//! byte-layout approach, applied to reserve-release/reserve-deposit actions
//! instead of mint/burn, and to a small fixed attestation-key set instead of
//! a rotating federation validator set. The field previously named "epoch"
//! (bound to federation validator-set revisions) is kept under the name
//! `attestation_epoch` here: it still exists to invalidate in-flight
//! signatures across an attestation-key rotation, but nothing about it
//! implies a federation.
//!
//! ## Message families
//!
//! - [`release_claim_message`] — authorizes `release_from_reserve`: exactly
//!   one Solana-side reserve release, for one confirmed Goldcoin deposit
//!   `(txid, vout)`, of one amount, to one recipient (constraint 1: 1
//!   Solana GLC released requires 1 corresponding GLC locked/received on
//!   the source side — this message is the mechanism that binds the two).
//! - [`goldcoin_completion_message`] — authorizes `record_goldcoin_completion`:
//!   records, on Solana, that a specific `WithdrawalObligation` was paid out
//!   on Goldcoin. Terminal and irreversible once verified on-chain.
//!
//! Both share a 57-byte prefix (domain tag, protocol version, program id,
//! attestation epoch) and diverge only at the action byte onward — the
//! action byte is the sole separator between the two families, so a
//! signature valid for one can never verify as the other.

/// 16-byte ASCII domain tag; never reused by any other message family in
/// this program. Deliberately distinct from the old (federated, mint/burn)
/// bridge's `GLC_BRIDGE_CLAIM` tag, so no signature from either system could
/// ever be mistaken for the other even if key material were somehow shared.
pub const CLAIM_DOMAIN_TAG: &[u8; 16] = b"GLC_RSV_CLAIM_V1";

/// Action discriminator for a Solana-side reserve release, authorized by a
/// confirmed Goldcoin deposit. `0x00` is deliberately never valid.
pub const ACTION_RELEASE_FROM_RESERVE: u8 = 0x01;

/// Action discriminator for recording a completed Goldcoin-side payout
/// against a `WithdrawalObligation`.
pub const ACTION_RECORD_GOLDCOIN_COMPLETION: u8 = 0x02;

/// Exact length of a release-claim message.
pub const RELEASE_CLAIM_MESSAGE_LEN: usize = 166;

/// Exact length of a Goldcoin-completion message.
pub const COMPLETION_MESSAGE_LEN: usize = 146;

/// Builds the canonical release-claim message.
///
/// Layout (166 bytes, all integers little-endian, txid verbatim):
///
/// | offset | len | field                                        |
/// |--------|-----|-----------------------------------------------|
/// | 0      | 16  | domain tag `b"GLC_RSV_CLAIM_V1"`              |
/// | 16     | 1   | protocol version (`u8`)                       |
/// | 17     | 32  | Solana program id                             |
/// | 49     | 8   | attestation-key epoch (`u64` LE)              |
/// | 57     | 1   | action type (`ACTION_RELEASE_FROM_RESERVE`)   |
/// | 58     | 32  | Goldcoin txid (`[u8; 32]` verbatim)           |
/// | 90     | 4   | vout (`u32` LE)                               |
/// | 94     | 8   | amount, atomic GLC units (`u64` LE)           |
/// | 102    | 32  | Solana recipient pubkey                       |
/// | 134    | 32  | reserve GLC SPL mint pubkey                   |
///
/// Domain separation: tag, program id, protocol version, attestation epoch,
/// and action type together guarantee a signature authorizes exactly one
/// release, for one deposit, to one recipient, of one amount, on one
/// deployment, under one attestation-key revision (constraint 5: replay
/// protection across restarts and concurrent operators — signature-level,
/// backed on-chain by the `DepositClaim` PDA replay guard).
#[allow(clippy::too_many_arguments)]
pub fn release_claim_message(
    protocol_version: u8,
    program_id: &[u8; 32],
    attestation_epoch: u64,
    txid: &[u8; 32],
    vout: u32,
    amount: u64,
    recipient: &[u8; 32],
    reserve_token_mint: &[u8; 32],
) -> [u8; RELEASE_CLAIM_MESSAGE_LEN] {
    let mut m = [0u8; RELEASE_CLAIM_MESSAGE_LEN];
    m[0..16].copy_from_slice(CLAIM_DOMAIN_TAG);
    m[16] = protocol_version;
    m[17..49].copy_from_slice(program_id);
    m[49..57].copy_from_slice(&attestation_epoch.to_le_bytes());
    m[57] = ACTION_RELEASE_FROM_RESERVE;
    m[58..90].copy_from_slice(txid);
    m[90..94].copy_from_slice(&vout.to_le_bytes());
    m[94..102].copy_from_slice(&amount.to_le_bytes());
    m[102..134].copy_from_slice(recipient);
    m[134..166].copy_from_slice(reserve_token_mint);
    m
}

/// Builds the canonical Goldcoin-completion message.
///
/// Layout (146 bytes, all integers little-endian, txid verbatim):
///
/// | offset | len | field                                     |
/// |--------|-----|--------------------------------------------|
/// | 0      | 16  | domain tag `b"GLC_RSV_CLAIM_V1"`           |
/// | 16     | 1   | protocol version (`u8`)                    |
/// | 17     | 32  | Solana program id                          |
/// | 49     | 8   | attestation-key epoch (`u64` LE)           |
/// | 57     | 1   | action (`ACTION_RECORD_GOLDCOIN_COMPLETION`)|
/// | 58     | 8   | withdrawal-obligation index (`u64` LE)     |
/// | 66     | 32  | Goldcoin payout txid (`[u8; 32]`)          |
/// | 98     | 8   | payout block height (`u64` LE)             |
/// | 106    | 8   | amount, atomic GLC units (`u64` LE)        |
/// | 114    | 32  | destination commitment (see below)          |
///
/// `dest_commitment` is `sha256` over the obligation's Goldcoin address
/// exactly as stored on-chain (`glc_address[..glc_address_len]`, opaque
/// ASCII bytes) — deliberately not a decoded/hashed pubkey-hash, so the
/// program never needs a base58 decoder on-chain (same reasoning as the old
/// bridge's ADR-0018 D6, reused here).
///
/// Binding `amount` and the destination means a signature authorizes
/// completion of one specific payout, to one specific destination, for one
/// specific amount — a claim each attestation signer can independently
/// verify against its own Goldcoin chain read before signing (constraint 3:
/// never release/record based only on a requester's claim; constraint 4:
/// verify source-chain state independently).
#[allow(clippy::too_many_arguments)]
pub fn goldcoin_completion_message(
    protocol_version: u8,
    program_id: &[u8; 32],
    attestation_epoch: u64,
    obligation_index: u64,
    payout_txid: &[u8; 32],
    payout_height: u64,
    amount: u64,
    dest_commitment: &[u8; 32],
) -> [u8; COMPLETION_MESSAGE_LEN] {
    let mut m = [0u8; COMPLETION_MESSAGE_LEN];
    m[0..16].copy_from_slice(CLAIM_DOMAIN_TAG);
    m[16] = protocol_version;
    m[17..49].copy_from_slice(program_id);
    m[49..57].copy_from_slice(&attestation_epoch.to_le_bytes());
    m[57] = ACTION_RECORD_GOLDCOIN_COMPLETION;
    m[58..66].copy_from_slice(&obligation_index.to_le_bytes());
    m[66..98].copy_from_slice(payout_txid);
    m[98..106].copy_from_slice(&payout_height.to_le_bytes());
    m[106..114].copy_from_slice(&amount.to_le_bytes());
    m[114..146].copy_from_slice(dest_commitment);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_sample() -> [u8; RELEASE_CLAIM_MESSAGE_LEN] {
        release_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            &[0x22; 32],
            0xAABBCCDD,
            0x1122334455667788,
            &[0x33; 32],
            &[0x44; 32],
        )
    }

    /// Golden vector: pins every byte. A change here is a signature-breaking
    /// protocol change and must be deliberate.
    #[test]
    fn release_golden_vector() {
        let m = release_sample();
        let mut expected = Vec::with_capacity(RELEASE_CLAIM_MESSAGE_LEN);
        expected.extend_from_slice(b"GLC_RSV_CLAIM_V1");
        expected.push(1);
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        expected.push(ACTION_RELEASE_FROM_RESERVE);
        expected.extend_from_slice(&[0x22; 32]);
        expected.extend_from_slice(&[0xDD, 0xCC, 0xBB, 0xAA]);
        expected.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        expected.extend_from_slice(&[0x33; 32]);
        expected.extend_from_slice(&[0x44; 32]);
        assert_eq!(expected.len(), RELEASE_CLAIM_MESSAGE_LEN);
        assert_eq!(m.as_slice(), expected.as_slice());
    }

    #[test]
    fn release_domain_tag_is_sixteen_bytes_and_stable() {
        assert_eq!(CLAIM_DOMAIN_TAG.len(), 16);
        assert_eq!(&release_sample()[0..16], b"GLC_RSV_CLAIM_V1");
    }

    #[test]
    fn release_differing_epoch_changes_exactly_its_own_bytes() {
        let a = release_sample();
        let b = release_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060709, // only the epoch differs
            &[0x22; 32],
            0xAABBCCDD,
            0x1122334455667788,
            &[0x33; 32],
            &[0x44; 32],
        );
        assert_eq!(a[..49], b[..49]);
        assert_ne!(a[49..57], b[49..57]);
        assert_eq!(a[57..], b[57..]);
    }

    fn completion_sample() -> [u8; COMPLETION_MESSAGE_LEN] {
        goldcoin_completion_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x1122334455667788,
            &[0x55; 32],
            0x00000000DEADBEEF,
            0x0A0B0C0D0E0F1011,
            &[0x66; 32],
        )
    }

    #[test]
    fn completion_golden_vector() {
        let m = completion_sample();
        let mut expected = Vec::with_capacity(COMPLETION_MESSAGE_LEN);
        expected.extend_from_slice(b"GLC_RSV_CLAIM_V1");
        expected.push(1);
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        expected.push(ACTION_RECORD_GOLDCOIN_COMPLETION);
        expected.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        expected.extend_from_slice(&[0x55; 32]);
        expected.extend_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE, 0x00, 0x00, 0x00, 0x00]);
        expected.extend_from_slice(&[0x11, 0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A]);
        expected.extend_from_slice(&[0x66; 32]);
        assert_eq!(expected.len(), COMPLETION_MESSAGE_LEN);
        assert_eq!(m.as_slice(), expected.as_slice());
    }

    #[test]
    fn action_byte_is_the_sole_separator_between_families() {
        let release = release_sample();
        let completion = completion_sample();
        assert_eq!(release[57], ACTION_RELEASE_FROM_RESERVE);
        assert_eq!(completion[57], ACTION_RECORD_GOLDCOIN_COMPLETION);
        assert_ne!(
            ACTION_RELEASE_FROM_RESERVE,
            ACTION_RECORD_GOLDCOIN_COMPLETION
        );
        assert_ne!(release[57], 0x00, "0x00 is never a valid action");
    }

    #[test]
    fn families_share_first_57_bytes_of_layout() {
        let release = release_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            &[0x22; 32],
            0,
            0,
            &[0x33; 32],
            &[0x44; 32],
        );
        let completion = completion_sample();
        assert_eq!(release[..57], completion[..57]);
    }

    #[test]
    fn completion_message_is_never_a_prefix_or_suffix_confusable_with_release() {
        let r = release_sample();
        let c = completion_sample();
        assert_ne!(COMPLETION_MESSAGE_LEN, RELEASE_CLAIM_MESSAGE_LEN);
        assert_ne!(r[..COMPLETION_MESSAGE_LEN], c[..]);
    }

    #[test]
    fn distinct_from_old_federated_bridge_domain_tag() {
        // Defensive: this bridge's signatures must never be confusable with
        // the old (federated, mint/burn) bridge's, even in principle.
        assert_ne!(CLAIM_DOMAIN_TAG, b"GLC_BRIDGE_CLAIM");
    }
}
