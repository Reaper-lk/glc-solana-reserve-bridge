//! Automatic vault-UTXO liquidity shaping: the daemon-driven counterpart
//! to the operator CLI `glc-admin split-vault-utxo`, closing the
//! 2026-08-30 production incident (docs/09-runbook.md's "Automatic UTXO
//! liquidity shaping" section): SolToGlc payouts repeatedly failing with
//! `TooManyInputs` because the mature pool had degraded into fragments
//! while large liquidity sat either as one oversized deposit UTXO or as
//! still-maturing payout change — with the only remedy being manual,
//! operator-run UTXO splits.
//!
//! # The split lifecycle
//!
//! Every split — automatic or CLI-initiated — moves through one state
//! machine, persisted in `vault_utxo_splits`:
//!
//! ```text
//! Built ──> Signed ──> Broadcast ──> Confirmed        (terminal)
//!   │          │            │
//!   └──────────┴────────────┴──────> Abandoned        (terminal)
//! ```
//!
//! - **`Built` IS the claim on the source outpoint.** From the moment
//!   `Ledger::record_vault_utxo_split_built` commits, the source is
//!   excluded from payout coin selection AND from the payout reservation
//!   guard (`Ledger::available_vault_utxos` / `reserve_vault_utxos`), so
//!   a payout and a split can never commit to the same UTXO regardless
//!   of process interleaving or restarts. Claiming happens BEFORE any
//!   signer round-trip — the round-trip is exactly the window the
//!   2026-08-30 review found a concurrent daemon could race.
//! - **`Broadcast` is not terminal.** [`maintain_broadcast_splits`]
//!   drives every `Broadcast` split onward each tick: the first observed
//!   confirmation (via the service's own synced chain view, never a
//!   node-claimed status) marks it `Confirmed`; a split the node no
//!   longer knows (mempool eviction) is re-broadcast from its exact
//!   stored bytes; a re-broadcast the node rejects with missing inputs
//!   is `Abandoned` — its phantom chunk rows are marked `Spent` in the
//!   same transaction, so accounting never keeps explaining value that
//!   cannot arrive.
//! - **`Abandoned` is the release valve, never a wedge.** A pending
//!   split whose source became unspendable (spent externally, reorged
//!   away, or refused by the reserve-floor check on resume) is abandoned
//!   — audit row kept forever, claim released, and the partial unique
//!   index (`ux_vault_utxo_splits_source`, v16) lets the outpoint be
//!   split again if it legitimately returns. No state can permanently
//!   stall shaping, and no path requires manual SQLite edits.
//!
//! # What one shaping tick does
//!
//! [`run_shaping_tick`] performs bookkeeping (confirmation marking) plus
//! AT MOST ONE transaction-shaped action per tick, in strict priority
//! order: (1) re-broadcast or abandon a `Broadcast` split that needs it;
//! (2) resume (or abandon) the oldest `Built`/`Signed` split; (3) only
//! while the payout-ready pool — mature `Available` UTXOs plus, at the
//! configured `zero_conf_change_max_depth`, currently-eligible 0-conf
//! payout change — is BELOW [`ShapingPolicy::target_available_count`],
//! consider one NEW split of the largest mature, unreserved,
//! never-successfully-split root-vault UTXO of at least
//! [`ShapingPolicy::min_source_atomic`].
//!
//! A new split goes through the IDENTICAL independent 2-of-3 signing
//! path the operator CLI uses ([`crate::signing::goldcoin_split`],
//! always via `RecoverySplitSource` — every signer independently proves
//! the plan it is signing serializes byte-identically to the persisted
//! unsigned transaction), including every signer's own non-overridable
//! reserve-floor refusal — an automatic trigger never gets a weaker
//! safety posture than a human one. The same floor formula is
//! pre-checked BEFORE the claim is written, so a floor refusal in the
//! common case never even creates a row.
//!
//! # Zero-conf composition (docs/09-runbook.md "Zero-conf payout change")
//!
//! Shaping composes with the 0-conf payout-change policy strictly on the
//! conservative side: chunk outputs receive NO
//! `goldcoin_payout_change_outpoints` provenance row — they are not
//! payout change — so they fail closed onto the full
//! `vault_min_confirmations` policy at every depth setting. The only
//! thing shaping READS from that policy is the eligible 0-conf pool
//! size, as part of deciding whether the pool is healthy enough to skip
//! shaping (payouts can genuinely draw on that pool, so ignoring it
//! over-triggered splits on healthy pools).

use std::time::Duration;

use thiserror::Error;

use super::indexer::GoldcoinRpc;
use super::rpc::{call_with_retry, BroadcastOutcome, RpcError};
use super::split::{self, SplitError};
use super::vault::MultisigVault;
use crate::goldcoin::multisig::{self, MultisigError};
use crate::ledger::{Ledger, LedgerError, PendingVaultUtxoSplit, ReserveDirection};
use crate::signing::goldcoin_split::{
    independently_sign_split_all_signers, RecoverySplitSource, SplitSigningError,
};
use crate::signing::signers::VaultSigner;

/// The operator-configured shaping knobs (`service/src/config.rs`,
/// `goldcoin.utxo_shaping_*` + the canonical `change_fanout_target_atomic`
/// chunk size), bundled like `goldcoin::payout::PayoutPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapingPolicy {
    /// The canonical payout-chunk size — `goldcoin.change_fanout_target_
    /// atomic`, the SAME target every payout's change fan-out uses: one
    /// sizing for the whole crate, never two drifting ones.
    pub chunk_target_atomic: u64,
    /// A new split is only considered while the payout-ready count
    /// (Available UTXOs of at least `chunk_target_atomic / 2`, plus
    /// currently-eligible 0-conf payout change of the same size) is
    /// below this.
    pub target_available_count: u32,
    /// Minimum size for a split candidate (config-validated to be at
    /// least two whole chunks).
    pub min_source_atomic: u64,
    /// Cap on chunk outputs per split; a larger source splits into
    /// correspondingly larger chunks (each itself a later candidate),
    /// bounding transaction size and shaping gradually.
    pub max_outputs_per_split: usize,
    pub fee_rate_per_kb: u64,
    /// `goldcoin.zero_conf_change_max_depth` — read-only here, solely so
    /// the pool-health trigger counts the same 0-conf change pool
    /// payouts can actually draw on. Shaping never makes anything
    /// 0-conf-eligible itself.
    pub zero_conf_change_max_depth: u32,
}

/// What one [`run_shaping_tick`] call actually did. At most one of the
/// transaction-shaped actions (`resumed_split_txid`, `new_split_txid`,
/// `rebroadcast_split_txid`, `abandoned_split`) is ever set per tick;
/// `confirmed_split_ids` is bookkeeping and can accompany any of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapingOutcome {
    /// `Broadcast` splits marked `Confirmed` this tick (first
    /// confirmation observed in the synced chain view).
    pub confirmed_split_ids: Vec<i64>,
    /// A `Broadcast` split the node had lost (mempool eviction) was
    /// re-broadcast from its exact stored bytes this tick.
    pub rebroadcast_split_txid: Option<[u8; 32]>,
    /// A split was abandoned this tick — `(split id, reason)`. Loud on
    /// purpose: an abandonment means a transaction this service signed
    /// can no longer take effect, which an operator should see even
    /// though no action is required.
    pub abandoned_split: Option<(i64, String)>,
    /// A previously stuck (`Built`/`Signed`) split was driven to
    /// `Broadcast` this tick.
    pub resumed_split_txid: Option<[u8; 32]>,
    /// A brand-new split was claimed, threshold-signed, and broadcast
    /// this tick.
    pub new_split_txid: Option<[u8; 32]>,
    /// Why no split action happened (pool healthy, no candidate, or the
    /// reserve-floor safety check refused) when everything above is
    /// empty.
    pub skipped: Option<String>,
    /// A lifecycle step (maintenance or resume) failed this tick. Loud
    /// and non-blocking: the error is surfaced here while the rest of
    /// the tick proceeds — one problematic split never freezes shaping.
    /// Persistent values deserve operator attention (`glc-admin
    /// split-vault-utxo --abandon` is the deliberate escape hatch).
    pub lifecycle_error: Option<String>,
}

impl ShapingOutcome {
    fn acted(&self) -> bool {
        self.rebroadcast_split_txid.is_some()
            || self.abandoned_split.is_some()
            || self.resumed_split_txid.is_some()
            || self.new_split_txid.is_some()
    }
}

#[derive(Debug, Error)]
pub enum ShapingError {
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("goldcoin rpc error: {0}")]
    Rpc(#[from] RpcError),
    #[error("split signing error: {0}")]
    Signing(#[from] SplitSigningError),
    #[error("split planning error: {0}")]
    Planning(#[from] SplitError),
    #[error("multisig assembly error: {0}")]
    Multisig(#[from] MultisigError),
    #[error("split #{0}: stored signed_tx_hex is missing or not valid hex")]
    BadStoredBytes(i64),
}

/// See module docs: bookkeeping plus at most one transaction-shaped
/// action per tick. `threshold` is `operators.vault_threshold`; the first
/// `threshold` entries of `vault_signers` are asked, matching every other
/// vault-spending path in this crate.
#[allow(clippy::too_many_arguments)]
pub async fn run_shaping_tick<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    policy: &ShapingPolicy,
    signer_timeout: Duration,
    allow_new_splits: bool,
    now: i64,
) -> Result<ShapingOutcome, ShapingError> {
    let mut outcome = ShapingOutcome::default();

    // 1. Lifecycle maintenance for splits already on the network —
    // always runs, never gated on pool health: a broadcast transaction's
    // fate must be driven to a terminal state regardless of anything
    // else. An ERROR here (an RPC surprise, a node rejection the outcome
    // mapping doesn't recognize) is recorded loudly and the tick
    // CONTINUES — one problematic split must never freeze the rest of
    // the lifecycle or new-split consideration (2026-08-30 re-review,
    // finding 4: a permanently rejected stored transaction otherwise
    // wedged all shaping forever, with `glc-admin split-vault-utxo
    // --abandon` as the operator's only-by-decision escape hatch).
    match maintain_broadcast_splits(ledger, goldcoin_rpc, None, &mut outcome, now).await {
        Ok(()) => {
            if outcome.acted() {
                return Ok(outcome);
            }
        }
        Err(e) => {
            outcome.lifecycle_error = Some(format!("broadcast-split maintenance: {e}"));
        }
    }

    // 2. Restart safety: a split already claimed/signed is always
    // finished — or abandoned — before any new one is considered. Same
    // error discipline as maintenance: record and continue.
    if let Some(pending) = ledger.pending_vault_utxo_splits()?.into_iter().next() {
        match resume_pending_split(
            ledger,
            goldcoin_rpc,
            vault,
            vault_signers,
            threshold,
            signer_timeout,
            &pending,
            &mut outcome,
            now,
        )
        .await
        {
            Ok(()) => return Ok(outcome),
            Err(e) => {
                outcome.lifecycle_error = Some(format!("resuming split #{}: {e}", pending.id));
            }
        }
    }

    // 3. NEW splits only beyond this point — `utxo_shaping_enabled =
    // false` stops here, after maintenance and resume have run (an
    // operator turning automatic shaping off must never strand what is
    // already in flight).
    if !allow_new_splits {
        outcome.skipped = Some("automatic shaping disabled: lifecycle maintenance only".into());
        return Ok(outcome);
    }

    // 4. Trigger check against the pool payouts can actually draw on:
    // mature Available UTXOs plus currently-eligible 0-conf payout
    // change (both at half the canonical chunk target or better — a pool
    // of slivers is not healthy no matter how many rows it has).
    let payout_ready_floor = policy.chunk_target_atomic / 2;
    let available = ledger.available_vault_utxos()?;
    let mut payout_ready = available
        .iter()
        .filter(|u| u.amount_atomic >= payout_ready_floor)
        .count();
    payout_ready += ledger
        .zero_conf_change_vault_utxos(policy.zero_conf_change_max_depth)?
        .iter()
        .filter(|u| u.amount_atomic >= payout_ready_floor)
        .count();
    if payout_ready as u32 >= policy.target_available_count {
        outcome.skipped = Some(format!(
            "pool healthy: {payout_ready} payout-ready UTXOs >= target {}",
            policy.target_available_count
        ));
        return Ok(outcome);
    }
    let maturing_chunks = ledger.unconfirmed_split_chunk_count()?;
    if maturing_chunks > 0 {
        outcome.skipped = Some(format!(
            "{maturing_chunks} chunk output(s) from a previous split still maturing — not \
             stacking another self-transaction on top"
        ));
        return Ok(outcome);
    }

    // 5. Candidate: the largest oversized root-vault UTXO with no LIVE
    // split row (`get_vault_utxo_split` ignores `Abandoned`, so a
    // released outpoint is a candidate again). `available` is already
    // sorted (amount DESC, txid, vout), so the first match is the
    // deterministic choice.
    //
    // Payout-liveness guard (2026-08-30 re-review, finding 6): splitting
    // a candidate takes its full value out of the MATURE pool for the
    // chunks' maturity window, and the solvency-aligned floor check
    // deliberately permits that (the value never leaves custody). But
    // already-admitted obligations need mature liquidity NOW — so a
    // candidate is only eligible while the rest of the mature pool can
    // still cover `pending_obligations` without it. In the bootstrap
    // shape (the giant deposit IS the reserve, nothing admitted yet)
    // `pending_obligations` is 0 and the guard passes; under live load
    // it defers the split until obligations drain or change matures,
    // during which payouts can keep spending the oversized UTXO
    // directly.
    let (_, _, _, pending_obligations) =
        ledger.reserve_snapshot(ReserveDirection::GoldcoinReserve)?;
    let mature_total: u64 = available.iter().map(|u| u.amount_atomic).sum();
    let root_script = vault.script_pubkey_hex();
    let mut candidate = None;
    let mut deferred_for_liveness = false;
    for u in &available {
        if u.amount_atomic < policy.min_source_atomic {
            break; // sorted DESC: nothing further qualifies either
        }
        if !u.script_pubkey_hex.eq_ignore_ascii_case(&root_script) {
            continue; // per-request derived deposit UTXOs are never split
        }
        if ledger.get_vault_utxo_split(u.txid, u.vout)?.is_some() {
            continue;
        }
        if mature_total.saturating_sub(u.amount_atomic) < pending_obligations {
            deferred_for_liveness = true;
            continue; // a smaller candidate may still fit
        }
        candidate = Some(u.clone());
        break;
    }
    if candidate.is_none() && deferred_for_liveness {
        outcome.skipped = Some(format!(
            "split deferred: every candidate would leave the mature pool below the \
             {pending_obligations} atomic units of already-admitted obligations — payouts keep \
             first claim; will retry as obligations drain or change matures"
        ));
        return Ok(outcome);
    }
    let Some(source) = candidate else {
        outcome.skipped = Some(format!(
            "no mature root-vault UTXO >= {} atomic to split ({payout_ready} payout-ready \
             UTXOs; pool will replenish via change maturity)",
            policy.min_source_atomic
        ));
        return Ok(outcome);
    };

    // A very large source splits into `max_outputs_per_split` larger
    // chunks rather than an unbounded number of target-sized ones —
    // deterministic, and each resulting chunk is itself a later candidate
    // if the pool thins again.
    let effective_chunk_target = source
        .amount_atomic
        .div_ceil(policy.max_outputs_per_split as u64)
        .max(policy.chunk_target_atomic);

    match execute_fresh_split(
        ledger,
        goldcoin_rpc,
        vault,
        vault_signers,
        threshold,
        signer_timeout,
        &source,
        effective_chunk_target,
        policy.fee_rate_per_kb,
        "auto: utxo liquidity shaping",
        now,
    )
    .await?
    {
        FreshSplitOutcome::Broadcast { txid } => outcome.new_split_txid = Some(txid),
        FreshSplitOutcome::RefusedFloor {
            reserve_after_fee,
            required_floor,
        } => {
            outcome.skipped = Some(format!(
                "reserve-floor safety check refused splitting {} atomic (reserve after fee \
                 would be {reserve_after_fee} < required floor {required_floor}) — will retry \
                 once the reserve recovers",
                source.amount_atomic
            ));
        }
        FreshSplitOutcome::Abandoned { split_id, reason } => {
            outcome.abandoned_split = Some((split_id, reason));
        }
    }
    Ok(outcome)
}

/// Drives every `Broadcast` split one step: first observed confirmation
/// -> `Confirmed` (bookkeeping; does not consume the tick's action
/// budget); node no longer knows the transaction -> re-broadcast the
/// exact stored bytes; re-broadcast refused for missing inputs ->
/// `Abandoned`. A transport-level RPC failure on the probe skips the
/// split until the next tick — an unreachable node must never trigger an
/// abandonment (fail closed on unknown state, never on absent state).
pub async fn maintain_broadcast_splits<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    only_split_id: Option<i64>,
    outcome: &mut ShapingOutcome,
    now: i64,
) -> Result<(), ShapingError> {
    for split in ledger.broadcast_vault_utxo_splits()? {
        // `Some(id)`: the per-outpoint CLI invocation — it must act on
        // exactly the split the operator named, never re-broadcast or
        // abandon an unrelated one under the operator's command
        // (2026-08-30 re-review, finding 7). `None`: the daemon tick
        // drives them all.
        if only_split_id.is_some_and(|id| id != split.id) {
            continue;
        }
        if ledger.max_confirmations_for_txid(split.txid)?.unwrap_or(0) >= 1 {
            ledger.record_vault_utxo_split_confirmed(split.id, now)?;
            outcome.confirmed_split_ids.push(split.id);
            continue;
        }
        let txid_hex = crate::goldcoin::hex::encode(&split.txid);
        match goldcoin_rpc.get_raw_transaction(&txid_hex).await {
            Ok(_) => {} // still known to the node (mempool) — nothing to do
            Err(RpcError::Method { .. }) => {
                // The node does not know this transaction any more:
                // mempool eviction (node restart, expiry, replacement).
                // Its bytes are still exactly as signed — re-submit them
                // verbatim; the node dedups if it re-learned the
                // transaction some other way.
                let Some(signed_hex) = split.signed_tx_hex.as_deref() else {
                    return Err(ShapingError::BadStoredBytes(split.id));
                };
                match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(signed_hex)).await? {
                    BroadcastOutcome::Accepted { .. }
                    | BroadcastOutcome::AlreadyInChain
                    | BroadcastOutcome::AlreadyInMempool => {
                        outcome.rebroadcast_split_txid = Some(split.txid);
                    }
                    BroadcastOutcome::MissingInputs => {
                        // The inputs are genuinely gone (spent by a
                        // conflicting transaction, or reorged away and
                        // re-spent): this split can never confirm.
                        let reason = format!(
                            "re-broadcast after eviction refused: missing inputs (txid {txid_hex})"
                        );
                        ledger.abandon_vault_utxo_split(split.id, &reason, now)?;
                        outcome.abandoned_split = Some((split.id, reason));
                    }
                }
                return Ok(()); // either way, that was this tick's action
            }
            Err(_) => {} // transport/malformed: retry next tick, touch nothing
        }
    }
    Ok(())
}

/// Resumes ONE pending (`Built`/`Signed`) split — or abandons it if its
/// source is no longer spendable (the release valve; see module docs).
/// The `Signed` path re-checks the reserve floor against CURRENT state
/// before putting never-yet-broadcast bytes on the network, unless the
/// node already knows the transaction (then the floor question is moot —
/// the bytes are out; only the bookkeeping is missing).
#[allow(clippy::too_many_arguments)]
pub async fn resume_pending_split<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    signer_timeout: Duration,
    pending: &PendingVaultUtxoSplit,
    outcome: &mut ShapingOutcome,
    now: i64,
) -> Result<(), ShapingError> {
    let snapshot = ledger
        .get_vault_utxo_split(pending.source_txid, pending.source_vout)?
        .ok_or(LedgerError::VaultUtxoSplitNotFound(pending.id))?;

    // The claim only holds while the source is genuinely ours to spend.
    // `sync_vault_utxos` reflects its real on-chain fate; anything but
    // `Available` (or a vanished row) means this split can never happen.
    let source_ok = matches!(
        ledger
            .get_vault_utxo(pending.source_txid, pending.source_vout)?
            .map(|r| r.state),
        Some(ref s) if s == "Available"
    );

    match pending.state.as_str() {
        "Signed" => {
            let signed_hex = snapshot
                .signed_tx_hex
                .clone()
                .ok_or(ShapingError::BadStoredBytes(pending.id))?;
            let bytes = crate::goldcoin::hex::decode_vec(&signed_hex)
                .map_err(|_| ShapingError::BadStoredBytes(pending.id))?;
            let txid = crate::goldcoin::tx::txid_of_serialized(&bytes);
            let txid_hex = crate::goldcoin::hex::encode(&txid);
            // Tri-state probe, exactly like `maintain_broadcast_splits`:
            // Ok = the node has the transaction; a METHOD error = the
            // node genuinely does not know it; a transport/malformed
            // error = we know nothing — defer to the next tick. `.is_ok()`
            // here was 2026-08-30 re-review finding 2: it conflated an
            // unreachable node with "not broadcast" and could abandon a
            // split whose transaction was already mined.
            let node_knows_it = match goldcoin_rpc.get_raw_transaction(&txid_hex).await {
                Ok(_) => true,
                Err(RpcError::Method { .. }) => false,
                Err(_) => {
                    outcome.skipped = Some(format!(
                        "split #{}: node unreachable while checking whether its transaction \
                         is already known — deferring to the next tick",
                        pending.id
                    ));
                    return Ok(());
                }
            };

            if !node_knows_it {
                if !source_ok {
                    let reason = format!(
                        "source {}:{} no longer Available before broadcast — abandoning \
                         never-broadcast split",
                        crate::goldcoin::hex::encode(&pending.source_txid),
                        pending.source_vout
                    );
                    ledger.abandon_vault_utxo_split(pending.id, &reason, now)?;
                    outcome.abandoned_split = Some((pending.id, reason));
                    return Ok(());
                }
                // Same non-overridable floor formula every signer applies
                // (`signing::goldcoin_split`), re-run against CURRENT
                // state — automatic resume is never weaker than a fresh
                // human-initiated split (2026-08-30 review, finding 9).
                if let Some((reserve_after_fee, required_floor)) =
                    floor_refusal(ledger, snapshot.fee_atomic)?
                {
                    let reason = format!(
                        "reserve-floor check refused resuming never-broadcast split \
                         (reserve after fee {reserve_after_fee} < floor {required_floor})"
                    );
                    ledger.abandon_vault_utxo_split(pending.id, &reason, now)?;
                    outcome.abandoned_split = Some((pending.id, reason));
                    return Ok(());
                }
                match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(&signed_hex)).await? {
                    BroadcastOutcome::Accepted { .. }
                    | BroadcastOutcome::AlreadyInChain
                    | BroadcastOutcome::AlreadyInMempool => {}
                    BroadcastOutcome::MissingInputs => {
                        let reason = format!(
                            "broadcast of resumed split refused: missing inputs (txid {txid_hex})"
                        );
                        ledger.abandon_vault_utxo_split(pending.id, &reason, now)?;
                        outcome.abandoned_split = Some((pending.id, reason));
                        return Ok(());
                    }
                }
            }
            let output_amounts = split::distribute_evenly(
                snapshot
                    .source_amount_atomic
                    .saturating_sub(snapshot.fee_atomic),
                snapshot.chunk_count as u64,
            );
            match ledger.record_vault_utxo_split_broadcast(
                pending.id,
                txid,
                &output_amounts,
                &vault.script_pubkey_hex(),
                now,
            ) {
                Ok(()) => {
                    outcome.resumed_split_txid = Some(txid);
                }
                // The source row is in a transient non-Available,
                // non-Spent state (e.g. a reorg re-classified it
                // 'Unconfirmed' while the node still resolves the split
                // transaction). Nothing here is safe to decide yet —
                // neither the broadcast bookkeeping nor an abandonment —
                // so wait for the ordinary sync to converge on one story
                // and retry (2026-08-30 re-review, finding 9).
                Err(LedgerError::VaultUtxoNotSplittable { state, .. }) => {
                    outcome.skipped = Some(format!(
                        "split #{}: node knows its transaction but the source row is currently \
                         '{state}' — waiting for the chain view to converge before recording",
                        pending.id
                    ));
                }
                Err(e) => return Err(e.into()),
            }
            Ok(())
        }
        "Built" => {
            if !source_ok {
                let reason = format!(
                    "source {}:{} no longer Available — abandoning unsigned split",
                    crate::goldcoin::hex::encode(&pending.source_txid),
                    pending.source_vout
                );
                ledger.abandon_vault_utxo_split(pending.id, &reason, now)?;
                outcome.abandoned_split = Some((pending.id, reason));
                return Ok(());
            }
            match sign_and_broadcast_built_split(
                ledger,
                goldcoin_rpc,
                vault,
                vault_signers,
                threshold,
                signer_timeout,
                pending.source_txid,
                pending.source_vout,
                pending.id,
                snapshot.chunk_target_atomic,
                now,
            )
            .await?
            {
                BuiltSplitResult::Broadcast { txid } => {
                    outcome.resumed_split_txid = Some(txid);
                }
                BuiltSplitResult::RefusedFloor {
                    reserve_after_fee,
                    required_floor,
                } => {
                    let reason = format!(
                        "reserve-floor check refused signing claimed split \
                         (reserve after fee {reserve_after_fee} < floor {required_floor})"
                    );
                    ledger.abandon_vault_utxo_split(pending.id, &reason, now)?;
                    outcome.abandoned_split = Some((pending.id, reason));
                }
                BuiltSplitResult::MissingInputs => {
                    let reason = "broadcast refused: missing inputs".to_string();
                    ledger.abandon_vault_utxo_split(pending.id, &reason, now)?;
                    outcome.abandoned_split = Some((pending.id, reason));
                }
            }
            Ok(())
        }
        other => {
            // pending_vault_utxo_splits only returns Built/Signed; anything
            // else is a query/schema drift that must fail loudly.
            Err(ShapingError::Ledger(
                LedgerError::VaultUtxoSplitNotRecoverable {
                    id: pending.id,
                    state: other.to_string(),
                },
            ))
        }
    }
}

/// Outcome of [`execute_fresh_split`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshSplitOutcome {
    Broadcast {
        txid: [u8; 32],
    },
    /// The pre-claim reserve-floor check refused — nothing was written.
    RefusedFloor {
        reserve_after_fee: u64,
        required_floor: u64,
    },
    /// The claim was written, but signing or broadcast then failed in a
    /// way that can never succeed — the claim was released again.
    Abandoned {
        split_id: i64,
        reason: String,
    },
}

/// The ONE fresh-split execution path, shared verbatim by the automatic
/// shaping tick and `glc-admin split-vault-utxo` (2026-08-30 review: two
/// diverging implementations was itself a finding): plan -> pre-check the
/// reserve floor -> CLAIM (`record_vault_utxo_split_built`, the
/// double-spend boundary) -> independent 2-of-3 signing via
/// [`RecoverySplitSource`] (byte-identity proven against the persisted
/// unsigned transaction; every signer re-runs the floor refusal) ->
/// broadcast -> atomic broadcast bookkeeping. A crash anywhere after the
/// claim leaves a `Built`/`Signed` row that [`resume_pending_split`]
/// finishes or abandons on the next tick — never a stranded state.
#[allow(clippy::too_many_arguments)]
pub async fn execute_fresh_split<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    signer_timeout: Duration,
    source: &crate::goldcoin::coin::VaultUtxo,
    chunk_target_atomic: u64,
    fee_rate_per_kb: u64,
    note: &str,
    now: i64,
) -> Result<FreshSplitOutcome, ShapingError> {
    let plan = split::plan_split(source, vault, chunk_target_atomic, fee_rate_per_kb)?;

    // Pre-claim floor check: the same formula every signer independently
    // re-runs after the claim. Checking it first keeps a routinely
    // refused split from writing (and abandoning) an audit row per tick.
    if let Some((reserve_after_fee, required_floor)) = floor_refusal(ledger, plan.fee_atomic)? {
        return Ok(FreshSplitOutcome::RefusedFloor {
            reserve_after_fee,
            required_floor,
        });
    }

    let unsigned_hex =
        crate::goldcoin::hex::encode(&split::build_unsigned_split_tx(&plan).serialize());
    let split_id = ledger.record_vault_utxo_split_built(
        &plan,
        chunk_target_atomic,
        &unsigned_hex,
        note,
        now,
    )?;

    match sign_and_broadcast_built_split(
        ledger,
        goldcoin_rpc,
        vault,
        vault_signers,
        threshold,
        signer_timeout,
        source.txid,
        source.vout,
        split_id,
        chunk_target_atomic,
        now,
    )
    .await?
    {
        BuiltSplitResult::Broadcast { txid } => Ok(FreshSplitOutcome::Broadcast { txid }),
        BuiltSplitResult::RefusedFloor {
            reserve_after_fee,
            required_floor,
        } => {
            let reason = format!(
                "reserve-floor check refused between claim and signing \
                 (reserve after fee {reserve_after_fee} < floor {required_floor})"
            );
            ledger.abandon_vault_utxo_split(split_id, &reason, now)?;
            Ok(FreshSplitOutcome::Abandoned { split_id, reason })
        }
        BuiltSplitResult::MissingInputs => {
            let reason = "broadcast refused: missing inputs".to_string();
            ledger.abandon_vault_utxo_split(split_id, &reason, now)?;
            Ok(FreshSplitOutcome::Abandoned { split_id, reason })
        }
    }
}

enum BuiltSplitResult {
    Broadcast {
        txid: [u8; 32],
    },
    RefusedFloor {
        reserve_after_fee: u64,
        required_floor: u64,
    },
    MissingInputs,
}

/// Signs a claimed (`Built`) split through the independent path and
/// broadcasts it, recording every step. Signer transport failures and
/// timeouts propagate as errors — the `Built`/`Signed` row stays put and
/// the next tick resumes it (restart safety); only outcomes that can
/// NEVER succeed are returned for the caller to abandon.
#[allow(clippy::too_many_arguments)]
async fn sign_and_broadcast_built_split<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    signer_timeout: Duration,
    source_txid: [u8; 32],
    source_vout: u32,
    split_id: i64,
    chunk_target_atomic: u64,
    now: i64,
) -> Result<BuiltSplitResult, ShapingError> {
    let sign_result = {
        let sign_source = RecoverySplitSource { ledger };
        independently_sign_split_all_signers(
            vault_signers,
            vault,
            &sign_source,
            source_txid,
            source_vout,
            chunk_target_atomic,
            0, // ignored by RecoverySplitSource: the persisted fee is authoritative
            threshold,
            signer_timeout,
        )
        .await
    };
    let (plan, mut tx, partials) = match sign_result {
        Ok(v) => v,
        Err(SplitSigningError::SafetyCheckFailed {
            mature_reserve_after,
            required_floor,
        }) => {
            return Ok(BuiltSplitResult::RefusedFloor {
                reserve_after_fee: mature_reserve_after,
                required_floor,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let sighash = tx.sighash_all(0, &vault.redeem_script());
    tx.inputs[0].script_sig = multisig::assemble(vault, &sighash, &partials)?;
    let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
    ledger.record_vault_utxo_split_signed(split_id, &signed_hex, now)?;

    match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(&signed_hex)).await? {
        BroadcastOutcome::Accepted { .. }
        | BroadcastOutcome::AlreadyInChain
        | BroadcastOutcome::AlreadyInMempool => {
            let split_txid = tx.txid();
            ledger.record_vault_utxo_split_broadcast(
                split_id,
                split_txid,
                &plan.output_amounts,
                &vault.script_pubkey_hex(),
                now,
            )?;
            Ok(BuiltSplitResult::Broadcast { txid: split_txid })
        }
        BroadcastOutcome::MissingInputs => Ok(BuiltSplitResult::MissingInputs),
    }
}

/// `Some((reserve_after_fee, required_floor))` when the split-refusing
/// reserve-floor condition holds — the solvency-invariant-aligned
/// `balance - fee >= protected_minimum + pending_obligations` formula
/// (see `signing::goldcoin_split`'s module docs for why only the fee
/// counts as leaving).
fn floor_refusal(ledger: &Ledger, fee_atomic: u64) -> Result<Option<(u64, u64)>, LedgerError> {
    let (balance, protected_minimum, _reserved, pending_obligations) =
        ledger.reserve_snapshot(ReserveDirection::GoldcoinReserve)?;
    let reserve_after_fee = balance.saturating_sub(fee_atomic);
    let required_floor = protected_minimum + pending_obligations;
    if reserve_after_fee < required_floor {
        Ok(Some((reserve_after_fee, required_floor)))
    } else {
        Ok(None)
    }
}
