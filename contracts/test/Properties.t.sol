// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

/// Property/fuzz tests for the invariants that must hold for ANY input, not
/// just the hand-picked ones the unit tests use.
contract PropertiesTest is BridgeTestBase {
    /// PROPERTY: an amount that is not an exact multiple of 1e10 can never
    /// create an obligation, whatever its magnitude.
    function testFuzz_non_canonical_amount_never_creates_obligation(uint256 raw) public {
        uint256 amount = bound(raw, INBOUND_MIN, INBOUND_MAX);
        uint256 remainder = bound(uint256(keccak256(abi.encode(raw))), 1, SCALE - 1);
        uint256 dirty = (amount / SCALE) * SCALE + remainder;
        vm.assume(dirty >= INBOUND_MIN && dirty <= INBOUND_MAX);

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.NonCanonicalAmount.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, dirty, _destination());

        assertEq(bridge.obligationCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
    }

    /// PROPERTY: any accepted deposit is an exact multiple of the scale, and the
    /// recorded principal equals what actually moved.
    function testFuzz_accepted_deposit_is_exact(uint256 raw) public {
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, INBOUND_MAX / SCALE) * SCALE;

        uint256 bridgeBefore = glc.balanceOf(address(bridge));
        uint256 idx = _deposit(alice, amount);

        assertEq(amount % SCALE, 0);
        assertEq(bridge.obligation(idx).amount, amount);
        assertEq(glc.balanceOf(address(bridge)) - bridgeBefore, amount);
        assertEq(bridge.outstandingRefundablePrincipal(), amount);
    }

    /// PROPERTY: a payout transfers exactly the authorized amount — never more.
    function testFuzz_payout_never_exceeds_authorized_amount(uint256 raw, bytes32 id) public {
        uint256 amount = bound(raw, OUTBOUND_MIN / SCALE, OUTBOUND_MAX / SCALE) * SCALE;

        uint256 recipientBefore = glc.balanceOf(bob);
        uint256 bridgeBefore = glc.balanceOf(address(bridge));

        _payout(id, bob, amount);

        assertEq(glc.balanceOf(bob) - recipientBefore, amount);
        assertEq(bridgeBefore - glc.balanceOf(address(bridge)), amount);
    }

    /// PROPERTY: a payout request id executes at most once, for any id.
    function testFuzz_payout_request_never_executes_twice(bytes32 id) public {
        uint256 amount = OUTBOUND_MIN;
        _payout(id, bob, amount);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: id,
            recipient: bob,
            amount: amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executePayout(r, sigs);
    }

    /// PROPERTY: an obligation can never be refunded twice, for any deposit.
    function testFuzz_obligation_never_refunds_twice(uint256 raw, bytes32 id1, bytes32 id2) public {
        vm.assume(id1 != id2);
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, INBOUND_MAX / SCALE) * SCALE;

        uint256 idx = _deposit(alice, amount);
        uint256 aliceBefore = glc.balanceOf(alice);
        _refund(id1, idx);
        assertEq(glc.balanceOf(alice), aliceBefore + amount);

        GlcRobinhoodBridge.RefundRequest memory r = GlcRobinhoodBridge.RefundRequest({
            requestId: id2,
            obligationIndex: idx,
            recipient: alice,
            amount: amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_refundHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeRefund(r, sigs);

        // The depositor received their principal exactly once.
        assertEq(glc.balanceOf(alice), aliceBefore + amount);
    }

    /// PROPERTY: an obligation can never be both settled and refunded.
    function testFuzz_settle_and_refund_are_mutually_exclusive(uint256 raw, bool settleFirst)
        public
    {
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, INBOUND_MAX / SCALE) * SCALE;
        uint256 idx = _deposit(alice, amount);

        if (settleFirst) {
            _settle(keccak256("s"), idx);
            GlcRobinhoodBridge.RefundRequest memory r = GlcRobinhoodBridge.RefundRequest({
                requestId: keccak256("r"),
                obligationIndex: idx,
                recipient: alice,
                amount: amount,
                signerEpoch: bridge.signerEpoch(),
                expiry: FAR_FUTURE
            });
            bytes[] memory sigs = _quorumAB(_refundHash(r));
            vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
            bridge.executeRefund(r, sigs);
            assertTrue(bridge.obligation(idx).status == GlcRobinhoodBridge.ObligationStatus.Settled);
        } else {
            _refund(keccak256("r"), idx);
            GlcRobinhoodBridge.SettlementRequest memory sr = GlcRobinhoodBridge.SettlementRequest({
                requestId: keccak256("s"),
                obligationIndex: idx,
                signerEpoch: bridge.signerEpoch(),
                expiry: FAR_FUTURE
            });
            bytes[] memory sigs = _quorumAB(_settlementHash(sr));
            vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
            bridge.executeSettlement(sr, sigs);
            assertTrue(
                bridge.obligation(idx).status == GlcRobinhoodBridge.ObligationStatus.Refunded
            );
        }
        assertEq(bridge.outstandingRefundableCount(), 0);
    }

    /// PROPERTY: exactly one terminal state is ever reached, for any deposit and
    /// any order of attempted exits. Settle, refund and abandon are pairwise
    /// exclusive, and the liability is released exactly once.
    function testFuzz_exactly_one_terminal_state(uint256 raw, uint8 pick) public {
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, INBOUND_MAX / SCALE) * SCALE;
        uint256 which = bound(pick, 0, 2);
        uint256 idx = _deposit(alice, amount);
        assertEq(bridge.outstandingRefundableCount(), 1);

        if (which == 0) {
            _settle(keccak256("t"), idx);
            assertTrue(bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Settled);
        } else if (which == 1) {
            _refund(keccak256("t"), idx);
            assertTrue(bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Refunded);
        } else {
            _abandon(keccak256("t"), idx);
            assertTrue(
                bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Abandoned
            );
        }

        // Liability released exactly once...
        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);

        // ...and every other exit is now closed.
        GlcRobinhoodBridge.AbandonmentRequest memory ar = GlcRobinhoodBridge.AbandonmentRequest({
            requestId: keccak256("x2"),
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory asigs = _quorumAB(_abandonmentHash(ar));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeAbandonment(ar, asigs);

        GlcRobinhoodBridge.SettlementRequest memory sr = GlcRobinhoodBridge.SettlementRequest({
            requestId: keccak256("s2"),
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory ssigs = _quorumAB(_settlementHash(sr));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeSettlement(sr, ssigs);
    }

    /// PROPERTY: abandoning never changes any token balance, for any amount.
    function testFuzz_abandon_never_moves_tokens(uint256 raw) public {
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, INBOUND_MAX / SCALE) * SCALE;
        uint256 idx = _deposit(alice, amount);

        uint256 bridgeBal = glc.balanceOf(address(bridge));
        uint256 aliceBal = glc.balanceOf(alice);

        _abandon(keccak256("fz"), idx);

        assertEq(glc.balanceOf(address(bridge)), bridgeBal);
        assertEq(glc.balanceOf(alice), aliceBal);
    }

    /// PROPERTY: a guardian veto never moves tokens and never changes config,
    /// whichever guardian acts and whenever they act.
    function testFuzz_veto_is_inert_except_for_migration(uint256 who, uint256 delay) public {
        uint256 pick = bound(who, 0, 2);
        address guardian = pick == 0 ? guardian1 : (pick == 1 ? guardian2 : guardian3);
        uint256 wait = bound(delay, 0, 400 days);

        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);

        uint256 bridgeBal = glc.balanceOf(address(bridge));
        uint64 epoch = bridge.signerEpoch();
        uint256 nonce = bridge.governanceNonce();

        vm.warp(block.timestamp + wait);
        vm.prank(guardian);
        bridge.vetoMigration();

        assertFalse(bridge.migrationCommitted());
        assertEq(bridge.migrationSuccessor(), address(0));
        assertEq(glc.balanceOf(address(bridge)), bridgeBal);
        assertEq(glc.balanceOf(successor), 0);
        assertEq(bridge.signerEpoch(), epoch);
        assertEq(bridge.governanceNonce(), nonce);
        assertTrue(bridge.depositsPaused());
        assertTrue(bridge.payoutsPaused());
    }

    /// PROPERTY: a single signature is never sufficient, for any signer and any
    /// payout. The quorum cannot collapse below two.
    function testFuzz_single_signature_never_sufficient(uint256 which, bytes32 id) public {
        uint256 pick = bound(which, 0, 2);
        uint256 pk = pick == 0 ? pkA : (pick == 1 ? pkB : pkC);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: id,
            recipient: bob,
            amount: OUTBOUND_MIN,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pk, _payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executePayout(r, sigs);
    }

    /// PROPERTY: the same signer twice is never a quorum, for any signer.
    function testFuzz_duplicate_signer_never_a_quorum(uint256 which, bytes32 id) public {
        uint256 pick = bound(which, 0, 2);
        uint256 pk = pick == 0 ? pkA : (pick == 1 ? pkB : pkC);
        address who = pick == 0 ? signerA : (pick == 1 ? signerB : signerC);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: id,
            recipient: bob,
            amount: OUTBOUND_MIN,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorum(_payoutHash(r), pk, pk);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, who)
        );
        bridge.executePayout(r, sigs);
    }

    /// PROPERTY: the signer set always holds exactly three distinct, non-zero
    /// members after any accepted rotation, and any other shape is rejected.
    function testFuzz_signer_set_always_three_distinct(address a, address b, address c) public {
        address[3] memory set = [a, b, c];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);

        bool hasZero = a == address(0) || b == address(0) || c == address(0);
        bool hasDup = a == b || a == c || b == c;

        if (hasZero || hasDup) {
            vm.expectRevert();
            bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
            // The outgoing set is untouched by a rejected rotation.
            assertTrue(bridge.isSigner(signerA));
            assertEq(bridge.signerEpoch(), 0);
        } else {
            bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
            assertTrue(bridge.isSigner(a));
            assertTrue(bridge.isSigner(b));
            assertTrue(bridge.isSigner(c));
            assertEq(bridge.signerEpoch(), 1);
        }
    }

    /// PROPERTY: the governance nonce is strictly monotonic and never reusable.
    function testFuzz_governance_nonce_monotonic(uint8 rounds) public {
        uint256 n = bound(rounds, 1, 12);
        uint256 previous = bridge.governanceNonce();

        for (uint256 i = 0; i < n; ++i) {
            bool pauseState = i % 2 == 0;
            uint256 nonce = bridge.governanceNonce();
            assertEq(nonce, previous);

            bytes32 payload = keccak256(abi.encode(pauseState, pauseState));
            bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, nonce, FAR_FUTURE);
            bridge.setPaused(pauseState, pauseState, nonce, FAR_FUTURE, _quorumAB(h));

            assertEq(bridge.governanceNonce(), nonce + 1);
            previous = nonce + 1;
        }
    }

    /// PROPERTY: a stale governance nonce is never accepted.
    function testFuzz_stale_governance_nonce_rejected(uint256 rawNonce) public {
        _setPaused(true, true);
        uint256 current = bridge.governanceNonce();
        uint256 wrong = bound(rawNonce, 0, type(uint128).max);
        vm.assume(wrong != current);

        bytes32 payload = keccak256(abi.encode(false, false));
        bytes32 h = _governanceHash(bridge.ACTION_SET_PAUSE(), payload, wrong, FAR_FUTURE);
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, current, wrong
            )
        );
        bridge.setPaused(false, false, wrong, FAR_FUTURE, sigs);
    }

    /// PROPERTY: migration transfers the full reserve — never less, never more —
    /// for any balance the contract happens to hold.
    function testFuzz_migration_moves_exactly_full_reserve(uint256 extra) public {
        uint256 topUp = bound(extra, 0, 10_000_000 * ONE_GLC);
        glc.mint(address(bridge), topUp);

        _pauseBothRoutes();
        address successor = address(_deployConformingSuccessor());
        _commitMigration(successor);
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());

        uint256 balance = glc.balanceOf(address(bridge));
        _finalizeMigration();

        assertEq(glc.balanceOf(address(bridge)), 0);
        assertEq(glc.balanceOf(successor), balance);
    }

    /// PROPERTY: while any obligation is unsettled, migration cannot finalize —
    /// so no depositor's principal is ever stranded by a terminal migration.
    function testFuzz_outstanding_liability_blocks_finalization(uint256 raw, uint8 count) public {
        uint256 n = bound(count, 1, 5);
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, (INBOUND_MAX / SCALE) / 5) * SCALE;

        for (uint256 i = 0; i < n; ++i) {
            uint256 idx = _deposit(alice, amount);
            assertEq(idx, i);
        }
        assertEq(bridge.outstandingRefundableCount(), n);

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
                GlcRobinhoodBridge.OutstandingRefundsRemain.selector, n, amount * n
            )
        );
        bridge.finalizeMigration(nonce, FAR_FUTURE, sigs);

        // Clearing every obligation unblocks it, so the gate is never a trap.
        for (uint256 i = 0; i < n; ++i) {
            _refund(keccak256(abi.encode("clear", i)), i);
        }
        _finalizeMigration();
        assertTrue(bridge.migrated());
    }

    /// PROPERTY: the encumbered reserve is always fully backed — the contract
    /// never holds less than it owes its unsettled depositors.
    function testFuzz_reserve_always_covers_outstanding_principal(uint256 raw, uint8 count) public {
        uint256 n = bound(count, 1, 5);
        uint256 amount = bound(raw, INBOUND_MIN / SCALE, (INBOUND_MAX / SCALE) / 5) * SCALE;

        for (uint256 i = 0; i < n; ++i) {
            uint256 idx = _deposit(alice, amount);
            assertEq(idx, i);
        }

        assertGe(glc.balanceOf(address(bridge)), bridge.outstandingRefundablePrincipal());

        // Draining the free reserve down to the floor must not break the cover.
        _payout(keccak256("drain"), bob, OUTBOUND_MAX);
        assertGe(glc.balanceOf(address(bridge)), bridge.outstandingRefundablePrincipal());
    }
}
