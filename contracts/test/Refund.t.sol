// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

contract RefundTest is BridgeTestBase {
    event RefundExecuted(
        uint256 indexed obligationIndex,
        bytes32 indexed requestId,
        address indexed recipient,
        uint256 amount
    );

    bytes32 internal constant REQ = keccak256("refund-1");
    uint256 internal constant DEPOSIT_AMOUNT = 1000 * 1e18;

    function _mkReq(uint256 index, address recipient, uint256 amount)
        internal
        view
        returns (GlcRobinhoodBridge.RefundRequest memory)
    {
        return GlcRobinhoodBridge.RefundRequest({
            requestId: REQ,
            obligationIndex: index,
            recipient: recipient,
            amount: amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
    }

    function test_happy_path() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 aliceBefore = glc.balanceOf(alice);

        vm.expectEmit(true, true, true, true, address(bridge));
        emit RefundExecuted(idx, REQ, alice, DEPOSIT_AMOUNT);
        _refund(REQ, idx);

        assertEq(glc.balanceOf(alice), aliceBefore + DEPOSIT_AMOUNT);
        assertTrue(bridge.obligation(idx).status == GlcRobinhoodBridge.ObligationStatus.Refunded);
        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
    }

    /// The single most important property: a fully valid 2-of-3 quorum still
    /// cannot send a refund anywhere except the original depositor. This is
    /// what stops `executeRefund` from being a treasury withdrawal.
    function test_valid_quorum_cannot_redirect_to_arbitrary_address() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, rogue, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidRefundRecipient.selector);
        bridge.executeRefund(r, sigs);
    }

    /// Likewise for the amount: signers choose WHICH obligation, never how much.
    function test_valid_quorum_cannot_over_refund() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT + SCALE);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidRefundAmount.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_no_partial_refund() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT / 2);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidRefundAmount.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_nonexistent_obligation() public {
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(0, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_index_beyond_count() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        assertEq(idx, 0);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(1, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_duplicate_refund() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _refund(REQ, idx);

        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        r.requestId = keccak256("refund-2");
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeRefund(r, sigs);
    }

    /// A settled obligation is no longer refundable: settle and refund are
    /// mutually exclusive exits from `Pending`.
    function test_cannot_refund_a_settled_obligation() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(keccak256("settle-0"), idx);

        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_requires_quorum() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, _refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_duplicate_signer() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorum(_refundHash(r), pkC, pkC);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, signerC)
        );
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_unauthorized_signer() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorum(_refundHash(r), pkRogue, pkA);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_expired() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        r.expiry = uint64(block.timestamp - 1);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AuthorizationExpired.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_rejects_wrong_epoch() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(idx, alice, DEPOSIT_AMOUNT);
        r.signerEpoch = 7;
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidSignerEpoch.selector, uint64(0), uint64(7)
            )
        );
        bridge.executeRefund(r, sigs);
    }

    /// One request id, one refund — even across different obligations.
    function test_rejects_request_id_replay_across_obligations() public {
        uint256 first = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 second = _deposit(alice, DEPOSIT_AMOUNT);
        _refund(REQ, first);

        GlcRobinhoodBridge.RefundRequest memory r = _mkReq(second, alice, DEPOSIT_AMOUNT);
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executeRefund(r, sigs);
    }

    /// Refunding must work while paused — a pause that blocked it would trap
    /// depositors' principal during exactly the incident that generates refunds.
    function test_works_while_both_directions_paused() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _pauseBothRoutes();
        uint256 before = glc.balanceOf(alice);
        _refund(REQ, idx);
        assertEq(glc.balanceOf(alice), before + DEPOSIT_AMOUNT);
    }

    /// And after a migration is committed — that window is precisely when
    /// outstanding liability has to be cleared.
    function test_works_after_migration_committed() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));

        uint256 before = glc.balanceOf(alice);
        _refund(REQ, idx);
        assertEq(glc.balanceOf(alice), before + DEPOSIT_AMOUNT);
        assertEq(bridge.outstandingRefundableCount(), 0);
    }

    /// No fee is taken: the depositor gets exactly their principal back.
    function test_no_refund_fee() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 before = glc.balanceOf(alice);
        _refund(REQ, idx);
        assertEq(glc.balanceOf(alice) - before, DEPOSIT_AMOUNT);
    }

    /// A refund is not subject to the outbound rolling limit: it returns the
    /// depositor's own principal, it is not a new outflow of reserve.
    function test_refund_does_not_consume_outbound_window() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _refund(REQ, idx);
        assertEq(bridge.outboundWindow().total, 0);
    }

    /// Refund is bounded by the depositor's own principal, not by the reserve
    /// floor: a floor set above the balance must not trap their money.
    function test_refund_ignores_protected_floor() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);

        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = glc.balanceOf(address(bridge));
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(h));

        uint256 before = glc.balanceOf(alice);
        _refund(REQ, idx);
        assertEq(glc.balanceOf(alice), before + DEPOSIT_AMOUNT);
    }
}
