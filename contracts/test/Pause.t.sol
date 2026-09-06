// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

contract PauseTest is BridgeTestBase {
    function test_each_guardian_can_pause() public {
        address[3] memory gs = [guardian1, guardian2, guardian3];
        for (uint256 i = 0; i < gs.length; ++i) {
            _openBothRoutes();
            vm.prank(gs[i]);
            bridge.guardianPause(true, true);
            assertTrue(bridge.depositsPaused());
            assertTrue(bridge.payoutsPaused());
        }
    }

    function test_guardian_can_pause_one_direction_only() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, false);
        assertTrue(bridge.depositsPaused());
        assertFalse(bridge.payoutsPaused());
    }

    function test_non_guardian_cannot_pause() public {
        vm.prank(outsider);
        vm.expectRevert(GlcRobinhoodBridge.UnauthorizedGuardian.selector);
        bridge.guardianPause(true, true);
    }

    /// A bridge SIGNER is not a guardian. The two authorities are separate.
    function test_signer_cannot_guardian_pause() public {
        vm.prank(signerA);
        vm.expectRevert(GlcRobinhoodBridge.UnauthorizedGuardian.selector);
        bridge.guardianPause(true, true);
    }

    /// The core guardian constraint: there is no argument that unpauses.
    function test_guardian_cannot_unpause() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);

        vm.prank(guardian1);
        vm.expectRevert(GlcRobinhoodBridge.NothingToPause.selector);
        bridge.guardianPause(false, false);

        assertTrue(bridge.depositsPaused());
        assertTrue(bridge.payoutsPaused());
    }

    /// Even calling with one flag set cannot clear the other.
    function test_guardian_call_never_clears_a_flag() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);
        vm.prank(guardian2);
        bridge.guardianPause(true, false);
        assertTrue(bridge.depositsPaused());
        assertTrue(bridge.payoutsPaused());
    }

    function test_quorum_can_unpause() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);
        _openBothRoutes();
        assertFalse(bridge.depositsPaused());
        assertFalse(bridge.payoutsPaused());
    }

    function test_single_signer_cannot_unpause() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);

        uint256 nonce = bridge.governanceNonce();
        bytes32 payload = keccak256(abi.encode(false, false));
        bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, nonce, FAR_FUTURE);
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, h);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.setPaused(false, false, nonce, FAR_FUTURE, sigs);
    }

    function test_unpause_replay_rejected() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);

        uint256 nonce = bridge.governanceNonce();
        bytes32 payload = keccak256(abi.encode(false, false));
        bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, nonce, FAR_FUTURE);
        bytes[] memory sigs = _quorumAB(h);
        bridge.setPaused(false, false, nonce, FAR_FUTURE, sigs);

        vm.prank(guardian1);
        bridge.guardianPause(true, true);

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, nonce + 1, nonce
            )
        );
        bridge.setPaused(false, false, nonce, FAR_FUTURE, sigs);
        assertTrue(bridge.depositsPaused());
    }

    function test_directional_pause_is_independent() public {
        _setPaused(true, false);
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());

        // Payouts still work.
        _payout(keccak256("p"), bob, OUTBOUND_MIN);

        _setPaused(false, true);
        uint256 idx = _deposit(alice, INBOUND_MIN);
        assertEq(idx, 0);
    }

    function test_expired_unpause_authorization_rejected() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);

        uint256 nonce = bridge.governanceNonce();
        bytes32 payload = keccak256(abi.encode(false, false));
        uint64 expiry = uint64(block.timestamp - 1);
        bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, nonce, expiry);
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AuthorizationExpired.selector);
        bridge.setPaused(false, false, nonce, expiry, sigs);
    }
}
