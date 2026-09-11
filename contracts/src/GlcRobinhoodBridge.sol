// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {SafeCast} from "@openzeppelin/contracts/utils/math/SafeCast.sol";

import {IGlcReserveBridgeSuccessor} from "./interfaces/IGlcReserveBridgeSuccessor.sol";

/// @title GlcRobinhoodBridge — reserve custody for the GLC L1 <-> Robinhood route.
/// @notice Holds and moves an EXISTING GLC ERC-20 reserve. This is not a token
///         contract: it cannot mint, cannot burn, and cannot create value. Every
///         payout is backed by GLC someone already put here.
///
/// # Trust model
///
/// There is no owner, no admin, no proxy and no upgrade path. Every privileged
/// operation is a 2-of-3 EIP-712 authorization from a fixed signer set, and the
/// only unilateral power any single key has is the guardians' ability to PAUSE —
/// never to unpause, never to move a token.
///
/// # Routes
///
/// Robinhood is one leg of every route this contract serves; the other leg is
/// either Goldcoin L1 or Solana. Four routes exist, and each names both legs
/// and a direction:
///
/// - `RhnToGlc` (inbound):  a user deposits Robinhood GLC here; the off-chain
///   bridge pays the canonical amount out on Goldcoin L1.
/// - `RhnToSol` (inbound):  the same, paid out on Solana instead.
/// - `GlcToRhn` (outbound): a Goldcoin L1 deposit is observed off-chain;
///   signers authorize a payout of GLC held here to the Robinhood recipient.
/// - `SolToRhn` (outbound): the same, sourced from Solana instead.
///
/// A route is always BOUND, never inferred. An inbound deposit names the route
/// it is destined for at the moment the depositor's transfer lands; that route
/// is stored on the obligation and bound into every later authorization about
/// it. An outbound payout names its route in the EIP-712 payload, so the
/// signed message states which network the payout settles against — a
/// `SolToRhn` signature can never execute as `GlcToRhn` even if the recipient,
/// amount and request id were identical.
///
/// # Route enablement vs. pause
///
/// TWO independent gates stand in front of every transfer, and both must be
/// open:
///
/// - the per-route enable flag, settable only by 2-of-3 signer governance and
///   only one route at a time. Every route is DISABLED at deployment.
/// - the direction pause, which any single guardian can assert unilaterally
///   and which no guardian can clear. It is route-agnostic on purpose: an
///   emergency pause closes every route in that direction at once, and no
///   amount of route enablement can reopen it.
///
/// The pause is checked FIRST, so it strictly overrides route enablement.
///
/// Only the Goldcoin routes are intended for activation at the time of
/// writing. `SolToRhn` and `RhnToSol` are structurally supported, deploy
/// disabled exactly like every other route, and stay that way until an
/// explicit governance authorization says otherwise. Nothing in this contract
/// treats them as special: "not yet enabled" is ordinary state, not a
/// half-built feature.
///
/// # Decimals
///
/// Robinhood GLC has 18 decimals; the bridge's canonical ledger unit has 8.
/// Every amount this contract accepts must therefore be an exact multiple of
/// `CANONICAL_SCALE` (1e10). Nothing rounds or truncates: an amount that cannot
/// be represented exactly in the canonical ledger is refused, because accepting
/// it would create an obligation the off-chain ledger cannot express.
contract GlcRobinhoodBridge is IGlcReserveBridgeSuccessor, EIP712, ReentrancyGuard {
    using SafeERC20 for IERC20;
    using SafeCast for uint256;

    // ---------------------------------------------------------------------
    // Constants
    // ---------------------------------------------------------------------

    /// EIP-712 domain name/version. Changing either invalidates every
    /// outstanding signature and is a protocol-version event.
    string private constant EIP712_NAME = "GlcRobinhoodBridge";
    string private constant EIP712_VERSION = "1";

    /// `10^(18 - 8)`: the exact factor between one Robinhood atomic unit and
    /// one canonical (8-decimal) ledger unit. Mirrors the off-chain
    /// `CANONICAL_TO_ROBINHOOD_SCALE`; the two must never diverge.
    uint256 public constant CANONICAL_SCALE = 1e10;

    /// The token precision this contract is written for. Asserted against the
    /// live `decimals()` at construction rather than assumed: a different value
    /// does not mean "scale differently", it means this is not the asset this
    /// code models, and deployment must fail.
    uint8 public constant EXPECTED_TOKEN_DECIMALS = 18;

    /// Fixed signer-set size and quorum. Both are constants, not configuration:
    /// a quorum that can be reconfigured is a quorum that can be configured to 1.
    uint256 public constant SIGNER_COUNT = 3;
    uint256 public constant SIGNER_THRESHOLD = 2;
    uint256 public constant GUARDIAN_COUNT = 3;

    /// Maximum length of the opaque destination payload, mirroring the Solana
    /// program's `MAX_GLC_ADDRESS_LEN`. One bound for every inbound route: a
    /// Goldcoin Base58Check address and a Solana pubkey (32 raw bytes, 44 as
    /// base58) both fit well inside it, and a per-route bound would be a second
    /// address-format opinion held on-chain. See `deposit` for why this
    /// contract does not parse the payload at all.
    uint256 public constant MAX_DESTINATION_LEN = 64;

    /// Rolling-limit bucket width. A FIXED-BUCKET window, not a true sliding
    /// one — the same deliberate simplification the Solana program documents in
    /// `limits.rs`. See `_consumeWindow` for the tradeoff.
    uint64 public constant ROLLING_WINDOW_SECONDS = 24 hours;

    /// Mandatory delay between committing a migration successor and being able
    /// to finalize it. Not shortenable by any code path.
    uint64 public constant MIGRATION_DELAY = 48 hours;

    /// Names the protocol FAMILY, not this version. A successor is expected to
    /// return this same value from `bridgeProtocolId()`.
    bytes32 public constant BRIDGE_PROTOCOL_ID = keccak256("glc.reserve-bridge.robinhood");

    // ---------------------------------------------------------------------
    // Action discriminators
    // ---------------------------------------------------------------------
    //
    // One shared space, mirroring `shared/src/claim.rs`'s discipline: a value
    // names exactly one action and is never reused. `0x00` is permanently
    // invalid, so a zeroed word can never be mistaken for an authorization.
    // The action byte is bound into every struct hash, so a signature valid for
    // one action can never verify as another even if every other field matched.

    uint8 public constant ACTION_PAYOUT = 0x01;
    uint8 public constant ACTION_REFUND = 0x02;
    uint8 public constant ACTION_SETTLE = 0x03;
    uint8 public constant ACTION_SET_PAUSE = 0x04;
    uint8 public constant ACTION_ROTATE_SIGNERS = 0x05;
    uint8 public constant ACTION_ROTATE_GUARDIANS = 0x06;
    uint8 public constant ACTION_SET_LIMITS = 0x07;
    uint8 public constant ACTION_COMMIT_MIGRATION = 0x08;
    uint8 public constant ACTION_FINALIZE_MIGRATION = 0x09;
    uint8 public constant ACTION_ABANDON = 0x0A;
    uint8 public constant ACTION_SET_ROUTE_ENABLED = 0x0B;
    /// Operator-initiated reserve withdrawal to the ONE treasury fixed at
    /// construction. The EVM counterpart of the Solana program's
    /// `treasury_withdraw` — see `executeTreasuryWithdraw`.
    uint8 public constant ACTION_TREASURY_WITHDRAW = 0x0C;

    // ---------------------------------------------------------------------
    // Route discriminators
    // ---------------------------------------------------------------------
    //
    // A SEPARATE space from the action bytes above, and it must stay separate:
    // a route says which pair of networks an operation moves value between,
    // an action says what the operation does to this contract's state. Both
    // are bound into every struct hash independently, so neither can stand in
    // for the other.
    //
    // As with actions, `0x00` is permanently invalid and values are never
    // reused or reordered — each is a wire value an off-chain decoder matches
    // on. The name reads source-to-destination: `ROUTE_RHN_TO_GLC` moves value
    // from Robinhood to Goldcoin, so from this contract's perspective it is a
    // DEPOSIT (GLC arrives here) and `ROUTE_GLC_TO_RHN` is a PAYOUT.

    uint8 public constant ROUTE_GLC_TO_RHN = 0x01;
    uint8 public constant ROUTE_RHN_TO_GLC = 0x02;
    uint8 public constant ROUTE_SOL_TO_RHN = 0x03;
    uint8 public constant ROUTE_RHN_TO_SOL = 0x04;

    /// Number of routes this contract knows. Not a bound on anything dynamic —
    /// the route set is fixed at compile time, and this exists so deployment
    /// can announce every one of them.
    uint256 public constant ROUTE_COUNT = 4;

    // ---------------------------------------------------------------------
    // Typehashes
    // ---------------------------------------------------------------------

    /// Every authorization carries BOTH the route byte and the pair of
    /// protocol chain ids that route resolves to. The pair is redundant given
    /// the byte, and deliberately so: the byte is what the contract switches
    /// on, while the ids are what a signer reviewing the payload actually
    /// recognizes. Binding only the byte would make every route's payload
    /// differ by one opaque number; binding both means a signature is
    /// self-describing and a route added later cannot silently collide with an
    /// existing one's digest.
    bytes32 public constant PAYOUT_TYPEHASH = keccak256(
        "PayoutAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
        "uint64 protocolDestChainId,address token,bytes32 requestId,address recipient,"
        "uint256 amount,uint64 signerEpoch,uint64 expiry)"
    );

    bytes32 public constant REFUND_TYPEHASH = keccak256(
        "RefundAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
        "uint64 protocolDestChainId,address token,bytes32 requestId,uint256 obligationIndex,"
        "address recipient,uint256 amount,uint64 signerEpoch,uint64 expiry)"
    );

    bytes32 public constant SETTLEMENT_TYPEHASH = keccak256(
        "SettlementAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
        "uint64 protocolDestChainId,bytes32 requestId,uint256 obligationIndex,uint64 signerEpoch,"
        "uint64 expiry)"
    );

    /// Deliberately a DIFFERENT type name from `SettlementAuth`, not just a
    /// different action byte, even though the field list is identical. Two
    /// independent separations now stand between "this deposit reached its
    /// Goldcoin destination" and "this deposit never will": the EIP-712 type
    /// name feeds the typehash, and the action byte is bound inside the struct.
    /// A signature for one can never verify as the other.
    bytes32 public constant ABANDONMENT_TYPEHASH = keccak256(
        "AbandonmentAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,"
        "uint64 protocolDestChainId,bytes32 requestId,uint256 obligationIndex,uint64 signerEpoch,"
        "uint64 expiry)"
    );

    /// Generic governance authorization. `payloadHash` is the keccak256 of the
    /// exact proposed change, so approving one payload can never authorize a
    /// different one under the same action.
    bytes32 public constant GOVERNANCE_TYPEHASH = keccak256(
        "GovernanceAuth(uint8 action,bytes32 payloadHash,uint64 signerEpoch,uint256 nonce,"
        "uint64 expiry)"
    );

    /// A treasury withdrawal binds NO route and NO protocol chain pair, and
    /// this is not an omission: it is not a movement between two networks,
    /// it is a movement out of this reserve to its operator's own treasury
    /// on this same network. Binding a route would make the payload claim
    /// a leg it does not have. What it binds instead is the treasury
    /// address itself, so a quorum signs the destination it can see and the
    /// contract compares that to the one it was constructed with — the
    /// same pairing the Solana program makes between the signed claim and
    /// its on-chain `RebalancePolicy`.
    bytes32 public constant TREASURY_WITHDRAW_TYPEHASH = keccak256(
        "TreasuryWithdrawAuth(uint8 action,address token,bytes32 requestId,address treasury,"
        "uint256 amount,uint64 signerEpoch,uint64 expiry)"
    );

    // ---------------------------------------------------------------------
    // Types
    // ---------------------------------------------------------------------

    /// Lifecycle of one inbound deposit obligation.
    ///
    /// `None` is the zero value, so an unwritten slot can never read as a live
    /// obligation. `Settled`, `Refunded` and `Abandoned` are all terminal and
    /// mutually exclusive: an obligation leaves `Pending` exactly once, and
    /// those three exits are the only ones that exist.
    ///
    /// `Abandoned` exists because `Settled` must never be used as a dumping
    /// ground. If an on-chain refund is permanently impossible -- a token-level
    /// blocklist on the depositor is the motivating case -- the obligation
    /// still has to be closed so migration can proceed, but recording it as
    /// `Settled` would assert a Goldcoin payout that never happened. That is a
    /// false entry in the one record an auditor relies on. `Abandoned` says
    /// what actually occurred: the principal stayed in bridge custody and the
    /// obligation was closed administratively.
    ///
    /// New variants are appended, never reordered: the numeric value of each
    /// status is a wire value that off-chain decoders match on.
    enum ObligationStatus {
        None,
        Pending,
        Settled,
        Refunded,
        Abandoned
    }

    /// The minimum deposit metadata needed to enforce the refund invariant and
    /// to re-derive the deposit's own route, and nothing else. The destination
    /// payload is NOT stored: it is needed only by the off-chain indexer, which
    /// reads it from the event.
    ///
    /// `route` is stored even though the destination payload is not, because
    /// the two answer different questions. The payload is an address the
    /// indexer pays out to and this contract never interprets; the route is a
    /// consensus-relevant fact that must be bound into every subsequent
    /// authorization about this obligation, and a value only recoverable from
    /// an event log is not a value the contract can bind. It is written once,
    /// when the depositor's transfer lands, and no code path mutates it —
    /// exactly like `depositor`.
    ///
    /// `depositor`, `status` and `route` share a single storage slot (22 of 32
    /// bytes), so recording the route costs no additional storage.
    struct Obligation {
        address depositor;
        ObligationStatus status;
        uint8 route;
        uint256 amount;
    }

    /// Fixed-bucket rolling-volume accumulator. One per DIRECTION, not per
    /// route: the two inbound routes share the inbound bucket and the two
    /// outbound routes share the outbound one.
    ///
    /// That sharing is deliberate. These limits exist to bound how fast the
    /// single, shared reserve can move, and a per-route budget would make the
    /// real ceiling the SUM of the configured limits rather than the number
    /// operators actually approved — enabling a second route would silently
    /// double the exposure the first one was sized against. A shared bucket
    /// means the stated limit is the true limit no matter how many routes are
    /// live. The two directions remain entirely independent of each other.
    struct Window {
        uint64 windowStart;
        uint256 total;
    }

    /// All configurable policy. Every field is in Robinhood atomic units
    /// (18 decimals) except where noted. Per-direction for the same reason the
    /// windows are: these bound movement of one shared reserve.
    struct Limits {
        uint256 inboundMin;
        uint256 inboundMax;
        uint256 inboundRollingLimit;
        uint256 outboundMin;
        uint256 outboundMax;
        uint256 outboundRollingLimit;
        uint256 protectedMinReserve;
    }

    /// A payout has no stored obligation to read a route from — the deposit it
    /// settles happened on another chain entirely — so the route is named by
    /// the signers and bound into the authorization.
    struct PayoutRequest {
        uint8 route;
        bytes32 requestId;
        address recipient;
        uint256 amount;
        uint64 signerEpoch;
        uint64 expiry;
    }

    /// `treasury` is carried on the wire so the signed payload NAMES its
    /// destination, but it is not a choice: `executeTreasuryWithdraw`
    /// reverts unless it equals the immutable `TREASURY`. It exists so a
    /// signer reviewing the fields sees an address, not an implication.
    struct TreasuryWithdrawRequest {
        bytes32 requestId;
        address treasury;
        uint256 amount;
        uint64 signerEpoch;
        uint64 expiry;
    }

    /// No `route` field, here or on the settlement and abandonment requests.
    /// The route of an obligation is not a signer's choice: it was fixed when
    /// the depositor's transfer landed, exactly like `depositor`. These paths
    /// read it from storage and bind THAT value, so a signature produced for
    /// the wrong route simply fails to verify. A caller-supplied field would
    /// only raise the question of what happens when it disagrees with storage,
    /// and the answer would always have to be "storage wins" — which makes the
    /// field decoration. `recipient` and `amount` differ: they are caller-
    /// supplied because they are the transfer's own parameters, and they are
    /// checked against the obligation precisely because of that.
    struct RefundRequest {
        bytes32 requestId;
        uint256 obligationIndex;
        address recipient;
        uint256 amount;
        uint64 signerEpoch;
        uint64 expiry;
    }

    struct SettlementRequest {
        bytes32 requestId;
        uint256 obligationIndex;
        uint64 signerEpoch;
        uint64 expiry;
    }

    struct AbandonmentRequest {
        bytes32 requestId;
        uint256 obligationIndex;
        uint64 signerEpoch;
        uint64 expiry;
    }

    // ---------------------------------------------------------------------
    // Immutables
    // ---------------------------------------------------------------------

    /// The GLC ERC-20 this contract custodies. Constructor-bound so a testnet
    /// deployment can point at a mock; deliberately NOT a hardcoded mainnet
    /// address.
    IERC20 public immutable TOKEN;

    /// The bridge's own NAMESPACED protocol chain identifiers — network
    /// qualified, and deliberately NOT the raw EIP-155 chain id (Robinhood
    /// mainnet's 4663). The off-chain ledger owns the mapping between the two;
    /// binding these into every authorization means a signature minted for the
    /// testnet route cannot be replayed against the mainnet route even in the
    /// impossible case that both shared a signer set and an EVM chain id.
    ///
    /// All three legs are supplied at construction and are immutable, including
    /// Solana's — which is configured even though no Solana route is enabled.
    /// Configuring a leg is not activating it: what makes a route live is its
    /// enable flag, and every one of those starts false. The alternative, a
    /// governance-settable chain id, would put a value that every signature
    /// depends on inside the mutable surface of a contract whose entire trust
    /// story is that it has no admin.
    uint64 public immutable PROTOCOL_CHAIN_GOLDCOIN;
    uint64 public immutable PROTOCOL_CHAIN_ROBINHOOD;
    uint64 public immutable PROTOCOL_CHAIN_SOLANA;

    /// The ONE address a reserve withdrawal may ever pay. Immutable, not
    /// storage, not governance-settable: this is the Robinhood counterpart
    /// of the Solana program's `RebalancePolicy` allowlist, and it is the
    /// stronger form of it. The Solana list can be changed by a threshold
    /// of keys behind a timelock; this cannot be changed by any set of
    /// keys at all. An attacker holding every signer credential can still
    /// only move reserve GLC to the operator's own treasury — loud,
    /// bounded, and reversible in a way an anonymous address is not.
    ///
    /// Rotating the treasury therefore means migrating to a successor
    /// contract, which is the existing, deliberately heavy path for every
    /// change of custody this contract does not model. That is consistent
    /// with "no owner, no admin, no upgrade path" rather than an exception
    /// to it.
    ///
    /// `address(0)` is permitted at construction and means "no withdrawal
    /// capability, ever": `executeTreasuryWithdraw` reverts
    /// `TreasuryNotConfigured`. A deployment that does not want this path
    /// declines it by construction rather than by never using it.
    address public immutable TREASURY;

    // ---------------------------------------------------------------------
    // Storage
    // ---------------------------------------------------------------------

    address[SIGNER_COUNT] private _signers;
    mapping(address => bool) private _isSigner;
    uint64 public signerEpoch;

    address[GUARDIAN_COUNT] private _guardians;
    mapping(address => bool) private _isGuardian;

    /// Direction-level emergency pause. Route-agnostic on purpose: see the
    /// contract header. A guardian can set either of these unilaterally.
    bool public depositsPaused;
    bool public payoutsPaused;

    /// Per-route enablement, keyed by route discriminator. Every route reads
    /// `false` until a 2-of-3 governance authorization says otherwise, so the
    /// zero value IS the safe value and a freshly deployed contract has no
    /// live route at all. Never read directly by a transfer path — go through
    /// `_requireRouteOpen`, which checks the pause first.
    mapping(uint8 => bool) private _routeEnabled;

    uint256 public governanceNonce;

    Limits private _limits;

    Window private _inboundWindow;
    Window private _outboundWindow;

    mapping(uint256 => Obligation) private _obligations;
    uint256 public obligationCount;

    /// Count and total principal of obligations still in `Pending`. These are
    /// the contract's live refund liability: GLC that is physically here but is
    /// not the bridge's to pay out or migrate.
    uint256 public outstandingRefundableCount;
    uint256 public outstandingRefundablePrincipal;

    /// Replay guard for every request-id-bearing authorization, keyed by
    /// `(action, requestId)` so a request id used under one action can never
    /// collide with the same id under another.
    mapping(bytes32 => bool) private _executedRequest;

    address public migrationSuccessor;
    uint64 public migrationCommittedAt;
    bool public migrationCommitted;
    bool public migrated;

    // ---------------------------------------------------------------------
    // Events
    // ---------------------------------------------------------------------

    /// The off-chain indexer's source of truth for an inbound deposit. Durable
    /// global identity is `(chain, address(this), obligationIndex)`; the index
    /// itself is contract-local and starts at zero.
    ///
    /// `route` is indexed because it is the field an indexer filters on: it
    /// says which network `destination` is an address ON, and therefore which
    /// payout leg owns this deposit. Without it the payload is unattributable
    /// bytes.
    event DepositCreated(
        uint256 indexed obligationIndex,
        address indexed depositor,
        uint8 indexed route,
        uint256 amount,
        uint256 canonicalAmount,
        bytes destination
    );

    event PayoutExecuted(
        bytes32 indexed requestId,
        address indexed recipient,
        uint8 indexed route,
        uint256 amount,
        uint64 signerEpoch
    );

    /// One reserve withdrawal, executed. `treasury` is always `TREASURY`
    /// and is emitted anyway, so a log-only reader sees where the GLC went
    /// without having to know the immutable.
    event TreasuryWithdrawExecuted(
        bytes32 indexed requestId, address indexed treasury, uint256 amount, uint64 signerEpoch
    );

    event RefundExecuted(
        uint256 indexed obligationIndex,
        bytes32 indexed requestId,
        address indexed recipient,
        uint256 amount
    );

    event ObligationSettled(uint256 indexed obligationIndex, bytes32 indexed requestId);

    /// An obligation was closed WITHOUT a Goldcoin payout and WITHOUT returning
    /// the principal. Deliberately not named like a settlement and deliberately
    /// carrying the depositor and amount, so an indexer reconstructing history
    /// can tell exactly whose principal the bridge retained and how much.
    event DepositAbandoned(
        uint256 indexed obligationIndex,
        address indexed depositor,
        bytes32 indexed requestId,
        uint256 amount
    );

    event DirectionPaused(bool depositsPaused, bool payoutsPaused, address indexed by);
    event DirectionUnpaused(bool depositsPaused, bool payoutsPaused);

    /// Emitted on every enablement change AND once per route at construction,
    /// so an indexer that only ever reads logs can enumerate the full route set
    /// and its state without a single `eth_call`. One uniform shape, never a
    /// separate "here are the initial routes" variant, so there is exactly one
    /// handler to write and no way for the two to disagree.
    event RouteEnabledChanged(uint8 indexed route, bool enabled);

    event SignerSetChanged(
        uint64 indexed previousEpoch, uint64 indexed newEpoch, address[3] signers
    );
    event GuardianSetChanged(address[3] guardians);
    event LimitsChanged(Limits limits);

    event MigrationCommitted(address indexed successor, uint64 committedAt, uint64 finalizableAt);
    event MigrationFinalized(address indexed successor, uint256 amount);
    event MigrationVetoed(address indexed guardian, address indexed successor);

    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error ZeroAddress();
    error DuplicateProtocolChain();
    error UnknownRoute(uint8 route);
    error RouteDisabled(uint8 route);
    error NotADepositRoute(uint8 route);
    error NotAPayoutRoute(uint8 route);
    error DuplicateSigner();
    error DuplicateGuardian();
    error UnexpectedTokenDecimals(uint8 actual);
    error InvalidAmount();
    error NonCanonicalAmount();
    error DepositsPaused();
    error PayoutsPaused();
    error InvalidDestinationLength();
    error AmountBelowMinimum();
    error AmountAboveMaximum();
    error ExceedsRollingLimit();
    error InexactTransfer(uint256 expected, uint256 received);
    error AuthorizationExpired();
    error InvalidSignerEpoch(uint64 expected, uint64 provided);
    error InvalidSignatureCount();
    error DuplicateSignerSignature(address signer);
    error UnauthorizedSigner(address signer);
    error RequestAlreadyExecuted();
    error ObligationNotFound();
    error ObligationNotPending();
    error InvalidRefundRecipient();
    error InvalidRefundAmount();
    error InvalidGovernanceNonce(uint256 expected, uint256 provided);
    error InsufficientReserve();
    error UnauthorizedGuardian();
    error NothingToPause();
    error InvalidLimits();
    error MigrationAlreadyCommitted();
    error MigrationNotCommitted();
    error MigrationNotReady();
    error MigrationRequiresPause();
    error OutstandingRefundsRemain(uint256 count, uint256 principal);
    error InvalidSuccessor();
    error AlreadyMigrated();
    error NoPendingMigration();
    error MigrationAlreadyFinalized();
    error TreasuryNotConfigured();
    error InvalidTreasury();
    error WrongTreasury(address expected, address provided);
    error WithdrawalRequiresPause();

    // ---------------------------------------------------------------------
    // Construction
    // ---------------------------------------------------------------------

    /// @param token_ the existing GLC ERC-20 this contract will custody.
    /// @param signers_ exactly three distinct, non-zero bridge signers.
    /// @param guardians_ exactly three distinct, non-zero guardians.
    /// @param protocolChainGoldcoin_ namespaced protocol id of the Goldcoin leg.
    /// @param protocolChainRobinhood_ namespaced protocol id of the Robinhood leg.
    /// @param protocolChainSolana_ namespaced protocol id of the Solana leg.
    ///        Required even though no Solana route is enabled at launch; see
    ///        the immutables above for why this is configuration, not
    ///        activation.
    /// @param limits_ initial policy. Validated exactly as a later change is.
    /// @param treasury_ the one address `executeTreasuryWithdraw` may pay, or
    ///        `address(0)` to deploy with no withdrawal capability at all.
    ///        See `TREASURY`.
    ///
    /// Every route launches FAIL-CLOSED, twice over: both directions are
    /// paused AND all four route enable flags are false. Opening any route
    /// therefore takes two independent 2-of-3 authorizations — one to clear the
    /// direction pause, one to enable that specific route — after the reserve,
    /// indexer and backend are demonstrably ready. There is no window in which
    /// a freshly deployed contract is live, and no single governance action
    /// that makes it live.
    constructor(
        IERC20 token_,
        address[SIGNER_COUNT] memory signers_,
        address[GUARDIAN_COUNT] memory guardians_,
        uint64 protocolChainGoldcoin_,
        uint64 protocolChainRobinhood_,
        uint64 protocolChainSolana_,
        Limits memory limits_,
        address treasury_
    ) EIP712(EIP712_NAME, EIP712_VERSION) {
        if (address(token_) == address(0)) revert ZeroAddress();
        // The reserve paying itself is not a withdrawal, and a treasury
        // that is the token contract would burn or strand the transfer
        // depending on the token. Neither is a destination.
        if (treasury_ == address(this) || treasury_ == address(token_)) revert InvalidTreasury();
        if (protocolChainGoldcoin_ == 0 || protocolChainRobinhood_ == 0) revert ZeroAddress();
        if (protocolChainSolana_ == 0) revert ZeroAddress();

        // Pairwise distinct. The route byte already separates every route's
        // digest, so this is not the only thing standing between two routes —
        // it is here because a signed payload must be TRUE, and two legs
        // sharing an id would make `SolToRhn` and `GlcToRhn` authorizations
        // state the same source network while meaning different ones. A
        // reviewer reading the payload would be reading a lie.
        if (
            protocolChainGoldcoin_ == protocolChainRobinhood_
                || protocolChainGoldcoin_ == protocolChainSolana_
                || protocolChainRobinhood_ == protocolChainSolana_
        ) revert DuplicateProtocolChain();

        uint8 decimals_ = IERC20Metadata(address(token_)).decimals();
        if (decimals_ != EXPECTED_TOKEN_DECIMALS) revert UnexpectedTokenDecimals(decimals_);

        TOKEN = token_;
        PROTOCOL_CHAIN_GOLDCOIN = protocolChainGoldcoin_;
        PROTOCOL_CHAIN_ROBINHOOD = protocolChainRobinhood_;
        PROTOCOL_CHAIN_SOLANA = protocolChainSolana_;
        TREASURY = treasury_;

        _installSigners(signers_);
        _installGuardians(guardians_);
        _validateLimits(limits_);
        _limits = limits_;

        depositsPaused = true;
        payoutsPaused = true;

        // Open both rolling buckets at deployment rather than leaving
        // `windowStart` at zero. On any real chain the difference is invisible
        // (a zero start is already older than the window, so the first transfer
        // resets it), but an explicit start makes the first bucket behave
        // identically everywhere instead of depending on how far the chain's
        // clock happens to be from the epoch.
        uint64 startedAt = _now64();
        _inboundWindow.windowStart = startedAt;
        _outboundWindow.windowStart = startedAt;

        emit SignerSetChanged(0, 0, signers_);
        emit GuardianSetChanged(guardians_);
        emit LimitsChanged(limits_);
        emit DirectionPaused(true, true, msg.sender);

        // Announce every route as disabled. No storage is written — `false` is
        // already the value — but a log-only indexer needs the route set to
        // exist in its stream before the first `RouteEnabledChanged` that turns
        // one on, or it has to hardcode the set to interpret that event.
        uint8[ROUTE_COUNT] memory allRoutes = _allRoutes();
        for (uint256 i = 0; i < ROUTE_COUNT; ++i) {
            emit RouteEnabledChanged(allRoutes[i], false);
        }
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    /// @return The ERC-20 reserve token. Part of `IGlcReserveBridgeSuccessor`,
    ///         so a future version of this contract can validate THIS one when
    ///         migrating in the other direction.
    function token() external view override returns (address) {
        return address(TOKEN);
    }

    /// @return The one address a reserve withdrawal may pay; zero when the
    ///         deployment has no withdrawal capability. See `TREASURY`.
    function treasury() external view returns (address) {
        return TREASURY;
    }

    /// @return The protocol family identifier. See `BRIDGE_PROTOCOL_ID`.
    function bridgeProtocolId() external pure override returns (bytes32) {
        return BRIDGE_PROTOCOL_ID;
    }

    function signers() external view returns (address[SIGNER_COUNT] memory) {
        return _signers;
    }

    function guardians() external view returns (address[GUARDIAN_COUNT] memory) {
        return _guardians;
    }

    function isSigner(address account) external view returns (bool) {
        return _isSigner[account];
    }

    function isGuardian(address account) external view returns (bool) {
        return _isGuardian[account];
    }

    function limits() external view returns (Limits memory) {
        return _limits;
    }

    /// @return Whether governance has enabled `route`. Reverts on a route that
    ///         does not exist rather than answering `false` for it, so a caller
    ///         cannot mistake "no such route" for "route is off".
    function routeEnabled(uint8 route) external view returns (bool) {
        _routeLegs(route);
        return _routeEnabled[route];
    }

    /// @return source The protocol chain `route` moves value from.
    /// @return dest The protocol chain `route` moves value to.
    function routeChains(uint8 route) external view returns (uint64 source, uint64 dest) {
        (source, dest,) = _routeLegs(route);
    }

    /// @return Whether `route` brings GLC INTO this reserve (a deposit route)
    ///         rather than paying it out.
    function isDepositRoute(uint8 route) external view returns (bool) {
        (,, bool inbound) = _routeLegs(route);
        return inbound;
    }

    /// @return Whether a transfer on `route` would be accepted right now — that
    ///         is, every gate is open: not migrated, the route is enabled, its
    ///         direction is not paused, and (for a deposit route) no migration
    ///         has been committed. Operators and monitoring should read THIS
    ///         rather than reassembling it from the individual flags, which is
    ///         where the two gates get confused for each other.
    function isRouteLive(uint8 route) external view returns (bool) {
        (,, bool inbound) = _routeLegs(route);
        if (migrated) return false;
        if (!_routeEnabled[route]) return false;
        if (inbound) return !depositsPaused && !migrationCommitted;
        return !payoutsPaused;
    }

    /// @return Every route this contract knows, in discriminator order.
    function routes() external pure returns (uint8[ROUTE_COUNT] memory) {
        return _allRoutes();
    }

    function obligation(uint256 index) external view returns (Obligation memory) {
        if (index >= obligationCount) revert ObligationNotFound();
        return _obligations[index];
    }

    /// @return The terminal (or live) state of one obligation, for indexers
    ///         that want the discriminant without decoding the whole struct.
    ///         `Pending` is the unresolved state; `Settled`, `Refunded` and
    ///         `Abandoned` are terminal and mutually exclusive.
    function obligationStatus(uint256 index) external view returns (ObligationStatus) {
        if (index >= obligationCount) revert ObligationNotFound();
        return _obligations[index].status;
    }

    /// @return Whether this obligation still counts against the refund
    ///         liability that gates `finalizeMigration`.
    function isObligationUnresolved(uint256 index) external view returns (bool) {
        if (index >= obligationCount) revert ObligationNotFound();
        return _obligations[index].status == ObligationStatus.Pending;
    }

    function inboundWindow() external view returns (Window memory) {
        return _inboundWindow;
    }

    function outboundWindow() external view returns (Window memory) {
        return _outboundWindow;
    }

    /// @return Whether `requestId` has already been consumed under `action`.
    function requestExecuted(uint8 action, bytes32 requestId) external view returns (bool) {
        return _executedRequest[_requestKey(action, requestId)];
    }

    /// @return The EIP-712 domain separator, recomputed if the chain forked.
    function domainSeparator() external view returns (bytes32) {
        return _domainSeparatorV4();
    }

    /// The GLC that is physically here but is NOT available for payouts or
    /// migration: the protected floor plus every unsettled depositor's
    /// principal. Exposed because operators need to reason about it directly.
    function encumberedReserve() public view returns (uint256) {
        return _limits.protectedMinReserve + outstandingRefundablePrincipal;
    }

    /// The timestamp at which `finalizeMigration` becomes callable. Zero when no
    /// migration is committed.
    function migrationFinalizableAt() external view returns (uint64) {
        if (!migrationCommitted) return 0;
        return migrationCommittedAt + MIGRATION_DELAY;
    }

    // ---------------------------------------------------------------------
    // Deposit (inbound: Robinhood -> Goldcoin or Solana)
    // ---------------------------------------------------------------------

    /// @notice Deposit Robinhood GLC into the reserve and create an obligation
    ///         for the off-chain bridge to pay out on `route`'s destination
    ///         network.
    /// @param route The inbound route this deposit is destined for:
    ///        `ROUTE_RHN_TO_GLC` or `ROUTE_RHN_TO_SOL`. Naming an outbound
    ///        route, or a route that does not exist, reverts.
    /// @param amount Robinhood atomic units. MUST be an exact multiple of
    ///        `CANONICAL_SCALE`.
    /// @param destination Opaque destination payload on `route`'s destination
    ///        network, 1..64 bytes.
    /// @return index The new contract-local obligation index.
    ///
    /// # Why the route is an explicit argument
    ///
    /// It is the one thing about a deposit that cannot be recovered afterwards.
    /// `destination` is opaque bytes; a Goldcoin address and a Solana address
    /// are both just bytes, and this contract cannot tell them apart without
    /// becoming the address parser it deliberately refuses to be. Leaving the
    /// route implicit would mean the same call created two indistinguishable
    /// obligations whose payouts belong on different networks — the depositor's
    /// funds would be routed by guesswork. Naming it costs one byte and makes
    /// the intent a signed, stored, event-emitted fact.
    ///
    /// # Why `destination` is opaque bytes
    ///
    /// This contract does not parse addresses of any kind, and must not. A
    /// Goldcoin address is Base58Check over a 25-byte payload whose version
    /// bytes are network-specific; a Solana address is a 32-byte ed25519 point
    /// usually written as base58. Implementing either in Solidity would put a
    /// second, independently-drifting decoder in the trust path for no benefit,
    /// since the bridge already validates the destination off-chain before it
    /// pays anything out. The Solana program takes the identical position — its
    /// `glc_address` is an opaque `Vec<u8>` bounded to 64 bytes, never decoded
    /// on-chain — and this mirrors it exactly, bound included. What the bytes
    /// MEAN is fixed by `route`, which is not opaque.
    ///
    /// # Why the balance is measured on both sides of the transfer
    ///
    /// GLC is expected to be a plain fixed-supply token, but this contract must
    /// not create an obligation for tokens it did not actually receive. A
    /// fee-on-transfer or rebasing token would deliver less than `amount`; the
    /// exact-receipt check turns that into a revert instead of a reserve
    /// shortfall that only surfaces when someone tries to withdraw.
    function deposit(uint8 route, uint256 amount, bytes calldata destination)
        external
        nonReentrant
        returns (uint256 index)
    {
        if (migrated) revert AlreadyMigrated();
        // Committing a migration permanently closes EVERY inbound route: the
        // liability set must be finite and closed before the 48-hour window
        // starts, or finalization could never be reached.
        if (migrationCommitted) revert MigrationAlreadyCommitted();

        (,, bool inbound) = _routeLegs(route);
        if (!inbound) revert NotADepositRoute(route);
        _requireRouteOpen(route, inbound);

        uint256 len = destination.length;
        if (len == 0 || len > MAX_DESTINATION_LEN) revert InvalidDestinationLength();

        Limits memory lim = _limits;
        _requireCanonicalAmount(amount);
        if (amount < lim.inboundMin) revert AmountBelowMinimum();
        if (amount > lim.inboundMax) revert AmountAboveMaximum();
        _consumeWindow(_inboundWindow, amount, lim.inboundRollingLimit);

        // Interaction before the obligation is written, because the amount
        // actually received is not knowable until the transfer has happened.
        // `nonReentrant` is what makes this ordering safe: a malicious token
        // cannot re-enter to observe or exploit the half-updated state.
        uint256 balanceBefore = TOKEN.balanceOf(address(this));
        TOKEN.safeTransferFrom(msg.sender, address(this), amount);
        uint256 received = TOKEN.balanceOf(address(this)) - balanceBefore;
        if (received != amount) revert InexactTransfer(amount, received);

        index = obligationCount;
        unchecked {
            obligationCount = index + 1;
            outstandingRefundableCount += 1;
        }
        outstandingRefundablePrincipal += amount;
        _obligations[index] = Obligation({
            depositor: msg.sender, status: ObligationStatus.Pending, route: route, amount: amount
        });

        emit DepositCreated(index, msg.sender, route, amount, amount / CANONICAL_SCALE, destination);
    }

    // ---------------------------------------------------------------------
    // Payout (outbound: Goldcoin or Solana -> Robinhood)
    // ---------------------------------------------------------------------

    /// @notice Pay existing reserve GLC to a Robinhood recipient, against a
    ///         deposit on `req.route`'s SOURCE network that the signers have
    ///         observed off-chain.
    /// @dev No minting: this moves GLC that is already here.
    ///
    /// `req.route` must be an outbound route (`ROUTE_GLC_TO_RHN` or
    /// `ROUTE_SOL_TO_RHN`) and must be enabled. Because the route and its
    /// resolved chain pair are both bound into the struct hash, a quorum that
    /// authorized a Goldcoin-sourced payout has not authorized an identical
    /// Solana-sourced one: the digests differ, so the signature simply does not
    /// verify. The request id replay guard is shared across routes, which is
    /// the correct direction — one id is consumed once, whichever route claimed
    /// it first.
    function executePayout(PayoutRequest calldata req, bytes[] calldata signatures)
        external
        nonReentrant
    {
        if (migrated) revert AlreadyMigrated();

        (uint64 sourceChain, uint64 destChain) = _requireOpenPayoutRoute(req.route);

        if (req.recipient == address(0)) revert ZeroAddress();

        Limits memory lim = _limits;
        _requireCanonicalAmount(req.amount);
        if (req.amount < lim.outboundMin) revert AmountBelowMinimum();
        if (req.amount > lim.outboundMax) revert AmountAboveMaximum();

        _requireSpendableReserve(req.amount, lim.protectedMinReserve);

        _authorize(
            _payoutStructHash(req, sourceChain, destChain), req.signerEpoch, req.expiry, signatures
        );

        // Effects before interaction: the replay guard and the rolling window
        // are committed before a single token moves.
        _consumeRequest(ACTION_PAYOUT, req.requestId);
        _consumeWindow(_outboundWindow, req.amount, lim.outboundRollingLimit);

        TOKEN.safeTransfer(req.recipient, req.amount);
        emit PayoutExecuted(req.requestId, req.recipient, req.route, req.amount, req.signerEpoch);
    }

    // ---------------------------------------------------------------------
    // Treasury withdrawal (operator-initiated, to the immutable TREASURY)
    // ---------------------------------------------------------------------

    /// @notice Move `req.amount` of reserve GLC to the immutable `TREASURY`.
    ///
    /// The EVM counterpart of the Solana program's `treasury_withdraw`, and
    /// the fourth and last way GLC leaves this contract: a payout to a
    /// bridge user, a refund to a depositor, a migration of the whole
    /// balance to a successor, and this — a bounded amount to the one
    /// address fixed at construction. The other three are unchanged.
    ///
    /// # The gates, in the order they are enforced
    ///
    /// 1. not migrated — terminal, nothing moves afterwards.
    /// 2. a treasury is configured — a zero `TREASURY` declines this path
    ///    by construction.
    /// 3. `req.treasury == TREASURY` — the destination is not a choice; the
    ///    field exists so the SIGNED payload names it.
    /// 4. BOTH directions paused. This is the withdrawal's own pause
    ///    requirement, distinct from the payout path's, and it is the same
    ///    pair `commitMigration` demands: with both directions closed no
    ///    obligation can be created or paid while the reserve is being
    ///    moved, and the pause is a separately-logged act a guardian or a
    ///    quorum performed on purpose. It mirrors the Solana instruction's
    ///    "bridge must ALREADY be globally paused" check, which the incident
    ///    review there explicitly declined to relax.
    /// 5. a canonical amount — an exact multiple of `CANONICAL_SCALE`, so
    ///    the off-chain ledger can account for it in its 8-decimal unit.
    /// 6. `_requireSpendableReserve` — the same floor every payout obeys:
    ///    the protected minimum and every unsettled depositor's principal
    ///    stay. An operator withdrawal can no more breach either than a
    ///    bridge settlement can.
    /// 7. 2-of-3 authorization over `TREASURY_WITHDRAW_TYPEHASH`.
    /// 8. `(ACTION_TREASURY_WITHDRAW, requestId)` consumed once.
    ///
    /// # What is deliberately NOT here
    ///
    /// No `outboundMin`/`outboundMax`, no rolling window. Fixing WHERE the
    /// reserve can pay is the bound an attacker has to defeat; capping HOW
    /// MUCH would only constrain legitimate treasury operations, which must
    /// be able to move the whole spendable reserve when custody demands it.
    /// This restates the Solana program's own rationale rather than
    /// inventing a policy of its own, and the protected floor in gate 6
    /// remains the one accounting bound.
    ///
    /// # Exact transfer
    ///
    /// The reserve's balance and the treasury's balance are both measured
    /// on both sides of the transfer, and the call reverts unless exactly
    /// `req.amount` left the one and arrived at the other. `SafeERC20`
    /// already reverts on a failed transfer; this additionally refuses a
    /// token that quietly moved a different amount in either direction, so
    /// the emitted figure is always both the figure that left custody and
    /// the figure the treasury holds.
    function executeTreasuryWithdraw(
        TreasuryWithdrawRequest calldata req,
        bytes[] calldata signatures
    ) external nonReentrant {
        if (migrated) revert AlreadyMigrated();
        if (TREASURY == address(0)) revert TreasuryNotConfigured();
        if (req.treasury != TREASURY) revert WrongTreasury(TREASURY, req.treasury);
        if (!depositsPaused || !payoutsPaused) revert WithdrawalRequiresPause();

        _requireCanonicalAmount(req.amount);
        _requireSpendableReserve(req.amount, _limits.protectedMinReserve);

        _authorize(_treasuryWithdrawStructHash(req), req.signerEpoch, req.expiry, signatures);

        // Effects before interaction.
        _consumeRequest(ACTION_TREASURY_WITHDRAW, req.requestId);

        // Measured on BOTH sides: what left custody, and what the treasury
        // received. The reserve's own delta is the accounting fact; the
        // treasury's is the operator's. A token that quietly diverged on
        // either side would make the emitted amount a lie somewhere.
        uint256 reserveBefore = TOKEN.balanceOf(address(this));
        uint256 treasuryBefore = TOKEN.balanceOf(TREASURY);
        TOKEN.safeTransfer(TREASURY, req.amount);
        uint256 left = reserveBefore - TOKEN.balanceOf(address(this));
        if (left != req.amount) revert InexactTransfer(req.amount, left);
        uint256 received = TOKEN.balanceOf(TREASURY) - treasuryBefore;
        if (received != req.amount) revert InexactTransfer(req.amount, received);

        emit TreasuryWithdrawExecuted(req.requestId, TREASURY, req.amount, req.signerEpoch);
    }

    // ---------------------------------------------------------------------
    // Refund
    // ---------------------------------------------------------------------

    /// @notice Return one specific obligation's deposit to the wallet that made
    ///         it, for a deposit that entered ManualReview and will not settle.
    ///
    /// This is emphatically NOT a withdrawal. The destination is not chosen by
    /// the caller or by the signers: it is DERIVED from the obligation's own
    /// recorded depositor, a value written when that user's transfer landed and
    /// never mutable afterwards. The amount is likewise not chosen — it must
    /// equal the recorded principal exactly. Signers choose WHICH obligation to
    /// refund, and nothing else. The same reasoning the Solana program's
    /// `refund_withdraw` documents, enforced here by explicit comparison.
    ///
    /// Deliberately callable while either or both directions are paused, while
    /// the obligation's own route is DISABLED, and after a migration has been
    /// committed: refunding is how outstanding liability is cleared, and any
    /// gate that blocked it would deadlock migration against the very
    /// obligations it must resolve. Route enablement gates the CREATION of new
    /// flow, never the closing of flow that already exists — otherwise turning
    /// a route off would strand every deposit made while it was on, which is
    /// the exact opposite of what an operator turning it off intends.
    ///
    /// No partial refunds. No on-chain refund fee.
    function executeRefund(RefundRequest calldata req, bytes[] calldata signatures)
        external
        nonReentrant
    {
        if (migrated) revert AlreadyMigrated();
        if (req.obligationIndex >= obligationCount) revert ObligationNotFound();

        Obligation storage ob = _obligations[req.obligationIndex];
        if (ob.status != ObligationStatus.Pending) revert ObligationNotPending();
        if (req.recipient != ob.depositor) revert InvalidRefundRecipient();
        if (req.amount != ob.amount) revert InvalidRefundAmount();

        // The obligation's OWN route, read from storage, never from the caller.
        _authorize(_refundStructHash(req, ob.route), req.signerEpoch, req.expiry, signatures);
        _consumeRequest(ACTION_REFUND, req.requestId);

        ob.status = ObligationStatus.Refunded;
        _releaseLiability(req.amount);

        TOKEN.safeTransfer(req.recipient, req.amount);
        emit RefundExecuted(req.obligationIndex, req.requestId, req.recipient, req.amount);
    }

    // ---------------------------------------------------------------------
    // Settlement
    // ---------------------------------------------------------------------

    /// @notice Record that one obligation was paid out on Goldcoin L1 and is
    ///         therefore no longer refundable.
    ///
    /// Moves no tokens. Mutates no principal and no address. Its ONLY effect is
    /// to move one obligation from `Pending` to `Settled`, releasing its
    /// principal from the encumbered reserve. It is the exact counterpart of the
    /// Solana program's `record_goldcoin_completion`, and it is what makes the
    /// contract's refund liability a finite, closeable set rather than an
    /// unbounded one — which is what lets a terminal migration be safe.
    ///
    /// The authorization binds the obligation's own route, so a settlement
    /// asserts payout on a NAMED network: "this deposit was paid out on
    /// Goldcoin" and "this deposit was paid out on Solana" are different signed
    /// statements, not one ambiguous one. Like refund, it stays available while
    /// paused and while the route is disabled — see `executeRefund`.
    ///
    /// Mutually exclusive with `executeRefund` by construction: both require
    /// `Pending`, and both leave a terminal status.
    function executeSettlement(SettlementRequest calldata req, bytes[] calldata signatures)
        external
        nonReentrant
    {
        if (migrated) revert AlreadyMigrated();
        if (req.obligationIndex >= obligationCount) revert ObligationNotFound();

        Obligation storage ob = _obligations[req.obligationIndex];
        if (ob.status != ObligationStatus.Pending) revert ObligationNotPending();

        (uint64 sourceChain, uint64 destChain,) = _routeLegs(ob.route);

        bytes32 structHash = keccak256(
            abi.encode(
                SETTLEMENT_TYPEHASH,
                ACTION_SETTLE,
                ob.route,
                sourceChain,
                destChain,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );
        _authorize(structHash, req.signerEpoch, req.expiry, signatures);
        _consumeRequest(ACTION_SETTLE, req.requestId);

        ob.status = ObligationStatus.Settled;
        _releaseLiability(ob.amount);

        emit ObligationSettled(req.obligationIndex, req.requestId);
    }

    // ---------------------------------------------------------------------
    // Abandonment
    // ---------------------------------------------------------------------

    /// @notice Close one obligation administratively, WITHOUT claiming it was
    ///         paid out on Goldcoin and WITHOUT returning the principal.
    ///
    /// The motivating case is a reserve token that can refuse a transfer to a
    /// specific address: if the depositor is blocklisted at the token level,
    /// `executeRefund` reverts permanently and the obligation can never be
    /// closed honestly. Without this path the only exits are to leave the
    /// obligation open forever -- which blocks migration -- or to record it as
    /// `Settled`, which asserts a Goldcoin payout that did not happen. Both are
    /// worse than saying plainly that the principal was retained.
    ///
    /// Moves no tokens. Mutates neither the recorded depositor nor the recorded
    /// principal. The GLC stays exactly where it is, in bridge custody, and is
    /// released from the encumbered reserve because the obligation behind it is
    /// closed -- so a later migration carries it across as part of the full
    /// remaining balance.
    ///
    /// This is NOT a withdrawal path: no address is named by the caller, no
    /// amount is named by the caller, and nothing leaves the contract.
    function executeAbandonment(AbandonmentRequest calldata req, bytes[] calldata signatures)
        external
        nonReentrant
    {
        if (migrated) revert AlreadyMigrated();
        if (req.obligationIndex >= obligationCount) revert ObligationNotFound();

        Obligation storage ob = _obligations[req.obligationIndex];
        if (ob.status != ObligationStatus.Pending) revert ObligationNotPending();

        (uint64 sourceChain, uint64 destChain,) = _routeLegs(ob.route);

        bytes32 structHash = keccak256(
            abi.encode(
                ABANDONMENT_TYPEHASH,
                ACTION_ABANDON,
                ob.route,
                sourceChain,
                destChain,
                req.requestId,
                req.obligationIndex,
                req.signerEpoch,
                req.expiry
            )
        );
        _authorize(structHash, req.signerEpoch, req.expiry, signatures);
        _consumeRequest(ACTION_ABANDON, req.requestId);

        ob.status = ObligationStatus.Abandoned;
        _releaseLiability(ob.amount);

        emit DepositAbandoned(req.obligationIndex, ob.depositor, req.requestId, ob.amount);
    }

    // ---------------------------------------------------------------------
    // Pause / unpause
    // ---------------------------------------------------------------------

    /// @notice Any one guardian may pause either or both directions, instantly.
    ///
    /// A guardian can only ever set a flag to `true`. There is no argument to
    /// this function that unpauses anything, so a compromised guardian key is a
    /// denial-of-service risk and nothing more: it cannot move a token, cannot
    /// change configuration, and cannot reopen a route.
    ///
    /// Deliberately DIRECTION-scoped rather than route-scoped. Pausing deposits
    /// closes every inbound route at once, and pausing payouts closes every
    /// outbound one. An emergency responder who has just seen something wrong
    /// should not have to enumerate routes to stop the bleeding, and should not
    /// be able to leave one open by accident. Route-level granularity is a
    /// governance concern, and it lives in `setRouteEnabled` behind a quorum.
    function guardianPause(bool pauseDeposits, bool pausePayouts) external {
        if (!_isGuardian[msg.sender]) revert UnauthorizedGuardian();
        if (!pauseDeposits && !pausePayouts) revert NothingToPause();

        if (pauseDeposits) depositsPaused = true;
        if (pausePayouts) payoutsPaused = true;

        emit DirectionPaused(depositsPaused, payoutsPaused, msg.sender);
    }

    /// @notice Set both direction flags under 2-of-3 signer authorization.
    ///
    /// This is the only path that can clear a pause. A single guardian can
    /// never reach it. Once a migration is committed the routes are closed
    /// permanently, so any attempt to clear a flag afterwards reverts.
    ///
    /// Clearing a pause does NOT enable any route: the two gates are
    /// independent, and a route that governance never enabled stays closed with
    /// both directions open. This is why unpausing cannot accidentally turn on
    /// `SolToRhn` or `RhnToSol`.
    function setPaused(
        bool depositsPaused_,
        bool payoutsPaused_,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) external {
        if (migrated) revert AlreadyMigrated();
        if (migrationCommitted && (!depositsPaused_ || !payoutsPaused_)) {
            revert MigrationAlreadyCommitted();
        }

        _governance(
            ACTION_SET_PAUSE,
            keccak256(abi.encode(depositsPaused_, payoutsPaused_)),
            nonce,
            expiry,
            signatures
        );

        depositsPaused = depositsPaused_;
        payoutsPaused = payoutsPaused_;

        // Exactly ONE event per call, carrying the complete resulting state.
        // Emitting both for a mixed transition (one direction paused, the other
        // open) told an indexer the same thing twice under two different names,
        // which reads as a contradiction even though it is not.
        if (depositsPaused_ || payoutsPaused_) {
            emit DirectionPaused(depositsPaused_, payoutsPaused_, msg.sender);
        } else {
            emit DirectionUnpaused(depositsPaused_, payoutsPaused_);
        }
    }

    // ---------------------------------------------------------------------
    // Route enablement
    // ---------------------------------------------------------------------

    /// @notice Enable or disable ONE route under 2-of-3 signer authorization.
    ///
    /// # Why one route at a time
    ///
    /// The routes are governed independently because they carry independent
    /// risk: `RhnToGlc` and `RhnToSol` differ in which chain's payout
    /// infrastructure has to be correct, and an incident on one is not a reason
    /// to touch the other. A whole-set setter would make every enablement
    /// change re-approve all four, so turning off a broken route would require
    /// signers to re-assert their approval of the three they were not asked
    /// about.
    ///
    /// The usual argument for whole-struct replacement — the one `setLimits`
    /// documents — does not apply here. That argument is about deltas whose
    /// meaning depends on state the signer cannot see. `(route, enabled)` is
    /// absolute, not a delta: it names exactly one route and exactly the state
    /// that route will be in afterwards, and it says nothing about any other.
    /// The payload hash covers both values, so approving "enable `RhnToGlc`"
    /// can never execute as "enable `RhnToSol`" or as a disable.
    ///
    /// # Relationship to the pause
    ///
    /// Enabling a route does not unpause anything, and cannot. A route is live
    /// only when governance has enabled it AND its direction is unpaused; the
    /// transfer paths check the pause first. A guardian who pauses during an
    /// incident cannot be overridden by enabling routes.
    ///
    /// # After a migration is committed
    ///
    /// Enabling is refused, matching `setPaused`'s refusal to unpause: a
    /// committed migration closes the bridge permanently and the obligation set
    /// must stay finite. DISABLING stays available, because it is always a safe
    /// direction to move in and refusing it would be pure obstruction.
    function setRouteEnabled(
        uint8 route,
        bool enabled,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) external {
        if (migrated) revert AlreadyMigrated();
        if (migrationCommitted && enabled) revert MigrationAlreadyCommitted();

        // Validate before spending the nonce, so a typo'd route is a clean
        // revert rather than a consumed governance slot.
        _routeLegs(route);

        _governance(
            ACTION_SET_ROUTE_ENABLED,
            keccak256(abi.encode(route, enabled)),
            nonce,
            expiry,
            signatures
        );

        _routeEnabled[route] = enabled;

        emit RouteEnabledChanged(route, enabled);
    }

    // ---------------------------------------------------------------------
    // Rotation
    // ---------------------------------------------------------------------

    /// @notice Replace the entire signer set under 2-of-3 CURRENT signer
    ///         authorization, and burn the epoch.
    ///
    /// Incrementing `signerEpoch` invalidates every authorization the outgoing
    /// set ever produced, immediately and without needing to enumerate them:
    /// the epoch is bound into every struct hash, so an old signature no longer
    /// hashes to anything this contract will accept.
    function rotateSigners(
        address[SIGNER_COUNT] calldata newSigners,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) external {
        if (migrated) revert AlreadyMigrated();

        _governance(
            ACTION_ROTATE_SIGNERS, keccak256(abi.encode(newSigners)), nonce, expiry, signatures
        );

        uint64 previousEpoch = signerEpoch;
        // Clear before installing, so a new set that reuses an outgoing member
        // is handled correctly rather than leaving a stale authorization bit.
        for (uint256 i = 0; i < SIGNER_COUNT; ++i) {
            _isSigner[_signers[i]] = false;
        }
        _installSigners(newSigners);
        signerEpoch = previousEpoch + 1;

        emit SignerSetChanged(previousEpoch, signerEpoch, newSigners);
    }

    /// @notice Replace the entire guardian set under 2-of-3 signer authorization.
    function rotateGuardians(
        address[GUARDIAN_COUNT] calldata newGuardians,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) external {
        if (migrated) revert AlreadyMigrated();

        _governance(
            ACTION_ROTATE_GUARDIANS, keccak256(abi.encode(newGuardians)), nonce, expiry, signatures
        );

        for (uint256 i = 0; i < GUARDIAN_COUNT; ++i) {
            _isGuardian[_guardians[i]] = false;
        }
        _installGuardians(newGuardians);

        emit GuardianSetChanged(newGuardians);
    }

    // ---------------------------------------------------------------------
    // Limits
    // ---------------------------------------------------------------------

    /// @notice Replace the whole limit set under 2-of-3 signer authorization.
    /// @dev Whole-struct replacement rather than per-field setters: the payload
    ///      hash then covers every value simultaneously, so signers approve a
    ///      complete, unambiguous policy rather than a delta whose meaning
    ///      depends on state they cannot see when signing.
    function setLimits(
        Limits calldata newLimits,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) external {
        if (migrated) revert AlreadyMigrated();

        _governance(ACTION_SET_LIMITS, keccak256(abi.encode(newLimits)), nonce, expiry, signatures);

        _validateLimits(newLimits);
        _limits = newLimits;

        emit LimitsChanged(newLimits);
    }

    // ---------------------------------------------------------------------
    // Migration
    // ---------------------------------------------------------------------

    /// @notice Commit, irreversibly, to migrating the whole reserve to
    ///         `successor` once the delay has elapsed and all liability is clear.
    ///
    /// Requires both directions already paused, so the obligation set is closed
    /// at this instant: no deposit can be created afterwards, which makes the
    /// outstanding refund liability finite and monotonically decreasing. The
    /// 48-hour delay that follows is when operators settle or refund every
    /// remaining obligation — and when a human independently verifies the
    /// successor.
    ///
    /// There is no `cancelMigration`. Committing is terminal for the routes.
    function commitMigration(
        address successor,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) external {
        if (migrated) revert AlreadyMigrated();
        if (migrationCommitted) revert MigrationAlreadyCommitted();
        if (!depositsPaused || !payoutsPaused) revert MigrationRequiresPause();
        if (successor == address(0)) revert ZeroAddress();
        if (successor == address(this) || successor.code.length == 0) revert InvalidSuccessor();

        _governance(
            ACTION_COMMIT_MIGRATION, keccak256(abi.encode(successor)), nonce, expiry, signatures
        );

        // Cheap structural checks that catch the obvious catastrophes. They do
        // not prove the successor is correct and are not intended to: the
        // 48-hour delay and human verification remain the real gate.
        if (IGlcReserveBridgeSuccessor(successor).token() != address(TOKEN)) {
            revert InvalidSuccessor();
        }
        if (IGlcReserveBridgeSuccessor(successor).bridgeProtocolId() != BRIDGE_PROTOCOL_ID) {
            revert InvalidSuccessor();
        }

        migrationSuccessor = successor;
        migrationCommittedAt = _now64();
        migrationCommitted = true;

        emit MigrationCommitted(
            successor, migrationCommittedAt, migrationCommittedAt + MIGRATION_DELAY
        );
    }

    /// @notice Move the ENTIRE remaining reserve to the committed successor and
    ///         render this contract permanently terminal.
    ///
    /// # The liability gate
    ///
    /// This reverts unless BOTH `outstandingRefundableCount` and
    /// `outstandingRefundablePrincipal` are exactly zero — that is, unless every
    /// obligation ever created has reached `Settled` or `Refunded`.
    ///
    /// This resolves the conflict between "migration moves the full balance and
    /// is terminal" and "a ManualReview deposit must be refundable": as
    /// specified without this gate, finalizing would strand the principal of
    /// every unsettled depositor. Blocking instead of stranding is the correct
    /// failure direction, and it never deadlocks, because refunding is always
    /// available (it works while paused and while a migration is committed) and
    /// returns the depositor their own money. Operators therefore always have a
    /// path to zero: settle what settled, refund the rest.
    ///
    /// Neither counter can grow after `commitMigration`, since deposits are
    /// permanently closed at that point. The set only shrinks.
    function finalizeMigration(uint256 nonce, uint64 expiry, bytes[] calldata signatures)
        external
        nonReentrant
    {
        if (migrated) revert AlreadyMigrated();
        if (!migrationCommitted) revert MigrationNotCommitted();
        // A validator can nudge `block.timestamp` by seconds. MIGRATION_DELAY is
        // 48 hours, so that leeway is ~5 orders of magnitude too small to matter,
        // and no cheaper monotonic clock exists on an EVM chain.
        // forge-lint: disable-next-line(block-timestamp)
        if (block.timestamp < migrationCommittedAt + MIGRATION_DELAY) revert MigrationNotReady();
        if (outstandingRefundableCount != 0 || outstandingRefundablePrincipal != 0) {
            revert OutstandingRefundsRemain(
                outstandingRefundableCount, outstandingRefundablePrincipal
            );
        }

        _governance(
            ACTION_FINALIZE_MIGRATION,
            keccak256(abi.encode(migrationSuccessor)),
            nonce,
            expiry,
            signatures
        );

        migrated = true;

        address successor = migrationSuccessor;
        uint256 amount = TOKEN.balanceOf(address(this));
        if (amount != 0) {
            TOKEN.safeTransfer(successor, amount);
        }

        emit MigrationFinalized(successor, amount);
    }

    /// @notice Any ONE current guardian may cancel a PENDING migration.
    ///
    /// # Why this exists
    ///
    /// Migration is the only unbounded transfer in this contract: it moves the
    /// entire reserve in one call, past every per-transfer limit, rolling limit
    /// and reserve floor. Without this function its only guard is the 48-hour
    /// delay, and the guardians -- whose entire purpose is emergency response
    /// -- are spectators to the single highest-impact action there is. A
    /// compromised signer quorum could commit a successor and simply wait.
    ///
    /// # The window
    ///
    /// Open from commitment until `finalizeMigration` actually executes. It
    /// deliberately does NOT close when 48 hours elapse: a right that expires
    /// on a timer is no use against an attacker who can wait for the timer.
    ///
    /// # What a veto does NOT do
    ///
    /// It moves no tokens, touches no signer or guardian set, changes no limit,
    /// unpauses nothing, and cannot name a replacement successor -- the
    /// function takes no arguments at all. Both directions stay paused exactly
    /// as they were, so a veto stops a migration without silently reopening a
    /// bridge that operators had deliberately closed. Returning to normal
    /// service, or committing a different successor, each require a fresh
    /// 2-of-3 signer authorization at the current governance nonce.
    ///
    /// A guardian may veto repeatedly; each re-commitment restarts the full
    /// 48-hour delay. That asymmetry is the point.
    function vetoMigration() external {
        if (!_isGuardian[msg.sender]) revert UnauthorizedGuardian();
        if (migrated) revert MigrationAlreadyFinalized();
        if (!migrationCommitted) revert NoPendingMigration();

        address vetoed = migrationSuccessor;
        migrationSuccessor = address(0);
        migrationCommittedAt = 0;
        migrationCommitted = false;

        emit MigrationVetoed(msg.sender, vetoed);
    }

    // ---------------------------------------------------------------------
    // Internal: routes
    // ---------------------------------------------------------------------

    /// The complete route topology, in ONE place.
    ///
    /// Every route-aware path — deposit, payout, refund, settlement,
    /// abandonment, and all four views — resolves its legs and its direction
    /// through this function, so the four routes cannot drift apart across the
    /// code that uses them. Adding a fifth route means editing exactly this
    /// body and `_allRoutes`.
    ///
    /// Reverts `UnknownRoute` on anything else, including `0x00`. There is no
    /// default arm and no fallthrough: an unrecognized route is never treated
    /// as a plausible one.
    ///
    /// @return source The protocol chain value comes FROM.
    /// @return dest The protocol chain value goes TO.
    /// @return inbound Whether GLC enters this reserve (a deposit route) rather
    ///         than leaving it (a payout route). Returned alongside the legs
    ///         rather than derived by comparing `source` to
    ///         `PROTOCOL_CHAIN_ROBINHOOD`, so the direction is stated by the
    ///         topology instead of inferred from a coincidence of ids.
    // The `inbound` literals below are the table's DATA, not a redundant
    // boolean test — the shape `boolean-cst` exists to catch. Writing the
    // direction out per row keeps every route's full definition on one line and
    // independent of any other invariant. The obvious alternative, deriving it
    // as `source == PROTOCOL_CHAIN_ROBINHOOD`, is true today only because the
    // constructor enforces the three chain ids are distinct; a lookup table
    // that silently depends on a check in another function is worse than four
    // literals.
    // forge-lint: disable-start(boolean-cst)
    function _routeLegs(uint8 route)
        internal
        view
        returns (uint64 source, uint64 dest, bool inbound)
    {
        if (route == ROUTE_GLC_TO_RHN) {
            return (PROTOCOL_CHAIN_GOLDCOIN, PROTOCOL_CHAIN_ROBINHOOD, false);
        }
        if (route == ROUTE_RHN_TO_GLC) {
            return (PROTOCOL_CHAIN_ROBINHOOD, PROTOCOL_CHAIN_GOLDCOIN, true);
        }
        if (route == ROUTE_SOL_TO_RHN) {
            return (PROTOCOL_CHAIN_SOLANA, PROTOCOL_CHAIN_ROBINHOOD, false);
        }
        if (route == ROUTE_RHN_TO_SOL) {
            return (PROTOCOL_CHAIN_ROBINHOOD, PROTOCOL_CHAIN_SOLANA, true);
        }
        revert UnknownRoute(route);
    }
    // forge-lint: disable-end(boolean-cst)

    function _allRoutes() internal pure returns (uint8[ROUTE_COUNT] memory) {
        return [ROUTE_GLC_TO_RHN, ROUTE_RHN_TO_GLC, ROUTE_SOL_TO_RHN, ROUTE_RHN_TO_SOL];
    }

    /// Both gates that stand in front of a transfer, in the order that makes
    /// the override explicit: the guardian-assertable PAUSE is checked before
    /// the governance-set enable flag.
    ///
    /// The order is not cosmetic. It decides which error an operator sees when
    /// both gates are shut, and during an incident the answer must be
    /// `DepositsPaused` / `PayoutsPaused` — the state a guardian just caused
    /// and the state that has to be cleared by quorum — rather than
    /// `RouteDisabled`, which would send them to reason about a flag that is
    /// not what is stopping them.
    ///
    /// @param route the route being used.
    /// @param inbound Pass the value returned by `_routeLegs`, never a literal:
    ///        it is what ties the pause flag checked here to the route actually
    ///        being used.
    function _requireRouteOpen(uint8 route, bool inbound) internal view {
        if (inbound) {
            if (depositsPaused) revert DepositsPaused();
        } else {
            if (payoutsPaused) revert PayoutsPaused();
        }
        if (!_routeEnabled[route]) revert RouteDisabled(route);
    }

    /// The refund struct hash, built in its own frame. Extracted for the same
    /// stack-depth reason as `_payoutStructHash`; see there.
    ///
    /// @param route the OBLIGATION's route, read from storage by the caller.
    ///        Taken as a parameter rather than re-read here so there is exactly
    ///        one load of it per refund and no chance of hashing a different
    ///        value than the one the caller validated against.
    function _refundStructHash(RefundRequest calldata req, uint8 route)
        internal
        view
        returns (bytes32)
    {
        (uint64 source, uint64 dest,) = _routeLegs(route);
        return keccak256(
            abi.encode(
                REFUND_TYPEHASH,
                ACTION_REFUND,
                route,
                source,
                dest,
                address(TOKEN),
                req.requestId,
                req.obligationIndex,
                req.recipient,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    /// The payout struct hash, built in its own frame.
    ///
    /// Extracted for the same reason as `_requireOpenPayoutRoute`: an eleven-
    /// field `abi.encode` alongside the route legs, the limit struct and the
    /// reserve arithmetic does not fit the EVM's addressable stack, and the
    /// alternative — compiling this contract through the IR pipeline — would
    /// change the deployed bytecode's identity for every function, not just
    /// this one. `foundry.toml` pins the codegen path deliberately; a local
    /// refactor is the cheaper answer.
    ///
    /// The legs are PARAMETERS, not recomputed here, so this function cannot
    /// disagree with the validation the caller already performed.
    function _payoutStructHash(PayoutRequest calldata req, uint64 source, uint64 dest)
        internal
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                PAYOUT_TYPEHASH,
                ACTION_PAYOUT,
                req.route,
                source,
                dest,
                address(TOKEN),
                req.requestId,
                req.recipient,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    /// `hashStruct(TreasuryWithdrawAuth)`. `address(TOKEN)` is bound, as in a
    /// payout, so a signature is specific to the asset as well as the
    /// contract; the treasury is bound so the quorum approved the
    /// destination it saw.
    function _treasuryWithdrawStructHash(TreasuryWithdrawRequest calldata req)
        internal
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                TREASURY_WITHDRAW_TYPEHASH,
                ACTION_TREASURY_WITHDRAW,
                address(TOKEN),
                req.requestId,
                req.treasury,
                req.amount,
                req.signerEpoch,
                req.expiry
            )
        );
    }

    /// `_routeLegs` + the payout-direction assertion + both gates, as one call.
    ///
    /// Folded together because `executePayout` is the deepest frame in this
    /// contract, and separate locals for the legs, the direction flag and the
    /// reserve arithmetic overflow the EVM's addressable stack. Keeping the
    /// direction flag in THIS frame rather than the caller's is what buys the
    /// room, and it reads no worse: the caller asks for an open payout route
    /// and gets its legs, or it reverts.
    ///
    /// There is deliberately no matching `_requireOpenDepositRoute`. `deposit`
    /// has stack room to spare and states its own two conditions inline, which
    /// is clearer than a helper that exists only for symmetry with one written
    /// under duress.
    ///
    /// @param route the payout route being authorized.
    /// @return source the protocol chain the payout settles against.
    /// @return dest the protocol chain the payout pays out on.
    function _requireOpenPayoutRoute(uint8 route)
        internal
        view
        returns (uint64 source, uint64 dest)
    {
        bool inbound;
        (source, dest, inbound) = _routeLegs(route);
        if (inbound) revert NotAPayoutRoute(route);
        _requireRouteOpen(route, inbound);
    }

    // ---------------------------------------------------------------------
    // Internal: authorization
    // ---------------------------------------------------------------------

    /// Verifies a 2-of-3 signer quorum over `structHash`.
    ///
    /// Exactly `SIGNER_THRESHOLD` signatures are required — not "at least".
    /// Accepting extras would mean deciding what to do with a third signature
    /// that is invalid, and every answer to that is a subtlety this contract
    /// does not need. Order is irrelevant and no sorting is required or
    /// assumed; distinctness is enforced by explicit comparison, which is exact
    /// for a threshold of two.
    ///
    /// Recovery is `ECDSA.recover`, which reverts on a malformed signature and
    /// on a non-canonical high-s value. No cryptography is implemented here.
    function _authorize(
        bytes32 structHash,
        uint64 epoch,
        uint64 expiry,
        bytes[] calldata signatures
    ) internal view {
        // Same reasoning as the migration delay: expiries are set in minutes to
        // hours by the off-chain signer service, far above validator timestamp
        // leeway. Treating a signature as valid a few seconds either side of its
        // stated expiry is not a security-relevant outcome.
        // forge-lint: disable-next-line(block-timestamp)
        if (block.timestamp > expiry) revert AuthorizationExpired();
        if (epoch != signerEpoch) revert InvalidSignerEpoch(signerEpoch, epoch);
        if (signatures.length != SIGNER_THRESHOLD) revert InvalidSignatureCount();

        bytes32 digest = _hashTypedDataV4(structHash);

        address first = ECDSA.recover(digest, signatures[0]);
        if (!_isSigner[first]) revert UnauthorizedSigner(first);

        address second = ECDSA.recover(digest, signatures[1]);
        if (!_isSigner[second]) revert UnauthorizedSigner(second);

        if (first == second) revert DuplicateSignerSignature(first);
    }

    /// Verifies a governance authorization and consumes the nonce.
    ///
    /// The nonce is checked against the expected value and incremented in the
    /// same step, so governance actions are strictly ordered and no signature
    /// can be replayed. Combined with the action discriminator and the payload
    /// hash, a signature approving one change authorizes exactly that change,
    /// once.
    function _governance(
        uint8 action,
        bytes32 payloadHash,
        uint256 nonce,
        uint64 expiry,
        bytes[] calldata signatures
    ) internal {
        if (nonce != governanceNonce) {
            revert InvalidGovernanceNonce(governanceNonce, nonce);
        }

        bytes32 structHash = keccak256(
            abi.encode(GOVERNANCE_TYPEHASH, action, payloadHash, signerEpoch, nonce, expiry)
        );
        _authorize(structHash, signerEpoch, expiry, signatures);

        unchecked {
            governanceNonce = nonce + 1;
        }
    }

    function _requestKey(uint8 action, bytes32 requestId) internal pure returns (bytes32) {
        return keccak256(abi.encode(action, requestId));
    }

    /// Marks `(action, requestId)` consumed, reverting if it already was. Keyed
    /// on the action as well as the id so the same request id under a different
    /// action is a different guard, never a collision.
    function _consumeRequest(uint8 action, bytes32 requestId) internal {
        bytes32 key = _requestKey(action, requestId);
        if (_executedRequest[key]) revert RequestAlreadyExecuted();
        _executedRequest[key] = true;
    }

    // ---------------------------------------------------------------------
    // Internal: state helpers
    // ---------------------------------------------------------------------

    function _requireCanonicalAmount(uint256 amount) internal pure {
        if (amount == 0) revert InvalidAmount();
        if (amount % CANONICAL_SCALE != 0) revert NonCanonicalAmount();
    }

    /// Fixed-bucket rolling limit.
    ///
    /// The bucket resets wholesale once `ROLLING_WINDOW_SECONDS` have elapsed
    /// since it opened, rather than sliding continuously. This is the same
    /// simplification the Solana program makes, chosen for the same reason: a
    /// true sliding window needs per-deposit storage, and an unbounded
    /// per-user array is a worse problem than the one it solves.
    ///
    /// # The tradeoff, and the CONFIGURATION RULE it imposes
    ///
    /// The worst case is exactly 2x, it is reachable, and it is not
    /// exceedable. Fill a bucket at `t0`, then at exactly `t0 + 24h` the reset
    /// condition holds and the full limit is available again: 2x the
    /// configured limit moves within a span of 86,400 seconds. It cannot be
    /// worse, because buckets open at least 24h apart, so any 24-hour span
    /// intersects at most two of them.
    ///
    /// THEREFORE: the configured on-chain rolling limit MUST be set to ONE
    /// HALF of the intended strict 24-hour policy limit.
    ///
    ///     desired strict policy   = 100,000 GLC / 24h
    ///     configure on-chain      =  50,000 GLC
    ///     worst-case actual burst = 100,000 GLC  (== policy)
    ///
    /// This is an operational requirement, not a suggestion: configuring the
    /// policy number directly silently doubles the real ceiling. It is pinned
    /// by a regression test, and it belongs in the deployment runbook.
    ///
    /// The mechanism is kept as-is deliberately. A true sliding window needs
    /// per-deposit storage, and an unbounded per-user array is a worse problem
    /// than the one it solves. This never UNDER-counts within a bucket and is
    /// O(1) in storage and gas.
    ///
    /// State is mutated only on success — a rejected transfer must never
    /// advance the window.
    function _consumeWindow(Window storage window, uint256 amount, uint256 limit) internal {
        uint64 nowTs = _now64();
        uint64 start = window.windowStart;
        uint256 total = window.total;

        // Validator timestamp leeway is seconds against a 24-hour bucket, and is
        // strictly dominated by the fixed-bucket tradeoff already documented
        // above: nudging the clock cannot produce an outcome a burst across a
        // genuine bucket boundary could not.
        // forge-lint: disable-next-line(block-timestamp)
        if (nowTs - start >= ROLLING_WINDOW_SECONDS) {
            start = nowTs;
            total = 0;
        }

        uint256 projected = total + amount;
        if (projected > limit) revert ExceedsRollingLimit();

        window.windowStart = start;
        window.total = projected;
    }

    /// Reverts unless `amount` can be paid without eating either the protected
    /// floor or any depositor's unsettled principal.
    ///
    /// A payout may never be funded out of a depositor's unsettled principal:
    /// that GLC is here to be either settled or returned, and spending it would
    /// strand a refund the bridge still owes. This is the on-chain half of the
    /// guarantee that makes terminal migration safe.
    ///
    /// Written as sequential subtraction rather than one summed comparison:
    /// `protectedMinReserve` is an unbounded operator-set value, and summing it
    /// could overflow into an opaque arithmetic panic instead of the named
    /// error an operator needs to see mid-incident. Each step here is
    /// individually guarded, so the only possible failure is
    /// `InsufficientReserve`.
    function _requireSpendableReserve(uint256 amount, uint256 protectedMinReserve) internal view {
        uint256 balance = TOKEN.balanceOf(address(this));
        if (balance < amount) revert InsufficientReserve();
        uint256 remaining = balance - amount;
        if (remaining < protectedMinReserve) revert InsufficientReserve();
        if (remaining - protectedMinReserve < outstandingRefundablePrincipal) {
            revert InsufficientReserve();
        }
    }

    /// Releases one obligation's principal from the encumbered reserve. Called
    /// on exactly one of settle or refund, each of which can happen at most once
    /// per obligation because both require and clear `Pending`.
    /// Deliberately CHECKED arithmetic, not `unchecked`. The invariant that
    /// makes an underflow impossible holds today (one increment site; both
    /// exits require and clear `Pending`), but if it were ever broken the wrap
    /// would be catastrophic rather than merely wrong: an
    /// `outstandingRefundablePrincipal` near 2^256 silently blocks every payout
    /// through the reserve check AND makes `finalizeMigration`'s zero-liability
    /// gate unreachable forever. A few gas is not worth turning a detectable
    /// revert into a permanently bricked contract.
    function _releaseLiability(uint256 amount) internal {
        outstandingRefundableCount -= 1;
        outstandingRefundablePrincipal -= amount;
    }

    /// Narrows `block.timestamp` to 64 bits through OpenZeppelin's `SafeCast`,
    /// which reverts on overflow, rather than an unchecked cast. The overflow
    /// is unreachable for any plausible chain (uint64 seconds runs to year
    /// 584,942,417,355), but an unchecked narrowing in a contract that gates a
    /// 48-hour migration delay on a timestamp is not worth defending, and a
    /// maintained primitive beats a hand-written bound check.
    function _now64() internal view returns (uint64) {
        return block.timestamp.toUint64();
    }

    /// Non-zero and pairwise-distinct, checked as straight-line code.
    ///
    /// Distinctness is compared directly between the three candidates rather
    /// than probed through the authority mapping. That is stricter: the mapping
    /// only reflects the OUTGOING set during a rotation, so a mapping-based
    /// check would depend on the caller having cleared it first, whereas this
    /// holds unconditionally.
    function _validateSignerTriple(address[SIGNER_COUNT] memory t) internal pure {
        if (t[0] == address(0) || t[1] == address(0) || t[2] == address(0)) revert ZeroAddress();
        if (t[0] == t[1] || t[0] == t[2] || t[1] == t[2]) revert DuplicateSigner();
    }

    function _validateGuardianTriple(address[GUARDIAN_COUNT] memory t) internal pure {
        if (t[0] == address(0) || t[1] == address(0) || t[2] == address(0)) revert ZeroAddress();
        if (t[0] == t[1] || t[0] == t[2] || t[1] == t[2]) revert DuplicateGuardian();
    }

    function _installSigners(address[SIGNER_COUNT] memory newSigners) internal {
        _validateSignerTriple(newSigners);
        _signers = newSigners;
        for (uint256 i = 0; i < SIGNER_COUNT; ++i) {
            _isSigner[newSigners[i]] = true;
        }
    }

    function _installGuardians(address[GUARDIAN_COUNT] memory newGuardians) internal {
        _validateGuardianTriple(newGuardians);
        _guardians = newGuardians;
        for (uint256 i = 0; i < GUARDIAN_COUNT; ++i) {
            _isGuardian[newGuardians[i]] = true;
        }
    }

    /// Zero semantics, stated explicitly rather than left to inference:
    /// - a minimum of zero is REJECTED; a zero-value deposit is never valid, and
    ///   `_requireCanonicalAmount` rejects it independently anyway.
    /// - a rolling limit of zero is REJECTED; it would close the direction
    ///   silently, and closing a direction is what the pause flags are for.
    /// - `protectedMinReserve` of zero IS permitted and means "no floor".
    function _validateLimits(Limits memory lim) internal pure {
        if (lim.inboundMin == 0 || lim.outboundMin == 0) revert InvalidLimits();
        if (lim.inboundMin > lim.inboundMax) revert InvalidLimits();
        if (lim.outboundMin > lim.outboundMax) revert InvalidLimits();
        if (lim.inboundRollingLimit < lim.inboundMax) revert InvalidLimits();
        if (lim.outboundRollingLimit < lim.outboundMax) revert InvalidLimits();

        // Every threshold an amount is compared against must itself be
        // expressible in the canonical 8-decimal ledger, or a limit could sit
        // between two representable amounts and mean something the off-chain
        // policy cannot state.
        if (
            lim.inboundMin % CANONICAL_SCALE != 0 || lim.inboundMax % CANONICAL_SCALE != 0
                || lim.inboundRollingLimit % CANONICAL_SCALE != 0
                || lim.outboundMin % CANONICAL_SCALE != 0 || lim.outboundMax % CANONICAL_SCALE != 0
                || lim.outboundRollingLimit % CANONICAL_SCALE != 0
                || lim.protectedMinReserve % CANONICAL_SCALE != 0
        ) {
            revert InvalidLimits();
        }
    }
}
