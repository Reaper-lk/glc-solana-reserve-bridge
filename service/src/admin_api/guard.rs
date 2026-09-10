//! The open-admission safety gate, extracted from `glc-admin
//! open-admission` so the CLI and the admin API run the IDENTICAL checks
//! — one implementation, two callers, no way for the two paths to drift.

use crate::ledger::{Direction, InboundAdmissionBlocker, Ledger, LedgerError, ReserveDirection};

/// Why [`open_admission_guarded`] did not open admission. The two cases
/// are deliberately distinct so the HTTP layer can keep its error
/// policy: a `Refused` is a validated, operator-facing safety refusal
/// (409 with the message verbatim — the same text `glc-admin` prints),
/// while `Ledger` is a storage failure that must go through
/// `AdminError::from(LedgerError)`'s redaction (a raw SQLite message can
/// embed the database path) and be reported as the 500 it is, never
/// dressed up as a refusal.
#[derive(Debug)]
pub enum OpenAdmissionError {
    Refused(String),
    Ledger(LedgerError),
}

impl std::fmt::Display for OpenAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenAdmissionError::Refused(message) => f.write_str(message),
            OpenAdmissionError::Ledger(e) => write!(f, "{e}"),
        }
    }
}

/// The three reserve-safety checks every "open something" guard in this
/// module runs, in one place so no caller can run a weaker subset.
///
/// `refusal_prefix` is the caller's own lead-in ("refusing to open
/// admission", "refusing to open admission for route GlcToRhn", ...), so
/// each guard keeps the message it has always printed while the CHECKS
/// stay literally the same three function calls:
///
/// 1. the hard reserve invariant ([`Ledger::check_invariant`]),
/// 2. the count-based mature-UTXO floor
///    ([`Ledger::check_utxo_liquidity_for_admission`]),
/// 3. the confirmed-liquidity admission buffer
///    ([`Ledger::check_liquidity_buffer_for_admission`]).
///
/// Checks 2 and 3 are `Ok(())` by construction for every reserve other
/// than `GoldcoinReserve` (they short-circuit inside the ledger, which
/// is where the "a UTXO pool is a Goldcoin concept" rule belongs); they
/// are still CALLED here rather than skipped by a match in this module,
/// so a future reserve that grows one of those concepts is covered
/// without anyone remembering to widen a list.
fn reserve_safety_checks(
    ledger: &mut Ledger,
    reserve: ReserveDirection,
    refusal_prefix: &str,
) -> Result<(), OpenAdmissionError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    ledger.check_invariant(reserve).map_err(|e| {
        OpenAdmissionError::Refused(format!(
            "{refusal_prefix}: {reserve:?}'s reserve invariant does not hold ({e})"
        ))
    })?;
    ledger
        .check_utxo_liquidity_for_admission(reserve, now)
        .map_err(|e: LedgerError| OpenAdmissionError::Refused(format!("{refusal_prefix}: {e}")))?;
    ledger
        .check_liquidity_buffer_for_admission(reserve, now)
        .map_err(|e: LedgerError| OpenAdmissionError::Refused(format!("{refusal_prefix}: {e}")))?;
    Ok(())
}

/// Re-opens admission for `direction` after three independent safety
/// checks, none weakening the others, all refusing unconditionally
/// (no override exists):
///
/// 1. The hard reserve invariant ([`Ledger::check_invariant`]) — so
///    admission is never re-opened onto an already-broken reserve.
/// 2. The same count-based UTXO-liquidity gate `fold_sol_deposit`
///    applies to a brand-new obligation
///    ([`Ledger::check_utxo_liquidity_for_admission`],
///    docs/09-runbook.md's "UTXO liquidity" section) — reopening
///    admission onto a mature UTXO pool still at or below the configured
///    floor would immediately re-admit exactly the demand backpressure
///    exists to hold back.
/// 3. The confirmed-liquidity admission buffer
///    ([`Ledger::check_liquidity_buffer_for_admission`],
///    docs/09-runbook.md's "Confirmed-liquidity admission safety
///    buffer") — while the automatic gate is still closed, clearing the
///    operator flag would change nothing observable: every new fold
///    would keep parking. Refusing with the headroom and the reopen
///    threshold in the message is strictly more useful than a
///    successful command that silently does nothing.
///
/// Only on all three passing does it call [`Ledger::set_admission`].
pub fn open_admission_guarded(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    note: &str,
) -> Result<(), OpenAdmissionError> {
    reserve_safety_checks(ledger, direction, "refusing to open admission")?;
    ledger
        .set_admission(direction, false, Some(note))
        .map_err(OpenAdmissionError::Ledger)
}

/// Re-opens ONE route's own admission gate, behind the SAME three
/// safety checks [`open_admission_guarded`] applies — deliberately
/// reusing them rather than restating a subset.
///
/// # Why the identical guards, on a narrower switch
///
/// Opening a route gate is opening admission: with the reserve-wide
/// switches already open, this one write is the difference between a
/// newly observed deposit parking in `ManualReview` and being admitted
/// against real reserve capacity. A route-scoped command with weaker
/// checks than the reserve-scoped one would therefore be a way to route
/// around `open-admission`'s guards entirely — close the reserve-wide
/// switch, open the route, and admit onto a broken invariant or a thin
/// UTXO pool that `open-admission` would have refused.
///
/// So the checks are not merely similar, they are the same function
/// calls against the route's own destination reserve:
///
/// 1. the hard reserve invariant ([`Ledger::check_invariant`]),
/// 2. the count-based mature-UTXO floor
///    ([`Ledger::check_utxo_liquidity_for_admission`]),
/// 3. the confirmed-liquidity admission buffer
///    ([`Ledger::check_liquidity_buffer_for_admission`]).
///
/// Closing has no guard, exactly as closing the reserve-wide switch has
/// none: refusing to close is refusing to stop taking deposits.
///
/// # What this does NOT do
///
/// It does not touch the reserve-wide `paused` or `admission_closed`
/// flags, and it cannot. A route whose reserve is paused stays closed
/// after this returns `Ok(())` — the two gates are ANDed by
/// [`crate::ledger::InboundAdmissionGates`] and neither can clear the
/// other. Nor does it touch enablement
/// ([`crate::routes::RouteGate`]), which is a separate axis.
pub fn open_route_admission_guarded(
    ledger: &mut Ledger,
    route: crate::routes::Route,
    note: &str,
) -> Result<(), OpenAdmissionError> {
    // The reserve this route settles out of — the same one its folds
    // gate against, so the guards below examine the pool that a
    // re-opened route would actually draw on. A route with no
    // `Direction` has no destination reserve and no admission gate;
    // `Ledger::set_route_admission` refuses it below, inside the audited
    // scope, so this returns the refusal rather than guessing a reserve.
    let Some(direction) = route.as_direction() else {
        return ledger
            .set_route_admission(route, false, Some(note))
            .map_err(OpenAdmissionError::Ledger);
    };
    let reserve = direction.destination_reserve();
    reserve_safety_checks(
        ledger,
        reserve,
        &format!("refusing to open admission for route {}", route.as_str()),
    )?;
    ledger
        .set_route_admission(route, false, Some(note))
        .map_err(OpenAdmissionError::Ledger)
}

/// Clears the LOCAL `RobinhoodReserve.paused` gate — the flag `GlcToRhn`
/// availability reads — behind the same reserve-safety checks every
/// other "open something" guard in this module runs, plus the
/// authoritative availability evaluator itself.
///
/// # What this flag is, and what it is not
///
/// It is `reserve_ledger.paused` on the `RobinhoodReserve` row: one term
/// of [`crate::ledger::InboundAdmissionGates`], and therefore one term of
/// the `available` verdict `GET /chains` publishes for `GlcToRhn`. It is
/// NOT the `GlcRobinhoodBridge` contract's `depositsPaused`/
/// `payoutsPaused` (governance, 2-of-3 quorum, `glc-admin
/// robinhood-governance-pause`), NOT the contract's `routeEnabled`, NOT
/// `bridge_routes` enablement (`glc-admin robinhood-route-enable`), and
/// NOT the config file's `[routes]` gate. Nothing here reads or writes
/// any of those, and clearing this one opens none of them.
///
/// `RhnToGlc` is unaffected in either direction: it settles out of
/// `GoldcoinReserve` ([`Direction::destination_reserve`]), so its pause
/// is `GoldcoinReserve`'s and stays exactly where it was.
///
/// # Why unpausing is guarded and pausing is not
///
/// Pausing is the emergency stop; refusing to stop taking demand is
/// never the safe answer, so [`crate::admin_api::
/// audited_set_robinhood_local_pause`] applies no check at all in that
/// direction. Clearing the flag is the opposite: with every other gate
/// already open it is the single write that turns `GlcToRhn` back on, so
/// it runs:
///
/// 1. [`reserve_safety_checks`] against `RobinhoodReserve` — the same
///    three calls `open_admission_guarded` makes, so this command can
///    never be the weak way around them;
/// 2. the availability evaluator itself, re-asked with `paused` cleared.
///
/// Step 2 is what makes this incapable of drifting from the answer the
/// public API publishes: it does not restate an inequality about
/// capacity, it calls [`crate::ledger::InboundAdmissionGates::
/// route_blocker`] — the same function [`crate::api`]'s
/// `route_availability` and `Ledger::fold_robinhood_deposit` reach — on
/// the real gate snapshot with only this one bit flipped. If what would
/// still refuse `GlcToRhn` is a CAPACITY/liquidity condition, the unpause
/// is refused: clearing an emergency stop onto a reserve that cannot
/// fund the route is exactly the state this guard exists to prevent.
///
/// A remaining OPERATOR switch (route admission, reserve admission) is
/// deliberately not a refusal — those are separate, deliberately-set
/// gates with their own audited commands, and refusing here would make
/// this command's success depend on state it must not touch.
pub fn unpause_robinhood_reserve_guarded(
    ledger: &mut Ledger,
    note: &str,
) -> Result<(), OpenAdmissionError> {
    const PREFIX: &str = "refusing to unpause the local RobinhoodReserve gate";
    let reserve = ReserveDirection::RobinhoodReserve;
    reserve_safety_checks(ledger, reserve, PREFIX)?;

    // The authoritative evaluator, asked the question this command is
    // about to make true: "with `paused` cleared, would GlcToRhn admit a
    // minimum-sized transfer?" Read through `Ledger`'s own public
    // snapshot (which never moves the confirmed-liquidity hysteresis),
    // so no gate is evaluated twice and none is re-derived here.
    let mut gates = ledger
        .inbound_admission_gates(Direction::GlcToRhn)
        .map_err(OpenAdmissionError::Ledger)?;
    gates.paused = false;
    // Only a CAPACITY/liquidity condition refuses. Every other blocker
    // is another operator switch with its own audited command
    // (`route-admission-open`, `open-admission`) or a per-address rate
    // limit no route-level answer can carry; unpausing is still the
    // right, and audited, thing to do while one of those stands.
    if let Some(
        blocker @ (InboundAdmissionBlocker::InsufficientCapacity
        | InboundAdmissionBlocker::LiquidityBufferLow
        | InboundAdmissionBlocker::UtxoLiquidityLow),
    ) = gates.route_blocker()
    {
        return Err(OpenAdmissionError::Refused(format!(
            "{PREFIX}: with the pause cleared, GlcToRhn would still be refused by {} \
             (confirmed headroom {}, reserved liquidity is already committed) — the \
             reserve cannot currently fund the route this flag gates",
            blocker.as_str(),
            gates.confirmed_headroom_atomic,
        )));
    }

    ledger
        .set_paused(reserve, false, Some(note))
        .map_err(OpenAdmissionError::Ledger)
}
