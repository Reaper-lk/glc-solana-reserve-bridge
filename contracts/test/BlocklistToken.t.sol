// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {BlocklistGlc} from "./mocks/BlocklistGlc.sol";
import {MockSuccessor} from "./mocks/MockSuccessor.sol";

/// Review finding M-3, now RESOLVED: what happens when the reserve token can
/// refuse a transfer to a specific depositor. Before `Abandoned` existed the
/// only exit was to record the obligation as `Settled`, asserting a Goldcoin
/// payout that never happened. These tests pin the honest exit instead.
contract BlocklistTokenTest is BridgeTestBase {
    BlocklistGlc internal btoken;
    GlcRobinhoodBridge internal bbridge;

    function setUp() public override {
        super.setUp();

        btoken = new BlocklistGlc();
        bbridge = new GlcRobinhoodBridge(
            btoken,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits()
        );
        _bootstrap(bbridge);

        btoken.mint(alice, RESERVE_SEED);
        btoken.mint(address(bbridge), RESERVE_SEED);
        vm.prank(alice);
        assertTrue(btoken.approve(address(bbridge), type(uint256).max));
    }

    function _rawSign(uint256 pk, bytes32 digest) internal pure returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }

    function _sigsFor(GlcRobinhoodBridge b, bytes32 structHash)
        internal
        view
        returns (bytes[] memory sigs)
    {
        bytes32 digest = MessageHashUtils.toTypedDataHash(b.domainSeparator(), structHash);
        sigs = new bytes[](2);
        sigs[0] = _rawSign(pkA, digest);
        sigs[1] = _rawSign(pkB, digest);
    }

    function _pause(GlcRobinhoodBridge b) internal {
        _setPausedOn(b, true, true);
    }

    /// A blocklisted depositor cannot be refunded: the token itself refuses.
    /// The obligation stays `Pending` and the liability stays outstanding.
    function test_blocklisted_depositor_cannot_be_refunded() public {
        vm.prank(alice);
        uint256 idx = bbridge.deposit(ROUTE_RHN_TO_GLC, 1000 * ONE_GLC, _destination());

        btoken.setBlocked(alice, true);

        GlcRobinhoodBridge.RefundRequest memory r = GlcRobinhoodBridge.RefundRequest({
            requestId: keccak256("blocked-refund"),
            obligationIndex: idx,
            recipient: alice,
            amount: 1000 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes32 h = _refundHashOn(bbridge, address(btoken), r);
        bytes[] memory sigs = _sigsFor(bbridge, h);
        vm.expectRevert(abi.encodeWithSelector(BlocklistGlc.Blocked.selector, alice));
        bbridge.executeRefund(r, sigs);

        assertEq(bbridge.outstandingRefundableCount(), 1);
        assertTrue(bbridge.obligation(idx).status == GlcRobinhoodBridge.ObligationStatus.Pending);
    }

    /// And therefore migration is blocked -- correctly refusing to strand the
    /// principal, but with no honest way to close the obligation. Settlement is
    /// the ONLY remaining exit, and it asserts a Goldcoin payout occurred.
    /// This is review finding M-3 in executable form.
    function test_blocklisted_refund_is_closed_honestly_as_abandoned() public {
        vm.prank(alice);
        uint256 idx = bbridge.deposit(ROUTE_RHN_TO_GLC, 1000 * ONE_GLC, _destination());
        btoken.setBlocked(alice, true);

        _pause(bbridge);
        address successor =
            address(new MockSuccessor(address(btoken), bbridge.BRIDGE_PROTOCOL_ID()));
        uint256 nonce = bbridge.governanceNonce();
        bytes32 ch = keccak256(
            abi.encode(
                bbridge.GOVERNANCE_TYPEHASH(),
                bbridge.ACTION_COMMIT_MIGRATION(),
                keccak256(abi.encode(successor)),
                bbridge.signerEpoch(),
                nonce,
                FAR_FUTURE
            )
        );
        bbridge.commitMigration(successor, nonce, FAR_FUTURE, _sigsFor(bbridge, ch));
        vm.warp(block.timestamp + bbridge.MIGRATION_DELAY());

        // Migration correctly refuses while the liability stands.
        uint256 fnonce = bbridge.governanceNonce();
        bytes32 fh = keccak256(
            abi.encode(
                bbridge.GOVERNANCE_TYPEHASH(),
                bbridge.ACTION_FINALIZE_MIGRATION(),
                keccak256(abi.encode(successor)),
                bbridge.signerEpoch(),
                fnonce,
                FAR_FUTURE
            )
        );
        bytes[] memory fsigs = _sigsFor(bbridge, fh);
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.OutstandingRefundsRemain.selector, uint256(1), 1000 * ONE_GLC
            )
        );
        bbridge.finalizeMigration(fnonce, FAR_FUTURE, fsigs);

        // The honest exit: close it as ABANDONED. No tokens move, and the
        // record does not claim a Goldcoin payout that never happened.
        uint256 heldBefore = btoken.balanceOf(address(bbridge));
        GlcRobinhoodBridge.AbandonmentRequest memory ar = GlcRobinhoodBridge.AbandonmentRequest({
            requestId: keccak256("honest-abandon"),
            obligationIndex: idx,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes32 ah = _abandonmentHashOn(bbridge, ar);
        bbridge.executeAbandonment(ar, _sigsFor(bbridge, ah));

        assertEq(bbridge.outstandingRefundableCount(), 0);
        assertEq(btoken.balanceOf(address(bbridge)), heldBefore, "abandon moves no tokens");
        assertTrue(bbridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Abandoned);
        assertTrue(
            bbridge.obligationStatus(idx) != GlcRobinhoodBridge.ObligationStatus.Settled,
            "must NOT be recorded as a Goldcoin settlement"
        );

        // With liability cleared, migration proceeds.
        uint256 f2 = bbridge.governanceNonce();
        bytes32 fh2 = keccak256(
            abi.encode(
                bbridge.GOVERNANCE_TYPEHASH(),
                bbridge.ACTION_FINALIZE_MIGRATION(),
                keccak256(abi.encode(successor)),
                bbridge.signerEpoch(),
                f2,
                FAR_FUTURE
            )
        );
        bbridge.finalizeMigration(f2, FAR_FUTURE, _sigsFor(bbridge, fh2));
        assertTrue(bbridge.migrated());
    }

    /// A blocklisted PAYOUT recipient simply reverts; no state is consumed,
    /// so the request id remains usable once the block is lifted.
    function test_blocklisted_payout_recipient_reverts_without_consuming_request() public {
        btoken.setBlocked(bob, true);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("blocked-payout"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes32 h = _payoutHashOn(bbridge, address(btoken), r);
        bytes[] memory sigs = _sigsFor(bbridge, h);
        vm.expectRevert(abi.encodeWithSelector(BlocklistGlc.Blocked.selector, bob));
        bbridge.executePayout(r, sigs);

        assertFalse(bbridge.requestExecuted(bbridge.ACTION_PAYOUT(), r.requestId));

        btoken.setBlocked(bob, false);
        bbridge.executePayout(r, sigs);
        assertTrue(bbridge.requestExecuted(bbridge.ACTION_PAYOUT(), r.requestId));
    }
}
