//! The Robinhood contract state the PUBLIC API is allowed to report, and
//! the one object-safe way to obtain it.
//!
//! # Why this module exists at all
//!
//! `crate::api`'s existing Robinhood-relevant figures come from the
//! ledger: the `reserve_ledger` row (`crate::robinhood::admin::
//! reserve_report`) and the indexer's own health state
//! (`crate::robinhood::health`). Neither of those knows the contract's
//! transfer limits or its rolling-window consumption, because neither is
//! where those live — they are storage on the deployed
//! `GlcRobinhoodBridge`, readable only over `eth_call`.
//!
//! A public endpoint that reported Robinhood limits from anywhere else
//! would be reporting something the chain does not enforce. There is no
//! service-side copy to fall back on and there deliberately must not be
//! one: `[robinhood.settlement]` carries submitter/gas/quorum policy and
//! no min, max or rolling limit at all, precisely so that a config file
//! can never disagree with the contract about what a user may transfer.
//! So the only two honest answers this module can produce are "here is
//! what the contract says" and "this is not available", and
//! [`RobinhoodContractStatus`] is exactly that pair.
//!
//! # Why the trait, rather than calling the RPC from `api`
//!
//! [`super::rpc::EvmCallRpc`] uses `impl Future` in return position, so
//! it is not object-safe and cannot be stored behind a `dyn` pointer.
//! `crate::api::BridgeApi` needs a `dyn` here rather than a third type
//! parameter: the Robinhood half is OPTIONAL, and a deployment with no
//! `[robinhood.settlement]` section must be able to construct the API
//! with no Robinhood reader at all without naming a phantom RPC type.
//! [`RobinhoodContractSource`] is that object-safe seam, and
//! [`LiveRobinhoodContractSource`] is its single production
//! implementation.
//!
//! # This module reads. It cannot write, and it cannot open a route.
//!
//! Every call below is an `eth_call` through [`super::calls::
//! BridgeReader`], the same read-only reader `preflight` and `glc-admin
//! robinhood-reserve` already use. There is no signer here, no
//! transaction builder, and nothing that touches
//! [`crate::routes::RouteGate`]. Reporting that a route is enabled ON THE
//! CONTRACT is not enabling it in this service — the gate's three local
//! checks are unaffected by anything in this file, and a route that is
//! open on-chain and closed here stays closed.

use crate::evm::{EvmAddress, EvmU256};

use super::calls::{BridgeLimits, BridgeReader, RollingWindow, ROLLING_WINDOW_SECONDS};
use super::rpc::{EvmBlockTag, EvmCallRpc};

/// One internally consistent reading of the contract state the public API
/// reports.
///
/// Every field is a value the contract returned in THIS read. Nothing is
/// defaulted, and there is no partial form: a read that could not obtain
/// all of it produces [`RobinhoodContractStatus::Unavailable`] rather
/// than a struct with some fields guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RobinhoodContractState {
    /// `limits()` — min/max per transfer and the rolling limit for each
    /// direction, plus the protected reserve floor. Robinhood's own
    /// 18-decimal units.
    pub limits: BridgeLimits,
    /// `inboundWindow()` — the DEPOSIT direction's rolling accumulator.
    pub inbound_window: RollingWindow,
    /// `outboundWindow()` — the PAYOUT direction's.
    pub outbound_window: RollingWindow,
    /// `depositsPaused()` — governance's inbound kill switch.
    pub deposits_paused: bool,
    /// `payoutsPaused()` — the outbound one. Separate flags on-chain, so
    /// separate here; collapsing them would report a paused payout leg as
    /// a paused bridge.
    pub payouts_paused: bool,
    /// `encumberedReserve()` — the protected floor plus every unsettled
    /// depositor's principal: GLC physically held by the contract that is
    /// not the bridge's to pay out.
    pub encumbered_reserve: EvmU256,
    /// The fixed rolling-window length both accumulators use, in seconds.
    /// A protocol constant, carried here so a caller need not hardcode it.
    pub window_seconds: u64,
}

/// The answer to "what does the Robinhood contract currently say", with
/// the two not-an-answer cases named rather than encoded as zeroes.
///
/// This is the type the public API's honesty rule is expressed in: a
/// caller that cannot get [`RobinhoodContractStatus::Available`] learns
/// that the figures are unknown, and never receives a number this service
/// made up to fill the field.
///
/// `Available` boxes its payload: the state is several `uint256` words
/// wide, and an unboxed variant would make every `NotConfigured` — the
/// answer every production deployment gets today — carry that footprint
/// for nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobinhoodContractStatus {
    /// No Robinhood contract is configured on this deployment, so there
    /// is no contract to ask. Distinct from [`Self::Unavailable`]: this
    /// one never becomes available without a config change and a restart.
    NotConfigured,
    /// A contract IS configured, but this read did not complete — the
    /// endpoint was unreachable, answered an error, or returned something
    /// that did not decode. Deliberately carries no detail: see
    /// [`RobinhoodContractSource::state`].
    Unavailable,
    Available(Box<RobinhoodContractState>),
}

impl RobinhoodContractStatus {
    /// The wire spelling, shared by every public DTO that carries one of
    /// these. Named constants rather than inline literals so the backend
    /// answer and a UI's branch on it cannot drift apart.
    pub fn as_str(&self) -> &'static str {
        match self {
            RobinhoodContractStatus::NotConfigured => AVAILABILITY_NOT_CONFIGURED,
            RobinhoodContractStatus::Unavailable => AVAILABILITY_UNAVAILABLE,
            RobinhoodContractStatus::Available(_) => AVAILABILITY_AVAILABLE,
        }
    }

    pub fn state(&self) -> Option<&RobinhoodContractState> {
        match self {
            RobinhoodContractStatus::Available(s) => Some(s),
            _ => None,
        }
    }
}

/// The figures are present and authoritative.
pub const AVAILABILITY_AVAILABLE: &str = "available";
/// This deployment has no Robinhood contract configured. A permanent
/// answer for this process, not a transient failure.
pub const AVAILABILITY_NOT_CONFIGURED: &str = "not_configured";
/// A contract is configured but could not be read right now. Transient,
/// and explicitly NOT a claim that the figures are zero.
pub const AVAILABILITY_UNAVAILABLE: &str = "unavailable";

/// An object-safe source of [`RobinhoodContractState`].
pub trait RobinhoodContractSource: Send + Sync {
    /// Reads the contract, or reports why it could not.
    ///
    /// Never returns an error: a failed read is
    /// [`RobinhoodContractStatus::Unavailable`], because the caller is a
    /// public endpoint whose correct behaviour on an RPC outage is to
    /// serve the rest of its response and say this part is unknown — not
    /// to fail the whole request, and not to substitute zeroes.
    ///
    /// The failure detail is deliberately dropped rather than returned.
    /// An RPC error string can name the endpoint's host, and this
    /// service's public API does not disclose infrastructure shape (see
    /// `crate::api`'s module docs); the operator-facing copy of the same
    /// failure is already logged and surfaced through
    /// [`super::health::RobinhoodHealth`], which redacts it properly.
    fn state(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RobinhoodContractStatus> + Send + '_>>;
}

/// The production [`RobinhoodContractSource`]: live `eth_call`s against
/// the configured `GlcRobinhoodBridge`.
pub struct LiveRobinhoodContractSource<R> {
    rpc: R,
    reader: BridgeReader,
}

impl<R: EvmCallRpc + Send + Sync> LiveRobinhoodContractSource<R> {
    pub fn new(rpc: R, bridge_contract: EvmAddress) -> Self {
        LiveRobinhoodContractSource {
            rpc,
            reader: BridgeReader::new(bridge_contract),
        }
    }

    /// All five reads, at `latest`, failing as a unit.
    ///
    /// They are separate `eth_call`s and therefore not a snapshot of one
    /// block; that is acceptable here and nowhere near a settlement path
    /// (every value-moving read in `crate::robinhood::calls` pins its own
    /// block tag). What matters for a display endpoint is that a caller
    /// never sees a limit from one read paired with a fabricated window
    /// from a failed one — hence `?` on every step and a single
    /// `Unavailable` for the whole set.
    async fn read(&self) -> Result<RobinhoodContractState, super::calls::ContractReadError> {
        let limits = self.reader.limits(&self.rpc, EvmBlockTag::Latest).await?;
        let inbound_window = self
            .reader
            .inbound_window(&self.rpc, EvmBlockTag::Latest)
            .await?;
        let outbound_window = self
            .reader
            .outbound_window(&self.rpc, EvmBlockTag::Latest)
            .await?;
        let deposits_paused = self
            .reader
            .deposits_paused(&self.rpc, EvmBlockTag::Latest)
            .await?;
        let payouts_paused = self
            .reader
            .payouts_paused(&self.rpc, EvmBlockTag::Latest)
            .await?;
        let encumbered_reserve = self
            .reader
            .encumbered_reserve(&self.rpc, EvmBlockTag::Latest)
            .await?;
        Ok(RobinhoodContractState {
            limits,
            inbound_window,
            outbound_window,
            deposits_paused,
            payouts_paused,
            encumbered_reserve,
            window_seconds: ROLLING_WINDOW_SECONDS,
        })
    }
}

impl<R: EvmCallRpc + Send + Sync> RobinhoodContractSource for LiveRobinhoodContractSource<R> {
    fn state(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RobinhoodContractStatus> + Send + '_>>
    {
        Box::pin(async move {
            match self.read().await {
                Ok(state) => RobinhoodContractStatus::Available(Box::new(state)),
                Err(e) => {
                    // Logged, not returned: see the trait's docs on why
                    // the public response carries no detail.
                    tracing::debug!(
                        error = %e,
                        "public Robinhood contract read failed; reporting the figures as unavailable"
                    );
                    RobinhoodContractStatus::Unavailable
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
