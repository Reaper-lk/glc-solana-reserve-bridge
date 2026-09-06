//! The Robinhood Network adapter — a deliberately inert stub.
//!
//! # What this type holds
//!
//! Nothing. No RPC client, no endpoint URL, no chain id, no token contract,
//! no mint, no decimals, no reserve address, no signer, no confirmation
//! depth. It is a zero-sized type.
//!
//! That is the design, not a placeholder. Every one of those values is
//! unresolved pending verified network information (see
//! `docs/30-robinhood-network-phase1.md` and the TODO list below), and a
//! guessed value here would be worse than an absent one: a wrong decimals
//! constant silently mis-scales amounts, a wrong finality depth silently
//! accepts reversible deposits, and a wrong token contract silently sends
//! funds to the wrong asset. An adapter that holds nothing cannot make any
//! of those mistakes.
//!
//! # Why it cannot be enabled by configuration
//!
//! [`ChainAdapter::capability`] here returns [`Capability::Unavailable`]
//! for every route unconditionally. There is no constructor parameter, no
//! config field, no environment variable and no ledger row that changes
//! that. Enabling a Robinhood route therefore requires editing this file —
//! a reviewable code change gated on the Phase-2 information — and not an
//! operator action, a deployment setting, or a database write.
//!
//! This is the third leg of [`crate::routes::RouteGate`]'s three-place AND,
//! and it is the leg that cannot be flipped from outside the repository.

use crate::chains::{Capability, ChainAdapter};
use crate::routes::{Chain, Route};

/// Chain parameters that must be supplied and verified before ANY Robinhood
/// settlement code is written. Recorded as a constant list rather than
/// prose so the Phase-2 work has an explicit, greppable checklist and so
/// nothing in this phase can quietly assume one of them.
///
/// Nothing reads this at runtime; it exists to be read by people.
pub const UNRESOLVED_CHAIN_PARAMETERS: &[&str] = &[
    "chain family (EVM / SVM / other) — determines adapter shape, address format, signing curve",
    "chain id / network id",
    "mainnet vs testnet endpoints, and whether a testnet exists",
    "L1 / L2 / rollup classification, settlement layer, and reorg or challenge window",
    "RPC endpoint(s) and authentication model",
    "GLC token contract address",
    "token standard, and any transfer hooks / freeze authority / fee-on-transfer behaviour",
    "token decimals",
    "reserve address and custody model (can 2-of-3 threshold custody be reproduced?)",
    "confirmation / finality rule before a deposit is irreversible",
    "block explorer URL templates",
    "fee model (gas token, who pays, whether the 300 bps bridge fee applies)",
    "reserve sizing: protected_minimum / target / warning / critical / per-transfer / rolling volume",
    "treasury withdrawal destination allowlist",
];

/// The inert Robinhood adapter. See the module docs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RobinhoodAdapter;

impl RobinhoodAdapter {
    pub const fn new() -> Self {
        RobinhoodAdapter
    }

    /// The single reason string this adapter ever reports. A constant so a
    /// test can assert the adapter is inert without matching on prose.
    pub const UNAVAILABLE_REASON: &'static str =
        "Robinhood Network support is not implemented: no chain parameters, RPC client, reserve \
         or signer are configured in this build";
}

impl ChainAdapter for RobinhoodAdapter {
    fn chain(&self) -> Chain {
        Chain::Robinhood
    }

    /// Unconditionally unavailable.
    ///
    /// Note the wildcard is intentional here, unlike everywhere else in
    /// this codebase: this adapter's answer must not become
    /// route-dependent by accident when a route variant is added. A future
    /// route that Robinhood should serve has to be added by deleting this
    /// method's unconditional return, which is exactly the reviewable
    /// change Phase 2 requires.
    fn capability(&self, _route: Route) -> Capability {
        Capability::unavailable(Self::UNAVAILABLE_REASON)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_unavailable_for_every_route_including_legacy_ones() {
        let adapter = RobinhoodAdapter::new();
        for route in Route::ALL {
            assert_eq!(
                adapter.capability(route),
                Capability::unavailable(RobinhoodAdapter::UNAVAILABLE_REASON),
                "the Robinhood adapter must report unavailable for {route:?}",
            );
        }
    }

    #[test]
    fn is_zero_sized_so_it_can_hold_no_chain_parameters() {
        // Not a style assertion: a zero-sized adapter provably carries no
        // endpoint, key, contract address or decimals value. If this ever
        // fails, someone has given the stub state, and the "cannot be
        // enabled by configuration" property in the module docs needs
        // re-examining.
        assert_eq!(std::mem::size_of::<RobinhoodAdapter>(), 0);
    }

    #[test]
    fn reports_its_own_chain() {
        assert_eq!(RobinhoodAdapter::new().chain(), Chain::Robinhood);
    }

    #[test]
    fn unresolved_parameter_checklist_is_present() {
        // Guards against the checklist being emptied without the
        // corresponding chain support actually being built.
        assert!(UNRESOLVED_CHAIN_PARAMETERS.len() >= 14);
    }
}
