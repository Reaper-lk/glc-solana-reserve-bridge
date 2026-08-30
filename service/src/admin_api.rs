//! The authenticated, privately-bound admin control plane
//! (docs/27-admin-control-plane.md).
//!
//! # Relationship to the public API's boundary
//!
//! [`crate::api`]'s module docs state that the public API never exposes
//! privileged admin operations — that boundary is unchanged: nothing here
//! is reachable through the public listener. This module is the
//! deliberate, separately-bound counterpart for operators: its listener
//! only starts when `service.admin_bind_addr` is configured (bind it
//! privately — localhost or an internal interface behind the operators'
//! reverse proxy, never a public address), and every request must carry a
//! per-operator bearer token ([`auth::OperatorRegistry`]).
//!
//! # What this exposes, and what it structurally cannot do
//!
//! Read-only views an operator needs (reserve health, on-chain
//! `BridgeConfig`/rolling-window state, the ManualReview backlog, the
//! rebalance workflow, the admin audit log, the fixed fee rate), plus the
//! LOCAL mutations `glc-admin` already supports through validated
//! `Ledger` logic: local pause/unpause per reserve direction, admission
//! open/close (through [`guard::open_admission_guarded`] — the same
//! invariant + UTXO-liquidity gates the CLI applies, never bypassable),
//! resume-manual-review (via `Ledger::resume_manual_review_sol_to_glc`,
//! whose internal safety checks — including the unconditional
//! source-wallet/recipient rate-limit re-checks — are never reimplemented
//! or pre-filtered here), and the rebalance request workflow
//! (`record-executed` records an out-of-band transaction reference
//! string; nothing here constructs or broadcasts anything).
//!
//! It never touches [`crate::signing`], never loads or holds any keypair,
//! and has no path that executes a command or submits a transaction. The
//! on-chain admin instructions (`set_paused`/`set_limit`/
//! `reset_rolling_volume_window`) remain CLI-only, gated by possession of
//! the admin keypair on the operator's own machine: for those, this
//! module only serves read-only state plus [`cli_command`]-generated
//! `glc-admin` command lines (labeled "CLI approval required") with the
//! atomic-unit conversions done server-side, in the same Rust code the
//! daemon itself trusts.
//!
//! Every mutation requires a non-empty `note` and is recorded —
//! successes AND refusals — in the append-only `admin_audit_log`
//! (schema v14, [`crate::ledger::Ledger::append_admin_audit`]) under the
//! operator identity the bearer token resolved to.
//!
//! # Browser sessions live elsewhere
//!
//! This API is bearer-only by design: it never sets or reads cookies and
//! never answers CORS preflight, so a browser's ambient credentials can
//! never authorize anything here (CSRF against it is structurally
//! impossible, not just mitigated). Requests carrying a `Cookie` or
//! `Origin` header are rejected outright — a legitimate caller is the
//! admin UI's server-side proxy (which holds the operator's token
//! server-side), `curl`, or another non-browser client.

pub mod auth;
pub mod cli_command;
pub mod guard;

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};

use crate::amount_conversion;
use crate::ledger::{
    AdminAuditEntry, AdminAuditFilter, AdminAuditOutcome, AdminAuditRow, Direction, Ledger,
    LedgerError, RebalanceKind, RebalanceRequest, RequestState, ReserveDirection,
};
use crate::ops::reserve_health;
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

use auth::OperatorRegistry;

// ------------------------------------------------------------- errors --

#[derive(Debug)]
pub enum AdminError {
    /// Malformed input (bad JSON, unknown direction, empty note): 400.
    BadRequest(String),
    /// The target row does not exist: 404.
    NotFound(String),
    /// A validated refusal from the underlying business logic (invariant
    /// does not hold, wrong state, rate-limited, ...): 409. The message
    /// is the same operator-facing text `glc-admin` would print.
    Conflict(String),
    /// Ledger/storage failure: 500.
    Ledger(String),
    /// The Solana RPC read failed or returned something undecodable: 503.
    Upstream(String),
}

impl AdminError {
    fn status(&self) -> StatusCode {
        match self {
            AdminError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AdminError::NotFound(_) => StatusCode::NOT_FOUND,
            AdminError::Conflict(_) => StatusCode::CONFLICT,
            AdminError::Ledger(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminError::Upstream(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminError::BadRequest(m)
            | AdminError::NotFound(m)
            | AdminError::Conflict(m)
            | AdminError::Ledger(m)
            | AdminError::Upstream(m) => f.write_str(m),
        }
    }
}

/// Every non-`Sqlite` `LedgerError` is a validated refusal whose message
/// is already written for an operator (`glc-admin` prints them verbatim
/// today) — surface those as 409. Raw storage errors are 500 and their
/// detail stays out of the response body (a SQLite message can embed the
/// database path).
impl From<LedgerError> for AdminError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::Sqlite(_) => AdminError::Ledger("ledger storage error".to_string()),
            LedgerError::RequestNotFound(id) => {
                AdminError::NotFound(format!("bridge request {id} not found"))
            }
            LedgerError::RebalanceNotFound(id) => {
                AdminError::NotFound(format!("rebalance request {id} not found"))
            }
            other => AdminError::Conflict(other.to_string()),
        }
    }
}

// ------------------------------------------------------- request types --

fn parse_reserve_direction(s: &str) -> Result<ReserveDirection, AdminError> {
    match s {
        "goldcoin" => Ok(ReserveDirection::GoldcoinReserve),
        "solana" => Ok(ReserveDirection::SolanaReserve),
        other => Err(AdminError::BadRequest(format!(
            "unknown direction {other:?} (expected goldcoin|solana)"
        ))),
    }
}

fn require_note(note: &str) -> Result<&str, AdminError> {
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return Err(AdminError::BadRequest(
            "a non-empty note is required for every admin mutation".to_string(),
        ));
    }
    Ok(trimmed)
}

#[derive(Debug, Deserialize)]
pub struct DirectionNoteInput {
    pub direction: String,
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct NoteInput {
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct RebalanceProposeInput {
    pub direction: String,
    /// `deposit` or `withdraw`.
    pub kind: String,
    pub amount_atomic: u64,
    pub required_approvals: u32,
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct RebalanceRecordExecutedInput {
    pub tx_reference: String,
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct RebalanceConfirmInput {
    pub observed_amount_atomic: u64,
    pub note: String,
}

// ------------------------------------------------------ response types --

/// One mutation's receipt: what the audit log now holds for it.
#[derive(Debug, Serialize)]
pub struct MutationReceipt {
    pub audit_id: i64,
    pub action: String,
    pub target: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DirectionStatusView {
    pub direction: String,
    pub paused: bool,
    pub pause_reason: Option<String>,
    pub admission_closed: bool,
    pub admission_reason: Option<String>,
    pub manual_review_count: usize,
}

#[derive(Debug, Serialize)]
pub struct AdminStatusView {
    pub glc_to_sol: DirectionStatusView,
    pub sol_to_glc: DirectionStatusView,
    pub post_finality_reorg_events: i64,
}

#[derive(Debug, Serialize)]
pub struct ReserveHealthView {
    pub direction: String,
    /// Native reserve unit: 8-decimal Goldcoin atomic for `goldcoin`,
    /// 6-decimal mint atomic for `solana` (docs/09-runbook.md's unit
    /// trap).
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub reserved_liquidity: u64,
    pub pending_obligations: u64,
    pub accrued_fees: u64,
    pub immature_vault_utxo_total: u64,
    pub mature_available_atomic: u64,
    pub available_utxo_count: u32,
    pub utxo_pool_warning: bool,
    pub paused: bool,
    pub admission_closed: bool,
    pub invariant_holds: bool,
}

#[derive(Debug, Serialize)]
pub struct RollingWindowView {
    /// `glc-to-sol` (release window) or `sol-to-glc` (deposit window).
    pub window: String,
    pub window_start: i64,
    pub window_total: u64,
    /// Remaining capacity in the CURRENT bucket, mirroring the on-chain
    /// arithmetic exactly (`accounts::rolling_volume_remaining`).
    pub remaining: u64,
}

#[derive(Debug, Serialize)]
pub struct OnchainView {
    pub paused: bool,
    pub release_paused: bool,
    pub deposit_paused: bool,
    /// All limits in 6-decimal Solana mint atomic units.
    pub min_transfer_amount: u64,
    pub per_transfer_limit: u64,
    pub protected_minimum: u64,
    pub rolling_volume_limit: u64,
    pub rolling_window_seconds: i64,
    pub obligation_count: u64,
    pub rolling_windows: Vec<RollingWindowView>,
}

/// The fee is a compile-time constant (docs/20-bridge-fee.md's
/// "Staged fee-change process") — this view is deliberately read-only
/// and there is no endpoint that can change it.
#[derive(Debug, Serialize)]
pub struct FeeView {
    pub bridge_fee_bps: u64,
    pub bridge_fee_percent_display: String,
    pub provenance: &'static str,
}

pub fn fee_view() -> FeeView {
    FeeView {
        bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
        bridge_fee_percent_display: cli_command::format_atomic_as_decimal_string(
            amount_conversion::BRIDGE_FEE_BPS,
            2,
        ),
        provenance: "Compile-time setting — requires code deployment to change",
    }
}

#[derive(Debug, Serialize)]
pub struct ManualReviewItemView {
    pub request_id: i64,
    pub direction: String,
    pub reason: Option<String>,
    /// Canonical (8-decimal) units.
    pub gross_amount_atomic: u64,
    pub net_amount_atomic: u64,
    pub created_at: i64,
    /// Unix time until which the SolToGlc recipient rate limit would
    /// refuse a resume, when one applies right now.
    pub recipient_rate_limited_until: Option<i64>,
    /// Unix time until which the SolToGlc source-wallet rate limit would
    /// refuse a resume, when one applies right now.
    pub source_wallet_rate_limited_until: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ManualReviewView {
    pub requests: Vec<ManualReviewItemView>,
}

#[derive(Debug, Serialize)]
pub struct RebalanceView {
    pub id: i64,
    pub direction: String,
    pub kind: String,
    pub amount_atomic: u64,
    pub state: String,
    pub reason: String,
    pub requested_by: String,
    pub requested_at: i64,
    pub required_approvals: u32,
    pub approved_by: Vec<String>,
    pub approved_at: Option<i64>,
    pub tx_reference: Option<String>,
    pub executed_at: Option<i64>,
    pub observed_amount_atomic: Option<u64>,
    pub confirmed_at: Option<i64>,
    pub failure_reason: Option<String>,
}

impl RebalanceView {
    fn from_request(r: RebalanceRequest) -> Self {
        RebalanceView {
            id: r.id,
            direction: format!("{:?}", r.direction),
            kind: format!("{:?}", r.kind),
            amount_atomic: r.amount_atomic,
            state: format!("{:?}", r.state),
            reason: r.reason,
            requested_by: r.requested_by,
            requested_at: r.requested_at,
            required_approvals: r.required_approvals,
            approved_by: r.approved_by,
            approved_at: r.approved_at,
            tx_reference: r.tx_reference,
            executed_at: r.executed_at,
            observed_amount_atomic: r.observed_amount_atomic,
            confirmed_at: r.confirmed_at,
            failure_reason: r.failure_reason,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RebalanceStatusView {
    pub direction: String,
    pub severity: String,
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub target_reserve: u64,
    pub warning_reserve: u64,
    pub critical_reserve: u64,
    pub suggested_deposit_atomic: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RebalancesView {
    pub assessments: Vec<RebalanceStatusView>,
    pub requests: Vec<RebalanceView>,
}

#[derive(Debug, Serialize)]
pub struct AuditRowView {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub note: String,
    pub outcome: String,
    pub error: Option<String>,
}

impl AuditRowView {
    fn from_row(r: AdminAuditRow) -> Self {
        let (outcome, error) = match r.outcome {
            AdminAuditOutcome::Success => ("success".to_string(), None),
            AdminAuditOutcome::Error(e) => ("error".to_string(), Some(e)),
        };
        AuditRowView {
            id: r.id,
            at: r.at,
            actor: r.actor,
            action: r.action,
            target: r.target,
            old_value: r.old_value,
            new_value: r.new_value,
            note: r.note,
            outcome,
            error,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditLogView {
    pub rows: Vec<AuditRowView>,
}

#[derive(Debug, Serialize)]
pub struct WhoamiView {
    pub operator: String,
}

// -------------------------------------------------------------- trait --

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The admin API's data/mutation boundary, mirroring
/// [`crate::api::ApiSource`]'s shape so the HTTP layer (routing, auth,
/// JSON) is testable against a stub. Mutating methods take the
/// authenticated operator name as `actor` and must record the attempt in
/// the admin audit log — success or refusal — before returning.
pub trait AdminSource: Send + Sync + 'static {
    fn status(&self) -> BoxFut<'_, Result<AdminStatusView, AdminError>>;
    fn reserve_health(&self) -> BoxFut<'_, Result<Vec<ReserveHealthView>, AdminError>>;
    fn onchain(&self) -> BoxFut<'_, Result<OnchainView, AdminError>>;
    fn manual_review(&self) -> BoxFut<'_, Result<ManualReviewView, AdminError>>;
    fn set_local_pause(
        &self,
        direction: ReserveDirection,
        paused: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn set_admission(
        &self,
        direction: ReserveDirection,
        closed: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn resume_manual_review(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalances(&self) -> BoxFut<'_, Result<RebalancesView, AdminError>>;
    fn rebalance(&self, id: i64) -> BoxFut<'_, Result<RebalanceView, AdminError>>;
    fn rebalance_propose(
        &self,
        input: RebalanceProposeInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_approve(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_reject(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_cancel(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_record_executed(
        &self,
        id: i64,
        input: RebalanceRecordExecutedInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_confirm(
        &self,
        id: i64,
        input: RebalanceConfirmInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn rebalance_fail(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>>;
    fn audit_log(&self, filter: AdminAuditFilter) -> BoxFut<'_, Result<AuditLogView, AdminError>>;
    fn cli_command(
        &self,
        input: cli_command::CliCommandInput,
    ) -> BoxFut<'_, Result<cli_command::CliCommandView, AdminError>>;
}

// ------------------------------------------------------- real impl --

/// The real [`AdminSource`]: a fresh `Ledger` connection per call (the
/// same `BEGIN IMMEDIATE`-based concurrency model `api::BridgeApi` and
/// `ops::OpsCollector` use) plus a live Solana RPC for the read-only
/// on-chain views. Holds no keys and no signer handles, by construction.
pub struct AdminApi<SR: SolanaRpc> {
    db_path: PathBuf,
    rpc: SR,
}

impl<SR: SolanaRpc> AdminApi<SR> {
    pub fn new(db_path: PathBuf, rpc: SR) -> Self {
        AdminApi { db_path, rpc }
    }

    fn open_ledger(&self) -> Result<Ledger, AdminError> {
        Ledger::open(&self.db_path)
            .map_err(|_| AdminError::Ledger("could not open ledger".to_string()))
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Runs one mutation with the standard audit discipline: capture
    /// `old_value` first, attempt the mutation, then append exactly one
    /// audit row recording what happened — the row is written on the
    /// refusal path too. The receipt carries the audit row id so the UI
    /// can link straight to it.
    #[allow(clippy::too_many_arguments)]
    fn audited_mutation(
        ledger: &mut Ledger,
        actor: &str,
        action: &str,
        target: &str,
        note: &str,
        old_value: Option<String>,
        new_value: Option<String>,
        result: Result<(), AdminError>,
    ) -> Result<MutationReceipt, AdminError> {
        let outcome = match &result {
            Ok(()) => AdminAuditOutcome::Success,
            Err(e) => AdminAuditOutcome::Error(e.to_string()),
        };
        let audit_id = ledger
            .append_admin_audit(&AdminAuditEntry {
                at: Self::now(),
                actor: actor.to_string(),
                action: action.to_string(),
                target: Some(target.to_string()),
                old_value: old_value.clone(),
                new_value: new_value.clone(),
                note: note.to_string(),
                outcome,
            })
            .map_err(AdminError::from)?;
        result.map(|()| MutationReceipt {
            audit_id,
            action: action.to_string(),
            target: target.to_string(),
            old_value,
            new_value,
        })
    }

    async fn fetch_onchain(&self) -> Result<OnchainView, AdminError> {
        let config_account = self
            .rpc
            .get_account(&accounts::bridge_config_pda())
            .await
            .map_err(|e| AdminError::Upstream(format!("bridge config read failed: {e}")))?
            .ok_or_else(|| AdminError::Upstream("bridge config account not found".to_string()))?;
        let config = accounts::decode_bridge_config(&config_account.data)
            .map_err(|e| AdminError::Upstream(format!("bridge config decode failed: {e}")))?;

        let now = Self::now();
        let mut rolling_windows = Vec::with_capacity(2);
        for (byte, name) in [(0u8, "glc-to-sol"), (1u8, "sol-to-glc")] {
            let account = self
                .rpc
                .get_account(&accounts::rolling_volume_window_pda(byte))
                .await
                .map_err(|e| AdminError::Upstream(format!("rolling window read failed: {e}")))?
                .ok_or_else(|| {
                    AdminError::Upstream("rolling window account not found".to_string())
                })?;
            let window = accounts::decode_rolling_volume_window(&account.data)
                .map_err(|e| AdminError::Upstream(format!("rolling window decode failed: {e}")))?;
            let remaining = accounts::rolling_volume_remaining(
                config.rolling_volume_limit,
                config.rolling_window_seconds,
                window,
                now,
            );
            rolling_windows.push(RollingWindowView {
                window: name.to_string(),
                window_start: window.window_start,
                window_total: window.window_total,
                remaining,
            });
        }

        Ok(OnchainView {
            paused: config.paused,
            release_paused: config.release_paused,
            deposit_paused: config.deposit_paused,
            min_transfer_amount: config.min_transfer_amount,
            per_transfer_limit: config.per_transfer_limit,
            protected_minimum: config.protected_minimum,
            rolling_volume_limit: config.rolling_volume_limit,
            rolling_window_seconds: config.rolling_window_seconds,
            obligation_count: config.obligation_count,
            rolling_windows,
        })
    }
}

fn direction_name(direction: ReserveDirection) -> &'static str {
    match direction {
        ReserveDirection::GoldcoinReserve => "goldcoin",
        ReserveDirection::SolanaReserve => "solana",
    }
}

impl<SR: SolanaRpc + Send + Sync + 'static> AdminSource for AdminApi<SR> {
    fn status(&self) -> BoxFut<'_, Result<AdminStatusView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut views = Vec::new();
            for (reserve, transfer_direction) in [
                (ReserveDirection::SolanaReserve, Direction::GlcToSol),
                (ReserveDirection::GoldcoinReserve, Direction::SolToGlc),
            ] {
                let snapshot = reserve_health::check(&ledger, reserve)?;
                let manual_review = ledger
                    .requests_by_state(transfer_direction, RequestState::ManualReview)?
                    .len();
                views.push(DirectionStatusView {
                    direction: direction_name(reserve).to_string(),
                    paused: snapshot.paused,
                    pause_reason: ledger.pause_reason(reserve)?,
                    admission_closed: snapshot.admission_closed,
                    admission_reason: ledger.admission_reason(reserve)?,
                    manual_review_count: manual_review,
                });
            }
            let sol_to_glc = views.pop().expect("two views were pushed");
            let glc_to_sol = views.pop().expect("two views were pushed");
            Ok(AdminStatusView {
                glc_to_sol,
                sol_to_glc,
                post_finality_reorg_events: ledger.post_finality_reorg_event_count()?,
            })
        })
    }

    fn reserve_health(&self) -> BoxFut<'_, Result<Vec<ReserveHealthView>, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut out = Vec::with_capacity(2);
            for direction in [
                ReserveDirection::GoldcoinReserve,
                ReserveDirection::SolanaReserve,
            ] {
                let s = reserve_health::check(&ledger, direction)?;
                out.push(ReserveHealthView {
                    direction: direction_name(direction).to_string(),
                    total_reserve_balance: s.total_reserve_balance,
                    protected_minimum: s.protected_minimum,
                    reserved_liquidity: s.reserved_liquidity,
                    pending_obligations: s.pending_obligations,
                    accrued_fees: s.accrued_fees,
                    immature_vault_utxo_total: s.immature_vault_utxo_total,
                    mature_available_atomic: s.utxo_pool.mature_available_atomic,
                    available_utxo_count: s.utxo_pool.available_utxo_count,
                    utxo_pool_warning: s.utxo_pool_warning,
                    paused: s.paused,
                    admission_closed: s.admission_closed,
                    invariant_holds: s.invariant_holds,
                });
            }
            Ok(out)
        })
    }

    fn onchain(&self) -> BoxFut<'_, Result<OnchainView, AdminError>> {
        Box::pin(self.fetch_onchain())
    }

    fn manual_review(&self) -> BoxFut<'_, Result<ManualReviewView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let now = Self::now();
            let mut requests = Vec::new();
            for direction in [Direction::SolToGlc, Direction::GlcToSol] {
                for req in ledger.requests_by_state(direction, RequestState::ManualReview)? {
                    // Rate-limit context via the SAME Ledger reads the
                    // public eligibility endpoint uses — never a second
                    // implementation of the window arithmetic.
                    let (recipient_until, wallet_until) = if direction == Direction::SolToGlc {
                        let recipient_until =
                            ledger.sol_to_glc_recipient_rate_limited_until(&req.recipient, now)?;
                        let wallet_until = match &req.requester {
                            Some(wallet) => {
                                ledger.sol_to_glc_source_wallet_rate_limited_until(wallet, now)?
                            }
                            None => None,
                        };
                        (recipient_until, wallet_until)
                    } else {
                        (None, None)
                    };
                    requests.push(ManualReviewItemView {
                        request_id: req.id,
                        direction: format!("{direction:?}"),
                        reason: req.manual_review_note.clone(),
                        gross_amount_atomic: req.gross_amount_atomic,
                        net_amount_atomic: req.net_amount_atomic,
                        created_at: req.created_at,
                        recipient_rate_limited_until: recipient_until,
                        source_wallet_rate_limited_until: wallet_until,
                    });
                }
            }
            Ok(ManualReviewView { requests })
        })
    }

    fn set_local_pause(
        &self,
        direction: ReserveDirection,
        paused: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let old = ledger.is_paused(direction)?;
            let result = ledger
                .set_paused(direction, paused, Some(&note))
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                if paused { "pause" } else { "unpause" },
                direction_name(direction),
                &note,
                Some(format!("paused={old}")),
                Some(format!("paused={paused}")),
                result,
            )
        })
    }

    fn set_admission(
        &self,
        direction: ReserveDirection,
        closed: bool,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            // Same restriction as `glc-admin close-admission`/
            // `open-admission`: only the Goldcoin direction implements
            // admission control in this version.
            if direction != ReserveDirection::GoldcoinReserve {
                return Err(AdminError::BadRequest(
                    "admission control is only implemented for direction goldcoin in this version"
                        .to_string(),
                ));
            }
            let old = ledger.is_admission_closed(direction)?;
            let result = if closed {
                ledger
                    .set_admission(direction, true, Some(&note))
                    .map_err(AdminError::from)
            } else {
                guard::open_admission_guarded(&mut ledger, direction, &note)
                    .map_err(AdminError::Conflict)
            };
            Self::audited_mutation(
                &mut ledger,
                &actor,
                if closed {
                    "admission_close"
                } else {
                    "admission_open"
                },
                direction_name(direction),
                &note,
                Some(format!("admission_closed={old}")),
                Some(format!("admission_closed={closed}")),
                result,
            )
        })
    }

    fn resume_manual_review(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let old_state = ledger
                .get_request(request_id)?
                .map(|r| format!("{:?}", r.state));
            // Called as-is: every safety check (SolToGlc-only, state,
            // reason whitelist, no-payout, capacity, and the
            // unconditional source-wallet/recipient rate-limit re-checks)
            // lives INSIDE this Ledger method and is never re-implemented
            // or pre-filtered here.
            let result = ledger
                .resume_manual_review_sol_to_glc(request_id, &note, "operator", Self::now())
                .map(|_| ())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "resume_manual_review",
                &request_id.to_string(),
                &note,
                old_state,
                Some("SourceFinalized".to_string()),
                result,
            )
        })
    }

    fn rebalances(&self) -> BoxFut<'_, Result<RebalancesView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut assessments = Vec::with_capacity(2);
            for direction in [
                ReserveDirection::GoldcoinReserve,
                ReserveDirection::SolanaReserve,
            ] {
                let a = crate::rebalance::assess(&ledger, direction)?;
                assessments.push(RebalanceStatusView {
                    direction: direction_name(direction).to_string(),
                    severity: format!("{:?}", a.severity),
                    total_reserve_balance: a.total_reserve_balance,
                    protected_minimum: a.protected_minimum,
                    target_reserve: a.target_reserve,
                    warning_reserve: a.warning_reserve,
                    critical_reserve: a.critical_reserve,
                    suggested_deposit_atomic: a.suggested_deposit_atomic,
                });
            }
            let requests = ledger
                .list_rebalances(None, false)?
                .into_iter()
                .map(RebalanceView::from_request)
                .collect();
            Ok(RebalancesView {
                assessments,
                requests,
            })
        })
    }

    fn rebalance(&self, id: i64) -> BoxFut<'_, Result<RebalanceView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            ledger
                .get_rebalance(id)?
                .map(RebalanceView::from_request)
                .ok_or_else(|| AdminError::NotFound(format!("rebalance request {id} not found")))
        })
    }

    fn rebalance_propose(
        &self,
        input: RebalanceProposeInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let direction = parse_reserve_direction(&input.direction)?;
            let kind = match input.kind.as_str() {
                "deposit" => RebalanceKind::Deposit,
                "withdraw" => RebalanceKind::Withdraw,
                other => {
                    return Err(AdminError::BadRequest(format!(
                        "unknown kind {other:?} (expected deposit|withdraw)"
                    )))
                }
            };
            let mut ledger = self.open_ledger()?;
            let result = ledger
                .propose_rebalance(
                    direction,
                    kind,
                    input.amount_atomic,
                    &input.note,
                    &actor,
                    input.required_approvals,
                    Self::now(),
                )
                .map_err(AdminError::from);
            let (target, result) = match result {
                Ok(id) => (id.to_string(), Ok(())),
                Err(e) => ("new".to_string(), Err(e)),
            };
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_propose",
                &target,
                &input.note,
                None,
                Some(format!(
                    "{:?} {:?} amount={}",
                    direction, kind, input.amount_atomic
                )),
                result,
            )
        })
    }

    fn rebalance_approve(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let result = ledger
                .approve_rebalance(id, &actor, Self::now())
                .map(|_| ())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_approve",
                &id.to_string(),
                &note,
                None,
                None,
                result,
            )
        })
    }

    fn rebalance_reject(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let result = ledger
                .reject_rebalance(id, &note, &actor, Self::now())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_reject",
                &id.to_string(),
                &note,
                None,
                None,
                result,
            )
        })
    }

    fn rebalance_cancel(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let result = ledger
                .cancel_rebalance(id, &note, &actor, Self::now())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_cancel",
                &id.to_string(),
                &note,
                None,
                None,
                result,
            )
        })
    }

    fn rebalance_record_executed(
        &self,
        id: i64,
        input: RebalanceRecordExecutedInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            // Records evidence of a transaction the operator already
            // executed through real custody tooling outside this system —
            // this never constructs, signs, or broadcasts anything.
            let result = ledger
                .record_rebalance_executed(id, &input.tx_reference, &actor, Self::now())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_record_executed",
                &id.to_string(),
                &input.note,
                None,
                Some(format!("tx_reference={}", input.tx_reference)),
                result,
            )
        })
    }

    fn rebalance_confirm(
        &self,
        id: i64,
        input: RebalanceConfirmInput,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let result = ledger
                .confirm_rebalance(id, input.observed_amount_atomic, &actor, Self::now())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_confirm",
                &id.to_string(),
                &input.note,
                None,
                Some(format!("observed_amount={}", input.observed_amount_atomic)),
                result,
            )
        })
    }

    fn rebalance_fail(
        &self,
        id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<MutationReceipt, AdminError>> {
        Box::pin(async move {
            let mut ledger = self.open_ledger()?;
            let result = ledger
                .fail_rebalance(id, &note, &actor, Self::now())
                .map_err(AdminError::from);
            Self::audited_mutation(
                &mut ledger,
                &actor,
                "rebalance_fail",
                &id.to_string(),
                &note,
                None,
                None,
                result,
            )
        })
    }

    fn audit_log(&self, filter: AdminAuditFilter) -> BoxFut<'_, Result<AuditLogView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let rows = ledger
                .list_admin_audit(&filter)?
                .into_iter()
                .map(AuditRowView::from_row)
                .collect();
            Ok(AuditLogView { rows })
        })
    }

    fn cli_command(
        &self,
        input: cli_command::CliCommandInput,
    ) -> BoxFut<'_, Result<cli_command::CliCommandView, AdminError>> {
        Box::pin(async move {
            let onchain = self.fetch_onchain().await?;
            cli_command::generate(&input, &onchain).map_err(AdminError::BadRequest)
        })
    }
}

// ------------------------------------------------------------- router --

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .expect("well-formed response")
}

fn error_response(err: AdminError) -> Response<Full<Bytes>> {
    json_response(
        err.status(),
        &ErrorBody {
            error: err.to_string(),
        },
    )
}

fn unauthorized() -> Response<Full<Bytes>> {
    json_response(
        StatusCode::UNAUTHORIZED,
        &ErrorBody {
            error: "missing or invalid bearer token".to_string(),
        },
    )
}

async fn read_json<T: serde::de::DeserializeOwned>(
    req: Request<hyper::body::Incoming>,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return Err(Box::new(json_response(
                StatusCode::BAD_REQUEST,
                &ErrorBody {
                    error: "could not read request body".to_string(),
                },
            )))
        }
    };
    serde_json::from_slice::<T>(&body).map_err(|e| {
        Box::new(json_response(
            StatusCode::BAD_REQUEST,
            &ErrorBody {
                error: format!("malformed request body: {e}"),
            },
        ))
    })
}

fn parse_audit_query(query: Option<&str>) -> Result<AdminAuditFilter, AdminError> {
    let mut filter = AdminAuditFilter::default();
    let Some(q) = query else {
        return Ok(filter);
    };
    for pair in q.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        if value.is_empty() {
            continue;
        }
        match key {
            "before_id" => {
                filter.before_id = Some(value.parse::<i64>().map_err(|_| {
                    AdminError::BadRequest("before_id must be an integer".to_string())
                })?);
            }
            "limit" => {
                filter.limit = Some(value.parse::<u32>().map_err(|_| {
                    AdminError::BadRequest("limit must be a positive integer".to_string())
                })?);
            }
            "action" => filter.action = Some(value.to_string()),
            "actor" => filter.actor = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(filter)
}

/// `/rebalances/{id}` and `/rebalances/{id}/{verb}` path parsing.
fn parse_rebalance_path(path: &str) -> Option<(i64, Option<&str>)> {
    let rest = path.strip_prefix("/rebalances/")?;
    let mut parts = rest.splitn(2, '/');
    let id = parts.next()?.parse::<i64>().ok()?;
    Some((id, parts.next()))
}

/// `/manual-review/{id}/resume` path parsing.
fn parse_manual_review_resume_path(path: &str) -> Option<i64> {
    let rest = path.strip_prefix("/manual-review/")?;
    let id = rest.strip_suffix("/resume")?;
    id.parse::<i64>().ok()
}

async fn handle<S: AdminSource>(
    req: Request<hyper::body::Incoming>,
    source: Arc<S>,
    registry: Arc<OperatorRegistry>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Structural anti-CSRF: this API is for non-browser callers only. A
    // request carrying browser-ambient credentials (`Cookie`) or a
    // browser-stamped `Origin` header is refused before authentication is
    // even considered — see the module docs.
    if req.headers().contains_key(hyper::header::COOKIE)
        || req.headers().contains_key(hyper::header::ORIGIN)
    {
        return Ok(json_response(
            StatusCode::FORBIDDEN,
            &ErrorBody {
                error: "browser-originated requests are not accepted by this API".to_string(),
            },
        ));
    }

    // Every endpoint — reads included — requires a valid operator token.
    let actor = match req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| registry.verify_bearer(v))
    {
        Some(operator) => operator.to_string(),
        None => return Ok(unauthorized()),
    };

    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = match (&method, path.as_str()) {
        (&Method::GET, "/whoami") => json_response(StatusCode::OK, &WhoamiView { operator: actor }),
        (&Method::GET, "/status") => match source.status().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserve-health") => match source.reserve_health().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/onchain") => match source.onchain().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/fee") => json_response(StatusCode::OK, &fee_view()),
        (&Method::GET, "/manual-review") => match source.manual_review().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/rebalances") => match source.rebalances().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/audit-log") => match parse_audit_query(req.uri().query()) {
            Ok(filter) => match source.audit_log(filter).await {
                Ok(v) => json_response(StatusCode::OK, &v),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },
        (&Method::POST, "/pause") | (&Method::POST, "/unpause") => {
            let pausing = path == "/pause";
            match read_json::<DirectionNoteInput>(req).await {
                Ok(input) => {
                    match (
                        parse_reserve_direction(&input.direction),
                        require_note(&input.note),
                    ) {
                        (Ok(direction), Ok(note)) => {
                            match source
                                .set_local_pause(direction, pausing, note.to_string(), actor)
                                .await
                            {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => error_response(e),
                    }
                }
                Err(resp) => *resp,
            }
        }
        (&Method::POST, "/admission/close") | (&Method::POST, "/admission/open") => {
            let closing = path == "/admission/close";
            match read_json::<DirectionNoteInput>(req).await {
                Ok(input) => {
                    match (
                        parse_reserve_direction(&input.direction),
                        require_note(&input.note),
                    ) {
                        (Ok(direction), Ok(note)) => {
                            match source
                                .set_admission(direction, closing, note.to_string(), actor)
                                .await
                            {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => error_response(e),
                    }
                }
                Err(resp) => *resp,
            }
        }
        (&Method::POST, "/rebalances") => match read_json::<RebalanceProposeInput>(req).await {
            Ok(input) => match require_note(&input.note) {
                Ok(_) => match source.rebalance_propose(input, actor).await {
                    Ok(v) => json_response(StatusCode::CREATED, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => error_response(e),
            },
            Err(resp) => *resp,
        },
        (&Method::POST, "/cli-command") => {
            match read_json::<cli_command::CliCommandInput>(req).await {
                Ok(input) => match source.cli_command(input).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                },
                Err(resp) => *resp,
            }
        }
        (&Method::POST, other_path) => {
            if let Some(request_id) = parse_manual_review_resume_path(other_path) {
                match read_json::<NoteInput>(req).await {
                    Ok(input) => match require_note(&input.note) {
                        Ok(note) => {
                            match source
                                .resume_manual_review(request_id, note.to_string(), actor)
                                .await
                            {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            }
                        }
                        Err(e) => error_response(e),
                    },
                    Err(resp) => *resp,
                }
            } else if let Some((id, Some(verb))) = parse_rebalance_path(other_path) {
                match verb {
                    "approve" | "reject" | "cancel" | "fail" => {
                        match read_json::<NoteInput>(req).await {
                            Ok(input) => match require_note(&input.note) {
                                Ok(note) => {
                                    let note = note.to_string();
                                    let result = match verb {
                                        "approve" => {
                                            source.rebalance_approve(id, note, actor).await
                                        }
                                        "reject" => source.rebalance_reject(id, note, actor).await,
                                        "cancel" => source.rebalance_cancel(id, note, actor).await,
                                        _ => source.rebalance_fail(id, note, actor).await,
                                    };
                                    match result {
                                        Ok(v) => json_response(StatusCode::OK, &v),
                                        Err(e) => error_response(e),
                                    }
                                }
                                Err(e) => error_response(e),
                            },
                            Err(resp) => *resp,
                        }
                    }
                    "record-executed" => {
                        match read_json::<RebalanceRecordExecutedInput>(req).await {
                            Ok(input) => match require_note(&input.note) {
                                Ok(_) => {
                                    match source.rebalance_record_executed(id, input, actor).await {
                                        Ok(v) => json_response(StatusCode::OK, &v),
                                        Err(e) => error_response(e),
                                    }
                                }
                                Err(e) => error_response(e),
                            },
                            Err(resp) => *resp,
                        }
                    }
                    "confirm" => match read_json::<RebalanceConfirmInput>(req).await {
                        Ok(input) => match require_note(&input.note) {
                            Ok(_) => match source.rebalance_confirm(id, input, actor).await {
                                Ok(v) => json_response(StatusCode::OK, &v),
                                Err(e) => error_response(e),
                            },
                            Err(e) => error_response(e),
                        },
                        Err(resp) => *resp,
                    },
                    _ => json_response(
                        StatusCode::NOT_FOUND,
                        &ErrorBody {
                            error: "not found".to_string(),
                        },
                    ),
                }
            } else {
                json_response(
                    StatusCode::NOT_FOUND,
                    &ErrorBody {
                        error: "not found".to_string(),
                    },
                )
            }
        }
        (&Method::GET, other_path) => {
            if let Some((id, None)) = parse_rebalance_path(other_path) {
                match source.rebalance(id).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            } else {
                json_response(
                    StatusCode::NOT_FOUND,
                    &ErrorBody {
                        error: "not found".to_string(),
                    },
                )
            }
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            &ErrorBody {
                error: "not found".to_string(),
            },
        ),
    };

    Ok(response)
}

/// Serves the admin API until `shutdown` flips. Bind `addr` privately —
/// see the module docs; there is no TLS termination here (put the
/// operators' reverse proxy in front for that), but unlike the public
/// API every request is authenticated.
pub async fn serve<S: AdminSource>(
    addr: SocketAddr,
    source: Arc<S>,
    registry: Arc<OperatorRegistry>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "admin API listening");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("admin API: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "admin API: accept failed");
                        continue;
                    }
                };
                let source = Arc::clone(&source);
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        handle(req, Arc::clone(&source), Arc::clone(&registry))
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%peer, error = %e, "admin API connection ended");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
