//! Robinhood reserve withdrawal: moving GLC from the custody contract to
//! its immutable `TREASURY`, on the strength of an APPROVED
//! `rebalance_requests` row.
//!
//! # Where this sits
//!
//! The EVM counterpart of `glc-treasury-withdraw` (Solana), built on the
//! machinery the three existing outbound operations already use rather
//! than beside it: the same `robinhood_transactions` row (a fourth
//! [`RobinhoodTxKind`]), the same nonce owner, the same signed-bytes
//! persistence, the same broadcast/replacement/receipt phases in
//! [`Settler`], the same 2-of-3 quorum through the same signer policy.
//! What is new is the payload ([`TreasuryWithdrawAuth`]), the gate
//! ([`ContractGate::check_treasury_withdraw`]) and the ledger row it
//! settles (a rebalance request, not a bridge request).
//!
//! # What an operator supplies, and what they do not
//!
//! A rebalance id. That is all. The AMOUNT is the approved request's
//! (canonical 8dp, widened exactly to the contract's 18dp); the
//! DESTINATION is the contract's own `treasury()`, read live and never
//! accepted as an argument; the PAUSE STATE is read from the contract
//! and never asserted by a flag. This mirrors the Solana tool, which
//! removed its `--destination` after the 2026-09-02 incident and has not
//! had one since.
//!
//! # Idempotent by construction
//!
//! [`begin`] on a rebalance that already has an operation row returns
//! that row. The row owns the nonce, the signed bytes and the receipt
//! state; [`Settler::tick_broadcast`] re-sends identical bytes for a
//! `Signed`/`Broadcast` row and [`Settler::tick_receipts`] polls it. A
//! second withdrawal for the same approval is a unique-index violation
//! (`ux_robinhood_tx_rebalance`), not a second transfer.

use serde::Serialize;

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::amount_conversion::CanonicalAtomic;
use crate::evm::{EvmAddress, EvmU256};
use crate::ledger::{
    BeginTxOutcome, Ledger, LedgerError, NewRobinhoodTx, RebalanceKind, RebalanceRequest,
    RebalanceState, ReserveDirection, RobinhoodTx, RobinhoodTxKind, RobinhoodTxState,
};

use super::admin::{self, Check};
use super::auth::{self, TreasuryWithdrawAuth, ACTION_TREASURY_WITHDRAW};
use super::calls::{self, BridgeReader, GateError, GateRefusal, TokenReader};
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmRpc, EvmSubmitRpc};
use super::settlement::{SettlementError, SettlementReport, Settler};

/// Why a treasury withdrawal could not be begun.
#[derive(Debug, thiserror::Error)]
pub enum TreasuryWithdrawError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Settlement(Box<SettlementError>),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error("rebalance request {rebalance_id} not found")]
    NotFound { rebalance_id: i64 },
    #[error(
        "rebalance request {rebalance_id} is a {direction} {kind}; only a RobinhoodReserve \
         Withdraw can be executed here"
    )]
    NotARobinhoodWithdrawal {
        rebalance_id: i64,
        direction: &'static str,
        kind: &'static str,
    },
    #[error(
        "rebalance request {rebalance_id} is in state {state}, not Approved. A withdrawal is \
         executed from Approved — after the request's required_approvals were collected with \
         `rebalance-approve` — and from nothing else"
    )]
    NotApproved { rebalance_id: i64, state: String },
    #[error("the approved amount {canonical} canonical units cannot be widened to 18dp: {detail}")]
    Amount { canonical: u64, detail: String },
    #[error("REFUSING — {0}")]
    Refused(String),
}

impl From<SettlementError> for TreasuryWithdrawError {
    fn from(e: SettlementError) -> Self {
        TreasuryWithdrawError::Settlement(Box::new(e))
    }
}

/// The contract-side facts a withdrawal is judged against, read at one
/// block. Read-only; needs no key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnchainContext {
    pub treasury: EvmAddress,
    pub deposits_paused: bool,
    pub payouts_paused: bool,
    pub migrated: bool,
    pub signer_epoch: u64,
    /// `balanceOf(bridge)`, 18dp.
    pub reserve_balance: EvmU256,
    /// `encumberedReserve()` = `protectedMinReserve + outstandingRefundablePrincipal`, 18dp.
    pub encumbered: EvmU256,
    /// `limits().protectedMinReserve`, 18dp.
    pub protected_min_reserve: EvmU256,
    /// Whether `(ACTION_TREASURY_WITHDRAW, requestId)` is already consumed
    /// — only meaningful once a request id exists; `None` before one does.
    pub already_executed: Option<bool>,
}

/// Reads [`OnchainContext`] from the deployment. `request_id` lets the
/// replay guard be read for a resumed operation.
pub async fn read_onchain<R: EvmCallRpc>(
    rpc: &R,
    bridge: EvmAddress,
    token: EvmAddress,
    request_id: Option<[u8; 32]>,
) -> Result<OnchainContext, calls::ContractReadError> {
    let reader = BridgeReader::new(bridge);
    let block = EvmBlockTag::Latest;
    let already_executed = match request_id {
        Some(id) => Some(
            reader
                .request_executed(rpc, ACTION_TREASURY_WITHDRAW, id, block)
                .await?,
        ),
        None => None,
    };
    Ok(OnchainContext {
        treasury: reader.treasury(rpc, block).await?,
        deposits_paused: reader.deposits_paused(rpc, block).await?,
        payouts_paused: reader.payouts_paused(rpc, block).await?,
        migrated: reader.migrated(rpc, block).await?,
        signer_epoch: reader.signer_epoch(rpc, block).await?,
        reserve_balance: TokenReader::new(token)
            .balance_of(rpc, bridge, block)
            .await?,
        encumbered: reader.encumbered_reserve(rpc, block).await?,
        protected_min_reserve: reader.limits(rpc, block).await?.protected_min_reserve,
        already_executed,
    })
}

/// Everything the dry run prints and the execute path re-verifies.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub rebalance: RebalanceRequest,
    /// The approved amount, widened. `None` only when widening failed —
    /// which a check below then reports.
    pub amount_robinhood: Option<RobinhoodAtomic>,
    /// The ledger's Robinhood reserve report, when the reserve is
    /// configured.
    pub ledger_reserve: Option<admin::ReserveReport>,
    pub onchain: Option<OnchainContext>,
    /// The operation already begun for this rebalance, if any.
    pub existing: Option<RobinhoodTx>,
    pub checks: Vec<Check>,
    /// How many authorization signers this deployment has configured.
    pub configured_signers: usize,
}

impl Assessment {
    pub fn eligible(&self) -> bool {
        admin::all_pass(&self.checks)
    }
}

/// The full check list, in the order an operator wants to read it. Pure
/// over its inputs: no I/O, so the same function backs the dry run and
/// the tests.
pub fn assess(
    ledger: &Ledger,
    rebalance_id: i64,
    onchain: Option<OnchainContext>,
    configured_signers: usize,
    now: i64,
) -> Result<Assessment, TreasuryWithdrawError> {
    let rebalance = ledger
        .get_rebalance(rebalance_id)?
        .ok_or(TreasuryWithdrawError::NotFound { rebalance_id })?;
    let existing = ledger.get_robinhood_tx_for_rebalance(rebalance_id)?;
    let ledger_reserve = admin::reserve_report(ledger, now)?;

    let mut checks = Vec::new();

    // ---- the request itself ----
    let is_robinhood_withdraw = rebalance.direction == ReserveDirection::RobinhoodReserve
        && rebalance.kind == RebalanceKind::Withdraw;
    checks.push(Check::of(
        "rebalance_is_robinhood_withdraw",
        is_robinhood_withdraw,
        format!(
            "rebalance #{rebalance_id} is {} {:?}",
            rebalance.direction.as_str(),
            rebalance.kind
        ),
    ));
    // An operation already in flight is resumable from any non-terminal
    // rebalance state; a fresh one needs Approved.
    let state_ok = match &existing {
        Some(_) => matches!(
            rebalance.state,
            RebalanceState::Approved | RebalanceState::Executed | RebalanceState::Confirmed
        ),
        None => rebalance.state == RebalanceState::Approved,
    };
    checks.push(Check::of(
        "rebalance_approved",
        state_ok,
        format!(
            "state {:?}, {}/{} approvals{}",
            rebalance.state,
            rebalance.approved_by.len(),
            rebalance.required_approvals,
            existing
                .as_ref()
                .map(|tx| format!(
                    " (operation #{} already exists, state {})",
                    tx.id,
                    tx.state.as_str()
                ))
                .unwrap_or_default()
        ),
    ));

    // ---- amount: canonical 8dp -> Robinhood 18dp, exactly ----
    let amount_robinhood =
        match RobinhoodAtomic::from_canonical(CanonicalAtomic(rebalance.amount_atomic)) {
            Ok(amount) => {
                checks.push(Check::pass(
                    "amount_widens_exactly_to_18dp",
                    format!(
                        "{} canonical (8dp) = {} Robinhood atomic (18dp) = {}",
                        rebalance.amount_atomic,
                        amount,
                        crate::chain_policy::human::format_glc(rebalance.amount_atomic)
                    ),
                ));
                Some(amount)
            }
            Err(e) => {
                checks.push(Check::fail("amount_widens_exactly_to_18dp", e.to_string()));
                None
            }
        };
    checks.push(Check::of(
        "amount_positive",
        rebalance.amount_atomic > 0,
        format!("{} canonical units", rebalance.amount_atomic),
    ));

    // ---- ledger-side reserve accounting ----
    match &ledger_reserve {
        None => checks.push(Check::fail(
            "ledger_reserve_configured",
            "no [reserve.robinhood] section: the Robinhood reserve is not configured in this \
             ledger, so nothing can be accounted against it",
        )),
        Some(r) => {
            checks.push(Check::pass(
                "ledger_reserve_configured",
                format!(
                    "balance {} protected_min {} reserved {} pending {} (canonical)",
                    r.balance_atomic,
                    r.protected_minimum_atomic,
                    r.reserved_liquidity_atomic,
                    r.pending_obligations_atomic
                ),
            ));
            let amount = i128::from(rebalance.amount_atomic);
            let after = i128::from(r.balance_atomic) - amount;
            checks.push(Check::of(
                "ledger_protected_minimum_preserved",
                after >= i128::from(r.protected_minimum_atomic),
                format!(
                    "post-withdraw balance {after} vs protected minimum {}",
                    r.protected_minimum_atomic
                ),
            ));
            checks.push(Check::of(
                "ledger_reserved_liquidity_preserved",
                i128::from(r.available_capacity_atomic) >= amount,
                format!(
                    "available capacity {} (balance - protected - reserved) vs amount {amount}",
                    r.available_capacity_atomic
                ),
            ));
            checks.push(Check::of(
                "ledger_pending_obligations_preserved",
                after - i128::from(r.protected_minimum_atomic)
                    >= i128::from(r.pending_obligations_atomic),
                format!(
                    "post-withdraw headroom above the floor {} vs pending outbound obligations {}",
                    after - i128::from(r.protected_minimum_atomic),
                    r.pending_obligations_atomic
                ),
            ));
            checks.push(Check::of(
                "ledger_invariant_holds",
                r.invariant_holds,
                "balance >= protected_minimum + reserved_liquidity",
            ));
        }
    }

    // ---- contract-side, when read ----
    match &onchain {
        None => checks.push(Check::fail(
            "onchain_state_read",
            "the contract was not read — this is a ledger-only assessment",
        )),
        Some(c) => {
            checks.push(Check::pass("onchain_state_read", "read at latest block"));
            checks.push(Check::of(
                "treasury_configured",
                c.treasury != EvmAddress::ZERO,
                if c.treasury == EvmAddress::ZERO {
                    "TREASURY is the zero address: this deployment has no withdrawal capability"
                        .to_string()
                } else {
                    format!("TREASURY = {}", c.treasury.to_checksum_string())
                },
            ));
            checks.push(Check::of(
                "not_migrated",
                !c.migrated,
                format!("migrated = {}", c.migrated),
            ));
            checks.push(Check::of(
                "both_directions_paused_onchain",
                c.deposits_paused && c.payouts_paused,
                format!(
                    "depositsPaused = {}, payoutsPaused = {} — read from the contract, not from a \
                     flag",
                    c.deposits_paused, c.payouts_paused
                ),
            ));
            if let Some(amount) = amount_robinhood {
                let amount_u256 = amount.to_u256();
                let spendable = c.reserve_balance.saturating_sub(c.encumbered);
                checks.push(Check::of(
                    "onchain_spendable_reserve_covers_amount",
                    c.reserve_balance >= amount_u256 && spendable >= amount_u256,
                    format!(
                        "balanceOf(bridge) {} - encumberedReserve {} = spendable {} vs amount {} \
                         (18dp)",
                        c.reserve_balance, c.encumbered, spendable, amount_u256
                    ),
                ));
            }
            if let Some(executed) = c.already_executed {
                checks.push(Check::of(
                    "not_already_executed_onchain",
                    !executed,
                    format!("requestExecuted = {executed}"),
                ));
            }
        }
    }

    // ---- quorum ----
    checks.push(Check::of(
        "signer_quorum_configured",
        configured_signers >= super::signer::SIGNER_THRESHOLD,
        format!(
            "{configured_signers} authorization signer(s) configured, {} required",
            super::signer::SIGNER_THRESHOLD
        ),
    ));

    Ok(Assessment {
        rebalance,
        amount_robinhood,
        ledger_reserve,
        onchain,
        existing,
        checks,
        configured_signers,
    })
}

/// Begins (or resumes) the withdrawal for one approved rebalance request.
///
/// Re-runs every check against FRESH state — the assessment the dry run
/// printed was a preview, never a precondition — and then:
///
/// 1. reads `treasury()` and `signerEpoch()` from the contract;
/// 2. derives the request id from the approval's durable identity;
/// 3. writes the `Authorizing` row (or finds the existing one);
/// 4. if, and only if, the row is still `Authorizing`, collects the
///    2-of-3 quorum and stores it.
///
/// Returns the operation id. Nothing is signed with the submitter key or
/// broadcast here; that is [`Settler::tick_broadcast`]'s job, driven by
/// [`drive`].
pub async fn begin<R>(
    settler: &Settler<R>,
    ledger: &mut Ledger,
    rebalance_id: i64,
    now: i64,
) -> Result<i64, TreasuryWithdrawError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    // Resume first. An existing row is the normal result of a re-run.
    if let Some(existing) = ledger.get_robinhood_tx_for_rebalance(rebalance_id)? {
        if existing.state == RobinhoodTxState::Authorizing {
            let payload = rebuild_treasury_withdraw_auth(settler, &existing)?;
            let authorization =
                auth::EvmAuthRequest::treasury_withdraw(settler.deployment().domain(), payload);
            let quorum = super::signer::collect_quorum(
                settler.signers_ref(),
                &settler.deployment().signers,
                &authorization,
                settler.signer_timeout(),
            )
            .await
            .map_err(|e| TreasuryWithdrawError::Settlement(Box::new(SettlementError::Quorum(e))))?;
            ledger.record_robinhood_authorization(existing.id, &quorum.for_storage(), now)?;
        }
        return Ok(existing.id);
    }

    let deployment = settler.deployment();
    let onchain = read_onchain(
        settler.rpc(),
        deployment.bridge_contract,
        deployment.token,
        None,
    )
    .await
    .map_err(GateError::Read)?;
    let assessment = assess(
        ledger,
        rebalance_id,
        Some(onchain.clone()),
        settler.signers_ref().len(),
        now,
    )?;
    let rebalance = &assessment.rebalance;
    if rebalance.direction != ReserveDirection::RobinhoodReserve
        || rebalance.kind != RebalanceKind::Withdraw
    {
        return Err(TreasuryWithdrawError::NotARobinhoodWithdrawal {
            rebalance_id,
            direction: rebalance.direction.as_str(),
            kind: match rebalance.kind {
                RebalanceKind::Deposit => "Deposit",
                RebalanceKind::Withdraw => "Withdraw",
            },
        });
    }
    if rebalance.state != RebalanceState::Approved {
        return Err(TreasuryWithdrawError::NotApproved {
            rebalance_id,
            state: rebalance.state.as_str().to_string(),
        });
    }
    let amount = RobinhoodAtomic::from_canonical(CanonicalAtomic(rebalance.amount_atomic))
        .map_err(|e| TreasuryWithdrawError::Amount {
            canonical: rebalance.amount_atomic,
            detail: e.to_string(),
        })?;
    if !assessment.eligible() {
        let failed: Vec<String> = assessment
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect();
        return Err(TreasuryWithdrawError::Refused(failed.join("; ")));
    }
    // The gate proper, as the broadcast phase will run it — so a refusal
    // is named here, before any custody domain is asked to sign.
    let domain = deployment.domain();
    let identity = auth::treasury_withdrawal_identity(
        rebalance.id,
        rebalance.requested_at,
        rebalance.amount_atomic,
    );
    let contract_request_id = auth::derive_treasury_withdraw_request_id(domain, &identity);
    calls::ContractGate::new(deployment.bridge_contract)
        .check_treasury_withdraw(
            settler.rpc(),
            contract_request_id,
            onchain.signer_epoch,
            onchain.treasury,
            EvmBlockTag::Latest,
        )
        .await?;

    let expiry = (now as u64).saturating_add(settler.config_ref().authorization_ttl.as_secs());
    let payload = TreasuryWithdrawAuth {
        token: deployment.token,
        request_id: contract_request_id,
        treasury: onchain.treasury,
        amount,
        signer_epoch: onchain.signer_epoch,
        expiry,
    };
    let authorization = auth::EvmAuthRequest::treasury_withdraw(domain, payload.clone());
    let digest = authorization.digest()?;

    let outcome = ledger.begin_robinhood_tx(
        &NewRobinhoodTx {
            kind: RobinhoodTxKind::TreasuryWithdraw,
            request_id: None,
            rebalance_request_id: Some(rebalance_id),
            route: None,
            bridge_contract: deployment.bridge_contract.to_bytes(),
            chain_id: deployment.chain_id.get(),
            contract_request_id,
            obligation_index: None,
            recipient: Some(onchain.treasury.to_bytes()),
            amount_robinhood: Some(amount.to_u256().to_be_bytes()),
            signer_epoch: onchain.signer_epoch,
            expiry,
            auth_digest: digest,
        },
        now,
    )?;
    let tx_id = match outcome {
        BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
    };

    let quorum = super::signer::collect_quorum(
        settler.signers_ref(),
        &deployment.signers,
        &authorization,
        settler.signer_timeout(),
    )
    .await
    .map_err(|e| TreasuryWithdrawError::Settlement(Box::new(SettlementError::Quorum(e))))?;
    ledger.record_robinhood_authorization(tx_id, &quorum.for_storage(), now)?;
    Ok(tx_id)
}

/// Rebuilds the payload from its stored row — the shared calldata
/// builder's input, whose digest is then compared against the stored one.
pub(crate) fn rebuild_treasury_withdraw_auth<R>(
    settler: &Settler<R>,
    tx: &RobinhoodTx,
) -> Result<TreasuryWithdrawAuth, SettlementError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let err = |detail: &str| SettlementError::Operation {
        subject: tx.subject(),
        detail: detail.to_string(),
    };
    Ok(TreasuryWithdrawAuth {
        token: settler.deployment().token,
        request_id: tx.contract_request_id,
        treasury: EvmAddress::from_bytes(
            tx.recipient
                .ok_or_else(|| err("a treasury withdrawal must name its treasury"))?,
        ),
        amount: RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes(
            tx.amount_robinhood
                .ok_or_else(|| err("a treasury withdrawal must name an amount"))?,
        ))
        .map_err(|e| err(&format!("the stored amount is not usable: {e}")))?,
        signer_epoch: tx.signer_epoch,
        expiry: tx.expiry,
    })
}

/// The rebalance-request side of a finalized withdrawal.
///
/// `Approved -> Executed` with the transaction hash as the reference,
/// then `Executed -> Confirmed` with the approved amount as the observed
/// figure — which also decrements the reserve's cached balance, so the
/// next reconciliation tick sees a drop it can explain. Both on the
/// strength of a receipt this service read, verified for the bridge's
/// event and for the replay guard, and counted to depth.
pub(crate) fn on_finalized(
    ledger: &mut Ledger,
    tx: &RobinhoodTx,
    now: i64,
) -> Result<(), SettlementError> {
    let rebalance_id = tx
        .rebalance_request_id
        .ok_or_else(|| SettlementError::Operation {
            subject: tx.subject(),
            detail: "a treasury withdrawal must name its rebalance request".to_string(),
        })?;
    let reference = tx
        .tx_hash
        .map(|h| crate::evm::hex::encode_lower(&h))
        .unwrap_or_else(|| format!("robinhood-tx-{}", tx.id));
    let amount = ledger
        .get_rebalance(rebalance_id)?
        .ok_or(LedgerError::RebalanceNotFound(rebalance_id))?
        .amount_atomic;
    ledger.record_rebalance_executed(rebalance_id, &reference, "system:robinhood", now)?;
    ledger.confirm_rebalance(rebalance_id, amount, "system:robinhood", now)?;
    Ok(())
}

/// The rebalance-request side of a reverted withdrawal: `Approved ->
/// Executed -> Failed`, carrying the hash of the transaction that
/// reverted and the reason. Terminal. A fresh proposal is a fresh
/// request id.
pub(crate) fn on_reverted(
    ledger: &mut Ledger,
    tx: &RobinhoodTx,
    now: i64,
) -> Result<(), SettlementError> {
    let rebalance_id = tx
        .rebalance_request_id
        .ok_or_else(|| SettlementError::Operation {
            subject: tx.subject(),
            detail: "a treasury withdrawal must name its rebalance request".to_string(),
        })?;
    let reference = tx
        .tx_hash
        .map(|h| crate::evm::hex::encode_lower(&h))
        .unwrap_or_else(|| format!("robinhood-tx-{}", tx.id));
    let state = ledger
        .get_rebalance(rebalance_id)?
        .ok_or(LedgerError::RebalanceNotFound(rebalance_id))?
        .state;
    if state == RebalanceState::Approved {
        ledger.record_rebalance_executed(rebalance_id, &reference, "system:robinhood", now)?;
    }
    ledger.fail_rebalance(
        rebalance_id,
        &format!(
            "the executeTreasuryWithdraw transaction {reference} was mined and REVERTED; \
             operation #{} is in ManualReview",
            tx.id
        ),
        "system:robinhood",
        now,
    )?;
    Ok(())
}

/// Drives one operation to a terminal state or a deadline.
///
/// Each round runs the settler's broadcast phase (signs on the first
/// round, re-sends identical bytes or replaces at the same nonce after)
/// and its receipt phase. Returns when the row is terminal, when
/// `deadline` passes, or when the row is not moving and `poll` says stop.
/// Never returns success for anything but `Finalized`.
pub async fn drive<R, C>(
    settler: &Settler<R>,
    ledger: &mut Ledger,
    tx_id: i64,
    mut clock: C,
    deadline: i64,
    mut between_rounds: impl FnMut(),
) -> Result<Outcome, TreasuryWithdrawError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
    C: FnMut() -> i64,
{
    let mut report = SettlementReport::default();
    loop {
        let now = clock();
        settler.tick_broadcast(ledger, now, &mut report).await;
        settler.tick_receipts(ledger, now, &mut report).await;
        let tx =
            ledger
                .get_robinhood_tx(tx_id)?
                .ok_or_else(|| LedgerError::RobinhoodTxInvalid {
                    id: tx_id,
                    detail: "the operation row vanished".to_string(),
                })?;
        if tx.state.is_terminal() || clock() >= deadline {
            return Ok(Outcome { tx, report });
        }
        between_rounds();
    }
}

/// What [`drive`] ended with.
#[derive(Debug)]
pub struct Outcome {
    pub tx: RobinhoodTx,
    pub report: SettlementReport,
}

/// The machine-readable result of `glc-admin robinhood-treasury-withdraw`.
///
/// `success` is true for exactly one combination: the operation row
/// `Finalized` AND the rebalance request `Confirmed`. Both, because the
/// shared receipt phase promotes the row to `Finalized` on depth and
/// only THEN verifies the operation's effect (the bridge event and the
/// replay guard); a row can therefore read `Finalized` while the
/// rebalance was never confirmed, and that is not success. A `Broadcast`
/// or `Included` row is unresolved and reports `false`; re-running the
/// command resumes it.
#[derive(Debug, Clone, Serialize)]
pub struct TreasuryWithdrawResult {
    pub operation_id: Option<i64>,
    pub rebalance_id: i64,
    /// `"DryRun"` when nothing was executed, else the [`RobinhoodTxState`].
    pub state: String,
    pub rebalance_state: String,
    pub success: bool,
    pub dry_run: bool,
    pub tx_hash: Option<String>,
    pub nonce: Option<u64>,
    /// Robinhood 18-decimal atomic units, decimal string.
    pub amount_atomic: String,
    /// Canonical 8-decimal atomic units, decimal string.
    pub amount_canonical_atomic: String,
    pub amount_glc: String,
    pub destination: Option<String>,
    pub receipt_status: Option<i64>,
    pub receipt_block_number: Option<i64>,
    pub confirmations: i64,
    pub required_confirmations: u64,
    pub failure_reason: Option<String>,
    pub checks: Vec<CheckView>,
    pub onchain: Option<OnchainView>,
    pub ledger_reserve_before: Option<LedgerReserveView>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckView {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnchainView {
    pub treasury: String,
    pub deposits_paused: bool,
    pub payouts_paused: bool,
    pub migrated: bool,
    pub signer_epoch: u64,
    pub reserve_balance_atomic: String,
    pub encumbered_reserve_atomic: String,
    pub protected_min_reserve_atomic: String,
    pub post_withdraw_reserve_atomic: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerReserveView {
    pub balance_atomic: u64,
    pub protected_minimum_atomic: u64,
    pub reserved_liquidity_atomic: u64,
    pub pending_obligations_atomic: u64,
    pub available_capacity_atomic: i64,
    pub post_withdraw_balance_atomic: i128,
    pub paused: bool,
}

impl TreasuryWithdrawResult {
    /// Builds the result from an assessment and, when one ran, the
    /// operation's final row.
    pub fn build(
        assessment: &Assessment,
        tx: Option<&RobinhoodTx>,
        rebalance_state: RebalanceState,
        required_confirmations: u64,
        dry_run: bool,
        errors: Vec<String>,
    ) -> TreasuryWithdrawResult {
        let amount_canonical = assessment.rebalance.amount_atomic;
        let amount_robinhood = assessment
            .amount_robinhood
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unrepresentable".to_string());
        let state = match tx {
            Some(tx) => tx.state.as_str().to_string(),
            None => "DryRun".to_string(),
        };
        let success = matches!(tx, Some(tx) if tx.state == RobinhoodTxState::Finalized)
            && rebalance_state == RebalanceState::Confirmed;
        let destination = tx
            .and_then(|t| t.recipient)
            .map(|r| EvmAddress::from_bytes(r).to_checksum_string())
            .or_else(|| {
                assessment
                    .onchain
                    .as_ref()
                    .map(|c| c.treasury.to_checksum_string())
            });
        TreasuryWithdrawResult {
            operation_id: tx.map(|t| t.id),
            rebalance_id: assessment.rebalance.id,
            state,
            rebalance_state: rebalance_state.as_str().to_string(),
            success,
            dry_run,
            tx_hash: tx
                .and_then(|t| t.tx_hash)
                .map(|h| crate::evm::hex::encode_lower(&h)),
            nonce: tx.and_then(|t| t.nonce),
            amount_atomic: amount_robinhood,
            amount_canonical_atomic: amount_canonical.to_string(),
            amount_glc: crate::chain_policy::human::format_glc(amount_canonical),
            destination,
            receipt_status: tx.and_then(|t| t.receipt_status),
            receipt_block_number: tx.and_then(|t| t.receipt_block_number),
            confirmations: tx.map(|t| t.confirmations).unwrap_or(0),
            required_confirmations,
            failure_reason: tx.and_then(|t| t.failure_reason.clone()),
            checks: assessment
                .checks
                .iter()
                .map(|c| CheckView {
                    name: c.name.to_string(),
                    ok: c.ok,
                    detail: c.detail.clone(),
                })
                .collect(),
            onchain: assessment.onchain.as_ref().map(|c| OnchainView {
                treasury: c.treasury.to_checksum_string(),
                deposits_paused: c.deposits_paused,
                payouts_paused: c.payouts_paused,
                migrated: c.migrated,
                signer_epoch: c.signer_epoch,
                reserve_balance_atomic: c.reserve_balance.to_string(),
                encumbered_reserve_atomic: c.encumbered.to_string(),
                protected_min_reserve_atomic: c.protected_min_reserve.to_string(),
                post_withdraw_reserve_atomic: assessment
                    .amount_robinhood
                    .map(|a| c.reserve_balance.saturating_sub(a.to_u256()).to_string()),
            }),
            ledger_reserve_before: assessment
                .ledger_reserve
                .as_ref()
                .map(|r| LedgerReserveView {
                    balance_atomic: r.balance_atomic,
                    protected_minimum_atomic: r.protected_minimum_atomic,
                    reserved_liquidity_atomic: r.reserved_liquidity_atomic,
                    pending_obligations_atomic: r.pending_obligations_atomic,
                    available_capacity_atomic: r.available_capacity_atomic,
                    post_withdraw_balance_atomic: i128::from(r.balance_atomic)
                        - i128::from(amount_canonical),
                    paused: r.paused,
                }),
            errors,
        }
    }
}

/// Names the gate refusal an operator is most likely to hit, for the
/// human summary line.
pub fn describe_refusal(refusal: &GateRefusal) -> String {
    refusal.to_string()
}

#[cfg(test)]
mod tests;
