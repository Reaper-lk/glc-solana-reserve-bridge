//! Integration tests for rebalance-policy governance: who may create,
//! change and cancel the treasury allowlist and the withdrawal limits.
//!
//! The whole point of this account is that the admin key cannot reach it.
//! An allowlist a compromised admin could edit would be worth nothing — the
//! attacker would add their own token account and then take the ordinary,
//! fully-attested treasury path. So the tests here are mostly about who is
//! refused.

mod common;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;
use glc_reserve_bridge::instructions::admin::PauseScope;

const RESERVE: u64 = 1_000_000;
const WITHDRAW_AMOUNT: u64 = 100_000;
const ROLLING_LIMIT: u64 = 250_000;
const WINDOW_SECONDS: i64 = 86_400;

// --------------------------------------------------------- initialize --

#[test]
fn initializes_with_exactly_one_canonical_treasury() {
    let authority = Keypair::new();
    let (svm, _signers, _mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);

    let policy = get_rebalance_policy(&svm);
    assert_eq!(policy.version, 0);
    assert_eq!(policy.treasury_count, 1);
    assert_eq!(policy.treasuries[0], treasury);
    assert_eq!(policy.rolling_limit, ROLLING_LIMIT);
    assert_eq!(policy.rolling_window_seconds, WINDOW_SECONDS);
    assert_eq!(policy.window_total, 0);
    assert!(policy.is_allowlisted(&treasury));
    // The unused tail is zeroed, so nothing stale can ever be read out of it.
    for slot in &policy.treasuries[1..] {
        assert_eq!(*slot, Pubkey::default());
    }
}

/// Creating the allowlist requires a threshold attestation. An admin
/// signature is neither necessary nor sufficient — this is the test that
/// says a compromised production host cannot install its own treasury.
#[test]
fn initialization_without_a_threshold_attestation_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint) = setup_with_reserve(&authority, RESERVE);
    let treasury_owner = Pubkey::new_unique();
    let treasury = create_ata(&mut svm, &treasury_owner, &mint, 0);

    let result = send(
        &mut svm,
        initialize_rebalance_policy_ix(
            &authority.pubkey(),
            &mint,
            vec![treasury],
            ROLLING_LIMIT,
            WINDOW_SECONDS,
        ),
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::MissingSignatureVerification);
    assert!(!rebalance_policy_exists(&svm));
}

#[test]
fn initialization_below_the_threshold_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let treasury_owner = Pubkey::new_unique();
    let treasury = create_ata(&mut svm, &treasury_owner, &mint, 0);

    let message =
        initialize_rebalance_policy_message(0, &[treasury], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0]], &message), // 1 of 2
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
}

/// The parameters are committed into the signed message, so an approval
/// for one allowlist cannot be submitted with a different one.
#[test]
fn initialization_with_parameters_the_attestation_did_not_cover_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let approved = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);
    let substituted = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let message =
        initialize_rebalance_policy_message(0, &[approved], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![substituted], // not what was approved
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
}

/// Raising the per-withdrawal limit past what was approved must fail for
/// the same reason: the limits are part of the commitment, not metadata.
#[test]
fn initialization_with_a_raised_limit_the_attestation_did_not_cover_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let message =
        initialize_rebalance_policy_message(0, &[treasury], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                ROLLING_LIMIT * 10,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
}

#[test]
fn initialization_is_one_time_only() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, _treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    let attacker_treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let message =
        initialize_rebalance_policy_message(0, &[attacker_treasury], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![attacker_treasury],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert!(
        result.is_err(),
        "a policy may only be created once; replacement goes through the timelock"
    );
    assert_eq!(get_rebalance_policy(&svm).treasury_count, 1);
    assert!(!get_rebalance_policy(&svm).is_allowlisted(&attacker_treasury));
}

#[test]
fn invalid_policy_parameters_are_rejected_at_initialization() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let t = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    // An empty allowlist.
    let message = initialize_rebalance_policy_message(0, &[], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::EmptyTreasuryAllowlist);

    // A duplicate entry.
    let message = initialize_rebalance_policy_message(0, &[t, t], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![t, t],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::DuplicateTreasuryDestination);

    assert!(!rebalance_policy_exists(&svm));
}

// ------------------------------------------------------ propose/execute --

struct Proposed {
    svm: litesvm::LiteSVM,
    signers: Vec<Keypair>,
    mint: Pubkey,
    authority: Keypair,
    treasury: Pubkey,
    second_treasury: Pubkey,
}

/// A live policy plus a queued proposal adding a SECOND treasury — the
/// staged-rotation shape the allowlist cap exists for.
fn proposed() -> Proposed {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    let second_treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let message = propose_rebalance_policy_message(
        0,
        &[treasury, second_treasury],
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            propose_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury, second_treasury],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    )
    .expect("proposal should succeed");

    Proposed {
        svm,
        signers,
        mint,
        authority,
        treasury,
        second_treasury,
    }
}

#[test]
fn a_proposal_does_not_take_effect_until_executed() {
    let p = proposed();
    let policy = get_rebalance_policy(&p.svm);
    assert_eq!(policy.version, 0);
    assert_eq!(policy.treasury_count, 1);
    assert!(!policy.is_allowlisted(&p.second_treasury));

    let pending = get_pending_rebalance_policy(&p.svm);
    assert_eq!(pending.treasury_count, 2);
    assert_eq!(pending.proposed_under_epoch, 0);
}

#[test]
fn execution_before_the_timelock_elapses_is_rejected() {
    let mut p = proposed();
    let result = send(
        &mut p.svm,
        execute_rebalance_policy_ix(&p.authority.pubkey(), &p.mint),
        &p.authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::RebalancePolicyTimelockNotElapsed);
    assert_eq!(get_rebalance_policy(&p.svm).treasury_count, 1);
}

#[test]
fn execution_after_the_timelock_applies_the_policy_and_bumps_the_version() {
    let mut p = proposed();
    warp_seconds(&mut p.svm, DEFAULT_TEST_TIMELOCK);

    // Permissionless: an unrelated account may execute.
    let stranger = Keypair::new();
    p.svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    send(
        &mut p.svm,
        execute_rebalance_policy_ix(&stranger.pubkey(), &p.mint),
        &stranger,
        &[],
    )
    .expect("execution is permissionless once the timelock has elapsed");

    let policy = get_rebalance_policy(&p.svm);
    assert_eq!(policy.version, 1);
    assert_eq!(policy.treasury_count, 2);
    assert!(policy.is_allowlisted(&p.treasury));
    assert!(policy.is_allowlisted(&p.second_treasury));
}

/// A policy update must not refill the withdrawal budget, or a quorum
/// could top itself up by re-approving the policy it already has.
#[test]
fn executing_a_policy_update_does_not_reset_the_rolling_window() {
    let mut p = proposed();

    // Spend some budget first.
    let message = treasury_withdraw_claim_message(0, 1, WITHDRAW_AMOUNT, &p.treasury, &p.mint, 0);
    send_ixs(
        &mut p.svm,
        &[
            ed25519_proof_ix(&[&p.signers[0], &p.signers[1]], &message),
            treasury_withdraw_ix(
                &p.authority.pubkey(),
                &p.mint,
                &p.treasury,
                1,
                WITHDRAW_AMOUNT,
                0,
            ),
        ],
        &p.authority,
        &[],
    )
    .expect("withdrawal");
    assert_eq!(get_rebalance_policy(&p.svm).window_total, WITHDRAW_AMOUNT);

    warp_seconds(&mut p.svm, DEFAULT_TEST_TIMELOCK);
    send(
        &mut p.svm,
        execute_rebalance_policy_ix(&p.authority.pubkey(), &p.mint),
        &p.authority,
        &[],
    )
    .expect("execute");

    let policy = get_rebalance_policy(&p.svm);
    assert_eq!(policy.version, 1);
    assert_eq!(
        policy.window_total, WITHDRAW_AMOUNT,
        "a governance change is not a budget top-up"
    );
}

/// An attestation-key rotation invalidates every queued policy change: the
/// quorum that approved it may no longer be the quorum that exists.
#[test]
fn a_proposal_dies_when_the_attestation_keys_rotate() {
    let mut p = proposed();

    let new_keys: Vec<Pubkey> = p.signers.iter().map(|s| s.pubkey()).rev().collect();
    let rotation_msg = rotation_message(0, &new_keys, 2);
    send_ixs(
        &mut p.svm,
        &[
            ed25519_proof_ix(&[&p.signers[0], &p.signers[1]], &rotation_msg),
            propose_rotation_ix(&p.authority.pubkey(), new_keys, 2),
        ],
        &p.authority,
        &[],
    )
    .expect("propose rotation");
    warp_seconds(&mut p.svm, DEFAULT_TEST_TIMELOCK);
    send(
        &mut p.svm,
        execute_rotation_ix(&p.authority.pubkey()),
        &p.authority,
        &[],
    )
    .expect("execute rotation");
    assert_eq!(get_attestation_key_set(&p.svm).epoch, 1);

    let result = send(
        &mut p.svm,
        execute_rebalance_policy_ix(&p.authority.pubkey(), &p.mint),
        &p.authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::StaleRebalancePolicyProposal);
    assert_eq!(get_rebalance_policy(&p.svm).treasury_count, 1);
}

#[test]
fn only_one_policy_change_may_be_pending_at_a_time() {
    let mut p = proposed();
    let third = create_ata(&mut p.svm, &Pubkey::new_unique(), &p.mint, 0);

    let message = propose_rebalance_policy_message(0, &[third], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut p.svm,
        &[
            ed25519_proof_ix(&[&p.signers[0], &p.signers[1]], &message),
            propose_rebalance_policy_ix(
                &p.authority.pubkey(),
                &p.mint,
                vec![third],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &p.authority,
        &[],
    );
    assert!(
        result.is_err(),
        "a briefly-compromised quorum must not be able to queue a backlog"
    );
}

#[test]
fn a_proposal_without_a_threshold_attestation_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, _signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    let attacker_treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let result = send(
        &mut svm,
        propose_rebalance_policy_ix(
            &authority.pubkey(),
            &mint,
            vec![treasury, attacker_treasury],
            ROLLING_LIMIT,
            WINDOW_SECONDS,
        ),
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::MissingSignatureVerification);
}

// ------------------------------------------------------------- cancel --

#[test]
fn a_threshold_proof_cancels_a_pending_change() {
    let mut p = proposed();
    let eta = get_pending_rebalance_policy(&p.svm).eta;

    let message = cancel_rebalance_policy_message(0, eta);
    send_ixs(
        &mut p.svm,
        &[
            ed25519_proof_ix(&[&p.signers[0], &p.signers[1]], &message),
            cancel_rebalance_policy_ix(&p.authority.pubkey()),
        ],
        &p.authority,
        &[],
    )
    .expect("cancel should succeed");

    warp_seconds(&mut p.svm, DEFAULT_TEST_TIMELOCK);
    let result = send(
        &mut p.svm,
        execute_rebalance_policy_ix(&p.authority.pubkey(), &p.mint),
        &p.authority,
        &[],
    );
    assert!(result.is_err(), "the pending account no longer exists");
    assert_eq!(get_rebalance_policy(&p.svm).treasury_count, 1);
}

#[test]
fn cancellation_without_a_threshold_attestation_is_rejected() {
    let mut p = proposed();
    let result = send(
        &mut p.svm,
        cancel_rebalance_policy_ix(&p.authority.pubkey()),
        &p.authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::MissingSignatureVerification);
}

/// A cancel signature commits to the exact `eta` it cancels, so it cannot
/// be held and replayed against a later re-proposal.
#[test]
fn a_cancel_proof_for_a_different_eta_is_rejected() {
    let mut p = proposed();
    let eta = get_pending_rebalance_policy(&p.svm).eta;
    let message = cancel_rebalance_policy_message(0, eta + 1);
    let result = send_ixs(
        &mut p.svm,
        &[
            ed25519_proof_ix(&[&p.signers[0], &p.signers[1]], &message),
            cancel_rebalance_policy_ix(&p.authority.pubkey()),
        ],
        &p.authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
}

// ------------------------------------------------ admin cannot reach it --

/// The summary property, stated as one test: with the admin key and
/// nothing else, none of the four policy instructions does anything.
#[test]
fn the_admin_key_alone_can_neither_create_change_nor_cancel_a_policy() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    let attacker_treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    // Cannot re-create.
    assert!(send(
        &mut svm,
        initialize_rebalance_policy_ix(
            &authority.pubkey(),
            &mint,
            vec![attacker_treasury],
            ROLLING_LIMIT,
            WINDOW_SECONDS,
        ),
        &authority,
        &[],
    )
    .is_err());

    // Cannot propose a change.
    assert!(send(
        &mut svm,
        propose_rebalance_policy_ix(
            &authority.pubkey(),
            &mint,
            vec![attacker_treasury],
            ROLLING_LIMIT,
            WINDOW_SECONDS,
        ),
        &authority,
        &[],
    )
    .is_err());

    // And cannot cancel a legitimate one.
    let message = propose_rebalance_policy_message(0, &[treasury], ROLLING_LIMIT, WINDOW_SECONDS);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            propose_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    )
    .expect("a threshold-approved proposal succeeds");
    assert!(send(
        &mut svm,
        cancel_rebalance_policy_ix(&authority.pubkey()),
        &authority,
        &[],
    )
    .is_err());

    let policy = get_rebalance_policy(&svm);
    assert_eq!(policy.treasury_count, 1);
    assert!(!policy.is_allowlisted(&attacker_treasury));
}

/// Policy governance is deliberately reachable while the bridge is
/// unpaused: it moves no funds, and requiring a pause would mean an
/// operator had to halt settlement to correct a treasury address.
#[test]
fn policy_governance_works_while_the_bridge_is_running() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    send(
        &mut svm,
        set_paused_ix(&authority.pubkey(), PauseScope::Global, false),
        &authority,
        &[],
    )
    .expect("unpause");

    let second = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);
    let message =
        propose_rebalance_policy_message(0, &[treasury, second], ROLLING_LIMIT, WINDOW_SECONDS);
    send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &message),
            propose_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury, second],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    )
    .expect("policy governance does not require a pause");
}

// -------------------------------------- cross-action governance replay --
//
// Each policy action signs under its own action byte
// (`ACTION_INITIALIZE_REBALANCE_POLICY` 0x09,
// `ACTION_PROPOSE_REBALANCE_POLICY` 0x07,
// `ACTION_CANCEL_REBALANCE_POLICY` 0x08). The byte sits inside the signed
// governance message, so an approval collected for one action is simply
// not a signature over another action's message. These tests hold that
// separation down: without it, a quorum persuaded to approve a harmless
// one-time initialization could have its signatures re-presented as an
// approval to REPLACE a live allowlist, which is a materially different
// decision.

/// An approval to CREATE the first policy must not be replayable as an
/// approval to REPLACE an existing one.
#[test]
fn an_initialization_attestation_cannot_authorize_a_policy_proposal() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    let second = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    // A genuine, current, threshold-strength attestation — over the
    // INITIALIZE message rather than the PROPOSE one.
    let init_message =
        initialize_rebalance_policy_message(0, &[treasury, second], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &init_message),
            propose_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury, second],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert!(!pending_rebalance_policy_exists(&svm));
    // The live allowlist is untouched.
    assert_eq!(get_rebalance_policy(&svm).treasury_count, 1);
}

/// And the converse: a proposal approval must not create the first policy.
#[test]
fn a_proposal_attestation_cannot_authorize_an_initialization() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let propose_message =
        propose_rebalance_policy_message(0, &[treasury], ROLLING_LIMIT, WINDOW_SECONDS);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &propose_message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert!(!rebalance_policy_exists(&svm));
}

/// A cancellation approval commits to an eta, not to a parameter set, so
/// it can authorize neither creation nor replacement.
#[test]
fn a_cancellation_attestation_cannot_authorize_a_policy_proposal() {
    let authority = Keypair::new();
    let (mut svm, signers, mint, treasury) =
        setup_paused_with_policy(&authority, RESERVE, ROLLING_LIMIT, WINDOW_SECONDS);
    let second = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    let cancel_message = cancel_rebalance_policy_message(0, 0);
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &cancel_message),
            propose_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury, second],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert!(!pending_rebalance_policy_exists(&svm));
}

/// The attestation-key epoch is inside the governance message, so an
/// approval gathered under one epoch cannot be replayed under another.
/// This is what makes a key rotation an effective revocation of every
/// governance signature still in flight.
#[test]
fn an_initialization_attestation_bound_to_another_epoch_is_rejected() {
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, RESERVE);
    let treasury = create_ata(&mut svm, &Pubkey::new_unique(), &mint, 0);

    // Genuine signatures from genuine current keys — over a message that
    // names an epoch the key set is not at.
    let wrong_epoch_message = initialize_rebalance_policy_message(
        1, // live epoch is 0
        &[treasury],
        ROLLING_LIMIT,
        WINDOW_SECONDS,
    );
    let result = send_ixs(
        &mut svm,
        &[
            ed25519_proof_ix(&[&signers[0], &signers[1]], &wrong_epoch_message),
            initialize_rebalance_policy_ix(
                &authority.pubkey(),
                &mint,
                vec![treasury],
                ROLLING_LIMIT,
                WINDOW_SECONDS,
            ),
        ],
        &authority,
        &[],
    );
    assert_bridge_error(result, BridgeError::SignatureMessageMismatch);
    assert!(!rebalance_policy_exists(&svm));
}
