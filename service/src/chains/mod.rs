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
/// Note there is deliberately no Solana↔Robinhood arm: direct bridging
/// between the two non-Goldcoin chains is out of scope, and this adapter
/// refusing it is one of the places that is structurally true rather than
/// merely undocumented.
#[derive(Debug, Default)]
pub struct SolanaAdapter;

impl ChainAdapter for SolanaAdapter {
    fn chain(&self) -> Chain {
        Chain::Solana
    }

    fn capability(&self, route: Route) -> Capability {
        match route {
            Route::GlcToSol | Route::SolToGlc => Capability::Operational,
            Route::GlcToRhn | Route::RhnToGlc => {
                Capability::unavailable("the Solana adapter does not serve Robinhood routes")
            }
        }
    }
}

/// The Goldcoin L1 leg. Operational for the two legacy routes; explicitly
/// NOT operational for the Robinhood routes, even though Goldcoin is one of
/// their two legs — the Goldcoin side of a Robinhood transfer would need
/// reserve accounting, payout construction and vault signing that this
/// phase does not build.
#[derive(Debug, Default)]
pub struct GoldcoinAdapter;

impl ChainAdapter for GoldcoinAdapter {
    fn chain(&self) -> Chain {
        Chain::Goldcoin
    }

    fn capability(&self, route: Route) -> Capability {
        match route {
            Route::GlcToSol | Route::SolToGlc => Capability::Operational,
            Route::GlcToRhn | Route::RhnToGlc => Capability::unavailable(
                "the Goldcoin adapter has no Robinhood-side reserve, payout or signing support",
            ),
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

    /// The Phase-1 registry: real Solana and Goldcoin adapters, plus the
    /// permanently-unavailable Robinhood stub.
    pub fn phase1() -> Self {
        ChainRegistry::new()
            .with(Box::new(GoldcoinAdapter))
            .with(Box::new(SolanaAdapter))
            .with(Box::new(RobinhoodAdapter::new()))
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
