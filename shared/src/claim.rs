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
//! - [`rebalance_withdraw_claim_message`] — **RETIRED**. Authorized the
//!   removed `rebalance_withdraw` instruction, which permitted an
//!   arbitrary destination token account. Kept only so the retired
//!   instruction's negative tests can still construct the exact bytes it
//!   used to accept; the on-chain instruction now fails closed before
//!   verifying anything, so a signature over these bytes authorizes
//!   nothing anywhere.
//! - [`treasury_withdraw_claim_message`] — authorizes `treasury_withdraw`:
//!   one operator-initiated reserve withdrawal to one ALLOWLISTED treasury
//!   token account, under one `RebalancePolicy` revision.
//! - [`refund_withdraw_claim_message`] — authorizes `refund_withdraw`: the
//!   return of one specific `WithdrawalObligation`'s deposit to the
//!   original depositor's canonical associated token account.
//!
//! Every family shares a 57-byte prefix (domain tag, protocol version,
//! program id, attestation epoch) and diverges only at the action byte
//! onward — the action byte is the sole separator between families, so a
//! signature valid for one can never verify as another. As defence in
//! depth every family additionally has its own unique total length, so a
//! cross-family confusion would have to survive two independent checks.
//!
//! ## Action-byte numbering
//!
//! Action discriminators are allocated from ONE space shared with
//! [`crate::governance`], even though the two use different domain tags:
//! a value is never reused across the two modules, so a single number
//! always names a single action. `0x00` is permanently invalid.
//!
//! | byte | module     | action                                  |
//! |------|------------|-----------------------------------------|
//! | 0x01 | claim      | release from reserve                    |
//! | 0x02 | claim      | record Goldcoin completion              |
//! | 0x03 | claim      | rebalance withdraw (RETIRED)            |
//! | 0x03 | governance | propose attestation-key rotation (\*)    |
//! | 0x04 | governance | cancel attestation-key rotation         |
//! | 0x05 | claim      | treasury withdraw                       |
//! | 0x06 | claim      | refund withdraw                         |
//! | 0x07 | governance | propose rebalance-policy update         |
//! | 0x08 | governance | cancel rebalance-policy update          |
//! | 0x09 | governance | initialize rebalance policy             |
//!
//! (\*) `0x03` is the one historical collision, predating this rule: it
//! names both the retired claim action and the governance rotation
//! proposal. The two are unambiguous in practice because they carry
//! different domain tags and different lengths, and the claim-side value
//! is now retired. Every value allocated since is unique across both.

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

/// Action discriminator for an intentional, operator-initiated reserve
/// rebalance withdrawal — structurally distinct from a bridge settlement
/// (`ACTION_RELEASE_FROM_RESERVE`): no Goldcoin deposit is being settled,
/// there is no recipient bound by prior chain observation, and this action
/// additionally requires the bridge to already be paused (enforced
/// on-chain in `instructions::rebalance_withdraw`, not by this message
/// alone).
pub const ACTION_REBALANCE_WITHDRAW: u8 = 0x03;

/// Action discriminator for an operator-initiated reserve withdrawal to an
/// ALLOWLISTED treasury token account — the replacement for the retired
/// [`ACTION_REBALANCE_WITHDRAW`]. The difference that matters is not the
/// number: it is that the on-chain instruction behind this action refuses
/// any destination not named in the on-chain `RebalancePolicy`, enforces a
/// dedicated per-withdrawal and rolling limit, and binds the policy
/// revision into the signed bytes.
pub const ACTION_TREASURY_WITHDRAW: u8 = 0x05;

/// Action discriminator for returning one specific `WithdrawalObligation`'s
/// deposit to its original depositor. Structurally distinct from
/// [`ACTION_TREASURY_WITHDRAW`]: the destination is not allowlisted, it is
/// DERIVED — the canonical associated token account of the obligation's own
/// recorded `requester` — so this action can never send funds anywhere the
/// original depositor does not already control.
pub const ACTION_REFUND_WITHDRAW: u8 = 0x06;

/// Exact length of a release-claim message.
pub const RELEASE_CLAIM_MESSAGE_LEN: usize = 166;

/// Exact length of a Goldcoin-completion message.
pub const COMPLETION_MESSAGE_LEN: usize = 146;

/// Exact length of a (retired) rebalance-withdrawal-claim message.
pub const REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN: usize = 138;

/// Exact length of a treasury-withdrawal-claim message.
pub const TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN: usize = 178;

/// Exact length of a refund-withdrawal-claim message.
pub const REFUND_WITHDRAW_CLAIM_MESSAGE_LEN: usize = 210;

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

/// Builds the canonical rebalance-withdrawal-claim message.
///
/// **RETIRED — authorizes nothing.** The `rebalance_withdraw` instruction
/// this message family authorized accepted an ARBITRARY destination token
/// account, subject only to the reserve mint and token program matching.
/// It has been replaced by [`treasury_withdraw_claim_message`] (allowlisted
/// treasury destination, dedicated per-withdrawal and rolling limits,
/// policy-revision binding) and [`refund_withdraw_claim_message`]
/// (destination derived from the depositor's own obligation). The on-chain
/// instruction now fails closed with `RebalanceWithdrawRetired` before it
/// verifies any signature, so bytes built here cannot move funds. This
/// builder is retained ONLY so tests can construct the exact message the
/// removed path used to accept and prove it is no longer accepted.
///
/// Layout (138 bytes, all integers little-endian):
///
/// | offset | len | field                                          |
/// |--------|-----|-------------------------------------------------|
/// | 0      | 16  | domain tag `b"GLC_RSV_CLAIM_V1"`                |
/// | 16     | 1   | protocol version (`u8`)                         |
/// | 17     | 32  | Solana program id                               |
/// | 49     | 8   | attestation-key epoch (`u64` LE)                |
/// | 57     | 1   | action type (`ACTION_REBALANCE_WITHDRAW`)       |
/// | 58     | 8   | nonce (`u64` LE) — replay guard                 |
/// | 66     | 8   | amount, atomic reserve-mint units (`u64` LE)    |
/// | 74     | 32  | destination token account pubkey               |
/// | 106    | 32  | reserve GLC SPL mint pubkey                     |
///
/// Binding `nonce`, `amount`, `destination`, and the reserve mint means a
/// signature authorizes exactly one withdrawal, of one amount, to one
/// destination, on one deployment, under one attestation-key revision —
/// each attestation signer can independently verify the operator's stated
/// intent (destination, amount) before signing, the same discipline as
/// [`release_claim_message`], applied to an operator-initiated rebalance
/// instead of a bridge settlement.
pub fn rebalance_withdraw_claim_message(
    protocol_version: u8,
    program_id: &[u8; 32],
    attestation_epoch: u64,
    nonce: u64,
    amount: u64,
    destination: &[u8; 32],
    reserve_token_mint: &[u8; 32],
) -> [u8; REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN] {
    let mut m = [0u8; REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN];
    m[0..16].copy_from_slice(CLAIM_DOMAIN_TAG);
    m[16] = protocol_version;
    m[17..49].copy_from_slice(program_id);
    m[49..57].copy_from_slice(&attestation_epoch.to_le_bytes());
    m[57] = ACTION_REBALANCE_WITHDRAW;
    m[58..66].copy_from_slice(&nonce.to_le_bytes());
    m[66..74].copy_from_slice(&amount.to_le_bytes());
    m[74..106].copy_from_slice(destination);
    m[106..138].copy_from_slice(reserve_token_mint);
    m
}

/// Builds the canonical treasury-withdrawal-claim message.
///
/// Layout (178 bytes, all integers little-endian):
///
/// | offset | len | field                                          |
/// |--------|-----|-------------------------------------------------|
/// | 0      | 16  | domain tag `b"GLC_RSV_CLAIM_V1"`                |
/// | 16     | 1   | protocol version (`u8`)                         |
/// | 17     | 32  | Solana program id                               |
/// | 49     | 8   | attestation-key epoch (`u64` LE)                |
/// | 57     | 1   | action type (`ACTION_TREASURY_WITHDRAW`)        |
/// | 58     | 8   | nonce (`u64` LE) — replay guard                 |
/// | 66     | 8   | amount, atomic reserve-mint units (`u64` LE)    |
/// | 74     | 32  | destination treasury token account              |
/// | 106    | 32  | reserve GLC SPL mint pubkey                     |
/// | 138    | 32  | reserve token account the funds leave (source)  |
/// | 170    | 8   | `RebalancePolicy.version` (`u64` LE)            |
///
/// Two bindings this family adds over the retired
/// [`rebalance_withdraw_claim_message`], both load-bearing:
///
/// - **`policy_version`** — a signature collected while the on-chain
///   allowlist/limits were at revision *n* is worthless the instant
///   governance moves them to *n+1*. Without it, an approval gathered
///   under a permissive policy could be held and replayed after the
///   policy tightened, or (worse) a signature gathered before a treasury
///   address was removed would still name that address.
/// - **`reserve_token_account`** — the source. It is derivable from the
///   reserve-authority PDA, the mint and the token program, but including
///   it verbatim means an attestation signer can validate the entire
///   movement (from, to, how much, under which policy) by parsing the
///   bytes it was asked to sign, with no PDA derivation of its own.
///
/// Everything else is unchanged in meaning from the retired family:
/// binding nonce, amount, destination and mint means a signature
/// authorizes exactly one withdrawal, of one amount, to one destination,
/// on one deployment, under one attestation-key revision.
#[allow(clippy::too_many_arguments)]
pub fn treasury_withdraw_claim_message(
    protocol_version: u8,
    program_id: &[u8; 32],
    attestation_epoch: u64,
    nonce: u64,
    amount: u64,
    destination: &[u8; 32],
    reserve_token_mint: &[u8; 32],
    reserve_token_account: &[u8; 32],
    policy_version: u64,
) -> [u8; TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN] {
    let mut m = [0u8; TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN];
    m[0..16].copy_from_slice(CLAIM_DOMAIN_TAG);
    m[16] = protocol_version;
    m[17..49].copy_from_slice(program_id);
    m[49..57].copy_from_slice(&attestation_epoch.to_le_bytes());
    m[57] = ACTION_TREASURY_WITHDRAW;
    m[58..66].copy_from_slice(&nonce.to_le_bytes());
    m[66..74].copy_from_slice(&amount.to_le_bytes());
    m[74..106].copy_from_slice(destination);
    m[106..138].copy_from_slice(reserve_token_mint);
    m[138..170].copy_from_slice(reserve_token_account);
    m[170..178].copy_from_slice(&policy_version.to_le_bytes());
    m
}

/// Builds the canonical refund-withdrawal-claim message.
///
/// Layout (210 bytes, all integers little-endian):
///
/// | offset | len | field                                          |
/// |--------|-----|-------------------------------------------------|
/// | 0      | 16  | domain tag `b"GLC_RSV_CLAIM_V1"`                |
/// | 16     | 1   | protocol version (`u8`)                         |
/// | 17     | 32  | Solana program id                               |
/// | 49     | 8   | attestation-key epoch (`u64` LE)                |
/// | 57     | 1   | action type (`ACTION_REFUND_WITHDRAW`)          |
/// | 58     | 8   | nonce (`u64` LE) — replay guard                 |
/// | 66     | 8   | amount, atomic reserve-mint units (`u64` LE)    |
/// | 74     | 32  | destination token account (the requester's ATA) |
/// | 106    | 32  | reserve GLC SPL mint pubkey                     |
/// | 138    | 32  | reserve token account the funds leave (source)  |
/// | 170    | 8   | withdrawal-obligation index (`u64` LE)          |
/// | 178    | 32  | the obligation's recorded `requester`           |
///
/// `obligation_index` is what makes a refund signature specific to one
/// deposit: the on-chain instruction loads that exact obligation PDA and
/// refuses unless the amount matches it and its status is still `Pending`.
///
/// `requester` is strictly redundant — `destination` is already the
/// canonical associated token account of `(requester, mint, token
/// program)`, which is a hash commitment to all three, and the on-chain
/// instruction enforces that derivation structurally via Anchor's
/// `associated_token::authority` constraint rather than by reading this
/// field. It is included anyway so an attestation signer can see, in the
/// bytes it is being asked to sign, WHO is being refunded — without
/// computing an ATA address itself. A signer that does derive the ATA and
/// finds it disagrees with `destination` is looking at a malformed
/// request and must refuse.
#[allow(clippy::too_many_arguments)]
pub fn refund_withdraw_claim_message(
    protocol_version: u8,
    program_id: &[u8; 32],
    attestation_epoch: u64,
    nonce: u64,
    amount: u64,
    destination: &[u8; 32],
    reserve_token_mint: &[u8; 32],
    reserve_token_account: &[u8; 32],
    obligation_index: u64,
    requester: &[u8; 32],
) -> [u8; REFUND_WITHDRAW_CLAIM_MESSAGE_LEN] {
    let mut m = [0u8; REFUND_WITHDRAW_CLAIM_MESSAGE_LEN];
    m[0..16].copy_from_slice(CLAIM_DOMAIN_TAG);
    m[16] = protocol_version;
    m[17..49].copy_from_slice(program_id);
    m[49..57].copy_from_slice(&attestation_epoch.to_le_bytes());
    m[57] = ACTION_REFUND_WITHDRAW;
    m[58..66].copy_from_slice(&nonce.to_le_bytes());
    m[66..74].copy_from_slice(&amount.to_le_bytes());
    m[74..106].copy_from_slice(destination);
    m[106..138].copy_from_slice(reserve_token_mint);
    m[138..170].copy_from_slice(reserve_token_account);
    m[170..178].copy_from_slice(&obligation_index.to_le_bytes());
    m[178..210].copy_from_slice(requester);
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

    fn rebalance_withdraw_sample() -> [u8; REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN] {
        rebalance_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x1122334455667788,
            0x0A0B0C0D0E0F1011,
            &[0x77; 32],
            &[0x44; 32],
        )
    }

    /// Golden vector: pins every byte, same discipline as the other two
    /// message families.
    #[test]
    fn rebalance_withdraw_golden_vector() {
        let m = rebalance_withdraw_sample();
        let mut expected = Vec::with_capacity(REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN);
        expected.extend_from_slice(b"GLC_RSV_CLAIM_V1");
        expected.push(1);
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        expected.push(ACTION_REBALANCE_WITHDRAW);
        expected.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        expected.extend_from_slice(&[0x11, 0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A]);
        expected.extend_from_slice(&[0x77; 32]);
        expected.extend_from_slice(&[0x44; 32]);
        assert_eq!(expected.len(), REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN);
        assert_eq!(m.as_slice(), expected.as_slice());
    }

    #[test]
    fn rebalance_withdraw_action_byte_distinct_from_release_and_completion() {
        assert_ne!(ACTION_REBALANCE_WITHDRAW, ACTION_RELEASE_FROM_RESERVE);
        assert_ne!(ACTION_REBALANCE_WITHDRAW, ACTION_RECORD_GOLDCOIN_COMPLETION);
        assert_ne!(ACTION_REBALANCE_WITHDRAW, 0x00);
    }

    #[test]
    fn rebalance_withdraw_length_distinct_from_release_and_completion() {
        assert_ne!(
            REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN,
            RELEASE_CLAIM_MESSAGE_LEN
        );
        assert_ne!(REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN, COMPLETION_MESSAGE_LEN);
    }

    #[test]
    fn rebalance_withdraw_shares_first_57_bytes_of_layout() {
        let w = rebalance_withdraw_sample();
        let r = release_sample();
        assert_eq!(w[..57], r[..57]);
    }

    #[test]
    fn rebalance_withdraw_differing_nonce_changes_exactly_its_own_bytes() {
        let a = rebalance_withdraw_sample();
        let b = rebalance_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x1122334455667789, // only the nonce differs
            0x0A0B0C0D0E0F1011,
            &[0x77; 32],
            &[0x44; 32],
        );
        assert_eq!(a[..58], b[..58]);
        assert_ne!(a[58..66], b[58..66]);
        assert_eq!(a[66..], b[66..]);
    }

    // ------------------------------------------------ treasury_withdraw --

    fn treasury_withdraw_sample() -> [u8; TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN] {
        treasury_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x1122334455667788,
            0x0A0B0C0D0E0F1011,
            &[0x77; 32],
            &[0x44; 32],
            &[0x99; 32],
            0x00000000000000AB,
        )
    }

    /// Golden vector: pins every byte, same discipline as every other
    /// family. A change here invalidates every outstanding treasury
    /// approval and is a deliberate protocol event.
    #[test]
    fn treasury_withdraw_golden_vector() {
        let m = treasury_withdraw_sample();
        let mut expected = Vec::with_capacity(TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN);
        expected.extend_from_slice(b"GLC_RSV_CLAIM_V1");
        expected.push(1);
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        expected.push(ACTION_TREASURY_WITHDRAW);
        expected.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        expected.extend_from_slice(&[0x11, 0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A]);
        expected.extend_from_slice(&[0x77; 32]);
        expected.extend_from_slice(&[0x44; 32]);
        expected.extend_from_slice(&[0x99; 32]);
        expected.extend_from_slice(&[0xAB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(expected.len(), TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN);
        assert_eq!(m.as_slice(), expected.as_slice());
    }

    /// The policy-revision binding: two otherwise-identical withdrawals
    /// approved under different `RebalancePolicy` revisions produce
    /// different bytes, so an approval gathered under the old allowlist
    /// cannot be replayed after governance changes it.
    #[test]
    fn treasury_withdraw_policy_version_changes_exactly_its_own_bytes() {
        let a = treasury_withdraw_sample();
        let b = treasury_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x1122334455667788,
            0x0A0B0C0D0E0F1011,
            &[0x77; 32],
            &[0x44; 32],
            &[0x99; 32],
            0x00000000000000AC, // only the policy version differs
        );
        assert_eq!(a[..170], b[..170]);
        assert_ne!(a[170..178], b[170..178]);
    }

    #[test]
    fn treasury_withdraw_destination_changes_exactly_its_own_bytes() {
        let a = treasury_withdraw_sample();
        let b = treasury_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x1122334455667788,
            0x0A0B0C0D0E0F1011,
            &[0x78; 32], // only the destination differs
            &[0x44; 32],
            &[0x99; 32],
            0x00000000000000AB,
        );
        assert_eq!(a[..74], b[..74]);
        assert_ne!(a[74..106], b[74..106]);
        assert_eq!(a[106..], b[106..]);
    }

    // -------------------------------------------------- refund_withdraw --

    fn refund_withdraw_sample() -> [u8; REFUND_WITHDRAW_CLAIM_MESSAGE_LEN] {
        refund_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x8000000000000005,
            0x0A0B0C0D0E0F1011,
            &[0x77; 32],
            &[0x44; 32],
            &[0x99; 32],
            0x0000000000000007,
            &[0xCC; 32],
        )
    }

    #[test]
    fn refund_withdraw_golden_vector() {
        let m = refund_withdraw_sample();
        let mut expected = Vec::with_capacity(REFUND_WITHDRAW_CLAIM_MESSAGE_LEN);
        expected.extend_from_slice(b"GLC_RSV_CLAIM_V1");
        expected.push(1);
        expected.extend_from_slice(&[0x11; 32]);
        expected.extend_from_slice(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        expected.push(ACTION_REFUND_WITHDRAW);
        expected.extend_from_slice(&[0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80]);
        expected.extend_from_slice(&[0x11, 0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A]);
        expected.extend_from_slice(&[0x77; 32]);
        expected.extend_from_slice(&[0x44; 32]);
        expected.extend_from_slice(&[0x99; 32]);
        expected.extend_from_slice(&[0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        expected.extend_from_slice(&[0xCC; 32]);
        assert_eq!(expected.len(), REFUND_WITHDRAW_CLAIM_MESSAGE_LEN);
        assert_eq!(m.as_slice(), expected.as_slice());
    }

    #[test]
    fn refund_withdraw_obligation_index_changes_exactly_its_own_bytes() {
        let a = refund_withdraw_sample();
        let b = refund_withdraw_claim_message(
            1,
            &[0x11; 32],
            0x0102030405060708,
            0x8000000000000005,
            0x0A0B0C0D0E0F1011,
            &[0x77; 32],
            &[0x44; 32],
            &[0x99; 32],
            0x0000000000000008, // only the obligation index differs
            &[0xCC; 32],
        );
        assert_eq!(a[..170], b[..170]);
        assert_ne!(a[170..178], b[170..178]);
        assert_eq!(a[178..], b[178..]);
    }

    // ------------------------------------------- cross-family separation --

    /// The property the on-chain instructions rely on: a signature
    /// gathered for one withdrawal class can never verify as the other.
    /// Byte 57 alone is sufficient (the program compares the FULL message
    /// for byte equality), but every family also carries a unique length,
    /// so a confusion would have to defeat two independent checks.
    #[test]
    fn every_claim_family_has_a_unique_action_byte_and_length() {
        let families: [(u8, usize); 5] = [
            (ACTION_RELEASE_FROM_RESERVE, RELEASE_CLAIM_MESSAGE_LEN),
            (ACTION_RECORD_GOLDCOIN_COMPLETION, COMPLETION_MESSAGE_LEN),
            (
                ACTION_REBALANCE_WITHDRAW,
                REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN,
            ),
            (
                ACTION_TREASURY_WITHDRAW,
                TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN,
            ),
            (ACTION_REFUND_WITHDRAW, REFUND_WITHDRAW_CLAIM_MESSAGE_LEN),
        ];
        for (i, (action_a, len_a)) in families.iter().enumerate() {
            assert_ne!(*action_a, 0x00, "0x00 is never a valid action");
            for (action_b, len_b) in families.iter().skip(i + 1) {
                assert_ne!(action_a, action_b, "duplicate action byte");
                assert_ne!(len_a, len_b, "duplicate message length");
            }
        }
    }

    /// A treasury approval and a refund approval that agree on every
    /// shared field still differ — the withdrawal CLASS is part of what
    /// the attestation signers approve, not an off-chain label.
    #[test]
    fn treasury_and_refund_messages_never_coincide() {
        let treasury = treasury_withdraw_claim_message(
            1,
            &[0x11; 32],
            7,
            9,
            100,
            &[0x77; 32],
            &[0x44; 32],
            &[0x99; 32],
            0,
        );
        let refund = refund_withdraw_claim_message(
            1,
            &[0x11; 32],
            7,
            9,
            100,
            &[0x77; 32],
            &[0x44; 32],
            &[0x99; 32],
            0,
            &[0x00; 32],
        );
        assert_eq!(treasury[..57], refund[..57]);
        assert_ne!(treasury[57], refund[57]);
        // Neither is a prefix of the other, so a truncating parser cannot
        // turn one into the other either.
        assert_ne!(
            treasury.as_slice(),
            &refund[..TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN]
        );
    }

    /// The retired family must remain byte-distinct from both
    /// replacements, so a signature collected for the old unrestricted
    /// path can never be presented as a treasury or refund approval.
    #[test]
    fn retired_rebalance_message_is_distinct_from_both_replacements() {
        let retired = rebalance_withdraw_sample();
        let treasury = treasury_withdraw_sample();
        let refund = refund_withdraw_sample();
        assert_ne!(retired[57], treasury[57]);
        assert_ne!(retired[57], refund[57]);
        assert_ne!(
            retired.as_slice(),
            &treasury[..REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN]
        );
        assert_ne!(
            retired.as_slice(),
            &refund[..REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN]
        );
    }
}
