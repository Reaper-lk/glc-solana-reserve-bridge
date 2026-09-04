//! GlcToSol `ManualReview` refunds: returning a Goldcoin deposit that the
//! bridge accepted on chain but could not settle, to the wallet that sent
//! it.
//!
//! # The problem this solves
//!
//! A `GlcToSol` deposit whose observed amount does not match the amount
//! the request reserved is parked in `ManualReview` with the note
//! `deposit_amount_mismatch: expected N observed M`. The money is real and
//! sitting in the vault; the request can never settle, because settling it
//! would release the WRONG amount of SPL. Until now the only exits were to
//! leave it parked forever or to hand-build a transaction.
//!
//! # What is trusted, and what is re-derived
//!
//! Nothing about the money is taken from the database on faith. Two facts
//! decide where value goes — HOW MUCH and TO WHOM — and both are derived
//! from Goldcoin RPC and then required to agree with independent indexed
//! evidence:
//!
//! - **How much.** The refund principal is the value of the deposit output
//!   as read from the chain now, cross-checked against
//!   `bridge_requests.observed_amount_atomic` — the DURABLE witness the
//!   indexer wrote at park time, in the same transaction as the source
//!   outpoint (schema v20). A request parked before that column existed
//!   has no historic second observation, and is handled by the explicit,
//!   separately reported [`AmountWitnessMode::LegacyRpcOnly`] rather than
//!   by silently accepting a weaker proof.
//!
//!   Deliberately NOT `vault_utxos`: that table is listunspent-derived
//!   root-vault spendable inventory, and nothing imports a per-request
//!   derived P2SH into the node, so a request-specific deposit can never
//!   appear there. Requiring it made every per-request deposit
//!   unrefundable. It is never
//!   `bridge_requests.amount_atomic` (the amount the request EXPECTED —
//!   for a mismatch that number is wrong by construction), and it is never
//!   parsed out of `manual_review_note`. The note's observed figure is
//!   free text: [`crate::ledger::Ledger::manual_review_reason_prefix`]
//!   reads only the reason before the first `':'`, and this module never
//!   reads the note at all.
//!
//! - **Whose.** The deposit output must pay THIS REQUEST'S deposit
//!   script, re-derived here from the request id and the configured root
//!   pubkeys via the one canonical
//!   [`crate::goldcoin::derivation::derive_request_vault`], and compared
//!   byte for byte. `bridge_requests.deposit_address` is never an
//!   authority; the stored script column is only a consistency witness.
//!
//! - **To whom.** The destination is traced from what the depositor
//!   actually spent: fetch the deposit transaction, require exactly one
//!   input, fetch that input's previous output, and recover the P2PKH
//!   hash160 from its scriptPubKey. There is no `--destination` argument
//!   and no database column that can redirect it.
//!
//! # Fail-closed, everywhere
//!
//! Every ambiguity is a refusal, never a default: more than one input
//! (ownership is not attributable to one sender), a coinbase or
//! unreported prevout, any script that is not canonical P2PKH, an
//! unreachable RPC, a chain/index disagreement, insufficient
//! confirmations, an existing refund, or any sign that a Solana release
//! has begun.
//!
//! # Independent Solana no-release check
//!
//! A GlcToSol request settles by `release_from_reserve`, which creates a
//! `DepositClaim` PDA seeded by the deposit's own `txid‖vout`. That PDA's
//! existence is the on-chain, database-independent witness that a release
//! has happened. This module reads it directly at `finalized`, in addition
//! to the database checks, and refuses if it exists — or if the read
//! cannot be completed. A missing PDA is only meaningful if the read
//! actually succeeded.
//!
//! # Custody and signing
//!
//! Refunds spend the production vault through exactly the same path as
//! ordinary payouts: same vault script, same UTXO selection, same fee
//! policy, same 2-of-3 remote signers, same assembly and verification. No
//! new key material, no hot wallet, no reduced threshold.
//!
//! # LIMITATION: signer-side derivation is future work
//!
//! [`IndependentRefundSource`] re-derives every fact independently of the
//! ledger row — but it runs in the ORCHESTRATOR process, not inside the
//! vault signers. The current remote-signer protocol
//! (`POST /v1/sign` with `{"payload_hex": "<32-byte sighash>"}`) transmits
//! a bare digest, so a signer today cannot verify what it is signing; it
//! is a blind signature oracle by construction. This module therefore does
//! NOT provide signer-daemon independent derivation, and nothing here
//! should be read as claiming it does.
//!
//! What it does provide is two independent derivations that must agree
//! (Goldcoin chain, and the indexed database) plus a full re-validation of
//! the assembled transaction before signing and again before broadcast —
//! strictly more verification than the ordinary payout path performs.
//!
//! [`RefundClaim`] is shaped deliberately as the payload a future
//! `sign_refund(claim, input_index)` endpoint would carry, and
//! [`IndependentRefundSource::derive`] is the logic that would move into
//! the signer daemon behind it. See docs/09-runbook.md.

use crate::goldcoin::address::{self, Network};
use crate::goldcoin::coin::{self, VaultUtxo};
use crate::goldcoin::payout::{self, PayoutInputContext, PayoutPlan, PayoutPolicy};
use crate::goldcoin::rpc::RpcError;
use crate::goldcoin::tx::Transaction;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{GlcRefundDbChecks, GoldcoinRefundRow, GoldcoinRefundState, Ledger};

/// The Goldcoin RPC surface a refund needs. Deliberately narrow: two
/// read methods and a broadcast. Narrow enough that the future signer
/// daemon can implement exactly this to perform the same trace.
pub trait RefundRpc {
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> impl std::future::Future<Output = Result<crate::goldcoin::rpc::DecodedTransaction, RpcError>>
           + Send;
    fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> impl std::future::Future<Output = Result<crate::goldcoin::rpc::BroadcastOutcome, RpcError>> + Send;
}

/// Everything a verifier needs to check a refund without trusting the
/// party that proposed it.
///
/// This is the future signer-protocol payload. A `sign_refund(claim,
/// input_index)` endpoint would receive exactly this, re-run
/// [`IndependentRefundSource::derive`] against its OWN Goldcoin RPC, and
/// refuse unless every field matched what it derived itself. Today the
/// orchestrator performs that comparison in-process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundClaim {
    pub request_id: i64,
    /// The deposit outpoint being returned.
    pub source_txid: [u8; 32],
    pub source_vout: u32,
    /// The principal, in Goldcoin atomic units, as independently observed
    /// on chain.
    pub observed_amount_atomic: u64,
    /// The outpoint the deposit transaction spent — the evidence the
    /// destination was derived from.
    pub source_input_txid: [u8; 32],
    pub source_input_vout: u32,
    /// The destination, derived from that prevout's scriptPubKey.
    pub refund_dest_p2pkh_hash: [u8; 20],
    /// The vault outpoints funding the refund, in construction order.
    pub input_outpoints: Vec<([u8; 32], u32)>,
    pub fee_atomic: u64,
}

/// Why a refund was refused. Every variant is a hard stop; there is no
/// override.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefundError {
    #[error("REFUSING — {0}")]
    Refused(String),
    #[error("REFUSING — Goldcoin RPC could not be read ({0}); an unreadable chain is a refusal, never an assumption")]
    Rpc(String),
    #[error("REFUSING — ledger error: {0}")]
    Ledger(String),
}

impl RefundError {
    fn refuse(msg: impl Into<String>) -> Self {
        RefundError::Refused(msg.into())
    }
}

impl From<crate::ledger::LedgerError> for RefundError {
    fn from(e: crate::ledger::LedgerError) -> Self {
        RefundError::Ledger(e.to_string())
    }
}

/// The independently derived, chain-proven facts about a deposit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedSource {
    pub source_txid: [u8; 32],
    pub source_vout: u32,
    /// Value of the deposit output, from the chain.
    pub observed_amount_atomic: u64,
    /// The deposit output's scriptPubKey, proven to be the vault's.
    pub deposit_script_hex: String,
    pub confirmations: i64,
    pub source_input_txid: [u8; 32],
    pub source_input_vout: u32,
    pub refund_dest_p2pkh_hash: [u8; 20],
    pub refund_dest_address: String,
}

/// Re-derives every money-deciding fact about a refund from Goldcoin RPC
/// alone, then requires the ledger's indexed view to agree.
///
/// Holds no ledger reference on purpose: `derive` takes only chain access
/// and the outpoint, so this whole type can move into a signer daemon
/// unchanged. The ledger cross-check is a separate, explicit step
/// ([`cross_check_indexed`]).
pub struct IndependentRefundSource<'a, R: RefundRpc> {
    pub rpc: &'a R,
    /// The scriptPubKey this request's deposit MUST pay, INDEPENDENTLY
    /// DERIVED by the caller from immutable inputs — the request id plus
    /// the configured root pubkeys, threshold and network — via the one
    /// canonical `goldcoin::derivation::derive_request_vault`, the same
    /// function request creation and payout recovery use.
    ///
    /// It is never read from `bridge_requests.deposit_address` or
    /// `deposit_script_pubkey_hex`. Those columns are compared against
    /// this value as a consistency witness (see `cross_check_indexed`),
    /// so a tampered column is a refusal rather than an authority.
    ///
    /// For a LEGACY request that predates per-request deposit addresses,
    /// the caller passes the root vault script — that regime's own
    /// binding — and the OP_RETURN request-id check applies instead.
    pub expected_deposit_script_hex: String,
    /// The ROOT vault script, used only to refuse "refunding" the vault to
    /// itself. Distinct from `expected_deposit_script_hex`: a per-request
    /// deposit address is NOT the root script, and conflating the two is
    /// exactly the defect this replaced.
    pub root_vault_script_hex: String,
    pub network: Network,
    /// Confirmations the DEPOSIT must have before it may be returned.
    pub required_confirmations: i64,
}

impl<R: RefundRpc> IndependentRefundSource<'_, R> {
    /// The full two-hop trace. Read-only; every failure is a refusal.
    pub async fn derive(
        &self,
        source_txid: [u8; 32],
        source_vout: u32,
    ) -> Result<DerivedSource, RefundError> {
        let txid_hex = crate::goldcoin::hex::encode(&source_txid);

        // ---- hop 1: the deposit transaction itself ----
        let deposit = self
            .rpc
            .get_raw_transaction(&txid_hex)
            .await
            .map_err(|e| RefundError::Rpc(e.to_string()))?;

        // The node is asked about a txid; it must answer about THAT txid.
        if deposit.txid != txid_hex {
            return Err(RefundError::refuse(format!(
                "Goldcoin RPC returned transaction {} when asked for {txid_hex}",
                deposit.txid
            )));
        }

        let confirmations = deposit.confirmations.ok_or_else(|| {
            RefundError::refuse(format!(
                "deposit {txid_hex} has no confirmation count (mempool-only); a deposit that is \
                 not in a block cannot be refunded"
            ))
        })?;
        if confirmations < self.required_confirmations {
            return Err(RefundError::refuse(format!(
                "deposit {txid_hex} has {confirmations} confirmations, below the required \
                 {}",
                self.required_confirmations
            )));
        }

        let out = deposit
            .vout
            .iter()
            .find(|v| v.n == source_vout)
            .ok_or_else(|| {
                RefundError::refuse(format!("deposit {txid_hex} has no output {source_vout}"))
            })?;

        // The output must pay THIS REQUEST'S deposit script, byte for
        // byte, where that script was derived independently from the
        // request id and config — not read from the database.
        //
        // This is the request-binding proof. An output paying some other
        // valid bridge script, or another request's derived address,
        // fails here: the script IS the request identity, so one
        // request's deposit can never authorize another's refund.
        if !out
            .script_pub_key
            .hex
            .eq_ignore_ascii_case(&self.expected_deposit_script_hex)
        {
            return Err(RefundError::refuse(format!(
                "deposit output {txid_hex}:{source_vout} pays {}, not this request's \
                 independently derived deposit script {}; it is not this request's deposit",
                out.script_pub_key.hex.to_lowercase(),
                self.expected_deposit_script_hex.to_lowercase()
            )));
        }

        let observed_amount_atomic = crate::goldcoin::deposit::glc_to_atomic_public(out.value);
        if observed_amount_atomic == 0 {
            return Err(RefundError::refuse(format!(
                "deposit output {txid_hex}:{source_vout} has zero value"
            )));
        }

        // ---- the single-input rule ----
        //
        // With one input the sender is unambiguous: whoever controlled that
        // outpoint funded this deposit. With two or more, the transaction
        // may combine outputs from different owners and there is no
        // principled way to choose which one "sent" it — refunding to any
        // of them could pay the wrong party. This conservative first
        // version refuses rather than guessing.
        if deposit.vin.len() != 1 {
            return Err(RefundError::refuse(format!(
                "deposit {txid_hex} has {} inputs; exactly one is required to attribute the \
                 sender unambiguously (this conservative first version refuses multi-input \
                 deposits rather than guessing which input to refund)",
                deposit.vin.len()
            )));
        }
        let (prev_txid_hex, prev_vout) = deposit.vin[0].prevout().ok_or_else(|| {
            RefundError::refuse(format!(
                "deposit {txid_hex}'s single input is a coinbase or has no reported outpoint; \
                 no sender can be traced"
            ))
        })?;
        let prev_txid_hex = prev_txid_hex.to_string();

        // ---- hop 2: the spent previous output ----
        let prev = self
            .rpc
            .get_raw_transaction(&prev_txid_hex)
            .await
            .map_err(|e| RefundError::Rpc(e.to_string()))?;
        if prev.txid != prev_txid_hex {
            return Err(RefundError::refuse(format!(
                "Goldcoin RPC returned transaction {} when asked for prevout parent {prev_txid_hex}",
                prev.txid
            )));
        }
        let prev_out = prev.vout.iter().find(|v| v.n == prev_vout).ok_or_else(|| {
            RefundError::refuse(format!(
                "prevout parent {prev_txid_hex} has no output {prev_vout}"
            ))
        })?;

        // Only a canonical P2PKH sender is supported. Anything else — P2SH,
        // multisig, segwit, OP_RETURN — cannot be reduced to one refund
        // address without guessing.
        let refund_dest_p2pkh_hash =
            address::p2pkh_hash_from_script_hex(&prev_out.script_pub_key.hex).map_err(|e| {
                RefundError::refuse(format!(
                    "sender output {prev_txid_hex}:{prev_vout} is not a supported address form \
                     ({e}); the refund destination cannot be established unambiguously"
                ))
            })?;
        let refund_dest_address = address::encode_p2pkh(&refund_dest_p2pkh_hash, self.network);

        // The refund must never pay the vault itself: that would burn the
        // user's deposit into bridge funds while reporting success.
        // Checked against BOTH bridge-controlled scripts: the root vault
        // and this request's own deposit address. Either would mean
        // "refunding" bridge-controlled funds back to the bridge while
        // reporting success to an operator.
        for (label, script) in [
            ("bridge vault", &self.root_vault_script_hex),
            (
                "request's own deposit address",
                &self.expected_deposit_script_hex,
            ),
        ] {
            if prev_out.script_pub_key.hex.eq_ignore_ascii_case(script) {
                return Err(RefundError::refuse(format!(
                    "the traced sender output {prev_txid_hex}:{prev_vout} pays the {label}; \
                     refusing to 'refund' bridge-controlled funds to the bridge"
                )));
            }
        }

        let source_input_txid = crate::goldcoin::hex::decode_exact::<32>(&prev_txid_hex)
            .map_err(|e| RefundError::refuse(format!("prevout txid is not 32-byte hex: {e}")))?;

        Ok(DerivedSource {
            source_txid,
            source_vout,
            observed_amount_atomic,
            deposit_script_hex: out.script_pub_key.hex.clone(),
            confirmations,
            source_input_txid,
            source_input_vout: prev_vout,
            refund_dest_p2pkh_hash,
            refund_dest_address,
        })
    }
}

/// Which independent witness backs the refund principal.
///
/// The two modes are NOT equivalent and are never reported as if they
/// were: one is a two-observation cross-check, the other is a single
/// verified read. An operator must be able to see which assurance they
/// are acting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountWitnessMode {
    /// `bridge_requests.observed_amount_atomic` (schema v20) exists: the
    /// amount the INDEXER independently decoded when it parked the
    /// request, written in the same transaction as the source outpoint.
    /// A fresh RPC read must equal it exactly — two independent
    /// observations of one fact, taken at different times by different
    /// code.
    DurableChainVsLedger,
    /// The request predates the v20 witness, so no historic second
    /// observation exists. The principal comes from the independently
    /// verified RPC output alone.
    ///
    /// This is deliberately NOT backfilled: the only historic record of
    /// the amount is `manual_review_note`'s free text, and parsing a
    /// number back out of an operator-readable message is exactly what
    /// the durable witness exists to avoid. Reconstructing it from chain
    /// history is out of scope for a migration.
    ///
    /// Every OTHER binding still applies in full — outpoint, derived
    /// script, stored-column agreement, confirmations, single input,
    /// prevout trace, no release, no prior refund, pause, 2-of-3 signing.
    /// Only the amount has one witness instead of two.
    LegacyRpcOnly,
}

impl AmountWitnessMode {
    /// The operator-facing banner. Deliberately verbose in the legacy
    /// case: a reduced assurance must read as reduced.
    pub fn describe(self) -> &'static str {
        match self {
            AmountWitnessMode::DurableChainVsLedger => "durable chain-vs-ledger",
            AmountWitnessMode::LegacyRpcOnly => {
                "legacy RPC-only — request predates observed_amount_atomic witness"
            }
        }
    }

    pub fn is_legacy(self) -> bool {
        matches!(self, AmountWitnessMode::LegacyRpcOnly)
    }
}

/// Cross-checks the chain-derived amount against the DURABLE ledger
/// witness, and cross-checks the independently derived deposit script
/// against the stored column.
///
/// # The amount
///
/// When `observed_amount_atomic` is present it must equal the fresh RPC
/// read exactly — no tolerance, in either direction. When it is absent
/// the request predates the witness and [`AmountWitnessMode::
/// LegacyRpcOnly`] is returned so the caller can report the reduced
/// assurance. The note is never consulted in either mode.
///
/// # The script
///
/// `bridge_requests.deposit_script_pubkey_hex`, WHERE PRESENT, must equal
/// the script the caller derived independently. The derivation is the
/// authority; the column is a consistency witness, so a tampered column
/// fails closed instead of redirecting a refund. A legacy row with no
/// stored script simply has nothing to compare, and the derived-script
/// match against the CHAIN still had to pass in `derive`.
pub fn cross_check_indexed(
    derived: &DerivedSource,
    checks: &GlcRefundDbChecks,
    expected_deposit_script_hex: &str,
) -> Result<AmountWitnessMode, RefundError> {
    if let Some(stored) = checks.stored_deposit_script_pubkey_hex.as_deref() {
        if !stored.eq_ignore_ascii_case(expected_deposit_script_hex) {
            return Err(RefundError::refuse(format!(
                "bridge_requests.deposit_script_pubkey_hex is {}, but this request's script \
                 derives independently to {}; refusing while the stored binding disagrees with \
                 the derivation",
                stored.to_lowercase(),
                expected_deposit_script_hex.to_lowercase()
            )));
        }
    }

    match checks.durable_observed_amount_atomic {
        Some(durable) => {
            if durable != derived.observed_amount_atomic {
                return Err(RefundError::refuse(format!(
                    "chain says the deposit was {} atomic but the durable ledger witness \
                     (bridge_requests.observed_amount_atomic) says {}; refusing while the two \
                     disagree",
                    derived.observed_amount_atomic, durable
                )));
            }
            Ok(AmountWitnessMode::DurableChainVsLedger)
        }
        None => Ok(AmountWitnessMode::LegacyRpcOnly),
    }
}

/// Builds the refund plan: the destination gets the FULL observed
/// principal, and the miner fee is additional vault expenditure.
///
/// This is the deliberate policy difference from a payout, which pays a
/// net amount. A user whose deposit could not be settled is made whole:
/// they receive exactly what they sent. The fee is the bridge's cost of
/// returning it, taken from vault change, never from the user's money.
pub fn build_refund_plan(
    derived: &DerivedSource,
    candidates: &[VaultUtxo],
    vault: &MultisigVault,
    policy: &PayoutPolicy,
) -> Result<PayoutPlan, RefundError> {
    let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
    let selection = coin::select(
        candidates,
        derived.observed_amount_atomic,
        policy.fee_rate_per_kb,
        vault.threshold,
        vault.redeem_script().len(),
        policy.max_inputs,
    )
    .map_err(|e| RefundError::refuse(format!("vault cannot fund the refund: {e}")))?;

    let (change_outputs, fee_atomic) = coin::finalize_fanout(
        &selection,
        derived.observed_amount_atomic,
        selection.selected.len(),
        input_bytes,
        policy.fee_rate_per_kb,
        policy.dust_threshold,
        policy.change_fanout_target_atomic,
        policy.change_fanout_max_outputs,
    );

    let input_contexts = selection
        .selected
        .iter()
        .map(|_| PayoutInputContext {
            vault: vault.clone(),
            funding_request_id: None,
        })
        .collect();

    Ok(PayoutPlan {
        inputs: selection.selected,
        input_contexts,
        dest_p2pkh_hash: derived.refund_dest_p2pkh_hash,
        payout_atomic: derived.observed_amount_atomic,
        change_outputs,
        vault_script_pubkey: vault.script_pubkey(),
        fee_atomic,
    })
}

/// The independent re-validation of a built transaction, run before
/// signing and again before broadcast.
///
/// Checks the assembled bytes against the independently derived facts —
/// not against the plan that produced them, which would be circular.
pub fn validate_refund_tx(
    tx: &Transaction,
    plan: &PayoutPlan,
    derived: &DerivedSource,
    vault: &MultisigVault,
    policy: &PayoutPolicy,
) -> Result<(), RefundError> {
    // Structural agreement between plan and bytes, reusing the payout
    // path's own verifier so refunds cannot drift from it.
    payout::verify_payout_tx(tx, plan).map_err(|e| {
        RefundError::refuse(format!("assembled transaction does not match plan: {e}"))
    })?;

    // Output 0 is the refund: exact principal, exact derived destination.
    let expected_script = crate::goldcoin::hex::decode_vec(&address::p2pkh_script_hex(
        &derived.refund_dest_p2pkh_hash,
    ))
    .map_err(|e| {
        RefundError::refuse(format!("could not build expected destination script: {e}"))
    })?;
    let first = tx
        .outputs
        .first()
        .ok_or_else(|| RefundError::refuse("refund transaction has no outputs"))?;
    if first.value_atomic != derived.observed_amount_atomic {
        return Err(RefundError::refuse(format!(
            "refund output is {} atomic but the independently observed deposit was {}",
            first.value_atomic, derived.observed_amount_atomic
        )));
    }
    if first.script_pubkey != expected_script {
        return Err(RefundError::refuse(
            "refund output does not pay the independently derived destination",
        ));
    }

    // Every other output must be vault change and nothing else.
    let vault_script = vault.script_pubkey();
    for (i, out) in tx.outputs.iter().enumerate().skip(1) {
        if out.script_pubkey != vault_script {
            return Err(RefundError::refuse(format!(
                "output {i} pays neither the refund destination nor the vault change script"
            )));
        }
    }

    // Inputs must be exactly the planned outpoints, in order.
    if tx.inputs.len() != plan.inputs.len() {
        return Err(RefundError::refuse(format!(
            "transaction spends {} inputs but the plan reserved {}",
            tx.inputs.len(),
            plan.inputs.len()
        )));
    }
    for (i, (txin, planned)) in tx.inputs.iter().zip(plan.inputs.iter()).enumerate() {
        if txin.prev_txid != planned.txid || txin.prev_vout != planned.vout {
            return Err(RefundError::refuse(format!(
                "input {i} is not the reserved outpoint"
            )));
        }
    }

    // Fee must be within policy. An excessive fee is vault value leaving
    // to miners with no ceiling, so it is bounded explicitly.
    let total_in: u64 = plan.inputs.iter().map(|u| u.amount_atomic).sum();
    let total_out: u64 = tx.outputs.iter().map(|o| o.value_atomic).sum();
    let actual_fee = total_in.saturating_sub(total_out);
    if actual_fee != plan.fee_atomic {
        return Err(RefundError::refuse(format!(
            "actual fee {actual_fee} does not equal the planned fee {}",
            plan.fee_atomic
        )));
    }
    let max_fee = max_reasonable_fee(tx, plan, policy);
    if actual_fee > max_fee {
        return Err(RefundError::refuse(format!(
            "fee {actual_fee} exceeds the policy ceiling {max_fee} for a transaction of this \
             size; refusing rather than paying an unreasonable miner fee out of the vault"
        )));
    }
    Ok(())
}

/// The fee ceiling: the configured rate applied to the real serialized
/// size, with a generous multiplier for rounding and size estimation.
/// Bounds a pathological fee without second-guessing normal policy.
fn max_reasonable_fee(tx: &Transaction, plan: &PayoutPlan, policy: &PayoutPolicy) -> u64 {
    let size_bytes = tx.serialize().len() as u64;
    let nominal = policy
        .fee_rate_per_kb
        .saturating_mul(size_bytes.max(1))
        .div_ceil(1000);
    // Three times the nominal rate, or the planned fee, whichever is
    // larger — the planned fee came from the shared payout fee math, so it
    // is by definition policy-conformant.
    nominal.saturating_mul(3).max(plan.fee_atomic)
}

/// A single named safety check, for the dry-run report.
#[derive(Debug, Clone)]
pub struct CheckLine {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// The full dry-run report. Strictly read-only to produce.
#[derive(Debug)]
pub struct RefundDryRunReport {
    pub request_id: i64,
    pub expected_gross_atomic: u64,
    pub source_txid: Option<[u8; 32]>,
    pub source_vout: Option<u32>,
    pub db: GlcRefundDbChecks,
    pub derived: Option<DerivedSource>,
    pub solana_release_absent: Option<bool>,
    pub solana_check_detail: String,
    pub existing_refund: Option<GoldcoinRefundRow>,
    pub plan: Option<PayoutPlan>,
    pub goldcoin_reserve_paused: bool,
    /// Which witness backs the principal. `None` when the trace never got
    /// far enough to establish one.
    pub amount_witness_mode: Option<AmountWitnessMode>,
    /// The scriptPubKey this request's deposit had to pay, as derived
    /// here from the request id and config — reported so an operator can
    /// compare it against the chain themselves.
    pub expected_deposit_script_hex: String,
    pub checks: Vec<CheckLine>,
    pub would_refund: bool,
}

impl RefundDryRunReport {
    pub fn refund_amount_atomic(&self) -> Option<u64> {
        self.derived.as_ref().map(|d| d.observed_amount_atomic)
    }
    pub fn fee_atomic(&self) -> Option<u64> {
        self.plan.as_ref().map(|p| p.fee_atomic)
    }
    /// Total vault outflow: what the user gets plus what miners take.
    pub fn vault_outflow_atomic(&self) -> Option<u64> {
        match (self.refund_amount_atomic(), self.fee_atomic()) {
            (Some(a), Some(f)) => Some(a + f),
            _ => None,
        }
    }
}

/// Formats atomic Goldcoin units as a decimal GLC string (8 decimals).
pub fn format_glc(atomic: u64) -> String {
    format!("{}.{:08}", atomic / 100_000_000, atomic % 100_000_000)
}

/// True when the request's state means a Solana release has begun.
pub fn state_implies_release_started(state: crate::ledger::RequestState) -> bool {
    use crate::ledger::RequestState as S;
    matches!(
        state,
        S::SettlementAuthorized
            | S::DestinationSubmitted
            | S::DestinationConfirmed
            | S::Settled
            | S::DestinationSubmissionFailed
    )
}

/// The Solana read a refund needs: does the `DepositClaim` PDA for this
/// deposit exist? Narrow on purpose — a refund never writes to Solana.
pub trait ReleaseWitnessRpc {
    fn account_exists(
        &self,
        pubkey: &solana_sdk::pubkey::Pubkey,
    ) -> impl std::future::Future<Output = Result<bool, String>> + Send;
}

/// Independently proves, from Solana chain state, that no release has been
/// created for this deposit.
///
/// `release_from_reserve` creates the `DepositClaim` PDA seeded by
/// `txid‖vout` with `init`, so the PDA exists if and only if a release has
/// executed for that exact deposit. Reading it is a database-independent
/// witness: even a ledger that had been tampered with, or one restored
/// from a stale backup, cannot make a released deposit look unreleased.
///
/// Fail-closed in both directions that matter: an RPC error is a refusal
/// (an unread chain proves nothing), and an existing PDA is a refusal.
pub async fn verify_no_solana_release<S: ReleaseWitnessRpc>(
    solana: &S,
    source_txid: [u8; 32],
    source_vout: u32,
) -> Result<(), RefundError> {
    let pda = crate::solana::accounts::deposit_claim_pda(&source_txid, source_vout);
    match solana.account_exists(&pda).await {
        Ok(false) => Ok(()),
        Ok(true) => Err(RefundError::refuse(format!(
            "a Solana DepositClaim already exists at {pda} for deposit {}:{source_vout} — the \
             SPL release for this deposit has already executed on chain, so returning the \
             Goldcoin would pay the user twice",
            crate::goldcoin::hex::encode(&source_txid)
        ))),
        Err(e) => Err(RefundError::Rpc(format!(
            "could not read the Solana DepositClaim PDA {pda} ({e}); refusing while it is \
             unknown whether an SPL release already exists"
        ))),
    }
}

fn check(name: &'static str, passed: bool, detail: impl Into<String>) -> CheckLine {
    CheckLine {
        name,
        passed,
        detail: detail.into(),
    }
}

/// Strictly read-only. Contacts no signer, loads no keypair, builds
/// nothing durable, writes nothing, and broadcasts nothing.
///
/// Runs BOTH halves of the verification — the database checks and the
/// independent chain derivation (Goldcoin trace plus the Solana
/// no-release witness) — and reports every one as PASS/FAIL rather than
/// stopping at the first failure, so an operator sees the whole picture.
#[allow(clippy::too_many_arguments)]
pub async fn dry_run_refund<R: RefundRpc, S: ReleaseWitnessRpc>(
    goldcoin: &R,
    solana: &S,
    ledger: &Ledger,
    request_id: i64,
    vault: &MultisigVault,
    policy: &PayoutPolicy,
    network: Network,
    required_confirmations: i64,
) -> Result<RefundDryRunReport, RefundError> {
    let db = ledger.glc_refund_db_checks(request_id)?;
    let request = ledger.get_request(request_id)?;
    let expected_gross_atomic = request.as_ref().map(|r| r.gross_amount_atomic).unwrap_or(0);
    let source = request
        .as_ref()
        .and_then(|r| r.source_txid.zip(r.source_vout));
    let existing_refund = ledger.get_goldcoin_refund(request_id)?;
    let goldcoin_reserve_paused = ledger
        .is_paused(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap_or(false);

    // THE canonical derivation — the same
    // `goldcoin::derivation::derive_request_vault` request creation
    // (`api.rs`) and payout recovery (`payout_recovery.rs`) use. There is
    // deliberately no refund-only reimplementation: one function, three
    // call sites, so they cannot drift.
    //
    // Computed from the request id plus the configured root pubkeys,
    // threshold and network — all immutable and public. No database value
    // participates, which is what makes this an independent proof of the
    // request binding rather than a restatement of what the DB claims.
    let expected_deposit_script_hex =
        crate::goldcoin::derivation::derive_request_vault(vault, request_id, network)
            .map_err(|e| {
                RefundError::refuse(format!(
                    "could not derive request {request_id}'s deposit script: {e}"
                ))
            })?
            .script_pubkey_hex();
    let mut amount_witness_mode: Option<AmountWitnessMode> = None;

    let mut checks = vec![
        check("request exists", db.request_found, ""),
        check(
            "direction is GlcToSol",
            db.direction_is_glc_to_sol,
            "only the Goldcoin leg of a GlcToSol deposit is refundable here",
        ),
        check("state is ManualReview", db.state_is_manual_review, ""),
        check(
            "manual_review reason is refundable",
            db.reason_is_refundable,
            format!("{:?}", Ledger::REFUNDABLE_GLC_MANUAL_REVIEW_REASONS),
        ),
        check("source outpoint recorded", db.has_source_outpoint, ""),
        check("no Goldcoin payout row", db.no_goldcoin_payout, ""),
        check("no destination transaction", db.no_destination_txid, ""),
        check("no settlement claim hash", db.no_settlement_claim, ""),
        check("no existing refund lifecycle", db.no_existing_refund, ""),
        // NOTE: there is deliberately no `vault_utxos` check. That table
        // is listunspent-derived spendable inventory for addresses the
        // NODE's wallet owns; nothing imports a per-request derived P2SH
        // into the node, so a request-specific deposit can never appear
        // there. The durable witness is on the request row instead.
        check(
            "durable amount witness present (schema v20)",
            // Absence is NOT a failure — it selects the legacy mode,
            // reported explicitly below.
            true,
            db.durable_observed_amount_atomic
                .map(|a| {
                    format!(
                        "ledger witness = {a} atomic ({} GLC), written at park time",
                        format_glc(a)
                    )
                })
                .unwrap_or_else(|| {
                    "absent — request predates the witness; LEGACY amount mode applies".to_string()
                }),
        ),
        check(
            "request state does not imply a release",
            request
                .as_ref()
                .map(|r| !state_implies_release_started(r.state))
                .unwrap_or(false),
            "",
        ),
    ];

    // ---- independent Goldcoin derivation ----
    let mut derived: Option<DerivedSource> = None;
    match source {
        None => checks.push(check(
            "independent Goldcoin source trace",
            false,
            "no source outpoint to trace",
        )),
        Some((txid, vout)) => {
            let src = IndependentRefundSource {
                rpc: goldcoin,
                expected_deposit_script_hex: expected_deposit_script_hex.clone(),
                root_vault_script_hex: vault.script_pubkey_hex(),
                network,
                required_confirmations,
            };
            match src.derive(txid, vout).await {
                Ok(d) => {
                    checks.push(check(
                        "independent Goldcoin source trace",
                        true,
                        format!(
                            "deposit {}:{} pays this request's derived deposit script, {} confirmations",
                            crate::goldcoin::hex::encode(&d.source_txid),
                            d.source_vout,
                            d.confirmations
                        ),
                    ));
                    checks.push(check(
                        "deposit sufficiently confirmed",
                        d.confirmations >= required_confirmations,
                        format!("{} >= {} required", d.confirmations, required_confirmations),
                    ));
                    checks.push(check(
                        "exactly one source input traced",
                        true,
                        format!(
                            "spent {}:{}",
                            crate::goldcoin::hex::encode(&d.source_input_txid),
                            d.source_input_vout
                        ),
                    ));
                    checks.push(check(
                        "refund destination derived from prevout",
                        true,
                        d.refund_dest_address.clone(),
                    ));
                    let xcheck = cross_check_indexed(&d, &db, &expected_deposit_script_hex);
                    checks.push(check(
                        "stored deposit script agrees with the derivation",
                        xcheck.is_ok(),
                        match (&xcheck, db.stored_deposit_script_pubkey_hex.as_deref()) {
                            (Err(e), _) => e.to_string(),
                            (Ok(_), Some(_)) => {
                                "stored column matches the derived script".to_string()
                            }
                            (Ok(_), None) => {
                                "no stored script (legacy row); the chain match above still \
                                 had to pass"
                                    .to_string()
                            }
                        },
                    ));
                    checks.push(check(
                        "chain amount agrees with the durable ledger witness",
                        xcheck.is_ok(),
                        match &xcheck {
                            Err(e) => e.to_string(),
                            Ok(AmountWitnessMode::DurableChainVsLedger) => format!(
                                "{} atomic ({} GLC) on both",
                                d.observed_amount_atomic,
                                format_glc(d.observed_amount_atomic)
                            ),
                            Ok(AmountWitnessMode::LegacyRpcOnly) => format!(
                                "LEGACY: no durable witness exists for this request, so the \
                                 principal rests on the verified RPC read alone ({} atomic, \
                                 {} GLC)",
                                d.observed_amount_atomic,
                                format_glc(d.observed_amount_atomic)
                            ),
                        },
                    ));
                    if let Ok(mode) = xcheck {
                        amount_witness_mode = Some(mode);
                        derived = Some(d);
                    }
                }
                Err(e) => checks.push(check(
                    "independent Goldcoin source trace",
                    false,
                    e.to_string(),
                )),
            }
        }
    }

    // ---- independent Solana no-release witness ----
    let mut solana_release_absent = None;
    let mut solana_check_detail = "not attempted (no source outpoint)".to_string();
    if let Some((txid, vout)) = source {
        match verify_no_solana_release(solana, txid, vout).await {
            Ok(()) => {
                solana_release_absent = Some(true);
                solana_check_detail = format!(
                    "DepositClaim PDA {} does not exist",
                    crate::solana::accounts::deposit_claim_pda(&txid, vout)
                );
            }
            Err(e) => {
                solana_release_absent = Some(false);
                solana_check_detail = e.to_string();
            }
        }
    }
    checks.push(check(
        "no Solana release exists (on-chain DepositClaim)",
        solana_release_absent == Some(true),
        solana_check_detail.clone(),
    ));

    // ---- plan (in-memory only; nothing reserved, nothing persisted) ----
    let mut plan = None;
    if let Some(d) = derived.as_ref() {
        let candidates = ledger.available_vault_utxos()?;
        match build_refund_plan(d, &candidates, vault, policy) {
            Ok(p) => {
                let tx = payout::build_unsigned_tx(&p);
                let valid = validate_refund_tx(&tx, &p, d, vault, policy);
                checks.push(check(
                    "vault can fund the refund",
                    true,
                    format!(
                        "{} input(s), fee {} atomic ({} GLC)",
                        p.inputs.len(),
                        p.fee_atomic,
                        format_glc(p.fee_atomic)
                    ),
                ));
                checks.push(check(
                    "transaction validates against derived facts",
                    valid.is_ok(),
                    valid
                        .as_ref()
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_default(),
                ));
                if valid.is_ok() {
                    plan = Some(p);
                }
            }
            Err(e) => checks.push(check("vault can fund the refund", false, e.to_string())),
        }
    }

    checks.push(check(
        "GoldcoinReserve paused (required for --execute only)",
        goldcoin_reserve_paused,
        if goldcoin_reserve_paused {
            "paused".to_string()
        } else {
            "not paused — a dry run does not need it; --execute will refuse until you pause \
             with: glc-admin pause --direction goldcoin"
                .to_string()
        },
    ));

    // `would_refund` deliberately ignores the pause: the pause is an
    // execution precondition an operator engages between the dry run and
    // the execute, exactly as the runbook's procedure describes.
    let would_refund = db.all_passed()
        && derived.is_some()
        && plan.is_some()
        && solana_release_absent == Some(true);

    Ok(RefundDryRunReport {
        request_id,
        expected_gross_atomic,
        source_txid: source.map(|s| s.0),
        source_vout: source.map(|s| s.1),
        db,
        derived,
        solana_release_absent,
        solana_check_detail,
        existing_refund,
        plan,
        goldcoin_reserve_paused,
        amount_witness_mode,
        expected_deposit_script_hex,
        checks,
        would_refund,
    })
}

/// What an execute actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundExecuteOutcome {
    /// Built, signed and broadcast for the first time.
    Broadcast { txid: [u8; 32] },
    /// A signed transaction already existed and was re-broadcast
    /// unchanged (crash recovery). No new transaction was constructed.
    Rebroadcast { txid: [u8; 32] },
    /// Already broadcast previously; nothing to do but wait for
    /// confirmations.
    AlreadyBroadcast { txid: [u8; 32] },
    /// Already terminal.
    AlreadyRefunded,
}

/// Executes — or safely resumes — a refund.
///
/// # Crash recovery is state-directed, never heuristic
///
/// The persisted refund state decides what happens, and the ordering of
/// writes is what makes each state recoverable:
///
/// - **no row** — nothing was ever reserved or signed. Re-verify
///   everything and build from scratch.
/// - **`Built`** — inputs are reserved and an unsigned transaction is
///   recorded, but nothing was signed, so no bytes can be in any mempool.
///   Re-verify and continue from signing, reusing the SAME reserved
///   inputs.
/// - **`Signed`** — signed bytes exist and may or may not have reached a
///   node before the crash. A replacement is NEVER built: the stored bytes
///   are re-broadcast. Same inputs and same outputs mean the same txid, so
///   a node that already has it answers `AlreadyInMempool`/`AlreadyInChain`
///   and that is treated as success, not as an error.
/// - **`Broadcast`** — the txid is recorded; only confirmations advance.
/// - **`Refunded`** — terminal, a clean no-op.
///
/// Every validation is re-run against FRESH state immediately before the
/// irreversible step, not merely at dry-run time.
#[allow(clippy::too_many_arguments)]
pub async fn execute_refund<R, S, F, Fut>(
    goldcoin: &R,
    solana: &S,
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
    vault: &MultisigVault,
    policy: &PayoutPolicy,
    network: Network,
    required_confirmations: i64,
    now: i64,
    sign: F,
) -> Result<RefundExecuteOutcome, RefundError>
where
    R: RefundRpc,
    S: ReleaseWitnessRpc,
    F: FnOnce(PayoutPlan, Transaction) -> Fut,
    // The signer callback returns the ASSEMBLED transaction, not a hex
    // string, so the bytes that will be broadcast are the same object this
    // module re-validates. Assembly is the caller's job because it is the
    // existing payout path's (`multisig::assemble` over the vault signers'
    // partials) — refunds reuse it rather than re-implementing it.
    Fut: std::future::Future<Output = Result<Transaction, String>>,
{
    // The local GoldcoinReserve pause is an execution precondition. This
    // command never engages or clears it: pausing is an operator decision
    // with its own consequences, and a command that paused on your behalf
    // would also have to decide when to unpause.
    if !ledger.is_paused(crate::ledger::ReserveDirection::GoldcoinReserve)? {
        return Err(RefundError::refuse(
            "GoldcoinReserve is not paused. Refund execution moves real vault funds and requires \
             the local Goldcoin pause first: glc-admin pause --db PATH --direction goldcoin \
             --note TEXT — and unpause explicitly afterwards; this command never pauses or \
             unpauses on its own",
        ));
    }

    let existing = ledger.get_goldcoin_refund(request_id)?;

    // ---- terminal / already-broadcast states resolve before any work ----
    if let Some(row) = existing.as_ref() {
        match row.state {
            GoldcoinRefundState::Refunded => return Ok(RefundExecuteOutcome::AlreadyRefunded),
            GoldcoinRefundState::Broadcast => {
                let txid = row.txid.ok_or_else(|| {
                    RefundError::refuse("refund is Broadcast but records no txid")
                })?;
                return Ok(RefundExecuteOutcome::AlreadyBroadcast { txid });
            }
            GoldcoinRefundState::Signed => {
                // A signed transaction may already be in a mempool. Build
                // nothing; re-broadcast exactly what was signed.
                let signed_hex = row.signed_tx_hex.clone().ok_or_else(|| {
                    RefundError::refuse("refund is Signed but records no signed transaction")
                })?;
                let txid = rebroadcast(goldcoin, &signed_hex).await?;
                ledger.record_goldcoin_refund_broadcast(request_id, txid, now)?;
                return Ok(RefundExecuteOutcome::Rebroadcast { txid });
            }
            GoldcoinRefundState::Built => { /* fall through: sign the reserved plan */ }
        }
    }

    // ---- re-verify EVERYTHING against fresh state ----
    let request = ledger
        .get_request(request_id)?
        .ok_or_else(|| RefundError::refuse(format!("bridge request {request_id} not found")))?;
    let (source_txid, source_vout) = request
        .source_txid
        .zip(request.source_vout)
        .ok_or_else(|| RefundError::refuse("request records no source outpoint"))?;

    if state_implies_release_started(request.state) {
        return Err(RefundError::refuse(format!(
            "request state {:?} indicates a Solana release has begun",
            request.state
        )));
    }

    let db = ledger.glc_refund_db_checks(request_id)?;
    // A resumed `Built` refund legitimately fails `no_existing_refund` and
    // `state_is_manual_review` (it is its own row, and already
    // RefundPending). Every other check must still hold.
    let resuming = matches!(
        existing.as_ref().map(|r| r.state),
        Some(GoldcoinRefundState::Built)
    );
    if let Some(refusal) = db.refusal.as_ref() {
        let excusable = resuming
            && (!db.no_existing_refund || !db.state_is_manual_review)
            && db.direction_is_glc_to_sol
            && db.reason_is_refundable
            && db.has_source_outpoint
            && db.no_goldcoin_payout
            && db.no_destination_txid
            && db.no_settlement_claim;
        if !excusable {
            return Err(RefundError::refuse(refusal.clone()));
        }
    }

    // Independent Goldcoin re-derivation, immediately before signing.
    //
    // The expected deposit script is re-derived HERE too, from the
    // request id and config, rather than carried over from whatever the
    // dry run computed — the same canonical
    // `derivation::derive_request_vault` used at request creation. The
    // daemon trusts nothing the CLI checked earlier, including this.
    let expected_deposit_script_hex =
        crate::goldcoin::derivation::derive_request_vault(vault, request_id, network)
            .map_err(|e| {
                RefundError::refuse(format!(
                    "could not derive request {request_id}'s deposit script: {e}"
                ))
            })?
            .script_pubkey_hex();
    let src = IndependentRefundSource {
        rpc: goldcoin,
        expected_deposit_script_hex: expected_deposit_script_hex.clone(),
        root_vault_script_hex: vault.script_pubkey_hex(),
        network,
        required_confirmations,
    };
    let derived = src.derive(source_txid, source_vout).await?;
    // Yields the witness mode; a mismatch against the durable witness, or
    // between the stored script column and the derivation, refuses here.
    let _amount_witness_mode = cross_check_indexed(&derived, &db, &expected_deposit_script_hex)?;

    // Independent Solana no-release witness, immediately before signing.
    verify_no_solana_release(solana, source_txid, source_vout).await?;

    // A resumed `Built` refund must still describe the same money.
    if let Some(row) = existing.as_ref() {
        if row.observed_amount_atomic != derived.observed_amount_atomic
            || row.refund_dest_p2pkh_hash != derived.refund_dest_p2pkh_hash
        {
            return Err(RefundError::refuse(
                "the persisted refund no longer matches what the chain now says about this \
                 deposit; refusing to sign a stale plan",
            ));
        }
    }

    // ---- build (or rebuild the identical reserved plan) ----
    let plan = if resuming {
        rebuild_reserved_plan(ledger, request_id, &derived, vault, policy)?
    } else {
        let candidates = ledger.available_vault_utxos()?;
        build_refund_plan(&derived, &candidates, vault, policy)?
    };

    let unsigned = payout::build_unsigned_tx(&plan);
    validate_refund_tx(&unsigned, &plan, &derived, vault, policy)?;

    // Persist BEFORE contacting a signer: after this commit the inputs are
    // reserved and the request is RefundPending, so a crash cannot lead to
    // a second, independently-built refund.
    if !resuming {
        ledger.begin_goldcoin_refund(
            request_id,
            derived.observed_amount_atomic,
            derived.source_input_txid,
            derived.source_input_vout,
            derived.refund_dest_p2pkh_hash,
            &derived.refund_dest_address,
            plan.fee_atomic,
            &plan.inputs,
            &crate::goldcoin::hex::encode(&unsigned.serialize()),
            note,
            actor,
            now,
        )?;
    }

    // ---- sign ----
    let signed_tx = sign(plan.clone(), unsigned.clone())
        .await
        .map_err(|e| RefundError::refuse(format!("vault signing failed: {e}")))?;

    // Re-validate the ASSEMBLED transaction against the same independently
    // derived facts before it can ever be broadcast. Signing must not be
    // able to change where the money goes: the outputs and the spent
    // outpoints are checked again here, after signatures were applied.
    validate_refund_tx(&signed_tx, &plan, &derived, vault, policy)?;

    let signed_hex = crate::goldcoin::hex::encode(&signed_tx.serialize());
    ledger.record_goldcoin_refund_signed(request_id, &signed_hex, now)?;

    // ---- broadcast ----
    let txid = rebroadcast(goldcoin, &signed_hex).await?;
    ledger.record_goldcoin_refund_broadcast(request_id, txid, now)?;
    Ok(RefundExecuteOutcome::Broadcast { txid })
}

/// Sends a signed transaction, treating "the node already knows it" as
/// success. Idempotent by construction: the same bytes always produce the
/// same txid.
async fn rebroadcast<R: RefundRpc>(rpc: &R, signed_hex: &str) -> Result<[u8; 32], RefundError> {
    use crate::goldcoin::rpc::BroadcastOutcome;
    let bytes = crate::goldcoin::hex::decode_vec(signed_hex)
        .map_err(|e| RefundError::refuse(format!("stored transaction is not hex: {e}")))?;
    // Computed from the exact bytes being sent, so a resumed broadcast can
    // never record a txid that belongs to different bytes.
    let txid = crate::goldcoin::tx::txid_of_serialized(&bytes);
    match rpc.send_raw_transaction(signed_hex).await {
        Ok(BroadcastOutcome::Accepted { .. })
        | Ok(BroadcastOutcome::AlreadyInChain)
        | Ok(BroadcastOutcome::AlreadyInMempool) => Ok(txid),
        Ok(BroadcastOutcome::MissingInputs) => Err(RefundError::refuse(
            "the node rejected the refund with missing-inputs: an input has been spent \
             elsewhere. Do NOT rebuild blindly — reconcile the vault first",
        )),
        Err(e) => Err(RefundError::Rpc(e.to_string())),
    }
}

/// Rebuilds the plan for a resumed `Built` refund from the persisted
/// reserved inputs, so a resume spends EXACTLY the outpoints already
/// reserved rather than re-running selection against a pool that may have
/// changed.
fn rebuild_reserved_plan(
    ledger: &Ledger,
    request_id: i64,
    derived: &DerivedSource,
    vault: &MultisigVault,
    policy: &PayoutPolicy,
) -> Result<PayoutPlan, RefundError> {
    let persisted = ledger.get_goldcoin_refund_inputs(request_id)?;
    if persisted.is_empty() {
        return Err(RefundError::refuse(
            "resumed refund has no persisted inputs",
        ));
    }
    let script_hex = vault.script_pubkey_hex();
    let inputs: Vec<VaultUtxo> = persisted
        .iter()
        .map(|(txid, vout, amount)| VaultUtxo {
            txid: *txid,
            vout: *vout,
            amount_atomic: *amount,
            script_pubkey_hex: script_hex.clone(),
        })
        .collect();

    let total_in: u64 = inputs.iter().map(|u| u.amount_atomic).sum();
    let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
    let selection = coin::SelectionResult {
        selected: inputs.clone(),
        total_selected: total_in,
        fee_atomic: 0,
    };
    let (change_outputs, fee_atomic) = coin::finalize_fanout(
        &selection,
        derived.observed_amount_atomic,
        inputs.len(),
        input_bytes,
        policy.fee_rate_per_kb,
        policy.dust_threshold,
        policy.change_fanout_target_atomic,
        policy.change_fanout_max_outputs,
    );
    let input_contexts = inputs
        .iter()
        .map(|_| PayoutInputContext {
            vault: vault.clone(),
            funding_request_id: None,
        })
        .collect();
    Ok(PayoutPlan {
        inputs,
        input_contexts,
        dest_p2pkh_hash: derived.refund_dest_p2pkh_hash,
        payout_atomic: derived.observed_amount_atomic,
        change_outputs,
        vault_script_pubkey: vault.script_pubkey(),
        fee_atomic,
    })
}

#[cfg(test)]
mod tests;
