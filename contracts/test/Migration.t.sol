// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {MockGlc} from "./mocks/MockGlc.sol";
import {MockSuccessor} from "./mocks/MockSuccessor.sol";
import {NonConformingSuccessor} from "./mocks/NonConformingSuccessor.sol";

contract MigrationTest is BridgeTestBase {
    event MigrationCommitted(address indexed successor, uint64 committedAt, uint64 finalizableAt);
    event MigrationFinalized(address indexed successor, uint256 amount);

    function _commitExpectRevert(address successor, bytes4 err) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(err);
        bridge.commitMigration(successor, nonce, FAR_FUTURE, sigs);
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
    // Commit
    // -----------------------------------------------------------------

    function test_requires_both_directions_paused() public {
        address successor = address(_deployConformingSuccessor());
        _commitExpectRevert(successor, GlcRobinhoodBridge.MigrationRequiresPause.selector);

        _setPaused(true, false);
        _commitExpectRevert(successor, GlcRobinhoodBridge.MigrationRequiresPause.selector);

        _setPaused(false, true);
        _commitExpectRevert(successor, GlcRobinhoodBridge.MigrationRequiresPause.selector);

        _pauseBothRoutes();
        _commitMigration(successor);
        assertTrue(bridge.migrationCommitted());
    }

    function test_requires_quorum() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, h);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.commitMigration(successor, nonce, FAR_FUTURE, sigs);
    }

    function test_rejects_zero_successor() public {
        _pauseBothRoutes();
        _commitExpectRevert(address(0), GlcRobinhoodBridge.ZeroAddress.selector);
    }

    function test_rejects_eoa_successor() public {
        _pauseBothRoutes();
        _commitExpectRevert(outsider, GlcRobinhoodBridge.InvalidSuccessor.selector);
    }

    function test_rejects_self_as_successor() public {
        _pauseBothRoutes();
        _commitExpectRevert(address(bridge), GlcRobinhoodBridge.InvalidSuccessor.selector);
    }

    function test_rejects_successor_with_wrong_token() public {
        _pauseBothRoutes();
        MockGlc otherToken = new MockGlc();
        address bad = address(new MockSuccessor(address(otherToken), bridge.BRIDGE_PROTOCOL_ID()));
        _commitExpectRevert(bad, GlcRobinhoodBridge.InvalidSuccessor.selector);
    }

    function test_rejects_successor_with_wrong_protocol_id() public {
        _pauseBothRoutes();
        address bad = address(new MockSuccessor(address(glc), keccak256("some.other.bridge")));
        _commitExpectRevert(bad, GlcRobinhoodBridge.InvalidSuccessor.selector);
    }

    /// A contract that does not implement the interface at all must be refused,
    /// not accepted by accident.
    function test_rejects_non_conforming_successor() public {
        _pauseBothRoutes();
        address bad = address(new NonConformingSuccessor());
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(bad)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert();
        bridge.commitMigration(bad, nonce, FAR_FUTURE, sigs);
    }

    function test_cannot_commit_twice() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        _commitExpectRevert(successor, GlcRobinhoodBridge.MigrationAlreadyCommitted.selector);
    }

    function test_commit_emits_finalizable_at() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        uint64 nowTs = uint64(block.timestamp);
        vm.expectEmit(true, true, true, true, address(bridge));
        emit MigrationCommitted(successor, nowTs, nowTs + 48 hours);
        _commitMigration(successor);
        assertEq(bridge.migrationFinalizableAt(), nowTs + 48 hours);
    }

    /// Committing permanently closes the routes: they can never be reopened.
    function test_routes_cannot_be_reopened_after_commit() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));

        uint256 nonce = bridge.governanceNonce();
        bytes32 payload = keccak256(abi.encode(false, false));
        bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, nonce, FAR_FUTURE);
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.MigrationAlreadyCommitted.selector);
        bridge.setPaused(false, false, nonce, FAR_FUTURE, sigs);
    }

    function test_no_new_deposits_after_commit() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.MigrationAlreadyCommitted.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    // -----------------------------------------------------------------
    // Delay
    // -----------------------------------------------------------------

    function test_cannot_finalize_before_commit() public {
        _finalizeExpectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.MigrationNotCommitted.selector)
        );
    }

    function test_cannot_finalize_early() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        _finalizeExpectRevert(abi.encodeWithSelector(GlcRobinhoodBridge.MigrationNotReady.selector));
    }

    /// Exact boundary: one second before the delay elapses it must fail, and at
    /// exactly the delay it must succeed.
    function test_delay_boundary_one_second_before() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + 48 hours - 1);
        _finalizeExpectRevert(abi.encodeWithSelector(GlcRobinhoodBridge.MigrationNotReady.selector));
    }

    function test_delay_boundary_exactly_at_delay() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);
        _finalizeMigration();
        assertTrue(bridge.migrated());
    }

    // -----------------------------------------------------------------
    // Outstanding refundable liability
    // -----------------------------------------------------------------

    /// The gate that stops terminal migration from stranding refunds.
    function test_cannot_finalize_with_outstanding_obligations() public {
        uint256 amount = 1000 * ONE_GLC;
        uint256 idx = _deposit(alice, amount);
        assertEq(idx, 0);

        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + 48 hours);

        _finalizeExpectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.OutstandingRefundsRemain.selector, uint256(1), amount
            )
        );
    }

    /// Refunding the outstanding obligation clears the gate.
    function test_refund_clears_the_gate() public {
        uint256 idx = _deposit(alice, 1000 * ONE_GLC);
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);

        _refund(keccak256("r0"), idx);
        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);

        _finalizeMigration();
        assertTrue(bridge.migrated());
    }

    /// So does settling it.
    function test_settlement_clears_the_gate() public {
        uint256 idx = _deposit(alice, 1000 * ONE_GLC);
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);

        _settle(keccak256("s0"), idx);
        _finalizeMigration();
        assertTrue(bridge.migrated());
    }

    /// A mixed set: some settled, some refunded, all must resolve.
    function test_mixed_resolution_clears_the_gate() public {
        uint256 a = _deposit(alice, 1000 * ONE_GLC);
        uint256 b = _deposit(bob, 2000 * ONE_GLC);
        uint256 c = _deposit(alice, 3000 * ONE_GLC);

        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + 48 hours);

        _settle(keccak256("s-a"), a);
        _finalizeExpectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.OutstandingRefundsRemain.selector, uint256(2), 5000 * ONE_GLC
            )
        );

        _refund(keccak256("r-b"), b);
        _settle(keccak256("s-c"), c);
        assertEq(bridge.outstandingRefundableCount(), 0);
        _finalizeMigration();
        assertTrue(bridge.migrated());
    }

    // -----------------------------------------------------------------
    // Finalize
    // -----------------------------------------------------------------

    function test_moves_full_balance_exactly() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);

        uint256 balance = glc.balanceOf(address(bridge));
        assertGt(balance, 0);

        vm.expectEmit(true, true, true, true, address(bridge));
        emit MigrationFinalized(successor, balance);
        _finalizeMigration();

        assertEq(glc.balanceOf(address(bridge)), 0);
        assertEq(glc.balanceOf(successor), balance);
    }

    /// No partial skim is possible: the amount is the whole balance, never a
    /// caller-supplied number.
    function test_no_partial_skim() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);

        uint256 balance = glc.balanceOf(address(bridge));
        _finalizeMigration();
        assertEq(glc.balanceOf(successor), balance);
        assertEq(glc.balanceOf(address(bridge)), 0);
    }

    /// The destination is the committed successor. `finalizeMigration` takes no
    /// address argument at all, so an arbitrary destination is not expressible.
    function test_arbitrary_destination_impossible() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);

        // A quorum over a DIFFERENT payload does not verify, because the
        // payload hash is the stored successor.
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_FINALIZE_MIGRATION(), keccak256(abi.encode(rogue)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert();
        bridge.finalizeMigration(nonce, FAR_FUTURE, sigs);

        _finalizeMigration();
        assertEq(glc.balanceOf(rogue), 0);
        assertEq(glc.balanceOf(successor), RESERVE_SEED);
    }

    /// An empty reserve migrates cleanly: the transfer is skipped, the
    /// contract still becomes terminal, and the event reports zero.
    function test_migrates_empty_reserve() public {
        _payout(keccak256("drain-1"), bob, OUTBOUND_MAX);
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.outboundMax = RESERVE_SEED;
        lim.outboundRollingLimit = RESERVE_SEED * 2;
        uint256 setNonce = bridge.governanceNonce();
        bytes32 setHash = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), setNonce, FAR_FUTURE
        );
        bridge.setLimits(lim, setNonce, FAR_FUTURE, _quorumAB(setHash));
        _payout(keccak256("drain-2"), bob, glc.balanceOf(address(bridge)));
        assertEq(glc.balanceOf(address(bridge)), 0);

        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);

        vm.expectEmit(true, true, true, true, address(bridge));
        emit MigrationFinalized(successor, 0);
        _finalizeMigration();

        assertTrue(bridge.migrated());
        assertEq(glc.balanceOf(successor), 0);
    }

    function test_cannot_migrate_twice() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + 48 hours);
        _finalizeMigration();
        _finalizeExpectRevert(abi.encodeWithSelector(GlcRobinhoodBridge.AlreadyMigrated.selector));
    }

    // -----------------------------------------------------------------
    // Terminal state
    // -----------------------------------------------------------------

    function _migrateFully() internal returns (address successor) {
        _pauseBothRoutes();
        successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + 48 hours);
        _finalizeMigration();
    }

    function test_terminal_blocks_deposit() public {
        _migrateFully();
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    function test_terminal_blocks_payout() public {
        _migrateFully();
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("after"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.executePayout(r, sigs);
    }

    function test_terminal_blocks_refund() public {
        _migrateFully();
        GlcRobinhoodBridge.RefundRequest memory r = GlcRobinhoodBridge.RefundRequest({
            requestId: keccak256("after-r"),
            obligationIndex: 0,
            recipient: alice,
            amount: 1000 * ONE_GLC,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_terminal_blocks_governance() public {
        _migrateFully();

        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.setLimits(lim, nonce, FAR_FUTURE, sigs);
    }

    function test_terminal_blocks_signer_rotation() public {
        _migrateFully();
        address[3] memory set = [address(0x5A1), address(0x5A2), address(0x5A3)];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
    }
}
