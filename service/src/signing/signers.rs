//! Production-capable signer abstractions (docs/22-production-readiness-
//! review.md P0 item "HSM/KMS-backed signer implementation").
//!
//! # What this module is, precisely
//!
//! [`VaultSigner`] (Goldcoin, secp256k1) and [`AttestationSigner`] (Solana
//! attestations, ed25519) are the two interfaces every settlement-signing
//! call site in this crate (`signing::goldcoin_vault::independently_sign`,
//! `signing::attestation::independently_attest_release`/
//! `independently_attest_completion`, and `orchestrator::Orchestrator`)
//! depends on from this point forward — never a concrete signer type.
//! [`crate::signing::goldcoin_vault::DevVaultSigner`]/
//! [`crate::signing::attestation::DevAttestationSigner`] are ONE
//! implementation of these traits (an in-memory, non-production stand-in
//! for local development and testing — see those modules' own docs) and
//! are used strictly in that role from here on; nothing in the settlement
//! path is typed against them directly anymore.
//!
//! # Why this exists now, with no real backend yet
//!
//! A real HSM/cloud-KMS-backed implementation needs a specific vendor
//! decision (docs/12-management-decisions.md item 2, still open) and real
//! infrastructure this environment does not have — building it here would
//! mean guessing at both. What CAN be done locally, and is done here, is
//! removing the structural reason a real implementation couldn't be
//! plugged in later: `Orchestrator` no longer names `DevVaultSigner`/
//! `DevAttestationSigner` anywhere in its own type signature, only these
//! traits (as `Box<dyn _>`, so a future deployment can even mix
//! implementations — e.g. one custody domain on a cloud KMS, another on a
//! hardware HSM, matching the approved trust model's "genuinely separate
//! custody domains" requirement, docs/02-trust-model.md).
//!
//! # Security shape, by construction
//!
//! - **Never exposes private key material.** Every method here accepts a
//!   fully-formed signing payload (a sighash, an already-canonical
//!   message) and returns only a signature and/or a public identity —
//!   there is no method anywhere on either trait that could leak, export,
//!   or hand back a secret key, and no way for a caller to ask for one.
//! - **Fails closed on any signer error.** Both trait methods that
//!   actually sign return `Result<_, SignerError>`; every call site
//!   propagates a signer failure as a hard error for that settlement
//!   attempt (retried next tick, per `Orchestrator`'s per-request
//!   fail-closed discipline) rather than falling back to a default,
//!   skipping the signer, or treating a missing signature as "close
//!   enough."
//! - **Timeout is a first-class outcome, not a hang.** `SignerError::Timeout`
//!   exists so a real network-backed implementation (HSM/KMS calls are
//!   inherently a network round trip) has a defined way to report "took
//!   too long," and every call site wraps the signer call in
//!   `tokio::time::timeout` using a configured bound
//!   (`OrchestratorConfig::signer_timeout`) as defense in depth even
//!   against an implementation that doesn't enforce its own timeout.

use std::future::Future;
use std::pin::Pin;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

/// Matches the `BoxFut` pattern already established in `api::ApiSource`/
/// `ops::health::ReportSource` — the standard way this crate makes an
/// async trait method usable through `dyn Trait` (native `async fn` in a
/// trait is not object-safe without this).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Uniform failure mode for any signer implementation, real or dev/test.
/// Deliberately carries only an `identity` string (a public identifier —
/// e.g. a hex-encoded pubkey — never anything secret) and a human-readable
/// detail, so a real HSM/KMS backend's specific error taxonomy (network
/// errors, IAM/policy denials, rate limiting) has an obvious place to map
/// into without this trait needing to know about any particular vendor.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SignerError {
    /// The signer (custody domain, HSM, KMS endpoint) could not be
    /// reached or is not currently able to sign — a liveness failure, not
    /// necessarily a security one. Retriable by the caller next tick.
    #[error("signer {identity} unavailable: {detail}")]
    Unavailable { identity: String, detail: String },
    /// The signer was reached but explicitly declined to sign (e.g. a
    /// policy check on the HSM/KMS side rejected the request) — distinct
    /// from `Unavailable` because a caller should not necessarily retry
    /// this identically; something about the request itself was refused.
    #[error("signer {identity} refused to sign: {detail}")]
    Rejected { identity: String, detail: String },
    /// The signer call did not complete within the configured bound. Set
    /// either by the implementation itself or by the generic
    /// `tokio::time::timeout` wrapper every call site applies as defense
    /// in depth (see module docs).
    #[error("signer {identity} timed out after {millis}ms")]
    Timeout { identity: String, millis: u64 },
}

/// One Goldcoin vault custody domain's signing capability (secp256k1,
/// P2SH multisig — `crate::goldcoin::multisig`). Implementations
/// independently derive nothing here — the caller
/// (`signing::goldcoin_vault::independently_sign`) has already
/// independently re-derived the exact payout plan and computed `sighash`
/// from it before ever reaching this trait; this trait's only job is
/// "sign this specific 32 bytes," nothing more.
pub trait VaultSigner: Send + Sync {
    /// The 33-byte compressed secp256k1 public key this signer signs
    /// with — public identity only, never the secret key. Used to
    /// populate `PartialSignature::vault_pubkey` and for audit logging.
    fn public_key(&self) -> [u8; 33];

    /// Signs `sighash` (a BIP-143-style Goldcoin/Bitcoin-fork sighash,
    /// already fully computed by the caller) and returns a DER-encoded
    /// ECDSA signature. Must not mutate, reinterpret, or partially sign
    /// anything other than exactly the 32 bytes given.
    fn sign_sighash<'a>(
        &'a self,
        sighash: &'a [u8; 32],
    ) -> BoxFut<'a, Result<Vec<u8>, SignerError>>;
}

/// One internal attestation custody domain's signing capability
/// (ed25519 — `crate::solana::ed25519`). As with [`VaultSigner`], the
/// caller (`signing::attestation::independently_attest_release`/
/// `independently_attest_completion`) has already independently
/// re-derived the canonical claim message from the ledger and a live
/// chain read before this trait is ever invoked — this trait only signs
/// the exact bytes it's given.
pub trait AttestationSigner: Send + Sync {
    /// This signer's Solana ed25519 public key — public identity only.
    /// Used both to build the ed25519-precompile proof
    /// (`solana::ed25519::build_attestation_proof`) and for audit
    /// logging (`ops`/`signature_grant_log`).
    fn pubkey(&self) -> Pubkey;

    /// Signs `message` (an already fully-assembled canonical claim —
    /// `glc_reserve_bridge_shared::claim::release_claim_message`/
    /// `goldcoin_completion_message`) and returns the ed25519 signature.
    fn sign_message<'a>(&'a self, message: &'a [u8]) -> BoxFut<'a, Result<Signature, SignerError>>;
}
