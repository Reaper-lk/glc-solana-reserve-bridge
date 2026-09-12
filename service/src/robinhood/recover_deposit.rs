//! Out-of-band recovery of ONE confirmed Robinhood deposit that the
//! scanner will never see: a `deposit()` that landed on a custody
//! contract other than the one `[robinhood.indexer]` names.
//!
//! # The incident this exists for
//!
//! After production was cut over to the V2 custody contract, two users'
//! `RhnToSol` deposits (V1 obligations #29 and #30) landed on V1 — the
//! public UI still named it and V1 was still accepting deposits. The
//! scanner filters `eth_getLogs` by the configured contract, exactly as
//! it should, so the deposits were never observed, never folded, and
//! never became refundable. Nothing in the scanner was wrong; the ledger
//! simply had no row for a deposit that provably happened.
//!
//! # What this does, and what it refuses to do
//!
//! Given a transaction hash and a config whose `[robinhood.indexer]`
//! names the contract the deposit was made to, [`recover`]:
//!
//! 1. fetches the RECEIPT, and refuses a transaction that is not mined
//!    or that reverted;
//! 2. takes the ONE `DepositCreated` log whose emitting address is the
//!    configured contract — a log from any other address is ignored, and
//!    a receipt with none, or more than one, is refused;
//! 3. decodes it with [`super::deposit_event::decode_deposit_created`],
//!    the scanner's own decoder — the same ABI, the same route mapping,
//!    the same destination handling;
//! 4. proves the deposit is FINAL by the scanner's own rule — at least
//!    `confirmation_depth` blocks below the head — and CANONICAL — the
//!    block the log names still carries the hash the log names;
//! 5. records the observation as `Final` under its durable identity
//!    `(robinhood, contract, obligation_index)`
//!    ([`Ledger::robinhood_record_final_observation`]) — no scan anchor,
//!    no cursor movement, and a second run finds the row and writes
//!    nothing;
//! 6. folds it through the SAME fold the daemon uses for that route
//!    ([`super::fold::fold_observation`] / [`fold_observation_to_solana`]),
//!    with the route reported CLOSED — so the request lands in
//!    `ManualReview`, holds no reserve capacity, and is refundable through
//!    `robinhood-refund` like any other parked deposit. A second run
//!    finds it already folded and creates nothing.
//!
//! It never manufactures a request from arguments: every figure in the
//! ledger row comes from the decoded log. It never pays out, never
//! refunds, and never touches any other request.
//!
//! [`fold_observation_to_solana`]: super::fold::fold_observation_to_solana

use crate::amount_conversion::CanonicalAtomic;
use crate::evm::{EvmAddress, EvmBlockHash, EvmTxHash};
use crate::ledger::{
    Ledger, LedgerError, RobinhoodDepositObservation, RobinhoodObservationOutcome,
};
use crate::routes::Route;

use super::config::RobinhoodIndexerConfig;
use super::deposit_event::{decode_deposit_created, deposit_created_topic0, DepositDecodeError};
use super::fold::{self, FoldError, FoldOutcome};
use super::rpc::{EvmRpc, EvmRpcError, EvmSubmitRpc};

/// Why a deposit could not be recovered. Every variant is a refusal
/// BEFORE any write, except [`Self::Fold`], which can only follow a
/// successfully recorded observation and leaves that observation in
/// place for a retry.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error("rpc: {0}")]
    Rpc(#[from] EvmRpcError),
    #[error("transaction {tx} is not mined (no receipt); nothing to recover")]
    NotMined { tx: String },
    #[error(
        "transaction {tx} REVERTED (receipt status 0); it created no obligation and there is \
         nothing to recover"
    )]
    Reverted { tx: String },
    #[error(
        "transaction {tx} emitted no DepositCreated log from {contract} — either it is not a \
         deposit, or it was made to a different contract than [robinhood.indexer] names \
         (logs from other addresses: {other_addresses})"
    )]
    NoDepositLog {
        tx: String,
        contract: String,
        other_addresses: String,
    },
    #[error(
        "transaction {tx} emitted {count} DepositCreated logs from {contract}; one deposit per \
         transaction is the only shape this recovers"
    )]
    MultipleDepositLogs {
        tx: String,
        contract: String,
        count: usize,
    },
    #[error("the DepositCreated log could not be decoded: {0}")]
    Decode(#[from] DepositDecodeError),
    #[error(
        "deposit in block {block} is only {depth} block(s) below head {head}; the scanner's \
         finality rule requires {required}. Re-run later"
    )]
    NotFinal {
        block: u64,
        head: u64,
        depth: u64,
        required: u64,
    },
    #[error(
        "block {block} is no longer canonical: the log names hash {claimed} but the chain now \
         holds {live}. A reorged deposit is not a deposit; nothing recorded"
    )]
    Reorged {
        block: u64,
        claimed: String,
        live: String,
    },
    #[error(
        "block {block} is unknown to the endpoint, so the deposit's canonical hash cannot be \
         confirmed; nothing recorded"
    )]
    BlockUnavailable { block: u64 },
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("folding the recovered observation: {0}")]
    Fold(#[from] FoldError),
}

/// What a deposit's recovery needs beyond the chain: the route's fee
/// rate, the source-side floor, and — for a Solana-bound deposit — the
/// reserve mint's decimals, all supplied by the caller from config and
/// live reads exactly as the daemon supplies them to the fold.
#[derive(Debug, Clone, Copy)]
pub struct RecoverInputs {
    pub fee_bps: u64,
    pub source_minimum: CanonicalAtomic,
    /// Required for `RhnToSol`; ignored for `RhnToGlc`.
    pub solana_decimals: Option<u8>,
    pub goldcoin_network: crate::goldcoin::address::Network,
}

/// Everything [`recover`] established and did, for the operator's eyes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub tx_hash: EvmTxHash,
    pub block_number: u64,
    pub block_hash: EvmBlockHash,
    pub contract: EvmAddress,
    pub obligation_index: u64,
    pub route: Route,
    pub depositor: EvmAddress,
    pub destination: Vec<u8>,
    pub amount_canonical: CanonicalAtomic,
    pub observation: RobinhoodObservationOutcome,
    pub fold: FoldOutcome,
}

impl Recovered {
    pub fn request_id(&self) -> i64 {
        self.fold.request_id()
    }
}

/// The read-only half: receipt, log, decode, finality, canonical hash.
/// What the dry run prints, and what [`recover`] runs before it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDeposit {
    pub observation: RobinhoodDepositObservation,
    pub route: Route,
    pub depositor: EvmAddress,
    pub head: u64,
}

pub async fn verify<R>(
    rpc: &R,
    indexer: &RobinhoodIndexerConfig,
    tx_hash: EvmTxHash,
) -> Result<VerifiedDeposit, RecoverError>
where
    R: EvmRpc + EvmSubmitRpc,
{
    let tx = tx_hash.to_string();
    let receipt = rpc
        .transaction_receipt(tx_hash)
        .await?
        .ok_or_else(|| RecoverError::NotMined { tx: tx.clone() })?;
    if !receipt.success {
        return Err(RecoverError::Reverted { tx });
    }

    // ---- the one DepositCreated from THE configured contract ----
    let topic0 = deposit_created_topic0();
    let contract = indexer.bridge_contract;
    let candidates: Vec<_> = receipt
        .logs
        .iter()
        .filter(|l| l.address == contract && l.topics.first() == Some(&topic0))
        .collect();
    match candidates.len() {
        0 => {
            let mut others: Vec<String> = receipt
                .logs
                .iter()
                .filter(|l| l.address != contract)
                .map(|l| l.address.to_checksum_string())
                .collect();
            others.sort();
            others.dedup();
            return Err(RecoverError::NoDepositLog {
                tx,
                contract: contract.to_checksum_string(),
                other_addresses: if others.is_empty() {
                    "none".to_string()
                } else {
                    others.join(", ")
                },
            });
        }
        1 => {}
        count => {
            return Err(RecoverError::MultipleDepositLogs {
                tx,
                contract: contract.to_checksum_string(),
                count,
            })
        }
    }
    let event = decode_deposit_created(candidates[0])?;
    debug_assert_eq!(event.contract, contract);

    // ---- final by the scanner's rule, canonical by the chain's ----
    let head = rpc.block_number().await?;
    let block = event.block_number();
    let depth = head.saturating_sub(block);
    if depth < indexer.confirmation_depth {
        return Err(RecoverError::NotFinal {
            block,
            head,
            depth,
            required: indexer.confirmation_depth,
        });
    }
    let live = rpc
        .block_by_number(block)
        .await?
        .ok_or(RecoverError::BlockUnavailable { block })?;
    if live.hash != event.block_hash() {
        return Err(RecoverError::Reorged {
            block,
            claimed: event.block_hash().to_string(),
            live: live.hash.to_string(),
        });
    }

    // The scanner's own shape, field for field (`indexer::scan_chunk`).
    let observation = RobinhoodDepositObservation {
        source_contract: event.contract.to_bytes(),
        obligation_index: event.obligation_index,
        route: event.route,
        depositor: event.depositor.to_bytes(),
        destination: event.destination.clone(),
        amount_robinhood_atomic: event.amount_word.to_be_bytes(),
        amount_canonical_atomic: event.canonical_amount.0,
        tx_hash: event.location.id.tx_hash.to_bytes(),
        log_index: event.location.id.log_index,
        block_number: block,
        block_hash: event.block_hash().to_bytes(),
    };
    Ok(VerifiedDeposit {
        observation,
        route: event.route,
        depositor: event.depositor,
        head,
    })
}

/// Verifies, records as `Final`, and folds with the route CLOSED. See
/// the module docs for every step and every refusal.
pub async fn recover<R>(
    rpc: &R,
    ledger: &mut Ledger,
    indexer: &RobinhoodIndexerConfig,
    tx_hash: EvmTxHash,
    inputs: RecoverInputs,
    now: i64,
) -> Result<Recovered, RecoverError>
where
    R: EvmRpc + EvmSubmitRpc,
{
    let verified = verify(rpc, indexer, tx_hash).await?;
    let observation_outcome =
        ledger.robinhood_record_final_observation(&verified.observation, now)?;

    let row = ledger
        .robinhood_observation_by_source(
            verified.observation.source_contract,
            verified.observation.obligation_index,
        )?
        .expect("the observation was recorded or already present a moment ago");

    // The route is passed CLOSED on purpose: a recovered deposit is parked,
    // refundable, and never paid out by this path — whatever the live
    // gates say. The fold's own refusals (destination, floor, amount)
    // rank ahead of that and are recorded as their own reasons.
    let fold = match verified.route {
        Route::RhnToSol => fold::fold_observation_to_solana(
            ledger,
            &row,
            inputs.fee_bps,
            inputs.source_minimum,
            inputs.solana_decimals.ok_or_else(|| {
                RecoverError::Fold(FoldError::UnsupportedRoute {
                    obligation_index: row.observation.obligation_index,
                    route: "RhnToSol without the reserve mint's decimals",
                })
            })?,
            false,
            now,
        )?,
        _ => fold::fold_observation(
            ledger,
            &row,
            inputs.goldcoin_network,
            inputs.fee_bps,
            inputs.source_minimum,
            false,
            now,
        )?,
    };

    Ok(Recovered {
        tx_hash,
        block_number: verified.observation.block_number,
        block_hash: EvmBlockHash::from_bytes(verified.observation.block_hash),
        contract: EvmAddress::from_bytes(verified.observation.source_contract),
        obligation_index: verified.observation.obligation_index,
        route: verified.route,
        depositor: verified.depositor,
        destination: verified.observation.destination.clone(),
        amount_canonical: CanonicalAtomic(verified.observation.amount_canonical_atomic),
        observation: observation_outcome,
        fold,
    })
}

#[cfg(test)]
mod tests;
