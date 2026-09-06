// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

contract LimitsTest is BridgeTestBase {
    function _set(GlcRobinhoodBridge.Limits memory lim) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(h));
    }

    function _setExpectRevert(GlcRobinhoodBridge.Limits memory lim, bytes4 err) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(err);
        bridge.setLimits(lim, nonce, FAR_FUTURE, sigs);
    }

    function test_valid_change() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMax = 30_000 * ONE_GLC;
        lim.inboundRollingLimit = 200_000 * ONE_GLC;
        _set(lim);
        assertEq(bridge.limits().inboundMax, 30_000 * ONE_GLC);
        assertEq(bridge.limits().inboundRollingLimit, 200_000 * ONE_GLC);
    }

    function test_requires_quorum() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMax = 30_000 * ONE_GLC;
        lim.inboundRollingLimit = 200_000 * ONE_GLC;
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, h);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.setLimits(lim, nonce, FAR_FUTURE, sigs);
    }

    function test_rejects_min_above_max() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMin = lim.inboundMax + SCALE;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    function test_rejects_outbound_min_above_max() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.outboundMin = lim.outboundMax + SCALE;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    /// A zero minimum is rejected explicitly: a zero-value deposit is never
    /// valid, and leaving the semantics implicit invites the wrong reading.
    function test_rejects_zero_minimum() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMin = 0;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    /// A rolling limit below the per-transfer max would make the max
    /// unreachable, which is a misconfiguration, not a policy.
    function test_rejects_rolling_limit_below_max() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundRollingLimit = lim.inboundMax - SCALE;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    function test_rejects_outbound_rolling_limit_below_max() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.outboundRollingLimit = lim.outboundMax - SCALE;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    function test_rejects_non_canonical_outbound_rolling_limit() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.outboundRollingLimit = OUTBOUND_ROLLING + 1;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    function test_rejects_non_canonical_min() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMin = INBOUND_MIN + 1;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    function test_rejects_non_canonical_max() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMax = INBOUND_MAX + 1;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    function test_rejects_non_canonical_protected_reserve() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = 1;
        _setExpectRevert(lim, GlcRobinhoodBridge.InvalidLimits.selector);
    }

    /// Zero IS a valid protected minimum, and means "no floor".
    function test_zero_protected_reserve_allowed() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = 0;
        _set(lim);
        assertEq(bridge.limits().protectedMinReserve, 0);
    }

    function test_replay_rejected() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMax = 30_000 * ONE_GLC;
        lim.inboundRollingLimit = 200_000 * ONE_GLC;
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        bridge.setLimits(lim, nonce, FAR_FUTURE, sigs);

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, nonce + 1, nonce
            )
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, sigs);
    }

    /// Limits are enforced against the NEW values immediately.
    function test_new_limits_take_effect() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMax = 200 * ONE_GLC;
        _set(lim);

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.AmountAboveMaximum.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, 300 * ONE_GLC, _destination());
    }
}
