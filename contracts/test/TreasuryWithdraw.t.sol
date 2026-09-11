// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {FeeOnTransferGlc} from "./mocks/FeeOnTransferGlc.sol";
import {MockSuccessor} from "./mocks/MockSuccessor.sol";

/// `executeTreasuryWithdraw`: the fourth and last way GLC leaves custody.
///
/// Organised by the gate that refuses, in the order the contract enforces
/// them, then the exact-transfer and replay semantics, then the things a
/// withdrawal must NOT be bounded by.
contract TreasuryWithdrawTest is BridgeTestBase {
    event TreasuryWithdrawExecuted(
        bytes32 indexed requestId, address indexed treasury, uint256 amount, uint64 signerEpoch
    );

    bytes32 internal constant REQ = keccak256("treasury-withdraw-1");

    function _req(address treasury_, uint256 amount)
        internal
        view
        returns (GlcRobinhoodBridge.TreasuryWithdrawRequest memory)
    {
        return GlcRobinhoodBridge.TreasuryWithdrawRequest({
            requestId: REQ,
            treasury: treasury_,
            amount: amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
    }

    function _withdraw(GlcRobinhoodBridge.TreasuryWithdrawRequest memory r) internal {
        bridge.executeTreasuryWithdraw(r, _quorumAB(_treasuryWithdrawHash(r)));
    }

    function _setProtectedFloor(uint256 floor) internal {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = floor;
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(h));
    }

    // -----------------------------------------------------------------
    // The happy path
    // -----------------------------------------------------------------

    function test_valid_withdrawal_with_both_directions_paused() public {
        _pauseBothRoutes();
        uint256 amount = 2500 * ONE_GLC;
        uint256 reserveBefore = glc.balanceOf(address(bridge));
        uint256 treasuryBefore = glc.balanceOf(treasury);

        vm.expectEmit(true, true, true, true, address(bridge));
        emit TreasuryWithdrawExecuted(REQ, treasury, amount, 0);
        _withdraw(_req(treasury, amount));

        assertEq(glc.balanceOf(treasury), treasuryBefore + amount, "treasury credited exactly");
        assertEq(glc.balanceOf(address(bridge)), reserveBefore - amount, "reserve debited exactly");
        assertTrue(bridge.requestExecuted(bridge.ACTION_TREASURY_WITHDRAW(), REQ));
    }

    function test_any_distinct_signer_pair_in_any_order() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bridge.executeTreasuryWithdraw(r, _quorum(_treasuryWithdrawHash(r), pkC, pkA));
        r.requestId = keccak256("treasury-withdraw-2");
        bridge.executeTreasuryWithdraw(r, _quorum(_treasuryWithdrawHash(r), pkB, pkC));
    }

    function test_treasury_view_reports_the_immutable() public view {
        assertEq(bridge.treasury(), treasury);
        assertEq(bridge.TREASURY(), treasury);
    }

    // -----------------------------------------------------------------
    // Gate 4: the authoritative pause. Both flags, on chain.
    // -----------------------------------------------------------------

    function test_refuses_while_fully_open() public {
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.WithdrawalRequiresPause.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    function test_refuses_with_only_payouts_paused() public {
        _setPaused(false, true);
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.WithdrawalRequiresPause.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    function test_refuses_with_only_deposits_paused() public {
        _setPaused(true, false);
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.WithdrawalRequiresPause.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    /// A guardian's unilateral pause is sufficient: the requirement is the
    /// STATE, not who produced it.
    function test_a_guardian_pause_satisfies_the_requirement() public {
        vm.prank(guardian1);
        bridge.guardianPause(true, true);
        _withdraw(_req(treasury, 100 * ONE_GLC));
    }

    // -----------------------------------------------------------------
    // Gates 2 and 3: the destination
    // -----------------------------------------------------------------

    function test_refuses_any_destination_other_than_the_immutable_treasury() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(bob, 100 * ONE_GLC);
        // Signed by a real quorum — the SIGNATURE is fine, the destination
        // is not. The contract must refuse on the destination, not on the
        // signature, so the operator sees the real reason.
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.WrongTreasury.selector, treasury, bob)
        );
        bridge.executeTreasuryWithdraw(r, sigs);
        assertEq(glc.balanceOf(bob), RESERVE_SEED, "nothing moved");
    }

    /// A signature over the correct treasury cannot be executed with a
    /// different one: the treasury is bound into the digest.
    function test_the_treasury_is_bound_into_the_signed_digest() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory signed = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(signed));
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory swapped = _req(bob, 100 * ONE_GLC);
        // Refused on the destination first (gate 3 precedes the signature
        // check), which is the point: the destination check does not depend
        // on the signature being wrong.
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.WrongTreasury.selector, treasury, bob)
        );
        bridge.executeTreasuryWithdraw(swapped, sigs);
    }

    function test_a_zero_treasury_declines_the_path_by_construction() public {
        GlcRobinhoodBridge b = new GlcRobinhoodBridge(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits(),
            address(0)
        );
        glc.mint(address(b), RESERVE_SEED);
        assertEq(b.treasury(), address(0));
        // Deployed paused in both directions already.
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r =
            GlcRobinhoodBridge.TreasuryWithdrawRequest({
                requestId: REQ,
                treasury: address(0),
                amount: 100 * ONE_GLC,
                signerEpoch: b.signerEpoch(),
                expiry: FAR_FUTURE
            });
        bytes[] memory sigs = _quorumABOn(b, _treasuryWithdrawHashOn(b, r));
        vm.expectRevert(GlcRobinhoodBridge.TreasuryNotConfigured.selector);
        b.executeTreasuryWithdraw(r, sigs);
    }

    function test_construction_refuses_the_token_as_treasury() public {
        vm.expectRevert(GlcRobinhoodBridge.InvalidTreasury.selector);
        new GlcRobinhoodBridge(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits(),
            address(glc)
        );
    }

    // -----------------------------------------------------------------
    // Gates 5 and 6: amount and reserve floor
    // -----------------------------------------------------------------

    function test_refuses_zero_amount() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 0);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidAmount.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    function test_refuses_a_non_canonical_amount() public {
        _pauseBothRoutes();
        // One atomic unit past a canonical boundary: unrepresentable at 8dp.
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC + 1);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.NonCanonicalAmount.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    function test_respects_the_protected_reserve_floor() public {
        _pauseBothRoutes();
        uint256 balance = glc.balanceOf(address(bridge));
        _setProtectedFloor(balance - 100 * ONE_GLC);

        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 200 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executeTreasuryWithdraw(r, sigs);

        // Exactly down to the floor is permitted.
        _withdraw(_req(treasury, 100 * ONE_GLC));
        assertEq(glc.balanceOf(address(bridge)), balance - 100 * ONE_GLC);
    }

    /// The reserve may never be withdrawn out of a depositor's unsettled
    /// principal — the same floor every payout obeys.
    function test_cannot_withdraw_outstanding_refundable_principal() public {
        uint256 principal = 1000 * ONE_GLC;
        _deposit(alice, principal);
        _pauseBothRoutes();
        uint256 balance = glc.balanceOf(address(bridge));

        // Everything but the principal is withdrawable...
        _withdraw(_req(treasury, balance - principal));
        assertEq(glc.balanceOf(address(bridge)), principal);

        // ...and not one canonical unit more.
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        r.requestId = keccak256("treasury-withdraw-2");
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    // -----------------------------------------------------------------
    // What a withdrawal is deliberately NOT bounded by
    // -----------------------------------------------------------------

    /// No `outboundMax`: the whole spendable reserve moves in one call.
    function test_is_not_capped_by_the_per_transfer_maximum() public {
        _pauseBothRoutes();
        uint256 amount = OUTBOUND_MAX * 10;
        assertGt(amount, OUTBOUND_MAX);
        _withdraw(_req(treasury, amount));
        assertEq(glc.balanceOf(treasury), amount);
    }

    /// No rolling window: a withdrawal neither consumes the outbound
    /// bucket nor is refused by it.
    function test_does_not_touch_the_outbound_rolling_window() public {
        _pauseBothRoutes();
        uint256 usedBefore = bridge.outboundWindow().total;
        _withdraw(_req(treasury, OUTBOUND_ROLLING + OUTBOUND_MAX));
        assertEq(bridge.outboundWindow().total, usedBefore, "rolling window untouched");
    }

    /// The entire spendable reserve, in one withdrawal.
    function test_can_withdraw_the_whole_reserve_above_the_floor() public {
        _pauseBothRoutes();
        uint256 balance = glc.balanceOf(address(bridge));
        _withdraw(_req(treasury, balance));
        assertEq(glc.balanceOf(address(bridge)), 0);
        assertEq(glc.balanceOf(treasury), balance);
    }

    /// THE policy test. A reserve that has been deliberately prepared for
    /// a drain — every depositor liability settled or refunded, the
    /// governance floor lowered to zero — is withdrawable IN FULL, in one
    /// call, with no cap of any kind standing in the way. The only
    /// constraints on the way there are accounting ones, and each is
    /// shown refusing exactly until it is legitimately cleared.
    function test_the_entire_reserve_is_withdrawable_once_liabilities_are_cleared() public {
        // A live depositor liability and a governance floor.
        uint256 principal = 1000 * ONE_GLC;
        uint256 index = _deposit(alice, principal);
        _setProtectedFloor(500 * ONE_GLC);
        _pauseBothRoutes();
        uint256 balance = glc.balanceOf(address(bridge));

        // 1. The whole balance is refused while a depositor is owed.
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory all = _req(treasury, balance);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(all));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executeTreasuryWithdraw(all, sigs);

        // 2. Liability cleared: the depositor is refunded their principal.
        _refund(keccak256("refund-1"), index);
        balance = glc.balanceOf(address(bridge));
        assertEq(bridge.outstandingRefundablePrincipal(), 0);

        // 3. Still refused by the FLOOR — an accounting constraint, not a
        //    cap — for exactly the floor's amount.
        all = _req(treasury, balance);
        sigs = _quorumAB(_treasuryWithdrawHash(all));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executeTreasuryWithdraw(all, sigs);

        // 4. Governance lowers the floor to zero: this is the deliberate
        //    "the reserve is being drained" decision, made by 2-of-3.
        _setProtectedFloor(0);

        // 5. Everything, in one withdrawal. No per-transfer, daily,
        //    rolling or percentage limit was ever consulted.
        _withdraw(_req(treasury, balance));
        assertEq(glc.balanceOf(address(bridge)), 0, "reserve fully drained");
        assertEq(glc.balanceOf(treasury), balance, "treasury holds all of it");
        assertGt(balance, OUTBOUND_MAX, "and it was far above the user payout cap");
        assertGt(balance, OUTBOUND_ROLLING, "and above the user rolling limit");
    }

    // -----------------------------------------------------------------
    // Gate 7: authorization
    // -----------------------------------------------------------------

    function test_one_signature_is_never_enough() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory one = new bytes[](1);
        one[0] = _sign(pkA, _treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executeTreasuryWithdraw(r, one);
    }

    function test_the_same_signer_twice_is_a_quorum_of_one() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorum(_treasuryWithdrawHash(r), pkA, pkA);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, signerA)
        );
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    function test_a_non_signer_is_refused() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorum(_treasuryWithdrawHash(r), pkA, pkRogue);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    /// A payout quorum over identical fields is not a withdrawal quorum:
    /// the typehash and the action byte both differ.
    function test_a_payout_signature_cannot_execute_a_withdrawal() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.PayoutRequest memory p = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: REQ,
            recipient: treasury,
            amount: 100 * ONE_GLC,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory payoutSigs = _quorumAB(_payoutHash(p));
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        // Recovery yields SOME address from a signature over the wrong
        // digest; it is not a signer.
        vm.expectRevert();
        bridge.executeTreasuryWithdraw(r, payoutSigs);
    }

    function test_expired_authorization_is_refused() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        r.expiry = uint64(block.timestamp - 1);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AuthorizationExpired.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    function test_stale_signer_epoch_is_refused() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        r.signerEpoch = bridge.signerEpoch() + 1;
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidSignerEpoch.selector, bridge.signerEpoch(), r.signerEpoch
            )
        );
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    // -----------------------------------------------------------------
    // Gate 8: replay
    // -----------------------------------------------------------------

    function test_the_same_request_id_executes_exactly_once() public {
        _pauseBothRoutes();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        bridge.executeTreasuryWithdraw(r, sigs);
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
        assertEq(glc.balanceOf(treasury), 100 * ONE_GLC, "paid once");
    }

    /// The replay guard is keyed on the action too: a payout that consumed
    /// this request id does not block a withdrawal under the same id, and
    /// vice versa. One id, one action, once.
    function test_replay_guard_is_per_action() public {
        _payout(REQ, bob, 100 * ONE_GLC);
        _pauseBothRoutes();
        _withdraw(_req(treasury, 100 * ONE_GLC));
        assertTrue(bridge.requestExecuted(bridge.ACTION_PAYOUT(), REQ));
        assertTrue(bridge.requestExecuted(bridge.ACTION_TREASURY_WITHDRAW(), REQ));
    }

    // -----------------------------------------------------------------
    // Gate 1: migration
    // -----------------------------------------------------------------

    function test_refused_after_migration() public {
        _pauseBothRoutes();
        MockSuccessor successor = _deployConformingSuccessor();
        _commitMigration(address(successor));
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        _finalizeMigration();
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r = _req(treasury, 100 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_treasuryWithdrawHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.executeTreasuryWithdraw(r, sigs);
    }

    /// A withdrawal while a migration is merely COMMITTED is permitted:
    /// both directions are paused (a precondition of committing), and
    /// moving reserve to the treasury before finalization is a legitimate
    /// operator decision that finalization then simply moves less.
    function test_permitted_while_a_migration_is_pending() public {
        _pauseBothRoutes();
        MockSuccessor successor = _deployConformingSuccessor();
        _commitMigration(address(successor));
        _withdraw(_req(treasury, 100 * ONE_GLC));
    }

    // -----------------------------------------------------------------
    // Exact transfer
    // -----------------------------------------------------------------

    /// A token that moves less than asked reverts the whole call rather
    /// than emitting a figure that did not leave custody.
    function test_reverts_on_an_inexact_transfer() public {
        FeeOnTransferGlc fee = new FeeOnTransferGlc();
        GlcRobinhoodBridge b = new GlcRobinhoodBridge(
            fee,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits(),
            treasury
        );
        fee.mint(address(b), RESERVE_SEED);
        // Deployed paused in both directions.
        uint256 amount = 100 * ONE_GLC;
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory r =
            GlcRobinhoodBridge.TreasuryWithdrawRequest({
                requestId: REQ,
                treasury: treasury,
                amount: amount,
                signerEpoch: b.signerEpoch(),
                expiry: FAR_FUTURE
            });
        bytes[] memory sigs = _quorumABOn(b, _treasuryWithdrawHashOn(b, r));
        vm.expectRevert();
        b.executeTreasuryWithdraw(r, sigs);
        assertEq(fee.balanceOf(address(b)), RESERVE_SEED, "nothing left custody");
        assertFalse(b.requestExecuted(b.ACTION_TREASURY_WITHDRAW(), REQ), "not consumed");
    }

    // -----------------------------------------------------------------
    // The other three outflows are unchanged
    // -----------------------------------------------------------------

    /// A refund is still possible while paused and after a withdrawal:
    /// the withdrawal cannot have eaten the principal a refund returns.
    function test_a_refund_still_works_after_a_withdrawal() public {
        uint256 principal = 1000 * ONE_GLC;
        uint256 index = _deposit(alice, principal);
        _pauseBothRoutes();
        uint256 balance = glc.balanceOf(address(bridge));
        _withdraw(_req(treasury, balance - principal));

        uint256 aliceBefore = glc.balanceOf(alice);
        _refund(keccak256("refund-1"), index);
        assertEq(glc.balanceOf(alice), aliceBefore + principal);
    }
}
