// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

contract RotationTest is BridgeTestBase {
    address internal newSigner1;
    address internal newSigner2;
    address internal newSigner3;
    uint256 internal npk1;
    uint256 internal npk2;
    uint256 internal npk3;

    function setUp() public override {
        super.setUp();
        (newSigner1, npk1) = makeAddrAndKey("newSigner1");
        (newSigner2, npk2) = makeAddrAndKey("newSigner2");
        (newSigner3, npk3) = makeAddrAndKey("newSigner3");
    }

    function _rotateSigners(address[3] memory set) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bridge.rotateSigners(set, nonce, FAR_FUTURE, _quorumAB(h));
    }

    function _rotateSignersExpectRevert(address[3] memory set, bytes4 err) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(err);
        bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
    }

    // -----------------------------------------------------------------
    // Signers
    // -----------------------------------------------------------------

    function test_valid_rotation() public {
        _rotateSigners([newSigner1, newSigner2, newSigner3]);

        assertEq(bridge.signerEpoch(), 1);
        assertTrue(bridge.isSigner(newSigner1));
        assertFalse(bridge.isSigner(signerA));
        assertFalse(bridge.isSigner(signerB));
        assertFalse(bridge.isSigner(signerC));
    }

    function test_rejects_zero_new_signer() public {
        _rotateSignersExpectRevert(
            [newSigner1, address(0), newSigner3], GlcRobinhoodBridge.ZeroAddress.selector
        );
    }

    function test_rejects_duplicate_new_signer() public {
        _rotateSignersExpectRevert(
            [newSigner1, newSigner2, newSigner1], GlcRobinhoodBridge.DuplicateSigner.selector
        );
    }

    function test_rejects_all_identical_new_signers() public {
        _rotateSignersExpectRevert(
            [newSigner1, newSigner1, newSigner1], GlcRobinhoodBridge.DuplicateSigner.selector
        );
    }

    function test_requires_quorum() public {
        address[3] memory set = [newSigner1, newSigner2, newSigner3];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, h);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
    }

    /// A rotation that reuses an outgoing member must still work, and must not
    /// leave that member's authority bit in a stale state.
    function test_rotation_overlapping_with_old_set() public {
        _rotateSigners([signerA, newSigner2, newSigner3]);
        assertTrue(bridge.isSigner(signerA));
        assertFalse(bridge.isSigner(signerB));
        assertFalse(bridge.isSigner(signerC));
        assertTrue(bridge.isSigner(newSigner2));
    }

    /// The whole point of the epoch: the outgoing set's signatures die instantly.
    function test_old_epoch_authorization_invalid_afterwards() public {
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("pre-rotation"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory preSigs = _quorumAB(_payoutHash(r));

        _rotateSigners([newSigner1, newSigner2, newSigner3]);

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidSignerEpoch.selector, uint64(1), uint64(0)
            )
        );
        bridge.executePayout(r, preSigs);
    }

    /// Re-stamping the old signature with the new epoch does not help either:
    /// the old keys are no longer signers.
    function test_old_signers_rejected_under_new_epoch() public {
        _rotateSigners([newSigner1, newSigner2, newSigner3]);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("post-rotation"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 1,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, signerA)
        );
        bridge.executePayout(r, sigs);
    }

    function test_new_signers_work_after_rotation() public {
        _rotateSigners([newSigner1, newSigner2, newSigner3]);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("post-rotation-ok"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 1,
            expiry: FAR_FUTURE
        });
        uint256 before = glc.balanceOf(bob);
        bridge.executePayout(r, _quorum(_payoutHash(r), npk1, npk3));
        assertEq(glc.balanceOf(bob), before + 500 * ONE_GLC);
    }

    /// A governance signature cannot be replayed after the nonce advances.
    function test_governance_replay_rejected() public {
        address[3] memory set = [newSigner1, newSigner2, newSigner3];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, nonce + 1, nonce
            )
        );
        bridge.rotateSigners(set, nonce, FAR_FUTURE, sigs);
    }

    /// Rotating twice requires the second authorization to carry BOTH the new
    /// nonce and the new epoch.
    function test_consecutive_rotations() public {
        _rotateSigners([newSigner1, newSigner2, newSigner3]);
        assertEq(bridge.signerEpoch(), 1);
        // Three governance actions from `setUp` (unpause, then each Goldcoin
        // route enabled separately) plus this rotation.
        assertEq(bridge.governanceNonce(), 4);

        address[3] memory third = [signerA, signerB, signerC];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_SIGNERS(), keccak256(abi.encode(third)), nonce, FAR_FUTURE
        );
        bridge.rotateSigners(third, nonce, FAR_FUTURE, _quorum(h, npk1, npk2));

        assertEq(bridge.signerEpoch(), 2);
        assertTrue(bridge.isSigner(signerA));
        assertFalse(bridge.isSigner(newSigner1));
    }

    // -----------------------------------------------------------------
    // Guardians
    // -----------------------------------------------------------------

    function _rotateGuardians(address[3] memory set) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_GUARDIANS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bridge.rotateGuardians(set, nonce, FAR_FUTURE, _quorumAB(h));
    }

    function test_valid_guardian_rotation() public {
        address g4 = address(0x6A4);
        address g5 = address(0x6A5);
        address g6 = address(0x6A6);
        _rotateGuardians([g4, g5, g6]);

        assertTrue(bridge.isGuardian(g4));
        assertFalse(bridge.isGuardian(guardian1));
    }

    /// The old guardian loses the ability to pause immediately.
    function test_old_guardian_loses_authority() public {
        _rotateGuardians([address(0x6A4), address(0x6A5), address(0x6A6)]);

        vm.prank(guardian1);
        vm.expectRevert(GlcRobinhoodBridge.UnauthorizedGuardian.selector);
        bridge.guardianPause(true, true);

        vm.prank(address(0x6A4));
        bridge.guardianPause(true, true);
        assertTrue(bridge.depositsPaused());
    }

    function test_rejects_invalid_guardian_set() public {
        address[3] memory bad = [address(0x6A4), address(0), address(0x6A6)];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_GUARDIANS(), keccak256(abi.encode(bad)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        bridge.rotateGuardians(bad, nonce, FAR_FUTURE, sigs);
    }

    function test_rejects_duplicate_guardian() public {
        address g4 = address(0x6A4);
        address[3] memory bad = [g4, address(0x6A5), g4];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_GUARDIANS(), keccak256(abi.encode(bad)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.DuplicateGuardian.selector);
        bridge.rotateGuardians(bad, nonce, FAR_FUTURE, sigs);
    }

    function test_guardian_rotation_replay_rejected() public {
        address[3] memory set = [address(0x6A4), address(0x6A5), address(0x6A6)];
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_ROTATE_GUARDIANS(), keccak256(abi.encode(set)), nonce, FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        bridge.rotateGuardians(set, nonce, FAR_FUTURE, sigs);

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, nonce + 1, nonce
            )
        );
        bridge.rotateGuardians(set, nonce, FAR_FUTURE, sigs);
    }

    /// A guardian rotation never grants token-moving power.
    function test_guardian_cannot_move_tokens() public {
        address g4 = address(0x6A4);
        _rotateGuardians([g4, address(0x6A5), address(0x6A6)]);
        uint256 balanceBefore = glc.balanceOf(address(bridge));
        vm.prank(g4);
        bridge.guardianPause(true, true);
        assertEq(glc.balanceOf(address(bridge)), balanceBefore);
    }
}
