// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

/// The read-only surface, plus the terminal guards on the governance entry
/// points that the migration suite does not otherwise reach.
contract ViewsTest is BridgeTestBase {
    function test_signers_view() public view {
        address[3] memory s = bridge.signers();
        assertEq(s[0], signerA);
        assertEq(s[1], signerB);
        assertEq(s[2], signerC);
    }

    function test_guardians_view() public view {
        address[3] memory g = bridge.guardians();
        assertEq(g[0], guardian1);
        assertEq(g[1], guardian2);
        assertEq(g[2], guardian3);
    }

    function test_bridge_protocol_id_is_the_family_constant() public view {
        assertEq(bridge.bridgeProtocolId(), keccak256("glc.reserve-bridge.robinhood"));
        assertEq(bridge.bridgeProtocolId(), bridge.BRIDGE_PROTOCOL_ID());
    }

    /// The encumbered reserve is the floor plus every unsettled principal —
    /// the GLC that is present but is not the bridge's to spend.
    function test_encumbered_reserve_tracks_floor_and_liability() public {
        assertEq(bridge.encumberedReserve(), 0);

        uint256 amount = 1000 * ONE_GLC;
        uint256 idx = _deposit(alice, amount);
        assertEq(bridge.encumberedReserve(), amount);

        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = 5000 * ONE_GLC;
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(h));
        assertEq(bridge.encumberedReserve(), amount + 5000 * ONE_GLC);

        _settle(keccak256("s"), idx);
        assertEq(bridge.encumberedReserve(), 5000 * ONE_GLC);
    }

    function test_migration_finalizable_at_is_zero_before_commit() public view {
        assertEq(bridge.migrationFinalizableAt(), 0);
    }

    function test_obligation_view_reverts_for_unknown_index() public {
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.obligation(0);
    }

    /// The two new indexer views must reject an unknown index rather than
    /// returning the zero status, which would read as a real obligation.
    function test_obligation_status_view_rejects_unknown_index() public {
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.obligationStatus(0);
    }

    function test_is_obligation_unresolved_rejects_unknown_index() public {
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.isObligationUnresolved(0);
    }

    /// All four indexer-visible states are distinguishable through the views.
    function test_views_distinguish_all_terminal_states() public {
        uint256 a = _deposit(alice, 1000 * ONE_GLC);
        uint256 b = _deposit(alice, 1000 * ONE_GLC);
        uint256 c = _deposit(alice, 1000 * ONE_GLC);
        uint256 d = _deposit(alice, 1000 * ONE_GLC);

        assertTrue(bridge.obligationStatus(a) == GlcRobinhoodBridge.ObligationStatus.Pending);
        assertTrue(bridge.isObligationUnresolved(a));

        _settle(keccak256("v-s"), b);
        _refund(keccak256("v-r"), c);
        _abandon(keccak256("v-x"), d);

        assertTrue(bridge.obligationStatus(b) == GlcRobinhoodBridge.ObligationStatus.Settled);
        assertTrue(bridge.obligationStatus(c) == GlcRobinhoodBridge.ObligationStatus.Refunded);
        assertTrue(bridge.obligationStatus(d) == GlcRobinhoodBridge.ObligationStatus.Abandoned);
        assertFalse(bridge.isObligationUnresolved(b));
        assertFalse(bridge.isObligationUnresolved(c));
        assertFalse(bridge.isObligationUnresolved(d));

        // The wire values are append-only and must not have shifted.
        assertEq(uint8(GlcRobinhoodBridge.ObligationStatus.None), 0);
        assertEq(uint8(GlcRobinhoodBridge.ObligationStatus.Pending), 1);
        assertEq(uint8(GlcRobinhoodBridge.ObligationStatus.Settled), 2);
        assertEq(uint8(GlcRobinhoodBridge.ObligationStatus.Refunded), 3);
        assertEq(uint8(GlcRobinhoodBridge.ObligationStatus.Abandoned), 4);
    }

    function test_request_executed_view() public {
        bytes32 id = keccak256("v");
        assertFalse(bridge.requestExecuted(bridge.ACTION_PAYOUT(), id));
        _payout(id, bob, OUTBOUND_MIN);
        assertTrue(bridge.requestExecuted(bridge.ACTION_PAYOUT(), id));
    }

    // -----------------------------------------------------------------
    // Terminal guards
    // -----------------------------------------------------------------

    function _migrateFully() internal {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        _finalizeMigration();
    }

    function test_terminal_blocks_set_paused() public {
        _migrateFully();
        uint256 nonce = bridge.governanceNonce();
        bytes32 payload = keccak256(abi.encode(true, true));
        bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, nonce, FAR_FUTURE);
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.setPaused(true, true, nonce, FAR_FUTURE, sigs);
    }

    function test_terminal_blocks_guardian_rotation() public {
        _migrateFully();
        address[3] memory set = [address(0x6B1), address(0x6B2), address(0x6B3)];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_GUARDIANS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.rotateGuardians(set, nonce, FAR_FUTURE, sigs);
    }

    function test_terminal_blocks_further_migration_commit() public {
        _migrateFully();
        address successor = address(_deployConformingSuccessor());
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.commitMigration(successor, nonce, FAR_FUTURE, sigs);
    }

    /// Guardians retain no token-moving power even in the terminal state.
    function test_guardian_pause_after_migration_moves_nothing() public {
        _migrateFully();
        uint256 balance = glc.balanceOf(address(bridge));
        vm.prank(guardian1);
        bridge.guardianPause(true, true);
        assertEq(glc.balanceOf(address(bridge)), balance);
    }
}
