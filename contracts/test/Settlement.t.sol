// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";

contract SettlementTest is BridgeTestBase {
    event ObligationSettled(uint256 indexed obligationIndex, bytes32 indexed requestId);

    bytes32 internal constant REQ = keccak256("settle-1");
    uint256 internal constant DEPOSIT_AMOUNT = 1000 * 1e18;

    function _mkReq(uint256 index)
        internal
        view
        returns (GlcRobinhoodBridge.SettlementRequest memory)
    {
        return GlcRobinhoodBridge.SettlementRequest({
            requestId: REQ,
            obligationIndex: index,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
    }

    function test_happy_path_releases_liability() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        assertEq(bridge.outstandingRefundableCount(), 1);
        assertEq(bridge.outstandingRefundablePrincipal(), DEPOSIT_AMOUNT);

        vm.expectEmit(true, true, true, true, address(bridge));
        emit ObligationSettled(idx, REQ);
        _settle(REQ, idx);

        assertTrue(bridge.obligation(idx).status == GlcRobinhoodBridge.ObligationStatus.Settled);
        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
    }

    /// Settlement moves no tokens. It is an accounting transition only.
    function test_moves_no_tokens() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 bridgeBalance = glc.balanceOf(address(bridge));
        uint256 aliceBalance = glc.balanceOf(alice);

        _settle(REQ, idx);

        assertEq(glc.balanceOf(address(bridge)), bridgeBalance);
        assertEq(glc.balanceOf(alice), aliceBalance);
    }

    /// It mutates neither the recorded depositor nor the recorded principal.
    function test_does_not_mutate_principal_or_address() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(REQ, idx);
        GlcRobinhoodBridge.Obligation memory ob = bridge.obligation(idx);
        assertEq(ob.depositor, alice);
        assertEq(ob.amount, DEPOSIT_AMOUNT);
    }

    function test_rejects_nonexistent_obligation() public {
        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(0);
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotFound.selector);
        bridge.executeSettlement(r, sigs);
    }

    function test_rejects_double_settlement() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(REQ, idx);

        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(idx);
        r.requestId = keccak256("settle-2");
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeSettlement(r, sigs);
    }

    /// Mutual exclusion in the other direction: a refunded obligation can never
    /// also be settled.
    function test_cannot_settle_a_refunded_obligation() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _refund(keccak256("refund-x"), idx);

        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ObligationNotPending.selector);
        bridge.executeSettlement(r, sigs);
    }

    function test_requires_quorum() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(idx);
        bytes[] memory sigs = new bytes[](1);
        sigs[0] = _sign(pkA, _settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.executeSettlement(r, sigs);
    }

    function test_rejects_unauthorized_signer() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(idx);
        bytes[] memory sigs = _quorum(_settlementHash(r), pkRogue, pkB);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.executeSettlement(r, sigs);
    }

    function test_rejects_expired() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(idx);
        r.expiry = uint64(block.timestamp - 1);
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AuthorizationExpired.selector);
        bridge.executeSettlement(r, sigs);
    }

    function test_rejects_request_id_replay() public {
        uint256 first = _deposit(alice, DEPOSIT_AMOUNT);
        uint256 second = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(REQ, first);

        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(second);
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executeSettlement(r, sigs);
    }

    /// Settlement grants no generic power: it cannot be used while the contract
    /// is terminal, and it is the only thing this authorization can do.
    function test_rejects_after_migration_finalized() public {
        uint256 idx = _deposit(alice, DEPOSIT_AMOUNT);
        _settle(REQ, idx);
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        _finalizeMigration();

        uint256 later = 0;
        GlcRobinhoodBridge.SettlementRequest memory r = _mkReq(later);
        r.requestId = keccak256("settle-after");
        bytes[] memory sigs = _quorumAB(_settlementHash(r));
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.executeSettlement(r, sigs);
    }
}
