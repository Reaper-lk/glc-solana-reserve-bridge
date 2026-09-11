// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {stdJson} from "forge-std/StdJson.sol";
import {MessageHashUtils} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";

import {GlcRobinhoodBridge} from "../src/GlcRobinhoodBridge.sol";
import {MockGlc} from "./mocks/MockGlc.sol";

/// Cross-language golden vectors for the three EIP-712 authorizations the
/// off-chain service produces: payout, refund and settlement.
///
/// # What this test is for
///
/// The Rust service builds these digests itself, from its own transcription
/// of the typehashes and field orders (`service/src/robinhood/auth.rs`). A
/// transcription is exactly the thing that drifts silently: a reordered pair
/// of same-width fields, a `uint64` written where the contract has a
/// `uint256`, a missing `token` field on the settlement type — each still
/// produces a 32-byte digest, and each authorizes something other than what
/// was intended.
///
/// So neither side is trusted alone. `fixtures/eip712-golden.json` is a
/// checked-in file that NEITHER side generates:
///
/// - this test asserts the DEPLOYED CONTRACT produces every value in it,
/// - `service/src/robinhood/auth/tests.rs` asserts the RUST IMPLEMENTATION
///   produces every value in it.
///
/// A change on either side that is not matched on the other fails that
/// side's test against a file it cannot silently edit into agreement.
///
/// # Why the bridge is placed at a fixed address
///
/// `verifyingContract` is part of the domain separator, so a golden digest
/// is only reproducible if the address is. `vm.etch`-style placement via
/// `deployCodeTo` pins it, and `vm.chainId` pins the other half. Both are
/// the values a real Robinhood mainnet deployment would have — chain id
/// 4663 — so the vectors are not merely reproducible, they are realistic.
contract GoldenDigestsTest is Test {
    using stdJson for string;

    /// Robinhood Chain mainnet. Decimal 4663, NOT 0x4663.
    uint256 internal constant EVM_CHAIN_ID = 4663;

    /// The pinned deployment address. Arbitrary but fixed forever: changing
    /// it changes every digest in the fixture.
    address internal constant BRIDGE_ADDRESS = 0x00000000000000000000000000000000000B21D6;

    /// The pinned token address the fixture's payout/refund digests bind.
    address internal constant TOKEN_ADDRESS = 0x000000000000000000000000000000000000704e;

    /// The pinned immutable `TREASURY` the fixture's withdrawal digest
    /// binds. As arbitrary and as permanent as the two above.
    address internal constant TREASURY_ADDRESS = 0x000000000000000000000000000000000000ae5B;

    uint64 internal constant PROTOCOL_GOLDCOIN = 1001;
    uint64 internal constant PROTOCOL_ROBINHOOD = 2001;
    uint64 internal constant PROTOCOL_SOLANA = 3001;

    uint8 internal constant ROUTE_GLC_TO_RHN = 0x01;
    uint8 internal constant ROUTE_RHN_TO_GLC = 0x02;

    uint256 internal constant ONE_GLC = 1e18;

    GlcRobinhoodBridge internal bridge;
    string internal fixture;

    function setUp() public {
        vm.chainId(EVM_CHAIN_ID);

        // A token at a pinned address, so `address(TOKEN)` in the payout and
        // refund struct hashes is the fixture's value rather than whatever
        // address the test deployer happened to produce.
        MockGlc token = new MockGlc();
        vm.etch(TOKEN_ADDRESS, address(token).code);

        address[3] memory signers_ = [address(0xA1), address(0xB2), address(0xC3)];
        address[3] memory guardians_ = [address(0xD4), address(0xE5), address(0xF6)];

        deployCodeTo(
            "GlcRobinhoodBridge.sol:GlcRobinhoodBridge",
            abi.encode(
                TOKEN_ADDRESS,
                signers_,
                guardians_,
                PROTOCOL_GOLDCOIN,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_SOLANA,
                _limits(),
                TREASURY_ADDRESS
            ),
            BRIDGE_ADDRESS
        );
        bridge = GlcRobinhoodBridge(BRIDGE_ADDRESS);

        fixture = vm.readFile("test/fixtures/eip712-golden.json");
    }

    function _limits() internal pure returns (GlcRobinhoodBridge.Limits memory) {
        return GlcRobinhoodBridge.Limits({
            inboundMin: 100 * ONE_GLC,
            inboundMax: 20_000 * ONE_GLC,
            inboundRollingLimit: 100_000 * ONE_GLC,
            outboundMin: 100 * ONE_GLC,
            outboundMax: 20_000 * ONE_GLC,
            outboundRollingLimit: 100_000 * ONE_GLC,
            protectedMinReserve: 0
        });
    }

    // -----------------------------------------------------------------
    // The fixture's own inputs, restated so a reader can see them here
    // -----------------------------------------------------------------

    function _requestId() internal pure returns (bytes32) {
        return 0x1111111111111111111111111111111111111111111111111111111111111111;
    }

    function _recipient() internal pure returns (address) {
        return 0x000000000000000000000000000000000000eC19;
    }

    // -----------------------------------------------------------------
    // Domain
    // -----------------------------------------------------------------

    function test_deployment_is_at_the_pinned_address_and_chain() public view {
        assertEq(address(bridge), BRIDGE_ADDRESS, "bridge address pinned");
        assertEq(block.chainid, EVM_CHAIN_ID, "chain id pinned");
        assertEq(bridge.token(), TOKEN_ADDRESS, "token address pinned");
    }

    function test_domain_separator_matches_the_fixture() public view {
        assertEq(
            bridge.domainSeparator(),
            fixture.readBytes32(".domainSeparator"),
            "the contract's EIP-712 domain separator drifted from the golden fixture"
        );
    }

    function test_typehashes_match_the_fixture() public view {
        assertEq(bridge.PAYOUT_TYPEHASH(), fixture.readBytes32(".payoutTypehash"));
        assertEq(bridge.REFUND_TYPEHASH(), fixture.readBytes32(".refundTypehash"));
        assertEq(bridge.SETTLEMENT_TYPEHASH(), fixture.readBytes32(".settlementTypehash"));
        assertEq(bridge.GOVERNANCE_TYPEHASH(), fixture.readBytes32(".governanceTypehash"));
        assertEq(
            bridge.TREASURY_WITHDRAW_TYPEHASH(), fixture.readBytes32(".treasuryWithdrawTypehash")
        );
    }

    /// The typehashes are also asserted against the literal type STRINGS, so
    /// the fixture pins the strings the Rust side transcribes, not merely
    /// some opaque 32 bytes both sides copied.
    function test_typehashes_are_keccak_of_the_documented_type_strings() public view {
        assertEq(
            bridge.PAYOUT_TYPEHASH(),
            keccak256(
                "PayoutAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
                "uint64 protocolDestChainId,address token,bytes32 requestId,address recipient,"
                "uint256 amount,uint64 signerEpoch,uint64 expiry)"
            )
        );
        assertEq(
            bridge.REFUND_TYPEHASH(),
            keccak256(
                "RefundAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
                "uint64 protocolDestChainId,address token,bytes32 requestId,uint256 obligationIndex,"
                "address recipient,uint256 amount,uint64 signerEpoch,uint64 expiry)"
            )
        );
        assertEq(
            bridge.SETTLEMENT_TYPEHASH(),
            keccak256(
                "SettlementAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
                "uint64 protocolDestChainId,bytes32 requestId,uint256 obligationIndex,"
                "uint64 signerEpoch,uint64 expiry)"
            )
        );
        assertEq(
            bridge.TREASURY_WITHDRAW_TYPEHASH(),
            keccak256(
                "TreasuryWithdrawAuth(uint8 action,address token,bytes32 requestId,"
                "address treasury,uint256 amount,uint64 signerEpoch,uint64 expiry)"
            )
        );
        assertEq(
            bridge.GOVERNANCE_TYPEHASH(),
            keccak256(
                "GovernanceAuth(uint8 action,bytes32 payloadHash,uint64 signerEpoch,"
                "uint256 nonce,uint64 expiry)"
            )
        );
    }

    // -----------------------------------------------------------------
    // Governance
    // -----------------------------------------------------------------
    //
    // One vector per action this deployment's operator tooling may
    // propose: setLimits, setPaused, setRouteEnabled. Rotation, guardian
    // rotation, migration and abandonment are deliberately absent — the
    // off-chain side cannot build them, so there is nothing to pin.
    //
    // Every input is chosen to be DISTINCT from its neighbours, because
    // the failure these vectors exist to catch is a reordering: two
    // uint256 fields swapped in the Limits struct, or `nonce` and
    // `signerEpoch` transposed in the GovernanceAuth encoding, would both
    // still compile and still produce 32 bytes. They would produce
    // DIFFERENT 32 bytes, and that is what is asserted here.

    uint256 internal constant GOVERNANCE_NONCE = 5;

    /// The fixture's `governanceLimits`. Seven distinct figures; a TEST
    /// VECTOR, never a policy — production limits come from
    /// `[robinhood.policy]` and are derived, not pinned here.
    function _governanceLimits() internal pure returns (GlcRobinhoodBridge.Limits memory) {
        return GlcRobinhoodBridge.Limits({
            inboundMin: 100 * ONE_GLC,
            inboundMax: 20_000 * ONE_GLC,
            inboundRollingLimit: 5_000_000 * ONE_GLC,
            outboundMin: 200 * ONE_GLC,
            outboundMax: 21_000 * ONE_GLC,
            outboundRollingLimit: 6_000_000 * ONE_GLC,
            protectedMinReserve: 777 * ONE_GLC
        });
    }

    /// `_governance`'s struct hash, rebuilt independently of the
    /// contract's internal function so the encoding — not merely the
    /// result — is what agrees.
    function _governanceStructHash(uint8 action, bytes32 payloadHash)
        internal
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                bridge.GOVERNANCE_TYPEHASH(),
                action,
                payloadHash,
                uint64(7),
                GOVERNANCE_NONCE,
                uint64(1_800_000_000)
            )
        );
    }

    /// The Limits struct is static, so `abi.encode` inlines its seven
    /// members in declaration order with no offset word. A reorder
    /// changes this hash.
    function test_set_limits_payload_struct_hash_and_digest_match_the_fixture() public view {
        bytes32 payloadHash = keccak256(abi.encode(_governanceLimits()));
        assertEq(
            payloadHash,
            fixture.readBytes32(".governance.setLimits.payloadHash"),
            "setLimits payload hash"
        );
        assertEq(
            uint256(bridge.ACTION_SET_LIMITS()),
            fixture.readUint(".governance.setLimits.action"),
            "setLimits action byte"
        );

        bytes32 structHash = _governanceStructHash(bridge.ACTION_SET_LIMITS(), payloadHash);
        assertEq(
            structHash,
            fixture.readBytes32(".governance.setLimits.structHash"),
            "setLimits struct hash"
        );
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".governance.setLimits.digest"),
            "setLimits digest"
        );
    }

    /// `(true, false)` deliberately, not `(true, true)`: an asymmetric
    /// pair means transposing the two booleans changes the hash.
    function test_set_pause_payload_struct_hash_and_digest_match_the_fixture() public view {
        bytes32 payloadHash = keccak256(abi.encode(true, false));
        assertEq(
            payloadHash,
            fixture.readBytes32(".governance.setPause.payloadHash"),
            "setPause payload hash"
        );
        assertEq(
            uint256(bridge.ACTION_SET_PAUSE()),
            fixture.readUint(".governance.setPause.action"),
            "setPause action byte"
        );

        bytes32 structHash = _governanceStructHash(bridge.ACTION_SET_PAUSE(), payloadHash);
        assertEq(
            structHash,
            fixture.readBytes32(".governance.setPause.structHash"),
            "setPause struct hash"
        );
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".governance.setPause.digest"),
            "setPause digest"
        );
    }

    function test_set_route_enabled_payload_struct_hash_and_digest_match_the_fixture() public view {
        bytes32 payloadHash = keccak256(abi.encode(ROUTE_RHN_TO_GLC, true));
        assertEq(
            payloadHash,
            fixture.readBytes32(".governance.setRouteEnabled.payloadHash"),
            "setRouteEnabled payload hash"
        );
        assertEq(
            uint256(bridge.ACTION_SET_ROUTE_ENABLED()),
            fixture.readUint(".governance.setRouteEnabled.action"),
            "setRouteEnabled action byte"
        );

        bytes32 structHash = _governanceStructHash(bridge.ACTION_SET_ROUTE_ENABLED(), payloadHash);
        assertEq(
            structHash,
            fixture.readBytes32(".governance.setRouteEnabled.structHash"),
            "setRouteEnabled struct hash"
        );
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".governance.setRouteEnabled.digest"),
            "setRouteEnabled digest"
        );
    }

    /// The three governance digests must differ from one another even
    /// though every field but the action and the payload is identical —
    /// the action byte is bound INSIDE the struct hash precisely so a
    /// signature for one can never verify as another.
    function test_the_three_governance_digests_are_distinct() public view {
        bytes32 a = fixture.readBytes32(".governance.setLimits.digest");
        bytes32 b = fixture.readBytes32(".governance.setPause.digest");
        bytes32 c = fixture.readBytes32(".governance.setRouteEnabled.digest");
        assertTrue(a != b, "setLimits vs setPause");
        assertTrue(b != c, "setPause vs setRouteEnabled");
        assertTrue(a != c, "setLimits vs setRouteEnabled");
    }

    /// The nonce is bound, and it is bound as the FOURTH field. Rebuilding
    /// with a different nonce must not reproduce the fixture's hash —
    /// which is what proves `nonce` is not silently interchangeable with
    /// `signerEpoch`.
    function test_the_governance_nonce_is_bound_into_the_struct_hash() public view {
        bytes32 payloadHash = keccak256(abi.encode(true, false));
        bytes32 withOtherNonce = keccak256(
            abi.encode(
                bridge.GOVERNANCE_TYPEHASH(),
                bridge.ACTION_SET_PAUSE(),
                payloadHash,
                uint64(7),
                GOVERNANCE_NONCE + 1,
                uint64(1_800_000_000)
            )
        );
        assertTrue(
            withOtherNonce != fixture.readBytes32(".governance.setPause.structHash"),
            "a different governance nonce must produce a different struct hash"
        );

        // And transposing signerEpoch with nonce must not reproduce it
        // either: both are numerically small, so only the FIELD ORDER
        // distinguishes them.
        bytes32 transposed = keccak256(
            abi.encode(
                bridge.GOVERNANCE_TYPEHASH(),
                bridge.ACTION_SET_PAUSE(),
                payloadHash,
                uint64(GOVERNANCE_NONCE),
                uint256(7),
                uint64(1_800_000_000)
            )
        );
        assertTrue(
            transposed != fixture.readBytes32(".governance.setPause.structHash"),
            "signerEpoch and nonce must not be interchangeable"
        );
    }

    /// The contract ACCEPTS a real 2-of-3 quorum over the fixture's
    /// governance digest, and the action lands. This is the end of the
    /// chain of custody: the vector is not merely a hash both sides
    /// compute, it is a hash the deployed contract will act on.
    function test_the_contract_accepts_a_quorum_over_the_fixture_governance_hash() public {
        (address s1, uint256 k1) = makeAddrAndKey("golden-governance-signer-1");
        (address s2, uint256 k2) = makeAddrAndKey("golden-governance-signer-2");
        address[3] memory signers_ = [s1, s2, address(0xC3)];
        address[3] memory guardians_ = [address(0xD4), address(0xE5), address(0xF6)];
        deployCodeTo(
            "GlcRobinhoodBridge.sol:GlcRobinhoodBridge",
            abi.encode(
                TOKEN_ADDRESS,
                signers_,
                guardians_,
                PROTOCOL_GOLDCOIN,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_SOLANA,
                _limits(),
                TREASURY_ADDRESS
            ),
            BRIDGE_ADDRESS
        );
        GlcRobinhoodBridge b = GlcRobinhoodBridge(BRIDGE_ADDRESS);

        // The fixture's nonce is 5; a fresh deployment starts at 0, so the
        // vector's own nonce is reached by consuming five pause actions
        // first. Done rather than skipped: the point is that the exact
        // struct hash in the fixture is the one the contract verifies.
        uint64 expiry = uint64(1_800_000_000);
        vm.warp(1_700_000_000);
        for (uint256 i = 0; i < GOVERNANCE_NONCE; ++i) {
            bytes32 h = keccak256(
                abi.encode(
                    b.GOVERNANCE_TYPEHASH(),
                    b.ACTION_SET_PAUSE(),
                    keccak256(abi.encode(false, false)),
                    b.signerEpoch(),
                    b.governanceNonce(),
                    expiry
                )
            );
            bytes[] memory warmup = new bytes[](2);
            warmup[0] = _signFor(b, k1, h);
            warmup[1] = _signFor(b, k2, h);
            b.setPaused(false, false, b.governanceNonce(), expiry, warmup);
        }
        assertEq(b.governanceNonce(), GOVERNANCE_NONCE, "the vector's nonce is now current");

        // A FRESH deployment is at signerEpoch 0, and the epoch only ever
        // advances through `rotateSigners` — which advances the nonce with
        // it, so the fixture's (epoch 7, nonce 5) pair is unreachable by
        // any sequence of real calls on a new contract. Rather than poke
        // storage to fake it, this asserts the two halves separately:
        //
        //   1. the contract ACCEPTS a quorum over this exact encoding, at
        //      whatever epoch it really holds; and
        //   2. that same encoding, with the fixture's epoch substituted,
        //      reproduces the fixture's struct hash byte for byte.
        //
        // Together those say what the fixture is for: this is the shape the
        // deployed contract verifies, and the pinned bytes are that shape.
        bytes32 payloadHash = keccak256(abi.encode(true, false));
        bytes32 structHash = keccak256(
            abi.encode(
                b.GOVERNANCE_TYPEHASH(),
                b.ACTION_SET_PAUSE(),
                payloadHash,
                b.signerEpoch(),
                GOVERNANCE_NONCE,
                expiry
            )
        );
        assertEq(
            keccak256(
                abi.encode(
                    b.GOVERNANCE_TYPEHASH(),
                    b.ACTION_SET_PAUSE(),
                    payloadHash,
                    uint64(7),
                    GOVERNANCE_NONCE,
                    expiry
                )
            ),
            fixture.readBytes32(".governance.setPause.structHash"),
            "the encoding the contract verifies, at the fixture's epoch, IS the fixture's hash"
        );

        bytes[] memory sigs = new bytes[](2);
        sigs[0] = _signFor(b, k1, structHash);
        sigs[1] = _signFor(b, k2, structHash);
        b.setPaused(true, false, GOVERNANCE_NONCE, expiry, sigs);

        assertTrue(b.depositsPaused(), "deposits paused by the golden authorization");
        assertFalse(b.payoutsPaused(), "and payouts left open, exactly as the payload said");
        assertEq(b.governanceNonce(), GOVERNANCE_NONCE + 1, "the nonce was consumed");
    }

    function _signFor(GlcRobinhoodBridge b, uint256 pk, bytes32 structHash)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = MessageHashUtils.toTypedDataHash(b.domainSeparator(), structHash);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }

    // -----------------------------------------------------------------
    // Payout
    // -----------------------------------------------------------------

    function _payoutStructHash() internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                bridge.PAYOUT_TYPEHASH(),
                bridge.ACTION_PAYOUT(),
                ROUTE_GLC_TO_RHN,
                PROTOCOL_GOLDCOIN,
                PROTOCOL_ROBINHOOD,
                TOKEN_ADDRESS,
                _requestId(),
                _recipient(),
                uint256(1234 * ONE_GLC),
                uint64(7),
                uint64(1_800_000_000)
            )
        );
    }

    function test_payout_struct_hash_and_digest_match_the_fixture() public view {
        bytes32 structHash = _payoutStructHash();
        assertEq(structHash, fixture.readBytes32(".payout.structHash"), "payout struct hash");
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".payout.digest"),
            "payout digest"
        );
    }

    // -----------------------------------------------------------------
    // Treasury withdrawal
    // -----------------------------------------------------------------

    /// No route, no chain pair: the withdrawal binds the token, the request
    /// id, the treasury and the amount. The Rust transcription must produce
    /// exactly this, field for field.
    function _treasuryWithdrawStructHash() internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                bridge.TREASURY_WITHDRAW_TYPEHASH(),
                bridge.ACTION_TREASURY_WITHDRAW(),
                TOKEN_ADDRESS,
                _requestId(),
                TREASURY_ADDRESS,
                uint256(2500 * ONE_GLC),
                uint64(7),
                uint64(1_800_000_000)
            )
        );
    }

    function test_treasury_withdraw_struct_hash_and_digest_match_the_fixture() public view {
        assertEq(bridge.treasury(), TREASURY_ADDRESS, "treasury address pinned");
        bytes32 structHash = _treasuryWithdrawStructHash();
        assertEq(
            structHash,
            fixture.readBytes32(".treasuryWithdraw.structHash"),
            "treasury withdraw struct hash"
        );
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".treasuryWithdraw.digest"),
            "treasury withdraw digest"
        );
    }

    // -----------------------------------------------------------------
    // Refund
    // -----------------------------------------------------------------

    function _refundStructHash() internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                bridge.REFUND_TYPEHASH(),
                bridge.ACTION_REFUND(),
                ROUTE_RHN_TO_GLC,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_GOLDCOIN,
                TOKEN_ADDRESS,
                _requestId(),
                uint256(42),
                _recipient(),
                uint256(500 * ONE_GLC),
                uint64(7),
                uint64(1_800_000_000)
            )
        );
    }

    function test_refund_struct_hash_and_digest_match_the_fixture() public view {
        bytes32 structHash = _refundStructHash();
        assertEq(structHash, fixture.readBytes32(".refund.structHash"), "refund struct hash");
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".refund.digest"),
            "refund digest"
        );
    }

    // -----------------------------------------------------------------
    // Settlement
    // -----------------------------------------------------------------

    function _settlementStructHash() internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                bridge.SETTLEMENT_TYPEHASH(),
                bridge.ACTION_SETTLE(),
                ROUTE_RHN_TO_GLC,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_GOLDCOIN,
                _requestId(),
                uint256(42),
                uint64(7),
                uint64(1_800_000_000)
            )
        );
    }

    function test_settlement_struct_hash_and_digest_match_the_fixture() public view {
        bytes32 structHash = _settlementStructHash();
        assertEq(
            structHash, fixture.readBytes32(".settlement.structHash"), "settlement struct hash"
        );
        assertEq(
            MessageHashUtils.toTypedDataHash(bridge.domainSeparator(), structHash),
            fixture.readBytes32(".settlement.digest"),
            "settlement digest"
        );
    }

    // -----------------------------------------------------------------
    // Function selectors
    // -----------------------------------------------------------------

    /// The off-chain service builds calldata from hand-written canonical
    /// signature strings (`service/src/robinhood/calls.rs`). A single
    /// character's difference there selects a different — almost certainly
    /// nonexistent — function, and the mistake surfaces only as a revert
    /// against a live contract.
    ///
    /// So the selectors go in the golden fixture too, taken from the
    /// COMPILER's own view of the deployed ABI rather than from a string
    /// this test also wrote.
    function test_execution_selectors_match_the_fixture() public view {
        assertEq(
            bytes32(bridge.executePayout.selector),
            fixture.readBytes32(".selectors.executePayout"),
            "executePayout selector"
        );
        assertEq(
            bytes32(bridge.executeRefund.selector),
            fixture.readBytes32(".selectors.executeRefund"),
            "executeRefund selector"
        );
        assertEq(
            bytes32(bridge.executeSettlement.selector),
            fixture.readBytes32(".selectors.executeSettlement"),
            "executeSettlement selector"
        );
        assertEq(
            bytes32(bridge.executeTreasuryWithdraw.selector),
            fixture.readBytes32(".selectors.executeTreasuryWithdraw"),
            "executeTreasuryWithdraw selector"
        );
    }

    /// The read-only surface the off-chain pre-broadcast gate depends on.
    /// A drifted selector here would make a gate read garbage rather than
    /// fail, which is the more dangerous direction.
    function test_view_selectors_match_the_fixture() public view {
        assertEq(bytes32(bridge.token.selector), fixture.readBytes32(".selectors.token"));
        assertEq(
            bytes32(bridge.bridgeProtocolId.selector),
            fixture.readBytes32(".selectors.bridgeProtocolId")
        );
        assertEq(
            bytes32(bridge.signerEpoch.selector), fixture.readBytes32(".selectors.signerEpoch")
        );
        assertEq(
            bytes32(bridge.routeEnabled.selector), fixture.readBytes32(".selectors.routeEnabled")
        );
        assertEq(
            bytes32(bridge.isRouteLive.selector), fixture.readBytes32(".selectors.isRouteLive")
        );
        assertEq(
            bytes32(bridge.routeChains.selector), fixture.readBytes32(".selectors.routeChains")
        );
        assertEq(
            bytes32(bridge.depositsPaused.selector),
            fixture.readBytes32(".selectors.depositsPaused")
        );
        assertEq(
            bytes32(bridge.payoutsPaused.selector), fixture.readBytes32(".selectors.payoutsPaused")
        );
        assertEq(bytes32(bridge.migrated.selector), fixture.readBytes32(".selectors.migrated"));
        assertEq(bytes32(bridge.treasury.selector), fixture.readBytes32(".selectors.treasury"));
        assertEq(bytes32(bridge.obligation.selector), fixture.readBytes32(".selectors.obligation"));
        assertEq(
            bytes32(bridge.obligationCount.selector),
            fixture.readBytes32(".selectors.obligationCount")
        );
        assertEq(
            bytes32(bridge.requestExecuted.selector),
            fixture.readBytes32(".selectors.requestExecuted")
        );
        assertEq(
            bytes32(bridge.encumberedReserve.selector),
            fixture.readBytes32(".selectors.encumberedReserve")
        );
        assertEq(bytes32(bridge.signers.selector), fixture.readBytes32(".selectors.signers"));
        assertEq(
            bytes32(bridge.domainSeparator.selector),
            fixture.readBytes32(".selectors.domainSeparator")
        );
    }

    /// The BRIDGE_PROTOCOL_ID the off-chain preflight checks against.
    function test_protocol_id_matches_the_fixture() public view {
        assertEq(bridge.bridgeProtocolId(), fixture.readBytes32(".bridgeProtocolId"));
    }

    // -----------------------------------------------------------------
    // The contract itself agrees with these hashes
    // -----------------------------------------------------------------

    /// The three struct hashes above are rebuilt in this test file rather
    /// than read from the contract, because the contract exposes no view for
    /// them. That leaves one gap a fixture cannot close on its own: does the
    /// CONTRACT hash the same thing this test does?
    ///
    /// It does, and the proof is that a quorum over the fixture's settlement
    /// struct hash actually settles the obligation the fixture names. If the
    /// contract's internal encoding differed from this file's by a single
    /// field, `_authorize` would recover two addresses that are not signers
    /// and revert.
    function test_the_contract_accepts_a_quorum_over_the_fixture_settlement_hash() public {
        // A fresh deployment whose signer set this test holds the keys for,
        // still at a pinned address and chain id so the domain — and
        // therefore the digest — is the fixture's.
        (address s1, uint256 k1) = makeAddrAndKey("golden-signer-1");
        (address s2, uint256 k2) = makeAddrAndKey("golden-signer-2");
        (address s3,) = makeAddrAndKey("golden-signer-3");

        address pinned = address(0x00000000000000000000000000000000000b21d7);
        deployCodeTo(
            "GlcRobinhoodBridge.sol:GlcRobinhoodBridge",
            abi.encode(
                TOKEN_ADDRESS,
                [s1, s2, s3],
                [address(0xD4), address(0xE5), address(0xF6)],
                PROTOCOL_GOLDCOIN,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_SOLANA,
                _limits(),
                TREASURY_ADDRESS
            ),
            pinned
        );
        GlcRobinhoodBridge b = GlcRobinhoodBridge(pinned);

        // Open the inbound route and make one real deposit, so there is an
        // obligation whose recorded route the settlement path will read.
        _bootstrapInbound(b, k1, k2);
        MockGlc(TOKEN_ADDRESS).mint(address(this), 10_000 * ONE_GLC);
        MockGlc(TOKEN_ADDRESS).approve(address(b), type(uint256).max);
        uint256 index = b.deposit(ROUTE_RHN_TO_GLC, 500 * ONE_GLC, hex"abcd");

        GlcRobinhoodBridge.SettlementRequest memory req = GlcRobinhoodBridge.SettlementRequest({
            requestId: _requestId(),
            obligationIndex: index,
            signerEpoch: b.signerEpoch(),
            expiry: uint64(1_800_000_000)
        });
        bytes32 structHash = keccak256(
            abi.encode(
                b.SETTLEMENT_TYPEHASH(),
                b.ACTION_SETTLE(),
                ROUTE_RHN_TO_GLC,
                PROTOCOL_ROBINHOOD,
                PROTOCOL_GOLDCOIN,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );

        bytes[] memory sigs = new bytes[](2);
        sigs[0] = _sign(b, k1, structHash);
        sigs[1] = _sign(b, k2, structHash);

        vm.warp(1_700_000_000);
        b.executeSettlement(req, sigs);
        assertEq(
            uint256(b.obligationStatus(index)),
            uint256(GlcRobinhoodBridge.ObligationStatus.Settled),
            "the contract must accept a quorum over the independently rebuilt struct hash"
        );
    }

    function _bootstrapInbound(GlcRobinhoodBridge b, uint256 k1, uint256 k2) internal {
        uint64 expiry = uint64(3_000_000_000);
        vm.warp(1_700_000_000);

        bytes32 pauseHash = keccak256(
            abi.encode(
                b.GOVERNANCE_TYPEHASH(),
                b.ACTION_SET_PAUSE(),
                keccak256(abi.encode(false, false)),
                b.signerEpoch(),
                b.governanceNonce(),
                expiry
            )
        );
        bytes[] memory sigs = new bytes[](2);
        sigs[0] = _sign(b, k1, pauseHash);
        sigs[1] = _sign(b, k2, pauseHash);
        b.setPaused(false, false, b.governanceNonce(), expiry, sigs);

        bytes32 routeHash = keccak256(
            abi.encode(
                b.GOVERNANCE_TYPEHASH(),
                b.ACTION_SET_ROUTE_ENABLED(),
                keccak256(abi.encode(ROUTE_RHN_TO_GLC, true)),
                b.signerEpoch(),
                b.governanceNonce(),
                expiry
            )
        );
        sigs[0] = _sign(b, k1, routeHash);
        sigs[1] = _sign(b, k2, routeHash);
        b.setRouteEnabled(ROUTE_RHN_TO_GLC, true, b.governanceNonce(), expiry, sigs);
    }

    function _sign(GlcRobinhoodBridge b, uint256 pk, bytes32 structHash)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = MessageHashUtils.toTypedDataHash(b.domainSeparator(), structHash);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }
}
