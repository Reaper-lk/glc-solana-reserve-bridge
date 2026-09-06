// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

/// Review finding M-3: closing an obligation whose refund is permanently
/// impossible, WITHOUT claiming a Goldcoin payout occurred.
contract AbandonmentTest is BridgeTestBase {
    event DepositAbandoned(
        uint256 indexed obligationIndex,
        address indexed depositor,
        bytes32 indexed requestId,
        uint256 amount
    );

    bytes32 internal constant REQ = keccak256("abandon-1");
    uint256 internal constant DEPOSIT_AMOUNT = 1000 * 1e18;

    function _mkReq(uint256 index)
        internal
        view
        returns (GlcRobinhoodBridge.AbandonmentRequest memory)
    {
        return GlcRobinhoodBridge.AbandonmentRequest({
            requestId: REQ,
            obligationIndex: index,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
    }

    // -----------------------------------------------------------------
    // Happy path and accounting
    // -----------------------------------------------------------------

    function test_unresolved_to_abandoned() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        assertTrue(bridge.isObligationUnresolved(idx));

        vm.expectEmit(true, true, true, true, address(bridge));
        emit DepositAbandoned(idx, alice, REQ, DEPOSIT_AMOUNT);
        _abandon(REQ, idx);

        assertTrue(bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Abandoned);
        assertFalse(bridge.isObligationUnresolved(idx));
    }

    /// Abandonment moves no tokens whatsoever.
    function test_moves_no_tokens() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 bridgeBal = glc.balanceOf(address(bridge));
        uint256 aliceBal = glc.balanceOf(alice);
        uint256 signerBal = glc.balanceOf(signerA);

        _abandon(REQ, idx);

        assertEq(glc.balanceOf(address(bridge)), bridgeBal);
        assertEq(glc.balanceOf(alice), aliceBal);
        assertEq(glc.balanceOf(signerA), signerBal);
    }

    function test_does_not_mutate_depositor_or_principal() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _abandon(REQ, idx);
        GlcRobinhoodBridge.Obligation memory ob = bridge.obligation(idx);
        assertEq(ob.depositor, alice);
        assertEq(ob.amount, DEPOSIT_AMOUNT);
    }

    /// Counters decrement exactly once, never twice.
    function test_counters_decrement_exactly_once() public {
        uint256 a = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 b = _deposit(bob, DEPOSIT_AMOUNT * 2);
        assertEq(bridge.outstandingRefundableCount(), 2);
        assertEq(bridge.outstandingRefundablePrincipal(), DEPOSIT_AMOUNT * 3);

        _abandon(REQ, a);
        assertEq(bridge.outstandingRefundableCount(), 1);
        assertEq(bridge.outstandingRefundablePrincipal(), DEPOSIT_AMOUNT * 2);

        _abandon(keccak256("abandon-2"), b);
        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
    }

    /// The retained principal stays in custody and becomes free reserve.
    function test_principal_remains_in_reserve() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 held = glc.balanceOf(address(bridge));
        _abandon(REQ, idx);
        assertEq(glc.balanceOf(address(bridge)), held);
        assertEq(bridge.encumberedReserve(), 0);
    }

    // -----------------------------------------------------------------
    // Terminal-state exclusivity
    // -----------------------------------------------------------------

    function test_refunded_cannot_be_abandoned() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _refund(keccak256("r"), idx);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeAbandonment(r, sigs);
    }

    function test_settled_cannot_be_abandoned() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(keccak256("s"), idx);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeAbandonment(r, sigs);
    }

    function test_abandoned_cannot_be_refunded() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _abandon(REQ, idx);

        GlcRobinhoodBridge.RefundRequest memory r = GlcRobinhoodBridge.RefundRequest({
            requestId: keccak256("r-after"),
            obligationIndex: idx,
            recipient: alice,
            amount: DEPOSIT_AMOUNT,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeRefund(r, sigs);
    }

    function test_abandoned_cannot_be_settled() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _abandon(REQ, idx);

        GlcRobinhoodBridge.SettlementRequest memory r = GlcRobinhoodBridge.SettlementRequest({
            requestId: keccak256("s-after"),
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeSettlement(r, sigs);
    }

    function test_cannot_abandon_twice() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _abandon(REQ, idx);

        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        r.requestId = keccak256("abandon-again");
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeAbandonment(r, sigs);
    }

    // -----------------------------------------------------------------
    // Authorization
    // -----------------------------------------------------------------

    function test_rejects_nonexistent_obligation() public {
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(0);
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.executeAbandonment(r, sigs);
    }

    function test_requires_quorum() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, _abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executeAbandonment(r, sigs);
    }

    function test_rejects_duplicate_signer() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorum(_abandonmentHash(r), pkB, pkB);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, signerB)
        );
        bridge.executeAbandonment(r, sigs);
    }

    function test_rejects_unauthorized_signer() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorum(_abandonmentHash(r), pkRogue, pkA);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.executeAbandonment(r, sigs);
    }

    function test_rejects_expired() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        r.expiry = uint64(block.timestamp - 1);
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AuthorizationExpired.selector);
        bridge.executeAbandonment(r, sigs);
    }

    function test_rejects_wrong_epoch() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        r.signerEpoch = 5;
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidSignerEpoch.selector, uint64(0), uint64(5)
            )
        );
        bridge.executeAbandonment(r, sigs);
    }

    function test_rejects_request_id_replay() public {
        uint256 a = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 b = _deposit(alice, DEPOSIT_AMOUNT);
        _abandon(REQ, a);

        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(b);
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executeAbandonment(r, sigs);
    }

    function test_rejects_after_migration_finalized() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(keccak256("s"), idx);
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        _finalizeMigration();

        GlcRobinhoodBridge.AbandonmentRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorumAB(_abandonmentHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.executeAbandonment(r, sigs);
    }

    // -----------------------------------------------------------------
    // Cross-action separation: abandon must never impersonate settlement
    // -----------------------------------------------------------------

    /// A settlement authorization must not be usable to abandon. The two have
    /// identical field lists, so ONLY the distinct type name and action byte
    /// separate them -- exactly the confusion this test exists to forbid.
    function test_settlement_signature_cannot_abandon() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        bytes32 id = keccak256("shared");

        GlcRobinhoodBridge.SettlementRequest memory sr = GlcRobinhoodBridge.SettlementRequest({
            requestId: id,
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory settleSigs = _quorumAB(_settlementHash(sr));

        GlcRobinhoodBridge.AbandonmentRequest memory ar = GlcRobinhoodBridge.AbandonmentRequest({
            requestId: id,
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        vm.expectRevert();
        bridge.executeAbandonment(ar, settleSigs);
    }

    /// And an abandonment authorization must not be usable to settle -- so a
    /// quorum cannot be tricked into fabricating a Goldcoin payout record.
    function test_abandonment_signature_cannot_settle() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        bytes32 id = keccak256("shared-2");

        GlcRobinhoodBridge.AbandonmentRequest memory ar = GlcRobinhoodBridge.AbandonmentRequest({
            requestId: id,
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory abandonSigs = _quorumAB(_abandonmentHash(ar));

        GlcRobinhoodBridge.SettlementRequest memory sr = GlcRobinhoodBridge.SettlementRequest({
            requestId: id,
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        vm.expectRevert();
        bridge.executeSettlement(sr, abandonSigs);
    }

    function test_refund_signature_cannot_abandon() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        bytes32 id = keccak256("shared-3");

        GlcRobinhoodBridge.RefundRequest memory rr = GlcRobinhoodBridge.RefundRequest({
            requestId: id,
            obligationIndex: idx,
            recipient: alice,
            amount: DEPOSIT_AMOUNT,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory refundSigs = _quorumAB(_refundHash(rr));

        GlcRobinhoodBridge.AbandonmentRequest memory ar = _mkReq(idx);
        ar.requestId = id;
        vm.expectRevert();
        bridge.executeAbandonment(ar, refundSigs);
    }

    /// The abandonment typehash is genuinely distinct from settlement's, not
    /// merely separated by the action byte.
    function test_typehashes_are_distinct() public view {
        assertTrue(bridge.ABANDONMENT_TYPEHASH() != bridge.SETTLEMENT_TYPEHASH());
        assertTrue(bridge.ACTION_ABANDON() != bridge.ACTION_SETTLE());
    }

    /// The same request id may be used once under each distinct action.
    function test_same_request_id_usable_once_per_action() public {
        uint256 a = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 b = _deposit(alice, DEPOSIT_AMOUNT);
        bytes32 id = keccak256("per-action");

        _abandon(id, a);
        assertTrue(bridge.requestExecuted(bridge.ACTION_ABANDON(), id));
        assertFalse(bridge.requestExecuted(bridge.ACTION_SETTLE(), id));
        _settle(id, b);
        assertTrue(bridge.requestExecuted(bridge.ACTION_SETTLE(), id));
    }

    // -----------------------------------------------------------------
    // Migration interaction
    // -----------------------------------------------------------------

    /// A mixture of all three terminal states clears the liability gate, and
    /// abandoned principal migrates across as part of the full reserve.
    function test_migration_finalizes_with_mixed_terminal_states() public {
        uint256 a = _deposit(alice, 1000 * ONE_GLC);
        uint256 b = _deposit(bob, 2000 * ONE_GLC);
        uint256 c = _deposit(alice, 3000 * ONE_GLC);

        _settle(keccak256("s-a"), a);
        _refund(keccak256("r-b"), b);
        _abandon(keccak256("x-c"), c);

        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
        assertTrue(bridge.obligationStatus(a) == GlcRobinhoodBridge.ObligationStatus.Settled);
        assertTrue(bridge.obligationStatus(b) == GlcRobinhoodBridge.ObligationStatus.Refunded);
        assertTrue(bridge.obligationStatus(c) == GlcRobinhoodBridge.ObligationStatus.Abandoned);

        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());

        uint256 balance = glc.balanceOf(address(bridge));
        _finalizeMigration();

        // The abandoned principal is part of the reserve that moved.
        assertEq(glc.balanceOf(successor), balance);
        assertEq(glc.balanceOf(address(bridge)), 0);
        assertGe(balance, 3000 * ONE_GLC);
    }

    /// Abandoning is what unblocks migration when refund is impossible.
    function test_abandonment_unblocks_migration() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());

        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_FINALIZE_MIGRATION(),
            keccak256(abi.encode(bridge.migrationSuccessor())),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.OutstandingRefundsRemain.selector, uint256(1), DEPOSIT_AMOUNT
            )
        );
        bridge.finalizeMigration(nonce, FAR_FUTURE, sigs);

        _abandon(REQ, idx);
        _finalizeMigration();
        assertTrue(bridge.migrated());
    }
}
