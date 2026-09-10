//! The single source of truth for "would this reserve admit new demand
//! right now, and if not, which gate refused".
//!
//! # Why this module exists
//!
//! Before it, the admission AND was written out three times — once in
//! [`Ledger::fold_sol_deposit`], once in
//! [`Ledger::fold_robinhood_deposit`], and (partially, and for one
//! route only) in the public API's `sol_to_glc_admission_open`. The
//! production incident that produced this module was the gap between the
//! second and the third: `GET /chains` reported `RhnToGlc` as
//! `enabled: true` — which is a statement about
//! [`crate::routes::RouteGate`], and was correct — while
//! `reserve_ledger.admission_closed` was set on `GoldcoinReserve`, so
//! every newly observed Robinhood deposit folded straight into
//! `ManualReview` with `admission_closed_at_fold`. A user could, and
//! did, make an irreversible on-chain deposit through a UI that had been
//! told the route was available.
//!
//! An `RhnToGlc` deposit is made directly to the custody contract; there
//! is no `POST /transfers` preflight in front of it and there cannot be
//! one. The only defence is that the availability signal the UI reads is
//! computed from the SAME state the fold will later gate on. So the
//! evaluation lives here, once, and every caller — both folds and the
//! public API — routes through it.
//!
//! # What is in scope, and what deliberately is not
//!
//! In scope: the DIRECTION-WIDE gates, i.e. everything that depends only
//! on the destination reserve's own state.
//!
//! - `paused` (operator pause, and the quota auto-pause `crate::quota`
//!   engages),
//! - `admission_closed` (the operator-only admission switch),
//! - `liquidity_admission_closed` (the automatic confirmed-liquidity
//!   gate's hysteresis state),
//! - the confirmed-liquidity admission safety buffer's per-request
//!   arithmetic,
//! - the mature-UTXO pool floor (`utxo_pool_min_available_count`),
//! - the plain capacity check.
//!
//! Not in scope: anything keyed on an identity rather than on the
//! reserve. The two rolling-24h limits are per-recipient and per-source-
//! wallet, so a route-level answer cannot evaluate them at all — they are
//! passed IN as [`InboundRateLimits`] by the fold, which knows the
//! addresses, and are reported to a UI by
//! `GET /recipients/{sol,rhn}-to-glc/eligibility` instead. Keeping them
//! as an input rather than moving them here is what lets one ranking
//! function serve both callers without either inventing a limit the
//! other does not apply.
//!
//! Also not in scope: the route gate itself
//! ([`crate::routes::RouteGate`]), the Robinhood custody contract's own
//! `routeEnabled`/pause flags, the destination's deliverability, and the
//! Solana program's on-chain rolling-volume window. Those are separate
//! gates with separate owners; see [`InboundAdmissionGates::read`]'s docs
//! for what a caller still has to check for itself.

use rusqlite::Connection;

use super::{Ledger, LedgerError, ReserveDirection};

/// The per-identity rolling-24h limits, supplied by a caller that knows
/// the recipient and the source wallet.
///
/// [`Default`] is "neither limit applies", which is the only honest
/// answer a route-level caller can give: it has no address to ask about.
/// A route-level `available` therefore says nothing about whether a
/// PARTICULAR user is inside a cooldown window, exactly as it says
/// nothing about whether their destination address parses — and the
/// eligibility endpoints exist to answer that half.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InboundRateLimits {
    pub source_wallet_rate_limited: bool,
    pub recipient_rate_limited: bool,
}

/// Which admission gate refused, ranked most specific first.
///
/// The variant order IS the ranking [`InboundAdmissionGates::blocker`]
/// applies, and it reproduces the `else if` chain both folds used to
/// spell out separately — so the same situation still produces the same
/// `manual_review_note` on either route, and now cannot stop doing so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundAdmissionBlocker {
    /// An operator has closed admission on this reserve
    /// (`glc-admin close-admission`). Never automatic.
    AdmissionClosed,
    /// The reserve's own local pause.
    ReservePaused,
    SourceWalletRateLimited,
    RecipientRateLimited,
    /// The mature, unreserved vault UTXO pool is at or below
    /// `utxo_pool_min_available_count`.
    UtxoLiquidityLow,
    /// Confirmed unreserved headroom is inside the admission safety
    /// buffer, either direction-wide (the hysteresis gate is closed) or
    /// for this particular amount.
    LiquidityBufferLow,
    /// The accounting figure itself is exhausted.
    InsufficientCapacity,
}

impl InboundAdmissionBlocker {
    /// The `bridge_requests.manual_review_note` a fold records for this
    /// blocker — read from [`Ledger`]'s reason constants, never
    /// re-spelled, so the note strings and the ranking that chooses
    /// between them live next to each other.
    pub fn manual_review_note(self) -> &'static str {
        match self {
            InboundAdmissionBlocker::AdmissionClosed => {
                Ledger::MANUAL_REVIEW_REASON_ADMISSION_CLOSED
            }
            InboundAdmissionBlocker::ReservePaused => Ledger::MANUAL_REVIEW_REASON_PAUSED,
            InboundAdmissionBlocker::SourceWalletRateLimited => {
                Ledger::MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED
            }
            InboundAdmissionBlocker::RecipientRateLimited => {
                Ledger::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED
            }
            InboundAdmissionBlocker::UtxoLiquidityLow => {
                Ledger::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW
            }
            InboundAdmissionBlocker::LiquidityBufferLow => {
                Ledger::MANUAL_REVIEW_REASON_LIQUIDITY_BUFFER_LOW
            }
            InboundAdmissionBlocker::InsufficientCapacity => {
                Ledger::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY
            }
        }
    }
}

/// One reserve's admission-relevant state, read at a single instant.
///
/// A plain data snapshot with no connection inside it: the fold reads it
/// from within its own write transaction (so the state a decision was
/// made against and the decision itself commit or roll back together),
/// while the API reads it from a read-only connection. Both then call
/// the same [`InboundAdmissionGates::blocker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundAdmissionGates {
    pub paused: bool,
    pub admission_closed: bool,
    /// The confirmed-liquidity gate's CURRENT hysteresis state. The fold
    /// passes the value it just re-evaluated and persisted; a read-only
    /// caller passes the persisted one. Never re-derived here — the
    /// hysteresis rule lives in
    /// [`Ledger::next_liquidity_admission_closed`] and must stay there.
    pub liquidity_admission_closed: bool,
    /// `total_reserve_balance - protected_minimum - reserved_liquidity`.
    pub confirmed_headroom_atomic: i64,
    /// The admission safety buffer's close threshold; `0` disables it.
    pub admission_buffer_atomic: i64,
    /// `0` disables the mature-UTXO-pool floor.
    pub min_available_utxo_count: i64,
    /// The live mature, unreserved UTXO count. Always `0` when
    /// `min_available_utxo_count` is `0` (the query is skipped, since
    /// the floor short-circuits) and for every reserve other than
    /// `GoldcoinReserve`, whose vault pool it is.
    pub available_utxo_count: i64,
}

impl InboundAdmissionGates {
    /// Reads every direction-wide gate for `reserve`.
    ///
    /// `liquidity_admission_closed` is an INPUT rather than a read
    /// because the two callers legitimately want different instants of
    /// it: [`Ledger::fold_sol_deposit`] and
    /// [`Ledger::fold_robinhood_deposit`] re-evaluate the hysteresis
    /// against current headroom and persist the transition inside their
    /// own transaction, then pass that fresh value; a read-only caller
    /// must never move the gate and passes the persisted column. Making
    /// it a parameter is what keeps this function incapable of writing.
    ///
    /// # This is necessary, never sufficient
    ///
    /// A caller still owns every gate that is not a property of the
    /// reserve: the route gate, the destination's deliverability, the
    /// two rolling-24h limits (via [`InboundRateLimits`]), and — for
    /// anything touching the Robinhood custody contract — the contract's
    /// own `routeEnabled`/`depositsPaused`/`payoutsPaused`/`signerEpoch`,
    /// which this service does not control and cannot cache.
    pub(crate) fn read(
        conn: &Connection,
        reserve: ReserveDirection,
        liquidity_admission_closed: bool,
    ) -> Result<Self, LedgerError> {
        let (
            paused,
            admission_closed,
            min_available_utxo_count,
            balance,
            protected_minimum,
            reserved,
        ): (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT paused, admission_closed, utxo_pool_min_available_count,
                        total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => LedgerError::ReserveNotInitialized(reserve),
                other => LedgerError::Sqlite(other),
            })?;
        let (admission_buffer_atomic, _reopen_atomic, _persisted_closed) =
            Ledger::read_liquidity_admission_row(conn, reserve)?;
        // The mature-pool floor is a property of the GOLDCOIN vault, so
        // it is asked about only for the reserve that vault backs —
        // exactly the short-circuit
        // `Ledger::check_utxo_liquidity_for_admission` already applies,
        // rather than a second opinion about which reserves own a UTXO
        // pool. Skipped again when the floor is disabled, since
        // `min_available_utxo_count == 0` short-circuits the check
        // itself and the count would be read only to be ignored.
        let available_utxo_count =
            if reserve == ReserveDirection::GoldcoinReserve && min_available_utxo_count > 0 {
                Ledger::count_available_vault_utxos(conn)?
            } else {
                0
            };
        Ok(InboundAdmissionGates {
            paused: paused != 0,
            admission_closed: admission_closed != 0,
            liquidity_admission_closed,
            confirmed_headroom_atomic: balance - protected_minimum - reserved,
            admission_buffer_atomic,
            min_available_utxo_count: if reserve == ReserveDirection::GoldcoinReserve {
                min_available_utxo_count
            } else {
                0
            },
            available_utxo_count,
        })
    }

    /// The read-only form: reads the persisted hysteresis state rather
    /// than evaluating it, so it can never move the gate.
    pub(crate) fn read_persisted(
        conn: &Connection,
        reserve: ReserveDirection,
    ) -> Result<Self, LedgerError> {
        let (_buffer, _reopen, persisted_closed) =
            Ledger::read_liquidity_admission_row(conn, reserve).map_err(|e| match e {
                LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                    LedgerError::ReserveNotInitialized(reserve)
                }
                other => other,
            })?;
        Self::read(conn, reserve, persisted_closed)
    }

    /// Whether the mature-UTXO pool floor is satisfied.
    fn utxo_liquidity_ok(&self) -> bool {
        self.min_available_utxo_count == 0
            || self.available_utxo_count > self.min_available_utxo_count
    }

    /// Whether admitting `net_destination_atomic` would still leave the
    /// confirmed-liquidity admission safety buffer intact — the full
    /// required formula
    ///
    /// ```text
    /// balance >= protected_minimum + reserved_liquidity
    ///            + net_destination_atomic + buffer
    /// ```
    ///
    /// rearranged around the already-computed headroom.
    fn liquidity_buffer_ok(&self, net_destination_atomic: i64) -> bool {
        self.admission_buffer_atomic <= 0
            || self.confirmed_headroom_atomic - net_destination_atomic
                >= self.admission_buffer_atomic
    }

    /// **The admission decision.** `None` means every gate is open and
    /// this amount would be admitted; `Some(blocker)` names the
    /// highest-ranked gate that refused.
    ///
    /// The ranking is the variant order of
    /// [`InboundAdmissionBlocker`], which reproduces verbatim the
    /// `else if` chain both folds previously spelled out for themselves.
    pub fn blocker(
        &self,
        net_destination_atomic: i64,
        limits: InboundRateLimits,
    ) -> Option<InboundAdmissionBlocker> {
        if self.admission_closed {
            Some(InboundAdmissionBlocker::AdmissionClosed)
        } else if self.paused {
            Some(InboundAdmissionBlocker::ReservePaused)
        } else if limits.source_wallet_rate_limited {
            Some(InboundAdmissionBlocker::SourceWalletRateLimited)
        } else if limits.recipient_rate_limited {
            Some(InboundAdmissionBlocker::RecipientRateLimited)
        } else if !self.utxo_liquidity_ok() {
            Some(InboundAdmissionBlocker::UtxoLiquidityLow)
        } else if self.liquidity_admission_closed
            || !self.liquidity_buffer_ok(net_destination_atomic)
        {
            Some(InboundAdmissionBlocker::LiquidityBufferLow)
        } else if net_destination_atomic > self.confirmed_headroom_atomic {
            Some(InboundAdmissionBlocker::InsufficientCapacity)
        } else {
            None
        }
    }

    /// The ROUTE-level question: is there any amount at all this reserve
    /// would admit right now?
    ///
    /// Defined as [`Self::blocker`] at the smallest amount that can
    /// exist — one atomic unit — and with no rate limits, because a
    /// route-level caller has no address to evaluate them against. That
    /// is not an approximation of the amount-dependent gates, it is
    /// their exact weakest form: `net <= headroom` at `net = 1` is
    /// `headroom > 0`, and `headroom - net >= buffer` at `net = 1` is
    /// `headroom > buffer`. Deriving it by CALLING the real decision,
    /// rather than by re-stating those two inequalities, is what makes
    /// this incapable of drifting away from what a fold will do.
    ///
    /// So `None` here means "a minimum-sized deposit would be admitted",
    /// never "this specific deposit would be" — a large enough one can
    /// still be held back by the buffer or by capacity, and is then
    /// parked and refundable exactly as before.
    pub fn route_blocker(&self) -> Option<InboundAdmissionBlocker> {
        self.blocker(1, InboundRateLimits::default())
    }
}

#[cfg(test)]
mod tests;
