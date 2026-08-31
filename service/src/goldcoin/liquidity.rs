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
//!   drives every `Broadcast` split onward each tick: reaching
//!   `vault_min_confirmations` (via the service's own synced chain view,
//!   never a node-claimed status) marks it `Confirmed` — the same depth
//!   the crate trusts outputs everywhere else, so a shallow reorg never
//!   orphans a split nothing maintains; a split the node no longer knows
//!   (mempool eviction) is re-broadcast from its exact stored bytes.
//! - **Automatic abandonment happens ONLY where it is provably safe** —
//!   a `Built` row (nothing was ever signed) whose source is gone, and a
//!   fresh broadcast the node rejects outright. Every ambiguous case —
//!   missing-inputs refusals (a reorg race can produce them transiently
//!   for a transaction that later confirms), a `Signed` split the local
//!   node has forgotten, a transient floor refusal — DEFERS with a loud
//!   [`ShapingOutcome::lifecycle_error`]/skip instead: fully signed
//!   bytes are never walked away from automatically. The irreversible
//!   decision belongs to the operator (`glc-admin split-vault-utxo
//!   --abandon --execute`), whose `Abandoned` row keeps its audit trail
//!   forever while the partial unique index
//!   (`ux_vault_utxo_splits_source`, v16) releases the outpoint. A
//!   deferring or erroring split never blocks the rest of the lifecycle
//!   or new-split consideration — no state can permanently stall
//!   shaping, and no path requires manual SQLite edits.
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
    /// `goldcoin.vault_min_confirmations` — the depth at which a
    /// `Broadcast` split becomes terminal `Confirmed`: the same depth
    /// the crate trusts vault outputs everywhere else, so a shallow
    /// reorg can never orphan a split nothing maintains any more.
    pub min_confirmations: i64,
}

/// What one [`run_shaping_tick`] call actually did. At most one of the
/// transaction-shaped actions (`resumed_split_txid`, `new_split_txid`,
/// `rebroadcast_split_txid`, `abandoned_split`) is ever set per tick;
/// `confirmed_split_ids` is bookkeeping and can accompany any of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapingOutcome {
    /// `Broadcast` splits marked `Confirmed` this tick (the synced
    /// chain view reached `vault_min_confirmations`).
    pub confirmed_split_ids: Vec<i64>,
    /// `Abandoned` splits whose transaction the chain proved alive after
    /// all — re-adopted into `Broadcast` this tick, with the loss debit
    /// reversed ([`crate::ledger::Ledger::readopt_vault_utxo_split`]).
    pub readopted_split_ids: Vec<i64>,
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
    match maintain_broadcast_splits(
        ledger,
        goldcoin_rpc,
        None,
        policy.min_confirmations,
        &mut outcome,
        now,
    )
    .await
    {
        Ok(()) => {
            if outcome.acted() {
                return Ok(outcome);
            }
        }
        Err(e) => {
            outcome.lifecycle_error = Some(format!("broadcast-split maintenance: {e}"));
        }
    }

    // 2. Restart safety: EVERY pending (Built/Signed) split is visited,
    // oldest first, with per-row error isolation (2026-08-31
    // production-readiness review, H4: attempting only the head let one
    // permanently deferring row starve every younger claimed split
    // forever). The first row that ACTS consumes the tick's one
    // transaction-shaped action; rows that defer or error have their
    // stories collected and the walk — and then new-split consideration
    // — continues, since a deferring row's source stays safely claimed
    // either way.
    let mut resume_notes: Vec<String> = Vec::new();
    for pending in ledger.pending_vault_utxo_splits()? {
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
            Ok(()) => {
                if outcome.acted() {
                    if !resume_notes.is_empty() {
                        outcome.lifecycle_error = Some(resume_notes.join("; "));
                    }
                    return Ok(outcome);
                }
                if let Some(msg) = outcome.skipped.take() {
                    resume_notes.push(msg);
                }
                if let Some(msg) = outcome.lifecycle_error.take() {
                    resume_notes.push(msg);
                }
            }
            Err(e) => {
                resume_notes.push(format!("resuming split #{}: {e}", pending.id));
            }
        }
    }
    if !resume_notes.is_empty() {
        outcome.lifecycle_error = Some(resume_notes.join("; "));
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
    let maturing_chunks = ledger.unconfirmed_split_chunk_count(now)?;
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

/// What the local node knows about a transaction — the ONE tri-state
/// probe every lifecycle decision uses (2026-08-31 production-readiness
/// review, H1: two `.is_ok()` call sites conflated "node unreachable"
/// with "transaction does not exist"). `Unknown` (transport/malformed
/// RPC failure) must always fail CLOSED: defer, refuse, change nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxProbe {
    /// The node returned the transaction (mempool or chain).
    Known,
    /// The node answered, and answered "no such transaction" (a JSON-RPC
    /// method error, e.g. -5).
    Absent,
    /// The node could not be asked (transport/malformed response) — no
    /// conclusion may be drawn.
    Unknown(String),
}

/// See [`TxProbe`].
pub async fn probe_transaction<GR: GoldcoinRpc>(goldcoin_rpc: &GR, txid_hex: &str) -> TxProbe {
    match goldcoin_rpc.get_raw_transaction(txid_hex).await {
        Ok(_) => TxProbe::Known,
        Err(RpcError::Method { .. }) => TxProbe::Absent,
        Err(e) => TxProbe::Unknown(e.to_string()),
    }
}

/// Reconstructs — and BYTE-VERIFIES against the persisted unsigned
/// transaction — a split's output amounts from its persisted figures,
/// the same strongest-available check the `Built` signing path applies
/// via `RecoverySplitSource` (2026-08-31 review, low finding: the
/// Signed-resume path previously trusted an unverified inline
/// reconstruction). `Err` = do not record anything; defer.
pub fn verified_output_amounts(
    snapshot: &crate::ledger::VaultUtxoSplitSnapshot,
    source_txid: [u8; 32],
    source_vout: u32,
    vault: &MultisigVault,
) -> Result<Vec<u64>, String> {
    let source = crate::goldcoin::coin::VaultUtxo {
        txid: source_txid,
        vout: source_vout,
        amount_atomic: snapshot.source_amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    let plan = split::reconstruct_plan(
        &source,
        vault,
        snapshot.chunk_count as u64,
        snapshot.fee_atomic,
    )
    .map_err(|e| format!("plan reconstruction failed: {e}"))?;
    let reconstructed_hex =
        crate::goldcoin::hex::encode(&split::build_unsigned_split_tx(&plan).serialize());
    if !reconstructed_hex.eq_ignore_ascii_case(&snapshot.unsigned_tx_hex) {
        return Err(
            "reconstructed unsigned transaction does not match the persisted one".to_string(),
        );
    }
    Ok(plan.output_amounts)
}

/// Startup/tick-front bookkeeping heal (2026-08-31 review, H3): for
/// every `Signed` split, if the node ALREADY KNOWS the exact transaction
/// its stored bytes hash to (a crash landed between broadcast acceptance
/// and the ledger commit — locally or in a concurrent `glc-admin` run),
/// record the Broadcast bookkeeping — source `Spent` with its marker,
/// chunk rows inserted, all atomically — BEFORE any reconciliation pass
/// could read the source's disappearance as an unexplained loss and
/// latch a false auto-pause. Probe-only: never signs, never sends bytes,
/// so it is structurally incapable of creating a duplicate split
/// transaction; an unreachable node simply defers (in which case
/// reconciliation's own chain reads are equally unavailable, so nothing
/// can misjudge in the meantime).
pub async fn heal_split_bookkeeping<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    now: i64,
) -> Result<Vec<i64>, ShapingError> {
    let mut healed = Vec::new();
    for pending in ledger.pending_vault_utxo_splits()? {
        if pending.state != "Signed" {
            continue;
        }
        let Some(snapshot) =
            ledger.get_vault_utxo_split(pending.source_txid, pending.source_vout)?
        else {
            continue;
        };
        let Some(signed_hex) = snapshot.signed_tx_hex.as_deref() else {
            continue;
        };
        let Ok(bytes) = crate::goldcoin::hex::decode_vec(signed_hex) else {
            continue;
        };
        let txid = crate::goldcoin::tx::txid_of_serialized(&bytes);
        let txid_hex = crate::goldcoin::hex::encode(&txid);
        if probe_transaction(goldcoin_rpc, &txid_hex).await != TxProbe::Known {
            continue; // Absent or Unknown: nothing to heal / fail closed
        }
        let Ok(amounts) =
            verified_output_amounts(&snapshot, pending.source_txid, pending.source_vout, vault)
        else {
            continue; // never record unverified amounts; resume surfaces it
        };
        match ledger.record_vault_utxo_split_broadcast(
            pending.id,
            txid,
            &amounts,
            &vault.script_pubkey_hex(),
            now,
        ) {
            Ok(()) => healed.push(pending.id),
            Err(LedgerError::VaultUtxoNotSplittable { .. }) => {} // transient; retry next tick
            Err(e) => return Err(e.into()),
        }
    }
    Ok(healed)
}

/// Drives every `Broadcast` split one step:/// Drives every `Broadcast` split one step: first observed confirmation
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
    min_confirmations: i64,
    outcome: &mut ShapingOutcome,
    now: i64,
) -> Result<(), ShapingError> {
    // On-chain contradiction check first: an Abandoned-with-txid split
    // whose outputs the synced chain view now shows confirmed was
    // abandoned in error — re-adopt it (state back to Broadcast, loss
    // debit reversed) so its value re-enters the lifecycle and the
    // accounting terms instead of surfacing as an unexplained surplus
    // (2026-08-31 production-readiness review, H2).
    for (id, txid) in ledger.abandoned_splits_with_txid()? {
        if only_split_id.is_some_and(|only| only != id) {
            continue;
        }
        if ledger.max_confirmations_for_txid(txid)?.unwrap_or(0) >= 1 {
            ledger.readopt_vault_utxo_split(id, now)?;
            outcome.readopted_split_ids.push(id);
        }
    }

    for split in ledger.broadcast_vault_utxo_splits()? {
        // `Some(id)`: the per-outpoint CLI invocation — it must act on
        // exactly the split the operator named, never re-broadcast or
        // abandon an unrelated one under the operator's command
        // (2026-08-30 re-review, finding 7). `None`: the daemon tick
        // drives them all.
        if only_split_id.is_some_and(|id| id != split.id) {
            continue;
        }
        // Terminal `Confirmed` only at the SAME depth the crate trusts
        // outputs everywhere else (`vault_min_confirmations`) — a split
        // marked terminal at one confirmation could be orphaned by a
        // shallow reorg with nothing left maintaining it (2026-08-30
        // third-pass review, finding 4). Until then the split stays
        // `Broadcast`: its chunks stay absence-flip-exempt and it keeps
        // being re-broadcast if the chain forgets it.
        if ledger.max_confirmations_for_txid(split.txid)?.unwrap_or(0) >= min_confirmations {
            ledger.record_vault_utxo_split_confirmed(split.id, now)?;
            outcome.confirmed_split_ids.push(split.id);
            continue;
        }
        // Per-split error isolation (third-pass finding 10): one split
        // whose probe/re-broadcast keeps failing must not starve
        // confirmation-marking and eviction recovery for every later
        // split — record the error loudly and keep iterating.
        let txid_hex = crate::goldcoin::hex::encode(&split.txid);
        match drive_one_broadcast_split(ledger, goldcoin_rpc, &split, &txid_hex, outcome, now).await
        {
            Ok(acted) => {
                if acted {
                    return Ok(()); // the tick's one transaction-shaped action
                }
            }
            Err(e) => {
                outcome.lifecycle_error =
                    Some(format!("maintaining broadcast split #{}: {e}", split.id));
            }
        }
    }
    Ok(())
}

/// Probe/re-broadcast step for one `Broadcast` split. Returns whether a
/// re-broadcast happened (the caller's one-action budget). NOTHING here
/// is irreversible: a `MissingInputs` refusal — which a reorg race can
/// produce transiently for a transaction that later confirms on the
/// winning chain — is surfaced as a lifecycle error for the operator
/// (whose `glc-admin split-vault-utxo --abandon` is the only path that
/// ever abandons an already-signed split), never an automatic
/// abandonment (third-pass finding 9; the retired `split_recovery`
/// module's `BroadcastConflict` posture, restored).
async fn drive_one_broadcast_split<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    split: &crate::ledger::UnconfirmedBroadcastSplit,
    txid_hex: &str,
    outcome: &mut ShapingOutcome,
    now: i64,
) -> Result<bool, ShapingError> {
    match probe_transaction(goldcoin_rpc, txid_hex).await {
        TxProbe::Known => {
            // Still known to the node — and any earlier missing-inputs
            // refusal was therefore transient (a reorg race): clear the
            // flag so the accounting terms keep explaining its chunks.
            ledger.clear_split_missing_inputs(split.id)?;
            Ok(false)
        }
        TxProbe::Absent => {
            // The node does not know this transaction any more: mempool
            // eviction (node restart, expiry, replacement). Its bytes
            // are still exactly as signed — re-submit them verbatim; the
            // node dedups if it re-learned the transaction another way.
            let Some(signed_hex) = split.signed_tx_hex.as_deref() else {
                return Err(ShapingError::BadStoredBytes(split.id));
            };
            match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(signed_hex)).await? {
                BroadcastOutcome::Accepted { .. }
                | BroadcastOutcome::AlreadyInChain
                | BroadcastOutcome::AlreadyInMempool => {
                    ledger.clear_split_missing_inputs(split.id)?;
                    outcome.rebroadcast_split_txid = Some(split.txid);
                    Ok(true)
                }
                BroadcastOutcome::MissingInputs => {
                    // Never auto-abandoned (signed bytes; reorg races
                    // refuse transiently) — but FLAGGED: after
                    // `SPLIT_MISSING_INPUTS_GRACE_SECS` the accounting
                    // terms stop explaining this split's chunks, so a
                    // genuine conflicting-spend loss surfaces as the
                    // reconciliation breach it is instead of being
                    // silently padded over, and new shaping stops being
                    // gated on chunks that are never coming (2026-08-31
                    // review, B2).
                    ledger.set_split_missing_inputs(split.id, now)?;
                    outcome.lifecycle_error = Some(format!(
                        "split #{}: re-broadcast after eviction refused for missing inputs \
                         (txid {txid_hex}) — a conflicting spend may have won, or a reorg is \
                         in flight; flagged (accounting stops explaining its chunks after the \
                         grace window, surfacing any real loss); operator --abandon is the \
                         deliberate release, never automatic",
                        split.id
                    ));
                    Ok(false)
                }
            }
        }
        TxProbe::Unknown(_) => Ok(false), // fail closed: retry next tick, touch nothing
    }
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
            let node_knows_it = match probe_transaction(goldcoin_rpc, &txid_hex).await {
                TxProbe::Known => true,
                TxProbe::Absent => false,
                TxProbe::Unknown(_) => {
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
                    // AMBIGUOUS, so never irreversible (third-pass
                    // finding 2): fully signed bytes exist. "The local
                    // node doesn't know the tx and the source looks
                    // spent" is exactly what a pre-crash broadcast plus
                    // local mempool amnesia looks like — the transaction
                    // may still be propagating from peers. Abandoning
                    // would release the claim while our signature lives
                    // on. Defer; the operator's --abandon is the only
                    // path that ever abandons a signed split.
                    outcome.lifecycle_error = Some(format!(
                        "split #{}: node does not know its transaction and the source {}:{} is \
                         not Available — cannot distinguish a dead split from local mempool \
                         amnesia after a pre-crash broadcast; deferring (operator --abandon is \
                         the deliberate release)",
                        pending.id,
                        crate::goldcoin::hex::encode(&pending.source_txid),
                        pending.source_vout
                    ));
                    return Ok(());
                }
                // Same non-overridable floor formula every signer applies
                // (`signing::goldcoin_split`), re-run against CURRENT
                // state — automatic resume is never weaker than a fresh
                // human-initiated split (2026-08-30 review, finding 9).
                if let Some((reserve_after_fee, required_floor)) =
                    floor_refusal(ledger, snapshot.fee_atomic)?
                {
                    // Deferred, not abandoned: the floor is a live
                    // figure (pending obligations drain, deposits land)
                    // and this split's bytes are already signed —
                    // walking away from them is the operator's call,
                    // not a transient number's.
                    outcome.skipped = Some(format!(
                        "split #{}: reserve-floor check deferred resuming never-broadcast \
                         split (reserve after fee {reserve_after_fee} < floor \
                         {required_floor}) — will retry as the reserve recovers",
                        pending.id
                    ));
                    return Ok(());
                }
                match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(&signed_hex)).await? {
                    BroadcastOutcome::Accepted { .. }
                    | BroadcastOutcome::AlreadyInChain
                    | BroadcastOutcome::AlreadyInMempool => {}
                    BroadcastOutcome::MissingInputs => {
                        // Same conservative posture as
                        // `drive_one_broadcast_split`: a reorg race can
                        // report missing inputs transiently; signed
                        // bytes are never auto-abandoned.
                        outcome.lifecycle_error = Some(format!(
                            "split #{}: broadcast refused for missing inputs (txid \
                             {txid_hex}) — needs operator investigation; --abandon is the \
                             deliberate release",
                            pending.id
                        ));
                        return Ok(());
                    }
                }
            }
            let output_amounts = match verified_output_amounts(
                &snapshot,
                pending.source_txid,
                pending.source_vout,
                vault,
            ) {
                Ok(a) => a,
                Err(reason) => {
                    // Never record amounts the persisted bytes do not
                    // vouch for — a corrupted row is an operator matter.
                    outcome.lifecycle_error = Some(format!(
                        "split #{}: refusing to record broadcast bookkeeping — {reason}",
                        pending.id
                    ));
                    return Ok(());
                }
            };
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
