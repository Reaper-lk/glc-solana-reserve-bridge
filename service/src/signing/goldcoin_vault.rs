//! Internal Goldcoin vault signing client: each custody-domain signer
//! independently re-derives a payout plan from its own view of chain/DB
//! state before contributing a partial signature — never signs a plan or
//! sighash simply because the orchestrator asserts it's correct.
//!
//! # Signing is behind a trait — `DevVaultSigner` is one implementation
//!
//! [`independently_sign`] takes `&dyn `[`crate::signing::signers::VaultSigner`]
//! — never a concrete signer type — so a real HSM/KMS-backed implementation
//! is a drop-in replacement, not a change to this settlement logic. Per the
//! approved trust model (docs/02-trust-model.md, docs/12-management-
//! decisions.md item 1: 2-of-3 internal threshold custody, HSM/KMS-backed
//! in production, **not** federation), production signing keys must live in
//! genuinely separate HSM/KMS custody domains. [`DevVaultSigner`] holds a
//! plain in-memory `libsecp256k1::SecretKey` — this is a **non-production
//! stand-in** for local development and testing only (see
//! IMPLEMENTATION_LOG.md), used strictly in that role from here on. No real
//! HSM/KMS backend is implemented in this phase; building one against the
//! trait is a distinct, later, explicitly-approved piece of work
//! (docs/12-management-decisions.md item 2).
//!
//! # Independent re-derivation, not shared trust
//!
//! [`IndependentPayoutSource`] is the abstraction that makes "never trust
//! the requester" concrete: a signer is handed only a `request_id` and its
//! OWN data source, and must reconstruct the entire payout plan — amount,
//! destination, UTXO selection — itself. It is structurally incapable of
//! being handed a pre-built plan to blindly sign. [`DevLedgerPayoutSource`]
//! wraps the same [`crate::ledger::Ledger`] every signer in this dev
//! harness shares, which is an honest simplification (true production
//! custody domains would each have their own independent Goldcoin RPC
//! connection and, ideally, their own replica of relevant chain state) —
//! documented so it is never mistaken for the production posture.

use std::time::Duration;

use thiserror::Error;

use crate::amount_conversion::{self, ConversionError};
use crate::goldcoin::address::Network;
use crate::goldcoin::coin::{self, VaultUtxo};
use crate::goldcoin::derivation::{self, DerivationError};
use crate::goldcoin::multisig::{self, PartialSignature};
use crate::goldcoin::payout::{self, PayoutInputContext, PayoutPlan, PayoutPolicy};
use crate::goldcoin::tx::Transaction;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{Direction, Ledger, LedgerError, RequestState};
use crate::signing::signers::{BoxFut, DerivedSignature, SignerError, VaultSigner};

#[derive(Debug, Error)]
pub enum SigningError {
    #[error("bridge request {0} not found")]
    RequestNotFound(i64),
    #[error("bridge request {0} is not in SourceFinalized state (found {1:?}) — refusing to sign")]
    NotSourceFinalized(i64, RequestState),
    #[error("bridge request {0} is not a SolToGlc request")]
    WrongDirection(i64),
    #[error("destination address is not a valid Goldcoin P2PKH address")]
    InvalidDestination(#[from] crate::goldcoin::address::AddressError),
    #[error("coin selection failed: {0}")]
    CoinSelection(#[from] coin::CoinSelectionError),
    #[error("re-derived payout plan fails its own conservation check (should never happen)")]
    PlanVerificationFailed(#[from] payout::PayoutVerifyError),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("bridge request {0}'s amount cannot be converted to a Goldcoin payout amount: {1}")]
    Conversion(i64, ConversionError),
    #[error("vault signer error: {0}")]
    Signer(#[from] SignerError),
    #[error("could not derive the request-specific vault/key: {0}")]
    Derivation(#[from] DerivationError),
    /// A selected `vault_utxos` row's scriptPubKey is neither the root
    /// vault's nor resolvable to any known GLC->SOL request's derived
    /// deposit script — should never happen, since every row synced into
    /// `vault_utxos` comes from an address this service itself watches
    /// (`Orchestrator::watched_goldcoin_addresses`). Fails closed rather
    /// than guessing which vault controls the input.
    #[error("vault UTXO scriptPubKey {0} does not match the root vault or any known request deposit address")]
    UnknownVaultUtxoScript(String),
    /// [`crate::goldcoin::payout_recovery::RecoveryPayoutSource`] only
    /// recovers a payout that has already reached `SettlementAuthorized`
    /// (the state `Ledger::record_goldcoin_payout_signed` leaves a request
    /// in) — a request still `SourceFinalized` has no existing signed
    /// payout to recover, and belongs to the normal build path instead.
    #[error("bridge request {0} is not in SettlementAuthorized state (found {1:?}) — refusing to recover")]
    NotSettlementAuthorized(i64, RequestState),
    /// No `goldcoin_payouts` row exists at all, or it exists but is not in
    /// `Signed` state (`Built` never finished signing; `Broadcast`/
    /// `Confirmed`/`Completed` already succeeded) — recovery only ever
    /// acts on a payout genuinely stuck after signing.
    #[error("request {0}'s Goldcoin payout is not in a recoverable Signed state (found {1})")]
    PayoutNotRecoverable(i64, String),
    /// A field independently recomputed from current ledger/request state
    /// disagrees with what was persisted at the time the payout was
    /// originally built — recovery refuses rather than proceeding on a
    /// payout record that may have been tampered with or has otherwise
    /// drifted from the facts it should still match exactly.
    #[error("request {request_id}'s persisted payout {field} does not match independently recomputed data — refusing to recover")]
    PayoutFieldMismatch {
        request_id: i64,
        field: &'static str,
    },
    /// The plan reconstructed from persisted `goldcoin_payouts`/
    /// `goldcoin_payout_inputs` rows does not serialize to byte-for-byte
    /// the same unsigned transaction that was originally built and
    /// persisted — the strongest possible check that recovery is signing
    /// the exact same transaction, not a subtly different one.
    #[error("request {0}'s reconstructed unsigned transaction does not match the originally persisted one — refusing to recover")]
    ReconstructedTxMismatch(i64),
}

pub struct DevVaultSigner {
    pub secret_key: libsecp256k1::SecretKey,
    pub pubkey: [u8; 33],
}

impl DevVaultSigner {
    /// Generates a fresh, random, NON-PRODUCTION keypair. Never used for
    /// anything but local dev/test — see module docs.
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        let secret_key = libsecp256k1::SecretKey::random(&mut rng);
        let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
        DevVaultSigner { secret_key, pubkey }
    }
}

impl VaultSigner for DevVaultSigner {
    fn public_key(&self) -> [u8; 33] {
        self.pubkey
    }

    fn sign_sighash<'a>(
        &'a self,
        sighash: &'a [u8; 32],
    ) -> BoxFut<'a, Result<Vec<u8>, SignerError>> {
        Box::pin(async move { Ok(multisig::sign_low_s(sighash, &self.secret_key)) })
    }

    /// Computes `derive_request_seckey(&self.secret_key, request_id)`
    /// fresh on every call — the derived key exists only for the
    /// duration of this call, never stored, never returned to the
    /// caller. This is the one real implementation of per-request
    /// derived signing; see the trait default's docs for why every
    /// other `VaultSigner` implementation fails closed instead.
    fn sign_derived<'a>(
        &'a self,
        request_id: i64,
        sighash: &'a [u8; 32],
    ) -> BoxFut<'a, Result<DerivedSignature, SignerError>> {
        Box::pin(async move {
            let identity = crate::goldcoin::hex::encode(&self.pubkey);
            let derived_sk = derivation::derive_request_seckey(&self.secret_key, request_id)
                .map_err(|e| SignerError::Rejected {
                    identity: identity.clone(),
                    detail: e.to_string(),
                })?;
            let derived_pk =
                libsecp256k1::PublicKey::from_secret_key(&derived_sk).serialize_compressed();
            Ok((derived_pk, multisig::sign_low_s(sighash, &derived_sk)))
        })
    }
}

/// What a signer re-derives a payout plan FROM — its own data source, never
/// a plan handed to it directly.
pub trait IndependentPayoutSource {
    /// `request.gross_amount_atomic` for a SolToGlc request is already
    /// canonical (the Solana indexer converts the real on-chain
    /// `WithdrawalObligation`'s raw amount to canonical units at fold
    /// time, docs/20-bridge-fee.md) — canonical is numerically Goldcoin-
    /// native, so no further chain-decimals conversion is needed here.
    /// This function still independently recomputes the fee/net breakdown
    /// from that stored gross (never trusting the stored fee/net columns
    /// directly) via `amount_conversion::verify_fee_breakdown`, so a
    /// tampered/stale stored fee or net value is never actually used to
    /// build a real payout.
    fn rederive_plan(
        &self,
        request_id: i64,
        vault: &MultisigVault,
        policy: &PayoutPolicy,
        network: Network,
    ) -> Result<PayoutPlan, SigningError>;
}

/// Dev/test re-derivation source: reconstructs the plan from the same
/// [`Ledger`] every signer in this harness shares (module docs note this
/// simplification explicitly).
pub struct DevLedgerPayoutSource<'a> {
    pub ledger: &'a Ledger,
}

impl IndependentPayoutSource for DevLedgerPayoutSource<'_> {
    fn rederive_plan(
        &self,
        request_id: i64,
        vault: &MultisigVault,
        policy: &PayoutPolicy,
        network: Network,
    ) -> Result<PayoutPlan, SigningError> {
        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(SigningError::RequestNotFound(request_id))?;
        if request.direction != Direction::SolToGlc {
            return Err(SigningError::WrongDirection(request_id));
        }
        if request.state != RequestState::SourceFinalized {
            return Err(SigningError::NotSourceFinalized(request_id, request.state));
        }

        let dest_addr = String::from_utf8_lossy(&request.recipient)
            .trim_end_matches('\0')
            .to_string();
        let dest_p2pkh_hash = crate::goldcoin::address::decode_p2pkh(&dest_addr, network)?;

        // `request.gross_amount_atomic` is canonical (== Goldcoin-native)
        // for both directions (docs/20-bridge-fee.md); the real Goldcoin
        // payout must move the NET amount, after the bridge fee, never the
        // gross deposit. Recomputed here, never trusted from the stored
        // fee/net columns directly.
        let fee_breakdown = amount_conversion::verify_fee_breakdown(
            request.gross_amount_atomic,
            request.fee_bps,
            request.fee_amount_atomic,
            request.net_amount_atomic,
        )
        .map_err(|e| SigningError::Conversion(request_id, e))?;
        let payout_atomic = fee_breakdown.net.0;

        let candidates: Vec<VaultUtxo> = self.ledger.available_vault_utxos()?;
        let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
        let selection = coin::select(
            &candidates,
            payout_atomic,
            policy.fee_rate_per_kb,
            vault.threshold,
            vault.redeem_script().len(),
            policy.max_inputs,
        )?;
        let (change_outputs, fee_atomic) = coin::finalize_fanout(
            &selection,
            payout_atomic,
            selection.selected.len(),
            input_bytes,
            policy.fee_rate_per_kb,
            policy.dust_threshold,
            policy.change_fanout_target_atomic,
            policy.change_fanout_max_outputs,
        );

        // Resolve, for each selected input independently, exactly which
        // vault controls it: the shared root vault for a legacy
        // static-vault UTXO, or a freshly re-derived request-specific
        // vault for a per-request deposit-address UTXO — never trusted
        // from anywhere but this signer's own ledger read, and never
        // cached/persisted (`goldcoin::derivation::derive_request_vault`
        // is pure public-key math, cheap to redo every time).
        let root_script = vault.script_pubkey_hex();
        let mut input_contexts = Vec::with_capacity(selection.selected.len());
        for utxo in &selection.selected {
            if utxo.script_pubkey_hex.eq_ignore_ascii_case(&root_script) {
                input_contexts.push(PayoutInputContext {
                    vault: vault.clone(),
                    funding_request_id: None,
                });
            } else {
                let funding_request_id = self
                    .ledger
                    .find_glc_to_sol_request_by_deposit_script(&utxo.script_pubkey_hex)?
                    .ok_or_else(|| {
                        SigningError::UnknownVaultUtxoScript(utxo.script_pubkey_hex.clone())
                    })?;
                let derived_vault =
                    derivation::derive_request_vault(vault, funding_request_id, network)?;
                input_contexts.push(PayoutInputContext {
                    vault: derived_vault,
                    funding_request_id: Some(funding_request_id),
                });
            }
        }

        let plan = PayoutPlan {
            inputs: selection.selected,
            input_contexts,
            dest_p2pkh_hash,
            payout_atomic,
            change_outputs,
            vault_script_pubkey: vault.script_pubkey(),
            fee_atomic,
        };
        Ok(plan)
    }
}

/// Independently re-derives the plan, builds the unsigned transaction,
/// verifies it against itself (defense in depth — should always pass since
/// both were just derived together, but catches a logic bug rather than
/// silently signing on top of one), and signs the given input. This is the
/// one function a vault signer calls; it never accepts a pre-built plan or
/// transaction as input.
///
/// `signer` is a trait object (`dyn VaultSigner`) deliberately — this
/// function never names `DevVaultSigner` or any other concrete signer
/// type, so a real HSM/KMS-backed implementation is a drop-in `Box<dyn
/// VaultSigner>` later, not a change to this settlement logic
/// (docs/22-production-readiness-review.md). `signer_timeout` bounds the
/// signing call itself (not the independent re-derivation above it) —
/// applied here, not left to the implementation alone, as defense in
/// depth against a hanging or misbehaving signer (see
/// `signing::signers` module docs).
#[allow(clippy::too_many_arguments)]
pub async fn independently_sign(
    signer: &dyn VaultSigner,
    vault: &MultisigVault,
    source: &dyn IndependentPayoutSource,
    request_id: i64,
    input_index: usize,
    policy: &PayoutPolicy,
    network: Network,
    signer_timeout: Duration,
) -> Result<(PartialSignature, PayoutPlan, Transaction), SigningError> {
    let plan = source.rederive_plan(request_id, vault, policy, network)?;
    let unsigned_tx = payout::build_unsigned_tx(&plan);
    payout::verify_payout_tx(&unsigned_tx, &plan)?;
    let ctx = &plan.input_contexts[input_index];
    let sighash = unsigned_tx.sighash_all(input_index, &ctx.vault.redeem_script());
    // Audit/timeout identity is always the signer's OWN root identity —
    // stable regardless of which (possibly derived) key ends up signing
    // this particular input; `log_signature_grant` callers use the same
    // convention (which custody domain cooperated, not which mechanical
    // scriptPubKey it happened to sign for).
    let identity = crate::goldcoin::hex::encode(&signer.public_key());
    let (vault_pubkey, der) = match ctx.funding_request_id {
        None => {
            let vault_pubkey = signer.public_key();
            let der =
                match tokio::time::timeout(signer_timeout, signer.sign_sighash(&sighash)).await {
                    Ok(Ok(der)) => der,
                    Ok(Err(e)) => return Err(SigningError::Signer(e)),
                    Err(_) => {
                        return Err(SigningError::Signer(SignerError::Timeout {
                            identity,
                            millis: signer_timeout.as_millis() as u64,
                        }))
                    }
                };
            (vault_pubkey, der)
        }
        Some(funding_request_id) => {
            match tokio::time::timeout(
                signer_timeout,
                signer.sign_derived(funding_request_id, &sighash),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => return Err(SigningError::Signer(e)),
                Err(_) => {
                    return Err(SigningError::Signer(SignerError::Timeout {
                        identity,
                        millis: signer_timeout.as_millis() as u64,
                    }))
                }
            }
        }
    };
    Ok((
        PartialSignature {
            vault_pubkey,
            der_signature: der,
        },
        plan,
        unsigned_tx,
    ))
}

/// Drives [`independently_sign`] across every configured signer and every
/// input of whatever plan `source` re-derives, collecting `threshold`
/// partials per input. Shared by [`crate::orchestrator::Orchestrator::
/// build_and_broadcast_payout`] (building a brand-new payout, `source` =
/// [`DevLedgerPayoutSource`]) and [`crate::goldcoin::payout_recovery::
/// recover_stuck_goldcoin_payout`] (re-signing an existing one stuck after
/// broadcast, `source` = `RecoveryPayoutSource`) — the exact same
/// independent per-signer, per-input signing sequence either way, so a
/// recovery signature is never produced by a weaker or different path
/// than a normal one.
///
/// `vault_signers[0]` signs every input first (establishing `plan`/`tx`,
/// which every other signer's call re-derives and must agree with —
/// [`independently_sign`]'s own re-verification catches any disagreement
/// input by input); `vault_signers[1..threshold]` then each sign every
/// input in turn. Only the first `threshold` of however many signers are
/// configured are ever asked, matching this service's existing posture
/// that a payout requires exactly `threshold` independent signatures, not
/// "whichever `threshold` happen to answer first."
#[allow(clippy::too_many_arguments)]
pub async fn independently_sign_all_inputs(
    vault_signers: &[Box<dyn VaultSigner>],
    vault: &MultisigVault,
    source: &dyn IndependentPayoutSource,
    request_id: i64,
    threshold: usize,
    policy: &PayoutPolicy,
    network: Network,
    signer_timeout: Duration,
) -> Result<(PayoutPlan, Transaction, Vec<Vec<PartialSignature>>), SigningError> {
    let (first_partial, plan, tx) = independently_sign(
        vault_signers[0].as_ref(),
        vault,
        source,
        request_id,
        0,
        policy,
        network,
        signer_timeout,
    )
    .await?;
    let mut partials: Vec<Vec<PartialSignature>> = vec![vec![first_partial]];
    for input_index in 1..plan.inputs.len() {
        let (partial, _, _) = independently_sign(
            vault_signers[0].as_ref(),
            vault,
            source,
            request_id,
            input_index,
            policy,
            network,
            signer_timeout,
        )
        .await?;
        partials.push(vec![partial]);
    }
    for signer in &vault_signers[1..threshold] {
        for (input_index, slot) in partials.iter_mut().enumerate() {
            let (partial, _, _) = independently_sign(
                signer.as_ref(),
                vault,
                source,
                request_id,
                input_index,
                policy,
                network,
                signer_timeout,
            )
            .await?;
            slot.push(partial);
        }
    }
    Ok((plan, tx, partials))
}

#[cfg(test)]
mod tests;
