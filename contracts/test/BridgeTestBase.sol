// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";

import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {MockGlc} from "./mocks/MockGlc.sol";
import {MockSuccessor} from "./mocks/MockSuccessor.sol";

/// Shared fixture and EIP-712 signing helpers.
///
/// The helpers rebuild each struct hash independently of the contract, from the
/// typehash string upward, so a test proves the contract agrees with the
/// specification rather than merely agreeing with itself. That discipline
/// extends to the route topology: `_legs` below restates which chains each
/// route connects rather than asking the contract, so a test would catch the
/// contract rewiring a route as much as it would catch a bad hash.
abstract contract BridgeTestBase is Test {
    uint256 internal constant SCALE = 1e10;
    uint256 internal constant ONE_GLC = 1e18;

    uint64 internal constant PROTOCOL_GOLDCOIN = 1001;
    uint64 internal constant PROTOCOL_ROBINHOOD = 2001;
    uint64 internal constant PROTOCOL_SOLANA = 3001;

    uint8 internal constant ROUTE_GLC_TO_RHN = 0x01;
    uint8 internal constant ROUTE_RHN_TO_GLC = 0x02;
    uint8 internal constant ROUTE_SOL_TO_RHN = 0x03;
    uint8 internal constant ROUTE_RHN_TO_SOL = 0x04;

    uint256 internal constant INBOUND_MIN = 100 * ONE_GLC;
    uint256 internal constant INBOUND_MAX = 20_000 * ONE_GLC;
    uint256 internal constant INBOUND_ROLLING = 100_000 * ONE_GLC;
    uint256 internal constant OUTBOUND_MIN = 100 * ONE_GLC;
    uint256 internal constant OUTBOUND_MAX = 20_000 * ONE_GLC;
    uint256 internal constant OUTBOUND_ROLLING = 100_000 * ONE_GLC;

    uint256 internal constant RESERVE_SEED = 1_000_000 * ONE_GLC;
    uint64 internal constant FAR_FUTURE = 4_000_000_000;

    GlcRobinhoodBridge internal bridge;
    MockGlc internal glc;

    uint256 internal pkA;
    uint256 internal pkB;
    uint256 internal pkC;
    address internal signerA;
    address internal signerB;
    address internal signerC;

    address internal guardian1 = address(0x6A1);
    address internal guardian2 = address(0x6A2);
    address internal guardian3 = address(0x6A3);

    /// The immutable treasury every test deployment is constructed with.
    /// A plain EOA-shaped address: the withdrawal path's destination is
    /// whatever the deployer fixed, and nothing about it is special.
    address internal treasury = address(0x7E45);
    address internal alice = address(0xA11CE);
    address internal bob = address(0xB0B);
    address internal outsider = address(0x0175DE0);

    uint256 internal pkRogue;
    address internal rogue;

    function setUp() public virtual {
        // A realistic wall-clock start. Foundry defaults to timestamp 1, which
        // is not a state any live chain is ever in.
        vm.warp(1_780_000_000);

        (signerA, pkA) = makeAddrAndKey("signerA");
        (signerB, pkB) = makeAddrAndKey("signerB");
        (signerC, pkC) = makeAddrAndKey("signerC");
        (rogue, pkRogue) = makeAddrAndKey("rogue");

        glc = new MockGlc();
        bridge = new GlcRobinhoodBridge(
            glc,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits(),
            treasury
        );

        glc.mint(alice, RESERVE_SEED);
        glc.mint(bob, RESERVE_SEED);
        glc.mint(address(bridge), RESERVE_SEED);

        vm.prank(alice);
        assertTrue(glc.approve(address(bridge), type(uint256).max));
        vm.prank(bob);
        assertTrue(glc.approve(address(bridge), type(uint256).max));

        // Every test starts after a normal launch: deployed fail-closed, then
        // explicitly opened by quorum. "Opened" means BOTH gates were cleared —
        // the directions unpaused and the two Goldcoin routes enabled one at a
        // time. The Solana routes are deliberately left disabled, which is the
        // state they ship in; a test that wants them on must say so.
        _bootstrap(bridge);
    }

    function _defaultLimits() internal pure returns (GlcRobinhoodBridge.Limits memory) {
        return GlcRobinhoodBridge.Limits({
            inboundMin: INBOUND_MIN,
            inboundMax: INBOUND_MAX,
            inboundRollingLimit: INBOUND_ROLLING,
            outboundMin: OUTBOUND_MIN,
            outboundMax: OUTBOUND_MAX,
            outboundRollingLimit: OUTBOUND_ROLLING,
            protectedMinReserve: 0
        });
    }

    // -----------------------------------------------------------------
    // Routes
    // -----------------------------------------------------------------

    /// The route an obligation was created on — or the default inbound route
    /// when the index does not exist.
    ///
    /// The fallback is what lets a test deliberately name a nonexistent
    /// obligation: it still needs a signable hash to hand the contract, and the
    /// contract rejects the index with `ObligationNotFound` long before it
    /// looks at a signature, so which route the hash was built over is
    /// immaterial. Reverting here instead would swallow the very error such a
    /// test exists to assert.
    function _obligationRoute(GlcRobinhoodBridge b, uint256 index) internal view returns (uint8) {
        if (index >= b.obligationCount()) return ROUTE_RHN_TO_GLC;
        return b.obligation(index).route;
    }

    /// The route topology, restated independently of the contract.
    function _legs(uint8 route) internal pure returns (uint64 source, uint64 dest) {
        if (route == ROUTE_GLC_TO_RHN) return (PROTOCOL_GOLDCOIN, PROTOCOL_ROBINHOOD);
        if (route == ROUTE_RHN_TO_GLC) return (PROTOCOL_ROBINHOOD, PROTOCOL_GOLDCOIN);
        if (route == ROUTE_SOL_TO_RHN) return (PROTOCOL_SOLANA, PROTOCOL_ROBINHOOD);
        if (route == ROUTE_RHN_TO_SOL) return (PROTOCOL_ROBINHOOD, PROTOCOL_SOLANA);
        revert("BridgeTestBase: unknown route");
    }

    // -----------------------------------------------------------------
    // Signing
    // -----------------------------------------------------------------

    function _signOn(GlcRobinhoodBridge b, uint256 pk, bytes32 structHash)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = MessageHashUtils.toTypedDataHash(b.domainSeparator(), structHash);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }

    function _sign(uint256 pk, bytes32 structHash) internal view returns (bytes memory) {
        return _signOn(bridge, pk, structHash);
    }

    function _quorum(bytes32 structHash, uint256 first, uint256 second)
        internal
        view
        returns (bytes[] memory sigs)
    {
        sigs = new bytes[](2);
        sigs[0] = _sign(first, structHash);
        sigs[1] = _sign(second, structHash);
    }

    function _quorumAB(bytes32 structHash) internal view returns (bytes[] memory) {
        return _quorum(structHash, pkA, pkB);
    }

    function _quorumABOn(GlcRobinhoodBridge b, bytes32 structHash)
        internal
        view
        returns (bytes[] memory sigs)
    {
        sigs = new bytes[](2);
        sigs[0] = _signOn(b, pkA, structHash);
        sigs[1] = _signOn(b, pkB, structHash);
    }

    // -----------------------------------------------------------------
    // Struct hashes (rebuilt from the typehash, not read off the contract)
    // -----------------------------------------------------------------

    function _payoutHash(GlcRobinhoodBridge.PayoutRequest memory req)
        internal
        view
        returns (bytes32)
    {
        return _payoutHashOn(bridge, address(glc), req);
    }

    function _payoutHashOn(
        GlcRobinhoodBridge b,
        address token_,
        GlcRobinhoodBridge.PayoutRequest memory req
    ) internal view returns (bytes32) {
        (uint64 source, uint64 dest) = _legs(req.route);
        return keccak256(
            abi.encode(
                b.PAYOUT_TYPEHASH(),
                b.ACTION_PAYOUT(),
                req.route,
                source,
                dest,
                token_,
                req.requestId,
                req.recipient,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    /// The obligation-closing hashes read the route from the obligation itself,
    /// exactly as the contract does: it is not a signer's choice.
    function _refundHash(GlcRobinhoodBridge.RefundRequest memory req)
        internal
        view
        returns (bytes32)
    {
        return _refundHashOn(bridge, address(glc), req);
    }

    function _refundHashOn(
        GlcRobinhoodBridge b,
        address token_,
        GlcRobinhoodBridge.RefundRequest memory req
    ) internal view returns (bytes32) {
        uint8 route = _obligationRoute(b, req.obligationIndex);
        (uint64 source, uint64 dest) = _legs(route);
        return keccak256(
            abi.encode(
                b.REFUND_TYPEHASH(),
                b.ACTION_REFUND(),
                route,
                source,
                dest,
                token_,
                req.requestId,
                req.obligationIndex,
                req.recipient,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    function _settlementHash(GlcRobinhoodBridge.SettlementRequest memory req)
        internal
        view
        returns (bytes32)
    {
        return _settlementHashOn(bridge, req);
    }

    function _settlementHashOn(
        GlcRobinhoodBridge b,
        GlcRobinhoodBridge.SettlementRequest memory req
    ) internal view returns (bytes32) {
        uint8 route = _obligationRoute(b, req.obligationIndex);
        (uint64 source, uint64 dest) = _legs(route);
        return keccak256(
            abi.encode(
                b.SETTLEMENT_TYPEHASH(),
                b.ACTION_SETTLE(),
                route,
                source,
                dest,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    function _abandonmentHash(GlcRobinhoodBridge.AbandonmentRequest memory req)
        internal
        view
        returns (bytes32)
    {
        return _abandonmentHashOn(bridge, req);
    }

    function _abandonmentHashOn(
        GlcRobinhoodBridge b,
        GlcRobinhoodBridge.AbandonmentRequest memory req
    ) internal view returns (bytes32) {
        uint8 route = _obligationRoute(b, req.obligationIndex);
        (uint64 source, uint64 dest) = _legs(route);
        return keccak256(
            abi.encode(
                b.ABANDONMENT_TYPEHASH(),
                b.ACTION_ABANDON(),
                route,
                source,
                dest,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    function _treasuryWithdrawHash(GlcRobinhoodBridge.TreasuryWithdrawRequest memory req)
        internal
        view
        returns (bytes32)
    {
        return _treasuryWithdrawHashOn(bridge, req);
    }

    function _treasuryWithdrawHashOn(
        GlcRobinhoodBridge b,
        GlcRobinhoodBridge.TreasuryWithdrawRequest memory req
    ) internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                b.TREASURY_WITHDRAW_TYPEHASH(),
                b.ACTION_TREASURY_WITHDRAW(),
                address(b.TOKEN()),
                req.requestId,
                req.treasury,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    function _governanceHash(uint8 action, bytes32 payloadHash, uint256 nonce, uint64 expiry)
        internal
        view
        returns (bytes32)
    {
        return _governanceHashOn(bridge, action, payloadHash, nonce, expiry);
    }

    function _governanceHashOn(
        GlcRobinhoodBridge b,
        uint8 action,
        bytes32 payloadHash,
        uint256 nonce,
        uint64 expiry
    ) internal view returns (bytes32) {
        return keccak256(
            abi.encode(b.GOVERNANCE_TYPEHASH(), action, payloadHash, b.signerEpoch(), nonce, expiry)
        );
    }

    // -----------------------------------------------------------------
    // Common operations
    // -----------------------------------------------------------------

    function _setPausedOn(GlcRobinhoodBridge b, bool depositsPaused_, bool payoutsPaused_)
        internal
    {
        uint256 nonce = b.governanceNonce();
        bytes32 payload = keccak256(abi.encode(depositsPaused_, payoutsPaused_));
        bytes32 h = _governanceHashOn(b, b.ACTION_SET_PAUSE(), payload, nonce, FAR_FUTURE);
        b.setPaused(depositsPaused_, payoutsPaused_, nonce, FAR_FUTURE, _quorumABOn(b, h));
    }

    function _setPaused(bool depositsPaused_, bool payoutsPaused_) internal {
        _setPausedOn(bridge, depositsPaused_, payoutsPaused_);
    }

    function _setRouteEnabledOn(GlcRobinhoodBridge b, uint8 route, bool enabled) internal {
        uint256 nonce = b.governanceNonce();
        bytes32 payload = keccak256(abi.encode(route, enabled));
        bytes32 h = _governanceHashOn(b, b.ACTION_SET_ROUTE_ENABLED(), payload, nonce, FAR_FUTURE);
        b.setRouteEnabled(route, enabled, nonce, FAR_FUTURE, _quorumABOn(b, h));
    }

    function _setRouteEnabled(uint8 route, bool enabled) internal {
        _setRouteEnabledOn(bridge, route, enabled);
    }

    /// Take a freshly deployed, fail-closed bridge to the state every test that
    /// is not about launch assumes: directions unpaused, both Goldcoin routes
    /// enabled, both Solana routes still off.
    function _bootstrap(GlcRobinhoodBridge b) internal {
        _setPausedOn(b, false, false);
        _setRouteEnabledOn(b, ROUTE_GLC_TO_RHN, true);
        _setRouteEnabledOn(b, ROUTE_RHN_TO_GLC, true);
    }

    function _openBothRoutes() internal {
        _setPaused(false, false);
    }

    function _pauseBothRoutes() internal {
        _setPaused(true, true);
    }

    function _deposit(address who, uint256 amount) internal returns (uint256 index) {
        return _depositOn(who, ROUTE_RHN_TO_GLC, amount, _destination());
    }

    function _depositOn(address who, uint8 route, uint256 amount, bytes memory destination)
        internal
        returns (uint256 index)
    {
        vm.prank(who);
        index = bridge.deposit(route, amount, destination);
    }

    function _destination() internal pure returns (bytes memory) {
        // A realistic Goldcoin mainnet P2PKH address, as opaque ASCII bytes.
        // The contract never parses this; only its length is constrained.
        return bytes("EX1qq7v9m2WmYQ8Xy3zLzB6nF1cRk4dTgHs");
    }

    function _solDestination() internal pure returns (bytes memory) {
        // A realistic Solana pubkey in base58, as opaque ASCII bytes. Same
        // treatment as the Goldcoin payload: never parsed, only bounded.
        return bytes("9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin");
    }

    function _payout(bytes32 requestId, address recipient, uint256 amount) internal {
        _payoutOn(ROUTE_GLC_TO_RHN, requestId, recipient, amount);
    }

    function _payoutOn(uint8 route, bytes32 requestId, address recipient, uint256 amount) internal {
        GlcRobinhoodBridge.PayoutRequest memory req = GlcRobinhoodBridge.PayoutRequest({
            route: route,
            requestId: requestId,
            recipient: recipient,
            amount: amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bridge.executePayout(req, _quorumAB(_payoutHash(req)));
    }

    function _refund(bytes32 requestId, uint256 index) internal {
        GlcRobinhoodBridge.Obligation memory ob = bridge.obligation(index);
        GlcRobinhoodBridge.RefundRequest memory req = GlcRobinhoodBridge.RefundRequest({
            requestId: requestId,
            obligationIndex: index,
            recipient: ob.depositor,
            amount: ob.amount,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bridge.executeRefund(req, _quorumAB(_refundHash(req)));
    }

    function _settle(bytes32 requestId, uint256 index) internal {
        GlcRobinhoodBridge.SettlementRequest memory req = GlcRobinhoodBridge.SettlementRequest({
            requestId: requestId,
            obligationIndex: index,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bridge.executeSettlement(req, _quorumAB(_settlementHash(req)));
    }

    function _abandon(bytes32 requestId, uint256 index) internal {
        GlcRobinhoodBridge.AbandonmentRequest memory req = GlcRobinhoodBridge.AbandonmentRequest({
            requestId: requestId,
            obligationIndex: index,
            signerEpoch: bridge.signerEpoch(),
            expiry: FAR_FUTURE
        });
        bridge.executeAbandonment(req, _quorumAB(_abandonmentHash(req)));
    }

    function _deployConformingSuccessor() internal returns (MockSuccessor) {
        return new MockSuccessor(address(glc), bridge.BRIDGE_PROTOCOL_ID());
    }

    function _commitMigration(address successor) internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_COMMIT_MIGRATION(), keccak256(abi.encode(successor)), nonce, FAR_FUTURE
        );
        bridge.commitMigration(successor, nonce, FAR_FUTURE, _quorumAB(h));
    }

    function _finalizeMigration() internal {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_FINALIZE_MIGRATION(),
            keccak256(abi.encode(bridge.migrationSuccessor())),
            nonce,
            FAR_FUTURE
        );
        bridge.finalizeMigration(nonce, FAR_FUTURE, _quorumAB(h));
    }
}
