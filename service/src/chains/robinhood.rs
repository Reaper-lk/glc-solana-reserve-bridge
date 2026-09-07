//! The Robinhood Network adapter: the third gate in
//! [`crate::routes::RouteGate`]'s three-place AND, and the one that
//! cannot be opened from outside this process.
//!
//! # What it holds, and why that is the whole design
//!
//! Either nothing, or a [`VerifiedDeployment`] — and there is no third
//! option. [`RobinhoodAdapter::unavailable`] constructs the first;
//! [`RobinhoodAdapter::verified`] constructs the second and takes a
//! `VerifiedDeployment`, which has exactly one constructor:
//! [`crate::robinhood::preflight::verify`], the function that reads the
//! deployed contracts and refuses on any disagreement.
//!
//! So possessing an operational adapter IS the evidence that preflight
//! ran and passed against this exact deployment. It is not a boolean an
//! operator can set, not a config field, and not a database row: a
//! deployment that skipped preflight, or whose preflight failed, cannot
//! construct one, and every Robinhood route is therefore closed no
//! matter what its config file or its `bridge_routes` table say.
//!
//! Phase 1's version of this file held NOTHING and refused every route
//! unconditionally, because no chain parameter had been resolved. That
//! posture has not been relaxed into a flag — it has been replaced by a
//! stronger one: the parameters must now be read from the chain and must
//! agree with the configuration, every startup.
//!
//! # The Solana<->Robinhood routes are refused unconditionally
//!
//! `SolToRhn` and `RhnToSol` report [`Capability::Unavailable`] even from
//! a fully verified adapter, and that is deliberate rather than
//! incidental. No settlement machinery exists for either: they have no
//! [`crate::ledger::Direction`], the ledger's `bridge_requests.direction`
//! CHECK cannot spell them, and no code path can construct a request for
//! one. Reporting them as available would be a claim this build cannot
//! honour.
//!
//! Enabling one is therefore not a configuration change. It requires
//! adding a `Direction` variant (a compile error at every exhaustive
//! match in the service), a schema migration to widen the CHECK, and a
//! settlement leg for the Solana side — which is exactly the reviewable
//! change such a route deserves.

use crate::chains::{Capability, ChainAdapter};
use crate::robinhood::preflight::VerifiedDeployment;
use crate::routes::{Chain, Route};

/// Chain parameters that had to be supplied and verified before ANY
/// Robinhood settlement code could be written.
///
/// Phase 1 recorded this as an explicit, greppable checklist of unknowns.
/// Every entry has since been resolved, and the list is kept — with each
/// entry's resolution named — because a checklist that is deleted once it
/// is satisfied leaves nothing for a reviewer to check the resolution
/// against.
///
/// Nothing reads this at runtime; it exists to be read by people.
pub const RESOLVED_CHAIN_PARAMETERS: &[(&str, &str)] = &[
    ("chain family", "EVM; secp256k1; 20-byte addresses (crate::evm)"),
    (
        "chain id / network id",
        "4663 mainnet, 46630 testnet (crate::evm::networks), asserted against eth_chainId every          tick and at preflight",
    ),
    (
        "RPC endpoint(s) and authentication model",
        "operator-configured HTTP JSON-RPC; no websocket, no subscription",
    ),
    (
        "GLC token contract address",
        "read from the bridge's own TOKEN(), cross-checked against the configured expected_token          at preflight",
    ),
    (
        "token decimals",
        "18, asserted against the live decimals() at preflight; the amount model refuses to start          on any other value",
    ),
    (
        "reserve address and custody model",
        "the GlcRobinhoodBridge contract itself; 2-of-3 EIP-712 signer quorum, no admin key",
    ),
    (
        "confirmation / finality rule",
        "operator-configured depth, counted head - block + 1, separately for inbound observation          and outbound settlement",
    ),
    (
        "fee model",
        "gas paid by a dedicated submitter EOA that holds no bridge authority; the 300 bps bridge          fee applies in canonical units exactly as on every other route",
    ),
    (
        "transaction envelope",
        "operator-configured (no default) AND verified against the chain's own baseFeePerGas at          preflight — the one property this repository had no prior evidence for",
    ),
    (
        "reserve sizing",
        "[reserve.robinhood] bounds, in canonical units, accounted as a third independent reserve",
    ),
];

/// Properties this adapter's verification deliberately does NOT
/// establish, recorded so that nobody reads a successful preflight as
/// covering them.
///
/// Each needs a separate mainnet token review, against the token's
/// SOURCE and its governance — not against any value an `eth_call` can
/// return.
pub const UNVERIFIED_TOKEN_PROPERTIES: &[&str] = &[
    "absence of a mint authority",
    "absence of a blocklist or freeze capability",
    "absence of a transfer hook or fee-on-transfer behaviour",
    "absence of a pause",
    "absence of an upgradeable proxy behind the token address",
    "the token's total supply and its distribution",
];

/// The Robinhood adapter. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodAdapter {
    /// `None` = no verified deployment in this process, so every route is
    /// unavailable. There is no way to set this except by passing a
    /// [`VerifiedDeployment`], which only preflight can produce.
    deployment: Option<VerifiedDeployment>,
}

impl Default for RobinhoodAdapter {
    /// Unavailable. A default that was operational would make "forgot to
    /// run preflight" indistinguishable from "preflight passed".
    fn default() -> Self {
        RobinhoodAdapter::unavailable()
    }
}

impl RobinhoodAdapter {
    /// An adapter that refuses every route.
    pub const fn unavailable() -> Self {
        RobinhoodAdapter { deployment: None }
    }

    /// An adapter backed by a deployment that passed
    /// [`crate::robinhood::preflight::verify`].
    pub fn verified(deployment: VerifiedDeployment) -> Self {
        RobinhoodAdapter {
            deployment: Some(deployment),
        }
    }

    /// The verified deployment, if this adapter has one.
    pub fn deployment(&self) -> Option<&VerifiedDeployment> {
        self.deployment.as_ref()
    }

    /// The reason an unverified adapter gives. A constant so a test can
    /// assert the adapter is inert without matching on prose.
    pub const UNAVAILABLE_REASON: &'static str =
        "Robinhood Network settlement is not verified in this process: no [robinhood.settlement]          section, or its startup preflight against the deployed contracts did not pass";

    /// The reason the two Solana<->Robinhood routes give, even from a
    /// fully verified adapter.
    pub const NOT_IMPLEMENTED_REASON: &'static str =
        "this route has no settlement machinery in this build: it has no ledger Direction, and          the ledger's own direction CHECK cannot store one. Enabling it is a code change, not a          configuration change.";
}

impl ChainAdapter for RobinhoodAdapter {
    fn chain(&self) -> Chain {
        Chain::Robinhood
    }

    /// Operational only for the two EXECUTABLE routes, and only when a
    /// verified deployment is present.
    ///
    /// Exhaustive over [`Route`] rather than wildcarded: adding a route
    /// variant must be a compile error here, so that a new route cannot
    /// silently inherit either answer.
    fn capability(&self, route: Route) -> Capability {
        match route {
            // Not this chain's routes at all. An adapter is asked only
            // about routes touching its own chain, but answering
            // "unavailable" rather than panicking keeps the registry's
            // lookup total.
            Route::GlcToSol | Route::SolToGlc => {
                Capability::unavailable(Self::NOT_IMPLEMENTED_REASON)
            }
            // Structurally supported, permanently non-executable in this
            // build. Refused whether or not a deployment is verified.
            Route::SolToRhn | Route::RhnToSol => {
                Capability::unavailable(Self::NOT_IMPLEMENTED_REASON)
            }
            Route::GlcToRhn | Route::RhnToGlc => match &self.deployment {
                None => Capability::unavailable(Self::UNAVAILABLE_REASON),
                Some(_) => Capability::Operational,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::{EvmAddress, EvmChainId, TxEnvelope};
    use crate::robinhood::auth::ProtocolChainPair;

    fn deployment() -> VerifiedDeployment {
        VerifiedDeployment {
            chain_id: EvmChainId::new(4663).unwrap(),
            bridge_contract: EvmAddress::from_bytes([0xb1; 20]),
            token: EvmAddress::from_bytes([0x70; 20]),
            token_decimals: 18,
            signers: [
                EvmAddress::from_bytes([0xa1; 20]),
                EvmAddress::from_bytes([0xa2; 20]),
                EvmAddress::from_bytes([0xa3; 20]),
            ],
            domain_separator: [0x5a; 32],
            glc_to_rhn_chains: ProtocolChainPair {
                source: 1001,
                dest: 2001,
            },
            rhn_to_glc_chains: ProtocolChainPair {
                source: 2001,
                dest: 1001,
            },
            tx_envelope: TxEnvelope::Eip1559,
            chain_has_base_fee: true,
        }
    }

    #[test]
    fn an_unverified_adapter_refuses_every_route() {
        let adapter = RobinhoodAdapter::unavailable();
        for route in Route::ALL {
            assert!(
                !adapter.capability(route).is_operational(),
                "an unverified adapter must refuse {route:?}",
            );
        }
    }

    #[test]
    fn the_default_adapter_is_the_unverified_one() {
        // A default that was operational would make "forgot to run
        // preflight" indistinguishable from "preflight passed".
        assert_eq!(RobinhoodAdapter::default(), RobinhoodAdapter::unavailable());
        assert!(RobinhoodAdapter::default().deployment().is_none());
    }

    #[test]
    fn a_verified_adapter_admits_exactly_the_two_executable_routes() {
        let adapter = RobinhoodAdapter::verified(deployment());
        assert!(adapter.capability(Route::GlcToRhn).is_operational());
        assert!(adapter.capability(Route::RhnToGlc).is_operational());
    }

    /// The property this whole phase preserves: `SolToRhn`/`RhnToSol`
    /// remain non-executable even from a fully verified deployment.
    #[test]
    fn the_solana_robinhood_routes_stay_refused_even_when_verified() {
        let adapter = RobinhoodAdapter::verified(deployment());
        for route in [Route::SolToRhn, Route::RhnToSol] {
            assert_eq!(
                adapter.capability(route),
                Capability::unavailable(RobinhoodAdapter::NOT_IMPLEMENTED_REASON),
                "{route:?} must stay non-executable",
            );
            // And the deeper guarantee underneath it: no Direction
            // exists, so no value-moving function can be called at all.
            assert_eq!(route.as_direction(), None);
        }
    }

    #[test]
    fn the_legacy_solana_routes_are_not_this_adapters_business() {
        let adapter = RobinhoodAdapter::verified(deployment());
        for route in [Route::GlcToSol, Route::SolToGlc] {
            assert!(!adapter.capability(route).is_operational());
        }
    }

    #[test]
    fn reports_its_own_chain() {
        assert_eq!(RobinhoodAdapter::unavailable().chain(), Chain::Robinhood);
        assert_eq!(
            RobinhoodAdapter::verified(deployment()).chain(),
            Chain::Robinhood
        );
    }

    #[test]
    fn the_resolved_parameter_checklist_is_present_and_names_the_envelope_decision() {
        // The Phase-1 checklist recorded fourteen unknowns. Its
        // replacement records how each was resolved — including the one
        // this repository had no prior evidence for.
        assert!(RESOLVED_CHAIN_PARAMETERS.len() >= 10);
        let envelope = RESOLVED_CHAIN_PARAMETERS
            .iter()
            .find(|(name, _)| *name == "transaction envelope")
            .expect("the envelope decision must be recorded");
        assert!(envelope.1.contains("verified"), "{}", envelope.1);
        assert!(envelope.1.contains("no default"), "{}", envelope.1);
    }

    /// A successful preflight must never be read as a token audit. The
    /// list of what it does NOT establish is part of the deliverable.
    #[test]
    fn the_unverified_token_properties_are_recorded() {
        assert!(UNVERIFIED_TOKEN_PROPERTIES.len() >= 5);
        for expected in ["mint authority", "blocklist", "upgradeable"] {
            assert!(
                UNVERIFIED_TOKEN_PROPERTIES
                    .iter()
                    .any(|p| p.contains(expected)),
                "{expected} must be named as NOT established by preflight",
            );
        }
    }
}
