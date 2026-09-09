//! Contract-state reads against `GlcRobinhoodBridge`, and the calldata
//! builders for the three operations this service executes.
//!
//! # Why a service-side flag is necessary and not sufficient
//!
//! [`crate::routes::RouteGate`]'s three gates decide whether THIS PROCESS
//! is willing to attempt a route. They say nothing about whether the
//! contract will accept the attempt, and the contract is the authority:
//! it holds the funds, it holds the enable flags, it holds the pause
//! flags, and it holds the signer epoch every authorization is bound to.
//!
//! A service that broadcast on its own flag alone would, whenever the two
//! disagreed, spend gas and consume a nonce to produce a revert. Worse,
//! it would do so in exactly the situation where disagreement matters
//! most: a guardian has just paused the contract during an incident, and
//! the service has not been told.
//!
//! So every value-moving operation reads the contract's own state
//! immediately before broadcasting and fails closed on any disagreement
//! ([`ContractGate::check`]). Phase E deliberately omitted `eth_call`;
//! this is where it lands, and this is what it is for.
//!
//! # The reads are pinned to a block
//!
//! Every read here takes an [`EvmBlockTag`]. A pre-broadcast gate reads
//! `Latest`; a post-receipt verification reads the exact block the
//! transaction landed in, so it checks the state the transaction actually
//! executed against rather than whatever the chain has moved on to since.
//!
//! # Calldata is built here, next to the reads
//!
//! Deliberately the same module. The calldata for `executePayout` and the
//! `eth_call` that checks `routeEnabled` describe the same operation from
//! two sides, and keeping them together is what makes it obvious that the
//! route byte in one is the route byte in the other.

use crate::evm::abi::{self, Calldata};
use crate::evm::{EvmAddress, EvmSignature, EvmU256};
use crate::routes::Route;

use super::auth::{PayoutAuth, RefundAuth, SettlementAuth};
use super::rpc::{EvmBlockTag, EvmCall, EvmCallRpc, EvmRpcError};

// ---------------------------------------------------------------------
// Function signatures
// ---------------------------------------------------------------------
//
// Canonical Solidity signatures, character for character: no spaces, no
// parameter names, struct arguments spelled as the parenthesised tuple of
// their field types in DECLARATION order. Each is a wire contract with
// deployed bytecode — a one-character difference selects a different
// (almost certainly nonexistent) function — so every one is pinned by a
// test against an independently computed keccak.

/// `PayoutRequest(uint8 route, bytes32 requestId, address recipient,
/// uint256 amount, uint64 signerEpoch, uint64 expiry)`.
pub const SIG_EXECUTE_PAYOUT: &str =
    "executePayout((uint8,bytes32,address,uint256,uint64,uint64),bytes[])";

/// `RefundRequest(bytes32 requestId, uint256 obligationIndex,
/// address recipient, uint256 amount, uint64 signerEpoch, uint64 expiry)`.
/// No `route` field — the contract reads the obligation's own.
pub const SIG_EXECUTE_REFUND: &str =
    "executeRefund((bytes32,uint256,address,uint256,uint64,uint64),bytes[])";

/// `SettlementRequest(bytes32 requestId, uint256 obligationIndex,
/// uint64 signerEpoch, uint64 expiry)`.
pub const SIG_EXECUTE_SETTLEMENT: &str =
    "executeSettlement((bytes32,uint256,uint64,uint64),bytes[])";

pub const SIG_TOKEN: &str = "token()";
pub const SIG_BRIDGE_PROTOCOL_ID: &str = "bridgeProtocolId()";
pub const SIG_SIGNER_EPOCH: &str = "signerEpoch()";
pub const SIG_ROUTE_ENABLED: &str = "routeEnabled(uint8)";
pub const SIG_IS_ROUTE_LIVE: &str = "isRouteLive(uint8)";
pub const SIG_ROUTE_CHAINS: &str = "routeChains(uint8)";
pub const SIG_DEPOSITS_PAUSED: &str = "depositsPaused()";
pub const SIG_PAYOUTS_PAUSED: &str = "payoutsPaused()";
pub const SIG_MIGRATED: &str = "migrated()";
pub const SIG_OBLIGATION: &str = "obligation(uint256)";
pub const SIG_OBLIGATION_STATUS: &str = "obligationStatus(uint256)";
pub const SIG_OBLIGATION_COUNT: &str = "obligationCount()";
pub const SIG_REQUEST_EXECUTED: &str = "requestExecuted(uint8,bytes32)";
pub const SIG_ENCUMBERED_RESERVE: &str = "encumberedReserve()";
pub const SIG_LIMITS: &str = "limits()";
pub const SIG_INBOUND_WINDOW: &str = "inboundWindow()";
pub const SIG_OUTBOUND_WINDOW: &str = "outboundWindow()";
pub const SIG_SIGNERS: &str = "signers()";
pub const SIG_DOMAIN_SEPARATOR: &str = "domainSeparator()";
pub const SIG_ERC20_BALANCE_OF: &str = "balanceOf(address)";
pub const SIG_ERC20_DECIMALS: &str = "decimals()";

/// `GlcRobinhoodBridge.BRIDGE_PROTOCOL_ID` =
/// `keccak256("glc.reserve-bridge.robinhood")`.
///
/// Names the protocol FAMILY, not this version — a successor is expected
/// to return the same value. Checked at preflight as evidence that the
/// configured address is a bridge of this protocol at all, rather than
/// some unrelated contract that happens to have a `token()`.
pub fn bridge_protocol_id() -> [u8; 32] {
    crate::evm::keccak256(b"glc.reserve-bridge.robinhood")
}

/// The `ObligationStatus` enum's wire values. Appended, never reordered —
/// each is a value an off-chain decoder matches on.
pub const OBLIGATION_STATUS_NONE: u8 = 0;
pub const OBLIGATION_STATUS_PENDING: u8 = 1;
pub const OBLIGATION_STATUS_SETTLED: u8 = 2;
pub const OBLIGATION_STATUS_REFUNDED: u8 = 3;
pub const OBLIGATION_STATUS_ABANDONED: u8 = 4;

/// One obligation as the contract holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obligation {
    /// The wallet whose transfer created this obligation. Written once,
    /// never mutated on-chain — which is what makes a refund's recipient
    /// derived rather than chosen.
    pub depositor: EvmAddress,
    pub status: u8,
    /// The obligation's OWN route, fixed when the deposit landed. Every
    /// subsequent authorization about this obligation binds THIS value,
    /// read from the chain, never a value this service supplied.
    pub route: u8,
    /// The principal, in Robinhood 18-decimal atomic units, as the exact
    /// 256-bit word.
    pub amount: EvmU256,
}

impl Obligation {
    pub fn is_pending(&self) -> bool {
        self.status == OBLIGATION_STATUS_PENDING
    }

    pub fn status_name(&self) -> &'static str {
        match self.status {
            OBLIGATION_STATUS_NONE => "None",
            OBLIGATION_STATUS_PENDING => "Pending",
            OBLIGATION_STATUS_SETTLED => "Settled",
            OBLIGATION_STATUS_REFUNDED => "Refunded",
            OBLIGATION_STATUS_ABANDONED => "Abandoned",
            _ => "Unknown",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContractReadError {
    #[error("reading {what} from the Robinhood bridge: {source}")]
    Rpc {
        what: &'static str,
        #[source]
        source: EvmRpcError,
    },
    #[error("decoding {what} from the Robinhood bridge: {source}")]
    Decode {
        what: &'static str,
        #[source]
        source: abi::AbiDecodeError,
    },
    #[error(
        "the contract returned obligation status {status} ({name}) for an obligation index this \
         service expected to be Pending"
    )]
    UnexpectedObligationStatus { status: u8, name: &'static str },
}

/// The contract's fixed rolling-window bucket width,
/// `GlcRobinhoodBridge.ROLLING_WINDOW_SECONDS`.
///
/// A transcription of a `public constant`, so it is read from the source
/// rather than from the chain: a constant costs a round trip to fetch and
/// cannot change without a redeployment, which preflight's contract
/// identity checks would catch anyway. Stated here so the reported
/// "window resets at" figure is computed from the same number the
/// contract uses rather than from an operator's assumption.
///
/// A FIXED-bucket window, not a sliding one — see the contract's
/// `_consumeWindow` docs. That matters for how the remaining figure is
/// read: it is what is left in THIS bucket, and the whole limit becomes
/// available again at `window_start + ROLLING_WINDOW_SECONDS`, not
/// gradually.
pub const ROLLING_WINDOW_SECONDS: u64 = 24 * 60 * 60;

/// `GlcRobinhoodBridge.Limits` — all configurable policy, in Robinhood
/// 18-decimal atomic units.
///
/// Per DIRECTION rather than per route, and the contract's own docs
/// record why: the two inbound routes share the inbound bucket and the
/// two outbound routes share the outbound one, so the stated limit is the
/// true limit no matter how many routes are live. Reporting these
/// per-route would imply a budget each route has to itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeLimits {
    pub inbound_min: EvmU256,
    pub inbound_max: EvmU256,
    pub inbound_rolling_limit: EvmU256,
    pub outbound_min: EvmU256,
    pub outbound_max: EvmU256,
    pub outbound_rolling_limit: EvmU256,
    /// GLC that may never be paid out, whatever else is true.
    pub protected_min_reserve: EvmU256,
}

/// `GlcRobinhoodBridge.Window` — one direction's fixed-bucket rolling
/// accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollingWindow {
    /// Unix seconds at which the current bucket opened.
    pub window_start: u64,
    /// Robinhood atomic units consumed in the current bucket.
    pub total: EvmU256,
}

impl RollingWindow {
    /// Unix seconds at which this bucket expires and the full limit
    /// becomes available again.
    pub fn resets_at(&self) -> u64 {
        self.window_start.saturating_add(ROLLING_WINDOW_SECONDS)
    }

    /// Whether the bucket `now` falls in is still the recorded one. A
    /// stale bucket reports a `total` the contract would discard on its
    /// next write, so a reader that ignored this would show consumption
    /// that is no longer charged against anything.
    pub fn is_current(&self, now: u64) -> bool {
        now < self.resets_at()
    }

    /// What remains of `limit` in the bucket that `now` falls in.
    ///
    /// Returns the WHOLE limit once the bucket has expired, mirroring
    /// `_consumeWindow`'s reset rather than reporting a stale total —
    /// the contract zeroes `total` on the next write past the boundary,
    /// so remaining capacity really is the full limit at that point.
    ///
    /// Saturating: a `total` above `limit` cannot arise from
    /// `_consumeWindow`, which refuses the write that would cause it, but
    /// a lowered limit can leave an existing bucket above the new value.
    /// That is zero remaining, not negative.
    pub fn remaining(&self, limit: EvmU256, now: u64) -> EvmU256 {
        if !self.is_current(now) {
            return limit;
        }
        limit.saturating_sub(self.total)
    }
}

/// A read-only view of one deployed `GlcRobinhoodBridge`.
///
/// Holds the address and nothing else; every method takes the RPC client
/// so the same view can be used against a live client, against a mock, or
/// against a client that provably cannot broadcast.
#[derive(Debug, Clone, Copy)]
pub struct BridgeReader {
    pub bridge: EvmAddress,
}

impl BridgeReader {
    pub fn new(bridge: EvmAddress) -> BridgeReader {
        BridgeReader { bridge }
    }

    async fn read_word<R: EvmCallRpc>(
        &self,
        rpc: &R,
        what: &'static str,
        data: Vec<u8>,
        block: EvmBlockTag,
    ) -> Result<abi::Word, ContractReadError> {
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.bridge,
                    data,
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        let [word] = abi::return_words::<1>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        Ok(word)
    }

    /// `TOKEN()` — the ERC-20 this contract custodies.
    pub async fn token<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<EvmAddress, ContractReadError> {
        let word = self
            .read_word(rpc, "token()", Calldata::new(SIG_TOKEN).finish(), block)
            .await?;
        abi::decode_address(&word, "token").map_err(|source| ContractReadError::Decode {
            what: "token()",
            source,
        })
    }

    /// `bridgeProtocolId()` — the protocol FAMILY identifier.
    pub async fn bridge_protocol_id<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<[u8; 32], ContractReadError> {
        self.read_word(
            rpc,
            "bridgeProtocolId()",
            Calldata::new(SIG_BRIDGE_PROTOCOL_ID).finish(),
            block,
        )
        .await
    }

    /// `domainSeparator()` — the contract's own EIP-712 domain separator.
    ///
    /// Read rather than only computed locally. The local computation
    /// ([`super::auth::BridgeDomain::separator`]) is the one the digests
    /// are actually built from; reading the contract's is what proves the
    /// two agree, against the deployment this process is pointed at,
    /// before a single authorization is minted. The cross-language golden
    /// fixture proves the FORMULA; this proves the DEPLOYMENT.
    pub async fn domain_separator<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<[u8; 32], ContractReadError> {
        self.read_word(
            rpc,
            "domainSeparator()",
            Calldata::new(SIG_DOMAIN_SEPARATOR).finish(),
            block,
        )
        .await
    }

    /// `signerEpoch()` — bound into every authorization. A rotation
    /// increments it and invalidates every signature the outgoing set
    /// ever produced.
    pub async fn signer_epoch<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<u64, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "signerEpoch()",
                Calldata::new(SIG_SIGNER_EPOCH).finish(),
                block,
            )
            .await?;
        abi::decode_u64(&word, "signerEpoch").map_err(|source| ContractReadError::Decode {
            what: "signerEpoch()",
            source,
        })
    }

    /// `signers()` — the three addresses the contract will accept
    /// authorizations from.
    pub async fn signers<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<[EvmAddress; 3], ContractReadError> {
        let what = "signers()";
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.bridge,
                    data: Calldata::new(SIG_SIGNERS).finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        // A fixed-size array of a static type is encoded INLINE as its
        // elements, with no length word and no offset — three words, not
        // four.
        let words = abi::return_words::<3>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        let mut out = [EvmAddress::ZERO; 3];
        for (i, word) in words.iter().enumerate() {
            out[i] = abi::decode_address(word, "signers")
                .map_err(|source| ContractReadError::Decode { what, source })?;
        }
        Ok(out)
    }

    /// `routeEnabled(route)` — governance's per-route flag.
    ///
    /// Reverts on a route the contract does not model, which is a
    /// definitive RPC error rather than `false`: "no such route" and
    /// "route is off" are different facts and the contract deliberately
    /// refuses to conflate them.
    pub async fn route_enabled<R: EvmCallRpc>(
        &self,
        rpc: &R,
        route_byte: u8,
        block: EvmBlockTag,
    ) -> Result<bool, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "routeEnabled(uint8)",
                Calldata::new(SIG_ROUTE_ENABLED)
                    .word(abi::word_u128(u128::from(route_byte)))
                    .finish(),
                block,
            )
            .await?;
        abi::decode_bool(&word, "routeEnabled").map_err(|source| ContractReadError::Decode {
            what: "routeEnabled(uint8)",
            source,
        })
    }

    /// `isRouteLive(route)` — EVERY gate at once: not migrated, route
    /// enabled, direction not paused, and (inbound only) no migration
    /// committed.
    ///
    /// The contract's own docs say operators and monitoring should read
    /// THIS rather than reassembling it from the individual flags,
    /// "which is where the two gates get confused for each other". This
    /// service takes that advice: [`ContractGate::check`] reads this as
    /// its primary verdict, and reads the individual flags only to tell
    /// an operator WHICH gate is shut.
    pub async fn is_route_live<R: EvmCallRpc>(
        &self,
        rpc: &R,
        route_byte: u8,
        block: EvmBlockTag,
    ) -> Result<bool, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "isRouteLive(uint8)",
                Calldata::new(SIG_IS_ROUTE_LIVE)
                    .word(abi::word_u128(u128::from(route_byte)))
                    .finish(),
                block,
            )
            .await?;
        abi::decode_bool(&word, "isRouteLive").map_err(|source| ContractReadError::Decode {
            what: "isRouteLive(uint8)",
            source,
        })
    }

    /// `routeChains(route)` — the pair of NAMESPACED protocol chain ids
    /// this route resolves to, which every authorization binds.
    ///
    /// Read from the contract rather than configured. They are immutables
    /// fixed at construction, and a configured copy would be a second
    /// place they are written down — one that could be wrong in a way
    /// that produces a digest the contract rejects, or (much worse) one
    /// that is valid for a different route.
    pub async fn route_chains<R: EvmCallRpc>(
        &self,
        rpc: &R,
        route_byte: u8,
        block: EvmBlockTag,
    ) -> Result<super::auth::ProtocolChainPair, ContractReadError> {
        let what = "routeChains(uint8)";
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.bridge,
                    data: Calldata::new(SIG_ROUTE_CHAINS)
                        .word(abi::word_u128(u128::from(route_byte)))
                        .finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        let [source_word, dest_word] = abi::return_words::<2>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        Ok(super::auth::ProtocolChainPair {
            source: abi::decode_u64(&source_word, "routeChains.source")
                .map_err(|source| ContractReadError::Decode { what, source })?,
            dest: abi::decode_u64(&dest_word, "routeChains.dest")
                .map_err(|source| ContractReadError::Decode { what, source })?,
        })
    }

    pub async fn deposits_paused<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<bool, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "depositsPaused()",
                Calldata::new(SIG_DEPOSITS_PAUSED).finish(),
                block,
            )
            .await?;
        abi::decode_bool(&word, "depositsPaused").map_err(|source| ContractReadError::Decode {
            what: "depositsPaused()",
            source,
        })
    }

    pub async fn payouts_paused<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<bool, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "payoutsPaused()",
                Calldata::new(SIG_PAYOUTS_PAUSED).finish(),
                block,
            )
            .await?;
        abi::decode_bool(&word, "payoutsPaused").map_err(|source| ContractReadError::Decode {
            what: "payoutsPaused()",
            source,
        })
    }

    /// `migrated()` — whether this deployment has handed its reserve to a
    /// successor. Terminal: every value-moving path reverts afterwards.
    pub async fn migrated<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<bool, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "migrated()",
                Calldata::new(SIG_MIGRATED).finish(),
                block,
            )
            .await?;
        abi::decode_bool(&word, "migrated").map_err(|source| ContractReadError::Decode {
            what: "migrated()",
            source,
        })
    }

    /// `obligationCount()` — the contract-local counter. An index at or
    /// above it does not exist.
    pub async fn obligation_count<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<u64, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "obligationCount()",
                Calldata::new(SIG_OBLIGATION_COUNT).finish(),
                block,
            )
            .await?;
        abi::decode_u64(&word, "obligationCount").map_err(|source| ContractReadError::Decode {
            what: "obligationCount()",
            source,
        })
    }

    /// `obligation(index)` — the full struct.
    ///
    /// This is the AUTHORITY for a refund's recipient and amount. The
    /// contract compares both against these stored values and reverts on
    /// any difference, so building a refund from anything else — an
    /// operator's input, a ledger row, an event this service decoded —
    /// would at best revert and at worst mean the ledger and the chain
    /// disagree about whose money it is.
    pub async fn obligation<R: EvmCallRpc>(
        &self,
        rpc: &R,
        index: u64,
        block: EvmBlockTag,
    ) -> Result<Obligation, ContractReadError> {
        let what = "obligation(uint256)";
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.bridge,
                    data: Calldata::new(SIG_OBLIGATION)
                        .word(abi::word_u128(u128::from(index)))
                        .finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        // A struct of only static fields returns as its fields inline:
        // four words, no offset, no length.
        let [depositor, status, route, amount] = abi::return_words::<4>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        Ok(Obligation {
            depositor: abi::decode_address(&depositor, "obligation.depositor")
                .map_err(|source| ContractReadError::Decode { what, source })?,
            status: abi::decode_u8(&status, "obligation.status")
                .map_err(|source| ContractReadError::Decode { what, source })?,
            route: abi::decode_u8(&route, "obligation.route")
                .map_err(|source| ContractReadError::Decode { what, source })?,
            amount: EvmU256::from_be_bytes(amount),
        })
    }

    /// `requestExecuted(action, requestId)` — whether the contract has
    /// already consumed this exact `(action, requestId)` pair.
    ///
    /// The single most important read in the restart story. After a crash
    /// with an uncertain broadcast, this answers "did my operation
    /// already happen" directly, from the contract's own replay guard,
    /// rather than by inferring it from a receipt that may have aged out
    /// of a node's index. `true` means the operation is DONE and must
    /// never be re-attempted under a fresh nonce.
    pub async fn request_executed<R: EvmCallRpc>(
        &self,
        rpc: &R,
        action: u8,
        request_id: [u8; 32],
        block: EvmBlockTag,
    ) -> Result<bool, ContractReadError> {
        let word = self
            .read_word(
                rpc,
                "requestExecuted(uint8,bytes32)",
                Calldata::new(SIG_REQUEST_EXECUTED)
                    .word(abi::word_u128(u128::from(action)))
                    .word(abi::word_bytes32(request_id))
                    .finish(),
                block,
            )
            .await?;
        abi::decode_bool(&word, "requestExecuted").map_err(|source| ContractReadError::Decode {
            what: "requestExecuted(uint8,bytes32)",
            source,
        })
    }

    /// `encumberedReserve()` — the protected floor plus every unsettled
    /// depositor's principal: GLC that is physically in the contract but
    /// is not the bridge's to pay out.
    pub async fn encumbered_reserve<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<EvmU256, ContractReadError> {
        self.read_word(
            rpc,
            "encumberedReserve()",
            Calldata::new(SIG_ENCUMBERED_RESERVE).finish(),
            block,
        )
        .await
        .map(EvmU256::from_be_bytes)
    }

    /// `limits()` — the contract's configured policy.
    ///
    /// A struct of seven static `uint256` fields, so it returns as seven
    /// words inline: no offset, no length. Read rather than mirrored in
    /// config: these are governed on-chain under signer quorum and a
    /// configured copy would be a second opinion that drifts silently the
    /// first time they are changed.
    pub async fn limits<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<BridgeLimits, ContractReadError> {
        let what = "limits()";
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.bridge,
                    data: Calldata::new(SIG_LIMITS).finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        let [inbound_min, inbound_max, inbound_rolling_limit, outbound_min, outbound_max, outbound_rolling_limit, protected_min_reserve] =
            abi::return_words::<7>(&raw)
                .map_err(|source| ContractReadError::Decode { what, source })?;
        Ok(BridgeLimits {
            inbound_min: EvmU256::from_be_bytes(inbound_min),
            inbound_max: EvmU256::from_be_bytes(inbound_max),
            inbound_rolling_limit: EvmU256::from_be_bytes(inbound_rolling_limit),
            outbound_min: EvmU256::from_be_bytes(outbound_min),
            outbound_max: EvmU256::from_be_bytes(outbound_max),
            outbound_rolling_limit: EvmU256::from_be_bytes(outbound_rolling_limit),
            protected_min_reserve: EvmU256::from_be_bytes(protected_min_reserve),
        })
    }

    /// `governanceNonce()` — the nonce the NEXT governance action must
    /// carry.
    ///
    /// Read immediately before an authorization is built and never
    /// cached: the contract compares it for strict equality and consumes
    /// it, so a value read a moment too early authorizes nothing. A
    /// concurrent governance action anywhere in the world invalidates a
    /// proposal built on the old number, which is exactly the ordering
    /// guarantee the nonce exists to provide.
    pub async fn governance_nonce<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<EvmU256, ContractReadError> {
        let what = "governanceNonce()";
        let word = self
            .read_word(
                rpc,
                what,
                Calldata::new(super::governance::SIG_GOVERNANCE_NONCE).finish(),
                block,
            )
            .await?;
        Ok(EvmU256::from_be_bytes(word))
    }

    /// `inboundWindow()` — the DEPOSIT direction's rolling accumulator,
    /// shared by both inbound routes.
    pub async fn inbound_window<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<RollingWindow, ContractReadError> {
        self.read_window(rpc, "inboundWindow()", SIG_INBOUND_WINDOW, block)
            .await
    }

    /// `outboundWindow()` — the PAYOUT direction's rolling accumulator,
    /// shared by both outbound routes.
    pub async fn outbound_window<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<RollingWindow, ContractReadError> {
        self.read_window(rpc, "outboundWindow()", SIG_OUTBOUND_WINDOW, block)
            .await
    }

    /// Both window getters return the same two-word struct; one decoder
    /// rather than two that could disagree about field order.
    async fn read_window<R: EvmCallRpc>(
        &self,
        rpc: &R,
        what: &'static str,
        signature: &str,
        block: EvmBlockTag,
    ) -> Result<RollingWindow, ContractReadError> {
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.bridge,
                    data: Calldata::new(signature).finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        let [window_start, total] = abi::return_words::<2>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        Ok(RollingWindow {
            window_start: abi::decode_u64(&window_start, "window.windowStart")
                .map_err(|source| ContractReadError::Decode { what, source })?,
            total: EvmU256::from_be_bytes(total),
        })
    }
}

/// A read-only view of the reserve ERC-20.
#[derive(Debug, Clone, Copy)]
pub struct TokenReader {
    pub token: EvmAddress,
}

impl TokenReader {
    pub fn new(token: EvmAddress) -> TokenReader {
        TokenReader { token }
    }

    /// `decimals()`. Asserted against
    /// [`crate::amount_conversion::robinhood::ROBINHOOD_DECIMALS`] at
    /// preflight — a different value does not mean "scale differently",
    /// it means this is not the asset this code models.
    pub async fn decimals<R: EvmCallRpc>(
        &self,
        rpc: &R,
        block: EvmBlockTag,
    ) -> Result<u8, ContractReadError> {
        let what = "decimals()";
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.token,
                    data: Calldata::new(SIG_ERC20_DECIMALS).finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        let [word] = abi::return_words::<1>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        abi::decode_u8(&word, "decimals")
            .map_err(|source| ContractReadError::Decode { what, source })
    }

    /// `balanceOf(holder)` — the reserve balance, in Robinhood 18-decimal
    /// atomic units.
    pub async fn balance_of<R: EvmCallRpc>(
        &self,
        rpc: &R,
        holder: EvmAddress,
        block: EvmBlockTag,
    ) -> Result<EvmU256, ContractReadError> {
        let what = "balanceOf(address)";
        let raw = rpc
            .call(
                &EvmCall {
                    to: self.token,
                    data: Calldata::new(SIG_ERC20_BALANCE_OF)
                        .word(abi::word_address(holder))
                        .finish(),
                },
                block,
            )
            .await
            .map_err(|source| ContractReadError::Rpc { what, source })?;
        let [word] = abi::return_words::<1>(&raw)
            .map_err(|source| ContractReadError::Decode { what, source })?;
        Ok(EvmU256::from_be_bytes(word))
    }
}

// ---------------------------------------------------------------------
// Calldata builders
// ---------------------------------------------------------------------

/// `executePayout(PayoutRequest, bytes[])`.
///
/// Takes the [`PayoutAuth`] the quorum actually signed rather than loose
/// parameters, so the calldata and the digest are built from ONE value.
/// A version taking six arguments could be called with a recipient that
/// was not the one signed for, and the mistake would only surface as a
/// revert.
pub fn encode_execute_payout(auth: &PayoutAuth, signatures: &[EvmSignature]) -> Vec<u8> {
    let route_byte = auth
        .route
        .contract_route_id()
        .expect("a PayoutAuth cannot be constructed for a non-contract route");
    Calldata::new(SIG_EXECUTE_PAYOUT)
        .word(abi::word_u128(u128::from(route_byte)))
        .word(abi::word_bytes32(auth.request_id))
        .word(abi::word_address(auth.recipient))
        .word(abi::word_u256(auth.amount.to_u256()))
        .word(abi::word_u128(u128::from(auth.signer_epoch)))
        .word(abi::word_u128(u128::from(auth.expiry)))
        .bytes_array(signatures.iter().map(|s| s.to_bytes().to_vec()).collect())
        .finish()
}

/// `executeRefund(RefundRequest, bytes[])`.
pub fn encode_execute_refund(auth: &RefundAuth, signatures: &[EvmSignature]) -> Vec<u8> {
    Calldata::new(SIG_EXECUTE_REFUND)
        .word(abi::word_bytes32(auth.request_id))
        .word(abi::word_u128(u128::from(auth.obligation_index)))
        .word(abi::word_address(auth.recipient))
        .word(abi::word_u256(auth.amount.to_u256()))
        .word(abi::word_u128(u128::from(auth.signer_epoch)))
        .word(abi::word_u128(u128::from(auth.expiry)))
        .bytes_array(signatures.iter().map(|s| s.to_bytes().to_vec()).collect())
        .finish()
}

/// `executeSettlement(SettlementRequest, bytes[])`.
pub fn encode_execute_settlement(auth: &SettlementAuth, signatures: &[EvmSignature]) -> Vec<u8> {
    Calldata::new(SIG_EXECUTE_SETTLEMENT)
        .word(abi::word_bytes32(auth.request_id))
        .word(abi::word_u128(u128::from(auth.obligation_index)))
        .word(abi::word_u128(u128::from(auth.signer_epoch)))
        .word(abi::word_u128(u128::from(auth.expiry)))
        .bytes_array(signatures.iter().map(|s| s.to_bytes().to_vec()).collect())
        .finish()
}

/// The contract-side gate every value-moving operation passes through
/// immediately before broadcasting.
///
/// # Why this is a separate type from the read functions
///
/// So that "did we check" is answerable by looking for one call rather
/// than by auditing that six reads happened in the right order at every
/// call site. [`ContractGate::check`] performs all of them and returns a
/// single verdict; there is no way to perform five of the six.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRefusal {
    /// The contract has migrated. Terminal — every value-moving path
    /// reverts from here on, and no retry will change that.
    Migrated,
    /// The route's own enable flag is off.
    RouteDisabled { route: &'static str },
    /// The direction is paused. A guardian can assert this unilaterally
    /// and instantly; it is the state an operator most needs named
    /// correctly during an incident.
    DirectionPaused { deposits: bool, payouts: bool },
    /// The contract's signer epoch is not the one the authorization was
    /// built for. A rotation happened; every signature from the outgoing
    /// set is now worthless and the operation must be re-authorized.
    SignerEpochChanged { expected: u64, actual: u64 },
    /// `(action, requestId)` has already been consumed on-chain. NOT a
    /// failure: it means the operation ALREADY HAPPENED, and the correct
    /// response is to record that, never to try again.
    AlreadyExecuted,
    /// A route this contract does not model as live even though every
    /// individual flag read as open — reported separately so a
    /// disagreement between `isRouteLive` and the individual flags is
    /// visible rather than silently resolved in favour of one of them.
    NotLive { route: &'static str },
}

impl std::fmt::Display for GateRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateRefusal::Migrated => f.write_str(
                "the Robinhood custody contract has migrated to a successor; no value-moving \
                 call will ever succeed against it again",
            ),
            GateRefusal::RouteDisabled { route } => {
                write!(f, "the contract reports route {route} as DISABLED")
            }
            GateRefusal::DirectionPaused { deposits, payouts } => write!(
                f,
                "the contract is paused (deposits={deposits}, payouts={payouts})"
            ),
            GateRefusal::SignerEpochChanged { expected, actual } => write!(
                f,
                "the contract's signer epoch is {actual} but this authorization was built for \
                 {expected}: the signer set rotated and every signature from the old set is void"
            ),
            GateRefusal::AlreadyExecuted => f.write_str(
                "the contract has already executed this (action, requestId): the operation is \
                 done and must not be attempted again",
            ),
            GateRefusal::NotLive { route } => write!(
                f,
                "the contract reports route {route} as not live even though its individual \
                 flags read as open"
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error(transparent)]
    Read(#[from] ContractReadError),
    #[error("contract-side gate refused: {0}")]
    Refused(GateRefusal),
}

/// Reads every contract-side condition an operation depends on, in one
/// place.
pub struct ContractGate {
    pub reader: BridgeReader,
}

impl ContractGate {
    pub fn new(bridge: EvmAddress) -> ContractGate {
        ContractGate {
            reader: BridgeReader::new(bridge),
        }
    }

    /// The full pre-broadcast gate.
    ///
    /// Order matters only for which refusal an operator sees when more
    /// than one condition is shut, and it is chosen to name the most
    /// actionable one first: an already-executed operation is not a
    /// problem at all, a migration is terminal, a pause is what a human
    /// just did, and a disabled route is a governance state.
    ///
    /// `route` is the SERVICE's route; `action`/`request_id` identify the
    /// operation in the contract's own replay guard; `signer_epoch` is
    /// the epoch the authorization was built for.
    pub async fn check<R: EvmCallRpc>(
        &self,
        rpc: &R,
        route: Route,
        action: u8,
        request_id: [u8; 32],
        signer_epoch: u64,
        block: EvmBlockTag,
    ) -> Result<(), GateError> {
        let route_byte = route.contract_route_id().ok_or_else(|| {
            GateError::Refused(GateRefusal::RouteDisabled {
                route: route.as_str(),
            })
        })?;

        // FIRST, because it is the only condition whose answer is "you are
        // already finished" rather than "you may not proceed". Asking it
        // first means a restart after an uncertain broadcast learns the
        // truth before it evaluates anything that could send it down a
        // retry path.
        if self
            .reader
            .request_executed(rpc, action, request_id, block)
            .await?
        {
            return Err(GateError::Refused(GateRefusal::AlreadyExecuted));
        }

        if self.reader.migrated(rpc, block).await? {
            return Err(GateError::Refused(GateRefusal::Migrated));
        }

        let epoch = self.reader.signer_epoch(rpc, block).await?;
        if epoch != signer_epoch {
            return Err(GateError::Refused(GateRefusal::SignerEpochChanged {
                expected: signer_epoch,
                actual: epoch,
            }));
        }

        // A refund and a settlement are deliberately NOT gated on the
        // route flag or the pause, and this is not an oversight: the
        // contract itself allows both while paused and while the route is
        // disabled, because refunding and settling are how OUTSTANDING
        // liability is cleared, and gating them would strand every deposit
        // made while a route was open. Gating them here would be a second,
        // stricter policy than the contract's — one that turns off exactly
        // the paths an operator needs during the incident that caused the
        // pause.
        let closes_an_obligation =
            action == super::auth::ACTION_REFUND || action == super::auth::ACTION_SETTLE;
        if !closes_an_obligation {
            let deposits = self.reader.deposits_paused(rpc, block).await?;
            let payouts = self.reader.payouts_paused(rpc, block).await?;
            if payouts {
                return Err(GateError::Refused(GateRefusal::DirectionPaused {
                    deposits,
                    payouts,
                }));
            }
            if !self.reader.route_enabled(rpc, route_byte, block).await? {
                return Err(GateError::Refused(GateRefusal::RouteDisabled {
                    route: route.as_str(),
                }));
            }
            // The contract's own composite verdict, read last: if it
            // disagrees with the individual flags this service just read,
            // the contract wins and the disagreement is reported rather
            // than resolved.
            if !self.reader.is_route_live(rpc, route_byte, block).await? {
                return Err(GateError::Refused(GateRefusal::NotLive {
                    route: route.as_str(),
                }));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
