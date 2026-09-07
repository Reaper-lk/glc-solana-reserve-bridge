//! The specific EVM networks this bridge is being built for.
//!
//! Kept separate from every other module in [`crate::evm`], which is
//! generic: those types describe *any* EVM chain and must stay usable for
//! one this repository has never heard of. This module is the one place a
//! particular network's identity is written down, and it is deliberately
//! not re-exported from [`crate::evm`]'s root, so reaching for a specific
//! network's constant is always spelled `evm::networks::...` and is visible
//! in a diff.
//!
//! # Scope
//!
//! Chain identity only. There is no RPC URL, no contract address, no token
//! address, and no per-network policy here: those are deployment
//! configuration, they differ between environments, and they belong in
//! [`crate::config`] when the route is actually built. A chain id is
//! different in kind — it is a property of the chain itself, fixed for as
//! long as the chain exists, and identical for every operator — so a
//! compile-time constant is the honest place for it.
//!
//! Nothing in this phase reads these constants on any production path. They
//! exist so that when a later phase does, the value is asserted in one place
//! against one source rather than retyped into a config file per
//! environment.

use std::num::NonZeroU64;

use super::chain_id::EvmChainId;

/// Robinhood Chain mainnet's EIP-155 chain id, as a plain integer.
///
/// Decimal 4663. Written as a decimal literal because that is how a chain id
/// is quoted everywhere except a JSON-RPC payload — and because the hex
/// spelling `0x4663` is the *different* chain id 18019, which is exactly the
/// confusion [`EvmChainId`]'s two separate parsers exist to prevent.
pub const ROBINHOOD_MAINNET_CHAIN_ID_RAW: u64 = 4663;

/// Robinhood Chain testnet's EIP-155 chain id, as a plain integer.
///
/// Decimal 46630 — the mainnet id with a trailing zero, which is a
/// convention several chains follow and which makes the two easy to
/// transpose. They are distinct [`EvmChainId`] values and no code should
/// ever derive one from the other arithmetically.
pub const ROBINHOOD_TESTNET_CHAIN_ID_RAW: u64 = 46630;

/// Robinhood Chain mainnet, as a validated [`EvmChainId`].
///
/// Built at compile time from [`ROBINHOOD_MAINNET_CHAIN_ID_RAW`], so there is
/// no startup unwrap and no way for a zero to reach it.
pub const ROBINHOOD_MAINNET_CHAIN_ID: EvmChainId = nonzero_chain_id(ROBINHOOD_MAINNET_CHAIN_ID_RAW);

/// Robinhood Chain testnet, as a validated [`EvmChainId`].
pub const ROBINHOOD_TESTNET_CHAIN_ID: EvmChainId = nonzero_chain_id(ROBINHOOD_TESTNET_CHAIN_ID_RAW);

/// Builds a chain id in a `const` context.
///
/// The `panic!` is unreachable for the constants above and is a **compile
/// error**, not a runtime panic, if it ever becomes reachable: a `const`
/// initialiser that panics fails to evaluate and the build stops. That is
/// the point — it turns "somebody wrote a 0 here" into a broken build rather
/// than something to discover at startup.
const fn nonzero_chain_id(raw: u64) -> EvmChainId {
    match NonZeroU64::new(raw) {
        Some(id) => EvmChainId::from_nonzero(id),
        None => panic!("a network chain-id constant must be non-zero"),
    }
}

/// Which Robinhood Chain network a deployment is pointed at.
///
/// An explicit two-variant enum rather than a bare chain id at call sites,
/// following the precedent of [`crate::goldcoin::address::Network`]: there is
/// deliberately no `Default`, so no code path can silently act on mainnet
/// because a config field was missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RobinhoodNetwork {
    Mainnet,
    Testnet,
}

impl RobinhoodNetwork {
    /// This network's EIP-155 chain id.
    pub const fn chain_id(self) -> EvmChainId {
        match self {
            RobinhoodNetwork::Mainnet => ROBINHOOD_MAINNET_CHAIN_ID,
            RobinhoodNetwork::Testnet => ROBINHOOD_TESTNET_CHAIN_ID,
        }
    }

    /// The lowercase name used in config files and log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            RobinhoodNetwork::Mainnet => "mainnet",
            RobinhoodNetwork::Testnet => "testnet",
        }
    }

    /// The network whose chain id is `chain_id`, if this bridge knows one.
    ///
    /// Returns `None` rather than a default for an unknown chain id: a node
    /// that reports a chain this build does not know about is a
    /// misconfiguration to refuse, never one to guess at.
    pub fn from_chain_id(chain_id: EvmChainId) -> Option<RobinhoodNetwork> {
        match chain_id.get() {
            ROBINHOOD_MAINNET_CHAIN_ID_RAW => Some(RobinhoodNetwork::Mainnet),
            ROBINHOOD_TESTNET_CHAIN_ID_RAW => Some(RobinhoodNetwork::Testnet),
            _ => None,
        }
    }
}

impl std::fmt::Display for RobinhoodNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests;
