//! The minimal HTTP surface a client (the bridge frontend, or any other
//! caller) needs to submit and track a bridge request
//! (docs/15-post-phase6-audit.md P0 item 4). Before this module, the only
//! way to reach `Ledger::create_request`/`Ledger::get_request` was a
//! direct in-process Rust call or raw SQL — there was no network-facing
//! way for anything external to interact with the bridge at all.
//!
//! # What this exposes, and what it deliberately does not
//!
//! Read/write operations matched to what an external caller (the future
//! bridge UI) actually needs: bridge status (including, per direction,
//! whether new transfers are currently acceptable), transfer limits
//! (including the fixed 3% bridge fee rate), reserve availability, a
//! non-sensitive health summary, a server-authoritative quote, creating a
//! GLC -> Solana transfer (which requires reserving capacity and handing
//! back deposit instructions), looking up a transfer's lifecycle
//! (including confirmation progress) by id, a wallet-scoped list of a
//! caller's own transfers, aggregate bridge statistics, a real
//! reserve-balance history, and a public settlement-event feed.
//!
//! It never exposes: custody keys or any signing material (this module
//! never touches [`crate::signing`]), privileged admin operations (pause/
//! unpause/limit changes stay on `glc-admin`, gated by possession of the
//! admin keypair; the LOCAL subset of those operations is additionally
//! reachable through the separately-bound, authenticated
//! [`crate::admin_api`] listener — a deliberate boundary change recorded
//! in that module's own docs, and still never through THIS public
//! listener), rebalancing/custody-transition detail (those are
//! operator-only, `glc-admin rebalance-*`/`custody-*`), or infrastructure
//! detail (RPC URLs, database paths, raw indexer internals — that is what
//! `ops::health` is for, and that endpoint's own docs already say to bind
//! it privately for exactly this reason). Reserve figures here are
//! limited to *available capacity* — a derived, bounded number ("how much
//! can currently move") — not the raw `total_reserve_balance`/
//! `protected_minimum`/`reserved_liquidity` breakdown `ops::health`
//! reports for an operator audience. [`PublicHealth`] is likewise a
//! small, derived subset of `ops::health::HealthReport` — halted/not and
//! a couple of counts, nothing an attacker could use to infer
//! infrastructure shape.
//!
//! # Solana -> GLC has no "create" step here
//!
//! A GLC -> Solana transfer must reserve capacity and obtain a
//! request-specific Goldcoin deposit address before any Goldcoin
//! transaction can reference it — a fresh address, unique to that one
//! request, derived from the same 2-of-3 signer set as every other
//! request (`goldcoin::derivation::derive_request_vault`) and persisted
//! against the request (`Ledger::set_glc_to_sol_deposit_address`).
//! Attribution is by that address alone: no `OP_RETURN`, memo, or
//! amount-matching trick is required, so an ordinary wallet — enter an
//! address and an amount, click send — is enough. (Requests created
//! before this addressing scheme existed still resolve via the legacy
//! shared vault address + `OP_RETURN` path — see
//! `goldcoin::deposit`/`goldcoin::indexer` — but nothing created through
//! this endpoint uses that path anymore.) So `POST /transfers` exists
//! for that direction. A Solana -> Goldcoin
//! transfer works the other way around: the user calls
//! `deposit_to_reserve` directly on-chain themselves (a plain SPL
//! transfer plus this bridge's own instruction, requiring no interaction
//! with this service beforehand), and this service's Solana indexer picks
//! it up automatically. There is nothing to "create" here for that
//! direction — `GET /status`'s `next_solana_obligation_index` is the one
//! piece of information a caller needs to construct that transaction
//! themselves.
//!
//! # No federation-era or wrapped-token language
//!
//! This is a reserve-backed bridge, not a federated one, and it does not
//! wrap or mint anything (docs/15-post-phase6-audit.md §4/§20) — there is
//! deliberately no `/federation`, `/federation/rounds`, or similarly
//! shaped endpoint here, even though a pre-existing frontend built against
//! the old bridge expects some. Connecting that frontend to this service
//! is later integration work, not something this module should paper
//! over by inventing federation-shaped responses that don't correspond to
//! anything this bridge actually does.

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

use crate::amount_conversion;
use crate::goldcoin::hex as glc_hex;
use crate::ledger::{
    CreateRequestOutcome, Direction, Ledger, LedgerError, RequestState, ReserveDirection,
};
use crate::ops::indexer_status::IndexerStatus;
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

/// The exact, approved end-user copy for a direction that currently
/// cannot accept a new transfer — for ANY of the reasons `glc_to_sol_
/// available`/`sol_to_glc_available` can be `false` (operator pause on
/// either layer, rolling-24h-volume quota exhausted, or reserve/
/// protected-minimum capacity insufficient): deliberately a single,
/// cause-agnostic message, never a technical reason code, and never a
/// claim about automatic reopening — there is no midnight reset and no
/// automatic unpause (docs/09-runbook.md's 2026-08-22 update). A UI
/// wanting the specific cause should read the boolean/numeric fields on
/// [`BridgeStatus`]/[`BridgeStats`] instead of parsing this string.
pub const DIRECTION_UNAVAILABLE_MESSAGE: &str = "Bridge capacity reached for this direction.\nTransfers are temporarily paused while reserves are replenished.\nPlease check the official Telegram for reopening updates.";

#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeStatus {
    pub goldcoin_paused: bool,
    pub solana_paused: bool,
    pub vault_address: String,
    pub next_solana_obligation_index: u64,
    /// Whether a NEW `GlcToSol` transfer can currently be created: the
    /// Solana reserve (this direction's destination — see
    /// [`Direction::destination_reserve`]) is unpaused, has capacity
    /// above zero, AND has rolling-24h-volume quota remaining. Derived,
    /// never a raw infrastructure detail — an in-flight transfer already
    /// reserved is unaffected either way.
    pub glc_to_sol_available: bool,
    /// Same as [`BridgeStatus::glc_to_sol_available`] for `SolToGlc`,
    /// whose destination is the Goldcoin reserve.
    pub sol_to_glc_available: bool,
    /// Whether `GlcToSol`'s (release, direction byte 0) rolling-24h-volume
    /// window is currently exhausted — `rolling_volume_remaining <
    /// min_transfer_amount`, i.e. no further transfer of any legal size
    /// could succeed right now. A live, read-only projection of on-chain
    /// state (`RollingVolumeWindow` vs `BridgeConfig::rolling_volume_
    /// limit`, [`accounts::rolling_volume_remaining`]) — this field
    /// itself never sets any pause; separately, this service's own
    /// background tick (`crate::quota`) engages this direction's local
    /// pause once it observes the same exhaustion, and unlike the
    /// on-chain window's own automatic reset, that local pause never
    /// clears itself — see [`DIRECTION_UNAVAILABLE_MESSAGE`]'s docs and
    /// docs/09-runbook.md's 2026-08-22 update.
    pub glc_to_sol_quota_exhausted: bool,
    /// Same as [`BridgeStatus::glc_to_sol_quota_exhausted`] for `SolToGlc`
    /// (deposit, direction byte 1).
    pub sol_to_glc_quota_exhausted: bool,
    /// Raw atomic units still available in `GlcToSol`'s current rolling-
    /// 24h-volume window — `0` when fully exhausted, up to the full
    /// `rolling_volume_limit` right after a fresh bucket reset. GLOBAL and
    /// PER DIRECTION: one `rolling_volume_limit` bounds both directions,
    /// each tracked in its own window (docs/09-runbook.md 2026-08-22).
    pub glc_to_sol_rolling_volume_remaining: u64,
    /// Same as [`BridgeStatus::glc_to_sol_rolling_volume_remaining`] for
    /// `SolToGlc`.
    pub sol_to_glc_rolling_volume_remaining: u64,
    /// Whether NEW `SolToGlc` obligations are currently admitted — a
    /// separate signal from [`BridgeStatus::goldcoin_paused`] (see
    /// `Ledger::set_admission`/docs/09-runbook.md's "Admission control
    /// (Solana->Goldcoin)" section). `false` means EITHER an operator has
    /// deliberately closed admission (`glc-admin close-admission`) OR the
    /// automatic confirmed-liquidity gate has closed it because confirmed
    /// unreserved Goldcoin headroom fell into the admission safety buffer
    /// (docs/09-runbook.md's "Confirmed-liquidity admission safety
    /// buffer"). A newly observed on-chain deposit still gets folded (its
    /// tokens are already locked on Solana regardless) but parks in
    /// `ManualReview` instead of processing normally. Already-accepted
    /// obligations are never affected by this either way — a UI should
    /// read `false` here as "not accepting new transfers right now",
    /// distinct from a reserve-health failure.
    ///
    /// The two causes are deliberately NOT distinguished here, matching
    /// this API's disclosure posture everywhere else (see
    /// [`DIRECTION_UNAVAILABLE_MESSAGE`] and `RecipientEligibility`'s own
    /// note): the user-facing answer is identical, and which of the two
    /// closed it — along with the raw headroom figures — is an operator
    /// detail, available on `glc-admin status`, the admin API and
    /// `/metrics`, never on the public endpoint.
    pub sol_to_glc_admission_open: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferLimits {
    pub min_transfer_amount: u64,
    pub per_transfer_limit: u64,
    /// The bridge fee rate in basis points (300 = 3%,
    /// docs/20-bridge-fee.md) — a fixed protocol constant, exposed here so
    /// a UI can display it without first needing a [`QuoteInput`]/
    /// [`QuoteOutput`] round trip for a specific amount.
    pub bridge_fee_bps: u64,
}

/// Non-sensitive operational health, for the same audience as every other
/// endpoint in this module (a public bridge UI) — deliberately a small,
/// derived subset of what [`crate::ops::health::HealthReport`] reports to
/// an operator: no RPC URLs, no database paths, no raw reserve balances,
/// no indexer-internal detail beyond "halted or not". See this module's
/// top-level docs for why that boundary exists.
#[derive(Debug, Serialize, Deserialize)]
pub struct PublicHealth {
    /// `false` iff the Goldcoin indexer is halted or a post-finality
    /// reorg has been detected — both are fail-closed states that require
    /// operator intervention and never auto-clear (docs/10-threat-model.md).
    pub healthy: bool,
    pub goldcoin_indexer_halted: bool,
    /// Requests parked in `ManualReview`, summed across both directions —
    /// visible so a UI can show "some transfers are under manual review"
    /// without exposing which ones or why.
    pub manual_review_backlog: u64,
    /// Cumulative count of post-finality Goldcoin reorg events ever
    /// detected (docs/10-threat-model.md P3) — any nonzero value means
    /// both reserves were paused at least once for this reason and
    /// require operator resolution before resuming.
    pub post_finality_reorg_events: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveAvailability {
    pub goldcoin_available_capacity: i64,
    pub solana_available_capacity: i64,
}

/// Request-count breakdown for one bridge [`Direction`], part of
/// [`BridgeStats`]. Counted in SQL (`Ledger::request_state_counts`), not
/// fetched row-by-row, so this stays cheap as `bridge_requests` grows.
#[derive(Debug, Serialize, Deserialize)]
pub struct DirectionStats {
    pub total_requests: i64,
    /// Sum of every non-terminal state (`RequestState::is_active`).
    pub in_progress_requests: i64,
    pub settled_requests: i64,
    pub manual_review_requests: i64,
}

/// Reserve-level aggregate for one [`ReserveDirection`], part of
/// [`BridgeStats`]. `settled_volume_atomic` and `accrued_fees_atomic` are
/// the same cumulative counters `ops::reserve_health`/`glc-admin status`
/// already track for an operator audience — surfaced here in their
/// public, capacity-only-equivalent form (no raw
/// `total_reserve_balance`/`protected_minimum`, matching
/// [`ReserveAvailability`]'s existing scope).
#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveStats {
    pub paused: bool,
    pub available_capacity: i64,
    /// Cumulative amount ever settled onto this reserve, in its own
    /// native destination units (docs/05-reserve-accounting.md) — real
    /// volume already recorded in `reserve_ledger.settled_liquidity_total`,
    /// never derived by summing individual transfers at request time.
    pub settled_volume_atomic: u64,
    /// Cumulative bridge-fee revenue accrued on this reserve, canonical
    /// units (docs/20-bridge-fee.md) — never counted toward capacity.
    pub accrued_fees_atomic: u64,
}

/// Public, non-sensitive aggregate bridge statistics (`GET /stats`).
/// Every figure is either a live derived check (availability) or a
/// cumulative counter already persisted by ordinary settlement/
/// reconciliation bookkeeping — nothing here is computed by scanning
/// history at request time beyond a single `GROUP BY` count query per
/// direction, and nothing is fabricated: an unavailable or zero figure is
/// reported as exactly that, never omitted or guessed.
#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeStats {
    pub goldcoin_paused: bool,
    pub solana_paused: bool,
    pub glc_to_sol_available: bool,
    pub sol_to_glc_available: bool,
    /// See [`BridgeStatus::glc_to_sol_quota_exhausted`].
    pub glc_to_sol_quota_exhausted: bool,
    /// See [`BridgeStatus::sol_to_glc_quota_exhausted`].
    pub sol_to_glc_quota_exhausted: bool,
    /// See [`BridgeStatus::glc_to_sol_rolling_volume_remaining`].
    pub glc_to_sol_rolling_volume_remaining: u64,
    /// See [`BridgeStatus::sol_to_glc_rolling_volume_remaining`].
    pub sol_to_glc_rolling_volume_remaining: u64,
    pub bridge_fee_bps: u64,
    pub glc_to_sol: DirectionStats,
    pub sol_to_glc: DirectionStats,
    pub goldcoin_reserve: ReserveStats,
    pub solana_reserve: ReserveStats,
    pub goldcoin_indexer_halted: bool,
    /// Seconds since each chain indexer's last completed tick — a
    /// freshness signal, not an infrastructure detail (no RPC URL, no
    /// host, no port).
    pub goldcoin_indexer_seconds_since_tick: i64,
    pub solana_indexer_seconds_since_tick: i64,
    pub post_finality_reorg_events: i64,
    pub as_of: i64,
}

/// One row of `GET /reserves/history` — a real, already-persisted
/// reconciliation-tick observation (`Ledger::reconciliation_findings_page`),
/// never a fabricated or interpolated data point. A `classification` of
/// `"SKIPPED: ..."` means this tick could not obtain a real chain read
/// (RPC failure, stale height) and recorded that fact rather than a
/// balance — callers should treat it as a gap, not a zero balance.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveHistoryEntry {
    pub id: i64,
    pub direction: String,
    pub detected_at: i64,
    pub expected_atomic: i64,
    pub observed_atomic: i64,
    pub delta_atomic: i64,
    pub classification: String,
    pub auto_paused: bool,
}

/// One row of `GET /explorer/events` — a real, already-persisted
/// bridge-request state transition (`Ledger::explorer_events_page`).
/// Deliberately reserve-bridge-native vocabulary (`RequestState`'s own
/// names), never federation/wrapped-token-era event kinds ("mint"/
/// "burn") — see this module's top-level docs. Carries no counterparty
/// address (this bridge does not truncate-and-publish one; see
/// [`TransferView`], which also omits `recipient`) and no operator
/// identity — rebalancing/custody-transition audit trails, which DO
/// carry real approver identities, stay operator-only via `glc-admin`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExplorerEvent {
    pub id: i64,
    pub request_id: i64,
    pub direction: String,
    pub from_state: Option<String>,
    pub to_state: String,
    pub at: i64,
    pub reason: Option<String>,
}

/// Cursor-paginated response envelope, shared by every list endpoint in
/// this module. `next_cursor` is `Some` only when the page returned
/// exactly `limit` items — i.e. there MIGHT be more; a short page is
/// proof there is nothing further, so `next_cursor` is `None` even
/// though a last item exists. Pass it back as `?cursor=` to fetch the
/// next (older) page.
#[derive(Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub as_of: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTransferInput {
    pub amount_atomic: u64,
    /// Base58 Solana pubkey the released funds should be sent to.
    pub recipient: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTransferOutput {
    pub request_id: i64,
    /// Send Goldcoin to this address to fund the transfer. A fresh
    /// address unique to THIS request, derived from the same 2-of-3
    /// signer set as every other request
    /// (`goldcoin::derivation::derive_request_vault`, Step 1) — never
    /// the shared legacy vault address. Attribution is by this address
    /// alone: no `OP_RETURN`, memo, or exact-amount trick is required or
    /// consulted for a request created through this endpoint. This
    /// service never constructs the deposit transaction itself; building
    /// and broadcasting it is the caller's own wallet's job.
    pub deposit_address: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferView {
    pub id: i64,
    pub direction: String,
    pub state: String,
    /// Canonical units (docs/20-bridge-fee.md) — what the user declared/
    /// deposited, before the bridge fee.
    pub gross_amount_atomic: u64,
    pub fee_bps: u64,
    /// Canonical units.
    pub fee_amount_atomic: u64,
    /// Canonical units — the real-world GLC entitlement delivered.
    pub net_amount_atomic: u64,
    pub created_at: i64,
    pub source_txid: Option<String>,
    pub source_confirmations: i64,
    /// The confirmation depth `source_confirmations` must reach before
    /// this request advances to `SourceFinalized`, so a UI can render
    /// "N/required confirmations" progress. Only meaningful for
    /// `GlcToSol` (a Goldcoin deposit is confirmation-tracked block by
    /// block); `None` for `SolToGlc`, whose Solana-side obligation folds
    /// directly to `SourceFinalized` once observed — there is no
    /// confirmation count to progress through.
    pub required_source_confirmations: Option<i64>,
    pub destination_txid: Option<String>,
    pub failure_reason: Option<String>,
}

/// Caller input for `GET /quote`: how much GROSS the caller intends to
/// bridge, in the ledger's canonical accounting unit (8 decimals,
/// docs/20-bridge-fee.md), regardless of direction. A future UI converts
/// a user-typed decimal GLC amount to this unit itself (`* 10^8`) before
/// calling — kept as one single, unambiguous unit here rather than one
/// that varies by direction, consistent with `CreateTransferInput`.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteInput {
    pub direction: String,
    pub gross_amount: u64,
}

/// Server-authoritative bridge quote. The UI displaying "You bridge: X
/// GLC / Bridge fee (3%): Y GLC / You receive: Z GLC" must source X/Y/Z
/// from here, never compute them itself (docs/20-bridge-fee.md) — this
/// endpoint runs the exact same `amount_conversion::compute_fee` the
/// server uses to actually build a settlement, so a quote can never
/// promise something a real transfer would compute differently.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteOutput {
    pub direction: String,
    /// Canonical units.
    pub gross_amount: u64,
    /// Human-readable GLC, computed via checked integer arithmetic (never
    /// a float) — e.g. `"12.34500000"`.
    pub gross_display_amount: String,
    pub fee_bps: u64,
    /// Canonical units.
    pub fee_amount: u64,
    pub fee_display_amount: String,
    /// Canonical units — the real-world GLC entitlement, before the
    /// destination chain's own decimal precision is applied.
    pub net_amount: u64,
    pub net_display_amount: String,
    /// The SOURCE chain's own atomic-unit decimals for this direction.
    pub source_decimals: u8,
    /// The DESTINATION chain's own atomic-unit decimals for this
    /// direction — what `net_amount` is actually converted to and
    /// released as, on-chain.
    pub destination_decimals: u8,
    pub source_asset: String,
    pub destination_asset: String,
}

/// `GET /recipients/sol-to-glc/eligibility?address=<Goldcoin p2pkh
/// address>&wallet=<base58 Solana pubkey, optional>` — whether a NEW
/// SolToGlc obligation naming this recipient (and, if `wallet` is given,
/// deposited from this Solana wallet) would currently be admitted, or
/// parked by ONE OF the two independent rolling 24-hour rate limits
/// (docs/09-runbook.md): the per-recipient limit
/// (`Ledger::sol_to_glc_recipient_rate_limited_until`) and the per-
/// source-wallet limit (`Ledger::sol_to_glc_source_wallet_rate_limited_until`)
/// that closes the bypass where one wallet spreads deposits across many
/// different recipients. Both read through the exact same query
/// `Ledger::fold_sol_deposit`'s admission check applies, so the answer is
/// always the authoritative ledger rule, never a re-implementation.
/// `wallet` is optional so existing callers that only know the recipient
/// so far keep working — omitting it simply means the source-wallet leg
/// is not checked yet. Read-only and purely advisory: the UI calls it to
/// warn a user BEFORE they sign a Solana transaction whose deposit would
/// only be parked in `ManualReview` — admission itself still re-checks at
/// fold time (independently, keyed by the on-chain `WithdrawalObligation`'s
/// own `requester`, never a client-provided string), so a stale (or
/// bypassed) answer here can never weaken either limit.
///
/// Deliberately minimal disclosure, consistent with this API never
/// exposing per-recipient/per-wallet identity elsewhere: a boolean, which
/// of the two limits is blocking (never both details at once — see
/// `blocked_reason`), and the reopen time — never which request is
/// blocking, its amount, its state, or anything else about the history.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecipientEligibility {
    /// Always `"SolToGlc"` — the only direction with either rate limit.
    /// `GlcToSol` recipients (Solana addresses) have none.
    pub direction: String,
    /// The trimmed address this answer is about — echoed back so a caller
    /// racing form edits can discard a stale response.
    pub address: String,
    /// The base58 wallet this answer also checked, when `?wallet=` was
    /// given — `null` when it was omitted, so a caller can tell "the
    /// wallet leg was not evaluated" apart from "it was evaluated and
    /// found eligible."
    pub wallet: Option<String>,
    /// `true` only when NEITHER limit currently blocks a new obligation.
    pub eligible: bool,
    /// Which limit is blocking, when `eligible` is `false`:
    /// [`BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED`] or
    /// [`BLOCKED_REASON_RECIPIENT_RATE_LIMITED`]. `null` when eligible.
    /// Checked wallet-first when `wallet` was provided and both would
    /// block, matching `Ledger::fold_sol_deposit`'s own precedence — the
    /// two limits are independently enforced either way, so this only
    /// affects which single reason is surfaced here.
    pub blocked_reason: Option<String>,
    /// Absolute unix second at which the blocking window reopens; `null`
    /// when eligible.
    pub retry_after: Option<i64>,
    /// The same instant as seconds from now, clamped at zero; `null` when
    /// eligible.
    pub retry_after_seconds: Option<i64>,
    /// The rolling window itself (86 400), shared by both limits, so
    /// clients need not hardcode "24 hours" in copy or logic.
    pub window_seconds: i64,
}

/// `blocked_reason` values [`RecipientEligibility`] reports — named
/// constants (rather than inline string literals at each call site) so the
/// backend answer and the UI's message-selection logic can never drift
/// apart on the exact spelling.
pub const BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED: &str = "source_wallet_rate_limited";
pub const BLOCKED_REASON_RECIPIENT_RATE_LIMITED: &str = "recipient_rate_limited";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid request: {0}")]
    BadRequest(String),
    /// Reserve/protected-minimum capacity constraint — one of the three
    /// direction-unavailable causes (see [`DIRECTION_UNAVAILABLE_MESSAGE`]).
    /// `available` is retained on the variant for server-side logging/
    /// introspection; the user-facing text is the same generic message as
    /// every other cause, never this raw number (this API's own module
    /// docs already avoid leaking raw reserve figures elsewhere).
    #[error("{}", DIRECTION_UNAVAILABLE_MESSAGE)]
    InsufficientLiquidity { available: i64 },
    /// Operator pause (on-chain `PauseScope::Release`/`Deposit`/`Global`,
    /// or this service's own local ledger gate) — see
    /// [`DIRECTION_UNAVAILABLE_MESSAGE`].
    #[error("{}", DIRECTION_UNAVAILABLE_MESSAGE)]
    Paused,
    /// Rolling-24h-volume quota exhausted for this direction, from a
    /// live, read-only on-chain check made right here at request time —
    /// see [`DIRECTION_UNAVAILABLE_MESSAGE`]. This specific check never
    /// itself sets any pause flag; it only reports the exhaustion up
    /// front instead of letting the deposit be accepted and fail later
    /// at on-chain release time. Separately, this service's own
    /// background tick (`crate::quota`) may ALSO have already engaged
    /// this direction's local pause once it observed the same
    /// exhaustion — in which case a caller may see [`ApiError::Paused`]
    /// instead on a later request, which (unlike the on-chain window's
    /// own automatic reset) never clears itself; see
    /// docs/09-runbook.md's 2026-08-22 update for the full workflow.
    /// Either way the end-user copy is identical.
    #[error("{}", DIRECTION_UNAVAILABLE_MESSAGE)]
    QuotaExhausted,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("could not read live chain state: {0}")]
    Upstream(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::InsufficientLiquidity { .. }
            | ApiError::Paused
            | ApiError::QuotaExhausted => StatusCode::CONFLICT,
            ApiError::Ledger(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Upstream(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Default page size for every cursor-paginated list endpoint, when
/// `?limit=` is omitted.
const DEFAULT_PAGE_LIMIT: u32 = 50;
/// Hard ceiling on page size — a client asking for more is not a 400
/// (it is a legitimate, if greedy, request), so the limit is silently
/// clamped down to this value rather than the request being rejected.
const MAX_PAGE_LIMIT: u32 = 200;

/// Everything the HTTP layer needs; implemented once against the real
/// ledger/chain ([`BridgeApi`]) and mockable for tests.
pub trait ApiSource: Send + Sync + 'static {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>>;
    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>>;
    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>>;
    fn create_glc_to_sol_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>>;
    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>>;
    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>>;
    fn health(&self) -> BoxFut<'_, Result<PublicHealth, ApiError>>;
    fn stats(&self) -> BoxFut<'_, Result<BridgeStats, ApiError>>;
    fn reserves_history(
        &self,
        direction: Option<ReserveDirection>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ReserveHistoryEntry>, ApiError>>;
    fn explorer_events(
        &self,
        direction: Option<Direction>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ExplorerEvent>, ApiError>>;
    /// Wallet-scoped "my activity" view — every transfer where
    /// `address` was either the `GlcToSol` destination or the `SolToGlc`
    /// depositor. `GET /transfers/:id` remains the id-based lookup; this
    /// is the address-based one a UI needs before it knows any request
    /// ids at all.
    fn list_transfers(
        &self,
        address: Option<[u8; 32]>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<TransferView>, ApiError>>;
    /// See [`RecipientEligibility`]. `address` is the raw user-entered
    /// Goldcoin destination address string; `wallet`, when given, is the
    /// connected Solana wallet's pubkey bytes (already base58-decoded by
    /// the query parser — never the raw string), checked against the
    /// source-wallet limit alongside the recipient limit.
    fn sol_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 32]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>>;
}

/// The concrete [`ApiSource`]: a fresh [`Ledger`] connection per call
/// (same concurrency model as [`crate::ops::collector::OpsCollector`] —
/// SQLite's own `BEGIN IMMEDIATE` transactions are the real safety
/// boundary, not a single shared in-process handle) plus a live chain
/// read for the handful of fields ([`BridgeStatus`]/[`TransferLimits`])
/// that only the on-chain `BridgeConfig` actually knows.
pub struct BridgeApi<SR: SolanaRpc> {
    db_path: PathBuf,
    solana_rpc: SR,
    vault_address: String,
    /// The root 2-of-3 vault (unmodified signer set/threshold) — used
    /// ONLY to derive a fresh request-specific deposit vault per new
    /// `GlcToSol` request (`goldcoin::derivation::derive_request_vault`,
    /// Step 1). Never itself the destination of a new request's deposit
    /// instructions; never used to sign anything here.
    root_vault: crate::goldcoin::vault::MultisigVault,
    goldcoin_network: crate::goldcoin::address::Network,
    reservation_ttl_secs: i64,
    goldcoin_confirmation_depth: i64,
    goldcoin_indexer_status: Arc<IndexerStatus>,
    solana_indexer_status: Arc<IndexerStatus>,
}

impl<SR: SolanaRpc> BridgeApi<SR> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db_path: PathBuf,
        solana_rpc: SR,
        vault_address: String,
        root_vault: crate::goldcoin::vault::MultisigVault,
        goldcoin_network: crate::goldcoin::address::Network,
        reservation_ttl_secs: i64,
        goldcoin_confirmation_depth: i64,
        goldcoin_indexer_status: Arc<IndexerStatus>,
        solana_indexer_status: Arc<IndexerStatus>,
    ) -> Self {
        BridgeApi {
            db_path,
            solana_rpc,
            vault_address,
            root_vault,
            goldcoin_network,
            reservation_ttl_secs,
            goldcoin_confirmation_depth,
            goldcoin_indexer_status,
            solana_indexer_status,
        }
    }

    fn open_ledger(&self) -> Result<Ledger, ApiError> {
        Ok(Ledger::open(&self.db_path)?)
    }

    /// Shared `BridgeRequest -> TransferView` projection, used by both
    /// [`ApiSource::get_transfer`] and [`ApiSource::list_transfers`] so
    /// the two never drift apart in what they expose.
    fn to_transfer_view(
        &self,
        ledger: &Ledger,
        request: crate::ledger::BridgeRequest,
    ) -> Result<TransferView, ApiError> {
        let destination_txid = ledger
            .get_destination_txid(request.id)?
            .map(|bytes| glc_hex::encode(&bytes));
        let required_source_confirmations = match request.direction {
            Direction::GlcToSol => Some(self.goldcoin_confirmation_depth),
            Direction::SolToGlc => None,
        };
        Ok(TransferView {
            id: request.id,
            direction: request.direction.as_str().to_string(),
            state: request.state.as_str().to_string(),
            gross_amount_atomic: request.gross_amount_atomic,
            fee_bps: request.fee_bps,
            fee_amount_atomic: request.fee_amount_atomic,
            net_amount_atomic: request.net_amount_atomic,
            created_at: request.created_at,
            source_txid: request.source_txid.map(|t| glc_hex::encode(&t)),
            source_confirmations: request.source_confirmations,
            required_source_confirmations,
            destination_txid,
            failure_reason: request.failure_reason,
        })
    }

    async fn fetch_bridge_config(&self) -> Result<accounts::BridgeConfigSnapshot, ApiError> {
        let account = self
            .solana_rpc
            .get_account(&accounts::bridge_config_pda())
            .await
            .map_err(|e| ApiError::Upstream(e.to_string()))?
            .ok_or_else(|| ApiError::Upstream("bridge_config account does not exist yet".into()))?;
        accounts::decode_bridge_config(&account.data).map_err(|e| ApiError::Upstream(e.to_string()))
    }

    /// Live rolling-24h-volume headroom remaining for one direction's
    /// window (`0` = release/`GlcToSol`, `1` = deposit/`SolToGlc` — see
    /// [`accounts::rolling_volume_window_pda`]), as of right now. A
    /// read-only chain read plus [`accounts::rolling_volume_remaining`]'s
    /// pure projection — never mutates anything, never itself a pause.
    async fn fetch_rolling_volume_remaining(
        &self,
        direction_byte: u8,
        config: &accounts::BridgeConfigSnapshot,
    ) -> Result<u64, ApiError> {
        let account = self
            .solana_rpc
            .get_account(&accounts::rolling_volume_window_pda(direction_byte))
            .await
            .map_err(|e| ApiError::Upstream(e.to_string()))?
            .ok_or_else(|| {
                ApiError::Upstream("rolling_volume_window account does not exist yet".into())
            })?;
        let window = accounts::decode_rolling_volume_window(&account.data)
            .map_err(|e| ApiError::Upstream(e.to_string()))?;
        Ok(accounts::rolling_volume_remaining(
            config.rolling_volume_limit,
            config.rolling_window_seconds,
            window,
            now_unix(),
        ))
    }
}

impl<SR: SolanaRpc + Send + Sync + 'static> ApiSource for BridgeApi<SR> {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let config = self.fetch_bridge_config().await?;
            let goldcoin_paused = ledger.is_paused(ReserveDirection::GoldcoinReserve)?;
            let solana_paused = ledger.is_paused(ReserveDirection::SolanaReserve)?;
            // Both admission axes, ANDed: either one being closed means
            // a new obligation would park, so either one must show here
            // as "not accepting new transfers". Read-only — neither this
            // endpoint nor any other read path ever evaluates (and so
            // never moves) the automatic gate.
            let sol_to_glc_admission_open = !ledger
                .is_admission_closed(ReserveDirection::GoldcoinReserve)?
                && !ledger.is_liquidity_admission_closed(ReserveDirection::GoldcoinReserve)?;
            let glc_to_sol_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(0, &config).await?;
            let sol_to_glc_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(1, &config).await?;
            let glc_to_sol_quota_exhausted =
                glc_to_sol_rolling_volume_remaining < config.min_transfer_amount;
            let sol_to_glc_quota_exhausted =
                sol_to_glc_rolling_volume_remaining < config.min_transfer_amount;
            let glc_to_sol_available = !solana_paused
                && !glc_to_sol_quota_exhausted
                && ledger.available_capacity(ReserveDirection::SolanaReserve)? > 0;
            let sol_to_glc_available = !goldcoin_paused
                && sol_to_glc_admission_open
                && !sol_to_glc_quota_exhausted
                && ledger.available_capacity(ReserveDirection::GoldcoinReserve)? > 0;
            Ok(BridgeStatus {
                goldcoin_paused,
                solana_paused,
                vault_address: self.vault_address.clone(),
                next_solana_obligation_index: config.obligation_count,
                glc_to_sol_available,
                sol_to_glc_available,
                glc_to_sol_quota_exhausted,
                sol_to_glc_quota_exhausted,
                glc_to_sol_rolling_volume_remaining,
                sol_to_glc_rolling_volume_remaining,
                sol_to_glc_admission_open,
            })
        })
    }

    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>> {
        Box::pin(async move {
            let config = self.fetch_bridge_config().await?;
            Ok(TransferLimits {
                min_transfer_amount: config.min_transfer_amount,
                per_transfer_limit: config.per_transfer_limit,
                bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, Result<PublicHealth, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let manual_review_backlog: u64 = [Direction::GlcToSol, Direction::SolToGlc]
                .iter()
                .map(|&d| {
                    ledger
                        .requests_by_state(d, RequestState::ManualReview)
                        .map(|r| r.len() as u64)
                        .unwrap_or(0)
                })
                .sum();
            let goldcoin_indexer_halted = self.goldcoin_indexer_status.is_halted();
            let post_finality_reorg_events = ledger.post_finality_reorg_event_count()?;
            Ok(PublicHealth {
                healthy: !goldcoin_indexer_halted && post_finality_reorg_events == 0,
                goldcoin_indexer_halted,
                manual_review_backlog,
                post_finality_reorg_events,
            })
        })
    }

    fn stats(&self) -> BoxFut<'_, Result<BridgeStats, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let config = self.fetch_bridge_config().await?;
            let goldcoin_paused = ledger.is_paused(ReserveDirection::GoldcoinReserve)?;
            let solana_paused = ledger.is_paused(ReserveDirection::SolanaReserve)?;
            let glc_to_sol_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(0, &config).await?;
            let sol_to_glc_rolling_volume_remaining =
                self.fetch_rolling_volume_remaining(1, &config).await?;
            let glc_to_sol_quota_exhausted =
                glc_to_sol_rolling_volume_remaining < config.min_transfer_amount;
            let sol_to_glc_quota_exhausted =
                sol_to_glc_rolling_volume_remaining < config.min_transfer_amount;
            let glc_to_sol_available = !solana_paused
                && !glc_to_sol_quota_exhausted
                && ledger.available_capacity(ReserveDirection::SolanaReserve)? > 0;
            let sol_to_glc_available = !goldcoin_paused
                && !sol_to_glc_quota_exhausted
                && ledger.available_capacity(ReserveDirection::GoldcoinReserve)? > 0;

            let glc_to_sol = direction_stats(ledger.request_state_counts(Direction::GlcToSol)?);
            let sol_to_glc = direction_stats(ledger.request_state_counts(Direction::SolToGlc)?);

            let goldcoin_reserve = ReserveStats {
                paused: goldcoin_paused,
                available_capacity: ledger.available_capacity(ReserveDirection::GoldcoinReserve)?,
                settled_volume_atomic: ledger
                    .settled_liquidity(ReserveDirection::GoldcoinReserve)?,
                accrued_fees_atomic: ledger.accrued_fees(ReserveDirection::GoldcoinReserve)?,
            };
            let solana_reserve = ReserveStats {
                paused: solana_paused,
                available_capacity: ledger.available_capacity(ReserveDirection::SolanaReserve)?,
                settled_volume_atomic: ledger.settled_liquidity(ReserveDirection::SolanaReserve)?,
                accrued_fees_atomic: ledger.accrued_fees(ReserveDirection::SolanaReserve)?,
            };

            let now = now_unix();
            Ok(BridgeStats {
                goldcoin_paused,
                solana_paused,
                glc_to_sol_available,
                sol_to_glc_available,
                glc_to_sol_quota_exhausted,
                sol_to_glc_quota_exhausted,
                glc_to_sol_rolling_volume_remaining,
                sol_to_glc_rolling_volume_remaining,
                bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                glc_to_sol,
                sol_to_glc,
                goldcoin_reserve,
                solana_reserve,
                goldcoin_indexer_halted: self.goldcoin_indexer_status.is_halted(),
                goldcoin_indexer_seconds_since_tick: self
                    .goldcoin_indexer_status
                    .seconds_since_tick(now),
                solana_indexer_seconds_since_tick: self
                    .solana_indexer_status
                    .seconds_since_tick(now),
                post_finality_reorg_events: ledger.post_finality_reorg_event_count()?,
                as_of: now,
            })
        })
    }

    fn reserves_history(
        &self,
        direction: Option<ReserveDirection>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ReserveHistoryEntry>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let rows = ledger.reconciliation_findings_page(direction, cursor, limit)?;
            let next_cursor = if rows.len() as u32 == limit {
                rows.last().map(|r| r.id.to_string())
            } else {
                None
            };
            let items = rows
                .into_iter()
                .map(|r| ReserveHistoryEntry {
                    id: r.id,
                    direction: r.direction.as_str().to_string(),
                    detected_at: r.detected_at,
                    expected_atomic: r.expected,
                    observed_atomic: r.observed,
                    delta_atomic: r.delta,
                    classification: r.classification,
                    auto_paused: r.auto_paused,
                })
                .collect();
            Ok(Page {
                items,
                next_cursor,
                as_of: now_unix(),
            })
        })
    }

    fn explorer_events(
        &self,
        direction: Option<Direction>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<ExplorerEvent>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let rows = ledger.explorer_events_page(direction, state, cursor, limit)?;
            let next_cursor = if rows.len() as u32 == limit {
                rows.last().map(|r| r.id.to_string())
            } else {
                None
            };
            let items = rows
                .into_iter()
                .map(|r| ExplorerEvent {
                    id: r.id,
                    request_id: r.request_id,
                    direction: r.direction.as_str().to_string(),
                    from_state: r.from_state.map(|s| s.as_str().to_string()),
                    to_state: r.to_state.as_str().to_string(),
                    at: r.at,
                    reason: r.reason,
                })
                .collect();
            Ok(Page {
                items,
                next_cursor,
                as_of: now_unix(),
            })
        })
    }

    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            Ok(ReserveAvailability {
                goldcoin_available_capacity: ledger
                    .available_capacity(ReserveDirection::GoldcoinReserve)?,
                solana_available_capacity: ledger
                    .available_capacity(ReserveDirection::SolanaReserve)?,
            })
        })
    }

    fn create_glc_to_sol_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>> {
        Box::pin(async move {
            if input.amount_atomic == 0 {
                return Err(ApiError::BadRequest("amount_atomic must be > 0".into()));
            }
            let recipient = input
                .recipient
                .parse::<Pubkey>()
                .map_err(|e| ApiError::BadRequest(format!("invalid recipient: {e}")))?;
            // `input.amount_atomic` is the caller-declared GROSS amount,
            // canonical units (Goldcoin-native) — the fee/net breakdown is
            // computed authoritatively HERE, server-side, at the fixed
            // protocol rate; nothing about it is accepted from the caller
            // (docs/20-bridge-fee.md: "never trust gross, fee or net
            // calculations supplied by the UI"). `CreateTransferInput` has
            // no fee/net field for exactly this reason — there is nothing
            // for a client to submit that could bypass or alter the fee.
            let fee_breakdown = amount_conversion::compute_fee(amount_conversion::CanonicalAtomic(
                input.amount_atomic,
            ))
            .map_err(|e| ApiError::BadRequest(format!("invalid amount: {e}")))?;
            let config = self.fetch_bridge_config().await?;
            let solana_decimals =
                accounts::fetch_reserve_mint_decimals(&self.solana_rpc, &config.reserve_token_mint)
                    .await
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
            let net_destination = fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                ApiError::BadRequest(format!(
                    "amount {} cannot be represented exactly after the bridge fee at the \
                     reserve mint's {solana_decimals}-decimal precision: {e}",
                    input.amount_atomic
                ))
            })?;
            let amounts = crate::ledger::RequestAmounts {
                gross_atomic: fee_breakdown.gross.0,
                fee_bps: fee_breakdown.fee_bps,
                fee_atomic: fee_breakdown.fee.0,
                net_atomic: fee_breakdown.net.0,
                net_destination_atomic: net_destination.0,
            };
            // Proactive rolling-24h-volume check, GlcToSol = release =
            // direction byte 0 (see `accounts::rolling_volume_window_pda`
            // docs). Without this, a deposit could be accepted here (off-
            // chain capacity reserved, Goldcoin funds requested from the
            // user) only to fail later at actual on-chain `release_from_
            // reserve` time when the same quota is checked for real — this
            // check can never be MORE permissive than that real check
            // (same limit, same window, same read), only catches the
            // rejection earlier, before the user has sent anything.
            let glc_to_sol_remaining = self.fetch_rolling_volume_remaining(0, &config).await?;
            if net_destination.0 > glc_to_sol_remaining {
                return Err(ApiError::QuotaExhausted);
            }
            let mut ledger = self.open_ledger()?;
            let now = now_unix();
            let outcome = ledger.create_request(
                Direction::GlcToSol,
                amounts,
                &recipient.to_bytes(),
                None,
                self.reservation_ttl_secs,
                now,
            )?;
            match outcome {
                CreateRequestOutcome::Reserved { request_id } => {
                    // Unique per-request deposit address (Step 1's pure
                    // derivation + Step 2's ledger support) — replaces
                    // the shared static vault address + OP_RETURN
                    // binding for every NEW request from here on. The
                    // legacy static-vault/OP_RETURN path keeps working
                    // for requests that already exist; it is simply
                    // never used again for a request created through
                    // this endpoint.
                    let derived_vault = crate::goldcoin::derivation::derive_request_vault(
                        &self.root_vault,
                        request_id,
                        self.goldcoin_network,
                    )
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
                    let mut ledger_for_address = self.open_ledger()?;
                    ledger_for_address.set_glc_to_sol_deposit_address(
                        request_id,
                        derived_vault.address(),
                        &derived_vault.script_pubkey_hex(),
                        &derived_vault.redeem_script_hex(),
                    )?;
                    Ok(CreateTransferOutput {
                        request_id,
                        deposit_address: derived_vault.address().to_string(),
                    })
                }
                CreateRequestOutcome::InsufficientLiquidity { available_capacity } => {
                    Err(ApiError::InsufficientLiquidity {
                        available: available_capacity,
                    })
                }
                CreateRequestOutcome::Paused => Err(ApiError::Paused),
            }
        })
    }

    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let Some(request) = ledger.get_request(id)? else {
                return Ok(None);
            };
            Ok(Some(self.to_transfer_view(&ledger, request)?))
        })
    }

    fn list_transfers(
        &self,
        address: Option<[u8; 32]>,
        state: Option<RequestState>,
        cursor: Option<i64>,
        limit: u32,
    ) -> BoxFut<'_, Result<Page<TransferView>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let requests = ledger.transfers_page(address, state, cursor, limit)?;
            let next_cursor = if requests.len() as u32 == limit {
                requests.last().map(|r| r.id.to_string())
            } else {
                None
            };
            let mut items = Vec::with_capacity(requests.len());
            for request in requests {
                items.push(self.to_transfer_view(&ledger, request)?);
            }
            Ok(Page {
                items,
                next_cursor,
                as_of: now_unix(),
            })
        })
    }

    fn sol_to_glc_recipient_eligibility(
        &self,
        address: String,
        wallet: Option<[u8; 32]>,
    ) -> BoxFut<'_, Result<RecipientEligibility, ApiError>> {
        Box::pin(async move {
            // Trimmed exactly as the UI trims before building the
            // on-chain instruction: the ledger's rate limit matches on
            // the raw bytes the obligation carried, so the bytes checked
            // here must be the bytes a deposit for this input would
            // actually carry.
            let address = address.trim().to_string();
            // Same acceptance rule as the payout path itself
            // (`goldcoin::payout_recovery`/`signing::goldcoin_vault`
            // decode a recipient with `decode_p2pkh` on this network) —
            // an address that could never be paid out gets a 400 here,
            // not a misleading eligibility verdict.
            crate::goldcoin::address::decode_p2pkh(&address, self.goldcoin_network)
                .map_err(|e| ApiError::BadRequest(format!("invalid Goldcoin address: {e}")))?;
            let ledger = self.open_ledger()?;
            let now = now_unix();
            // Checked wallet-first (see `RecipientEligibility::blocked_reason`'s
            // doc comment) — independent of the recipient check either
            // way, so this only decides which single reason is reported
            // when both would block.
            let wallet_retry_after = match wallet {
                Some(requester) => {
                    ledger.sol_to_glc_source_wallet_rate_limited_until(&requester, now)?
                }
                None => None,
            };
            let recipient_retry_after =
                ledger.sol_to_glc_recipient_rate_limited_until(address.as_bytes(), now)?;
            let (blocked_reason, retry_after) = match (wallet_retry_after, recipient_retry_after) {
                (Some(t), _) => (Some(BLOCKED_REASON_SOURCE_WALLET_RATE_LIMITED), Some(t)),
                (None, Some(t)) => (Some(BLOCKED_REASON_RECIPIENT_RATE_LIMITED), Some(t)),
                (None, None) => (None, None),
            };
            Ok(RecipientEligibility {
                direction: "SolToGlc".to_string(),
                address,
                wallet: wallet.map(|w| Pubkey::new_from_array(w).to_string()),
                eligible: retry_after.is_none(),
                blocked_reason: blocked_reason.map(str::to_string),
                retry_after,
                retry_after_seconds: retry_after.map(|t| (t - now).max(0)),
                window_seconds: Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS,
            })
        })
    }

    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>> {
        Box::pin(async move {
            let direction: Direction = input
                .direction
                .parse()
                .map_err(|e: String| ApiError::BadRequest(e))?;
            if input.gross_amount == 0 {
                return Err(ApiError::BadRequest("gross_amount must be > 0".into()));
            }
            let config = self.fetch_bridge_config().await?;
            let solana_decimals =
                accounts::fetch_reserve_mint_decimals(&self.solana_rpc, &config.reserve_token_mint)
                    .await
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
            let goldcoin_decimals = amount_conversion::GOLDCOIN_DECIMALS as u8;
            let (source_decimals, destination_decimals, source_asset, destination_asset) =
                match direction {
                    Direction::GlcToSol => (
                        goldcoin_decimals,
                        solana_decimals,
                        "GLC (Goldcoin)",
                        "GLC (Solana)",
                    ),
                    Direction::SolToGlc => (
                        solana_decimals,
                        goldcoin_decimals,
                        "GLC (Solana)",
                        "GLC (Goldcoin)",
                    ),
                };
            let fee_breakdown = amount_conversion::compute_fee(amount_conversion::CanonicalAtomic(
                input.gross_amount,
            ))
            .map_err(|e| ApiError::BadRequest(format!("invalid amount: {e}")))?;
            // Confirms the net entitlement is actually deliverable at the
            // destination chain's real precision — a quote must never
            // promise an amount a real transfer would then reject
            // (docs/20-bridge-fee.md).
            match direction {
                Direction::GlcToSol => {
                    fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                        ApiError::BadRequest(format!(
                            "amount {} cannot be represented exactly after the bridge fee at \
                             the reserve mint's {solana_decimals}-decimal precision: {e}",
                            input.gross_amount
                        ))
                    })?;
                }
                Direction::SolToGlc => {} // canonical == Goldcoin-native; always exact
            }
            Ok(QuoteOutput {
                direction: input.direction,
                gross_amount: fee_breakdown.gross.0,
                gross_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.gross.0,
                    goldcoin_decimals,
                ),
                fee_bps: fee_breakdown.fee_bps,
                fee_amount: fee_breakdown.fee.0,
                fee_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.fee.0,
                    goldcoin_decimals,
                ),
                net_amount: fee_breakdown.net.0,
                net_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.net.0,
                    goldcoin_decimals,
                ),
                source_decimals,
                destination_decimals,
                source_asset: source_asset.to_string(),
                destination_asset: destination_asset.to_string(),
            })
        })
    }
}

/// Renders an atomic amount as a fixed-point decimal string via checked
/// integer arithmetic only — never a float (docs/20-bridge-fee.md).
fn format_atomic_as_decimal_string(atomic: u64, decimals: u8) -> String {
    let scale = 10u64.pow(u32::from(decimals));
    let whole = atomic / scale;
    let frac = atomic % scale;
    format!("{whole}.{frac:0width$}", width = decimals as usize)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reduces a `(RequestState, count)` breakdown (`Ledger::
/// request_state_counts`) into [`DirectionStats`]'s summary shape.
fn direction_stats(counts: Vec<(RequestState, i64)>) -> DirectionStats {
    let mut total_requests = 0i64;
    let mut in_progress_requests = 0i64;
    let mut settled_requests = 0i64;
    let mut manual_review_requests = 0i64;
    for (state, count) in counts {
        total_requests += count;
        if state.is_active() {
            in_progress_requests += count;
        }
        match state {
            RequestState::Settled => settled_requests += count,
            RequestState::ManualReview => manual_review_requests += count,
            _ => {}
        }
    }
    DirectionStats {
        total_requests,
        in_progress_requests,
        settled_requests,
        manual_review_requests,
    }
}

/// Parses a raw HTTP query string (`a=1&b=2`) into a lookup map. No
/// percent-decoding: every query parameter this API accepts is a simple
/// enum name or a small non-negative integer, none of which ever need it
/// — a value that did would fail the corresponding typed parse below
/// (e.g. `direction`, `state`) or be rejected as a malformed cursor/limit,
/// rather than being silently misinterpreted.
fn parse_query_string(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        if !key.is_empty() {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

/// Shared `?cursor=`/`?limit=` parsing for every paginated list endpoint.
/// An absent or empty value falls back to the default; a present but
/// non-numeric value is a 400 (malformed pagination); a `limit` above
/// [`MAX_PAGE_LIMIT`] is silently clamped down to it, never rejected —
/// see [`MAX_PAGE_LIMIT`]'s own docs for why.
fn parse_page_params(
    q: &std::collections::HashMap<String, String>,
) -> Result<(Option<i64>, u32), ApiError> {
    let cursor = match q.get("cursor").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(
            s.parse::<i64>()
                .map_err(|_| ApiError::BadRequest("cursor must be an integer".into()))?,
        ),
    };
    let limit = match q.get("limit").map(String::as_str) {
        Some("") | None => DEFAULT_PAGE_LIMIT,
        Some(s) => {
            let n: u32 = s
                .parse()
                .map_err(|_| ApiError::BadRequest("limit must be a positive integer".into()))?;
            if n == 0 {
                return Err(ApiError::BadRequest("limit must be >= 1".into()));
            }
            n.min(MAX_PAGE_LIMIT)
        }
    };
    Ok((cursor, limit))
}

/// `?direction=` for `GET /reserves/history` — the reserve-level axis
/// (`ReserveDirection`), matching `glc-admin`'s own `goldcoin`/`solana`
/// CLI convention rather than the internal `GoldcoinReserve`/
/// `SolanaReserve` enum spelling.
fn parse_reserves_history_query(
    query: Option<&str>,
) -> Result<(Option<ReserveDirection>, Option<i64>, u32), ApiError> {
    let q = parse_query_string(query);
    let direction = match q.get("direction").map(String::as_str) {
        Some("") | None => None,
        Some("goldcoin") => Some(ReserveDirection::GoldcoinReserve),
        Some("solana") => Some(ReserveDirection::SolanaReserve),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unknown direction {other:?} (expected goldcoin|solana)"
            )))
        }
    };
    let (cursor, limit) = parse_page_params(&q)?;
    Ok((direction, cursor, limit))
}

/// `(direction, to_state, cursor, limit)` — parsed `/explorer/events`
/// query parameters.
type ExplorerEventsQuery = (Option<Direction>, Option<RequestState>, Option<i64>, u32);

/// `(address, state, cursor, limit)` — parsed `GET /transfers` query
/// parameters.
type ListTransfersQuery = (Option<[u8; 32]>, Option<RequestState>, Option<i64>, u32);

/// `?address=`/`?state=` for `GET /transfers` — `address` is a base58
/// Solana pubkey (same spelling `POST /transfers`'s `recipient` field
/// already accepts), `state` a `RequestState` filter.
/// `(address, wallet)` for `GET /recipients/sol-to-glc/eligibility` —
/// `address` required, passed through raw for
/// [`ApiSource::sol_to_glc_recipient_eligibility`] to validate as a
/// Goldcoin address; `wallet` optional, a base58 Solana pubkey (same
/// spelling `GET /transfers`'s `?address=` already accepts) parsed here
/// so the trait boundary carries real bytes, never an unvalidated string.
/// `parse_query_string`'s no-percent-decoding rule holds here too: both a
/// base58check address and a base58 pubkey are purely alphanumeric, so a
/// value that would need decoding is not valid and fails validation,
/// never gets misread.
fn parse_recipient_eligibility_query(
    query: Option<&str>,
) -> Result<(String, Option<[u8; 32]>), ApiError> {
    let q = parse_query_string(query);
    let address = match q.get("address").map(String::as_str) {
        Some("") | None => {
            return Err(ApiError::BadRequest(
                "address query parameter is required".into(),
            ))
        }
        Some(s) => s.to_string(),
    };
    let wallet = match q.get("wallet").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(
            s.parse::<Pubkey>()
                .map_err(|e| ApiError::BadRequest(format!("invalid wallet: {e}")))?
                .to_bytes(),
        ),
    };
    Ok((address, wallet))
}

fn parse_list_transfers_query(query: Option<&str>) -> Result<ListTransfersQuery, ApiError> {
    let q = parse_query_string(query);
    let address = match q.get("address").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(
            s.parse::<Pubkey>()
                .map_err(|e| ApiError::BadRequest(format!("invalid address: {e}")))?
                .to_bytes(),
        ),
    };
    let state = match q.get("state").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.parse::<RequestState>().map_err(ApiError::BadRequest)?),
    };
    let (cursor, limit) = parse_page_params(&q)?;
    Ok((address, state, cursor, limit))
}

/// `?direction=`/`?state=` for `GET /explorer/events` — the transfer-level
/// axis (`Direction`) and an optional `RequestState` filter, both parsed
/// via their own `FromStr` (the same `"GlcToSol"`/`"SolToGlc"` spelling
/// `POST /quote` already accepts as input).
fn parse_explorer_events_query(query: Option<&str>) -> Result<ExplorerEventsQuery, ApiError> {
    let q = parse_query_string(query);
    let direction = match q.get("direction").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.parse::<Direction>().map_err(ApiError::BadRequest)?),
    };
    let state = match q.get("state").map(String::as_str) {
        Some("") | None => None,
        Some(s) => Some(s.parse::<RequestState>().map_err(ApiError::BadRequest)?),
    };
    let (cursor, limit) = parse_page_params(&q)?;
    Ok((direction, state, cursor, limit))
}

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

fn error_response(err: ApiError) -> Response<Full<Bytes>> {
    json_response(
        err.status(),
        &ErrorBody {
            error: err.to_string(),
        },
    )
}

async fn handle<S: ApiSource>(
    req: Request<hyper::body::Incoming>,
    source: Arc<S>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = match (&method, path.as_str()) {
        (&Method::GET, "/status") => match source.status().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/limits") => match source.limits().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserve") => match source.reserve().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/health") => match source.health().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/stats") => match source.stats().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserves/history") => {
            match parse_reserves_history_query(req.uri().query()) {
                Ok((direction, cursor, limit)) => {
                    match source.reserves_history(direction, cursor, limit).await {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, "/explorer/events") => {
            match parse_explorer_events_query(req.uri().query()) {
                Ok((direction, state, cursor, limit)) => {
                    match source
                        .explorer_events(direction, state, cursor, limit)
                        .await
                    {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, "/transfers") => match parse_list_transfers_query(req.uri().query()) {
            Ok((address, state, cursor, limit)) => {
                match source.list_transfers(address, state, cursor, limit).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                }
            }
            Err(e) => error_response(e),
        },
        (&Method::POST, "/transfers") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorBody {
                            error: "could not read request body".into(),
                        },
                    ))
                }
            };
            match serde_json::from_slice::<CreateTransferInput>(&body) {
                Ok(input) => match source.create_glc_to_sol_transfer(input).await {
                    Ok(v) => json_response(StatusCode::CREATED, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody {
                        error: format!("malformed request body: {e}"),
                    },
                ),
            }
        }
        (&Method::POST, "/quote") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorBody {
                            error: "could not read request body".into(),
                        },
                    ))
                }
            };
            match serde_json::from_slice::<QuoteInput>(&body) {
                Ok(input) => match source.quote(input).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody {
                        error: format!("malformed request body: {e}"),
                    },
                ),
            }
        }
        (&Method::GET, "/recipients/sol-to-glc/eligibility") => {
            match parse_recipient_eligibility_query(req.uri().query()) {
                Ok((address, wallet)) => {
                    match source
                        .sol_to_glc_recipient_eligibility(address, wallet)
                        .await
                    {
                        Ok(v) => json_response(StatusCode::OK, &v),
                        Err(e) => error_response(e),
                    }
                }
                Err(e) => error_response(e),
            }
        }
        (&Method::GET, p) if p.starts_with("/transfers/") => {
            let id_str = &p["/transfers/".len()..];
            match id_str.parse::<i64>() {
                Ok(id) => match source.get_transfer(id).await {
                    Ok(Some(v)) => json_response(StatusCode::OK, &v),
                    Ok(None) => json_response(
                        StatusCode::NOT_FOUND,
                        &ErrorBody {
                            error: format!("no transfer with id {id}"),
                        },
                    ),
                    Err(e) => error_response(e),
                },
                Err(_) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody {
                        error: "transfer id must be an integer".into(),
                    },
                ),
            }
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            &ErrorBody {
                error: "not found".into(),
            },
        ),
    };
    Ok(response)
}

/// Serves the bridge API until `shutdown` fires. No authentication and no
/// TLS termination here (same posture as [`crate::ops::health::serve`]) —
/// run this behind a reverse proxy that provides both if it is ever
/// reachable from outside a trusted network.
pub async fn serve<S: ApiSource>(
    addr: SocketAddr,
    source: Arc<S>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "bridge API listening");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("bridge API: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "bridge API: accept failed");
                        continue;
                    }
                };
                let source = Arc::clone(&source);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(req, Arc::clone(&source)));
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%peer, error = %e, "bridge API connection ended");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
