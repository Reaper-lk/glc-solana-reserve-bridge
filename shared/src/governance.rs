//! Canonical attestation-signed governance messages.
//!
//! Reused near-verbatim from the old bridge's `shared::governance`
//! (docs/01-reuse-inventory.md classifies the propose/timelock/
//! permissionless-execute pattern as directly reusable): governance actions
//! are authorized by the same threshold mechanism as a reserve release (an
//! ed25519-precompile instruction immediately before the governance
//! instruction, carrying threshold signatures over the canonical bytes
//! built here). Used for the two governance domains that must never be a
//! single admin key's decision under the approved trust model
//! (docs/02-trust-model.md):
//!
//! 1. **Attestation-key rotation** — an admin able to rotate the
//!    attestation keys unilaterally could install attacker-controlled keys
//!    and defeat the whole threshold design.
//! 2. **The rebalance policy** (`RebalancePolicy`: the treasury-destination
//!    allowlist) — an admin able to edit the allowlist unilaterally could
//!    add their own token account and then take the ordinary,
//!    fully-audited treasury-withdrawal path, which would make the
//!    allowlist decorative. That is precisely the shape of the 2026-09-02
//!    incident, so the allowlist is governed by the same
//!    threshold-plus-timelock mechanism as the keys themselves.
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

/// Propose a `RebalancePolicy` update (the treasury allowlist).
/// Continues the SHARED action-byte numbering
/// documented in `crate::claim` — `0x05`/`0x06` belong to that module's
/// treasury/refund withdrawal claims and are never reused here.
pub const ACTION_PROPOSE_REBALANCE_POLICY: u8 = 0x07;

/// Cancel the currently pending `RebalancePolicy` update.
pub const ACTION_CANCEL_REBALANCE_POLICY: u8 = 0x08;

/// One-time initialization of the `RebalancePolicy` account. A distinct
/// action from [`ACTION_PROPOSE_REBALANCE_POLICY`] on purpose: an approval
/// to CREATE the first policy must never be replayable as an approval to
/// REPLACE an existing one, and vice versa. Initialization is not
/// timelocked (it only ever adds restrictions where none existed, and
/// `treasury_withdraw` fails closed until it has run), whereas every later
/// change is.
pub const ACTION_INITIALIZE_REBALANCE_POLICY: u8 = 0x09;

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

/// Canonical byte layout of a rebalance-policy proposal's parameters,
/// which the caller hashes to obtain the `params_commitment` for
/// [`governance_message`].
///
/// Layout: `treasury_count (1) || treasuries (32 each, in the order
/// proposed)`.
///
/// Treasury **order is significant**, exactly as it is for
/// [`rotation_params`]: it is the order the allowlist will be stored in,
/// so two proposals differing only in ordering are genuinely different
/// proposals and produce different commitments. That is deliberate — an
/// approver reviewing a proposal reviews a specific, ordered list, not a
/// set that could be re-encoded into different on-chain bytes after the
/// fact.
///
/// The allowlist is the whole policy, so it is the whole commitment.
/// There is no amount ceiling, rate limit or rolling budget to commit to
/// alongside it: approving "these destinations" approves the entire
/// control, with nothing left ungoverned.
pub fn rebalance_policy_params(treasuries: &[[u8; 32]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + treasuries.len() * 32);
    out.push(treasuries.len() as u8);
    for t in treasuries {
        out.extend_from_slice(t);
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

    /// Every action byte allocated in EITHER module names exactly one
    /// action. `0x03` is the one grandfathered exception (see
    /// `crate::claim`'s action-byte numbering table): it names both the
    /// governance rotation proposal and the retired rebalance-withdraw
    /// claim, which carry different domain tags and different lengths.
    #[test]
    fn action_bytes_are_unique_across_both_modules_except_the_grandfathered_one() {
        let all: [(&str, u8); 8] = [
            ("claim::release", crate::claim::ACTION_RELEASE_FROM_RESERVE),
            (
                "claim::completion",
                crate::claim::ACTION_RECORD_GOLDCOIN_COMPLETION,
            ),
            (
                "claim::rebalance(retired)",
                crate::claim::ACTION_REBALANCE_WITHDRAW,
            ),
            ("claim::treasury", crate::claim::ACTION_TREASURY_WITHDRAW),
            ("claim::refund", crate::claim::ACTION_REFUND_WITHDRAW),
            ("gov::propose_rotation", ACTION_PROPOSE_ROTATION),
            ("gov::cancel_rotation", ACTION_CANCEL_ROTATION),
            ("gov::propose_policy", ACTION_PROPOSE_REBALANCE_POLICY),
        ];
        for (i, (name_a, a)) in all.iter().enumerate() {
            assert_ne!(*a, 0x00, "{name_a}: 0x00 is never a valid action");
            for (name_b, b) in all.iter().skip(i + 1) {
                let grandfathered =
                    *a == crate::claim::ACTION_REBALANCE_WITHDRAW && *b == ACTION_PROPOSE_ROTATION;
                assert!(
                    a != b || grandfathered,
                    "action byte {a:#04x} is claimed by both {name_a} and {name_b}"
                );
            }
        }
        // The two newest governance actions are unique against everything.
        for (name, other) in all.iter() {
            assert_ne!(
                ACTION_CANCEL_REBALANCE_POLICY, *other,
                "cancel-policy collides with {name}"
            );
            assert_ne!(
                ACTION_INITIALIZE_REBALANCE_POLICY, *other,
                "initialize-policy collides with {name}"
            );
        }
        assert_ne!(
            ACTION_CANCEL_REBALANCE_POLICY,
            ACTION_INITIALIZE_REBALANCE_POLICY
        );
    }

    #[test]
    fn rebalance_policy_params_layout_is_pinned() {
        let params = rebalance_policy_params(&[[0xAA; 32], [0xBB; 32]]);
        let mut expected = Vec::new();
        expected.push(2u8);
        expected.extend_from_slice(&[0xAA; 32]);
        expected.extend_from_slice(&[0xBB; 32]);
        assert_eq!(params, expected);
        assert_eq!(params.len(), 1 + 64);
    }

    /// Ordering and membership are each part of what gets approved — no
    /// two materially different allowlists may hash alike.
    #[test]
    fn rebalance_policy_params_distinguish_order_and_membership() {
        let base = rebalance_policy_params(&[[0xAA; 32], [0xBB; 32]]);
        assert_ne!(
            base,
            rebalance_policy_params(&[[0xBB; 32], [0xAA; 32]]),
            "ordering must be significant"
        );
        assert_ne!(
            base,
            rebalance_policy_params(&[[0xAA; 32], [0xCC; 32]]),
            "membership must be significant"
        );
        assert_ne!(
            base,
            rebalance_policy_params(&[[0xAA; 32]]),
            "count must be significant"
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
