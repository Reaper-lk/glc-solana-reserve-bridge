// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";

import {Vm} from "forge-std/Vm.sol";

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {BlackHoleSuccessor, StateWritingSuccessor} from "./mocks/AdversarialSuccessors.sol";

/// Cases added as a direct result of the Phase D adversarial review. Each one
/// pins a property the review reasoned about, so the reasoning cannot silently
/// stop being true.
contract AdversarialTest is BridgeTestBase {
    // -----------------------------------------------------------------
    // Signature ordering
    // -----------------------------------------------------------------

    /// The SAME pair in both orders must both verify. The contract assumes no
    /// ordering, and nothing may come to depend on one.
    function test_same_pair_in_reversed_order_both_verify() public {
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("order-ab"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        uint256 before = glc.balanceOf(bob);
        bridge.executePayout(r, _quorum(_payoutHash(r), pkA, pkB));

        r.requestId = keccak256("order-ba");
        bridge.executePayout(r, _quorum(_payoutHash(r), pkB, pkA));

        assertEq(glc.balanceOf(bob), before + 1000 * ONE_GLC);
    }

    /// Reversing the order must not let the same signer count twice.
    function test_reversed_order_does_not_bypass_duplicate_check() public {
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("dup-order"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorum(_payoutHash(r), pkC, pkC);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, signerC)
        );
        bridge.executePayout(r, sigs);
    }

    // -----------------------------------------------------------------
    // Cross-action replay: commit vs finalize (identical payload bytes)
    // -----------------------------------------------------------------

    /// `commitMigration` and `finalizeMigration` hash the same payload bytes
    /// (`abi.encode(address)`); only the action discriminator separates them.
    function test_finalize_authorization_cannot_commit() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        uint256 nonce = bridge.governanceNonce();

        bytes32 h = _governanceHash(
            bridge.ACTION_FINALIZE_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert();
        bridge.commitMigration(successor, nonce, FAR_FUTURE, sigs);
        assertFalse(bridge.migrationCommitted());
    }

    function test_commit_authorization_cannot_finalize() public {
        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());

        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert();
        bridge.finalizeMigration(nonce, FAR_FUTURE, sigs);
        assertFalse(bridge.migrated());
    }

    // -----------------------------------------------------------------
    // Successor validation: what it does and does not catch
    // -----------------------------------------------------------------

    /// The successor interface is `view`, so solc emits STATICCALL: a successor
    /// that writes state during validation cannot execute at all. This is what
    /// makes migration validation structurally reentrancy-free.
    function test_state_writing_successor_is_rejected() public {
        _pauseBothRoutes();
        address s = address(new StateWritingSuccessor(address(glc), bridge.BRIDGE_PROTOCOL_ID()));
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(s)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert();
        bridge.commitMigration(s, nonce, FAR_FUTURE, sigs);
        assertFalse(bridge.migrationCommitted());
    }

    /// DOCUMENTS A KNOWN LIMIT, it does not assert safety. A successor that
    /// merely returns the right answers passes every on-chain check and can
    /// swallow the entire reserve permanently. Successor validation catches
    /// wrong-token, wrong-protocol, EOA and self -- nothing more. The 48-hour
    /// delay and human verification are the real gate.
    function test_black_hole_successor_passes_every_onchain_check() public {
        _pauseBothRoutes();
        address s = address(new BlackHoleSuccessor(address(glc), bridge.BRIDGE_PROTOCOL_ID()));
        _commitMigration(s);
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        _finalizeMigration();

        assertEq(glc.balanceOf(s), RESERVE_SEED);
        assertEq(glc.balanceOf(address(bridge)), 0);
        assertTrue(bridge.migrated());
    }

    // -----------------------------------------------------------------
    // Rolling window: the accepted 2x boundary, pinned
    // -----------------------------------------------------------------

    /// The fixed-bucket tradeoff, as a standing regression test: exactly 2x the
    /// configured limit is reachable across a bucket boundary, and no more.
    /// If this test ever fails, the window semantics changed.
    function test_rolling_window_worst_case_is_exactly_two_times() public {
        uint256 t0 = block.timestamp;
        uint256 chunk = INBOUND_MAX;
        uint256 n = INBOUND_ROLLING / chunk;

        for (uint256 i = 0; i < n; ++i) {
            assertEq(_deposit(alice, chunk), i);
        }
        assertEq(bridge.inboundWindow().total, INBOUND_ROLLING);

        // One second early: still the same bucket, nothing more fits.
        vm.warp(t0 + 24 hours - 1);
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());

        // Exactly at the boundary the bucket resets and the full limit returns.
        vm.warp(t0 + 24 hours);
        for (uint256 i = 0; i < n; ++i) {
            assertEq(_deposit(alice, chunk), n + i);
        }

        // 2x the configured limit moved within an 86,400-second span...
        assertEq(bridge.obligationCount(), n * 2);
        assertEq(bridge.outstandingRefundablePrincipal(), INBOUND_ROLLING * 2);

        // ...and NOT a unit more: a third bucket needs another full 24 hours.
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    /// THE CONFIGURATION RULE, as an executable assertion.
    ///
    /// Because the fixed bucket's proven worst case is exactly 2x across a
    /// boundary, the on-chain rolling limit MUST be configured at ONE HALF of
    /// the intended strict 24-hour policy limit. This test demonstrates that
    /// halving works: with the contract set to 50,000 GLC, the worst-case
    /// bucket-boundary burst is exactly the 100,000 GLC policy maximum.
    function test_halved_configuration_bounds_burst_to_policy_limit() public {
        uint256 policyLimit = 100_000 * ONE_GLC;
        uint256 configured = policyLimit / 2;

        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMax = configured;
        lim.inboundRollingLimit = configured;
        uint256 nonce = bridge.governanceNonce();
        bytes32 gh = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(gh));

        uint256 t0 = block.timestamp;
        assertEq(_deposit(alice, configured), 0);

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());

        vm.warp(t0 + 24 hours);
        assertEq(_deposit(alice, configured), 1);

        // Worst case reached, and it equals the intended policy maximum.
        uint256 moved = bridge.outstandingRefundablePrincipal();
        assertEq(moved, configured * 2);
        assertEq(moved, policyLimit, "halved config bounds the burst to policy");

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    // -----------------------------------------------------------------
    // Regressions for the review fixes
    // -----------------------------------------------------------------

    /// An enormous protected floor must produce the NAMED error, not an opaque
    /// arithmetic panic (review finding L-2).
    function test_huge_protected_floor_gives_named_error_not_panic() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = (type(uint256).max / SCALE) * SCALE;
        uint256 nonce = bridge.governanceNonce();
        bytes32 gh = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(gh));

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("panic-probe"),
            recipient: bob,
            amount: OUTBOUND_MIN,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executePayout(r, sigs);
    }

    bytes32 internal constant PAUSED_SIG = keccak256("DirectionPaused(bool,bool,address)");
    bytes32 internal constant UNPAUSED_SIG = keccak256("DirectionUnpaused(bool,bool)");

    function _countPauseEvents(Vm.Log[] memory logs)
        internal
        pure
        returns (uint256 paused, uint256 unpaused)
    {
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].topics.length == 0) continue;
            if (logs[i].topics[0] == PAUSED_SIG) paused++;
            if (logs[i].topics[0] == UNPAUSED_SIG) unpaused++;
        }
    }

    /// A payout larger than the entire balance must give the named error, and
    /// must exercise the first guard of the sequential reserve check rather
    /// than underflowing (review fix J-5).
    function test_payout_exceeding_entire_balance_gives_named_error() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.outboundMax = RESERVE_SEED * 4;
        lim.outboundRollingLimit = RESERVE_SEED * 8;
        uint256 nonce = bridge.governanceNonce();
        bytes32 gh = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(gh));

        uint256 balance = glc.balanceOf(address(bridge));
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("over-balance"),
            recipient: bob,
            amount: balance + SCALE,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executePayout(r, sigs);
    }

    /// A mixed pause transition emits exactly ONE event carrying full state
    /// (review finding L-3). Before the fix this emitted both, telling an
    /// indexer the same thing twice under two contradictory names.
    function test_mixed_pause_emits_exactly_one_event() public {
        vm.recordLogs();
        _setPaused(true, false);
        (uint256 paused, uint256 unpaused) = _countPauseEvents(vm.getRecordedLogs());
        assertEq(paused + unpaused, 1, "exactly one pause event per call");
        assertEq(paused, 1);
        assertEq(unpaused, 0);
    }

    /// Fully open emits the unpaused variant, exactly once.
    function test_full_unpause_emits_exactly_one_unpaused_event() public {
        _pauseBothRoutes();
        vm.recordLogs();
        _setPaused(false, false);
        (uint256 paused, uint256 unpaused) = _countPauseEvents(vm.getRecordedLogs());
        assertEq(unpaused, 1);
        assertEq(paused, 0);
    }
}
