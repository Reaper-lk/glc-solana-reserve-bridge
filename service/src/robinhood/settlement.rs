//! The Robinhood settlement engine: the phases that drive every outbound
//! Robinhood operation from a bridge request to a finalized on-chain
//! transaction.
//!
//! # The two flows, and their ordering
//!
//! ```text
//! GlcToRhn   Goldcoin deposit confirmed
//!            -> fee/net in canonical 8dp
//!            -> net widened exactly to Robinhood 18dp
//!            -> PayoutAuth authorized 2-of-3
//!            -> nonce allocated, transaction signed, persisted
//!            -> executePayout broadcast
//!            -> receipt read back (status must be 1)
//!            -> confirmation depth reached
//!            -> request Settled
//!
//! RhnToGlc   Robinhood deposit FINALIZED (Phase E)
//!            -> folded into one bridge request
//!            -> Goldcoin payout built and broadcast (existing machinery)
//!            -> Goldcoin payout CONFIRMED at the required depth
//!            -> SettlementAuth authorized 2-of-3
//!            -> executeSettlement broadcast
//!            -> receipt read back, confirmation depth reached
//!            -> obligation Settled on-chain, request Settled
//! ```
//!
//! ## The `RhnToGlc` ordering is the load-bearing part
//!
//! `executeSettlement` moves an obligation out of `Pending`, which is the
//! ONLY state a refund can be issued from. Marking it settled is
//! therefore an irreversible statement that the depositor has been paid
//! on Goldcoin — and once made, the depositor's principal can never be
//! returned.
//!
//! So it is the LAST step, and its precondition is the strongest
//! available evidence that the Goldcoin payout is irreversible: the
//! payout's own transaction, verified against the chain, at or past
//! `required_goldcoin_confirmations`. Not "broadcast". Not "in the
//! mempool". Not "we signed it". Settling any earlier would mean that a
//! Goldcoin payout which never confirmed had already destroyed the
//! refund path for the deposit it was supposed to satisfy.
//!
//! This is the exact discipline the Solana leg already follows —
//! `record_goldcoin_completion` runs only from a `Confirmed` payout — and
//! it is preserved rather than reinvented.
//!
//! # Every phase is resumable at every point
//!
//! No phase holds state across a tick. Each reads what is durably
//! recorded, decides one step, and commits it before moving on. The
//! ordering constraints that make that safe are database CHECKs rather
//! than the order of statements here (see
//! [`crate::ledger::robinhood_tx`]), so a process killed between any two
//! lines resumes correctly.
//!
//! # Refunds live in [`super::refund`]
//!
//! Deliberately a separate module. A refund is the OPPOSITE decision
//! from a settlement — it says the payout will never happen — and the two
//! are mutually exclusive on-chain by construction. Keeping them apart
//! means no function can accidentally reach both.

use std::time::Duration;

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::amount_conversion::{verify_fee_breakdown, CanonicalAtomic};
use crate::evm::{EvmAddress, EvmU256};
use crate::ledger::{
    BeginTxOutcome, Direction, Ledger, LedgerError, NewRobinhoodTx, RequestState, RobinhoodTx,
    RobinhoodTxKind, RobinhoodTxState,
};
use crate::routes::Route;

use super::auth::{self, PayoutAuth, SettlementAuth, ACTION_PAYOUT, ACTION_SETTLE};
use super::calls::{self, ContractGate, GateError, GateRefusal};
use super::preflight::VerifiedDeployment;
use super::rpc::{EvmBlockTag, EvmBroadcastOutcome, EvmCall, EvmCallRpc, EvmRpc, EvmSubmitRpc};
use super::settlement_config::RobinhoodSettlementConfig;
use super::signer::{collect_quorum, EvmAuthSigner, QuorumError};
use super::submitter::{self, SubmitError, Submitter};

#[derive(Debug, thiserror::Error)]
pub enum SettlementError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Submit(#[from] SubmitError),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error("assembling the 2-of-3 authorization quorum: {0}")]
    Quorum(#[from] QuorumError),
    #[error("request {request_id}: {detail}")]
    Request { request_id: i64, detail: String },
    #[error(
        "request {request_id}'s stored fee breakdown does not reconcile at its own recorded \
         rate: {detail}"
    )]
    Fee { request_id: i64, detail: String },
    #[error(
        "request {request_id}'s net entitlement of {canonical} canonical unit(s) cannot be \
         represented exactly in Robinhood's 18-decimal unit: {detail}"
    )]
    AmountConversion {
        request_id: i64,
        canonical: u64,
        detail: String,
    },
}

/// One tick's worth of Robinhood settlement activity, for the
/// orchestrator's report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettlementReport {
    pub folded: u32,
    pub folded_parked: u32,
    pub authorized: u32,
    pub broadcast: u32,
    pub replaced: u32,
    pub included: u32,
    pub finalized: u32,
    pub reverted: u32,
    pub manual_review: u32,
    pub errors: Vec<String>,
}

/// The settlement engine: everything needed to authorize, sign,
/// broadcast and confirm a Robinhood operation.
///
/// Holds the verified deployment rather than the raw config, so every
/// value it binds into an authorization — the token, the chain pair, the
/// domain — is one that was PROVEN against the deployed contract at
/// preflight rather than one that was configured and hoped for.
pub struct Settler<R> {
    rpc: R,
    submitter: Submitter,
    signers: Vec<Box<dyn EvmAuthSigner>>,
    deployment: VerifiedDeployment,
    config: RobinhoodSettlementConfig,
    signer_timeout: Duration,
    goldcoin_network: crate::goldcoin::address::Network,
    required_goldcoin_confirmations: i64,
}

impl<R> Settler<R>
where
    // All three surfaces, because this is the one component that
    // legitimately needs all three: it reads the head to count
    // confirmations ([`EvmRpc`]), reads contract state to gate every
    // broadcast ([`EvmCallRpc`]), and broadcasts ([`EvmSubmitRpc`]).
    //
    // The deposit indexer's bound stays [`EvmRpc`] alone, so observing
    // deposits still cannot send a transaction — see
    // [`super::rpc`]'s module docs.
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc: R,
        submitter: Submitter,
        signers: Vec<Box<dyn EvmAuthSigner>>,
        deployment: VerifiedDeployment,
        config: RobinhoodSettlementConfig,
        signer_timeout: Duration,
        goldcoin_network: crate::goldcoin::address::Network,
        required_goldcoin_confirmations: i64,
    ) -> Settler<R> {
        Settler {
            rpc,
            submitter,
            signers,
            deployment,
            config,
            signer_timeout,
            goldcoin_network,
            required_goldcoin_confirmations,
        }
    }

    pub fn deployment(&self) -> &VerifiedDeployment {
        &self.deployment
    }

    pub fn submitter_address(&self) -> EvmAddress {
        self.submitter.address()
    }

    // -----------------------------------------------------------------
    // Phase 1 — fold finalized Robinhood deposits
    // -----------------------------------------------------------------

    /// Folds every FINAL, not-yet-folded observation into a bridge
    /// request.
    ///
    /// Runs whether or not the route is open — see
    /// [`super::fold`]'s module docs for why a closed route parks a
    /// deposit rather than ignoring it. `route_open` decides only whether
    /// the resulting request is payable.
    pub fn tick_fold(
        &self,
        ledger: &mut Ledger,
        route_open: bool,
        now: i64,
        report: &mut SettlementReport,
    ) {
        let observations = match ledger.unfolded_final_robinhood_observations() {
            Ok(rows) => rows,
            Err(e) => {
                report
                    .errors
                    .push(format!("unfolded_final_robinhood_observations: {e}"));
                return;
            }
        };
        for observation in observations {
            let index = observation.observation.obligation_index;
            // Only the executable inbound route folds. An `RhnToSol`
            // observation is recorded and left alone: there is no
            // machinery to settle it, and inventing a parked request for
            // it would imply one exists.
            if observation.observation.route != Route::RhnToGlc {
                continue;
            }
            match super::fold::fold_observation(
                ledger,
                &observation,
                self.goldcoin_network,
                route_open,
                now,
            ) {
                Ok(super::fold::FoldOutcome::FoldedFinalized { .. }) => report.folded += 1,
                Ok(super::fold::FoldOutcome::FoldedManualReview { .. }) => {
                    report.folded += 1;
                    report.folded_parked += 1;
                }
                Ok(super::fold::FoldOutcome::AlreadyFolded { .. }) => {}
                Err(e) => report
                    .errors
                    .push(format!("folding Robinhood obligation {index}: {e}")),
            }
        }
    }

    // -----------------------------------------------------------------
    // Phase 2 — authorize
    // -----------------------------------------------------------------

    /// Creates and authorizes the outbound operation for every request
    /// that is ready for one.
    ///
    /// - `GlcToRhn` in `SourceFinalized`: its Goldcoin deposit is final,
    ///   so the Robinhood payout may be authorized.
    /// - `RhnToGlc` in `DestinationConfirmed`: its GOLDCOIN PAYOUT has
    ///   confirmed at the required depth, so — and only so — the
    ///   obligation may be authorized for settlement.
    pub async fn tick_authorize(
        &self,
        ledger: &mut Ledger,
        now: i64,
        report: &mut SettlementReport,
    ) {
        let payouts =
            match ledger.requests_by_state(Direction::GlcToRhn, RequestState::SourceFinalized) {
                Ok(r) => r,
                Err(e) => {
                    report
                        .errors
                        .push(format!("requests_by_state(GlcToRhn, SourceFinalized): {e}"));
                    Vec::new()
                }
            };
        for request in payouts {
            if let Err(e) = self.authorize_payout(ledger, request.id, now).await {
                report.errors.push(format!(
                    "authorizing payout for request {}: {e}",
                    request.id
                ));
            } else {
                report.authorized += 1;
            }
        }

        let settlements = match ledger
            .requests_by_state(Direction::RhnToGlc, RequestState::DestinationConfirmed)
        {
            Ok(r) => r,
            Err(e) => {
                report.errors.push(format!(
                    "requests_by_state(RhnToGlc, DestinationConfirmed): {e}"
                ));
                Vec::new()
            }
        };
        for request in settlements {
            match self.authorize_settlement(ledger, request.id, now).await {
                Ok(true) => report.authorized += 1,
                Ok(false) => {}
                Err(e) => report.errors.push(format!(
                    "authorizing settlement for request {}: {e}",
                    request.id
                )),
            }
        }
    }

    /// The `GlcToRhn` payout authorization.
    ///
    /// # The amount
    ///
    /// The stored fee breakdown is RECOMPUTED from the request's own
    /// gross at its own recorded rate and required to reconcile
    /// (`verify_fee_breakdown`) — the same discipline every other
    /// settlement path applies, so a tampered fee column can never
    /// produce a payout. The recomputed NET is then widened to
    /// Robinhood's 18-decimal unit through the one conversion that
    /// enforces exactness.
    async fn authorize_payout(
        &self,
        ledger: &mut Ledger,
        request_id: i64,
        now: i64,
    ) -> Result<(), SettlementError> {
        // Already begun? Resume rather than re-authorize.
        if let Some(existing) = ledger.get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)? {
            if existing.state != RobinhoodTxState::Authorizing {
                return Ok(());
            }
        }

        let request = ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;

        let breakdown = verify_fee_breakdown(
            request.gross_amount_atomic,
            request.fee_bps,
            request.fee_amount_atomic,
            request.net_amount_atomic,
        )
        .map_err(|e| SettlementError::Fee {
            request_id,
            detail: e.to_string(),
        })?;
        let amount = CanonicalAtomic(breakdown.net.0)
            .to_robinhood()
            .map_err(|e| SettlementError::AmountConversion {
                request_id,
                canonical: breakdown.net.0,
                detail: e.to_string(),
            })?;

        // The recipient is a 20-byte EVM address, stored as the request's
        // `recipient` bytes. Parsed rather than assumed: a request whose
        // recipient is not an address cannot be paid out and must not
        // silently become one.
        let recipient = EvmAddress::try_from_slice(&request.recipient).map_err(|e| {
            SettlementError::Request {
                request_id,
                detail: format!("recipient is not a 20-byte EVM address: {e}"),
            }
        })?;

        // The durable identity of a Goldcoin-sourced payout: the outpoint
        // that funded it plus the row it created. Deterministic, so a
        // restart re-derives the SAME contract request id and re-attempts
        // the same on-chain request rather than creating a second one.
        let (txid, vout) = match (request.source_txid, request.source_vout) {
            (Some(txid), Some(vout)) => (txid, vout),
            _ => {
                return Err(SettlementError::Request {
                    request_id,
                    detail: "a GlcToRhn payout must name the Goldcoin outpoint that funded it"
                        .to_string(),
                })
            }
        };
        let identity = auth::goldcoin_source_identity(txid, vout, request_id);
        let domain = self.deployment.domain();
        let contract_request_id =
            auth::derive_request_id(ACTION_PAYOUT, Route::GlcToRhn, domain, &identity)?;

        // The signer epoch is read LIVE, not configured: an authorization
        // bound to a stale epoch is worthless, and reading it here means
        // the payload is built against the epoch that was current when
        // the signatures were requested.
        let signer_epoch = self
            .deployment_reader()
            .signer_epoch(&self.rpc, EvmBlockTag::Latest)
            .await
            .map_err(calls::GateError::Read)?;

        let expiry = (now as u64).saturating_add(self.config.authorization_ttl.as_secs());
        let chains = self
            .deployment
            .chains_for(Route::GlcToRhn)
            .expect("preflight verified the GlcToRhn chain pair");

        let authorization = auth::EvmAuthRequest::payout(
            domain,
            PayoutAuth {
                route: Route::GlcToRhn,
                chains,
                token: self.deployment.token,
                request_id: contract_request_id,
                recipient,
                amount,
                signer_epoch,
                expiry,
            },
        );
        let digest = authorization.digest()?;

        let outcome = ledger.begin_robinhood_tx(
            &NewRobinhoodTx {
                kind: RobinhoodTxKind::Payout,
                request_id,
                route: Route::GlcToRhn,
                bridge_contract: self.deployment.bridge_contract.to_bytes(),
                chain_id: self.deployment.chain_id.get(),
                contract_request_id,
                obligation_index: None,
                recipient: Some(recipient.to_bytes()),
                amount_robinhood: Some(amount.to_u256().to_be_bytes()),
                signer_epoch,
                expiry,
                auth_digest: digest,
            },
            now,
        )?;
        let tx_id = match outcome {
            BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
        };

        self.collect_and_store(ledger, tx_id, &authorization, now)
            .await
    }

    /// The `RhnToGlc` settlement authorization.
    ///
    /// # The precondition, checked against the CHAIN and not only the
    /// ledger
    ///
    /// The request being in `DestinationConfirmed` means this service
    /// observed its Goldcoin payout at the required depth. That is the
    /// evidence, and it is re-derived here from the payout row rather
    /// than inferred from the request's state alone: the payout must
    /// exist, must have a transaction id, and must be at or past the
    /// configured depth.
    ///
    /// Returns `false` when the request is simply not ready yet — not an
    /// error, just a step whose turn has not come.
    async fn authorize_settlement(
        &self,
        ledger: &mut Ledger,
        request_id: i64,
        now: i64,
    ) -> Result<bool, SettlementError> {
        if let Some(existing) =
            ledger.get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)?
        {
            if existing.state != RobinhoodTxState::Authorizing {
                return Ok(false);
            }
        }

        // THE ordering guard. Re-derived from the payout row rather than
        // taken from the request's state, because this is the single
        // check standing between "the depositor was paid on Goldcoin" and
        // "this deposit's refund path has been destroyed".
        let payout =
            ledger
                .get_goldcoin_payout(request_id)?
                .ok_or_else(|| SettlementError::Request {
                    request_id,
                    detail:
                        "no Goldcoin payout exists for this request: a Robinhood obligation is \
                         only ever settled AFTER its Goldcoin payout confirmed"
                            .to_string(),
                })?;
        if payout.txid.is_none() {
            return Err(SettlementError::Request {
                request_id,
                detail: "the Goldcoin payout has no transaction id: it was never broadcast"
                    .to_string(),
            });
        }
        if payout.confirmations < self.required_goldcoin_confirmations {
            // Not an error. The payout is confirming; settlement waits.
            return Ok(false);
        }
        if payout.state != "Confirmed" && payout.state != "Completed" {
            return Ok(false);
        }

        let request = ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        let obligation_index =
            request
                .source_obligation_index
                .ok_or_else(|| SettlementError::Request {
                    request_id,
                    detail: "an RhnToGlc request must name the obligation it settles".to_string(),
                })?;

        let domain = self.deployment.domain();
        let identity = auth::obligation_identity(obligation_index);
        let contract_request_id =
            auth::derive_request_id(ACTION_SETTLE, Route::RhnToGlc, domain, &identity)?;

        let signer_epoch = self
            .deployment_reader()
            .signer_epoch(&self.rpc, EvmBlockTag::Latest)
            .await
            .map_err(calls::GateError::Read)?;
        let expiry = (now as u64).saturating_add(self.config.authorization_ttl.as_secs());
        let chains = self
            .deployment
            .chains_for(Route::RhnToGlc)
            .expect("preflight verified the RhnToGlc chain pair");

        let authorization = auth::EvmAuthRequest::settlement(
            domain,
            SettlementAuth {
                route: Route::RhnToGlc,
                chains,
                request_id: contract_request_id,
                obligation_index,
                signer_epoch,
                expiry,
            },
        );
        let digest = authorization.digest()?;

        let outcome = ledger.begin_robinhood_tx(
            &NewRobinhoodTx {
                kind: RobinhoodTxKind::Settlement,
                request_id,
                route: Route::RhnToGlc,
                bridge_contract: self.deployment.bridge_contract.to_bytes(),
                chain_id: self.deployment.chain_id.get(),
                contract_request_id,
                obligation_index: Some(obligation_index),
                // A settlement moves nothing and names neither a
                // recipient nor an amount.
                recipient: None,
                amount_robinhood: None,
                signer_epoch,
                expiry,
                auth_digest: digest,
            },
            now,
        )?;
        let tx_id = match outcome {
            BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
        };

        self.collect_and_store(ledger, tx_id, &authorization, now)
            .await?;
        Ok(true)
    }

    /// Collects a verified 2-of-3 quorum and stores it.
    ///
    /// Shared by all three operation kinds, so there is exactly one place
    /// a quorum is assembled and exactly one place it is verified.
    async fn collect_and_store(
        &self,
        ledger: &mut Ledger,
        tx_id: i64,
        authorization: &auth::EvmAuthRequest,
        now: i64,
    ) -> Result<(), SettlementError> {
        let quorum = collect_quorum(
            &self.signers,
            &self.deployment.signers,
            authorization,
            self.signer_timeout,
        )
        .await?;
        ledger.record_robinhood_authorization(tx_id, &quorum.for_storage(), now)?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Phase 3 — sign and broadcast
    // -----------------------------------------------------------------

    /// Signs and broadcasts every authorized operation, and re-broadcasts
    /// every in-flight one that is due.
    pub async fn tick_broadcast(
        &self,
        ledger: &mut Ledger,
        now: i64,
        report: &mut SettlementReport,
    ) {
        // Reconcile the nonce cursor once per tick, before any
        // allocation. A failure here is not fatal: the allocator's
        // authority is the ledger, and the observed count is only a
        // floor.
        if let Err(e) = self.submitter.reconcile_nonce(&self.rpc, ledger, now).await {
            report
                .errors
                .push(format!("reconciling the submitter nonce: {e}"));
        }

        // BOTH work lists are read BEFORE either is processed, and this
        // is load-bearing rather than stylistic.
        //
        // `sign_and_send` moves a row from `Authorized` to `Broadcast`.
        // If the in-flight list were read afterwards it would contain the
        // rows this same tick just sent, and every first broadcast would
        // immediately be followed by a redundant second send of the same
        // bytes — harmless to correctness (the bytes are identical) but a
        // wasted round trip on every single operation, and a misleading
        // `broadcast_attempts` count for the operator reading it.
        //
        // Reading both up front means a row promoted during this tick is
        // simply picked up by the NEXT one, which is when a receipt could
        // first plausibly exist anyway.
        let authorized = match ledger.robinhood_txs_in_state(RobinhoodTxState::Authorized) {
            Ok(rows) => rows,
            Err(e) => {
                report
                    .errors
                    .push(format!("robinhood_txs_in_state(Authorized): {e}"));
                Vec::new()
            }
        };
        let in_flight = match ledger.robinhood_txs_in_state(RobinhoodTxState::Broadcast) {
            Ok(rows) => rows,
            Err(e) => {
                report
                    .errors
                    .push(format!("robinhood_txs_in_state(Broadcast): {e}"));
                Vec::new()
            }
        };

        for tx in authorized {
            match self.sign_and_send(ledger, &tx, now).await {
                Ok(()) => report.broadcast += 1,
                Err(e) => {
                    report.errors.push(format!(
                        "broadcasting {} for request {}: {e}",
                        tx.kind.as_str(),
                        tx.request_id
                    ));
                }
            }
        }

        for tx in in_flight {
            match self.rebroadcast_or_replace(ledger, &tx, now).await {
                Ok(true) => report.replaced += 1,
                Ok(false) => {}
                Err(e) => report.errors.push(format!(
                    "re-broadcasting {} for request {}: {e}",
                    tx.kind.as_str(),
                    tx.request_id
                )),
            }
        }
    }

    /// One authorized operation: gate, fund-check, estimate, allocate,
    /// sign, persist, broadcast.
    ///
    /// The ORDER is the design. Everything that can refuse happens before
    /// a nonce is allocated, so a refusal never leaves a gap in the nonce
    /// sequence that every later transaction would queue behind.
    async fn sign_and_send(
        &self,
        ledger: &mut Ledger,
        tx: &RobinhoodTx,
        now: i64,
    ) -> Result<(), SettlementError> {
        // The authorization must not have expired between being minted
        // and being sent. The contract checks this too and reverts on it;
        // checking here means an expired authorization costs no gas and
        // no nonce, and is re-minted on a later tick instead.
        if (now as u64) >= tx.expiry {
            ledger.mark_robinhood_tx_manual_review(
                tx.id,
                "the authorization expired before it was broadcast; it must be re-minted, which \
                 requires a fresh quorum",
                now,
            )?;
            return Ok(());
        }

        let call = self.build_call(ledger, tx)?;

        // The contract-side gate, read live. A service-side flag is
        // necessary and not sufficient.
        match ContractGate::new(self.deployment.bridge_contract)
            .check(
                &self.rpc,
                tx.route,
                tx.action,
                tx.contract_request_id,
                tx.signer_epoch,
                EvmBlockTag::Latest,
            )
            .await
        {
            Ok(()) => {}
            Err(GateError::Refused(GateRefusal::AlreadyExecuted)) => {
                // The operation already happened on-chain under this
                // exact request id — almost certainly a broadcast this
                // service lost track of. NOT retried.
                ledger.record_robinhood_already_executed(tx.id, now)?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }

        // Funding, BEFORE the nonce: an underfunded submitter must
        // produce no half-built operation.
        self.submitter.check_funding(&self.rpc).await?;

        // Gas estimation doubles as a dry run — `eth_estimateGas`
        // EXECUTES the call, so a revert here is caught before a nonce is
        // consumed.
        let gas_limit = self.submitter.estimate_gas(&self.rpc, &call).await?;
        let fees = self.submitter.read_fees(&self.rpc, 0).await?;

        // Now, and only now, the nonce — persisted before anything is
        // signed.
        let nonce = ledger.allocate_robinhood_nonce(
            tx.id,
            self.submitter.address().to_bytes(),
            self.deployment.chain_id.get(),
            now,
        )?;

        let signed = self.submitter.sign(nonce, gas_limit, fees, &call);
        // Persisted BEFORE the broadcast. After this commit, a crash at
        // any point is recovered by re-broadcasting these exact bytes.
        ledger.record_robinhood_signed(
            tx.id,
            self.config.tx_envelope.as_str(),
            gas_limit,
            &submitter::describe_fees(fees),
            &signed.raw,
            signed.hash.to_bytes(),
            now,
        )?;

        let refreshed = ledger
            .get_robinhood_tx(tx.id)?
            .ok_or(LedgerError::RequestNotFound(tx.request_id))?;
        match self.submitter.broadcast(&self.rpc, &refreshed).await {
            Ok(EvmBroadcastOutcome::Accepted { .. }) | Ok(EvmBroadcastOutcome::AlreadyKnown) => {
                ledger.record_robinhood_broadcast(tx.id, now)?;
                Ok(())
            }
            Ok(EvmBroadcastOutcome::NonceTooLow) => {
                // The chain has moved past this nonce. Something
                // consumed it — possibly this very transaction. Recorded
                // as broadcast so the receipt phase investigates, which
                // is the only way to find out which.
                ledger.record_robinhood_broadcast(tx.id, now)?;
                Ok(())
            }
            Ok(EvmBroadcastOutcome::ReplacementUnderpriced) => {
                // Only reachable if a transaction already occupies this
                // nonce — i.e. this operation was already broadcast and
                // this service lost the record. Recorded as broadcast so
                // the receipt phase resolves it.
                ledger.record_robinhood_broadcast(tx.id, now)?;
                Ok(())
            }
            Ok(EvmBroadcastOutcome::Rejected { code, message }) => {
                // Still recorded as broadcast: the bytes and the nonce
                // are durable either way, and a definitive rejection now
                // may become an acceptance later (a temporarily full
                // mempool, a fee floor that drops). The receipt phase and
                // the staleness rule decide what happens next.
                ledger.record_robinhood_broadcast(tx.id, now)?;
                Err(SettlementError::Request {
                    request_id: tx.request_id,
                    detail: format!("the node refused the broadcast (code {code}): {message}"),
                })
            }
            Err(e) => {
                // The question was never answered. The bytes may or may
                // not have reached the node. Recorded as broadcast — the
                // safe direction, because it means the next tick
                // re-sends the IDENTICAL bytes rather than building
                // anything new.
                ledger.record_robinhood_broadcast(tx.id, now)?;
                Err(e.into())
            }
        }
    }

    /// Re-broadcasts an in-flight operation, bumping its fee if it has
    /// been waiting long enough.
    ///
    /// A replacement keeps the SAME nonce. It is the same operation at a
    /// higher fee, never a second one.
    ///
    /// Returns `true` when a fee-bumped replacement was actually signed.
    async fn rebroadcast_or_replace(
        &self,
        ledger: &mut Ledger,
        tx: &RobinhoodTx,
        now: i64,
    ) -> Result<bool, SettlementError> {
        // A broadcast that has been unresolved for too long is not a
        // pending transaction any more; it is an incident.
        if submitter::broadcast_is_stale(tx, now) {
            ledger.mark_robinhood_tx_manual_review(
                tx.id,
                "this broadcast has been unresolved past the incident threshold: no receipt, and \
                 the contract's replay guard still reports the operation as not executed. Do NOT \
                 re-authorize it — determine what happened to the nonce first.",
                now,
            )?;
            return Ok(false);
        }

        match self.submitter.replacement_due(tx, now) {
            Ok(false) => {
                // Not due for a bump; re-send the identical bytes, which
                // is always safe and is how a dropped transaction
                // re-enters a mempool.
                match self.submitter.broadcast(&self.rpc, tx).await {
                    Ok(_) => {
                        ledger.record_robinhood_broadcast(tx.id, now)?;
                        Ok(false)
                    }
                    Err(e) => Err(e.into()),
                }
            }
            Ok(true) => {
                let call = self.build_call(ledger, tx)?;
                let nonce = tx.nonce.ok_or_else(|| SettlementError::Request {
                    request_id: tx.request_id,
                    detail: "an in-flight operation with no nonce cannot be replaced".to_string(),
                })?;
                let gas_limit = tx.gas_limit.unwrap_or(self.config.max_gas_limit);
                let fees = self
                    .submitter
                    .read_fees(&self.rpc, tx.replacement_attempts as u32 + 1)
                    .await?;
                let signed = self.submitter.sign(nonce, gas_limit, fees, &call);
                ledger.record_robinhood_replacement(
                    tx.id,
                    &submitter::describe_fees(fees),
                    &signed.raw,
                    signed.hash.to_bytes(),
                    now,
                )?;
                let refreshed = ledger
                    .get_robinhood_tx(tx.id)?
                    .ok_or(LedgerError::RequestNotFound(tx.request_id))?;
                self.submitter.broadcast(&self.rpc, &refreshed).await?;
                ledger.record_robinhood_broadcast(tx.id, now)?;
                Ok(true)
            }
            Err(e @ SubmitError::ReplacementBudgetExhausted { .. }) => {
                ledger.mark_robinhood_tx_manual_review(tx.id, &e.to_string(), now)?;
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Rebuilds the exact calldata for one operation from its stored
    /// authorization and its stored signatures.
    ///
    /// # Why it is rebuilt rather than stored
    ///
    /// The calldata is a pure function of the authorization payload and
    /// the signatures, both of which ARE stored. Rebuilding it means the
    /// bytes that go on the wire are derived from the same values the
    /// digest was derived from, every time — there is no second copy that
    /// could drift. The digest is additionally re-derived and compared
    /// against the stored one, so a payload that changed underneath
    /// already-collected signatures is caught before it is sent rather
    /// than reverting on-chain.
    fn build_call(&self, ledger: &Ledger, tx: &RobinhoodTx) -> Result<EvmCall, SettlementError> {
        let stored = ledger.robinhood_auth_signatures(tx.id)?;
        if stored.len() != super::signer::SIGNER_THRESHOLD {
            return Err(SettlementError::Request {
                request_id: tx.request_id,
                detail: format!(
                    "expected {} stored signatures, found {}",
                    super::signer::SIGNER_THRESHOLD,
                    stored.len()
                ),
            });
        }
        let signatures: Vec<crate::evm::EvmSignature> = stored
            .iter()
            .map(|s| crate::evm::EvmSignature::from_bytes(s.signature))
            .collect::<Result<_, _>>()
            .map_err(|e| SettlementError::Request {
                request_id: tx.request_id,
                detail: format!("a stored signature is malformed: {e}"),
            })?;

        let domain = self.deployment.domain();
        let chains =
            self.deployment
                .chains_for(tx.route)
                .ok_or_else(|| SettlementError::Request {
                    request_id: tx.request_id,
                    detail: format!("route {} has no verified chain pair", tx.route.as_str()),
                })?;

        let (data, digest) = match tx.kind {
            RobinhoodTxKind::Payout => {
                let payload = PayoutAuth {
                    route: tx.route,
                    chains,
                    token: self.deployment.token,
                    request_id: tx.contract_request_id,
                    recipient: EvmAddress::from_bytes(tx.recipient.ok_or_else(|| {
                        SettlementError::Request {
                            request_id: tx.request_id,
                            detail: "a payout must name a recipient".to_string(),
                        }
                    })?),
                    amount: RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes(
                        tx.amount_robinhood
                            .ok_or_else(|| SettlementError::Request {
                                request_id: tx.request_id,
                                detail: "a payout must name an amount".to_string(),
                            })?,
                    ))
                    .map_err(|e| SettlementError::Request {
                        request_id: tx.request_id,
                        detail: format!("the stored payout amount is not usable: {e}"),
                    })?,
                    signer_epoch: tx.signer_epoch,
                    expiry: tx.expiry,
                };
                (
                    calls::encode_execute_payout(&payload, &signatures),
                    payload.digest(domain)?,
                )
            }
            RobinhoodTxKind::Settlement => {
                let payload = SettlementAuth {
                    route: tx.route,
                    chains,
                    request_id: tx.contract_request_id,
                    obligation_index: tx.obligation_index.ok_or_else(|| {
                        SettlementError::Request {
                            request_id: tx.request_id,
                            detail: "a settlement must name an obligation".to_string(),
                        }
                    })?,
                    signer_epoch: tx.signer_epoch,
                    expiry: tx.expiry,
                };
                (
                    calls::encode_execute_settlement(&payload, &signatures),
                    payload.digest(domain)?,
                )
            }
            RobinhoodTxKind::Refund => {
                let payload = super::refund::rebuild_refund_auth(self, tx)?;
                (
                    calls::encode_execute_refund(&payload, &signatures),
                    payload.digest(domain)?,
                )
            }
        };

        // The check that makes rebuilding safe: the payload this call was
        // just built from must hash to the digest the quorum actually
        // signed.
        if digest != tx.auth_digest {
            return Err(SettlementError::Request {
                request_id: tx.request_id,
                detail: "the rebuilt authorization payload does not hash to the digest the \
                         quorum signed — something about this operation changed after it was \
                         authorized"
                    .to_string(),
            });
        }

        Ok(EvmCall {
            to: self.deployment.bridge_contract,
            data,
        })
    }

    pub(crate) fn deployment_reader(&self) -> calls::BridgeReader {
        calls::BridgeReader::new(self.deployment.bridge_contract)
    }

    pub(crate) fn rpc(&self) -> &R {
        &self.rpc
    }

    pub(crate) fn config_ref(&self) -> &RobinhoodSettlementConfig {
        &self.config
    }

    pub(crate) fn signer_timeout(&self) -> Duration {
        self.signer_timeout
    }

    pub(crate) fn signers_ref(&self) -> &[Box<dyn EvmAuthSigner>] {
        &self.signers
    }

    // -----------------------------------------------------------------
    // Phase 4 — receipts and finality
    // -----------------------------------------------------------------

    /// Reads receipts back for every broadcast operation and advances
    /// confirmations for every included one.
    pub async fn tick_receipts(
        &self,
        ledger: &mut Ledger,
        now: i64,
        report: &mut SettlementReport,
    ) {
        let head = match self.rpc.block_number().await {
            Ok(head) => head,
            Err(e) => {
                report.errors.push(format!("reading the chain head: {e}"));
                return;
            }
        };

        for state in [RobinhoodTxState::Broadcast, RobinhoodTxState::Included] {
            let rows = match ledger.robinhood_txs_in_state(state) {
                Ok(rows) => rows,
                Err(e) => {
                    report
                        .errors
                        .push(format!("robinhood_txs_in_state({state:?}): {e}"));
                    continue;
                }
            };
            for tx in rows {
                match self.poll_receipt(ledger, &tx, head, now).await {
                    Ok(ReceiptOutcome::Pending) => {}
                    Ok(ReceiptOutcome::Included) => report.included += 1,
                    Ok(ReceiptOutcome::Finalized) => report.finalized += 1,
                    Ok(ReceiptOutcome::Reverted) => report.reverted += 1,
                    Ok(ReceiptOutcome::ManualReview) => report.manual_review += 1,
                    Err(e) => report.errors.push(format!(
                        "polling the receipt for {} on request {}: {e}",
                        tx.kind.as_str(),
                        tx.request_id
                    )),
                }
            }
        }
    }

    async fn poll_receipt(
        &self,
        ledger: &mut Ledger,
        tx: &RobinhoodTx,
        head: u64,
        now: i64,
    ) -> Result<ReceiptOutcome, SettlementError> {
        let Some(hash) = Submitter::tracked_hash(tx) else {
            return Ok(ReceiptOutcome::Pending);
        };
        let receipt =
            self.rpc
                .transaction_receipt(hash)
                .await
                .map_err(|source| SubmitError::Rpc {
                    doing: "reading a transaction receipt",
                    source,
                })?;

        let Some(receipt) = receipt else {
            // Not mined yet, as far as this node knows. That is NOT
            // "failed" and NOT "will not be mined" — the broadcast phase
            // handles re-sending and the staleness rule handles giving up.
            return Ok(ReceiptOutcome::Pending);
        };

        // A receipt for a transaction that is not the one this row is
        // tracking is not an answer to the question that was asked.
        if receipt.tx_hash.to_bytes() != hash.to_bytes() {
            return Err(SettlementError::Request {
                request_id: tx.request_id,
                detail: "the node returned a receipt for a different transaction".to_string(),
            });
        }

        let state = ledger.record_robinhood_receipt(
            tx.id,
            receipt.success,
            receipt.block_number,
            receipt.block_hash.to_bytes(),
            now,
        )?;

        match state {
            RobinhoodTxState::Reverted => {
                // A revert is terminal for the transaction AND for the
                // request: the operation did not happen and re-sending
                // the same call would revert identically. It goes to a
                // human with the evidence attached.
                ledger.mark_robinhood_tx_manual_review(
                    tx.id,
                    &format!(
                        "the {} transaction was mined in block {} and REVERTED. It consumed its \
                         nonce and its gas and achieved nothing. Do not re-authorize until the \
                         contract-side precondition that failed is understood.",
                        tx.kind.as_str(),
                        receipt.block_number
                    ),
                    now,
                )?;
                self.on_reverted(ledger, tx, now)?;
                return Ok(ReceiptOutcome::Reverted);
            }
            RobinhoodTxState::ManualReview => return Ok(ReceiptOutcome::ManualReview),
            _ => {}
        }

        // Included and successful. Included is not finished: depth is
        // counted the same way every other chain in this service counts
        // it — `head - block + 1`, so a depth of 1 means "in the head
        // block itself".
        let confirmations = if head >= receipt.block_number {
            (head - receipt.block_number + 1) as i64
        } else {
            // A head behind the block we just read a receipt from is a
            // lagging replica, not a negative depth.
            0
        };
        let promoted = ledger.update_robinhood_confirmations(
            tx.id,
            confirmations,
            self.config.required_confirmations,
            now,
        )?;

        if promoted {
            // Verified before anything is completed: the operation's own
            // event must be present in the receipt's logs, and the
            // contract's replay guard must now report it executed. A
            // status of 1 says "the transaction did not revert"; these
            // say "it did THIS".
            self.verify_operation_effect(&receipt, tx).await?;
            self.on_finalized(ledger, tx, now)?;
            return Ok(ReceiptOutcome::Finalized);
        }
        Ok(ReceiptOutcome::Included)
    }

    /// Confirms that a successful receipt actually performed the
    /// operation this row describes.
    ///
    /// # Why a status of 1 is not enough
    ///
    /// `status = 1` means the transaction did not revert. It does not
    /// mean it did what this service intended: a transaction to the right
    /// address with the wrong calldata, or a receipt that belongs to some
    /// other call, would also carry status 1.
    ///
    /// Two independent confirmations are required:
    ///
    /// 1. The receipt's logs contain an event from the bridge contract —
    ///    the operation's own emission.
    /// 2. The contract's replay guard reports `(action, requestId)` as
    ///    executed, read AT THE RECEIPT'S OWN BLOCK so it describes the
    ///    state that transaction produced rather than whatever the chain
    ///    has moved on to.
    async fn verify_operation_effect(
        &self,
        receipt: &super::rpc::EvmReceipt,
        tx: &RobinhoodTx,
    ) -> Result<(), SettlementError> {
        let from_bridge = receipt
            .logs
            .iter()
            .any(|log| log.address == self.deployment.bridge_contract);
        if !from_bridge {
            return Err(SettlementError::Request {
                request_id: tx.request_id,
                detail: format!(
                    "the receipt for this {} succeeded but contains NO event from the bridge \
                     contract — a status of 1 says the transaction did not revert, not that it \
                     did what was intended",
                    tx.kind.as_str()
                ),
            });
        }
        let executed = self
            .deployment_reader()
            .request_executed(
                &self.rpc,
                tx.action,
                tx.contract_request_id,
                EvmBlockTag::Number(receipt.block_number),
            )
            .await
            .map_err(calls::GateError::Read)?;
        if !executed {
            return Err(SettlementError::Request {
                request_id: tx.request_id,
                detail: "the contract's replay guard does not report this (action, requestId) as \
                         executed at the receipt's own block — the transaction succeeded but did \
                         not perform this operation"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// The bridge-request side of a finalized operation.
    fn on_finalized(
        &self,
        ledger: &mut Ledger,
        tx: &RobinhoodTx,
        now: i64,
    ) -> Result<(), SettlementError> {
        match tx.kind {
            // A `GlcToRhn` payout IS the settlement: the GLC has left the
            // custody contract and reached the recipient.
            RobinhoodTxKind::Payout => {
                ledger.mark_robinhood_payout_settled(tx.request_id, now)?;
            }
            // A `RhnToGlc` settlement is the LAST step: the Goldcoin
            // payout already confirmed, and this closed the obligation
            // on-chain.
            RobinhoodTxKind::Settlement => {
                ledger.mark_robinhood_settlement_confirmed(tx.request_id, now)?;
                ledger.mark_robinhood_observation_settled(tx.request_id, now)?;
            }
            RobinhoodTxKind::Refund => {
                ledger.mark_robinhood_refund_confirmed(tx.request_id, now)?;
            }
        }
        Ok(())
    }

    /// The bridge-request side of a reverted operation.
    ///
    /// Every kind lands the REQUEST in `ManualReview` too, not only the
    /// transaction row: a reverted payout means a user is owed money that
    /// did not move, a reverted settlement means an obligation is still
    /// refundable after its Goldcoin payout confirmed, and a reverted
    /// refund means a depositor's principal is still held. All three need
    /// a human, and none of them may be retried by a loop.
    fn on_reverted(
        &self,
        ledger: &mut Ledger,
        tx: &RobinhoodTx,
        now: i64,
    ) -> Result<(), SettlementError> {
        ledger.mark_robinhood_request_manual_review(
            tx.request_id,
            &format!("its {} transaction reverted on Robinhood", tx.kind.as_str()),
            now,
        )?;
        Ok(())
    }
}

/// What one receipt poll concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptOutcome {
    Pending,
    Included,
    Finalized,
    Reverted,
    ManualReview,
}

#[cfg(test)]
mod tests;
