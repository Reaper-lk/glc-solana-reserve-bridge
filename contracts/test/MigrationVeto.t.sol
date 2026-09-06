// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

/// Review finding M-1: a single guardian must be able to stop a pending
/// migration, and that right must not expire on a timer.
contract MigrationVetoTest is BridgeTestBase {
    event MigrationVetoed(address indexed guardian, address indexed successor);

    address internal successor;

    function setUp() public override {
        super.setUp();
        _pauseBothRoutes();
        successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
    }

    function _finalizeExpectRevert(bytes memory err) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_FINALIZE_MIGRATION(),
            keccak256(abi.encode(bridge.migrationSuccessor())),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(err);
        bridge.finalizeMigration(nonce, FAR_FUTURE, sigs);
    }

    // -----------------------------------------------------------------
    // Who may veto
    // -----------------------------------------------------------------

    function test_each_guardian_can_veto() public {
        address[3] memory gs = [guardian1, guardian2, guardian3];
        for (uint256 i = 0; i < gs.length; ++i) {
            if (i > 0) _commitMigration(successor);
            assertTrue(bridge.migrationCommitted());
            vm.prank(gs[i]);
            bridge.vetoMigration();
            assertFalse(bridge.migrationCommitted());
        }
    }

    function test_non_guardian_cannot_veto() public {
        vm.prank(outsider);
        vm.expectRevert(GlcRobinhoodBridge.UnauthorizedGuardian.selector);
        bridge.vetoMigration();
        assertTrue(bridge.migrationCommitted());
    }

    /// A bridge SIGNER is not a guardian; the veto is a separate authority.
    function test_signer_cannot_veto() public {
        vm.prank(signerA);
        vm.expectRevert(GlcRobinhoodBridge.UnauthorizedGuardian.selector);
        bridge.vetoMigration();
    }

    /// A rotated-out guardian loses the veto immediately.
    function test_removed_guardian_cannot_veto_after_rotation() public {
        address g4 = address(0x6C1);
        address[3] memory set = [g4, address(0x6C2), address(0x6C3)];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_GUARDIANS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bridge.rotateGuardians(set, nonce, FAR_FUTURE, _quorumAB(h));

        vm.prank(guardian1);
        vm.expectRevert(GlcRobinhoodBridge.UnauthorizedGuardian.selector);
        bridge.vetoMigration();

        vm.prank(g4);
        bridge.vetoMigration();
        assertFalse(bridge.migrationCommitted());
    }

    // -----------------------------------------------------------------
    // When it may be used
    // -----------------------------------------------------------------

    function test_veto_before_delay_elapses() public {
        vm.warp(block.timestamp + 1 hours);
        vm.prank(guardian1);
        bridge.vetoMigration();
        assertFalse(bridge.migrationCommitted());
    }

    /// The right must NOT expire merely because 48 hours passed. An attacker
    /// who can wait out a timer is exactly the threat this defends against.
    function test_veto_still_works_after_delay_elapses() public {
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        vm.prank(guardian1);
        bridge.vetoMigration();
        assertFalse(bridge.migrationCommitted());
        _finalizeExpectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.MigrationNotCommitted.selector)
        );
    }

    function test_veto_works_long_after_delay() public {
        vm.warp(block.timestamp + 365 days);
        vm.prank(guardian1);
        bridge.vetoMigration();
        assertFalse(bridge.migrationCommitted());
    }

    function test_veto_impossible_after_finalize() public {
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        _finalizeMigration();
        assertTrue(bridge.migrated());

        vm.prank(guardian1);
        vm.expectRevert(GlcRobinhoodBridge.MigrationAlreadyFinalized.selector);
        bridge.vetoMigration();
    }

    function test_veto_with_no_pending_migration_reverts() public {
        vm.prank(guardian1);
        bridge.vetoMigration();

        vm.prank(guardian2);
        vm.expectRevert(GlcRobinhoodBridge.NoPendingMigration.selector);
        bridge.vetoMigration();
    }

    // -----------------------------------------------------------------
    // Effects
    // -----------------------------------------------------------------

    function test_veto_clears_successor_and_timestamp() public {
        vm.expectEmit(true, true, true, true, address(bridge));
        emit MigrationVetoed(guardian1, successor);
        vm.prank(guardian1);
        bridge.vetoMigration();

        assertEq(bridge.migrationSuccessor(), address(0));
        assertEq(bridge.migrationCommittedAt(), 0);
        assertEq(bridge.migrationFinalizableAt(), 0);
        assertFalse(bridge.migrationCommitted());
        assertFalse(bridge.migrated());
    }

    function test_finalize_fails_after_veto() public {
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        vm.prank(guardian1);
        bridge.vetoMigration();
        _finalizeExpectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.MigrationNotCommitted.selector)
        );
        assertEq(glc.balanceOf(successor), 0);
    }

    /// A veto stops a migration; it does not reopen a bridge that operators
    /// deliberately closed.
    function test_routes_remain_paused_after_veto() public {
        vm.prank(guardian1);
        bridge.vetoMigration();

        assertTrue(bridge.depositsPaused());
        assertTrue(bridge.payoutsPaused());

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    /// Guardians move no tokens, ever.
    function test_veto_moves_no_tokens() public {
        uint256 bridgeBefore = glc.balanceOf(address(bridge));
        uint256 guardianBefore = glc.balanceOf(guardian1);
        uint256 successorBefore = glc.balanceOf(successor);

        vm.prank(guardian1);
        bridge.vetoMigration();

        assertEq(glc.balanceOf(address(bridge)), bridgeBefore);
        assertEq(glc.balanceOf(guardian1), guardianBefore);
        assertEq(glc.balanceOf(successor), successorBefore);
    }

    /// A veto grants no configuration power of any kind.
    function test_veto_changes_no_configuration() public {
        address[3] memory signersBefore = bridge.signers();
        address[3] memory guardiansBefore = bridge.guardians();
        uint64 epochBefore = bridge.signerEpoch();
        uint256 nonceBefore = bridge.governanceNonce();
        GlcRobinhoodBridge.Limits memory limsBefore = bridge.limits();

        vm.prank(guardian1);
        bridge.vetoMigration();

        assertEq(bridge.signers()[0], signersBefore[0]);
        assertEq(bridge.guardians()[0], guardiansBefore[0]);
        assertEq(bridge.signerEpoch(), epochBefore);
        assertEq(bridge.governanceNonce(), nonceBefore);
        assertEq(bridge.limits().inboundMax, limsBefore.inboundMax);
        assertEq(bridge.limits().protectedMinReserve, limsBefore.protectedMinReserve);
    }

    // -----------------------------------------------------------------
    // Recovery / replay
    // -----------------------------------------------------------------

    /// The commit authorization that created the vetoed migration cannot be
    /// replayed to bring it back: its governance nonce is already spent.
    function test_old_commit_authorization_cannot_restore_migration() public {
        uint256 usedNonce = bridge.governanceNonce() - 1;
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(),
            keccak256(abi.encode(successor)),
            usedNonce,
            FAR_FUTURE
        );
        bytes[] memory staleSigs = _quorumAB(h);

        vm.prank(guardian1);
        bridge.vetoMigration();

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, usedNonce + 1, usedNonce
            )
        );
        bridge.commitMigration(successor, usedNonce, FAR_FUTURE, staleSigs);
        assertFalse(bridge.migrationCommitted());
    }

    /// A finalize authorization signed BEFORE the veto is dead afterwards: a
    /// re-commit consumes the nonce it was bound to.
    function test_presigned_finalize_dies_after_veto_and_recommit() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_FINALIZE_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bytes[] memory presigned = _quorumAB(h);

        vm.prank(guardian1);
        bridge.vetoMigration();

        _commitMigration(successor);
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, nonce + 1, nonce
            )
        );
        bridge.finalizeMigration(nonce, FAR_FUTURE, presigned);
    }

    /// A fresh migration can be committed normally after a veto, and it
    /// restarts the full 48-hour clock.
    function test_fresh_migration_after_veto_restarts_the_clock() public {
        vm.warp(block.timestamp + 47 hours);
        vm.prank(guardian1);
        bridge.vetoMigration();

        uint256 recommitAt = block.timestamp;
        _commitMigration(successor);
        assertEq(bridge.migrationFinalizableAt(), recommitAt + 48 hours);

        vm.warp(recommitAt + 48 hours - 1);
        _finalizeExpectRevert(abi.encodeWithSelector(GlcRobinhoodBridge.MigrationNotReady.selector));

        vm.warp(recommitAt + 48 hours);
        _finalizeMigration();
        assertTrue(bridge.migrated());
    }

    /// A guardian can veto repeatedly; the quorum never wins by waiting.
    function test_guardian_can_veto_repeatedly() public {
        for (uint256 i = 0; i < 3; ++i) {
            vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
            vm.prank(guardian1);
            bridge.vetoMigration();
            assertFalse(bridge.migrationCommitted());
            _commitMigration(successor);
        }
        assertTrue(bridge.migrationCommitted());
        assertEq(glc.balanceOf(successor), 0);
    }

    /// A different successor may be committed after a veto -- but only by a
    /// fresh quorum, never by the guardian.
    function test_veto_cannot_nominate_a_replacement() public {
        vm.prank(guardian1);
        bridge.vetoMigration();
        assertEq(bridge.migrationSuccessor(), address(0));

        address other = address(_deployConformingSuccessor());
        _commitMigration(other);
        assertEq(bridge.migrationSuccessor(), other);
    }
}
