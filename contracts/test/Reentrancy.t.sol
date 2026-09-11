// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {ReentrantGlc} from "./mocks/ReentrantGlc.sol";

contract ReentrancyTest is BridgeTestBase {
    ReentrantGlc internal hostile;
    GlcRobinhoodBridge internal hostileBridge;

    function setUp() public override {
        super.setUp();

        hostile = new ReentrantGlc();
        hostileBridge = new GlcRobinhoodBridge(
            hostile,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits(),
            treasury
        );

        _bootstrap(hostileBridge);

        hostile.mint(alice, RESERVE_SEED);
        hostile.mint(address(hostileBridge), RESERVE_SEED);
        vm.prank(alice);
        assertTrue(hostile.approve(address(hostileBridge), type(uint256).max));
    }

    /// A token that re-enters `deposit` during `transferFrom` must not be able
    /// to observe or exploit the half-updated state.
    function test_deposit_cannot_be_reentered() public {
        hostile.arm(
            address(hostileBridge),
            abi.encodeCall(
                GlcRobinhoodBridge.deposit, (ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination())
            )
        );

        vm.prank(alice);
        uint256 idx = hostileBridge.deposit(ROUTE_RHN_TO_GLC, 500 * ONE_GLC, _destination());

        assertEq(idx, 0);
        assertTrue(hostile.reentryAttempted());
        assertFalse(hostile.reentrySucceeded());
        // Exactly one obligation exists: the re-entrant call created nothing.
        assertEq(hostileBridge.obligationCount(), 1);
        assertEq(hostileBridge.outstandingRefundableCount(), 1);
    }

    /// Likewise for a payout re-entering itself.
    function test_payout_cannot_be_reentered() public {
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("reenter"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs =
            _quorumABOn(hostileBridge, _payoutHashOn(hostileBridge, address(hostile), r));

        hostile.arm(
            address(hostileBridge), abi.encodeCall(GlcRobinhoodBridge.executePayout, (r, sigs))
        );

        uint256 before = hostile.balanceOf(bob);
        hostileBridge.executePayout(r, sigs);

        assertTrue(hostile.reentryAttempted());
        assertFalse(hostile.reentrySucceeded());
        // Paid exactly once.
        assertEq(hostile.balanceOf(bob), before + 500 * ONE_GLC);
    }
}
