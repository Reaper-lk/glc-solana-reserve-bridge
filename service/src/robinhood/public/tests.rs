use super::*;

use crate::robinhood::rpc::EvmRpcError;
use crate::robinhood::testkit::{MockNode, BRIDGE};

/// An `EvmCallRpc` that always fails — a node that is up but answering an
/// error, or a socket that never opened.
struct DeadRpc;

impl EvmCallRpc for DeadRpc {
    async fn call(
        &self,
        _call: &super::super::rpc::EvmCall,
        _block: EvmBlockTag,
    ) -> Result<Vec<u8>, EvmRpcError> {
        Err(EvmRpcError::Transport("connection refused".into()))
    }

    async fn code_at(
        &self,
        _address: crate::evm::EvmAddress,
        _block: EvmBlockTag,
    ) -> Result<Vec<u8>, EvmRpcError> {
        Err(EvmRpcError::Transport("connection refused".into()))
    }
}

fn glc(whole: u64) -> EvmU256 {
    EvmU256::from_u128(u128::from(whole) * 1_000_000_000_000_000_000)
}

#[tokio::test]
async fn a_healthy_contract_reports_every_figure_it_returned() {
    let node = MockNode::new(BRIDGE);
    let source = LiveRobinhoodContractSource::new(node, BRIDGE);

    let status = source.state().await;
    assert_eq!(status.as_str(), AVAILABILITY_AVAILABLE);
    let state = status.state().expect("available");

    // Exactly `MockContract::healthy`'s figures, not defaults.
    assert_eq!(state.limits.inbound_min, glc(1));
    assert_eq!(state.limits.inbound_max, glc(10_000));
    assert_eq!(state.limits.inbound_rolling_limit, glc(100_000));
    assert_eq!(state.limits.protected_min_reserve, glc(1_000));
    assert_eq!(state.inbound_window.total, glc(250));
    assert!(!state.deposits_paused);
    assert!(!state.payouts_paused);
    assert_eq!(state.window_seconds, ROLLING_WINDOW_SECONDS);
}

#[tokio::test]
async fn the_two_pause_flags_are_reported_separately() {
    let node = MockNode::new(BRIDGE);
    node.with(|s| {
        s.contract.deposits_paused = true;
        s.contract.payouts_paused = false;
    });
    let source = LiveRobinhoodContractSource::new(node, BRIDGE);

    let status = source.state().await;
    let state = status.state().expect("available");
    // Collapsing these would report a paused deposit leg as a paused
    // bridge, which is a different and wrong claim about the payout leg.
    assert!(state.deposits_paused);
    assert!(!state.payouts_paused);
}

#[tokio::test]
async fn an_unreachable_endpoint_is_unavailable_never_zeroes() {
    let source = LiveRobinhoodContractSource::new(DeadRpc, BRIDGE);

    let status = source.state().await;
    assert_eq!(status.as_str(), AVAILABILITY_UNAVAILABLE);
    // The load-bearing assertion: there is NO state to read figures out
    // of, so no caller can mistake a failed read for a zeroed contract.
    assert!(status.state().is_none());
    assert!(!matches!(status, RobinhoodContractStatus::NotConfigured));
}

#[tokio::test]
async fn a_partial_read_fails_as_a_unit_rather_than_mixing_reads() {
    // `bridge_code` empty makes every `eth_call` return empty data, which
    // the decoder refuses. The point is that no field survives into a
    // half-populated state.
    let node = MockNode::new(BRIDGE);
    node.with(|s| s.contract.bridge_code.clear());
    let source = LiveRobinhoodContractSource::new(node, BRIDGE);

    assert!(source.state().await.state().is_none());
}

#[test]
fn the_three_availability_spellings_are_distinct_and_stable() {
    // A UI branches on these strings; two of them collapsing into one
    // would silently merge "never will be available" with "retry later".
    assert_eq!(AVAILABILITY_AVAILABLE, "available");
    assert_eq!(AVAILABILITY_NOT_CONFIGURED, "not_configured");
    assert_eq!(AVAILABILITY_UNAVAILABLE, "unavailable");
    assert_eq!(
        RobinhoodContractStatus::NotConfigured.as_str(),
        AVAILABILITY_NOT_CONFIGURED
    );
    assert_eq!(
        RobinhoodContractStatus::Unavailable.as_str(),
        AVAILABILITY_UNAVAILABLE
    );
    assert!(RobinhoodContractStatus::NotConfigured.state().is_none());
    assert!(RobinhoodContractStatus::Unavailable.state().is_none());
}
