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
//! and has no path that executes a command or submits a transaction —
//! including for ManualReview refunds, whose read-only listing and dry
//! run are served here (`GET /refunds`, `GET /refunds/{id}/dry-run`)
//! while execution stays a [`cli_command`]-generated `glc-admin` line for
//! an operator to run with their own keypair. The
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
//! (schema v15, [`crate::ledger::Ledger::append_admin_audit`]) under the
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
use solana_sdk::pubkey::Pubkey;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

use crate::amount_conversion;
use crate::ledger::SolanaRefundState;
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
    /// The AUTOMATIC confirmed-liquidity gate, reported alongside — never
    /// merged into — the operator-only `admission_closed` above, so an
    /// operator console can always show WHICH of the two is holding new
    /// SolToGlc obligations back (docs/09-runbook.md's
    /// "Confirmed-liquidity admission safety buffer"). Always `false` for
    /// `glc-to-sol`.
    pub liquidity_admission_closed: bool,
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
    pub liquidity_admission_closed: bool,
    /// Confirmed unreserved headroom and the thresholds it is judged
    /// against — signed, because a negative value is itself diagnostic
    /// (see `Ledger::available_capacity`). `(0, 0)` thresholds mean the
    /// buffer is disabled on this deployment.
    pub confirmed_admission_headroom: i64,
    pub admission_buffer_atomic: i64,
    pub admission_reopen_atomic: i64,
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
    /// All limits in the reserve mint's own atomic units — see
    /// `reserve_mint_decimals` for how many decimals that is (read LIVE
    /// from the mint, never assumed).
    pub min_transfer_amount: u64,
    pub per_transfer_limit: u64,
    pub protected_minimum: u64,
    pub rolling_volume_limit: u64,
    pub rolling_window_seconds: i64,
    pub obligation_count: u64,
    /// The reserve mint's live decimals; `None` only before
    /// `initialize_reserve_vault` has configured a mint.
    pub reserve_mint_decimals: Option<u8>,
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

/// One row of the ManualReview refund table: either a refund CANDIDATE (a
/// `SolToGlc` request still in `ManualReview` whose park reason is on
/// [`Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS`] and which has no refund
/// row yet) or an existing refund lifecycle row at any stage.
///
/// Purely a projection of already-persisted state — every field is read
/// from the ledger. Neither the amount nor the destination is ever
/// accepted from a caller: the destination only exists here once
/// `begin_solana_refund` has derived and recorded it from the verified
/// on-chain `WithdrawalObligation.requester`.
#[derive(Debug, Serialize)]
pub struct RefundItemView {
    pub request_id: i64,
    /// `bridge_requests.state` — `ManualReview`, `RefundPending`,
    /// `RefundBroadcast`, or `Refunded`.
    pub request_state: String,
    pub direction: String,
    pub manual_review_reason: Option<String>,
    /// Canonical (8-decimal) gross deposit — exactly what a refund
    /// returns; no fee is deducted (docs/09-runbook.md).
    pub gross_amount_atomic: u64,
    /// The same quantity as a GLC decimal string, for display only.
    pub gross_amount_display_glc: String,
    pub source_obligation_index: Option<u64>,
    /// Original depositor, base58. From the verified on-chain obligation.
    pub requester: Option<String>,
    /// Refund destination, base58 — present once a refund row exists.
    /// Always the depositor's canonical ATA, never operator-supplied.
    pub destination_token_account: Option<String>,
    /// `Pending` | `Broadcast` | `Confirmed`; `None` for a candidate.
    pub refund_state: Option<String>,
    pub refund_signature: Option<String>,
    pub refund_nonce: Option<u64>,
    /// The reserve mint's own native atomic units.
    pub refund_amount_solana_atomic: Option<u64>,
    pub refund_note: Option<String>,
    pub refund_created_by: Option<String>,
    pub refund_created_at: Option<i64>,
    pub refund_broadcast_at: Option<i64>,
    pub refund_confirmed_at: Option<i64>,
    /// The refund confirmed and the request is `Refunded` — terminal.
    /// The console must never offer a refund or resume action on a
    /// terminal row.
    pub terminal: bool,
    /// Whether offering a dry run for this row is meaningful right now.
    /// False for terminal rows.
    pub dry_run_available: bool,
}

#[derive(Debug, Serialize)]
pub struct RefundsView {
    pub refunds: Vec<RefundItemView>,
}

/// Canonical ledger amounts are 8-decimal Goldcoin-native units
/// (docs/20-bridge-fee.md). The refunded GLC quantity is the same whether
/// expressed canonically or in the mint's own 6 decimals, so the listing
/// renders it from the canonical gross and needs no live mint read.
const CANONICAL_DISPLAY_DECIMALS: u8 = 8;

/// Base58 for a 32-byte on-chain address, so views never leak raw byte
/// arrays into JSON.
fn base58(bytes: &[u8; 32]) -> String {
    solana_sdk::pubkey::Pubkey::from(*bytes).to_string()
}

/// One named safety check from the dry run, projected for display.
#[derive(Debug, Serialize)]
pub struct RefundCheckView {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// True only for the global-pause check: an operator precondition for
    /// executing, not a property of the request. The console reports it
    /// separately so a not-yet-engaged pause never reads as the request
    /// being ineligible.
    pub is_execute_precondition: bool,
}

/// The chain-derived refund plan — every value DERIVED from the verified
/// on-chain obligation and live `BridgeConfig`, never caller-supplied.
#[derive(Debug, Serialize)]
pub struct RefundPlanView {
    pub obligation_index: u64,
    pub obligation_pda: String,
    pub requester: String,
    pub destination_token_account: String,
    pub destination_exists: bool,
    pub reserve_mint: String,
    pub token_program: String,
    pub mint_decimals: u8,
    /// Native (mint-decimal) units, and the same quantity as GLC.
    pub amount_solana_atomic: u64,
    pub amount_display_glc: String,
    pub gross_canonical_atomic: u64,
    pub refund_nonce: u64,
    pub nonce_pda: String,
    pub nonce_pda_exists: bool,
    pub attestation_epoch: u64,
    pub attestation_threshold: u8,
    pub attestation_key_count: usize,
    pub bridge_paused: bool,
    pub protected_minimum: u64,
    pub reserve_token_account: String,
    pub reserve_balance: u64,
    pub reserve_balance_after: u64,
}

/// The reserve-safety check: stricter than the on-chain floor, because it
/// also excludes liquidity reserved for GlcToSol releases and every other
/// still-open refund.
#[derive(Debug, Serialize)]
pub struct RefundCapacityView {
    pub amount_solana_atomic: u64,
    pub total_reserve_balance: i64,
    pub protected_minimum: i64,
    pub reserved_liquidity: i64,
    pub other_open_refunds_atomic: i64,
    pub ok: bool,
}

/// Result of the strict read-only refund dry run — the identical
/// `solana::refund::dry_run_refund` the `glc-admin refund-manual-review`
/// dry run uses, projected to JSON. Contacts no signer, loads no keypair,
/// writes nothing, broadcasts nothing.
#[derive(Debug, Serialize)]
pub struct RefundDryRunView {
    pub request_id: i64,
    pub request_state: String,
    pub manual_review_reason: Option<String>,
    /// `None` when the chain-side verification failed; `plan_error` then
    /// carries the fail-closed reason, which is itself a failed check.
    pub plan: Option<RefundPlanView>,
    pub plan_error: Option<String>,
    pub capacity: Option<RefundCapacityView>,
    pub checks: Vec<RefundCheckView>,
    /// Every REQUEST-level check passes.
    pub eligible_ignoring_pause: bool,
    /// The on-chain global pause is currently engaged.
    pub pause_engaged: bool,
    /// Executing right now would proceed (eligible AND paused), or the
    /// refund already confirmed and executing is a safe no-op.
    pub would_execute: bool,
    pub already_refunded: bool,
    /// Operator-facing one-line verdict, worded exactly as the CLI's.
    pub verdict: String,
}

/// Projects the shared [`crate::solana::refund::RefundDryRunReport`] into
/// its JSON view. Pure — it adds no eligibility logic of its own; every
/// boolean here comes from the report the refund module produced.
fn refund_dry_run_view(report: crate::solana::refund::RefundDryRunReport) -> RefundDryRunView {
    let verdict = if report.already_refunded {
        "ALREADY REFUNDED — terminal; executing would report the existing transaction and \
         change nothing"
    } else if report.would_execute {
        "ELIGIBLE — executing would proceed (all checks re-run against fresh state first)"
    } else if report.eligible_ignoring_pause {
        "ELIGIBLE, PENDING GLOBAL PAUSE — every request-level check passes. Engage the \
         on-chain global pause, then run the generated command; unpause explicitly afterwards"
    } else {
        "NOT ELIGIBLE — execution would refuse (no override exists)"
    }
    .to_string();

    let plan_error = report.plan.as_ref().err().cloned();
    let plan = report.plan.ok().map(|p| RefundPlanView {
        obligation_index: p.obligation_index,
        obligation_pda: p.obligation_pda.to_string(),
        requester: p.requester.to_string(),
        destination_token_account: p.destination_token_account.to_string(),
        destination_exists: p.destination_exists,
        reserve_mint: p.reserve_mint.to_string(),
        token_program: p.token_program.to_string(),
        mint_decimals: p.mint_decimals,
        amount_solana_atomic: p.amount_solana_atomic,
        amount_display_glc: cli_command::format_atomic_as_decimal_string(
            p.amount_solana_atomic,
            // Live mint decimals, bounded exactly as `cli_command`
            // bounds every chain-fed decimals value before formatting.
            p.mint_decimals.min(19),
        ),
        gross_canonical_atomic: p.gross_canonical_atomic,
        refund_nonce: p.nonce,
        nonce_pda: p.nonce_pda.to_string(),
        nonce_pda_exists: p.nonce_pda_exists,
        attestation_epoch: p.attestation_epoch,
        attestation_threshold: p.attestation_threshold,
        attestation_key_count: p.attestation_keys.len(),
        bridge_paused: p.bridge_paused,
        protected_minimum: p.protected_minimum,
        reserve_token_account: p.reserve_token_account.to_string(),
        reserve_balance: p.reserve_balance,
        reserve_balance_after: p.reserve_balance.saturating_sub(p.amount_solana_atomic),
    });

    RefundDryRunView {
        request_id: report.request.id,
        request_state: report.request.state.as_str().to_string(),
        manual_review_reason: report.db_checks.manual_review_reason.clone(),
        plan,
        plan_error,
        capacity: report.capacity.map(|c| RefundCapacityView {
            amount_solana_atomic: c.amount_solana_atomic,
            total_reserve_balance: c.total_reserve_balance,
            protected_minimum: c.protected_minimum,
            reserved_liquidity: c.reserved_liquidity,
            other_open_refunds_atomic: c.other_open_refunds_atomic,
            ok: c.ok,
        }),
        checks: report
            .checks
            .into_iter()
            .map(|c| RefundCheckView {
                name: c.name.to_string(),
                ok: c.ok,
                detail: c.detail,
                is_execute_precondition: c.is_execute_precondition,
            })
            .collect(),
        eligible_ignoring_pause: report.eligible_ignoring_pause,
        pause_engaged: report.pause_engaged,
        would_execute: report.would_execute,
        already_refunded: report.already_refunded,
        verdict,
    }
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
            // The SAME slugs this API accepts as input
            // (`parse_reserve_direction`, `RebalanceProposeInput.kind`),
            // so a value read from a response round-trips into a request
            // body — and an explicit mapping, never `{:?}`, so a Rust
            // enum rename can't silently change the wire format.
            direction: direction_name(r.direction).to_string(),
            kind: match r.kind {
                RebalanceKind::Deposit => "deposit".to_string(),
                RebalanceKind::Withdraw => "withdraw".to_string(),
            },
            amount_atomic: r.amount_atomic,
            state: r.state.as_str().to_string(),
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
    /// Read-only: refund candidates and refund lifecycle rows. Pure
    /// ledger projection — no RPC, no signer, no keypair, no mutation.
    fn refunds(&self) -> BoxFut<'_, Result<RefundsView, AdminError>>;
    /// Read-only: the strict PR-#50 refund dry run for one request,
    /// delegating to [`crate::solana::refund::dry_run_refund`] verbatim.
    /// Reads the ledger and the chain; writes nothing, contacts no
    /// signer, loads no keypair, broadcasts nothing.
    fn refund_dry_run(&self, request_id: i64) -> BoxFut<'_, Result<RefundDryRunView, AdminError>>;
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
        now_unix()
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

        // The mint's LIVE decimals — never a compile-time assumption
        // (`amount_conversion` module docs) — read here so every consumer
        // of this view, notably `cli_command`'s GLC→atomic conversion,
        // converts against what the chain actually says. `None` only in
        // the pre-`initialize_reserve_vault` state, where no mint is
        // configured yet.
        let reserve_mint_decimals = if config.reserve_token_mint == Pubkey::default() {
            None
        } else {
            Some(
                accounts::fetch_reserve_mint_decimals(&self.rpc, &config.reserve_token_mint)
                    .await
                    .map_err(|e| {
                        AdminError::Upstream(format!("reserve mint decimals read failed: {e}"))
                    })?,
            )
        };

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
            reserve_mint_decimals,
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

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn storage_error() -> AdminError {
    AdminError::Ledger("ledger storage error".to_string())
}

/// Descriptor for one audited admin action — who did what to what, with
/// the mandatory note and the new-value snapshot the audit row records.
/// The OLD value is deliberately not a field: it is read inside the
/// audited transaction (see [`audited_mutation`]) so concurrent
/// mutations can never record stale, impossible old→new histories.
pub struct AuditedAction<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub target: String,
    pub note: &'a str,
    pub new_value: Option<String>,
}

/// Runs one admin mutation with the audit discipline, ATOMICALLY: the
/// old-value read, the mutation, and the audit row all share one
/// `BEGIN IMMEDIATE` scope ([`Ledger::begin_admin_action`]), so either
/// the mutation persists together with its audit row or neither does —
/// an audit-append failure rolls the already-applied mutation back
/// instead of leaving it committed and unaudited (where a retry would
/// duplicate a non-idempotent action), and the old-value snapshot is
/// taken under the same write lock, so two racing operators can never
/// both record the same impossible old→new transition. A validated
/// refusal from the mutation rolls back only the mutation's own writes
/// (its inner savepoint) and the scope then commits just the failure
/// audit row, so refusals stay audited. `on_success` may adjust the
/// entry once the mutation's result is known — the target for propose
/// (the id it just created), the new-value for resume (whose no-op
/// outcome must never be recorded as a transition that happened).
///
/// Shared by the admin API's HTTP handlers and `glc-admin`'s local
/// mutation commands — one implementation, so the two surfaces cannot
/// drift on what gets audited.
pub fn audited_mutation<T>(
    ledger: &mut Ledger,
    mut params: AuditedAction<'_>,
    old_value: impl FnOnce(&mut Ledger) -> Result<Option<String>, AdminError>,
    mutation: impl FnOnce(&mut Ledger) -> Result<T, AdminError>,
    on_success: impl FnOnce(&T, &mut AuditedAction<'_>),
) -> Result<(T, MutationReceipt), AdminError> {
    ledger.begin_admin_action().map_err(|_| storage_error())?;
    // Inside the scope: this read holds the same write lock the mutation
    // will use, so the snapshot cannot go stale under a concurrent
    // mutation. A failing pre-read aborts before anything mutated —
    // nothing to audit.
    let old_value = match old_value(ledger) {
        Ok(v) => v,
        Err(e) => {
            let _ = ledger.rollback_admin_action();
            return Err(e);
        }
    };
    let result = mutation(ledger);
    if let Ok(value) = &result {
        on_success(value, &mut params);
    }
    let outcome = match &result {
        Ok(_) => AdminAuditOutcome::Success,
        Err(e) => AdminAuditOutcome::Error(e.to_string()),
    };
    let entry = AdminAuditEntry {
        at: now_unix(),
        actor: params.actor.to_string(),
        action: params.action.to_string(),
        target: Some(params.target.clone()),
        old_value: old_value.clone(),
        new_value: params.new_value.clone(),
        note: params.note.to_string(),
        outcome,
    };
    match ledger.append_admin_audit(&entry) {
        Ok(audit_id) => {
            if ledger.commit_admin_action().is_err() {
                let _ = ledger.rollback_admin_action();
                return Err(storage_error());
            }
            let value = result?;
            Ok((
                value,
                MutationReceipt {
                    audit_id,
                    action: params.action.to_string(),
                    target: params.target,
                    old_value,
                    new_value: params.new_value,
                },
            ))
        }
        Err(_) => {
            let _ = ledger.rollback_admin_action();
            Err(AdminError::Ledger(
                "ledger storage error: the audit row could not be written, so the action was \
                 rolled back"
                    .to_string(),
            ))
        }
    }
}

/// Local reserve-direction pause/unpause, audited — the one
/// implementation behind both `POST /pause`//`unpause` and `glc-admin
/// pause`/`unpause`.
pub fn audited_set_local_pause(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    paused: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    // One note shape regardless of surface: the CLI validates but does
    // not trim, the HTTP layer trims — normalize here so the shared
    // audit log never records the same note padded from one surface and
    // bare from the other.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: if paused { "pause" } else { "unpause" },
            target: direction_name(direction).to_string(),
            note,
            new_value: Some(format!("paused={paused}")),
        },
        |l| Ok(Some(format!("paused={}", l.is_paused(direction)?))),
        |l| {
            l.set_paused(direction, paused, Some(note))
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Admission close/open, audited — the one implementation behind both
/// `POST /admission/...` and `glc-admin close-admission`/
/// `open-admission`. The goldcoin-direction-only rule is enforced INSIDE
/// the audited scope, so even that refusal leaves an audit row.
pub fn audited_set_admission(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    closed: bool,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    // One note shape regardless of surface: the CLI validates but does
    // not trim, the HTTP layer trims — normalize here so the shared
    // audit log never records the same note padded from one surface and
    // bare from the other.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: if closed {
                "admission_close"
            } else {
                "admission_open"
            },
            target: direction_name(direction).to_string(),
            note,
            new_value: Some(format!("admission_closed={closed}")),
        },
        |l| {
            if direction == ReserveDirection::GoldcoinReserve {
                Ok(Some(format!(
                    "admission_closed={}",
                    l.is_admission_closed(direction)?
                )))
            } else {
                Ok(None)
            }
        },
        |l| {
            // Same restriction as always: only the Goldcoin direction
            // implements admission control in this version.
            if direction != ReserveDirection::GoldcoinReserve {
                return Err(AdminError::BadRequest(
                    "admission control is only implemented for direction goldcoin in this version"
                        .to_string(),
                ));
            }
            if closed {
                l.set_admission(direction, true, Some(note))
                    .map_err(AdminError::from)
            } else {
                guard::open_admission_guarded(l, direction, note).map_err(|e| match e {
                    guard::OpenAdmissionError::Refused(message) => AdminError::Conflict(message),
                    guard::OpenAdmissionError::Ledger(ledger_error) => {
                        AdminError::from(ledger_error)
                    }
                })
            }
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// ManualReview resume, audited — the one implementation behind both
/// `POST /manual-review/{id}/resume` and `glc-admin
/// resume-manual-review`. The authenticated `actor` is recorded on BOTH
/// trails: the admin audit row and `bridge_request_state_log`'s
/// transition row (never a hardcoded placeholder), so per-operator
/// attribution survives into the request's authoritative history.
pub fn audited_resume_manual_review(
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
) -> Result<(crate::ledger::ResumeManualReviewOutcome, MutationReceipt), AdminError> {
    // One note shape regardless of surface: the CLI validates but does
    // not trim, the HTTP layer trims — normalize here so the shared
    // audit log never records the same note padded from one surface and
    // bare from the other.
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "resume_manual_review",
            target: request_id.to_string(),
            note,
            // Overwritten by `on_success` below once the real outcome is
            // known — an AlreadyResumed no-op must never be recorded as
            // a transition that happened.
            new_value: None,
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            // Called as-is: every safety check (SolToGlc-only, state,
            // reason whitelist, no-payout, capacity, and the
            // unconditional source-wallet/recipient rate-limit
            // re-checks) lives INSIDE this Ledger method and is never
            // re-implemented or pre-filtered here.
            l.resume_manual_review_sol_to_glc(request_id, note, actor, now_unix())
                .map_err(AdminError::from)
        },
        |outcome, params| {
            params.new_value = Some(match outcome {
                crate::ledger::ResumeManualReviewOutcome::Resumed => "SourceFinalized".to_string(),
                crate::ledger::ResumeManualReviewOutcome::AlreadyResumed { state } => {
                    format!("no-op: already resumed (state={})", state.as_str())
                }
            });
        },
    )
}

/// Refund lifecycle begin, audited — the `ManualReview -> RefundPending`
/// transition plus the `solana_refunds` row, atomic with its audit row.
/// EXECUTION is a CLI-only surface (`glc-admin refund-manual-review
/// --execute`, via `solana::refund::execute_refund`): the HTTP admin API
/// deliberately has no refund EXECUTION route, matching its
/// no-keypair/no-transaction posture — refund execution needs the admin
/// keypair and the signer stack, which never belong on that surface. The
/// API does serve the read-only halves (`GET /refunds`, `GET
/// /refunds/{id}/dry-run`) and generates the `glc-admin` command line for
/// a human to run, exactly as it does for `set_paused`/`set_limit`.
pub fn audited_begin_solana_refund(
    ledger: &mut Ledger,
    request_id: i64,
    verified: &crate::ledger::VerifiedRefundInputs,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    let verified = *verified;
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "refund_begin",
            target: request_id.to_string(),
            note,
            new_value: Some(format!(
                "RefundPending (nonce {:#x}, amount {} native, destination ATA of original \
                 requester)",
                Ledger::solana_refund_nonce(request_id).map_err(AdminError::from)?,
                verified.amount_solana_atomic
            )),
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.begin_solana_refund(request_id, &verified, note, actor, now_unix())
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Refund broadcast record, audited — persists the transaction signature,
/// blockhash, epoch, and the simulation summary BEFORE the send, atomic
/// with its audit row.
#[allow(clippy::too_many_arguments)]
pub fn audited_record_solana_refund_broadcast(
    ledger: &mut Ledger,
    request_id: i64,
    refund_signature: &str,
    recent_blockhash: &str,
    attestation_epoch: u64,
    simulation_summary: &str,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "refund_broadcast",
            target: request_id.to_string(),
            note,
            new_value: Some(format!(
                "RefundBroadcast tx {refund_signature} ({simulation_summary})"
            )),
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.record_solana_refund_broadcast(
                request_id,
                refund_signature,
                recent_blockhash,
                attestation_epoch,
                now_unix(),
            )
            .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
}

/// Refund confirmation, audited — the terminal `Refunded` transition plus
/// the SolanaReserve book debit, atomic with its audit row.
pub fn audited_mark_solana_refund_confirmed(
    ledger: &mut Ledger,
    request_id: i64,
    note: &str,
    actor: &str,
) -> Result<MutationReceipt, AdminError> {
    let note = note.trim();
    audited_mutation(
        ledger,
        AuditedAction {
            actor,
            action: "refund_confirm",
            target: request_id.to_string(),
            note,
            new_value: Some("Refunded".to_string()),
        },
        |l| {
            Ok(l.get_request(request_id)?
                .map(|r| r.state.as_str().to_string()))
        },
        |l| {
            l.mark_solana_refund_confirmed(request_id, now_unix())
                .map_err(AdminError::from)
        },
        |_, _| {},
    )
    .map(|((), receipt)| receipt)
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
                let snapshot = reserve_health::check(&ledger, reserve, unix_now())?;
                let manual_review = ledger
                    .requests_by_state(transfer_direction, RequestState::ManualReview)?
                    .len();
                views.push(DirectionStatusView {
                    direction: direction_name(reserve).to_string(),
                    paused: snapshot.paused,
                    pause_reason: ledger.pause_reason(reserve)?,
                    admission_closed: snapshot.admission_closed,
                    admission_reason: ledger.admission_reason(reserve)?,
                    liquidity_admission_closed: snapshot.liquidity_admission_closed,
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
                let s = reserve_health::check(&ledger, direction, unix_now())?;
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
                    liquidity_admission_closed: s.liquidity_admission_closed,
                    confirmed_admission_headroom: s.confirmed_admission_headroom,
                    admission_buffer_atomic: s.admission_buffer_atomic,
                    admission_reopen_atomic: s.admission_reopen_atomic,
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
                        // `Direction::as_str` — the exact spelling the
                        // rest of the system parses ("GlcToSol"/
                        // "SolToGlc"), explicit rather than `{:?}` so a
                        // Rust rename can't change the wire format.
                        direction: direction.as_str().to_string(),
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

    fn refunds(&self) -> BoxFut<'_, Result<RefundsView, AdminError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let mut refunds: Vec<RefundItemView> = Vec::new();
            let mut with_row: std::collections::HashSet<i64> = std::collections::HashSet::new();

            // Existing refund lifecycle rows, at any stage.
            for row in ledger.list_solana_refunds(false)? {
                with_row.insert(row.request_id);
                let request = ledger.get_request(row.request_id)?;
                let request_state = request
                    .as_ref()
                    .map(|r| r.state.as_str().to_string())
                    .unwrap_or_else(|| "Unknown".to_string());
                let terminal = row.state == SolanaRefundState::Confirmed;
                let gross = request.as_ref().map(|r| r.gross_amount_atomic).unwrap_or(0);
                refunds.push(RefundItemView {
                    request_id: row.request_id,
                    request_state,
                    direction: Direction::SolToGlc.as_str().to_string(),
                    manual_review_reason: Some(row.manual_review_reason.clone()),
                    gross_amount_atomic: gross,
                    gross_amount_display_glc: cli_command::format_atomic_as_decimal_string(
                        gross,
                        CANONICAL_DISPLAY_DECIMALS,
                    ),
                    source_obligation_index: Some(row.obligation_index),
                    requester: Some(base58(&row.requester)),
                    destination_token_account: Some(base58(&row.destination_token_account)),
                    refund_state: Some(row.state.as_str().to_string()),
                    refund_signature: row.refund_signature.clone(),
                    refund_nonce: Some(row.nonce),
                    refund_amount_solana_atomic: Some(row.amount_solana_atomic),
                    refund_note: Some(row.note.clone()),
                    refund_created_by: Some(row.created_by.clone()),
                    refund_created_at: Some(row.created_at),
                    refund_broadcast_at: row.broadcast_at,
                    refund_confirmed_at: row.confirmed_at,
                    terminal,
                    // A terminal refund is done: the console must never
                    // offer it any further action.
                    dry_run_available: !terminal,
                });
            }

            // Refund CANDIDATES: still parked in ManualReview, with a
            // reason on the same whitelist the refund path itself
            // enforces — read from `Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS`
            // so this listing can never drift from what is actually
            // refundable.
            for req in ledger.requests_by_state(Direction::SolToGlc, RequestState::ManualReview)? {
                if with_row.contains(&req.id) {
                    continue;
                }
                let whitelisted = req
                    .manual_review_note
                    .as_deref()
                    .is_some_and(|r| Ledger::REFUNDABLE_MANUAL_REVIEW_REASONS.contains(&r));
                if !whitelisted {
                    continue;
                }
                refunds.push(RefundItemView {
                    request_id: req.id,
                    request_state: req.state.as_str().to_string(),
                    direction: req.direction.as_str().to_string(),
                    manual_review_reason: req.manual_review_note.clone(),
                    gross_amount_atomic: req.gross_amount_atomic,
                    gross_amount_display_glc: cli_command::format_atomic_as_decimal_string(
                        req.gross_amount_atomic,
                        CANONICAL_DISPLAY_DECIMALS,
                    ),
                    source_obligation_index: req.source_obligation_index,
                    requester: req.requester.as_ref().map(base58),
                    // Derived only at dry-run time, from the verified
                    // on-chain obligation — never stored ahead of it and
                    // never caller-supplied.
                    destination_token_account: None,
                    refund_state: None,
                    refund_signature: None,
                    refund_nonce: None,
                    refund_amount_solana_atomic: None,
                    refund_note: None,
                    refund_created_by: None,
                    refund_created_at: None,
                    refund_broadcast_at: None,
                    refund_confirmed_at: None,
                    terminal: false,
                    dry_run_available: true,
                });
            }

            refunds.sort_by_key(|r| r.request_id);
            Ok(RefundsView { refunds })
        })
    }

    fn refund_dry_run(&self, request_id: i64) -> BoxFut<'_, Result<RefundDryRunView, AdminError>> {
        Box::pin(async move {
            use crate::solana::refund;
            // Phased exactly as `refund::dry_run_refund` phases itself,
            // for one reason: `Ledger` is not `Sync`, so a `&Ledger` held
            // across the chain-read `.await` would make this handler
            // future non-`Send`. Each ledger borrow is therefore opened
            // and dropped inside its own scope. No check is
            // reimplemented here — phases 1 and 3 are the refund
            // module's own functions, and the verdict comes from its
            // `assemble_refund_dry_run`.
            let inputs = {
                let ledger = self.open_ledger()?;
                // A clean 404 for a request that simply does not exist,
                // rather than surfacing it as a failed dry run.
                if ledger.get_request(request_id)?.is_none() {
                    return Err(AdminError::NotFound(format!(
                        "bridge request {request_id} not found"
                    )));
                }
                refund::refund_dry_run_ledger_inputs(&ledger, request_id)
                    .map_err(AdminError::Upstream)?
            };
            let plan = refund::build_refund_plan(&self.rpc, &inputs.request).await;
            let capacity = match &plan {
                Ok(p) => {
                    let ledger = self.open_ledger()?;
                    Some(ledger.solana_refund_capacity(request_id, p.amount_solana_atomic)?)
                }
                Err(_) => None,
            };
            Ok(refund_dry_run_view(refund::assemble_refund_dry_run(
                inputs, plan, capacity,
            )))
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
            audited_set_local_pause(&mut ledger, direction, paused, &note, &actor)
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
            audited_set_admission(&mut ledger, direction, closed, &note, &actor)
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
            audited_resume_manual_review(&mut ledger, request_id, &note, &actor)
                .map(|(_outcome, receipt)| receipt)
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
                    severity: match a.severity {
                        crate::rebalance::ImbalanceSeverity::Normal => "Normal",
                        crate::rebalance::ImbalanceSeverity::Warning => "Warning",
                        crate::rebalance::ImbalanceSeverity::Critical => "Critical",
                    }
                    .to_string(),
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_propose",
                    target: "new".to_string(),
                    note: &input.note,
                    new_value: Some(format!(
                        "{} {} amount={}",
                        direction_name(direction),
                        input.kind,
                        input.amount_atomic
                    )),
                },
                |_| Ok(None),
                |l| {
                    l.propose_rebalance(
                        direction,
                        kind,
                        input.amount_atomic,
                        &input.note,
                        &actor,
                        input.required_approvals,
                        now_unix(),
                    )
                    .map_err(AdminError::from)
                },
                |id, params| params.target = id.to_string(),
            )
            .map(|(_id, receipt)| receipt)
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_approve",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.approve_rebalance(id, &actor, now_unix())
                        .map(|_| ())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_reject",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.reject_rebalance(id, &note, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_cancel",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.cancel_rebalance(id, &note, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_record_executed",
                    target: id.to_string(),
                    note: &input.note,
                    new_value: Some(format!("tx_reference={}", input.tx_reference)),
                },
                |_| Ok(None),
                |l| {
                    l.record_rebalance_executed(id, &input.tx_reference, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_confirm",
                    target: id.to_string(),
                    note: &input.note,
                    new_value: Some(format!("observed_amount={}", input.observed_amount_atomic)),
                },
                |_| Ok(None),
                |l| {
                    l.confirm_rebalance(id, input.observed_amount_atomic, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
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
            audited_mutation(
                &mut ledger,
                AuditedAction {
                    actor: &actor,
                    action: "rebalance_fail",
                    target: id.to_string(),
                    note: &note,
                    new_value: None,
                },
                |_| Ok(None),
                |l| {
                    l.fail_rebalance(id, &note, &actor, now_unix())
                        .map_err(AdminError::from)
                },
                |_, _| {},
            )
            .map(|((), receipt)| receipt)
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

/// Every admin mutation body is a small JSON object; anything beyond
/// this is a mistake or abuse, and buffering it unbounded on the daemon
/// that also runs settlements would be an OOM lever for anyone holding a
/// leaked token.
const MAX_BODY_BYTES: u64 = 64 * 1024;

async fn read_json<T: serde::de::DeserializeOwned>(
    req: Request<hyper::body::Incoming>,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    let limited = http_body_util::Limited::new(req.into_body(), MAX_BODY_BYTES as usize);
    let body = match limited.collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return Err(Box::new(json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorBody {
                    error: format!("request body unreadable or larger than {MAX_BODY_BYTES} bytes"),
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

/// Minimal application/x-www-form-urlencoded value decoding for the
/// audit filters: `+` is a space and `%XX` is a byte — an operator name
/// like "ops team" arrives as `ops+team` (URLSearchParams) or
/// `ops%20team` and must filter the same rows either way. A malformed
/// escape is a caller error, not something to pass through silently.
fn percent_decode(value: &str) -> Result<String, AdminError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(|| {
                        AdminError::BadRequest(format!("malformed percent-escape in {value:?}"))
                    })?;
                out.push(hex);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| AdminError::BadRequest(format!("query value {value:?} is not valid UTF-8")))
}

fn parse_audit_query(query: Option<&str>) -> Result<AdminAuditFilter, AdminError> {
    let mut filter = AdminAuditFilter::default();
    let Some(q) = query else {
        return Ok(filter);
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        // Strict on an audit surface: an empty filter value (`?actor=`)
        // or an unknown key (`?acton=...`) silently matching EVERY row
        // would misattribute what a reviewer reads as filtered results —
        // fail loudly instead.
        if value.is_empty() {
            return Err(AdminError::BadRequest(format!(
                "query parameter {key:?} has an empty value"
            )));
        }
        match key {
            "before_id" => {
                filter.before_id = Some(value.parse::<i64>().map_err(|_| {
                    AdminError::BadRequest("before_id must be an integer".to_string())
                })?);
            }
            "limit" => {
                let n = value.parse::<u32>().map_err(|_| {
                    AdminError::BadRequest("limit must be a positive integer".to_string())
                })?;
                // Zero would be a permanently empty page that looks like
                // "no audit rows" — reject it like the public API's
                // pagination does, never serve it.
                if n == 0 {
                    return Err(AdminError::BadRequest("limit must be >= 1".to_string()));
                }
                filter.limit = Some(n);
            }
            "action" => filter.action = Some(percent_decode(value)?),
            "actor" => filter.actor = Some(percent_decode(value)?),
            other => {
                return Err(AdminError::BadRequest(format!(
                    "unknown query parameter {other:?} (expected before_id|limit|action|actor)"
                )))
            }
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

/// `/refunds/{id}/dry-run` path parsing. There is deliberately no
/// `/refunds/{id}/execute` counterpart: refund execution needs the admin
/// keypair and the attestation signer stack, which this API never holds
/// (see the module docs). The console renders a `glc-admin` command line
/// for CLI approval instead.
fn parse_refund_dry_run_path(path: &str) -> Option<i64> {
    let rest = path.strip_prefix("/refunds/")?;
    let id = rest.strip_suffix("/dry-run")?;
    id.parse::<i64>().ok()
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
        (&Method::GET, "/refunds") => match source.refunds().await {
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
            if let Some(request_id) = parse_refund_dry_run_path(other_path) {
                match source.refund_dry_run(request_id).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            } else if let Some((id, None)) = parse_rebalance_path(other_path) {
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
