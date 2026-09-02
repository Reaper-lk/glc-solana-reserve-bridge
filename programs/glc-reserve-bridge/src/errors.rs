//! Program error taxonomy.

use anchor_lang::prelude::*;

#[error_code]
pub enum BridgeError {
    // ---- Attestation key set ----
    #[msg("Attestation key set must contain at least one key")]
    EmptyAttestationKeySet,
    #[msg("Attestation key set exceeds MAX_ATTESTATION_KEYS")]
    TooManyAttestationKeys,
    #[msg("Attestation key set contains a duplicate key")]
    DuplicateAttestationKey,
    #[msg("Attestation key is the all-zero (default) pubkey, which cannot sign")]
    InvalidAttestationKey,
    #[msg("Signature threshold must be at least one")]
    ZeroThreshold,
    #[msg("Signature threshold exceeds the number of attestation keys")]
    ThresholdExceedsKeyCount,
    #[msg(
        "A single-key threshold defeats the approved trust model (docs/02-trust-model.md): \
         no single key may be capable of releasing reserves"
    )]
    ThresholdBelowMinimum,

    // ---- Admin / initialization ----
    #[msg("Signer is not the program upgrade authority")]
    UnauthorizedInitializer,
    #[msg("Signer is not the bridge admin")]
    UnauthorizedAdmin,
    #[msg("Pause state is already set to the requested value")]
    PauseStateUnchanged,
    #[msg("New admin is the same as the current admin")]
    AdminUnchanged,
    #[msg("No admin transfer is pending")]
    NoPendingAdmin,
    #[msg("Signer is not the pending admin")]
    PendingAdminMismatch,
    #[msg("Arithmetic overflow")]
    ArithmeticOverflow,
    #[msg("Arithmetic underflow")]
    ArithmeticUnderflow,

    // ---- Reserve vault ----
    #[msg("Reserve vault has already been configured")]
    ReserveAlreadyConfigured,
    #[msg("Reserve vault has not been configured yet")]
    ReserveNotConfigured,
    #[msg("Token account does not match the configured reserve mint")]
    WrongReserveMint,
    #[msg("Bridge is paused (global emergency pause)")]
    BridgeGloballyPaused,
    #[msg("Goldcoin -> Solana release direction is paused")]
    ReleaseDirectionPaused,
    #[msg("Solana -> Goldcoin deposit direction is paused")]
    DepositDirectionPaused,
    #[msg("Token program does not match the program the reserve mint was configured under")]
    WrongTokenProgram,
    #[msg("Mint or token account carries a Token-2022 extension that is not explicitly supported")]
    UnsupportedTokenExtension,
    #[msg("Mint or token account data could not be parsed as valid Token/Token-2022 state")]
    UnreadableTokenState,

    // ---- Attestation verification ----
    #[msg("Attestation epoch does not match the current attestation-key epoch")]
    StaleAttestationEpoch,
    #[msg("Expected an ed25519 signature-verification instruction immediately before this one")]
    MissingSignatureVerification,
    #[msg("Ed25519 verification instruction data is malformed or references other instructions")]
    MalformedSignatureVerification,
    #[msg("A signature covers bytes other than the expected claim message")]
    SignatureMessageMismatch,
    #[msg("A signature was produced by a key that is not a current attestation key")]
    UnknownAttestationSigner,
    #[msg("The same attestation key signed more than once")]
    DuplicateAttestationSignature,
    #[msg("Fewer unique attestation signatures than the required threshold")]
    InsufficientSignatures,

    // ---- Deposit / release amounts ----
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Amount is below the configured minimum transfer amount")]
    BelowMinimumTransfer,
    #[msg("Amount exceeds the configured per-transfer limit")]
    ExceedsPerTransferLimit,
    #[msg("Amount would exceed the configured rolling volume limit for this direction")]
    ExceedsRollingVolumeLimit,
    #[msg("Goldcoin destination address length is invalid")]
    InvalidGlcAddressLength,

    // ---- Reserve sufficiency (constraint 6: fail closed) ----
    #[msg("Reserve balance is insufficient to cover this release once the protected minimum is preserved")]
    InsufficientReserveBalance,

    // ---- Governance (attestation-key rotation; timelocked, threshold-approved) ----
    #[msg("Governance timelock must be greater than zero seconds")]
    ZeroGovernanceTimelock,
    #[msg("Pending governance action is not of the expected type")]
    WrongGovernanceAction,
    #[msg("Governance action is still inside its timelock window")]
    GovernanceTimelockNotElapsed,
    #[msg("Pending governance action was approved under a different attestation epoch")]
    StaleGovernanceProposal,
    #[msg("A governance action is already pending; cancel it first")]
    GovernanceActionAlreadyPending,
    #[msg("No governance action is pending")]
    NoGovernanceActionPending,

    // ---- Withdrawal obligation completion ----
    #[msg("Withdrawal obligation is already completed; completion is terminal and irreversible")]
    ObligationAlreadyCompleted,
    #[msg("Withdrawal obligation is not in the Pending state and cannot be completed")]
    ObligationNotPending,
    #[msg("Obligation's reserved payout region is not empty; refusing to overwrite it")]
    PayoutRecordAlreadySet,
    #[msg("Payout txid must not be all zeroes")]
    ZeroPayoutTxid,
    #[msg("Payout block height must be greater than zero")]
    ZeroPayoutHeight,
    #[msg("Completion amount does not match the obligation's recorded amount")]
    CompletionAmountMismatch,
    #[msg("Completion destination commitment does not match the obligation's stored address")]
    CompletionDestinationMismatch,

    // ---- Rebalancing (structurally distinct from user settlements) ----
    #[msg("Rebalance amount must be greater than zero")]
    ZeroRebalanceAmount,
    #[msg("Bridge must be globally paused before a rebalance withdrawal is authorized")]
    BridgeNotPaused,
    #[msg("Rebalance withdrawal destination must not be the reserve token account itself")]
    RebalanceDestinationIsReserveItself,

    // ---- Reserve withdrawal policy (2026-09-02 hardening) ----
    #[msg(
        "rebalance_withdraw is retired: it permitted an arbitrary destination token account. \
         Use treasury_withdraw (allowlisted treasury) or refund_withdraw (depositor's own ATA)"
    )]
    RebalanceWithdrawRetired,
    #[msg(
        "the RebalancePolicy account does not exist; no treasury destination is allowlisted, \
         so no operator-initiated withdrawal is authorized"
    )]
    RebalancePolicyNotConfigured,
    #[msg("RebalancePolicy on-chain state is not internally valid; refusing to act on it")]
    InvalidRebalancePolicy,
    #[msg(
        "withdrawal destination is not an allowlisted treasury token account in the on-chain \
         RebalancePolicy"
    )]
    DestinationNotAllowlisted,
    #[msg(
        "amount would exceed the RebalancePolicy rolling withdrawal limit for the current window"
    )]
    ExceedsRebalanceRollingLimit,
    #[msg(
        "nonce is in the wrong namespace for this withdrawal class: treasury withdrawals must \
         clear the high bit, refunds must set it"
    )]
    WrongNonceNamespace,
    #[msg("treasury allowlist must contain at least one destination")]
    EmptyTreasuryAllowlist,
    #[msg("treasury allowlist exceeds MAX_TREASURY_DESTINATIONS")]
    TooManyTreasuryDestinations,
    #[msg("treasury allowlist contains a duplicate destination")]
    DuplicateTreasuryDestination,
    #[msg("treasury destination is the all-zero (default) pubkey")]
    InvalidTreasuryDestination,
    #[msg("treasury destination must not be the reserve token account itself")]
    TreasuryDestinationIsReserveItself,
    #[msg("a RebalancePolicy update is already pending; cancel it first")]
    RebalancePolicyAlreadyPending,
    #[msg("pending RebalancePolicy update is still inside its timelock window")]
    RebalancePolicyTimelockNotElapsed,
    #[msg("pending RebalancePolicy update was approved under a different attestation epoch")]
    StaleRebalancePolicyProposal,
    #[msg("refund amount does not match the withdrawal obligation's recorded amount")]
    RefundAmountMismatch,

    // ---- Program upgrade timelock (docs/12-management-decisions.md item 3) ----
    #[msg("Upgrade timelock must be greater than zero seconds")]
    ZeroUpgradeTimelock,
    #[msg("Program upgrade is still inside its timelock window")]
    UpgradeTimelockNotElapsed,
    #[msg("The buffer supplied to execute_upgrade does not match the one proposed")]
    WrongUpgradeBuffer,
    #[msg(
        "This program's upgrade_authority PDA does not yet hold the real on-chain upgrade \
         authority; call accept_upgrade_authority first"
    )]
    UpgradeAuthorityNotYetAccepted,
    #[msg("Signer is not this program's current real upgrade authority")]
    NotCurrentUpgradeAuthority,
}
