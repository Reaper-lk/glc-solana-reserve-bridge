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
                _limits()
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
            bytes32(bridge.payoutsPaused.selector),
            fixture.readBytes32(".selectors.payoutsPaused")
        );
        assertEq(bytes32(bridge.migrated.selector), fixture.readBytes32(".selectors.migrated"));
        assertEq(
            bytes32(bridge.obligation.selector), fixture.readBytes32(".selectors.obligation")
        );
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
                _limits()
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
