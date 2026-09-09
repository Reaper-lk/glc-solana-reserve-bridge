//! Folding a FINALIZED Robinhood deposit observation into exactly one
//! `bridge_requests` row.
//!
//! # The one job, stated precisely
//!
//! A `DepositCreated` event that this service has observed, decoded,
//! recorded, and watched reach the configured confirmation depth is an
//! irreversible transfer of a user's GLC into the custody contract. From
//! that moment the bridge owes the user a Goldcoin payout OR a Robinhood
//! refund — never nothing, and never both. This module is where that
//! obligation enters the ledger.
//!
//! # Why folding happens even when the route is disabled
//!
//! This is the most important design decision in the module and it is
//! deliberately the opposite of what "the route is off" suggests.
//!
//! A deposit that has already landed on-chain cannot be un-landed by a
//! flag on this side. If a disabled route meant "do not fold", then
//! turning a route off — or a deployment simply not being configured for
//! it yet — would leave real, irreversible deposits with no ledger row,
//! no reserve accounting, no operator visibility and no refund path. The
//! money would be in the contract and nothing in this service would know.
//!
//! So a finalized deposit is ALWAYS folded, and the route gate governs
//! what happens NEXT: a request folded while the route is closed lands in
//! `ManualReview`, holds no reserve, pays out nothing, and is visible to
//! an operator with an explicit reason. When the route later opens it can
//! be resumed through the existing manual-review resume path, or refunded
//! through the Robinhood refund path — both of which are deliberate acts.
//!
//! That is the same posture `Ledger::fold_sol_deposit` already takes for
//! a Solana deposit that arrives while the Goldcoin reserve is paused: the
//! deposit is recorded, parked, and made visible, rather than dropped.
//!
//! # Amount handling
//!
//! The event carries BOTH the 18-decimal `amount` and the contract's own
//! `canonicalAmount`, and the Phase E decoder already cross-checked that
//! the second is exactly the first divided by `CANONICAL_SCALE`. This
//! module re-derives the canonical amount from the 18-decimal word a
//! second time, through
//! [`crate::amount_conversion::robinhood::RobinhoodAtomic::to_canonical`],
//! and refuses on any disagreement.
//!
//! Deriving it again is not redundant caution about the decoder: it is
//! what makes the exactness rule apply at the moment the amount becomes
//! MONEY IN A LEDGER rather than at the moment it was read off a wire. An
//! amount that is not an exact multiple of 10^10 has no canonical
//! representation, so it cannot be admitted — and because the contract
//! refuses such a deposit on-chain (`_requireCanonicalAmount`), observing
//! one means the contract and this service disagree about what was
//! deposited, which is not a rounding decision to make.
//!
//! # Fee
//!
//! The normal bridge fee policy, in canonical units, through the one fee
//! engine every route uses
//! ([`crate::amount_conversion::compute_fee_at_bps`]). There is still no
//! second Robinhood fee PATH and there must not be one — a second way of
//! computing what a user is owed is a second thing that can be wrong.
//!
//! What is Robinhood-specific is the RATE, and only the rate: it is a
//! parameter of this function rather than a constant read inside it, so
//! the chain launching under different commercial terms changes one
//! number rather than adding an arithmetic path. The caller supplies it
//! from [`crate::chain_policy::ChainPolicies::fee_bps_for`], which falls
//! back to the compiled-in [`crate::amount_conversion::BRIDGE_FEE_BPS`]
//! for any chain with no configured policy — so a deployment with no
//! `[robinhood.policy]` section prices exactly as it did before, and the
//! Goldcoin<->Solana routes are untouched either way.
//!
//! The rate is snapshotted onto the request, and every later step settles
//! at THAT snapshot, so changing the configured rate cannot re-price
//! anything already in flight.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic};
use crate::evm::EvmU256;
use crate::ledger::{Ledger, LedgerError, RobinhoodObservationRow};
use crate::routes::Route;

/// Why one observation could not be folded.
///
/// Every variant is a refusal to create a request, never a silent skip:
/// a finalized deposit that cannot be folded is a condition an operator
/// has to see.
#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(
        "observation {obligation_index}'s 18-decimal amount cannot be represented exactly in \
         canonical units: {detail}. The contract refuses a non-canonical deposit on-chain, so \
         observing one means this service and the contract disagree about what was deposited"
    )]
    NotCanonical {
        obligation_index: u64,
        detail: String,
    },
    #[error(
        "observation {obligation_index} records canonicalAmount {recorded} but its 18-decimal \
         amount scales down to {derived} — two independently recorded facts about one deposit \
         disagree"
    )]
    AmountDisagreement {
        obligation_index: u64,
        recorded: u64,
        derived: u64,
    },
    #[error("observation {obligation_index}: computing the bridge fee failed: {detail}")]
    Fee {
        obligation_index: u64,
        detail: String,
    },
    #[error(
        "observation {obligation_index} is on route {route}, which this service cannot settle: \
         only RhnToGlc is executable"
    )]
    UnsupportedRoute {
        obligation_index: u64,
        route: &'static str,
    },
    #[error(
        "observation {obligation_index} has finality {finality}, but only a FINAL observation may \
         be folded — a provisional deposit can still be reorged away"
    )]
    NotFinal {
        obligation_index: u64,
        finality: &'static str,
    },
    #[error(
        "observation {obligation_index}'s destination payload is not a usable Goldcoin address: \
         {detail}. The deposit is real and irreversible; it must be refunded on Robinhood rather \
         than paid out to a guess"
    )]
    UndeliverableDestination {
        obligation_index: u64,
        detail: String,
    },
}

/// What [`fold_observation`] did with one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldOutcome {
    /// A new request was created and is eligible to pay out: the route is
    /// open and the destination reserve had capacity.
    FoldedFinalized { request_id: i64 },
    /// A new request was created and PARKED in `ManualReview`. The
    /// deposit is recorded and visible; nothing is reserved and nothing
    /// will pay out until a human resumes or refunds it.
    FoldedManualReview { request_id: i64 },
    /// This observation already has a request. The normal, expected
    /// result of a re-tick or a restart.
    AlreadyFolded { request_id: i64 },
}

impl FoldOutcome {
    pub fn request_id(self) -> i64 {
        match self {
            FoldOutcome::FoldedFinalized { request_id }
            | FoldOutcome::FoldedManualReview { request_id }
            | FoldOutcome::AlreadyFolded { request_id } => request_id,
        }
    }
}

/// The canonical amounts one observation resolves to, with every
/// exactness rule applied.
///
/// Split out from the fold itself so the arithmetic can be tested against
/// adversarial values without a database, and so the fold reads as the
/// admission decision it is rather than as arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldAmounts {
    pub gross_canonical: u64,
    pub fee_bps: u64,
    pub fee_canonical: u64,
    pub net_canonical: u64,
}

/// Resolves and cross-checks one observation's amounts at `fee_bps`.
///
/// `fee_bps` is the rate this chain's approved policy prices at. It is
/// passed in rather than read from a constant so that one chain's
/// commercial terms cannot become another's; it is still validated
/// against [`crate::amount_conversion::HISTORICAL_FEE_BPS`] inside
/// `compute_fee_at_bps`, so a rate the protocol does not know fails
/// closed here rather than producing a request that could never settle.
pub fn resolve_amounts(
    observation: &RobinhoodObservationRow,
    fee_bps: u64,
) -> Result<FoldAmounts, FoldError> {
    let obligation_index = observation.observation.obligation_index;

    // The 18-decimal word, narrowed through the one conversion that
    // enforces exactness. `RobinhoodAtomic::try_from_u256` additionally
    // refuses a word above `u128::MAX` rather than truncating it.
    let robinhood = RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes(
        observation.observation.amount_robinhood_atomic,
    ))
    .map_err(|e| FoldError::NotCanonical {
        obligation_index,
        detail: e.to_string(),
    })?;
    let derived = robinhood
        .to_canonical()
        .map_err(|e| FoldError::NotCanonical {
            obligation_index,
            detail: e.to_string(),
        })?;

    // The contract emitted `canonicalAmount` as its own field and the
    // decoder already checked it against the 18-decimal word. Checking it
    // AGAIN here is what makes a single stored number trustworthy at the
    // moment it becomes an entitlement: two independent derivations of
    // one value, agreeing.
    if derived.0 != observation.observation.amount_canonical_atomic {
        return Err(FoldError::AmountDisagreement {
            obligation_index,
            recorded: observation.observation.amount_canonical_atomic,
            derived: derived.0,
        });
    }

    // The normal fee policy at this chain's approved rate — the same fee
    // engine every other route uses. The snapshot is stored on the
    // request and every later step settles at THAT rate, so a rate change
    // mid-flight cannot alter what this user is owed.
    let breakdown =
        compute_fee_at_bps(CanonicalAtomic(derived.0), fee_bps).map_err(|e| FoldError::Fee {
            obligation_index,
            detail: e.to_string(),
        })?;

    Ok(FoldAmounts {
        gross_canonical: breakdown.gross.0,
        fee_bps: breakdown.fee_bps,
        fee_canonical: breakdown.fee.0,
        net_canonical: breakdown.net.0,
    })
}

/// Validates that an observation's opaque destination payload is a
/// Goldcoin address this service can actually pay out to.
///
/// # Why this is checked at fold time and not at payout time
///
/// A destination that cannot be paid out is a deposit that must be
/// REFUNDED, and the sooner that is known the better: parking it in
/// `ManualReview` with an explicit reason at fold time puts it in front
/// of an operator immediately, rather than surfacing as a payout failure
/// after the route opens.
///
/// The contract deliberately does not parse addresses — it stores opaque
/// bytes and lets the route say what they mean — so this is the first
/// point at which anything checks that the bytes are an address at all.
pub fn validate_goldcoin_destination(
    observation: &RobinhoodObservationRow,
    network: crate::goldcoin::address::Network,
) -> Result<String, FoldError> {
    let obligation_index = observation.observation.obligation_index;
    let text = std::str::from_utf8(&observation.observation.destination).map_err(|_| {
        FoldError::UndeliverableDestination {
            obligation_index,
            detail: "the destination payload is not valid UTF-8, so it is not a Base58Check \
                     Goldcoin address"
                .to_string(),
        }
    })?;
    // P2PKH specifically, and on THIS network: the payout builder
    // (`signing::goldcoin_vault`) decodes the recipient the same way and
    // would refuse anything else, so accepting a broader form here would
    // only defer the failure to a point where a reserve reservation had
    // already been taken.
    crate::goldcoin::address::decode_p2pkh(text, network).map_err(|e| {
        FoldError::UndeliverableDestination {
            obligation_index,
            detail: e.to_string(),
        }
    })?;
    Ok(text.to_string())
}

/// Folds one FINAL observation into a bridge request.
///
/// Idempotent by the ledger's own unique indexes — the durable
/// `(source_chain, source_contract, source_obligation_index)` identity
/// and the `folded_request_id` link — not by a prior read: two ticks
/// racing each other both attempt the insert and exactly one wins.
pub fn fold_observation(
    ledger: &mut Ledger,
    observation: &RobinhoodObservationRow,
    network: crate::goldcoin::address::Network,
    fee_bps: u64,
    route_open: bool,
    now: i64,
) -> Result<FoldOutcome, FoldError> {
    let obligation_index = observation.observation.obligation_index;

    if observation.finality != crate::ledger::RobinhoodFinality::Final {
        return Err(FoldError::NotFinal {
            obligation_index,
            finality: observation.finality.as_str(),
        });
    }
    if observation.observation.route != Route::RhnToGlc {
        return Err(FoldError::UnsupportedRoute {
            obligation_index,
            route: observation.observation.route.as_str(),
        });
    }

    let amounts = resolve_amounts(observation, fee_bps)?;

    // A destination this service cannot pay out to is folded anyway — the
    // deposit is real — but never as payable. It is parked with an
    // explicit reason so the refund path is the obvious next step.
    let destination = match validate_goldcoin_destination(observation, network) {
        Ok(address) => Some(address),
        Err(FoldError::UndeliverableDestination { detail, .. }) => {
            return ledger
                .fold_robinhood_deposit(
                    observation,
                    amounts.gross_canonical,
                    amounts.fee_bps,
                    amounts.fee_canonical,
                    amounts.net_canonical,
                    None,
                    false,
                    Some(&format!("undeliverable destination: {detail}")),
                    now,
                )
                .map_err(FoldError::from);
        }
        Err(other) => return Err(other),
    };

    ledger
        .fold_robinhood_deposit(
            observation,
            amounts.gross_canonical,
            amounts.fee_bps,
            amounts.fee_canonical,
            amounts.net_canonical,
            destination.as_deref(),
            route_open,
            None,
            now,
        )
        .map_err(FoldError::from)
}

#[cfg(test)]
mod tests;
