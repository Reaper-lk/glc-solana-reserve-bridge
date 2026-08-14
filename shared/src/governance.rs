//! Canonical attestation-signed governance messages.
//!
//! Reused near-verbatim from the old bridge's `shared::governance`
//! (docs/01-reuse-inventory.md classifies the propose/timelock/
//! permissionless-execute pattern as directly reusable): governance actions
//! are authorized by the same threshold mechanism as a reserve release (an
//! ed25519-precompile instruction immediately before the governance
//! instruction, carrying threshold signatures over the canonical bytes
//! built here). Currently used for attestation-key rotation only — the
//! only governance action that must never be a single admin key's decision
//! under the approved trust model (docs/02-trust-model.md): an admin able
//! to rotate the attestation keys unilaterally could install attacker-
//! controlled keys and defeat the whole threshold design.
//!
//! # Why the parameters are hashed rather than inlined
//!
//! Even though `MAX_ATTESTATION_KEYS` is small (8, vs. the old federation's
//! 16), the message commits to `sha256(canonical parameter bytes)` rather
//! than inlining the proposed key list, for the same reason the old bridge
//! did: it keeps the signed message a fixed length regardless of key count,
//! and the program recomputes the hash from the instruction's own arguments
//! and compares — the binding is identical in strength, only the encoding
//! is compact and uniform across possible future governance actions.

/// 16-byte domain tag, distinct from `claim::CLAIM_DOMAIN_TAG` (and from the
/// old bridge's own governance tag) — a governance signature can never be
/// reinterpreted as a claim, or vice versa, even if every other field
/// coincided.
pub const GOVERNANCE_DOMAIN_TAG: &[u8; 16] = b"GLC_RSV_GOVRN_V1";

/// Propose an attestation-key-set/threshold rotation. Continues the
/// action-type numbering started by `claim::ACTION_RELEASE_FROM_RESERVE`
/// (0x01) and `claim::ACTION_RECORD_GOLDCOIN_COMPLETION` (0x02); 0x00
/// remains permanently invalid.
pub const ACTION_PROPOSE_ROTATION: u8 = 0x03;

/// Cancel the currently pending governance action.
pub const ACTION_CANCEL_ROTATION: u8 = 0x04;

/// Exact length of a governance message: 16 + 1 + 32 + 8 + 1 + 32.
pub const GOVERNANCE_MESSAGE_LEN: usize = 90;

/// Builds the canonical governance message.
///
/// Layout (all integers little-endian):
///
/// | offset | len | field |
/// |--------|-----|-------|
/// | 0      | 16  | domain tag `b"GLC_RSV_GOVRN_V1"` |
/// | 16     | 1   | protocol version |
/// | 17     | 32  | Solana program id |
/// | 49     | 8   | current attestation-key epoch (`u64` LE) |
/// | 57     | 1   | action type |
/// | 58     | 32  | parameter commitment (SHA-256) |
///
/// Binding the CURRENT epoch means a governance signature dies the moment
/// the attestation-key set rotates. Binding the program id prevents a
/// signature collected for one deployment being replayed against another.
///
/// Pure and allocation-free, so it is byte-identical under SBF and on the
/// host (same discipline as `claim::release_claim_message`).
pub fn governance_message(
    protocol_version: u8,
    program_id: &[u8; 32],
    epoch: u64,
    action: u8,
    params_commitment: &[u8; 32],
) -> [u8; GOVERNANCE_MESSAGE_LEN] {
    let mut m = [0u8; GOVERNANCE_MESSAGE_LEN];
    m[0..16].copy_from_slice(GOVERNANCE_DOMAIN_TAG);
    m[16] = protocol_version;
    m[17..49].copy_from_slice(program_id);
    m[49..57].copy_from_slice(&epoch.to_le_bytes());
    m[57] = action;
    m[58..90].copy_from_slice(params_commitment);
    m
}

/// Canonical byte layout of a rotation's parameters, which the caller
/// hashes to obtain the `params_commitment` for [`governance_message`].
///
/// Layout: `threshold (1) || key_count (4 LE) || keys (32 each, in the
/// order proposed)`.
///
/// Key **order is significant**: it is the order the set will be stored
/// in, and it determines each key's bitmask index in the on-chain
/// duplicate check. Two proposals differing only in ordering are genuinely
/// different proposals and produce different commitments.
pub fn rotation_params(threshold: u8, keys: &[[u8; 32]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + keys.len() * 32);
    out.push(threshold);
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        out.extend_from_slice(k);
    }
    out
}

/// Canonical parameter layout for a cancellation. A cancel signature must
/// not be replayable against a *different* pending action, so it commits to
/// the exact action it cancels: the pending action's type and its
/// execution time.
pub fn cancel_params(pending_action: u8, pending_eta: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8);
    out.push(pending_action);
    out.extend_from_slice(&pending_eta.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_message_golden_vector() {
        let m = governance_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            ACTION_PROPOSE_ROTATION,
            &[0x22; 32],
        );
        let mut expected = Vec::with_capacity(GOVERNANCE_MESSAGE_LEN);
        expected.extend_from_slice(b"GLC_RSV_GOVRN_V1");
        expected.push(1);
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        expected.push(0x03);
        expected.extend_from_slice(&[0x22; 32]);
        assert_eq!(expected.len(), GOVERNANCE_MESSAGE_LEN);
        assert_eq!(m.as_slice(), expected.as_slice());
    }

    #[test]
    fn domain_tag_is_distinct_from_the_claim_tag() {
        assert_ne!(
            GOVERNANCE_DOMAIN_TAG.as_slice(),
            crate::claim::CLAIM_DOMAIN_TAG.as_slice(),
            "a governance signature must never be reinterpretable as a claim"
        );
        assert_eq!(GOVERNANCE_DOMAIN_TAG.len(), 16);
    }

    #[test]
    fn action_types_do_not_collide_with_claim_actions() {
        assert_ne!(
            ACTION_PROPOSE_ROTATION,
            crate::claim::ACTION_RELEASE_FROM_RESERVE
        );
        assert_ne!(
            ACTION_PROPOSE_ROTATION,
            crate::claim::ACTION_RECORD_GOLDCOIN_COMPLETION
        );
        assert_ne!(ACTION_PROPOSE_ROTATION, ACTION_CANCEL_ROTATION);
        assert_ne!(
            ACTION_PROPOSE_ROTATION, 0x00,
            "0x00 is never a valid action"
        );
    }

    #[test]
    fn every_field_changes_the_message() {
        let base = governance_message(1, &[0x11; 32], 7, ACTION_PROPOSE_ROTATION, &[0x22; 32]);
        assert_ne!(
            base,
            governance_message(2, &[0x11; 32], 7, ACTION_PROPOSE_ROTATION, &[0x22; 32])
        );
        assert_ne!(
            base,
            governance_message(1, &[0x99; 32], 7, ACTION_PROPOSE_ROTATION, &[0x22; 32])
        );
        assert_ne!(
            base,
            governance_message(1, &[0x11; 32], 8, ACTION_PROPOSE_ROTATION, &[0x22; 32])
        );
        assert_ne!(
            base,
            governance_message(1, &[0x11; 32], 7, ACTION_CANCEL_ROTATION, &[0x22; 32])
        );
        assert_ne!(
            base,
            governance_message(1, &[0x11; 32], 7, ACTION_PROPOSE_ROTATION, &[0x33; 32])
        );
    }

    #[test]
    fn rotation_params_layout_is_pinned() {
        let v = [[0xAAu8; 32], [0xBBu8; 32]];
        let p = rotation_params(3, &v);
        assert_eq!(p.len(), 1 + 4 + 64);
        assert_eq!(p[0], 3);
        assert_eq!(&p[1..5], &2u32.to_le_bytes());
        assert_eq!(&p[5..37], &[0xAA; 32]);
        assert_eq!(&p[37..69], &[0xBB; 32]);
    }

    #[test]
    fn rotation_params_distinguish_order_threshold_and_membership() {
        let a = [[0xAAu8; 32], [0xBBu8; 32]];
        let reordered = [[0xBBu8; 32], [0xAAu8; 32]];
        let different = [[0xAAu8; 32], [0xCCu8; 32]];
        assert_ne!(rotation_params(2, &a), rotation_params(2, &reordered));
        assert_ne!(rotation_params(2, &a), rotation_params(1, &a));
        assert_ne!(rotation_params(2, &a), rotation_params(2, &different));
        assert_ne!(rotation_params(2, &a), rotation_params(2, &a[..1]));
    }

    #[test]
    fn cancel_params_bind_the_specific_pending_action() {
        assert_eq!(cancel_params(ACTION_PROPOSE_ROTATION, 1_000).len(), 9);
        assert_ne!(
            cancel_params(ACTION_PROPOSE_ROTATION, 1_000),
            cancel_params(ACTION_PROPOSE_ROTATION, 1_001),
            "a cancel signature must not be replayable against a re-proposal"
        );
    }

    #[test]
    fn message_length_matches_the_constant() {
        assert_eq!(
            governance_message(1, &[0; 32], 0, ACTION_PROPOSE_ROTATION, &[0; 32]).len(),
            GOVERNANCE_MESSAGE_LEN
        );
    }
}
