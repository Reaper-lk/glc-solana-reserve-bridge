//! Per-chain launch policy: the fee rate and the transfer ceilings an
//! operator approved for ONE chain, stated in the ledger's canonical
//! 8-decimal unit.
//!
//! # Why policy is per chain
//!
//! Until this module existed the bridge had exactly one fee rate
//! ([`crate::amount_conversion::BRIDGE_FEE_BPS`], compiled in) and no
//! configured transfer ceilings at all — the Solana program's own
//! `min_transfer_amount`/`per_transfer_limit`/`rolling_volume_limit` are
//! read FROM the chain (`crate::solana::accounts`), never mirrored here.
//! That worked while there was one destination chain.
//!
//! It stops working the moment a second chain launches under different
//! commercial terms. A single compiled-in rate cannot say "6% to
//! Robinhood, 3% to Solana", and a single global ceiling cannot say
//! "20,000 GLC per Robinhood transfer" without also saying it about
//! Solana. So policy becomes a value keyed by chain.
//!
//! # What this is NOT
//!
//! It is **not** an enforcement layer, and reading it as one would be the
//! most dangerous mistake available here. Both bridges enforce their own
//! per-transfer and rolling limits ON CHAIN — the Solana program in
//! `programs/glc-reserve-bridge/src/limits.rs`, the Robinhood contract in
//! `GlcRobinhoodBridge._consumeWindow` and its `deposit`/`executePayout`
//! bounds checks. Those are the hard limits. A value here can only ever
//! be a STATEMENT of what the operator believes the chain is configured
//! to allow, which is why the Robinhood binding
//! ([`crate::robinhood::policy`]) exists solely to compare this statement
//! against the deployed contract's `limits()` and report every
//! disagreement.
//!
//! # Solana can never acquire a policy through this mechanism
//!
//! [`POLICY_GOVERNED_CHAINS`] lists the chains a configured policy may
//! name, and [`ChainPolicies::insert`] refuses every other chain. Today
//! that list holds Robinhood and nothing else, so no configuration file,
//! however written, can change the Goldcoin<->Solana fee or limits: that
//! route keeps pricing at the compiled-in [`BRIDGE_FEE_BPS`] and keeps
//! reading its ceilings off the Solana program account, exactly as
//! before. Adding a chain to this list is a deliberate, reviewable edit,
//! not a config-file consequence.
//!
//! [`BRIDGE_FEE_BPS`]: crate::amount_conversion::BRIDGE_FEE_BPS

use std::collections::BTreeMap;

use crate::amount_conversion::{
    CanonicalAtomic, BPS_DENOMINATOR, BRIDGE_FEE_BPS, HISTORICAL_FEE_BPS,
};
use crate::routes::Chain;

/// The chains a configured [`ChainPolicy`] may name.
///
/// A deliberate allow-list rather than "any chain the enum can spell".
/// `Chain` names every chain this deployment knows, including the two
/// whose policy is not configurable at all, and letting a config file
/// name one of those would be exactly the silent Solana behaviour change
/// this module must make impossible.
pub const POLICY_GOVERNED_CHAINS: &[Chain] = &[Chain::Robinhood];

/// Why a per-chain policy was refused.
///
/// Every variant names the chain, because a policy error with no chain in
/// it is unreadable the moment there is more than one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainPolicyError {
    #[error(
        "{chain} is not a policy-governed chain — its fee and limits are not configurable. \
         Goldcoin<->Solana prices at the compiled-in BRIDGE_FEE_BPS and reads its transfer and \
         rolling ceilings from the Solana program account; a config file must never be able to \
         change either"
    )]
    ChainNotPolicyGoverned { chain: &'static str },
    #[error("{chain}: a policy for this chain was already configured — declare it exactly once")]
    DuplicatePolicy { chain: &'static str },
    #[error(
        "{chain}: fee_bps must not be zero. A zero rate is not a discount, it is a rate the \
         protocol has never charged, and every later fee-breakdown check \
         (amount_conversion::verify_fee_breakdown) would refuse a request carrying it"
    )]
    ZeroFeeBps { chain: &'static str },
    #[error(
        "{chain}: fee_bps {fee_bps} is not below the 10000 basis-point denominator \
         (amount_conversion::BPS_DENOMINATOR) — a rate at or above 100% leaves the user nothing \
         and cannot be a fee"
    )]
    FeeBpsOutOfRange { chain: &'static str, fee_bps: u64 },
    #[error(
        "{chain}: fee_bps {fee_bps} is not a rate this protocol charges \
         (amount_conversion::HISTORICAL_FEE_BPS = {known:?}). The rate is snapshotted onto every \
         request and re-checked against that list at settlement, so a rate accepted here but \
         absent there would price a request that could never settle. Add the rate to \
         HISTORICAL_FEE_BPS in the same change that configures it"
    )]
    UnknownFeeBps {
        chain: &'static str,
        fee_bps: u64,
        known: &'static [u64],
    },
    #[error(
        "{chain}: per_transfer_limit must not be zero — a zero ceiling closes the chain silently, \
         and closing a route is what the route flags and the pause gates are for"
    )]
    ZeroPerTransferLimit { chain: &'static str },
    #[error(
        "{chain}: rolling_daily_limit must not be zero, for the same reason as per_transfer_limit"
    )]
    ZeroRollingDailyLimit { chain: &'static str },
    #[error(
        "{chain}: rolling_daily_limit {rolling} is below per_transfer_limit {per_transfer} \
         (canonical 8dp) — a single legal transfer could not fit in a whole day's budget, so the \
         per-transfer ceiling would be unreachable and the stated policy self-contradictory"
    )]
    RollingBelowPerTransfer {
        chain: &'static str,
        per_transfer: u64,
        rolling: u64,
    },
}

/// One chain's approved launch policy, validated at construction.
///
/// Fields are private and there is exactly one constructor, so possessing
/// a `ChainPolicy` IS the evidence that every check in
/// [`ChainPolicy::new`] passed — the same discipline
/// `crate::robinhood::preflight::VerifiedDeployment` uses.
///
/// Amounts are [`CanonicalAtomic`] (8 decimals), the unit every ledger
/// figure in this service already uses. Converting them to a chain's
/// native precision is the job of that chain's binding module, never of
/// the caller reading these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainPolicy {
    chain: Chain,
    fee_bps: u64,
    per_transfer_limit: CanonicalAtomic,
    rolling_daily_limit: CanonicalAtomic,
}

impl ChainPolicy {
    /// Validates and constructs.
    ///
    /// Every check refuses a value that would be silently harmful rather
    /// than obviously wrong: a zero ceiling closes a chain without saying
    /// so, and a fee rate outside [`HISTORICAL_FEE_BPS`] prices requests
    /// that can never settle.
    pub fn new(
        chain: Chain,
        fee_bps: u64,
        per_transfer_limit: CanonicalAtomic,
        rolling_daily_limit: CanonicalAtomic,
    ) -> Result<ChainPolicy, ChainPolicyError> {
        let name = chain.as_str();
        if !POLICY_GOVERNED_CHAINS.contains(&chain) {
            return Err(ChainPolicyError::ChainNotPolicyGoverned { chain: name });
        }
        if fee_bps == 0 {
            return Err(ChainPolicyError::ZeroFeeBps { chain: name });
        }
        if fee_bps >= BPS_DENOMINATOR {
            return Err(ChainPolicyError::FeeBpsOutOfRange {
                chain: name,
                fee_bps,
            });
        }
        if !HISTORICAL_FEE_BPS.contains(&fee_bps) {
            return Err(ChainPolicyError::UnknownFeeBps {
                chain: name,
                fee_bps,
                known: HISTORICAL_FEE_BPS,
            });
        }
        if per_transfer_limit.0 == 0 {
            return Err(ChainPolicyError::ZeroPerTransferLimit { chain: name });
        }
        if rolling_daily_limit.0 == 0 {
            return Err(ChainPolicyError::ZeroRollingDailyLimit { chain: name });
        }
        if rolling_daily_limit.0 < per_transfer_limit.0 {
            return Err(ChainPolicyError::RollingBelowPerTransfer {
                chain: name,
                per_transfer: per_transfer_limit.0,
                rolling: rolling_daily_limit.0,
            });
        }
        Ok(ChainPolicy {
            chain,
            fee_bps,
            per_transfer_limit,
            rolling_daily_limit,
        })
    }

    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// The rate NEW requests on this chain price at, and which is
    /// snapshotted onto each request. Settlement of an in-flight request
    /// always uses that snapshot, never this value, so changing a
    /// configured rate cannot re-price anything already created.
    pub fn fee_bps(&self) -> u64 {
        self.fee_bps
    }

    /// The largest single transfer this policy approves, canonical 8dp.
    pub fn per_transfer_limit(&self) -> CanonicalAtomic {
        self.per_transfer_limit
    }

    /// The STRICT 24-hour ceiling this policy approves, canonical 8dp.
    ///
    /// "Strict" is load-bearing and is not the same number as the value
    /// configured on a chain whose rolling window is a fixed bucket — see
    /// [`crate::robinhood::policy`], which derives the on-chain figure
    /// from this one rather than passing it through.
    pub fn rolling_daily_limit(&self) -> CanonicalAtomic {
        self.rolling_daily_limit
    }
}

/// Every configured per-chain policy, keyed by chain.
///
/// A map rather than a struct of named options: adding a chain is then a
/// config section plus an entry in [`POLICY_GOVERNED_CHAINS`], not a
/// change to this type and every match on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainPolicies {
    entries: BTreeMap<Chain, ChainPolicy>,
}

impl ChainPolicies {
    pub fn new() -> ChainPolicies {
        ChainPolicies::default()
    }

    /// Records one chain's policy, refusing a chain that is not
    /// policy-governed and refusing a second policy for a chain that
    /// already has one.
    pub fn insert(&mut self, policy: ChainPolicy) -> Result<(), ChainPolicyError> {
        let chain = policy.chain();
        if !POLICY_GOVERNED_CHAINS.contains(&chain) {
            return Err(ChainPolicyError::ChainNotPolicyGoverned {
                chain: chain.as_str(),
            });
        }
        if self.entries.contains_key(&chain) {
            return Err(ChainPolicyError::DuplicatePolicy {
                chain: chain.as_str(),
            });
        }
        self.entries.insert(chain, policy);
        Ok(())
    }

    /// This chain's configured policy, or `None`.
    ///
    /// `None` is a real answer, not a hole to fill with another chain's
    /// numbers: it means this chain is governed by whatever it was
    /// governed by before any policy existed.
    pub fn get(&self, chain: Chain) -> Option<&ChainPolicy> {
        self.entries.get(&chain)
    }

    /// The fee rate NEW requests on `chain` price at.
    ///
    /// Falls back to the compiled-in [`BRIDGE_FEE_BPS`] for a chain with
    /// no configured policy — which is every chain today except
    /// Robinhood, and is precisely the behaviour that existed before this
    /// module. The fallback is the GLOBAL constant and never another
    /// chain's configured rate: one chain's commercial terms must never
    /// leak into another's.
    pub fn fee_bps_for(&self, chain: Chain) -> u64 {
        match self.entries.get(&chain) {
            Some(policy) => policy.fee_bps(),
            None => BRIDGE_FEE_BPS,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every configured policy, in a stable chain order.
    pub fn iter(&self) -> impl Iterator<Item = &ChainPolicy> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests;
