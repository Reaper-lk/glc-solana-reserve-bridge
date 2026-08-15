//! Internal Goldcoin vault signing client: each custody-domain signer
//! independently re-derives a payout plan from its own view of chain/DB
//! state before contributing a partial signature — never signs a plan or
//! sighash simply because the orchestrator asserts it's correct.
//!
//! # DEV/TEST KEY POSTURE ONLY
//!
//! Per the approved trust model (docs/02-trust-model.md, docs/12-management-
//! decisions.md item 1: 2-of-3 internal threshold custody, HSM/KMS-backed
//! in production, **not** federation), production signing keys must live in
//! genuinely separate HSM/KMS custody domains. [`DevVaultSigner`] holds a
//! plain in-memory `libsecp256k1::SecretKey` — this is a **non-production
//! stand-in** for local development and testing only (see
//! IMPLEMENTATION_LOG.md). No production key material, HSM integration, or
//! real custody-domain separation is implemented in this phase; building
//! that is a distinct, later, explicitly-approved piece of work
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

use thiserror::Error;

use crate::amount_conversion::{self, ConversionError};
use crate::goldcoin::address::Network;
use crate::goldcoin::coin::{self, VaultUtxo};
use crate::goldcoin::multisig::PartialSignature;
use crate::goldcoin::payout::{self, PayoutPlan};
use crate::goldcoin::tx::Transaction;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{Direction, Ledger, LedgerError, RequestState};

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

    fn sign_sighash(&self, sighash: &[u8; 32]) -> Vec<u8> {
        let msg = libsecp256k1::Message::parse(sighash);
        let (sig, _) = libsecp256k1::sign(&msg, &self.secret_key);
        sig.serialize_der().as_ref().to_vec()
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
        fee_rate_per_kb: u64,
        dust_threshold: u64,
        max_inputs: usize,
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
        fee_rate_per_kb: u64,
        dust_threshold: u64,
        max_inputs: usize,
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
            request.fee_amount_atomic,
            request.net_amount_atomic,
        )
        .map_err(|e| SigningError::Conversion(request_id, e))?;
        let payout_atomic = fee_breakdown.net.0;

        let candidates: Vec<VaultUtxo> = self.ledger.available_vault_utxos()?;
        let selection = coin::select(
            &candidates,
            payout_atomic,
            fee_rate_per_kb,
            vault.threshold,
            vault.redeem_script().len(),
            max_inputs,
        )?;
        let (change_atomic, fee_atomic) = coin::finalize(&selection, payout_atomic, dust_threshold);

        let plan = PayoutPlan {
            inputs: selection.selected,
            dest_p2pkh_hash,
            payout_atomic,
            change_atomic,
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
#[allow(clippy::too_many_arguments)]
pub fn independently_sign(
    signer: &DevVaultSigner,
    vault: &MultisigVault,
    source: &dyn IndependentPayoutSource,
    request_id: i64,
    input_index: usize,
    fee_rate_per_kb: u64,
    dust_threshold: u64,
    max_inputs: usize,
    network: Network,
) -> Result<(PartialSignature, PayoutPlan, Transaction), SigningError> {
    let plan = source.rederive_plan(
        request_id,
        vault,
        fee_rate_per_kb,
        dust_threshold,
        max_inputs,
        network,
    )?;
    let unsigned_tx = payout::build_unsigned_tx(&plan);
    payout::verify_payout_tx(&unsigned_tx, &plan)?;
    let sighash = unsigned_tx.sighash_all(input_index, &vault.redeem_script());
    let der = signer.sign_sighash(&sighash);
    Ok((
        PartialSignature {
            vault_pubkey: signer.pubkey,
            der_signature: der,
        },
        plan,
        unsigned_tx,
    ))
}

#[cfg(test)]
mod tests;
