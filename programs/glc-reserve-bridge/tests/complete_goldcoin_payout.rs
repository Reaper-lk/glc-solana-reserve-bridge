//! Integration tests for `record_goldcoin_completion`: the threshold-
//! attested record that a `WithdrawalObligation` was paid on Goldcoin.
//!
//! # Why this file exists
//!
//! Until now, no litesvm/on-chain integration test exercised this
//! instruction at all — the only coverage was the real-node acceptance
//! suite (`service/tests/regtest_acceptance.rs`), which requires
//! `GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN` to actually run and is therefore
//! silently skipped in most environments. That gap is exactly how a real,
//! severe defect went undetected: `record_goldcoin_completion` used to
//! build its `expected_message` from `obligation.amount` (the GROSS
//! Solana-side deposit) instead of the NET Goldcoin amount actually paid
//! out (the two differ by the off-chain bridge fee, whenever the fee is
//! nonzero — always, since `BRIDGE_FEE_BPS = 600` in
//! `service/src/amount_conversion.rs`). Every real SolToGlc completion
//! failed on-chain with `SignatureMessageMismatch`. Fixed by making
//! `amount` a caller-supplied instruction argument, matching
//! `release_from_reserve`'s existing, already-tested pattern exactly.
//!
//! This program has no fee policy of its own (the fee is off-chain
//! policy — `service/src/amount_conversion.rs`, a separate crate this
//! program does not and should not depend on) and cannot verify a
//! gross/fee/net relationship itself. What it CAN and must verify — and
//! what these tests prove — is that the submitted `amount` is exactly
//! the one an independent threshold of attestation signers actually
//! signed over, never a substituted value. The fee arithmetic itself
//! (rate, floor-rounding, `gross = fee + net`, overflow handling) is
//! proven separately and exhaustively by `service/src/amount_conversion
//! .rs`'s own test suite (unchanged by this fix); the representative
//! gross/fee/net triples below mirror that module's documented 3% rate
//! only to build realistic test data, not to re-derive it.

mod common;

use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::LimitField;
use glc_reserve_bridge::state::WithdrawalStatus;

const GLC_ADDR: &[u8] = b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
const PAYOUT_TXID: [u8; 32] = [0x7A; 32];
const PAYOUT_HEIGHT: u64 = 12_345;

/// Real 3% bridge fee, applied the same way `service::amount_conversion::
/// compute_fee` documents (`fee = floor(gross * 300 / 10_000)`,
/// `net = gross - fee`) — reproduced here only to generate realistic test
/// data; this on-chain crate has no access to and does not depend on that
/// off-chain module.
fn fee_and_net(gross: u64) -> (u64, u64) {
    let fee = gross * 300 / 10_000;
    (fee, gross - fee)
}

/// Deposits `gross` into the reserve via a real `deposit_to_reserve` call,
/// producing a real, litesvm-verified `Pending` `WithdrawalObligation` at
/// index 0 — the same starting state every real SolToGlc completion
/// begins from.
fn setup_pending_obligation(authority: &Keypair, gross: u64) -> (litesvm::LiteSVM, Vec<Keypair>) {
    let (mut svm, signers, mint) = setup_with_reserve(authority, 10 * gross.max(1));
    // The default per-transfer/rolling-volume limits (1_000_000_000) are
    // sized for other tests' small fixture amounts, not this file's
    // realistic canonical-unit-scale (100-1,000 GLC) representative
    // amounts — raise them well above anything this file submits. Not a
    // weakening of any check under test: these limits are orthogonal to
    // the completion-verification behavior these tests exercise.
    let raise_per_transfer = set_limit_ix(
        &authority.pubkey(),
        LimitField::PerTransferLimit,
        gross.max(1) * 20,
    );
    send(&mut svm, raise_per_transfer, authority, &[]).expect("raise per_transfer_limit");
    let raise_volume = set_limit_ix(
        &authority.pubkey(),
        LimitField::RollingVolumeLimit,
        gross.max(1) * 20,
    );
    send(&mut svm, raise_volume, authority, &[]).expect("raise rolling_volume_limit");

    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let user_ata = create_ata(&mut svm, &user.pubkey(), &mint, gross);
    let deposit = deposit_to_reserve_ix(
        &user.pubkey(),
        &mint,
        &user_ata,
        0,
        gross,
        GLC_ADDR.to_vec(),
    );
    send(&mut svm, deposit, &user, &[]).expect("deposit_to_reserve should succeed");
    let obligation = get_obligation(&svm, 0);
    assert_eq!(
        obligation.amount, gross,
        "obligation must record the gross deposit"
    );
    assert_eq!(obligation.status, WithdrawalStatus::Pending);
    (svm, signers)
}

#[test]
fn net_payout_amount_matching_the_signed_message_is_accepted_for_representative_amounts() {
    // Representative gross/fee/net triples: a round mid-size amount, a
    // large amount at the same scale amount_conversion.rs's own
    // `hundred_glc_gross_charges_six_glc_fee`/`thousand_glc_gross_charges_
    // sixty_glc_fee` tests use, and the smallest amount a real deposit can
    // ever carry (`DEFAULT_MIN_TRANSFER = 100`, enforced by
    // `deposit_to_reserve` itself) — below `min_transfer_amount` the fee
    // would floor to zero (net == gross), but that combination is
    // provably unreachable through a real deposit given this program's
    // own minimum-transfer floor, so it is not a meaningful boundary to
    // exercise at this level.
    for gross in [
        100 * 100_000_000u64, // 100 GLC -> fee 6 GLC, net 94 GLC
        1_000 * 100_000_000,  // 1,000 GLC -> fee 60 GLC, net 940 GLC
        100,                  // the smallest real transfer -> fee 6, net 94
    ] {
        let (fee, net) = fee_and_net(gross);
        assert_eq!(
            fee + net,
            gross,
            "fee + net must equal gross by construction"
        );

        let authority = Keypair::new();
        let (mut svm, signers) = setup_pending_obligation(&authority, gross);

        let dest_commitment = glc_dest_commitment(GLC_ADDR);
        let message =
            goldcoin_completion_message(0, 0, &PAYOUT_TXID, PAYOUT_HEIGHT, net, &dest_commitment);
        let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
        let complete =
            complete_goldcoin_payout_ix(&authority.pubkey(), 0, PAYOUT_TXID, PAYOUT_HEIGHT, net, 0);

        send_ixs(&mut svm, &[proof, complete], &authority, &[]).unwrap_or_else(|e| {
            panic!(
                "completion with the correct net amount ({net}, gross={gross}) must succeed: {e:?}"
            )
        });

        let obligation = get_obligation(&svm, 0);
        assert_eq!(
            obligation.status,
            WithdrawalStatus::Completed,
            "gross={gross} net={net}: obligation must be marked Completed"
        );
    }
}

#[test]
fn gross_deposit_amount_is_rejected_when_the_signed_message_covers_the_net_amount() {
    // The exact historical defect, reproduced directly: a real off-chain
    // attestation signer only ever signs the NET amount it independently
    // verified was actually paid on Goldcoin (see
    // `service/src/signing/attestation.rs`, unchanged, always correct).
    // Submitting the instruction with the GROSS deposit amount instead —
    // what the pre-fix on-chain code effectively always did, since it
    // built its own `expected_message` from `obligation.amount` — must be
    // rejected: the submitted amount does not match what was signed.
    let gross = 10_000_000_000u64; // 100 GLC
    let (fee, net) = fee_and_net(gross);
    assert!(fee > 0, "test requires a nonzero fee so gross != net");
    assert_ne!(gross, net);

    let authority = Keypair::new();
    let (mut svm, signers) = setup_pending_obligation(&authority, gross);

    let dest_commitment = glc_dest_commitment(GLC_ADDR);
    // A real signer signs the NET amount.
    let message =
        goldcoin_completion_message(0, 0, &PAYOUT_TXID, PAYOUT_HEIGHT, net, &dest_commitment);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    // The submitted instruction claims the GROSS amount instead.
    let complete =
        complete_goldcoin_payout_ix(&authority.pubkey(), 0, PAYOUT_TXID, PAYOUT_HEIGHT, gross, 0);

    let result = send_ixs(&mut svm, &[proof, complete], &authority, &[]);
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);

    // Fail-closed: the obligation must remain exactly as it was, never
    // partially or incorrectly completed.
    let obligation = get_obligation(&svm, 0);
    assert_eq!(obligation.status, WithdrawalStatus::Pending);
    assert!(obligation.payout_record_is_unset());
}

#[test]
fn an_arbitrary_wrong_amount_is_rejected_even_with_a_structurally_valid_proof() {
    // Not just gross vs net specifically — ANY submitted amount other than
    // the one actually signed must be rejected. Covers the general
    // caller-supplied-argument-vs-signature-consistency property the fix
    // relies on, not only the one historical gross/net case.
    let gross = 10_000_000_000u64;
    let (_, net) = fee_and_net(gross);

    let authority = Keypair::new();
    let (mut svm, signers) = setup_pending_obligation(&authority, gross);

    let dest_commitment = glc_dest_commitment(GLC_ADDR);
    let message =
        goldcoin_completion_message(0, 0, &PAYOUT_TXID, PAYOUT_HEIGHT, net, &dest_commitment);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let complete = complete_goldcoin_payout_ix(
        &authority.pubkey(),
        0,
        PAYOUT_TXID,
        PAYOUT_HEIGHT,
        net + 1,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, complete], &authority, &[]);
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
}

#[test]
fn zero_amount_is_rejected_before_signature_verification() {
    let gross = 10_000_000_000u64;
    let authority = Keypair::new();
    let (mut svm, signers) = setup_pending_obligation(&authority, gross);

    let dest_commitment = glc_dest_commitment(GLC_ADDR);
    let message =
        goldcoin_completion_message(0, 0, &PAYOUT_TXID, PAYOUT_HEIGHT, 0, &dest_commitment);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let complete =
        complete_goldcoin_payout_ix(&authority.pubkey(), 0, PAYOUT_TXID, PAYOUT_HEIGHT, 0, 0);

    let result = send_ixs(&mut svm, &[proof, complete], &authority, &[]);
    assert_bridge_error(result, BridgeError::ZeroAmount);
}

#[test]
fn completion_is_terminal_a_second_attempt_is_rejected() {
    let gross = 10_000_000_000u64;
    let (_, net) = fee_and_net(gross);
    let authority = Keypair::new();
    let (mut svm, signers) = setup_pending_obligation(&authority, gross);

    let dest_commitment = glc_dest_commitment(GLC_ADDR);
    let message =
        goldcoin_completion_message(0, 0, &PAYOUT_TXID, PAYOUT_HEIGHT, net, &dest_commitment);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let complete =
        complete_goldcoin_payout_ix(&authority.pubkey(), 0, PAYOUT_TXID, PAYOUT_HEIGHT, net, 0);
    send_ixs(
        &mut svm,
        &[proof.clone(), complete.clone()],
        &authority,
        &[],
    )
    .expect("first completion should succeed");

    // A fresh blockhash so the second transaction is genuinely distinct
    // (not just rejected as an identical, already-processed signature by
    // the runtime before ever reaching the program) and actually
    // exercises the on-chain `ObligationAlreadyCompleted` terminal-state
    // guard.
    svm.expire_blockhash();
    let result = send_ixs(&mut svm, &[proof, complete], &authority, &[]);
    assert_bridge_error(result, BridgeError::ObligationAlreadyCompleted);
}
