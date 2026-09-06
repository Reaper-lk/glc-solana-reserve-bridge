// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BridgeTestBase} from "./BridgeTestBase.sol";
import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {MockGlc} from "./mocks/MockGlc.sol";

/// The four-route surface: `GlcToRhn`, `RhnToGlc`, `SolToRhn`, `RhnToSol`.
///
/// Three properties are what these tests exist to pin, and they are the three
/// that would be expensive to discover were wrong:
///
///  1. Every route ships DISABLED, and no single action makes one live.
///  2. The emergency pause strictly overrides route enablement, never the
///     reverse.
///  3. A route is BOUND into every authorization, so an authorization minted
///     for one route can never execute as another.
contract RoutesTest is BridgeTestBase {
    event RouteEnabledChanged(uint8 indexed route, bool enabled);

    event DepositCreated(
        uint256 indexed obligationIndex,
        address indexed depositor,
        uint8 indexed route,
        uint256 amount,
        uint256 canonicalAmount,
        bytes destination
    );

    /// A bridge in the state it is actually deployed in: nothing opened.
    function _freshBridge() internal returns (GlcRobinhoodBridge b, MockGlc token_) {
        token_ = new MockGlc();
        b = new GlcRobinhoodBridge(
            token_,
            [signerA, signerB, signerC],
            [guardian1, guardian2, guardian3],
            PROTOCOL_GOLDCOIN,
            PROTOCOL_ROBINHOOD,
            PROTOCOL_SOLANA,
            _defaultLimits()
        );
    }

    function _allFour() internal pure returns (uint8[4] memory) {
        return [ROUTE_GLC_TO_RHN, ROUTE_RHN_TO_GLC, ROUTE_SOL_TO_RHN, ROUTE_RHN_TO_SOL];
    }

    // =================================================================
    // Topology
    // =================================================================

    function test_route_discriminators_are_distinct_and_nonzero() public view {
        uint8[4] memory r = _allFour();
        assertEq(bridge.ROUTE_COUNT(), 4);
        for (uint256 i = 0; i < 4; ++i) {
            assertTrue(r[i] != 0, "0x00 must never name a route");
            for (uint256 j = i + 1; j < 4; ++j) {
                assertTrue(r[i] != r[j], "route discriminators must be distinct");
            }
        }
        assertEq(abi.encode(bridge.routes()), abi.encode(r));
    }

    /// Each route's legs, asserted against the values the deployment was given
    /// rather than against the contract's own view of them.
    function test_route_chain_pairs() public view {
        (uint64 s1, uint64 d1) = bridge.routeChains(ROUTE_GLC_TO_RHN);
        assertEq(s1, PROTOCOL_GOLDCOIN);
        assertEq(d1, PROTOCOL_ROBINHOOD);

        (uint64 s2, uint64 d2) = bridge.routeChains(ROUTE_RHN_TO_GLC);
        assertEq(s2, PROTOCOL_ROBINHOOD);
        assertEq(d2, PROTOCOL_GOLDCOIN);

        (uint64 s3, uint64 d3) = bridge.routeChains(ROUTE_SOL_TO_RHN);
        assertEq(s3, PROTOCOL_SOLANA);
        assertEq(d3, PROTOCOL_ROBINHOOD);

        (uint64 s4, uint64 d4) = bridge.routeChains(ROUTE_RHN_TO_SOL);
        assertEq(s4, PROTOCOL_ROBINHOOD);
        assertEq(d4, PROTOCOL_SOLANA);
    }

    function test_route_directions() public view {
        assertFalse(bridge.isDepositRoute(ROUTE_GLC_TO_RHN));
        assertTrue(bridge.isDepositRoute(ROUTE_RHN_TO_GLC));
        assertFalse(bridge.isDepositRoute(ROUTE_SOL_TO_RHN));
        assertTrue(bridge.isDepositRoute(ROUTE_RHN_TO_SOL));
    }

    /// A route that does not exist is not answered as "off"; it is refused.
    function test_unknown_route_views_revert() public {
        uint8[3] memory bad = [uint8(0x00), uint8(0x05), type(uint8).max];
        for (uint256 i = 0; i < bad.length; ++i) {
            vm.expectRevert(
                abi.encodeWithSelector(GlcRobinhoodBridge.UnknownRoute.selector, bad[i])
            );
            bridge.routeEnabled(bad[i]);

            vm.expectRevert(
                abi.encodeWithSelector(GlcRobinhoodBridge.UnknownRoute.selector, bad[i])
            );
            bridge.routeChains(bad[i]);

            vm.expectRevert(
                abi.encodeWithSelector(GlcRobinhoodBridge.UnknownRoute.selector, bad[i])
            );
            bridge.isRouteLive(bad[i]);
        }
    }

    // =================================================================
    // Deployment is fail-closed
    // =================================================================

    function test_every_route_deploys_disabled() public {
        (GlcRobinhoodBridge b,) = _freshBridge();
        uint8[4] memory r = _allFour();
        for (uint256 i = 0; i < 4; ++i) {
            assertFalse(b.routeEnabled(r[i]), "route must deploy disabled");
            assertFalse(b.isRouteLive(r[i]), "route must not be live at deployment");
        }
        assertTrue(b.depositsPaused());
        assertTrue(b.payoutsPaused());
    }

    /// Deployment announces the whole route set, so a log-only indexer never has
    /// to hardcode it.
    function test_deployment_announces_every_route_as_disabled() public {
        uint8[4] memory r = _allFour();
        for (uint256 i = 0; i < 4; ++i) {
            vm.expectEmit(true, false, false, true);
            emit RouteEnabledChanged(r[i], false);
        }
        _freshBridge();
    }

    /// The two gates are independent: clearing the pause opens nothing on its
    /// own. This is the property that stops an unpause from switching on the
    /// Solana routes by accident.
    function test_unpausing_alone_makes_no_route_live() public {
        (GlcRobinhoodBridge b,) = _freshBridge();
        _setPausedOn(b, false, false);

        assertFalse(b.depositsPaused());
        assertFalse(b.payoutsPaused());

        uint8[4] memory r = _allFour();
        for (uint256 i = 0; i < 4; ++i) {
            assertFalse(b.routeEnabled(r[i]));
            assertFalse(b.isRouteLive(r[i]), "unpause must not enable a route");
        }
    }

    /// And enabling a route while paused opens nothing either. Two independent
    /// authorizations are required to reach a live route, in either order.
    function test_enabling_alone_makes_no_route_live() public {
        (GlcRobinhoodBridge b, MockGlc token_) = _freshBridge();
        _setRouteEnabledOn(b, ROUTE_RHN_TO_GLC, true);

        assertTrue(b.routeEnabled(ROUTE_RHN_TO_GLC));
        assertFalse(b.isRouteLive(ROUTE_RHN_TO_GLC), "still paused");

        token_.mint(alice, RESERVE_SEED);
        vm.startPrank(alice);
        token_.approve(address(b), type(uint256).max);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        b.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
        vm.stopPrank();
    }

    // =================================================================
    // The Solana routes are not activated
    // =================================================================

    /// The state this branch actually ships: Goldcoin routes live, Solana
    /// routes structurally present and switched off.
    function test_solana_routes_are_supported_but_not_activated() public view {
        assertTrue(bridge.routeEnabled(ROUTE_GLC_TO_RHN));
        assertTrue(bridge.routeEnabled(ROUTE_RHN_TO_GLC));
        assertTrue(bridge.isRouteLive(ROUTE_GLC_TO_RHN));
        assertTrue(bridge.isRouteLive(ROUTE_RHN_TO_GLC));

        assertFalse(bridge.routeEnabled(ROUTE_SOL_TO_RHN));
        assertFalse(bridge.routeEnabled(ROUTE_RHN_TO_SOL));
        assertFalse(bridge.isRouteLive(ROUTE_SOL_TO_RHN));
        assertFalse(bridge.isRouteLive(ROUTE_RHN_TO_SOL));
    }

    function test_disabled_solana_deposit_is_refused() public {
        vm.prank(alice);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.RouteDisabled.selector, ROUTE_RHN_TO_SOL)
        );
        bridge.deposit(ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());
    }

    function test_disabled_solana_payout_is_refused() public {
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_SOL_TO_RHN,
            requestId: keccak256("sol-payout"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.RouteDisabled.selector, ROUTE_SOL_TO_RHN)
        );
        bridge.executePayout(r, sigs);
    }

    // =================================================================
    // Per-route governance is independent
    // =================================================================

    function test_enabling_one_route_leaves_the_others_untouched() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);

        assertTrue(bridge.routeEnabled(ROUTE_RHN_TO_SOL));
        assertFalse(bridge.routeEnabled(ROUTE_SOL_TO_RHN), "sibling route must stay off");
        assertTrue(bridge.routeEnabled(ROUTE_GLC_TO_RHN));
        assertTrue(bridge.routeEnabled(ROUTE_RHN_TO_GLC));
    }

    function test_disabling_one_route_leaves_the_others_untouched() public {
        _setRouteEnabled(ROUTE_RHN_TO_GLC, false);

        assertFalse(bridge.routeEnabled(ROUTE_RHN_TO_GLC));
        assertTrue(bridge.routeEnabled(ROUTE_GLC_TO_RHN), "the other direction stays open");

        // The disabled inbound route is closed...
        vm.prank(alice);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.RouteDisabled.selector, ROUTE_RHN_TO_GLC)
        );
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());

        // ...while the outbound one still pays.
        _payout(keccak256("still-paying"), bob, 500 * ONE_GLC);
    }

    function test_route_enablement_emits() public {
        vm.expectEmit(true, false, false, true);
        emit RouteEnabledChanged(ROUTE_SOL_TO_RHN, true);
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);
    }

    /// The payload hash covers the route, so approving one route's enablement
    /// authorizes nothing about any other.
    function test_authorization_for_one_route_cannot_enable_another() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_SOL_TO_RHN, true)),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);

        // Same signatures, different route.
        vm.expectRevert();
        bridge.setRouteEnabled(ROUTE_RHN_TO_SOL, true, nonce, FAR_FUTURE, sigs);

        assertFalse(bridge.routeEnabled(ROUTE_RHN_TO_SOL));
        assertFalse(bridge.routeEnabled(ROUTE_SOL_TO_RHN));
    }

    /// And it covers the target state, so an approval to enable is not an
    /// approval to disable.
    function test_authorization_to_enable_cannot_disable() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_RHN_TO_GLC, true)),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);

        vm.expectRevert();
        bridge.setRouteEnabled(ROUTE_RHN_TO_GLC, false, nonce, FAR_FUTURE, sigs);

        assertTrue(bridge.routeEnabled(ROUTE_RHN_TO_GLC));
    }

    function test_route_enablement_requires_quorum() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_SOL_TO_RHN, true)),
            nonce,
            FAR_FUTURE
        );

        bytes[] memory rogueSigs = _quorum(h, pkA, pkRogue);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, rogueSigs);

        bytes[] memory one = new bytes[](1);
        one[0] = _sign(pkA, h);
        vm.expectRevert(GlcRobinhoodBridge.InvalidSignatureCount.selector);
        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, one);

        bytes[] memory dup = _quorum(h, pkA, pkA);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.DuplicateSignerSignature.selector, signerA)
        );
        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, dup);

        assertFalse(bridge.routeEnabled(ROUTE_SOL_TO_RHN));
    }

    function test_route_enablement_consumes_the_nonce_exactly_once() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_SOL_TO_RHN, true)),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);

        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, sigs);
        assertEq(bridge.governanceNonce(), nonce + 1);

        vm.expectRevert(
            abi.encodeWithSelector(
                GlcRobinhoodBridge.InvalidGovernanceNonce.selector, nonce + 1, nonce
            )
        );
        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, sigs);
    }

    /// A typo'd route must not burn a governance slot: the signers would have to
    /// re-sign everything queued behind it.
    function test_unknown_route_rejected_without_consuming_the_nonce() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(uint8(0x05), true)),
            nonce,
            FAR_FUTURE
        );

        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnknownRoute.selector, uint8(0x05))
        );
        bridge.setRouteEnabled(0x05, true, nonce, FAR_FUTURE, sigs);

        assertEq(bridge.governanceNonce(), nonce, "nonce must not be consumed");
    }

    /// A guardian's only power is to pause. It can never turn a route on.
    function test_guardian_cannot_enable_a_route() public {
        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_SOL_TO_RHN, true)),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = new bytes[](2);
        sigs[0] = _sign(pkRogue, h);
        sigs[1] = _sign(pkA, h);

        vm.prank(guardian1);
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnauthorizedSigner.selector, rogue)
        );
        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, sigs);

        assertFalse(bridge.routeEnabled(ROUTE_SOL_TO_RHN));
    }

    // =================================================================
    // Pause strictly overrides enablement
    // =================================================================

    /// With both gates shut the operator is told about the PAUSE, because that
    /// is the one a guardian just asserted and the one that has to be cleared.
    function test_pause_is_reported_before_route_disablement() public {
        _setRouteEnabled(ROUTE_RHN_TO_GLC, false);
        _pauseBothRoutes();

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("paused-payout"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.PayoutsPaused.selector);
        bridge.executePayout(r, sigs);
    }

    /// One guardian, one call, and every route in that direction is shut —
    /// including routes enabled after the pause was asserted.
    function test_guardian_pause_closes_every_route_in_the_direction() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);

        vm.prank(guardian1);
        bridge.guardianPause(true, false);

        assertFalse(bridge.isRouteLive(ROUTE_RHN_TO_GLC));
        assertFalse(bridge.isRouteLive(ROUTE_RHN_TO_SOL));
        // The outbound direction is untouched.
        assertTrue(bridge.isRouteLive(ROUTE_GLC_TO_RHN));

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());
    }

    /// Enabling a route during a pause does not lift the pause, so a compromised
    /// signer quorum cannot use route governance to undo a guardian's action
    /// without also passing the separate unpause authorization.
    function test_enabling_a_route_cannot_override_a_guardian_pause() public {
        vm.prank(guardian2);
        bridge.guardianPause(true, true);

        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        assertTrue(bridge.routeEnabled(ROUTE_RHN_TO_SOL));
        assertFalse(bridge.isRouteLive(ROUTE_RHN_TO_SOL));

        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.DepositsPaused.selector);
        bridge.deposit(ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());
    }

    // =================================================================
    // Deposits bind their destination route
    // =================================================================

    function test_deposit_records_and_emits_its_route() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);

        uint256 amount = 500 * ONE_GLC;
        vm.expectEmit(true, true, true, true);
        emit DepositCreated(0, alice, ROUTE_RHN_TO_SOL, amount, amount / SCALE, _solDestination());
        uint256 idx = _depositOn(alice, ROUTE_RHN_TO_SOL, amount, _solDestination());

        GlcRobinhoodBridge.Obligation memory ob = bridge.obligation(idx);
        assertEq(ob.route, ROUTE_RHN_TO_SOL);
        assertEq(ob.depositor, alice);
        assertEq(ob.amount, amount);
    }

    /// Two deposits identical in every other respect stay distinguishable,
    /// which is the whole reason the route is an argument.
    function test_deposits_on_different_routes_are_distinguishable() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);

        uint256 toGlc = _depositOn(alice, ROUTE_RHN_TO_GLC, INBOUND_MIN, _destination());
        uint256 toSol = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _destination());

        assertEq(bridge.obligation(toGlc).route, ROUTE_RHN_TO_GLC);
        assertEq(bridge.obligation(toSol).route, ROUTE_RHN_TO_SOL);
        assertEq(bridge.obligation(toGlc).amount, bridge.obligation(toSol).amount);
    }

    function test_deposit_rejects_a_payout_route() public {
        uint8[2] memory payoutRoutes = [ROUTE_GLC_TO_RHN, ROUTE_SOL_TO_RHN];
        for (uint256 i = 0; i < payoutRoutes.length; ++i) {
            vm.prank(alice);
            vm.expectRevert(
                abi.encodeWithSelector(
                    GlcRobinhoodBridge.NotADepositRoute.selector, payoutRoutes[i]
                )
            );
            bridge.deposit(payoutRoutes[i], INBOUND_MIN, _destination());
        }
    }

    function test_deposit_rejects_an_unknown_route() public {
        uint8[3] memory bad = [uint8(0x00), uint8(0x05), type(uint8).max];
        for (uint256 i = 0; i < bad.length; ++i) {
            vm.prank(alice);
            vm.expectRevert(
                abi.encodeWithSelector(GlcRobinhoodBridge.UnknownRoute.selector, bad[i])
            );
            bridge.deposit(bad[i], INBOUND_MIN, _destination());
        }
    }

    // =================================================================
    // Payouts bind their source route in EIP-712
    // =================================================================

    function test_payout_on_an_enabled_solana_route() public {
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        uint256 before = glc.balanceOf(bob);
        _payoutOn(ROUTE_SOL_TO_RHN, keccak256("sol-payout"), bob, 500 * ONE_GLC);
        assertEq(glc.balanceOf(bob), before + 500 * ONE_GLC);
    }

    /// THE cross-route replay property. Everything about these two payouts is
    /// identical except the route, and a quorum that approved the Goldcoin one
    /// has not approved the Solana one.
    function test_payout_signature_cannot_be_replayed_onto_another_route() public {
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        GlcRobinhoodBridge.PayoutRequest memory approved = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_GLC_TO_RHN,
            requestId: keccak256("cross-route"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(approved));

        // A genuinely separate struct: `memory a = b` would alias, and mutating
        // it would silently rewrite the request that is supposed to stay valid.
        GlcRobinhoodBridge.PayoutRequest memory swapped = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_SOL_TO_RHN,
            requestId: approved.requestId,
            recipient: approved.recipient,
            amount: approved.amount,
            signerEpoch: approved.signerEpoch,
            expiry: approved.expiry
        });

        uint256 before = glc.balanceOf(bob);
        vm.expectRevert();
        bridge.executePayout(swapped, sigs);
        assertEq(glc.balanceOf(bob), before, "no tokens may move");

        // The originally-approved route still works, so the failure above is
        // the route binding and not a broken signature.
        bridge.executePayout(approved, sigs);
        assertEq(glc.balanceOf(bob), before + 500 * ONE_GLC);
    }

    /// The reverse direction of the same property: a Solana-route authorization
    /// cannot be spent as a Goldcoin one.
    function test_solana_payout_signature_cannot_be_replayed_as_goldcoin() public {
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        GlcRobinhoodBridge.PayoutRequest memory req = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_SOL_TO_RHN,
            requestId: keccak256("sol-only"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(req));

        req.route = ROUTE_GLC_TO_RHN;
        vm.expectRevert();
        bridge.executePayout(req, sigs);
    }

    /// A payout hash built over the WRONG chain pair for its route does not
    /// verify, so the chain ids are genuinely bound and not merely decorative.
    function test_payout_hash_must_carry_the_routes_own_chain_pair() public {
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        GlcRobinhoodBridge.PayoutRequest memory req = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_SOL_TO_RHN,
            requestId: keccak256("wrong-legs"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });

        // Correct route byte, but Goldcoin's chain id as the source.
        bytes32 wrong = keccak256(
            abi.encode(
                bridge.PAYOUT_TYPEHASH(),
                bridge.ACTION_PAYOUT(),
                req.route,
                PROTOCOL_GOLDCOIN,
                PROTOCOL_ROBINHOOD,
                address(glc),
                req.requestId,
                req.recipient,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );

        bytes[] memory wrongSigs = _quorumAB(wrong);
        vm.expectRevert();
        bridge.executePayout(req, wrongSigs);
    }

    function test_payout_rejects_a_deposit_route() public {
        uint8[2] memory depositRoutes = [ROUTE_RHN_TO_GLC, ROUTE_RHN_TO_SOL];
        for (uint256 i = 0; i < depositRoutes.length; ++i) {
            GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
                route: depositRoutes[i],
                requestId: keccak256(abi.encode("wrong-direction", i)),
                recipient: bob,
                amount: 500 * ONE_GLC,
                signerEpoch: 0,
                expiry: FAR_FUTURE
            });
            bytes[] memory sigs = _quorumAB(_payoutHash(r));
            vm.expectRevert(
                abi.encodeWithSelector(
                    GlcRobinhoodBridge.NotAPayoutRoute.selector, depositRoutes[i]
                )
            );
            bridge.executePayout(r, sigs);
        }
    }

    function test_payout_rejects_an_unknown_route() public {
        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: 0x05,
            requestId: keccak256("unknown-route-payout"),
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        vm.expectRevert(
            abi.encodeWithSelector(GlcRobinhoodBridge.UnknownRoute.selector, uint8(0x05))
        );
        bridge.executePayout(r, new bytes[](2));
    }

    /// The replay guard is keyed on `(action, requestId)` and is deliberately
    /// NOT per-route: one request id is spent once, whichever route claimed it.
    function test_request_id_replay_guard_is_shared_across_routes() public {
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        bytes32 id = keccak256("shared-id");
        _payoutOn(ROUTE_GLC_TO_RHN, id, bob, 500 * ONE_GLC);

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_SOL_TO_RHN,
            requestId: id,
            recipient: bob,
            amount: 500 * ONE_GLC,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.RequestAlreadyExecuted.selector);
        bridge.executePayout(r, sigs);
    }

    // =================================================================
    // Obligation-closing paths bind the obligation's route
    // =================================================================

    function test_settlement_binds_the_obligations_route() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);
        uint256 idx = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());

        GlcRobinhoodBridge.SettlementRequest memory req = GlcRobinhoodBridge.SettlementRequest({
            requestId: keccak256("settle-sol"),
            obligationIndex: idx,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });

        // A hash built as though the deposit had been destined for Goldcoin.
        bytes32 wrong = keccak256(
            abi.encode(
                bridge.SETTLEMENT_TYPEHASH(),
                bridge.ACTION_SETTLE(),
                ROUTE_RHN_TO_GLC,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_GOLDCOIN,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );
        bytes[] memory wrongSigs = _quorumAB(wrong);
        bytes[] memory rightSigs = _quorumAB(_settlementHash(req));
        vm.expectRevert();
        bridge.executeSettlement(req, wrongSigs);

        // The obligation's own route settles it.
        bridge.executeSettlement(req, rightSigs);
        assertTrue(bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Settled);
    }

    function test_refund_binds_the_obligations_route() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);
        uint256 idx = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());

        GlcRobinhoodBridge.RefundRequest memory req = GlcRobinhoodBridge.RefundRequest({
            requestId: keccak256("refund-sol"),
            obligationIndex: idx,
            recipient: alice,
            amount: INBOUND_MIN,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });

        bytes32 wrong = keccak256(
            abi.encode(
                bridge.REFUND_TYPEHASH(),
                bridge.ACTION_REFUND(),
                ROUTE_RHN_TO_GLC,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_GOLDCOIN,
                address(glc),
                req.requestId,
                req.obligationIndex,
                req.recipient,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
        bytes[] memory wrongSigs = _quorumAB(wrong);
        bytes[] memory rightSigs = _quorumAB(_refundHash(req));
        vm.expectRevert();
        bridge.executeRefund(req, wrongSigs);

        bridge.executeRefund(req, rightSigs);
        assertTrue(bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Refunded);
    }

    function test_abandonment_binds_the_obligations_route() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);
        uint256 idx = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());

        GlcRobinhoodBridge.AbandonmentRequest memory req = GlcRobinhoodBridge.AbandonmentRequest({
            requestId: keccak256("abandon-sol"),
            obligationIndex: idx,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });

        bytes32 wrong = keccak256(
            abi.encode(
                bridge.ABANDONMENT_TYPEHASH(),
                bridge.ACTION_ABANDON(),
                ROUTE_RHN_TO_GLC,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_GOLDCOIN,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );
        bytes[] memory wrongSigs = _quorumAB(wrong);
        bytes[] memory rightSigs = _quorumAB(_abandonmentHash(req));
        vm.expectRevert();
        bridge.executeAbandonment(req, wrongSigs);

        bridge.executeAbandonment(req, rightSigs);
        assertTrue(bridge.obligationStatus(idx) == GlcRobinhoodBridge.ObligationStatus.Abandoned);
    }

    /// Turning a route OFF must never strand the deposits made while it was on.
    /// Route enablement gates the creation of flow, not the closing of it.
    function test_disabling_a_route_does_not_strand_its_obligations() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);
        uint256 toRefund = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());
        uint256 toSettle = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());
        uint256 toAbandon = _depositOn(alice, ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());

        _setRouteEnabled(ROUTE_RHN_TO_SOL, false);
        _pauseBothRoutes();
        assertFalse(bridge.routeEnabled(ROUTE_RHN_TO_SOL));

        uint256 aliceBefore = glc.balanceOf(alice);
        _refund(keccak256("r"), toRefund);
        assertEq(glc.balanceOf(alice), aliceBefore + INBOUND_MIN);

        _settle(keccak256("s"), toSettle);
        _abandon(keccak256("a"), toAbandon);

        assertEq(bridge.outstandingRefundableCount(), 0);
        assertEq(bridge.outstandingRefundablePrincipal(), 0);
    }

    // =================================================================
    // Limits and windows are shared per direction
    // =================================================================

    /// The inbound rolling budget is one bucket for both inbound routes, so
    /// enabling a second route cannot double the exposure the first was sized
    /// against.
    function test_inbound_rolling_window_is_shared_across_routes() public {
        _setRouteEnabled(ROUTE_RHN_TO_SOL, true);

        uint256 chunk = INBOUND_MAX;
        uint256 fullChunks = INBOUND_ROLLING / chunk;
        for (uint256 i = 0; i < fullChunks; ++i) {
            _depositOn(alice, ROUTE_RHN_TO_GLC, chunk, _destination());
        }

        // The budget is spent. The OTHER inbound route gets no fresh allowance.
        vm.prank(alice);
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.deposit(ROUTE_RHN_TO_SOL, INBOUND_MIN, _solDestination());
    }

    function test_outbound_rolling_window_is_shared_across_routes() public {
        _setRouteEnabled(ROUTE_SOL_TO_RHN, true);

        uint256 chunk = OUTBOUND_MAX;
        uint256 fullChunks = OUTBOUND_ROLLING / chunk;
        for (uint256 i = 0; i < fullChunks; ++i) {
            _payoutOn(ROUTE_GLC_TO_RHN, keccak256(abi.encode("out", i)), bob, chunk);
        }

        GlcRobinhoodBridge.PayoutRequest memory r = GlcRobinhoodBridge.PayoutRequest({
            route: ROUTE_SOL_TO_RHN,
            requestId: keccak256("over-budget"),
            recipient: bob,
            amount: OUTBOUND_MIN,
            signerEpoch: 0,
            expiry: FAR_FUTURE
        });
        bytes[] memory sigs = _quorumAB(_payoutHash(r));
        vm.expectRevert(GlcRobinhoodBridge.ExceedsRollingLimit.selector);
        bridge.executePayout(r, sigs);
    }

    // =================================================================
    // Interaction with migration
    // =================================================================

    /// A committed migration is terminal for the routes: nothing may be turned
    /// back on, but turning things OFF stays available.
    function test_committed_migration_blocks_enabling_but_allows_disabling() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));

        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_SOL_TO_RHN, true)),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.MigrationAlreadyCommitted.selector);
        bridge.setRouteEnabled(ROUTE_SOL_TO_RHN, true, nonce, FAR_FUTURE, sigs);

        _setRouteEnabled(ROUTE_RHN_TO_GLC, false);
        assertFalse(bridge.routeEnabled(ROUTE_RHN_TO_GLC));
    }

    function test_after_migration_no_route_is_live_and_none_can_be_set() public {
        _pauseBothRoutes();
        _commitMigration(address(_deployConformingSuccessor()));
        vm.warp(block.timestamp + bridge.MIGRATION_DELAY());
        _finalizeMigration();

        uint8[4] memory r = _allFour();
        for (uint256 i = 0; i < 4; ++i) {
            assertFalse(bridge.isRouteLive(r[i]));
        }

        uint256 nonce = bridge.governanceNonce();
        bytes32 h = _governanceHash(
            bridge.ACTION_SET_ROUTE_ENABLED(),
            keccak256(abi.encode(ROUTE_RHN_TO_GLC, false)),
            nonce,
            FAR_FUTURE
        );
        bytes[] memory sigs = _quorumAB(h);
        vm.expectRevert(GlcRobinhoodBridge.AlreadyMigrated.selector);
        bridge.setRouteEnabled(ROUTE_RHN_TO_GLC, false, nonce, FAR_FUTURE, sigs);
    }
}
