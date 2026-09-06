// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {FeeOnTransferGlc} from "./mocks/FeeOnTransferGlc.sol";

contract DepositTest is BridgeTestBase {
    event DepositCreated(
        uint256 indexed obligationIndex,
        address indexed depositor,
        uint8 indexed route,
        uint256 amount,
        uint256 canonicalAmount,
        bytes destination
    );

    function test_happy_path() public {
        uint256 amount = 500 * ONE_GLC;
        uint256 bridgeBefore = glc.balanceOf(address(bridge));
        uint256 aliceBefore = glc.balanceOf(alice);

        uint256 index = _deposit(alice, amount);

        assertEq(index, 0);
        assertEq(glc.balanceOf(address(bridge)), bridgeBefore + amount);
        assertEq(glc.balanceOf(alice), aliceBefore - amount);

        GlcRobinhoodBridge.Obligation memory ob = bridge.obligation(0);
        assertEq(ob.depositor, alice);
        assertEq(ob.amount, amount);
        assertTrue(ob.status == GlcRobinhoodBridge.ObligationStatus.Pending);

        assertEq(bridge.obligationCount(), 1);
        assertEq(bridge.outstandingRefundableCount(), 1);
        assertEq(bridge.outstandingRefundablePrincipal(), amount);
    }

    /// The obligation index is contract-local and deterministic: it starts at
    /// zero and increments by exactly one.
    function test_index_starts_at_zero_and_increments() public {
        assertEq(_deposit(alice, INBOUND_MIN), 0);
        assertEq(_deposit(bob, INBOUND_MIN), 1);
        assertEq(_deposit(alice, INBOUND_MIN), 2);
        assertEq(bridge.obligationCount(), 3);
    }

    function test_emits_complete_event() public {
        uint256 amount = 777 * ONE_GLC;
        vm.expectEmit(true, true, true, true, address(bridge));
        emit DepositCreated(0, alice, ROUTE_RHN_TO_GLC, amount, amount / SCALE, _destination());
        uint256 index = _deposit(alice, amount);
        assertEq(index, 0);
    }

    /// The canonical amount is the exact 8-decimal value, with no rounding
    /// anywhere: scaling down and back up must be the identity.
    function test_canonical_amount_is_exact() public {
        uint256 amount = INBOUND_MIN + 12_345 * SCALE;
        uint256 index = _deposit(alice, amount);
        assertEq(index, 0);
        assertEq(bridge.obligation(0).amount, amount);
        assertEq((amount / SCALE) * SCALE, amount);
    }

    function test_rejects_below_minimum() public {
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.AmountBelowMinimum.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN - SCALE, _destination());
    }

    function test_accepts_exact_minimum() public {
        assertEq(_deposit(alice, INBOUND_MIN), 0);
    }

    function test_rejects_above_maximum() public {
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.AmountAboveMaximum.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MAX + SCALE, _destination());
    }

    function test_accepts_exact_maximum() public {
        assertEq(_deposit(alice, INBOUND_MAX), 0);
    }

    function test_rejects_zero_amount() public {
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.InvalidAmount.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, 0, _destination());
    }

    /// The core decimal invariant: an amount the canonical 8-decimal ledger
    /// cannot represent exactly must never create an obligation.
    function test_rejects_non_canonical_granularity() public {
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.NonCanonicalAmount.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN + 1, _destination());
    }

    function test_rejects_off_by_one_below_scale() public {
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.NonCanonicalAmount.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN + SCALE - 1, _destination());
    }

    function test_rejects_when_paused() public {
        _setPaused(true, false);
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    function test_rejects_empty_destination() public {
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.InvalidDestinationLength.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, "");
    }

    function test_rejects_oversized_destination() public {
        bytes memory tooLong = new bytes(65);
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.InvalidDestinationLength.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, tooLong);
    }

    function test_accepts_max_length_destination() public {
        bytes memory maxLen = new bytes(64);
        vm.prank(alice);
        uint256 index = bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, maxLen);
        assertEq(index, 0);
    }

    /// A deflationary token delivers less than requested; the bridge must
    /// refuse rather than book an obligation it cannot cover.
    function test_rejects_fee_on_transfer_token() public {
        FeeOnTransferGlc feeToken = new FeeOnTransferGlc();
        GlcRobinhoodBridge feeBridge = new GlcRobinhoodBridge(
            feeToken,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits()
        );

        _bootstrap(feeBridge);

        feeToken.mint(alice, RESERVE_SEED);
        vm.startPrank(alice);
        assertTrue(feeToken.approve(address(feeBridge), type(uint256).max));
        uint256 amount = 1000 * ONE_GLC;
        uint256 expectedReceived = amount - (amount * feeToken.FEE_BPS()) / 10_000;
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InexactTransfer.selector, amount, expectedReceived
            )
        );
        feeBridge.deposit(ROUTE_RHN_TO_GLC, amount, _destination());
        vm.stopPrank();

        assertEq(feeBridge.obligationCount(), 0);
    }

    // -----------------------------------------------------------------
    // Rolling limit
    // -----------------------------------------------------------------

    /// A user calling the contract directly cannot exceed the contract-side
    /// rolling limit, regardless of what the backend would have admitted.
    function test_rolling_limit_blocks_direct_bypass() public {
        uint256 chunk = INBOUND_MAX;
        uint256 fullChunks = INBOUND_ROLLING / chunk;
        for (uint256 i = 0; i < fullChunks; ++i) {
            uint256 idx = _deposit(alice, chunk);
            assertEq(idx, i);
        }
        assertEq(bridge.inboundWindow().total, INBOUND_ROLLING);

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    function test_rolling_window_resets_after_period() public {
        uint256 chunk = INBOUND_MAX;
        uint256 fullChunks = INBOUND_ROLLING / chunk;
        for (uint256 i = 0; i < fullChunks; ++i) {
            uint256 idx = _deposit(alice, chunk);
            assertEq(idx, i);
        }
        vm.warp(block.timestamp + bridge.ROLLING_WINDOW_SECONDS());
        uint256 next = _deposit(alice, INBOUND_MIN);
        assertEq(next, fullChunks);
        assertEq(bridge.inboundWindow().total, INBOUND_MIN);
    }

    /// One second before the bucket expires, the old total still applies.
    function test_rolling_window_boundary_is_exact() public {
        uint256 chunk = INBOUND_MAX;
        uint256 fullChunks = INBOUND_ROLLING / chunk;
        for (uint256 i = 0; i < fullChunks; ++i) {
            uint256 idx = _deposit(alice, chunk);
            assertEq(idx, i);
        }
        vm.warp(block.timestamp + bridge.ROLLING_WINDOW_SECONDS() - 1);
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }

    /// A rejected deposit must not advance the window.
    function test_rejected_deposit_does_not_advance_window() public {
        uint256 idx = _deposit(alice, INBOUND_MAX);
        assertEq(idx, 0);
        uint256 totalAfter = bridge.inboundWindow().total;

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.NonCanonicalAmount.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN + 1, _destination());

        assertEq(bridge.inboundWindow().total, totalAfter);
    }

    /// Inbound and outbound accounting are entirely independent.
    function test_inbound_does_not_consume_outbound_window() public {
        uint256 idx = _deposit(alice, INBOUND_MAX);
        assertEq(idx, 0);
        assertEq(bridge.inboundWindow().total, INBOUND_MAX);
        assertEq(bridge.outboundWindow().total, 0);
    }

    function test_rejects_after_migration_committed() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.MigrationAlreadyCommitted.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
    }
}
