// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

/// ECDSA hygiene and cross-action domain separation.
contract AuthorizationTest is BridgeTestBase {
    uint256 internal constant SECP256K1_N =
        0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
    uint8 internal constant V27 = 27;
    uint8 internal constant V28 = 28;

    function _payoutReq(bytes32 id)
        internal
        view
        returns (GlcRobinhoodBridge.PayoutRequest memory)
    {
        return GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: id,
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
    }

    function test_rejects_malformed_signature_length() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _payoutReq(keccak256("m1"));
        bytes[] memory sigs = new bytes[](2);
        sigs[0] = hex"1234";
        sigs[1] = _sign(pkB, _payoutHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureLength.selector, uint256(2))
        );
        bridge.executePayout(r, sigs);
    }

    function test_rejects_empty_signature_bytes() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _payoutReq(keccak256("m2"));
        bytes[] memory sigs = new bytes[](2);
        sigs[0] = "";
        sigs[1] = _sign(pkB, _payoutHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureLength.selector, uint256(0))
        );
        bridge.executePayout(r, sigs);
    }

    function test_rejects_garbage_signature() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _payoutReq(keccak256("m3"));
        bytes[] memory sigs = new bytes[](2);
        sigs[0] = _sign(pkA, _payoutHash(r));
        bytes memory garbage = new bytes(65);
        sigs[1] = garbage;
        vm.expectRevert();
        bridge.executePayout(r, sigs);
    }

    /// Signature malleability: the high-s counterpart of a valid signature must
    /// be rejected, not silently accepted as a second distinct signature.
    function test_rejects_high_s_signature() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _payoutReq(keccak256("m4"));
        bytes32 structHash = _payoutHash(r);
        bytes32 digest =
            keccak256(abi.encodePacked(hex"1901", bridge.domainSeparator(), structHash));
        (uint8 v, bytes32 rr, bytes32 s) = vm.sign(pkA, digest);

        bytes32 highS = bytes32(SECP256K1_N - uint256(s));
        uint8 flipped = v == V27 ? V28 : V27;

        bytes[] memory sigs = new bytes[](2);
        sigs[0] = abi.encodePacked(rr, highS, flipped);
        sigs[1] = _sign(pkB, structHash);
        vm.expectRevert(abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureS.selector, highS));
        bridge.executePayout(r, sigs);
    }

    /// The malleable form cannot be used to fake a second distinct signer
    /// either: it is rejected before distinctness is ever considered.
    function test_high_s_cannot_forge_second_signer() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _payoutReq(keccak256("m5"));
        bytes32 structHash = _payoutHash(r);
        bytes32 digest =
            keccak256(abi.encodePacked(hex"1901", bridge.domainSeparator(), structHash));
        (uint8 v, bytes32 rr, bytes32 s) = vm.sign(pkA, digest);
        bytes32 highS = bytes32(SECP256K1_N - uint256(s));
        uint8 flipped = v == V27 ? V28 : V27;

        bytes[] memory sigs = new bytes[](2);
        sigs[0] = _sign(pkA, structHash);
        sigs[1] = abi.encodePacked(rr, highS, flipped);
        vm.expectRevert(abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureS.selector, highS));
        bridge.executePayout(r, sigs);
    }

    function test_rejects_invalid_v() public {
        GlcRobinhoodBridge.PayoutRequest memory r = _payoutReq(keccak256("m6"));
        bytes32 structHash = _payoutHash(r);
        bytes32 digest =
            keccak256(abi.encodePacked(hex"1901", bridge.domainSeparator(), structHash));
        (, bytes32 rr, bytes32 s) = vm.sign(pkA, digest);

        bytes[] memory sigs = new bytes[](2);
        sigs[0] = abi.encodePacked(rr, s, uint8(29));
        sigs[1] = _sign(pkB, structHash);
        vm.expectRevert(ECDSA.ECDSAInvalidSignature.selector);
        bridge.executePayout(r, sigs);
    }

    // -----------------------------------------------------------------
    // Cross-action separation
    // -----------------------------------------------------------------

    /// A signature over a settlement authorization must not authorize a refund
    /// of the same obligation, even with an identical request id.
    function test_settlement_signature_cannot_authorize_refund() public {
        uint256 idx = _deposit(alice, 1000 * ONE_GLC);
        bytes32 id = keccak256("shared-id");

        GlcRobinhoodBridge.SettlementRequest memory sr = GlcRobinhoodBridge.SettlementRequest({
            requestId: id,
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory settleSigs = _quorumAB(_settlementHash(sr));

        GlcRobinhoodBridge.RefundRequest memory rr = GlcRobinhoodBridge.RefundRequest({
            requestId: id,
            obligationIndex: idx,
            recipient: alice,
            amount: 1000 * ONE_GLC,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        vm.expectRevert();
        bridge.executeRefund(rr, settleSigs);
    }

    /// And the reverse.
    function test_refund_signature_cannot_authorize_settlement() public {
        uint256 idx = _deposit(alice, 1000 * ONE_GLC);
        bytes32 id = keccak256("shared-id-2");

        GlcRobinhoodBridge.RefundRequest memory rr = GlcRobinhoodBridge.RefundRequest({
            requestId: id,
            obligationIndex: idx,
            recipient: alice,
            amount: 1000 * ONE_GLC,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bytes[] memory refundSigs = _quorumAB(_refundHash(rr));

        GlcRobinhoodBridge.SettlementRequest memory sr = GlcRobinhoodBridge.SettlementRequest({
            requestId: id,
            obligationIndex: idx,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        vm.expectRevert();
        bridge.executeSettlement(sr, refundSigs);
    }

    /// A governance authorization for one action cannot be replayed as another,
    /// even at the same nonce with the same payload.
    function test_governance_action_discriminator_is_binding() public {
        uint256 nonce = bridge.governanceNonce();
        address[3] memory set = [address(0x6A4), address(0x6A5), address(0x6A6)];
        bytes32 payload = keccak256(abi.encode(set));

        bytes32 guardianHash =
            _governanceHash(bridge.ACTION_ROTATE_GUARDIANS(), payload, nonce, FAR_FUTURE);
        bytes[] memory sigs = _quorumAB(guardianHash);

        // Same payload, same nonce, different action -> must not verify.
        vm.expectRevert();
        bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
    }

    /// The request-id replay guard is keyed per action, so the same id may be
    /// used once under each distinct action without colliding.
    function test_same_request_id_usable_once_per_action() public {
        uint256 idx = _deposit(alice, 1000 * ONE_GLC);
        bytes32 id = keccak256("per-action");

        _settle(id, idx);
        assertTrue(bridge.requestExecuted(bridge.ACTION_SETTLE(), id));
        assertFalse(bridge.requestExecuted(bridge.ACTION_PAYOUT(), id));

        _payout(id, bob, 500 * ONE_GLC);
        assertTrue(bridge.requestExecuted(bridge.ACTION_PAYOUT(), id));
    }
}
