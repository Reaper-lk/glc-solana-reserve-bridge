//! `glc-admin robinhood-reserve-init`: give a ledger that has never seen a
//! Robinhood reserve its `RobinhoodReserve` row, seeded from the chain.
//!
//! # Why this exists
//!
//! The daemon is the only thing that creates a `reserve_ledger` row
//! (`glc-bridge-daemon` calls `Ledger::configure_reserve` at start-up with
//! a ZERO balance and lets reconciliation raise it to `balanceOf(bridge)`
//! on its first tick). That is the right behaviour for the production
//! ledger and the wrong tool for an ISOLATED one: starting a daemon
//! against a fresh database re-indexes every chain from its checkpoints
//! and would re-fold every historical deposit as a brand-new payout. A
//! ledger meant to exercise a second deployment — a successor bridge
//! holding a small test reserve — needs its row created and baselined
//! WITHOUT a daemon anywhere near it. That is all this does.
//!
//! # What "seeded from the chain" means
//!
//! The balance written is exactly `balanceOf(bridge)` as the token
//! contract reports it at the latest block, widened from the token's 18
//! decimals to the ledger's canonical 8 through the exactness-checked
//! conversion — never a figure from the command line, never a figure from
//! the config. `reserved_liquidity`, `pending_obligations` and every
//! other accounting column start at zero, which is only true of a ledger
//! that has NO Robinhood activity — so the command checks that first and
//! refuses otherwise ([`ReserveInitError::LedgerHasRobinhoodActivity`]).
//!
//! # The deployment is verified before anything is written
//!
//! [`run`] goes through [`super::preflight::verify`] — the same
//! chain-id / contract-code / protocol-family / token / decimals /
//! signer-set / domain-separator gate the daemon refuses to start
//! without — so a config pointed at the wrong network, the wrong
//! contract, or a contract custodying a different token fails closed
//! with the preflight's own message, and no row is created.
//!
//! # Idempotency, narrowly
//!
//! A row that already exists is refused unless it already says exactly
//! what this command would write: balance equal to the chain, protected
//! minimum equal to the config, nothing reserved, nothing pending. That
//! case is reported as [`ReserveInitOutcome::AlreadyInitialized`] and
//! nothing is written. Anything else — a different balance, a live
//! reservation, a different floor — is a ledger with a history, and a
//! command whose job is to create a baseline must not overwrite one.
//! There is deliberately no `--force`.
//!
//! # It does not know which ledger is "production"
//!
//! The ledger is whatever `[service].db_path` names. The activity guard
//! and the existing-row refusal are what make pointing this at a live
//! ledger harmless: a production ledger has a reserve row and has
//! activity, so it is refused on both counts before a write. But the
//! operator, not this code, is responsible for pointing it at the
//! isolated file in the first place.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::amount_conversion::CanonicalAtomic;
use crate::config::ReserveBounds;
use crate::evm::{EvmAddress, EvmU256};
use crate::ledger::{Ledger, LedgerError, ReserveDirection, RobinhoodActivity};

use super::calls::{ContractReadError, TokenReader};
use super::config::RobinhoodIndexerConfig;
use super::preflight::{self, PreflightError, VerifiedDeployment};
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmRpc, EvmSubmitRpc};
use super::settlement_config::RobinhoodSettlementConfig;

/// Why the reserve could not be initialized. Every variant is a refusal
/// BEFORE any write.
#[derive(Debug, thiserror::Error)]
pub enum ReserveInitError {
    #[error(
        "this config has no [reserve.robinhood] section, so there is no protected minimum or \
         threshold band to install. Add one (protected_minimum / critical_reserve / \
         warning_reserve / target_reserve, canonical 8dp) and re-run"
    )]
    NoReserveBounds,
    #[error(
        "[reserve.robinhood].critical_reserve ({critical}) must exceed protected_minimum \
         ({protected_minimum}) — the daemon asserts the same invariant"
    )]
    BoundsInvalid {
        protected_minimum: u64,
        critical: u64,
    },
    #[error("the deployment did not verify, so nothing was written: {0}")]
    Preflight(#[from] PreflightError),
    #[error("reading balanceOf({bridge}) on the token: {source}")]
    BalanceRead {
        bridge: String,
        #[source]
        source: ContractReadError,
    },
    #[error(
        "balanceOf(bridge) = {atomic} (18dp) is not an exact multiple of the canonical scale \
         (1e10), so it cannot be carried in the ledger's 8dp unit without rounding — refusing \
         to seed a figure the chain does not hold: {detail}"
    )]
    NotCanonical { atomic: String, detail: String },
    #[error(
        "this ledger already has Robinhood activity ({activity}); a baseline with zero reserved \
         liquidity and zero pending obligations would be a lie about it. This command is for a \
         ledger that has never accounted a Robinhood operation — an isolated one, not this"
    )]
    LedgerHasRobinhoodActivity { activity: String },
    #[error(
        "a RobinhoodReserve row already exists and does not say what this command would write \
         — balance {balance} (chain says {observed}), protected minimum {protected_minimum} \
         (config says {configured_minimum}), reserved {reserved}, pending {pending}. Refusing \
         to overwrite a baseline with a history; there is no --force"
    )]
    RowExistsAndDiffers {
        balance: u64,
        observed: u64,
        protected_minimum: u64,
        configured_minimum: u64,
        reserved: u64,
        pending: u64,
    },
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

/// What [`run`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveInitOutcome {
    /// The row was created and baselined from the chain.
    Initialized(ReserveInitReport),
    /// The row already held exactly this; nothing was written.
    AlreadyInitialized(ReserveInitReport),
}

impl ReserveInitOutcome {
    pub fn report(&self) -> &ReserveInitReport {
        match self {
            ReserveInitOutcome::Initialized(r) | ReserveInitOutcome::AlreadyInitialized(r) => r,
        }
    }
}

/// The figures an operator reads back, in both units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveInitReport {
    pub bridge: EvmAddress,
    pub token: EvmAddress,
    pub chain_id: u64,
    /// `balanceOf(bridge)`, 18dp, exactly as read.
    pub observed_robinhood_atomic: EvmU256,
    /// The same figure in the ledger's unit.
    pub balance_canonical: CanonicalAtomic,
    pub bounds: ReserveBounds,
}

/// Verifies the deployment, reads the reserve balance, and — on a ledger
/// with no Robinhood history — creates and baselines the
/// `RobinhoodReserve` row. Never starts anything, never reads a key.
pub async fn run<R>(
    rpc: &R,
    ledger: &mut Ledger,
    indexer: &RobinhoodIndexerConfig,
    settlement: &RobinhoodSettlementConfig,
    bounds: Option<ReserveBounds>,
    now: i64,
) -> Result<ReserveInitOutcome, ReserveInitError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    // ---- config, before the chain is asked anything ----
    let bounds = bounds.ok_or(ReserveInitError::NoReserveBounds)?;
    if bounds.critical_reserve <= bounds.protected_minimum {
        return Err(ReserveInitError::BoundsInvalid {
            protected_minimum: bounds.protected_minimum,
            critical: bounds.critical_reserve,
        });
    }

    // ---- the deployment, exactly as the daemon would refuse it ----
    let verified: VerifiedDeployment = preflight::verify(rpc, indexer, settlement).await?;

    // ---- the authoritative balance ----
    let observed = TokenReader::new(verified.token)
        .balance_of(rpc, verified.bridge_contract, EvmBlockTag::Latest)
        .await
        .map_err(|source| ReserveInitError::BalanceRead {
            bridge: verified.bridge_contract.to_checksum_string(),
            source,
        })?;
    let balance_canonical = observed
        .try_to_u128()
        .map_err(|e| ReserveInitError::NotCanonical {
            atomic: observed.to_word_hex(),
            detail: e.to_string(),
        })
        .and_then(|raw| {
            RobinhoodAtomic::new(raw)
                .to_canonical()
                .map_err(|e| ReserveInitError::NotCanonical {
                    atomic: raw.to_string(),
                    detail: e.to_string(),
                })
        })?;

    let report = ReserveInitReport {
        bridge: verified.bridge_contract,
        token: verified.token,
        chain_id: verified.chain_id.get(),
        observed_robinhood_atomic: observed,
        balance_canonical,
        bounds,
    };

    // ---- the ledger must have no Robinhood history ----
    let activity: RobinhoodActivity = ledger.robinhood_activity()?;
    if !activity.is_empty() {
        return Err(ReserveInitError::LedgerHasRobinhoodActivity {
            activity: activity.to_string(),
        });
    }

    // ---- an existing row is accepted only if it already says this ----
    match ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve) {
        Ok((balance, protected_minimum, reserved, pending)) => {
            let same = balance == balance_canonical.0
                && protected_minimum == bounds.protected_minimum
                && reserved == 0
                && pending == 0;
            if same {
                return Ok(ReserveInitOutcome::AlreadyInitialized(report));
            }
            return Err(ReserveInitError::RowExistsAndDiffers {
                balance,
                observed: balance_canonical.0,
                protected_minimum,
                configured_minimum: bounds.protected_minimum,
                reserved,
                pending,
            });
        }
        Err(LedgerError::ReserveNotInitialized(_)) => {}
        Err(other) => return Err(other.into()),
    }

    // ---- create, then baseline — two writes the daemon and reconciliation
    // would otherwise make, in the same order ----
    ledger.configure_reserve(
        ReserveDirection::RobinhoodReserve,
        balance_canonical.0,
        bounds.protected_minimum,
        bounds.target_reserve,
        bounds.warning_reserve,
        bounds.critical_reserve,
        now,
    )?;
    ledger.refresh_reserve_balance(ReserveDirection::RobinhoodReserve, balance_canonical.0, now)?;

    Ok(ReserveInitOutcome::Initialized(report))
}

#[cfg(test)]
mod tests;
