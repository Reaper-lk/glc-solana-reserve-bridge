//! The strict `DepositCreated` decoder — the only place raw EVM log bytes
//! become a deposit this service is willing to believe in.
//!
//! # The event, exactly as the deployed contract declares it
//!
//! ```solidity
//! event DepositCreated(
//!     uint256 indexed obligationIndex,
//!     address indexed depositor,
//!     uint8   indexed route,
//!     uint256 amount,
//!     uint256 canonicalAmount,
//!     bytes   destination
//! );
//! ```
//!
//! (`contracts/src/GlcRobinhoodBridge.sol`, emitted from exactly one
//! place: the end of `deposit()`.)
//!
//! Its canonical signature — what topic0 is the keccak-256 of — is
//! therefore [`DEPOSIT_CREATED_SIGNATURE`]:
//! `DepositCreated(uint256,address,uint8,uint256,uint256,bytes)`.
//!
//! Three indexed parameters means four topics, and the non-indexed tail
//! `(uint256, uint256, bytes)` is what the `data` blob ABI-encodes:
//!
//! ```text
//! topics[0] = keccak256(DEPOSIT_CREATED_SIGNATURE)
//! topics[1] = obligationIndex          (uint256, as-is)
//! topics[2] = depositor                (address, left-padded to 32 bytes)
//! topics[3] = route                    (uint8,   left-padded to 32 bytes)
//!
//! data[  0.. 32) = amount              (uint256)
//! data[ 32.. 64) = canonicalAmount     (uint256)
//! data[ 64.. 96) = 0x60                (offset of `destination`'s tail)
//! data[ 96..128) = destination.length  (uint256)
//! data[128..   ) = destination bytes, right-padded with zeros to a
//!                  32-byte multiple
//! ```
//!
//! # Strictness is the feature
//!
//! Everything below refuses rather than repairs. There is no arm that
//! truncates a wide value, tolerates a short field, ignores trailing
//! bytes, accepts a non-zero pad, or follows an offset the encoder would
//! never have produced. That is not defensiveness for its own sake: this
//! decoder's output is a durable claim that somebody deposited a specific
//! amount, and every lenient arm is a way for a malformed or hostile RPC
//! response to make that claim say something it should not.
//!
//! Two checks are worth calling out because they are not generic ABI
//! hygiene:
//!
//! - **The dynamic-tail offset must be exactly `0x60`.** Solidity's
//!   encoder always emits that for this parameter list. A different
//!   offset can only come from a hand-built payload, and honouring it
//!   would let one blob be read as several different destinations.
//! - **`amount` must be exactly `canonicalAmount * CANONICAL_SCALE`.**
//!   The contract enforces this on the way in (`_requireCanonicalAmount`
//!   plus `amount / CANONICAL_SCALE` in the emit). Re-deriving it here
//!   means the amount finally stored has two independent witnesses that
//!   agree, so a single corrupted word cannot move it.
//!
//! # Only inbound routes exist here
//!
//! `route` is accepted for exactly two values — `0x02`
//! (`ROUTE_RHN_TO_GLC`) and `0x04` (`ROUTE_RHN_TO_SOL`) — the two routes
//! on which Robinhood is the SOURCE and this event means "GLC arrived".
//!
//! `0x01` (`GlcToRhn`) and `0x03` (`SolToRhn`) are the payout direction.
//! The contract cannot emit `DepositCreated` for either: `deposit()`
//! resolves the route's legs and reverts with `NotADepositRoute` unless
//! the route is inbound, and `DepositCreated` is emitted nowhere else.
//! So an outbound or unknown route in a log carrying this topic0 is not a
//! deposit to reject and move past — it means the configured address is
//! not the contract this service believes it is. It is reported as
//! [`DepositDecodeError::NotAnInboundRoute`], and the indexer treats that
//! as a halt, not a skip. See [`super::indexer`].
//!
//! The two accepted ids are not spelled out again here: they are derived
//! from [`Route::contract_route_id`], the existing single source of truth
//! for the wire mapping, so the two cannot drift apart.

use thiserror::Error;

use crate::amount_conversion::robinhood::{RobinhoodAtomic, CANONICAL_TO_ROBINHOOD_SCALE};
use crate::amount_conversion::CanonicalAtomic;
use crate::evm::hash::EvmBlockHash;
use crate::evm::keccak256;
use crate::evm::{EvmAddress, EvmLogId, EvmLogLocation, EvmU256};
use crate::routes::Route;

use super::rpc::{EvmRawLog, EvmTopic};

/// The canonical event signature. Byte-for-byte what topic0 hashes; the
/// parameter types must be the ABI's own spellings (`uint256`, never
/// `uint`), and the whitespace must be exactly none.
pub const DEPOSIT_CREATED_SIGNATURE: &str =
    "DepositCreated(uint256,address,uint8,uint256,uint256,bytes)";

/// The contract's `MAX_DESTINATION_LEN`. A destination is opaque bytes on
/// the destination network and is never parsed here; only its length is
/// this decoder's business, and the bound mirrors the contract's own so a
/// payload the contract could not have stored is refused.
pub const MAX_DESTINATION_LEN: usize = 64;

/// The two routes on which a `DepositCreated` log can legitimately exist,
/// derived from the wire mapping rather than restated.
pub const INBOUND_DEPOSIT_ROUTES: [Route; 2] = [Route::RhnToGlc, Route::RhnToSol];

/// `topics[0]` for [`DEPOSIT_CREATED_SIGNATURE`].
///
/// Computed rather than pasted, so it cannot be a stale copy of a
/// signature that has since changed; the test module pins the resulting
/// value against the literal an independent keccak implementation
/// produces, which is what catches the signature itself being edited.
pub fn deposit_created_topic0() -> EvmTopic {
    keccak256(DEPOSIT_CREATED_SIGNATURE.as_bytes())
}

/// One decoded, fully validated deposit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositCreatedEvent {
    /// The contract-local obligation counter, narrowed at this boundary.
    /// See [`DepositDecodeError::ObligationIndexTooLarge`].
    pub obligation_index: u64,
    /// Always one of [`INBOUND_DEPOSIT_ROUTES`].
    pub route: Route,
    /// The wire byte, kept alongside the resolved route so both spellings
    /// are recorded facts rather than one derived from the other.
    pub contract_route_id: u8,
    pub depositor: EvmAddress,
    /// Opaque, 1..=[`MAX_DESTINATION_LEN`] bytes. Never parsed.
    pub destination: Vec<u8>,
    /// The raw 18-decimal amount, as the exact 256-bit word.
    pub amount_word: EvmU256,
    /// The same amount in the Robinhood token's own unit, narrowed to the
    /// service's existing amount model.
    pub amount: RobinhoodAtomic,
    /// The event's own `canonicalAmount`, proven equal to
    /// `amount / CANONICAL_SCALE`.
    pub canonical_amount: CanonicalAtomic,
    /// Which contract emitted it — the log's own `address`, not the
    /// configured one. The indexer checks the two agree.
    pub contract: EvmAddress,
    /// Identity and chain position, as [`crate::evm`] models them.
    pub location: EvmLogLocation,
}

impl DepositCreatedEvent {
    pub fn block_number(&self) -> u64 {
        self.location.block_number
    }

    pub fn block_hash(&self) -> EvmBlockHash {
        self.location.block_hash
    }

    pub fn log_id(&self) -> EvmLogId {
        self.location.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DepositDecodeError {
    #[error("log topic0 is not DepositCreated")]
    NotDepositCreated,
    #[error(
        "DepositCreated must carry exactly 4 topics (3 indexed parameters plus the signature), \
         found {found}"
    )]
    WrongTopicCount { found: usize },
    #[error(
        "topic {topic} encodes a {kind} but its upper {leading} byte(s) are not zero — a \
         left-padded value with a dirty pad is not a value this decoder will guess at"
    )]
    DirtyTopicPadding {
        topic: usize,
        kind: &'static str,
        leading: usize,
    },
    #[error(
        "DepositCreated data must be {expected} bytes for a {destination_len}-byte destination, \
         found {found}"
    )]
    WrongDataLength {
        expected: usize,
        found: usize,
        destination_len: usize,
    },
    #[error("DepositCreated data is {found} bytes, too short to hold even its fixed head")]
    DataTooShort { found: usize },
    #[error(
        "the destination's dynamic-tail offset is {found} but Solidity always encodes {expected} \
         for this parameter list — refusing to follow a hand-built pointer"
    )]
    UnexpectedTailOffset { expected: u64, found: String },
    #[error("the destination's declared length {found} does not fit this platform's usize")]
    DestinationLengthTooLarge { found: String },
    #[error(
        "destination length {found} is outside the contract's 1..={max} bound — the contract \
         could not have stored this"
    )]
    DestinationLengthOutOfRange { found: usize, max: usize },
    #[error("the destination's trailing ABI padding is not zero")]
    DirtyDestinationPadding,
    #[error(
        "route byte {route:#04x} is not an inbound deposit route (expected {:#04x} RhnToGlc or \
         {:#04x} RhnToSol) — the contract cannot emit DepositCreated for it, so the configured \
         address is not the contract this service expects",
        0x02,
        0x04
    )]
    NotAnInboundRoute { route: u8 },
    #[error("obligation index {found} exceeds what a contract-local deposit counter can be")]
    ObligationIndexTooLarge { found: String },
    #[error("deposit amount is zero")]
    ZeroAmount,
    #[error("amount {amount} exceeds the largest representable Robinhood atomic amount")]
    AmountTooLarge { amount: String },
    #[error("canonical amount {canonical} exceeds the ledger's canonical unit")]
    CanonicalAmountTooLarge { canonical: String },
    #[error(
        "the event's canonicalAmount {claimed} does not match amount {amount} divided by the \
         canonical scale ({derived}) — the two words that must agree do not"
    )]
    CanonicalAmountMismatch {
        amount: String,
        claimed: String,
        derived: String,
    },
}

/// The fixed head of the `data` blob: `amount`, `canonicalAmount`, and
/// the offset word for `destination`.
const DATA_HEAD_LEN: usize = 96;
/// The only offset Solidity emits for `destination` here — see the module
/// docs.
const DESTINATION_TAIL_OFFSET: u64 = 96;

/// Decodes one raw log into a validated deposit, or refuses it.
///
/// Returns [`DepositDecodeError::NotDepositCreated`] for any log whose
/// topic0 is something else. That is the ONE refusal a caller may treat
/// as "not for me": the bridge contract emits eleven other events, and
/// seeing one of them is ordinary. Every other variant means this log
/// claims to be a `DepositCreated` and is not one that can be believed —
/// see [`super::indexer`] for why those halt rather than skip.
pub fn decode_deposit_created(log: &EvmRawLog) -> Result<DepositCreatedEvent, DepositDecodeError> {
    if log.topics.first() != Some(&deposit_created_topic0()) {
        return Err(DepositDecodeError::NotDepositCreated);
    }
    if log.topics.len() != 4 {
        return Err(DepositDecodeError::WrongTopicCount {
            found: log.topics.len(),
        });
    }

    // ---- topics[1]: obligationIndex ------------------------------------
    let obligation_word = EvmU256::from_be_bytes(log.topics[1]);
    let obligation_index =
        obligation_word
            .try_to_u64()
            .map_err(|_| DepositDecodeError::ObligationIndexTooLarge {
                found: obligation_word.to_string(),
            })?;

    // ---- topics[2]: depositor (address, left-padded) --------------------
    let depositor_word = &log.topics[2];
    if depositor_word[..12].iter().any(|b| *b != 0) {
        return Err(DepositDecodeError::DirtyTopicPadding {
            topic: 2,
            kind: "address",
            leading: 12,
        });
    }
    let mut depositor_bytes = [0u8; 20];
    depositor_bytes.copy_from_slice(&depositor_word[12..]);
    let depositor = EvmAddress::from_bytes(depositor_bytes);

    // ---- topics[3]: route (uint8, left-padded) --------------------------
    let route_word = &log.topics[3];
    if route_word[..31].iter().any(|b| *b != 0) {
        return Err(DepositDecodeError::DirtyTopicPadding {
            topic: 3,
            kind: "uint8",
            leading: 31,
        });
    }
    let contract_route_id = route_word[31];
    let route = inbound_route_from_contract_id(contract_route_id).ok_or(
        DepositDecodeError::NotAnInboundRoute {
            route: contract_route_id,
        },
    )?;

    // ---- data: (uint256 amount, uint256 canonicalAmount, bytes) ---------
    if log.data.len() < DATA_HEAD_LEN {
        return Err(DepositDecodeError::DataTooShort {
            found: log.data.len(),
        });
    }
    let amount_word = word_at(&log.data, 0);
    let canonical_word = word_at(&log.data, 32);
    let offset_word = word_at(&log.data, 64);

    // A pointer wider than a `u64` is reported in its full 256-bit form
    // rather than narrowed, because the only thing that matters about it
    // is that it is not 0x60, and truncating it to say so would print a
    // number the log does not contain.
    if !matches!(offset_word.try_to_u64(), Ok(offset) if offset == DESTINATION_TAIL_OFFSET) {
        return Err(DepositDecodeError::UnexpectedTailOffset {
            expected: DESTINATION_TAIL_OFFSET,
            found: offset_word.to_string(),
        });
    }

    // The length word sits immediately after the head, which the length
    // check above has already proven is present.
    if log.data.len() < DATA_HEAD_LEN + 32 {
        return Err(DepositDecodeError::DataTooShort {
            found: log.data.len(),
        });
    }
    let length_word = word_at(&log.data, DATA_HEAD_LEN);
    let declared_len =
        length_word
            .try_to_u64()
            .map_err(|_| DepositDecodeError::DestinationLengthTooLarge {
                found: length_word.to_string(),
            })?;
    let declared_len = usize::try_from(declared_len).map_err(|_| {
        DepositDecodeError::DestinationLengthTooLarge {
            found: declared_len.to_string(),
        }
    })?;
    if declared_len == 0 || declared_len > MAX_DESTINATION_LEN {
        return Err(DepositDecodeError::DestinationLengthOutOfRange {
            found: declared_len,
            max: MAX_DESTINATION_LEN,
        });
    }

    // The blob must be EXACTLY the encoding of this destination: head,
    // length, payload, and the zero padding that rounds the payload up to
    // a 32-byte multiple. Not "at least" — a longer blob is carrying
    // something this decoder is not reading, and quietly ignoring it
    // would let one log mean two things.
    let padded_len = declared_len.div_ceil(32) * 32;
    let expected_len = DATA_HEAD_LEN + 32 + padded_len;
    if log.data.len() != expected_len {
        return Err(DepositDecodeError::WrongDataLength {
            expected: expected_len,
            found: log.data.len(),
            destination_len: declared_len,
        });
    }
    let payload_start = DATA_HEAD_LEN + 32;
    let destination = log.data[payload_start..payload_start + declared_len].to_vec();
    if log.data[payload_start + declared_len..]
        .iter()
        .any(|b| *b != 0)
    {
        return Err(DepositDecodeError::DirtyDestinationPadding);
    }

    // ---- the two amounts, and the invariant that ties them together ----
    if amount_word.is_zero() {
        return Err(DepositDecodeError::ZeroAmount);
    }
    let amount = RobinhoodAtomic::try_from_u256(amount_word).map_err(|_| {
        DepositDecodeError::AmountTooLarge {
            amount: amount_word.to_string(),
        }
    })?;
    let derived =
        amount
            .to_canonical()
            .map_err(|_| DepositDecodeError::CanonicalAmountMismatch {
                amount: amount_word.to_string(),
                claimed: canonical_word.to_string(),
                // `to_canonical` refuses a value that is not a whole multiple
                // of the scale, which is precisely the disagreement being
                // reported: the contract only ever emits exact multiples.
                derived: format!("not a whole multiple of {CANONICAL_TO_ROBINHOOD_SCALE}"),
            })?;
    let claimed =
        canonical_word
            .try_to_u64()
            .map_err(|_| DepositDecodeError::CanonicalAmountTooLarge {
                canonical: canonical_word.to_string(),
            })?;
    if claimed != derived.0 {
        return Err(DepositDecodeError::CanonicalAmountMismatch {
            amount: amount_word.to_string(),
            claimed: claimed.to_string(),
            derived: derived.0.to_string(),
        });
    }

    Ok(DepositCreatedEvent {
        obligation_index,
        route,
        contract_route_id,
        depositor,
        destination,
        amount_word,
        amount,
        canonical_amount: derived,
        contract: log.address,
        location: EvmLogLocation::new(
            EvmLogId::new(log.tx_hash, log.log_index),
            log.block_number,
            log.block_hash,
        ),
    })
}

/// The wire byte -> [`Route`] mapping, restricted to the inbound half.
///
/// Derived from [`Route::contract_route_id`] rather than written out, so
/// there is exactly one place the byte values live and no second copy to
/// fall out of step with the deployed bytecode.
pub fn inbound_route_from_contract_id(id: u8) -> Option<Route> {
    INBOUND_DEPOSIT_ROUTES
        .into_iter()
        .find(|route| route.contract_route_id() == Some(id))
}

/// The 32-byte ABI word starting at `offset`. The caller has already
/// proven the slice is long enough.
fn word_at(data: &[u8], offset: usize) -> EvmU256 {
    let mut word = [0u8; 32];
    word.copy_from_slice(&data[offset..offset + 32]);
    EvmU256::from_be_bytes(word)
}

#[cfg(test)]
mod tests;
