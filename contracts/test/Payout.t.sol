// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

contract PayoutTest is BridgeTestBase {
    event PayoutExecuted(
        bytes32 indexed requestId,
        address indexed recipient,
        uint8 indexed route,
        uint256 amount,
        uint64 signerEpoch
    );

    bytes32 internal constant REQ = keccak256("payout-1");

    function _req(address recipient, uint256 amount)
        internal
        view
        returns (GlcRobinhoodBridge.PayoutRequest memory)
    {
        return GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: REQ,
            recipient: recipient,
            amount: amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
    }

    function test_valid_two_of_three() public {
        uint256 amount = 500 * ONE_GLC;
        uint256 before = glc.balanceOf(bob);

        vm.expectEmit(true, true, true, true, address(bridge));
        emit PayoutExecuted(REQ, bob, ROUTE_GLC_TO_RHN, amount, 0);
        _payout(REQ, bob, amount);

        assertEq(glc.balanceOf(bob), before + amount);
        assertTrue(bridge.requestExecuted(bridge.ACTION_PAYOUT(), REQ));
    }

    /// Any two distinct signers work; order is irrelevant and no sorting is
    /// assumed by the contract.
    function test_any_distinct_pair_in_any_order() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bridge.executePayout(r, _quorum(h, pkC, pkA));

        r.requestId = keccak256("payout-2");
        h = _payoutHash(r);
        bridge.executePayout(r, _quorum(h, pkB, pkC));

        r.requestId = keccak256("payout-3");
        h = _payoutHash(r);
        bridge.executePayout(r, _quorum(h, pkB, pkA));
    }

    function test_rejects_single_signature() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, _payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_three_signatures() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = new bytes[](3);
        sigs[0] = _sign(pkA, h);
        sigs[1] = _sign(pkB, h);
        sigs[2] = _sign(pkC, h);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_empty_signatures() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executePayout(r, new bytes[](0));
    }

    /// The same signer twice is not a quorum, however well-formed each
    /// signature is on its own.
    function test_rejects_duplicate_signer() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorum(h, pkA, pkA);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, signerA)
        );
        bridge.executePayout(r, sigs);
    }

    function test_rejects_unauthorized_signer() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorum(h, pkRogue, pkA);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.executePayout(r, sigs);
    }

    function test_rejects_one_valid_one_rogue() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorum(h, pkA, pkRogue);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.executePayout(r, sigs);
    }

    function test_rejects_expired() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        r.expiry = uint64(block.timestamp - 1);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AuthorizationExpired.selector);
        bridge.executePayout(r, sigs);
    }

    function test_accepts_at_exact_expiry() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        r.expiry = uint64(block.timestamp);
        bridge.executePayout(r, _quorumAB(_payoutHash(r)));
    }

    function test_rejects_wrong_signer_epoch() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        r.signerEpoch = 1;
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidSignerEpoch.selector, uint64(0), uint64(1)
            )
        );
        bridge.executePayout(r, sigs);
    }

    // -----------------------------------------------------------------
    // Domain binding
    // -----------------------------------------------------------------

    bytes32 internal constant DOMAIN_TYPEHASH = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
    );

    function _domainFor(uint256 chainId, address verifying) internal pure returns (bytes32) {
        return keccak256(
            abi.encode(
                DOMAIN_TYPEHASH,
                keccak256(bytes("GlcRobinhoodBridge")),
                keccak256(bytes("1")),
                chainId,
                verifying
            )
        );
    }

    function _signUnderDomain(uint256 pk, bytes32 domain, bytes32 structHash)
        internal
        pure
        returns (bytes memory)
    {
        bytes32 digest = keccak256(abi.encodePacked(hex"1901", domain, structHash));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }

    /// A signature minted for a different chain id must not verify here.
    function test_rejects_wrong_chain_domain() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes32 foreign = _domainFor(block.chainid + 1, address(bridge));
        bytes[] memory foreignSigs = new bytes[](2);
        foreignSigs[0] = _signUnderDomain(pkA, foreign, h);
        foreignSigs[1] = _signUnderDomain(pkB, foreign, h);
        vm.expectRevert();
        bridge.executePayout(r, foreignSigs);
    }

    /// A signature minted for a different verifying contract must not verify.
    function test_rejects_wrong_verifying_contract() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes32 foreign = _domainFor(block.chainid, address(0xDEAD));
        bytes[] memory foreignSigs = new bytes[](2);
        foreignSigs[0] = _signUnderDomain(pkA, foreign, h);
        foreignSigs[1] = _signUnderDomain(pkB, foreign, h);
        vm.expectRevert();
        bridge.executePayout(r, foreignSigs);
    }

    // -----------------------------------------------------------------
    // Tampering
    // -----------------------------------------------------------------

    function test_rejects_tampered_recipient() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorumAB(h);
        r.recipient = rogue;
        vm.expectRevert();
        bridge.executePayout(r, sigs);
    }

    function test_rejects_tampered_amount() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorumAB(h);
        r.amount = 5000 * ONE_GLC;
        vm.expectRevert();
        bridge.executePayout(r, sigs);
    }

    function test_rejects_tampered_request_id() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes32 h = _payoutHash(r);
        bytes[] memory sigs = _quorumAB(h);
        r.requestId = keccak256("other");
        vm.expectRevert();
        bridge.executePayout(r, sigs);
    }

    // -----------------------------------------------------------------
    // Replay / state
    // -----------------------------------------------------------------

    function test_rejects_replay() public {
        _payout(REQ, bob, 500 * ONE_GLC);
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_when_paused() public {
        _setPaused(false, true);
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.PayoutsPaused.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_zero_recipient() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(address(0), 500 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_non_canonical_amount() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 500 * ONE_GLC + 1);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.NonCanonicalAmount.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_below_minimum() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, OUTBOUND_MIN - SCALE);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AmountBelowMinimum.selector);
        bridge.executePayout(r, sigs);
    }

    function test_rejects_above_maximum() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, OUTBOUND_MAX + SCALE);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AmountAboveMaximum.selector);
        bridge.executePayout(r, sigs);
    }

    // -----------------------------------------------------------------
    // Reserve floor and rolling limit
    // -----------------------------------------------------------------

    function _setProtectedFloor(uint256 floor) internal {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.protectedMinReserve = floor;
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_LIMITS(), keccak256(abi.encode(lim)), nonce, FAR_FUTURE
        );
        bridge.setLimits(lim, nonce, FAR_FUTURE, _quorumAB(h));
    }

    function test_respects_protected_reserve_floor() public {
        uint256 balance = glc.balanceOf(address(bridge));
        _setProtectedFloor(balance - 100 * ONE_GLC);

        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 200 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executePayout(r, sigs);

        // Exactly down to the floor is permitted.
        r = _req(bob, 100 * ONE_GLC);
        bridge.executePayout(r, _quorumAB(_payoutHash(r)));
        assertEq(glc.balanceOf(address(bridge)), balance - 100 * ONE_GLC);
    }

    /// A payout may never be funded out of a depositor's unsettled principal.
    function test_cannot_pay_out_of_outstanding_refundable_principal() public {
        // Drain the free reserve so only depositor principal would remain.
        uint256 balance = glc.balanceOf(address(bridge));
        _setProtectedFloor(balance);

        uint256 idx = _deposit(alice, 1000 * ONE_GLC);
        assertEq(idx, 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 1000 * ONE_GLC);

        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, 1000 * ONE_GLC);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InsufficientReserve.selector);
        bridge.executePayout(r, sigs);

        // Settling releases that principal, and only then is it payable.
        _settle(keccak256("settle-0"), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
        bridge.executePayout(r, _quorumAB(_payoutHash(r)));
    }

    function test_rolling_limit_enforced() public {
        uint256 chunk = OUTBOUND_MAX;
        uint256 fullChunks = OUTBOUND_ROLLING / chunk;
        for (uint256 i = 0; i < fullChunks; ++i) {
            _payout(keccak256(abi.encode("p", i)), bob, chunk);
        }
        GlcRobinhoodBridge.PayoutRequest memory r = _req(bob, OUTBOUND_MIN);
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.executePayout(r, sigs);
    }

    function test_outbound_does_not_consume_inbound_window() public {
        _payout(REQ, bob, OUTBOUND_MAX);
        assertEq(bridge.outboundWindow().total, OUTBOUND_MAX);
        assertEq(bridge.inboundWindow().total, 0);
    }
}
