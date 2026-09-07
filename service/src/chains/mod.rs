//! Chain adapters: the third and last of the three independent gates in
//! [`crate::routes::RouteGate`].
//!
//! # Scope of this trait in Phase 1
//!
//! [`ChainAdapter`] deliberately carries ONE method beyond identity:
//! [`ChainAdapter::capability`]. It has no `reserve_balance`, no
//! `submit_release`, no `confirmations`, no RPC handle and no signer.
//!
//! That is not an oversight and not a stub to fill in later by reflex.
//! Those methods cannot be designed without the Robinhood chain parameters
//! that are explicitly unresolved in this phase (chain family, finality
//! model, token standard, decimals — see
//! `docs/30-robinhood-network-phase1.md`), and adding unimplemented
//! signatures now would bake in guesses about all four. The settlement
//! surface is Phase 2 work, designed against verified network information.
//!
//! What this trait DOES give Phase 1 is the property that matters: a
//! third, independent place where a route must be affirmatively declared
//! operational, which the Robinhood adapter can never satisfy because it
//! holds nothing with which to operate.
//!
//! # Solana and Goldcoin adapters wrap nothing
//!
//! [`SolanaAdapter`] and [`GoldcoinAdapter`] do not wrap, re-implement, or
//! re-route any existing settlement code. The Solana↔Goldcoin machinery
//! (`orchestrator`, `solana::*`, `goldcoin::*`, `signing::*`) is untouched
//! by this module and does not call into it. They exist here only so the
//! registry is total over [`Chain`] and so the capability gate has a real
//! answer for the legacy routes rather than a special case.

use std::collections::BTreeMap;

use crate::routes::{Chain, Route};

pub mod robinhood;

pub use robinhood::RobinhoodAdapter;

/// Whether an adapter can currently serve a route.
///
/// `Unavailable` carries an operator-facing reason. It is never a
/// recoverable/retryable signal — an adapter that is unavailable for a
/// route is unavailable until the deployment changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    Operational,
    Unavailable { reason: String },
}

impl Capability {
    pub fn unavailable(reason: impl Into<String>) -> Capability {
        Capability::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn is_operational(&self) -> bool {
        matches!(self, Capability::Operational)
    }
}

/// One chain's participation in the route gate.
pub trait ChainAdapter: Send + Sync {
    fn chain(&self) -> Chain;

    /// Whether this chain can currently act as a leg of `route`.
    ///
    /// Called for BOTH legs of every route on every admission decision
    /// (`RouteGate::ensure_enabled`), so it must be cheap and must not
    /// perform I/O. It is a statement about this deployment's
    /// capabilities, not about live chain health — liveness is the
    /// indexers'/reconciliation's job and has its own separate pause
    /// machinery.
    fn capability(&self, route: Route) -> Capability;
}

/// The Solana leg. Operational for the two legacy routes it has always
/// served, and for nothing else.
///
/// `SolToRhn`/`RhnToSol` now EXIST as routes, and this adapter still
/// refuses them. That is the point: the custody contract models the two
/// Solana↔Robinhood routes structurally and ships them disabled, so the
/// service must be able to NAME them without being able to serve them.
/// Refusing here is structurally true rather than merely undocumented —
/// the Solana adapter has no Robinhood-side reserve, payout construction
/// or settlement machinery of any kind.
#[derive(Debug, Default)]
pub struct SolanaAdapter;

impl ChainAdapter for SolanaAdapter {
    fn chain(&self) -> Chain {
        Chain::Solana
    }

    fn capability(&self, route: Route) -> Capability {
        match route {
            Route::GlcToSol | Route::SolToGlc => Capability::Operational,
            Route::GlcToRhn | Route::RhnToGlc | Route::SolToRhn | Route::RhnToSol => {
                Capability::unavailable("the Solana adapter does not serve Robinhood routes")
            }
        }
    }
}

/// The Goldcoin L1 leg.
///
/// # What "capable" means here, and what it does not
///
/// This adapter answers ONE question: can the Goldcoin machinery in this
/// build serve the Goldcoin leg of this route? It is not an enablement
/// switch and it opens nothing — [`crate::routes::RouteGate`] consults it
/// as the third of three independent gates, and for anything touching the
/// Robinhood custody contract there are further gates this service does
/// not control at all (the contract's own `routeEnabled`, its pause
/// flags, its `signerEpoch`), plus preflight, the signer quorum, reserve
/// availability and the local pause. `Operational` here means "the code
/// exists", never "the route is open".
///
/// It follows that the answer must be TRUE rather than aspirational. An
/// adapter that reported a route it cannot actually serve would move a
/// structural guarantee into a runtime failure — the same reasoning
/// [`crate::chains::robinhood::RobinhoodAdapter`] records for the two
/// Solana<->Robinhood routes.
///
/// # Why the two Robinhood routes differ
///
/// They are not symmetric, because Goldcoin plays a different role in
/// each.
///
/// - **`RhnToGlc`** — Goldcoin is the DESTINATION. Settling it means
///   building and broadcasting a vault payout, and
///   `Orchestrator::tick_goldcoin_payouts` already sweeps every direction
///   whose `destination_is_goldcoin()`, which includes this one. The
///   payout plan, coin selection, fee policy, 2-of-3 vault signing and
///   broadcast are the SAME machinery a `SolToGlc` payout uses — not a
///   second implementation. So this leg is genuinely served.
///
/// - **`GlcToRhn`** — Goldcoin is the SOURCE. Serving it means accepting
///   a Goldcoin deposit against a per-request vault address and promoting
///   it to `SourceFinalized`. That pipeline is now route-aware end to
///   end, and is the SAME pipeline `GlcToSol` uses rather than a second
///   one: `Ledger::set_goldcoin_deposit_address`,
///   `Ledger::find_goldcoin_deposit_request_by_script`,
///   `Ledger::record_glc_deposit_observed`,
///   `Ledger::all_goldcoin_deposit_addresses` (which is what puts the
///   address on the node's watch list), the reorg sweeps, the
///   coin-selection exclusion that keeps an unfinalized deposit
///   unspendable, and `goldcoin::indexer`'s `promote_confirming` all key
///   on [`crate::ledger::Direction::source_is_goldcoin`]. So a
///   `GlcToRhn` request is assigned a deposit address at creation, its
///   deposit is matched by script alone, and it reaches
///   `SourceFinalized` under the identical confirmation depth,
///   exact-amount and duplicate rules.
///
///   What is NOT claimed by this answer: that the route is open (it is
///   not — `Route::GlcToRhn.default_enabled()` is `false`), or that a
///   `GlcToRhn` deposit parked in `ManualReview` can be refunded
///   automatically. `Ledger::glc_refund_db_checks` still refuses that
///   direction by name, because its no-settlement-has-begun proof reads
///   Solana columns a Robinhood payout never writes. That gap is a
///   refusal, not a silent hole. See
///   `docs/33-robinhood-admin-phase-g.md`.
#[derive(Debug, Default)]
pub struct GoldcoinAdapter;

impl GoldcoinAdapter {
    /// The reason the two Solana<->Robinhood routes are refused: neither
    /// leg is Goldcoin, so this adapter is not a party to them at all.
    pub const NOT_A_GOLDCOIN_ROUTE_REASON: &'static str =
        "neither leg of this route is Goldcoin L1";
}

impl ChainAdapter for GoldcoinAdapter {
    fn chain(&self) -> Chain {
        Chain::Goldcoin
    }

    /// Exhaustive over [`Route`] rather than wildcarded: adding a route
    /// variant must be a compile error here, so a new route cannot
    /// silently inherit either answer.
    fn capability(&self, route: Route) -> Capability {
        match route {
            // The two legacy routes, unchanged and untouched.
            Route::GlcToSol | Route::SolToGlc => Capability::Operational,
            // Goldcoin is the DESTINATION: the existing vault payout
            // machinery already serves it. Capability only — every route
            // gate still stands in front of it.
            Route::RhnToGlc => Capability::Operational,
            // Goldcoin is the SOURCE, and the deposit pipeline that
            // serves it is the same one `GlcToSol` uses, keyed on
            // `Direction::source_is_goldcoin` rather than on a single
            // direction. Capability only — same caveat as above.
            Route::GlcToRhn => Capability::Operational,
            // Not this adapter's routes at all. An adapter is only asked
            // about routes touching its own chain, but answering rather
            // than panicking keeps the registry's lookup total.
            Route::SolToRhn | Route::RhnToSol => {
                Capability::unavailable(Self::NOT_A_GOLDCOIN_ROUTE_REASON)
            }
        }
    }
}

/// Every chain this deployment knows about, keyed by [`Chain`].
///
/// Total by construction: [`ChainRegistry::capability`] treats an absent
/// chain as unavailable rather than as an error or a default-allow, so a
/// registry that was built wrong closes routes instead of opening them.
pub struct ChainRegistry {
    adapters: BTreeMap<Chain, Box<dyn ChainAdapter>>,
}

impl ChainRegistry {
    pub fn new() -> Self {
        ChainRegistry {
            adapters: BTreeMap::new(),
        }
    }

    pub fn with(mut self, adapter: Box<dyn ChainAdapter>) -> Self {
        self.adapters.insert(adapter.chain(), adapter);
        self
    }

    /// The registry a deployment with no verified Robinhood settlement
    /// has: real Solana and Goldcoin adapters, plus a Robinhood adapter
    /// that refuses every route.
    ///
    /// This is what an unmodified production deployment resolves to, and
    /// what every existing caller gets: the Solana<->Goldcoin behaviour
    /// is unchanged, and no Robinhood route can open.
    pub fn legacy_only() -> Self {
        ChainRegistry::new()
            .with(Box::new(GoldcoinAdapter))
            .with(Box::new(SolanaAdapter))
            .with(Box::new(RobinhoodAdapter::unavailable()))
    }

    /// The former name of [`ChainRegistry::legacy_only`], kept because it
    /// is the spelling every existing call site uses.
    pub fn phase1() -> Self {
        ChainRegistry::legacy_only()
    }

    /// The registry for a deployment whose Robinhood settlement passed
    /// [`crate::robinhood::preflight::verify`].
    ///
    /// Takes the `VerifiedDeployment` rather than a boolean, so the ONLY
    /// way to build an operational Robinhood adapter is to have actually
    /// verified one — see `crate::chains::robinhood`'s module docs.
    pub fn with_verified_robinhood(
        deployment: crate::robinhood::preflight::VerifiedDeployment,
    ) -> Self {
        ChainRegistry::new()
            .with(Box::new(GoldcoinAdapter))
            .with(Box::new(SolanaAdapter))
            .with(Box::new(RobinhoodAdapter::verified(deployment)))
    }

    pub fn capability(&self, chain: Chain, route: Route) -> Capability {
        match self.adapters.get(&chain) {
            Some(adapter) => adapter.capability(route),
            // Fail closed: an unregistered chain can serve nothing.
            None => Capability::unavailable(format!(
                "no adapter is registered for chain {}",
                chain.as_str()
            )),
        }
    }

    pub fn contains(&self, chain: Chain) -> bool {
        self.adapters.contains_key(&chain)
    }

    pub fn chains(&self) -> impl Iterator<Item = Chain> + '_ {
        self.adapters.keys().copied()
    }
}

impl Default for ChainRegistry {
    fn default() -> Self {
        ChainRegistry::phase1()
    }
}

#[cfg(test)]
mod tests;
