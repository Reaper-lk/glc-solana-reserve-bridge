//! Integration tests for `release_from_reserve`: the Goldcoin-deposit ->
//! Solana-reserve-release path. Covers the safety-critical properties named
//! in the brief: replay rejection, insufficient-reserve rejection, cap
//! enforcement, pause enforcement, and threshold-attestation verification.

mod common;

use anchor_spl::associated_token::get_associated_token_address;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;

const TXID: [u8; 32] = [0x42; 32];
const VOUT: u32 = 0;
const AMOUNT: u64 = 5_000;

#[test]
fn happy_path_2_of_3_releases_1_to_1_and_records_claim() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    send_ixs(&mut svm, &[proof, release], &authority, &[]).expect("release should succeed");

    assert_eq!(token_balance(&svm, &recipient_ata), AMOUNT);
    let reserve_ata = get_associated_token_address(&reserve_authority_pda(), &mint);
    assert_eq!(token_balance(&svm, &reserve_ata), 1_000_000 - AMOUNT);

    let claim = get_claim(&svm, &TXID, VOUT);
    assert_eq!(claim.amount, AMOUNT);
    assert_eq!(claim.recipient, recipient.pubkey());
}

#[test]
fn release_uses_the_real_mints_decimals_not_a_hardcoded_constant() {
    // Regression: `transfer_checked`'s `decimals` argument used to be the
    // hardcoded `GLC_DECIMALS` constant (8), not the reserve mint's own
    // `decimals` field. `transfer_checked` validates its `decimals`
    // argument against the mint account and errors on any mismatch, so a
    // real mint with a different decimals count than the hardcoded guess
    // would have made every release fail on-chain — exactly the situation
    // found for the real production Solana GLC mint, which uses 6
    // decimals, not 8 (docs/16-p0-checkpoint.md). This pins the fix: a
    // mint with a decimals count *other than* the old hardcoded value
    // must still release correctly.
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve_and_decimals(&authority, 1_000_000, 6);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    send_ixs(&mut svm, &[proof, release], &authority, &[])
        .expect("release must succeed against a 6-decimal mint, not just an 8-decimal one");
    assert_eq!(token_balance(&svm, &recipient_ata), AMOUNT);
}

#[test]
fn replay_of_the_same_txid_vout_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );
    send_ixs(&mut svm, &[proof.clone(), release.clone()], &authority, &[])
        .expect("first release should succeed");

    // A second attempt with the exact same (txid, vout) — the claim PDA
    // already exists, so account creation (`init`) fails. This is the
    // on-chain replay guard constraint 5 requires.
    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert!(result.is_err(), "replay must be rejected");
}

#[test]
fn insufficient_reserve_fails_closed() {
    let authority = Keypair::new();
    // Reserve holds less than the requested amount.
    let (mut svm, signers, mint) = setup_with_reserve(&authority, AMOUNT - 1);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::InsufficientReserveBalance);

    // Fail-closed means no claim was created and no funds moved.
    assert_eq!(svm.get_account(&claim_pda(&TXID, VOUT)), None);
    assert_eq!(token_balance(&svm, &recipient_ata), 0);
}

#[test]
fn release_below_protected_minimum_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, AMOUNT + 500);
    // Raise the protected minimum above what would remain after the
    // release: reserve holds AMOUNT + 500, protected_minimum = 600 means at
    // most 500 - (600 - 500) ... concretely: releasing AMOUNT would leave
    // 500, which is below a 600 floor.
    let set_min = set_limit_ix(
        &authority.pubkey(),
        glc_reserve_bridge::instructions::admin::LimitField::ProtectedMinimum,
        600,
    );
    send(&mut svm, set_min, &authority, &[]).expect("set_limit should succeed");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::InsufficientReserveBalance);
}

#[test]
fn amount_above_per_transfer_limit_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 10_000_000);
    let set_limit = set_limit_ix(
        &authority.pubkey(),
        glc_reserve_bridge::instructions::admin::LimitField::PerTransferLimit,
        AMOUNT - 1,
    );
    send(&mut svm, set_limit, &authority, &[]).expect("set_limit should succeed");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::ExceedsPerTransferLimit);
}

#[test]
fn amount_above_rolling_volume_limit_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 10_000_000);
    let set_limit = set_limit_ix(
        &authority.pubkey(),
        glc_reserve_bridge::instructions::admin::LimitField::RollingVolumeLimit,
        AMOUNT - 1,
    );
    send(&mut svm, set_limit, &authority, &[]).expect("set_limit should succeed");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::ExceedsRollingVolumeLimit);
}

/// Regression for the exact invariant a quota-exhausted -> operator-pause
/// -> unpause operational workflow depends on: `set_paused` and the
/// rolling-volume window are completely independent on-chain state
/// (`BridgeConfig::release_paused` vs. `RollingVolumeWindow::window_
/// total`, checked by two unrelated `require!`s in `release_from_
/// reserve`/`limits::enforce_and_record_rolling_volume`) — flipping the
/// pause flag off and back on again must never reset, touch, or bypass
/// the quota. An operator who pauses a direction, then unpauses it while
/// the rolling window is still genuinely exhausted, must see the exact
/// same `ExceedsRollingVolumeLimit` rejection as before the pause/
/// unpause round trip, not a silent quota reset and not a stale pause
/// error.
#[test]
fn pausing_and_unpausing_the_release_leg_does_not_reset_or_bypass_the_rolling_volume_quota() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 10_000_000);
    let set_limit = set_limit_ix(
        &authority.pubkey(),
        glc_reserve_bridge::instructions::admin::LimitField::RollingVolumeLimit,
        AMOUNT - 1,
    );
    send(&mut svm, set_limit, &authority, &[]).expect("set_limit should succeed");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = || {
        release_from_reserve_ix(
            &authority.pubkey(),
            &mint,
            &recipient.pubkey(),
            &recipient_ata,
            TXID,
            VOUT,
            AMOUNT,
            0,
        )
    };

    // Confirm quota exhaustion on its own, before any pause is involved.
    let result = send_ixs(&mut svm, &[proof.clone(), release()], &authority, &[]);
    assert_bridge_error(result, BridgeError::ExceedsRollingVolumeLimit);

    // Operator pauses the release leg (e.g. in response to the exhaustion
    // being visible off-chain), then unpauses it again (e.g. after a
    // reserve refill/reconciliation) — the quota itself was never
    // refilled or reset by either step.
    let pause = set_paused_ix(&authority.pubkey(), PauseScope::Release, true);
    send(&mut svm, pause, &authority, &[]).expect("pause should succeed");
    let unpause = set_paused_ix(&authority.pubkey(), PauseScope::Release, false);
    send(&mut svm, unpause, &authority, &[]).expect("unpause should succeed");

    let config = get_config(&svm);
    assert!(
        !config.release_paused,
        "the pause round trip must leave the direction unpaused"
    );

    // A fresh blockhash only — the clock is untouched, so this cannot
    // itself cross a rolling-window bucket boundary; without it the
    // second, otherwise-identical transaction would be rejected as
    // `AlreadyProcessed` rather than actually re-checked.
    svm.expire_blockhash();

    // The exact same claim, replayed after the pause/unpause round trip,
    // must still fail on the rolling-volume check — never succeed
    // (quota silently bypassed) and never fail with a pause error
    // (a stale/incorrectly-cleared pause state).
    let result = send_ixs(&mut svm, &[proof, release()], &authority, &[]);
    assert_bridge_error(result, BridgeError::ExceedsRollingVolumeLimit);
}

#[test]
fn globally_paused_bridge_rejects_release() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let pause = set_paused_ix(&authority.pubkey(), PauseScope::Global, true);
    send(&mut svm, pause, &authority, &[]).expect("pause should succeed");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::BridgeGloballyPaused);
}

#[test]
fn directionally_paused_release_leg_rejects_release_but_would_not_block_deposit() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let pause = set_paused_ix(&authority.pubkey(), PauseScope::Release, true);
    send(&mut svm, pause, &authority, &[]).expect("directional pause should succeed");

    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::ReleaseDirectionPaused);

    // Confirm the config reflects ONLY the release direction paused — the
    // deposit direction and global flag are untouched, proving pause scopes
    // are independent (docs/09-runbook.md's directional-pause requirement).
    let config = get_config(&svm);
    assert!(config.release_paused);
    assert!(!config.deposit_paused);
    assert!(!config.paused);
}

#[test]
fn single_signature_below_threshold_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    // Only one of the required 2 signers — the approved trust model
    // (docs/02-trust-model.md) requires threshold >= 2 precisely so this
    // must fail.
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
    assert_eq!(
        token_balance(&svm, &recipient_ata),
        0,
        "no partial release on insufficient signatures"
    );
}

#[test]
fn signatures_from_non_attestation_keys_are_rejected() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    // Two keys that were never part of the attestation key set — a
    // compromised orchestrator cannot manufacture authority by signing with
    // arbitrary keys (constraint 3).
    let outsiders = keys(2);
    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&outsiders[0], &outsiders[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::UnknownAttestationSigner);
}

#[test]
fn tampered_amount_after_signing_is_rejected() {
    // The signed message binds the amount; a submitter cannot ask for a
    // valid attestation on one amount and release a different one.
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);

    let message = release_claim_message(0, &TXID, VOUT, AMOUNT, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    // Instruction claims a different (larger) amount than what was signed.
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        TXID,
        VOUT,
        AMOUNT + 1,
        0,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
}
