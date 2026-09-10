//! Per-route bridge fees: exactly one rate per executable route,
//! resolved once at config load and never defaulted afterwards.
//!
//! # What this replaces
//!
//! The bridge used to price everything at one number. The rate lived in
//! [`crate::amount_conversion::BRIDGE_FEE_BPS`], a compiled-in constant,
//! and [`crate::amount_conversion::compute_fee`] applied it to whatever
//! it was handed. When Robinhood arrived under different commercial
//! terms, [`crate::chain_policy`] added a per-CHAIN rate on top —
//! `ChainPolicies::fee_bps_for(Chain)` — and the Robinhood fold started
//! using it.
//!
//! That left the bridge with two fee mechanisms and one number reaching
//! places the other was supposed to own. Concretely, before this module
//! existed:
//!
//! - `POST /transfers` priced a **`GlcToRhn`** transfer at the compiled-in
//!   global rate, not at the configured Robinhood rate.
//! - `GET /quote` quoted **every** direction at the global rate, so a
//!   Robinhood quote and the request it turned into disagreed.
//! - `GET /robinhood/limits` reported the global rate as the fee
//!   "`robinhood::fold` charges", which it was not.
//!
//! All three were the same bug: a rate that belongs to one route being
//! read from somewhere that does not know which route is being priced.
//!
//! # The rule
//!
//! **Every executable route resolves exactly one `fee_bps`, by
//! `Route`.** There is no fallback at request time, no per-chain default
//! and no global constant behind it: [`RouteFees::fee_bps`] returns a
//! rate that was explicitly resolved for THAT route, or it fails.
//!
//! Falling back is a *config-load* concern, and it happens once, there
//! (see [`crate::config`]'s `[fees]` handling), producing a table with an
//! entry for every executable route. After load, a missing entry is an
//! error and never a number — because the alternative is pricing a
//! Robinhood transfer at Solana's rate, which is exactly what this
//! module exists to make impossible.
//!
//! # Non-executable routes cannot have a fee
//!
//! [`RouteFees::insert`] refuses any route without a
//! [`Route::as_direction`]. `SolToRhn`/`RhnToSol` have no settlement
//! machinery, so a fee for one would be a price on something that cannot
//! move value — a claim nothing else in the system could honour, exactly
//! like an `enabled = 1` row in `bridge_routes` for the same routes. The
//! refusal is here as well as in the config parser so a future caller
//! that builds a table by hand cannot bypass it, and it is a narrowing
//! rather than a gate: refusing a fee does not make a route executable or
//! non-executable, it only refuses to state a price for one that isn't.
//!
//! # A rate is configuration, and it is validated by RANGE
//!
//! Any rate in [`MIN_FEE_BPS`]`..=`[`MAX_FEE_BPS`] may be configured for
//! any executable route, and changing between them is a config edit and
//! whatever restart that deployment already needs — **never a binary
//! rebuild**. `4%` is a fee like any other.
//!
//! The bounds are not product policy; they are where the arithmetic and
//! the transfer stop making sense:
//!
//! - **`0` is allowed.** `fee = 0`, `net = gross`. A free route is a real
//!   commercial choice, and nothing downstream treats it specially.
//! - **`10_000` (100%) is refused.** `fee = gross`, so `net = 0` on every
//!   single transfer: a route that can never deliver anything is not a
//!   route, and pricing one would be configuring requests that cannot
//!   settle. `9_999` (99.99%) is the largest rate that still delivers
//!   something, and is therefore the maximum.
//! - **Above `10_000` is refused by the arithmetic itself**, one layer
//!   down: `net = gross - fee` on `u64` would underflow, i.e. the net
//!   entitlement would be negative
//!   ([`crate::amount_conversion::compute_fee_at_bps`]).
//!
//! # What used to be here
//!
//! Every rate used to additionally be checked against a compiled-in
//! allowlist of the rates the protocol had previously charged. It meant a
//! fee was partly a CODE artefact: moving a route to a rate nobody had
//! used before required editing and releasing the binary for a value that
//! lives in a config file. That allowlist is gone from every runtime
//! path, and no equivalent of it exists anywhere else.
//!
//! The fail-closed protection it was standing next to is unchanged and is
//! the one that actually catches a tampered ledger row:
//! [`crate::amount_conversion::verify_fee_breakdown`] requires a stored
//! request's gross, rate, fee and net to reconcile EXACTLY, and every
//! settlement is built from the freshly recomputed figures rather than the
//! stored ones. See docs/20-bridge-fee.md.

use std::collections::BTreeMap;

use crate::amount_conversion::BPS_DENOMINATOR;
use crate::routes::Route;

pub mod edit;

/// Why a per-route fee was refused.
///
/// Every variant names the route, because a fee error with no route in it
/// is unreadable the moment there is more than one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeeError {
    #[error(
        "{route} has no settlement machinery in this build (Route::as_direction is None), so it \
         cannot be priced. Configuring a fee for it would state a price for a path that cannot \
         move value; remove it from [fees]"
    )]
    RouteNotExecutable { route: &'static str },

    #[error(
        "{route}: fee_bps {fee_bps} is not a usable rate. {max} basis points is the maximum \
         ({max_percent}); at {denominator} bps (100%) the fee would consume the whole gross \
         amount and every transfer on this route would deliver nothing, and above that the net \
         entitlement would be negative"
    )]
    FeeBpsOutOfRange {
        route: &'static str,
        fee_bps: u64,
        max: u64,
        max_percent: &'static str,
        denominator: u64,
    },

    #[error("{route}: a fee for this route was declared more than once — declare it exactly once")]
    DuplicateFee { route: &'static str },

    #[error(
        "{route} is executable but no fee is configured for it. Every executable route must \
         resolve exactly one rate: add `{route} = <bps>` to the config's [fees] table. Nothing \
         here falls back to another route's rate or to a global default"
    )]
    MissingFee { route: &'static str },
}

/// The lowest configurable rate: zero, i.e. a free route.
///
/// Allowed deliberately. `fee = 0` and `net = gross` is arithmetically
/// fine and settles end to end; refusing it would be a product opinion
/// dressed as a safety check.
pub const MIN_FEE_BPS: u64 = 0;

/// The highest configurable rate: 9,999 bps (99.99%).
///
/// One below [`crate::amount_conversion::BPS_DENOMINATOR`], and that
/// single basis point is the whole reason: at exactly 100% the fee equals
/// the gross, so `net` is 0 for every transfer and the route can never
/// deliver anything. Above 100% the net entitlement would be negative,
/// which the arithmetic itself refuses one layer down.
pub const MAX_FEE_BPS: u64 = BPS_DENOMINATOR - 1;

/// Every route this build can actually settle, in registry order.
///
/// DERIVED from [`Route::ALL`] and [`Route::as_direction`] rather than
/// listed, so a future route that gains settlement machinery is
/// automatically one that must be priced — and a config that forgets it
/// fails closed at startup instead of quietly inheriting somebody else's
/// rate.
pub fn executable_routes() -> impl Iterator<Item = Route> {
    Route::ALL
        .into_iter()
        .filter(|route| route.as_direction().is_some())
}

/// One rate per executable route.
///
/// Constructed by [`crate::config`] and then read-only. Cheap to clone —
/// at most one `u64` per route — so the API layer holds its own copy
/// rather than reaching through an `Arc` on every quote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteFees {
    entries: BTreeMap<Route, u64>,
}

impl RouteFees {
    pub fn new() -> Self {
        RouteFees {
            entries: BTreeMap::new(),
        }
    }

    /// Records `route`'s rate.
    ///
    /// Refuses a non-executable route, a duplicate, and any rate outside
    /// [`MIN_FEE_BPS`]`..=`[`MAX_FEE_BPS`]. It does NOT consult a list of
    /// previously-charged rates: a fee is configuration, and any rate in
    /// range is as valid as any other.
    pub fn insert(&mut self, route: Route, fee_bps: u64) -> Result<(), FeeError> {
        if route.as_direction().is_none() {
            return Err(FeeError::RouteNotExecutable {
                route: route.as_str(),
            });
        }
        if self.entries.contains_key(&route) {
            return Err(FeeError::DuplicateFee {
                route: route.as_str(),
            });
        }
        // The ONLY rate rule: a range, and both ends of it are arithmetic
        // rather than policy. `MIN_FEE_BPS` is 0, so the lower bound is
        // structural (the type is unsigned) and stated for symmetry.
        if !(MIN_FEE_BPS..=MAX_FEE_BPS).contains(&fee_bps) {
            return Err(FeeError::FeeBpsOutOfRange {
                route: route.as_str(),
                fee_bps,
                max: MAX_FEE_BPS,
                max_percent: "99.99%",
                denominator: BPS_DENOMINATOR,
            });
        }
        self.entries.insert(route, fee_bps);
        Ok(())
    }

    /// The rate `route` prices NEW requests at.
    ///
    /// **Fails closed.** There is no default here and there must never be
    /// one: an executable route with no configured rate is a
    /// misconfiguration, and answering it with another route's number —
    /// or with the compiled-in constant — is precisely the leak this
    /// module exists to prevent.
    pub fn fee_bps(&self, route: Route) -> Result<u64, FeeError> {
        if route.as_direction().is_none() {
            return Err(FeeError::RouteNotExecutable {
                route: route.as_str(),
            });
        }
        self.entries
            .get(&route)
            .copied()
            .ok_or(FeeError::MissingFee {
                route: route.as_str(),
            })
    }

    /// The rate for `route`, or `None` — for display code that must show
    /// "not configured" rather than refuse.
    pub fn get(&self, route: Route) -> Option<u64> {
        self.entries.get(&route).copied()
    }

    /// Whether every executable route has a rate. Checked at config load;
    /// exposed so a tool can report the same verdict without duplicating
    /// the rule.
    pub fn covers_every_executable_route(&self) -> Result<(), FeeError> {
        for route in executable_routes() {
            if !self.entries.contains_key(&route) {
                return Err(FeeError::MissingFee {
                    route: route.as_str(),
                });
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every configured rate, in registry order.
    pub fn iter(&self) -> impl Iterator<Item = (Route, u64)> + '_ {
        Route::ALL
            .into_iter()
            .filter_map(|route| self.entries.get(&route).map(|bps| (route, *bps)))
    }
}

#[cfg(test)]
mod tests;
