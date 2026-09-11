// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {MockGlc} from "./mocks/MockGlc.sol";
import {WrongDecimalsGlc} from "./mocks/WrongDecimalsGlc.sol";

contract DeploymentTest is BridgeTestBase {
    function _deploy(
        IERC20 token_,
        address[3] memory signers_,
        address[3] memory guardians_,
        uint64 pcGold,
        uint64 pcRhn,
        GlcRobinhoodBridge.Limits memory lim
    ) internal returns (GlcRobinhoodBridge) {
        return _deploy(token_, signers_, guardians_, pcGold, pcRhn, PROTOCOL_SOLANA, lim);
    }

    function _deploy(
        IERC20 token_,
        address[3] memory signers_,
        address[3] memory guardians_,
        uint64 pcGold,
        uint64 pcRhn,
        uint64 pcSol,
        GlcRobinhoodBridge.Limits memory lim
    ) internal returns (GlcRobinhoodBridge) {
        return new GlcRobinhoodBridge(
            token_, signers_, guardians_, pcGold, pcRhn, pcSol, lim, treasury
        );
    }

    function test_rejects_zero_token() public {
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        _deploy(
            IERC20(address(0)),
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_wrong_token_decimals() public {
        WrongDecimalsGlc sixDecimals = new WrongDecimalsGlc();
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnexpectedTokenDecimals.selector, uint8(6))
        );
        _deploy(
            IERC20(address(sixDecimals)),
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_zero_signer() public {
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        _deploy(
            glc,
            [signerA, address(0), signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_duplicate_signer() public {
        vm.expectRevert(GlcRobinhoodBridge.DuplicateSigner.selector);
        _deploy(
            glc,
            [signerA, signerB, signerA],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_zero_guardian() public {
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, address(0), guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_duplicate_guardian() public {
        vm.expectRevert(GlcRobinhoodBridge.DuplicateGuardian.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian1],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_zero_protocol_chain_ids() public {
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            0,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );

        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            0,
            _defaultLimits()
        );
    }

    /// The Solana leg is required at construction even though no Solana route
    /// is enabled at launch. Configuring a leg is not activating it, and a leg
    /// left unset would be a chain id every future signature depends on that
    /// nobody ever chose.
    function test_rejects_zero_solana_protocol_chain_id() public {
        vm.expectRevert(GlcRobinhoodBridge.ZeroAddress.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            0,
            _defaultLimits()
        );
    }

    /// Two legs sharing an id would make a route's signed payload state the
    /// wrong source network, so every pair must be distinct.
    function test_rejects_duplicate_protocol_chain_ids() public {
        vm.expectRevert(GlcRobinhoodBridge.DuplicateProtocolChain.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_GOLDCOIN,
            PROTOCOL_SOLANA,
            _defaultLimits()
        );

        vm.expectRevert(GlcRobinhoodBridge.DuplicateProtocolChain.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_GOLDCOIN,
            _defaultLimits()
        );

        vm.expectRevert(GlcRobinhoodBridge.DuplicateProtocolChain.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
    }

    function test_rejects_invalid_limits_at_deployment() public {
        GlcRobinhoodBridge.Limits memory lim = _defaultLimits();
        lim.inboundMin = lim.inboundMax + SCALE;
        vm.expectRevert(GlcRobinhoodBridge.InvalidLimits.selector);
        _deploy(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            lim
        );
    }

    /// The route must launch fail-closed: no window exists in which a freshly
    /// deployed bridge is live.
    function test_launches_fail_closed() public {
        MockGlc token = new MockGlc();
        GlcRobinhoodBridge fresh = _deploy(
            token,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            _defaultLimits()
        );
        assertTrue(fresh.depositsPaused());
        assertTrue(fresh.payoutsPaused());

        // The second gate: every route ships off as well, so clearing the
        // pause alone still leaves nothing live.
        uint8[4] memory routes_ = fresh.routes();
        for (uint256 i = 0; i < routes_.length; ++i) {
            assertFalse(fresh.routeEnabled(routes_[i]));
            assertFalse(fresh.isRouteLive(routes_[i]));
        }
    }

    function test_initial_state() public view {
        assertEq(bridge.obligationCount(), 0);
        assertEq(bridge.signerEpoch(), 0);
        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
        assertFalse(bridge.migrationCommitted());
        assertFalse(bridge.migrated());
        assertEq(bridge.migrationSuccessor(), address(0));
        assertEq(address(bridge.TOKEN()), address(glc));
        assertEq(bridge.token(), address(glc));
        assertTrue(bridge.isSigner(signerA));
        assertTrue(bridge.isSigner(signerB));
        assertTrue(bridge.isSigner(signerC));
        assertFalse(bridge.isSigner(rogue));
        assertTrue(bridge.isGuardian(guardian1));
        assertFalse(bridge.isGuardian(signerA));
    }

    /// Guardians and signers are separate authorities: neither set may be
    /// assumed to imply the other.
    function test_guardians_are_not_signers() public view {
        assertFalse(bridge.isSigner(guardian1));
        assertFalse(bridge.isSigner(guardian2));
        assertFalse(bridge.isSigner(guardian3));
        assertFalse(bridge.isGuardian(signerA));
        assertFalse(bridge.isGuardian(signerB));
        assertFalse(bridge.isGuardian(signerC));
    }

    function test_constants() public view {
        assertEq(bridge.CANONICAL_SCALE(), 1e10);
        assertEq(bridge.SIGNER_COUNT(), 3);
        assertEq(bridge.SIGNER_THRESHOLD(), 2);
        assertEq(bridge.GUARDIAN_COUNT(), 3);
        assertEq(bridge.MIGRATION_DELAY(), 48 hours);
        assertEq(bridge.ROLLING_WINDOW_SECONDS(), 24 hours);
        assertEq(bridge.MAX_DESTINATION_LEN(), 64);
        assertEq(bridge.EXPECTED_TOKEN_DECIMALS(), 18);
        assertEq(bridge.ROUTE_COUNT(), 4);
    }

    /// The raw EIP-155 chain id and the bridge's namespaced protocol chain ids
    /// are different things and must never be conflated.
    function test_protocol_chain_ids_are_not_evm_chain_id() public view {
        assertEq(bridge.PROTOCOL_CHAIN_GOLDCOIN(), PROTOCOL_GOLDCOIN);
        assertEq(bridge.PROTOCOL_CHAIN_ROBINHOOD(), PROTOCOL_ROBINHOOD);
        assertEq(bridge.PROTOCOL_CHAIN_SOLANA(), PROTOCOL_SOLANA);
        assertTrue(bridge.PROTOCOL_CHAIN_ROBINHOOD() != block.chainid);
    }
}
