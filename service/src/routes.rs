//! Bridge route identity and the fail-closed route gate.
//!
//! # Why a `Route` type exists alongside `ledger::Direction`
//!
//! [`crate::ledger::Direction`] is the *settlement* axis: every reserve
//! mutation, every state-machine transition, every signer claim and every
//! row in `bridge_requests` is keyed by it. It has exactly two variants and
//! this module does not touch it.
//!
//! [`Route`] is the *admission* axis: the set of source→destination pairs
//! this deployment is willing to talk about at all, including ones that are
//! not implemented. It is a strict superset of `Direction`.
//!
//! The two are deliberately different types, and the conversion is
//! deliberately partial:
//!
//! ```text
//! Route::GlcToSol  ->  Some(Direction::GlcToSol)
//! Route::SolToGlc  ->  Some(Direction::SolToGlc)
//! Route::GlcToRhn  ->  None
//! Route::RhnToGlc  ->  None
//! ```
//!
//! [`Route::as_direction`] returning `None` is the load-bearing security
//! property of this whole phase. Every function that can move value —
//! `Ledger::create_request`, `Ledger::fold_sol_deposit`, every orchestrator
//! settlement phase, every attestation/vault claim builder — requires a
//! `Direction`. There is no total conversion from `Route` to `Direction`
//! and no `From` impl, so a Robinhood route cannot reach any of them: not
//! because a boolean was checked, but because the value needed to call them
//! cannot be constructed. A bypass would have to add a new `Direction`
//! variant, which is a compile error at every existing `match` in the
//! service.
//!
//! This is also why Phase 1 needs no `bridge_requests` schema change: no
//! Robinhood row can be constructed to insert, and the table's existing
//! `CHECK (direction IN ('GlcToSol','SolToGlc'))` remains a second,
//! independent backstop underneath the type system.
//!
//! # The three-place AND
//!
//! [`RouteGate::ensure_enabled`] admits a route only when ALL THREE of the
//! following independently say yes:
//!
//! 1. **Config** — [`RoutesConfig`], from the TOML file. A missing section,
//!    a missing field, or `false` all mean disabled.
//! 2. **Ledger** — the `bridge_routes` table (see [`crate::ledger::Ledger::
//!    route_enabled`]). A missing table, a missing row, or `enabled = 0`
//!    all mean disabled.
//! 3. **Adapter capability** — [`crate::chains::ChainAdapter::capability`].
//!    An adapter that is not operational for a route means disabled,
//!    regardless of what the other two say.
//!
//! Each gate fails closed on its own, and each is evaluated on every call —
//! none is cached. An operator cannot enable a Robinhood route by editing
//! config alone, by editing the database alone, or by both together: the
//! Phase-1 [`crate::chains::robinhood::RobinhoodAdapter`] has no chain
//! parameters, no RPC client and no signer, and reports
//! [`crate::chains::Capability::Unavailable`] unconditionally.
//!
//! # Legacy routes are enabled by construction, not by configuration
//!
//! `GlcToSol`/`SolToGlc` are production traffic that predates this module.
//! Their [`Route::default_enabled`] is `true`, so every gate above resolves
//! to "enabled" against an unmodified production config file and an
//! unmigrated production ledger — the existing Solana↔Goldcoin behaviour is
//! bit-for-bit unchanged. New routes default to `false`.
//!
//! That single `default_enabled` rule is what lets all three gates share
//! one fallback and lets Phase 2's `bridge_routes` migration seed legacy
//! rows to `1` and Robinhood rows to `0` without changing any behaviour.

use crate::chains::{Capability, ChainRegistry};
use crate::ledger::{Direction, Ledger, LedgerError};

/// A chain this deployment knows the name of. Knowing a chain's name says
/// nothing about whether any route to it is usable — see [`RouteGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Chain {
    Goldcoin,
    Solana,
    /// Robinhood Network. Every chain parameter (family, chain id, RPC,
    /// token contract, decimals, finality) is deliberately UNRESOLVED in
    /// this phase — see `docs/30-robinhood-network-phase1.md`. Nothing in
    /// this codebase may assume any of them.
    Robinhood,
}

impl Chain {
    pub fn as_str(self) -> &'static str {
        match self {
            Chain::Goldcoin => "goldcoin",
            Chain::Solana => "solana",
            Chain::Robinhood => "robinhood",
        }
    }

    /// Operator/UI-facing name. Not an identifier — never parse this.
    pub fn display_name(self) -> &'static str {
        match self {
            Chain::Goldcoin => "Goldcoin L1",
            Chain::Solana => "Solana",
            Chain::Robinhood => "Robinhood Network",
        }
    }

    pub const ALL: [Chain; 3] = [Chain::Goldcoin, Chain::Solana, Chain::Robinhood];
}

impl std::str::FromStr for Chain {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "goldcoin" => Ok(Chain::Goldcoin),
            "solana" => Ok(Chain::Solana),
            "robinhood" => Ok(Chain::Robinhood),
            other => Err(format!("unknown chain {other:?}")),
        }
    }
}

/// A source→destination pair this deployment can be asked about.
///
/// The wire spelling is the identifier: it appears in the public API, in
/// operator tooling, and (for legacy routes only) in `bridge_requests.
/// direction`. `GlcToRhn`/`RhnToGlc` follow the existing `GlcToSol`/
/// `SolToGlc` convention deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Route {
    GlcToSol,
    SolToGlc,
    /// Goldcoin L1 → Robinhood Network. **Disabled in this phase.**
    GlcToRhn,
    /// Robinhood Network → Goldcoin L1. **Disabled in this phase.**
    RhnToGlc,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::GlcToSol => "GlcToSol",
            Route::SolToGlc => "SolToGlc",
            Route::GlcToRhn => "GlcToRhn",
            Route::RhnToGlc => "RhnToGlc",
        }
    }

    pub fn source_chain(self) -> Chain {
        match self {
            Route::GlcToSol | Route::GlcToRhn => Chain::Goldcoin,
            Route::SolToGlc => Chain::Solana,
            Route::RhnToGlc => Chain::Robinhood,
        }
    }

    pub fn destination_chain(self) -> Chain {
        match self {
            Route::GlcToSol => Chain::Solana,
            Route::SolToGlc | Route::RhnToGlc => Chain::Goldcoin,
            Route::GlcToRhn => Chain::Robinhood,
        }
    }

    /// The settlement [`Direction`] this route executes as, or `None` if
    /// this route has no settlement machinery.
    ///
    /// **This is the type-level firewall described in the module docs.**
    /// `None` is not "not yet wired up" — it means no `Direction` value
    /// exists for this route, so none of the reserve/ledger/signing
    /// functions that require one can be called with it at all. Do not add
    /// a total conversion, a `From` impl, an `unwrap_or`, or a default
    /// here: each would convert a compile-time guarantee into a runtime
    /// check.
    pub fn as_direction(self) -> Option<Direction> {
        match self {
            Route::GlcToSol => Some(Direction::GlcToSol),
            Route::SolToGlc => Some(Direction::SolToGlc),
            Route::GlcToRhn | Route::RhnToGlc => None,
        }
    }

    /// What every gate resolves to when it holds no explicit opinion: an
    /// absent config section, an absent `bridge_routes` table, or an absent
    /// row. `true` only for the two routes that predate the route registry,
    /// so an unmodified production deployment is unaffected; `false` for
    /// everything else, so an unconfigured route is a disabled route.
    pub fn default_enabled(self) -> bool {
        match self {
            Route::GlcToSol | Route::SolToGlc => true,
            Route::GlcToRhn | Route::RhnToGlc => false,
        }
    }

    /// Whether this route existed before the route registry. Used only to
    /// document/justify [`Route::default_enabled`]; never itself a gate.
    pub fn is_legacy(self) -> bool {
        matches!(self, Route::GlcToSol | Route::SolToGlc)
    }

    pub const ALL: [Route; 4] = [
        Route::GlcToSol,
        Route::SolToGlc,
        Route::GlcToRhn,
        Route::RhnToGlc,
    ];
}

impl std::str::FromStr for Route {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GlcToSol" => Ok(Route::GlcToSol),
            "SolToGlc" => Ok(Route::SolToGlc),
            "GlcToRhn" => Ok(Route::GlcToRhn),
            "RhnToGlc" => Ok(Route::RhnToGlc),
            other => Err(format!("unknown route {other:?}")),
        }
    }
}

impl From<Direction> for Route {
    /// Widening a settlement direction to its route is always total and
    /// lossless — it is only the reverse ([`Route::as_direction`]) that is
    /// partial.
    fn from(direction: Direction) -> Route {
        match direction {
            Direction::GlcToSol => Route::GlcToSol,
            Direction::SolToGlc => Route::SolToGlc,
        }
    }
}

/// Which of the three independent gates refused, and the operator-facing
/// reason. The variant is deliberately reported (rather than collapsed into
/// one opaque "disabled") so an operator debugging a route that will not
/// open can tell config from database from adapter without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisabledBy {
    Config,
    Ledger,
    Adapter { reason: String },
}

impl DisabledBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisabledBy::Config => "config",
            DisabledBy::Ledger => "ledger",
            DisabledBy::Adapter { .. } => "adapter",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RouteGateError {
    /// The route is known but not open. Carries no chain detail, no
    /// balance, and no timing — a caller learns only that this route
    /// cannot be used and why in the coarsest terms.
    #[error("route {route} is not enabled ({}): {reason}", disabled_by.as_str())]
    Disabled {
        route: &'static str,
        disabled_by: DisabledBy,
        reason: String,
    },
    #[error("ledger error while resolving route state: {0}")]
    Ledger(#[from] LedgerError),
}

impl RouteGateError {
    /// Approved end-user copy for a route that exists but is not open yet.
    /// Deliberately cause-agnostic and free of any promise about when it
    /// opens — the same discipline as
    /// [`crate::api::DIRECTION_UNAVAILABLE_MESSAGE`].
    pub const UNAVAILABLE_MESSAGE: &'static str =
        "This route is not available yet.\nRobinhood Network support is in development and \
         cannot be used for transfers.";
}

/// Per-route enable flags as declared by the config file.
///
/// Built by [`crate::config::Config`]; a route with no explicit entry
/// resolves to [`Route::default_enabled`]. Deliberately not a `HashMap`
/// with a permissive `get`: the lookup is exhaustive over [`Route`], so
/// adding a route variant is a compile error here until its config
/// semantics are decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutesConfig {
    glc_to_sol: bool,
    sol_to_glc: bool,
    glc_to_rhn: bool,
    rhn_to_glc: bool,
}

impl Default for RoutesConfig {
    /// Every route at its [`Route::default_enabled`] value — i.e. exactly
    /// what an existing production config file (which names no routes at
    /// all) resolves to.
    fn default() -> Self {
        RoutesConfig {
            glc_to_sol: Route::GlcToSol.default_enabled(),
            sol_to_glc: Route::SolToGlc.default_enabled(),
            glc_to_rhn: Route::GlcToRhn.default_enabled(),
            rhn_to_glc: Route::RhnToGlc.default_enabled(),
        }
    }
}

impl RoutesConfig {
    pub fn enabled(&self, route: Route) -> bool {
        match route {
            Route::GlcToSol => self.glc_to_sol,
            Route::SolToGlc => self.sol_to_glc,
            Route::GlcToRhn => self.glc_to_rhn,
            Route::RhnToGlc => self.rhn_to_glc,
        }
    }

    /// Applies the optional `[robinhood]` config section's two flags. The
    /// legacy routes have no config surface at all and are not settable
    /// here — there is deliberately no way to express "turn off GlcToSol"
    /// in this struct, because that control already exists as the
    /// pause/admission machinery and must not gain a second, divergent
    /// spelling.
    pub fn with_robinhood(mut self, glc_to_rhn: bool, rhn_to_glc: bool) -> Self {
        self.glc_to_rhn = glc_to_rhn;
        self.rhn_to_glc = rhn_to_glc;
        self
    }
}

/// The single admission gate. Constructed once at startup and consulted on
/// every route-bearing request; holds no cached verdict.
pub struct RouteGate {
    config: RoutesConfig,
    registry: ChainRegistry,
}

impl RouteGate {
    pub fn new(config: RoutesConfig, registry: ChainRegistry) -> Self {
        RouteGate { config, registry }
    }

    /// A gate that admits exactly the two legacy routes — the resolved
    /// state of an unmodified production deployment.
    pub fn legacy_only() -> Self {
        RouteGate::new(RoutesConfig::default(), ChainRegistry::phase1())
    }

    pub fn config(&self) -> &RoutesConfig {
        &self.config
    }

    pub fn registry(&self) -> &ChainRegistry {
        &self.registry
    }

    /// The one function every entry point calls. Returns `Ok(())` only when
    /// config, ledger, and adapter capability all independently admit the
    /// route.
    ///
    /// Evaluation order is config → ledger → adapter, and it short-circuits;
    /// the order affects only which `disabled_by` an operator sees when more
    /// than one gate is closed, never whether the route opens.
    pub fn ensure_enabled(&self, ledger: &Ledger, route: Route) -> Result<(), RouteGateError> {
        // Gate 1 — config.
        if !self.config.enabled(route) {
            return Err(RouteGateError::Disabled {
                route: route.as_str(),
                disabled_by: DisabledBy::Config,
                reason: "not enabled in the service configuration".to_string(),
            });
        }

        // Gate 2 — persisted route state. A missing table or row resolves
        // to `Route::default_enabled`, so this is live today against an
        // unmigrated ledger and stays correct after the Phase-2 migration
        // seeds the table.
        if !ledger.route_enabled(route.as_str(), route.default_enabled())? {
            return Err(RouteGateError::Disabled {
                route: route.as_str(),
                disabled_by: DisabledBy::Ledger,
                reason: "disabled in the ledger's bridge_routes state".to_string(),
            });
        }

        // Gate 3 — adapter capability. Both chains must be operational for
        // this route: a route is only as usable as its weaker leg.
        for chain in [route.source_chain(), route.destination_chain()] {
            match self.registry.capability(chain, route) {
                Capability::Operational => {}
                Capability::Unavailable { reason } => {
                    return Err(RouteGateError::Disabled {
                        route: route.as_str(),
                        disabled_by: DisabledBy::Adapter {
                            reason: reason.clone(),
                        },
                        reason,
                    });
                }
            }
        }

        Ok(())
    }

    /// Non-failing form for read-only listings (`GET /chains`, `GET
    /// /status`). Never used to authorize anything — [`RouteGate::
    /// ensure_enabled`] is the only admission decision.
    pub fn is_enabled(&self, ledger: &Ledger, route: Route) -> bool {
        self.ensure_enabled(ledger, route).is_ok()
    }

    /// The reason a route is closed, for display. `None` when it is open.
    pub fn disabled_reason(&self, ledger: &Ledger, route: Route) -> Option<String> {
        match self.ensure_enabled(ledger, route) {
            Ok(()) => None,
            Err(RouteGateError::Disabled { .. }) => {
                Some(RouteGateError::UNAVAILABLE_MESSAGE.to_string())
            }
            // A ledger read failure is not a "reason this route is closed",
            // but it must never render as "open" either.
            Err(RouteGateError::Ledger(_)) => Some(RouteGateError::UNAVAILABLE_MESSAGE.to_string()),
        }
    }
}

#[cfg(test)]
mod tests;
