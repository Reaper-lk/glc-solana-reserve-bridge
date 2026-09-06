//! Robinhood (EVM) deposit observations, the scan cursor, the reorg
//! journal, and the Robinhood-local halt — every mutation of the four
//! tables schema v22 adds.
//!
//! # Why this is a sibling file rather than more of `ledger/mod.rs`
//!
//! Same reason `goldcoin::indexer` calls into `Ledger` instead of writing
//! its own SQL: the invariants live in exactly one place. This is that
//! place for Robinhood. It is a separate FILE only because
//! `ledger/mod.rs` is already several thousand lines and these rows share
//! no table with anything in it — the `impl Ledger` block below is the
//! same type, and callers see one API.
//!
//! # This module cannot settle anything
//!
//! There is no method here that touches `bridge_requests`, `reserve_
//! ledger`, `vault_utxos`, or any reservation. There is no method that
//! marks an observation payable, because `robinhood_deposit_
//! observations.settled` is pinned to `0` by a database `CHECK` (see
//! `schema::apply_v22`). Observing a Robinhood deposit moves no value and
//! opens no route; the four gates that keep Robinhood closed
//! (`crate::routes::RouteGate`'s three, plus the absence of a
//! `Direction`) are untouched by everything below.
//!
//! # The three-way outcome of recording an observation
//!
//! Idempotency is not "insert or ignore". Re-reading the same range after
//! a restart, or rescanning an overlap, MUST be free — but two different
//! events claiming one identity is a fact about the world that no retry
//! can fix, and swallowing it would mean the service's record of a
//! deposit silently disagreed with the chain's. So
//! [`Ledger::robinhood_apply_scan_range`] resolves each event to exactly
//! one of:
//!
//! - **Recorded** — new identity, inserted.
//! - **Already recorded** — an existing row agrees FIELD BY FIELD. A
//!   no-op, and the normal outcome of any overlap or replay.
//! - **Conflict** — an existing row disagrees on any recorded field. The
//!   whole range's transaction is rolled back (so the cursor does not
//!   advance and nothing is half-written) and the caller halts. This is
//!   never resolved automatically.
//!
//! A reorg is deliberately NOT a conflict: it is reconciled first, by
//! [`Ledger::robinhood_rollback_reorg`], which tombstones the orphaned
//! provisional rows so the identity is free again before the rescan that
//! would otherwise collide with them.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::OptionalExtension;

use super::{write_tx, Ledger, LedgerError};
use crate::routes::Route;

/// How much of an observation the service is willing to believe.
///
/// Deliberately three states and not a boolean: `Reorged` is not "less
/// final than provisional", it is a tombstone — a row that describes what
/// this service believed at a point in time and that the canonical chain
/// has since contradicted. Keeping it is what makes the identity indexes
/// scoped rather than global (see `schema::apply_v22`) and what leaves an
/// auditor something to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RobinhoodFinality {
    /// Seen on the canonical chain, but shallower than the configured
    /// confirmation depth. Can still be orphaned by an ordinary reorg.
    Provisional,
    /// At or beyond the configured confirmation depth. Being contradicted
    /// after this point is an incident, not a reorg — see
    /// [`Ledger::robinhood_final_observations_above`].
    Final,
    /// Orphaned while still provisional. Never re-promoted, never
    /// deleted.
    Reorged,
}

impl RobinhoodFinality {
    pub fn as_str(self) -> &'static str {
        match self {
            RobinhoodFinality::Provisional => "Provisional",
            RobinhoodFinality::Final => "Final",
            RobinhoodFinality::Reorged => "Reorged",
        }
    }
}

impl std::str::FromStr for RobinhoodFinality {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Provisional" => Ok(RobinhoodFinality::Provisional),
            "Final" => Ok(RobinhoodFinality::Final),
            "Reorged" => Ok(RobinhoodFinality::Reorged),
            other => Err(format!("unknown Robinhood finality state {other:?}")),
        }
    }
}

impl ToSql for RobinhoodFinality {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RobinhoodFinality {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|_| FromSqlError::InvalidType)
    }
}

/// One `DepositCreated` sighting, exactly as the decoder resolved it.
///
/// Every field here is a recorded fact about the log, and every one of
/// them participates in the conflict check: two events sharing an
/// identity but differing in ANY of these is a contradiction, not a
/// duplicate. `block_number`/`block_hash` are included in that check on
/// purpose — a reorg that genuinely moves a deposit is reconciled by
/// tombstoning first, so by the time an insert is attempted there is no
/// legitimate way for a surviving row to sit in a different block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodDepositObservation {
    /// The 20-byte address of the `GlcRobinhoodBridge` deployment whose
    /// local counter produced `obligation_index`.
    pub source_contract: [u8; 20],
    pub obligation_index: u64,
    /// Always one of the two INBOUND routes; the decoder refuses the rest.
    pub route: Route,
    pub depositor: [u8; 20],
    /// Opaque payload on the destination network, 1..=64 bytes. Never
    /// parsed by the ledger.
    pub destination: Vec<u8>,
    /// The exact 32-byte big-endian `uint256` the event carried.
    pub amount_robinhood_atomic: [u8; 32],
    /// The event's own `canonicalAmount`, cross-checked against
    /// `amount_robinhood_atomic` by the decoder before it gets here.
    pub amount_canonical_atomic: u64,
    pub tx_hash: [u8; 32],
    pub log_index: u64,
    pub block_number: u64,
    pub block_hash: [u8; 32],
}

/// A stored observation, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodObservationRow {
    pub id: i64,
    pub observation: RobinhoodDepositObservation,
    pub finality: RobinhoodFinality,
    pub observed_at: i64,
    pub finalized_at: Option<i64>,
    pub reorged_at: Option<i64>,
}

/// What [`Ledger::robinhood_apply_scan_range`] did with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobinhoodObservationOutcome {
    Recorded,
    /// An existing row already says exactly this. The normal, expected
    /// result of a rescan or a restart.
    AlreadyRecorded,
}

/// The one recorded field on which a re-observed event disagreed with the
/// row already stored under the same durable identity.
///
/// Carries the field NAME rather than the whole pair of rows: an operator
/// reading this needs to know what changed, and a diff of thirteen
/// columns buries that. The full rows are both still in the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodObservationConflict {
    pub obligation_index: u64,
    pub field: &'static str,
    pub stored: String,
    pub observed: String,
}

impl std::fmt::Display for RobinhoodObservationConflict {
    /// Reads as the sentence an operator needs: which obligation, which
    /// fact, what was believed, what the chain now says.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "obligation {}: field `{}` was recorded as {} but the chain now reports {}",
            self.obligation_index, self.field, self.stored, self.observed
        )
    }
}

/// Why the Robinhood indexer is stopped. Persisted, so it survives a
/// restart — an in-memory flag would let a bounce look like a fix.
///
/// Every variant requires a human. None of them clears itself, and none
/// of them pauses a reserve: a Robinhood fault must not take live
/// Solana<->Goldcoin traffic down (see `schema::apply_v22`'s blast-radius
/// note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobinhoodHaltReason {
    /// Two events claim one durable identity and disagree.
    ObservationConflict,
    /// A reorg orphaned a block that a `Final` observation sits in.
    PostFinalityReorg,
    /// No retained scan anchor still matches the live chain, so the fork
    /// point is beyond anything this service can prove — never guessed.
    ReorgBeyondRetainedAnchors,
    /// The endpoint reported a chain id other than the configured one.
    ChainIdMismatch,
    /// A `DepositCreated` log decoded cleanly but named a route this
    /// contract cannot emit for a deposit — the address is not the
    /// contract this service thinks it is.
    UnexpectedContractRoute,
}

impl RobinhoodHaltReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RobinhoodHaltReason::ObservationConflict => "observation_conflict",
            RobinhoodHaltReason::PostFinalityReorg => "post_finality_reorg",
            RobinhoodHaltReason::ReorgBeyondRetainedAnchors => "reorg_beyond_retained_anchors",
            RobinhoodHaltReason::ChainIdMismatch => "chain_id_mismatch",
            RobinhoodHaltReason::UnexpectedContractRoute => "unexpected_contract_route",
        }
    }
}

impl std::str::FromStr for RobinhoodHaltReason {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "observation_conflict" => Ok(RobinhoodHaltReason::ObservationConflict),
            "post_finality_reorg" => Ok(RobinhoodHaltReason::PostFinalityReorg),
            "reorg_beyond_retained_anchors" => Ok(RobinhoodHaltReason::ReorgBeyondRetainedAnchors),
            "chain_id_mismatch" => Ok(RobinhoodHaltReason::ChainIdMismatch),
            "unexpected_contract_route" => Ok(RobinhoodHaltReason::UnexpectedContractRoute),
            other => Err(format!("unknown Robinhood halt reason {other:?}")),
        }
    }
}

/// A persisted halt, as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodHalt {
    pub reason: RobinhoodHaltReason,
    pub detail: String,
    pub halted_at: i64,
}

/// Counts and heights the indexer's health state reports without having
/// to hold any of the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RobinhoodObservationSummary {
    pub provisional: i64,
    pub finalized: i64,
    pub reorged: i64,
    /// Highest block that holds a `Final` observation, if any.
    pub highest_finalized_block: Option<u64>,
}

/// Result of applying one scanned range.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RobinhoodRangeApplied {
    pub recorded: u32,
    pub already_recorded: u32,
}

fn blob20(v: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    let n = v.len().min(20);
    out[..n].copy_from_slice(&v[..n]);
    out
}

fn blob32(v: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = v.len().min(32);
    out[..n].copy_from_slice(&v[..n]);
    out
}

/// `u64` -> `i64` for a column SQLite stores as a signed integer.
///
/// Every value that reaches this is already bounded far below `i64::MAX`
/// by the decoder (a block height, a log index, an obligation counter, a
/// canonical amount), so this is a belt-and-braces refusal rather than a
/// live failure mode: a value that did not fit could only come from a
/// malformed response the decoder should already have rejected, and
/// storing a negative number for it would be worse than refusing.
fn to_i64(value: u64, field: &'static str) -> Result<i64, LedgerError> {
    i64::try_from(value).map_err(|_| LedgerError::RobinhoodValueOutOfRange {
        field,
        value: value.to_string(),
    })
}

impl Ledger {
    // ------------------------------------------------ scan cursor / anchors --

    /// The scan cursor: the highest block this service has anchored on,
    /// with that block's hash. `None` means nothing has ever been scanned
    /// — the caller must then start from its configured start block and
    /// must never invent one (see
    /// `crate::robinhood::indexer::RobinhoodIndexer::tick`).
    pub fn robinhood_scan_cursor(&self) -> Result<Option<(u64, [u8; 32])>, LedgerError> {
        self.conn
            .query_row(
                "SELECT block_number, block_hash FROM robinhood_scanned_blocks
                 ORDER BY block_number DESC LIMIT 1",
                [],
                |r| {
                    let n: i64 = r.get(0)?;
                    let h: Vec<u8> = r.get(1)?;
                    Ok((n as u64, blob32(&h)))
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Every retained anchor, highest first — the walk-back set for reorg
    /// detection.
    ///
    /// Sparse by construction (see `schema::apply_v22`): one anchor per
    /// scanned range plus one per block that held a deposit, pruned to a
    /// window. That is sufficient because a block hash commits to its
    /// whole ancestry, so the highest anchor that still matches the live
    /// chain certifies everything beneath it.
    pub fn robinhood_scan_anchors_desc(&self) -> Result<Vec<(u64, [u8; 32])>, LedgerError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT block_number, block_hash FROM robinhood_scanned_blocks
             ORDER BY block_number DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let n: i64 = r.get(0)?;
                let h: Vec<u8> = r.get(1)?;
                Ok((n as u64, blob32(&h)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -------------------------------------------------------- observations --

    /// Applies one scanned block range atomically: every decoded event in
    /// it, plus the anchor that moves the cursor to `anchor_block`, plus
    /// the anchor pruning — all in ONE transaction.
    ///
    /// **The cursor advancing and the observations landing are the same
    /// commit.** That is the whole point of taking a range rather than an
    /// event: a crash can leave the range entirely applied or entirely
    /// unapplied, never "recorded but not cursored" (which would need
    /// idempotency to save it) or "cursored but not recorded" (which
    /// nothing could save — the deposits would simply have been skipped).
    ///
    /// `deposit_block_anchors` are the blocks that actually held a
    /// deposit in this range; `cursor_block`/`cursor_hash` is the range's
    /// last block, which becomes the new cursor. All are written as
    /// anchors in the same commit.
    ///
    /// `retain_anchors_below` is the depth below the new cursor within
    /// which anchors are kept in full; below that exactly one — the
    /// highest — survives, so the reorg walk can always tell "a reorg
    /// within the window" from "a reorg deeper than anything I can prove"
    /// without the table growing without bound. Callers pass their
    /// confirmation depth.
    ///
    /// Returns `Err(LedgerError::RobinhoodObservationConflict)` — with the
    /// transaction rolled back, so nothing at all was applied and the
    /// cursor did not move — when a re-observed identity disagrees with
    /// what is stored. See this module's docs.
    pub fn robinhood_apply_scan_range(
        &mut self,
        observations: &[RobinhoodDepositObservation],
        deposit_block_anchors: &[(u64, [u8; 32])],
        cursor_block: u64,
        cursor_hash: [u8; 32],
        retain_anchors_below: u64,
        now: i64,
    ) -> Result<RobinhoodRangeApplied, LedgerError> {
        let cursor_block_i = to_i64(cursor_block, "cursor_block")?;
        let mut applied = RobinhoodRangeApplied::default();

        let tx = write_tx(&mut self.conn)?;
        for observation in observations {
            match record_observation(&tx, observation, now) {
                Ok(RobinhoodObservationOutcome::Recorded) => applied.recorded += 1,
                Ok(RobinhoodObservationOutcome::AlreadyRecorded) => applied.already_recorded += 1,
                Err(e) => {
                    // Explicit rollback rather than relying on the drop
                    // guard, so the failure path is the same shape as
                    // every other multi-step mutation in this crate.
                    let _ = tx.rollback();
                    return Err(e);
                }
            }
        }

        // The anchors. `ON CONFLICT` rather than a plain insert because a
        // rescan legitimately re-anchors a block already anchored, and
        // after a reorg the same height carries a different hash.
        //
        // Every block that held a deposit is anchored alongside the
        // cursor, not just the range's last block: those are precisely
        // the heights where being wrong about which block was canonical
        // would matter, so they are the heights worth being able to
        // re-check directly on a later tick.
        for (block, hash) in deposit_block_anchors
            .iter()
            .copied()
            .chain(std::iter::once((cursor_block, cursor_hash)))
        {
            tx.execute(
                "INSERT INTO robinhood_scanned_blocks (block_number, block_hash, scanned_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(block_number) DO UPDATE SET block_hash = excluded.block_hash,
                    scanned_at = excluded.scanned_at",
                rusqlite::params![to_i64(block, "anchor_block")?, hash.as_slice(), now],
            )?;
        }

        // Prune, keeping the highest anchor below the floor so a reorg
        // deeper than the retention window is still DETECTABLE (it fails
        // to match that anchor) rather than silently unfalsifiable.
        let floor = cursor_block_i.saturating_sub(to_i64(retain_anchors_below, "retain")?);
        tx.execute(
            "DELETE FROM robinhood_scanned_blocks
             WHERE block_number < ?1
               AND block_number <> (SELECT MAX(block_number) FROM robinhood_scanned_blocks
                                    WHERE block_number < ?1)",
            [floor],
        )?;

        tx.commit()?;
        Ok(applied)
    }

    /// Promotes every `Provisional` observation that has reached
    /// `confirmation_depth` confirmations against `head` to `Final`.
    ///
    /// Depth is counted the same way `goldcoin::indexer` counts it —
    /// `head - block_number + 1`, so a `confirmation_depth` of 1 means
    /// "in the head block itself". Returns the obligation indexes
    /// promoted, for the caller's log.
    ///
    /// Deliberately performs no RPC and re-verifies no block hash. Its
    /// caller has already established, this same tick, that the scan
    /// cursor's block is canonical — and a canonical block certifies its
    /// entire ancestry — while any observation orphaned by a reorg was
    /// tombstoned before this runs. Re-checking here would be a second,
    /// weaker copy of a guarantee that already holds.
    pub fn robinhood_promote_final(
        &mut self,
        head: u64,
        confirmation_depth: u64,
        now: i64,
    ) -> Result<Vec<u64>, LedgerError> {
        // The highest block that is deep enough to be final. Saturating:
        // a head shallower than the depth simply promotes nothing.
        let head = to_i64(head, "head")?;
        let depth = to_i64(confirmation_depth.max(1), "confirmation_depth")?;
        let final_at_or_below = head - depth + 1;

        let tx = write_tx(&mut self.conn)?;
        let promoted: Vec<u64> = {
            let mut stmt = tx.prepare(
                "SELECT source_obligation_index FROM robinhood_deposit_observations
                 WHERE finality = 'Provisional' AND block_number <= ?1
                 ORDER BY block_number, log_index",
            )?;
            let rows: Result<Vec<u64>, _> = stmt
                .query_map([final_at_or_below], |r| {
                    r.get::<_, i64>(0).map(|v| v as u64)
                })?
                .collect();
            rows?
        };
        tx.execute(
            "UPDATE robinhood_deposit_observations
                SET finality = 'Final', finalized_at = ?2
             WHERE finality = 'Provisional' AND block_number <= ?1",
            rusqlite::params![final_at_or_below, now],
        )?;
        tx.commit()?;
        Ok(promoted)
    }

    /// Which `Final` observations sit in a block above `fork_block` —
    /// i.e. which irreversible sightings a rollback to `fork_block` would
    /// contradict.
    ///
    /// Read-only, and called BEFORE any rollback. A non-empty result is
    /// the post-finality reorg case: it is never reconciled automatically
    /// (the rollback below deliberately only ever touches `Provisional`
    /// rows), it halts the indexer for a human. Same shape and same
    /// reasoning as `Ledger::detect_post_finality_reorg` on the Goldcoin
    /// side.
    pub fn robinhood_final_observations_above(
        &self,
        fork_block: u64,
    ) -> Result<Vec<u64>, LedgerError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT source_obligation_index FROM robinhood_deposit_observations
             WHERE finality = 'Final' AND block_number > ?1
             ORDER BY block_number, log_index",
        )?;
        let rows = stmt
            .query_map([to_i64(fork_block, "fork_block")?], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rolls the scan cursor back to `fork_block`, tombstones every
    /// `Provisional` observation above it, and journals the reorg.
    ///
    /// `Final` rows are never touched — by the time this runs, the caller
    /// has already established via
    /// [`Ledger::robinhood_final_observations_above`] that there are
    /// none above the fork. Tombstoning (rather than deleting) is what
    /// frees the durable identity for whatever the canonical chain
    /// actually contains, while keeping a record of what was believed;
    /// both identity indexes are scoped past tombstones for exactly this.
    ///
    /// Returns how many observations were orphaned.
    pub fn robinhood_rollback_reorg(
        &mut self,
        fork_block: u64,
        fork_hash: [u8; 32],
        old_tip_block: u64,
        old_tip_hash: [u8; 32],
        now: i64,
    ) -> Result<i64, LedgerError> {
        let fork_block_i = to_i64(fork_block, "fork_block")?;
        let old_tip_block_i = to_i64(old_tip_block, "old_tip_block")?;

        let tx = write_tx(&mut self.conn)?;
        // Refuse rather than silently downgrade: rolling back over a
        // final observation is the incident the caller was supposed to
        // detect first, and this is the last place it can be caught.
        let final_above: i64 = tx.query_row(
            "SELECT COUNT(*) FROM robinhood_deposit_observations
             WHERE finality = 'Final' AND block_number > ?1",
            [fork_block_i],
            |r| r.get(0),
        )?;
        if final_above != 0 {
            let _ = tx.rollback();
            return Err(LedgerError::RobinhoodPostFinalityReorg {
                fork_block,
                finalized_above: final_above,
            });
        }

        let orphaned = tx.execute(
            "UPDATE robinhood_deposit_observations
                SET finality = 'Reorged', reorged_at = ?2
             WHERE finality = 'Provisional' AND block_number > ?1",
            rusqlite::params![fork_block_i, now],
        )? as i64;

        tx.execute(
            "DELETE FROM robinhood_scanned_blocks WHERE block_number > ?1",
            [fork_block_i],
        )?;
        // Re-anchor the fork block itself at its LIVE hash: it is the new
        // cursor, and the caller has just verified this hash against the
        // chain.
        tx.execute(
            "INSERT INTO robinhood_scanned_blocks (block_number, block_hash, scanned_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(block_number) DO UPDATE SET block_hash = excluded.block_hash,
                scanned_at = excluded.scanned_at",
            rusqlite::params![fork_block_i, fork_hash.as_slice(), now],
        )?;
        tx.execute(
            "INSERT INTO robinhood_reorg_events
                (detected_at, fork_block, fork_hash, old_tip_block, old_tip_hash, orphaned_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                now,
                fork_block_i,
                fork_hash.as_slice(),
                old_tip_block_i,
                old_tip_hash.as_slice(),
                orphaned
            ],
        )?;
        tx.commit()?;
        Ok(orphaned)
    }

    // ---------------------------------------------------------- halt state --

    /// The persisted Robinhood halt, if the indexer is stopped.
    pub fn robinhood_halt(&self) -> Result<Option<RobinhoodHalt>, LedgerError> {
        let row: Option<(String, Option<String>, i64)> = self
            .conn
            .query_row(
                "SELECT halt_reason, halt_detail, halted_at FROM robinhood_indexer_state
                 WHERE id = 0 AND halt_reason IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((reason, detail, halted_at)) = row else {
            return Ok(None);
        };
        let reason = reason
            .parse()
            .map_err(LedgerError::RobinhoodStateMalformed)?;
        Ok(Some(RobinhoodHalt {
            reason,
            detail: detail.unwrap_or_default(),
            halted_at,
        }))
    }

    /// Records a halt. Idempotent in the direction that matters: the
    /// FIRST reason wins and later calls do not overwrite it, because the
    /// first thing that went wrong is what an operator needs to see, and
    /// a halted indexer's subsequent complaints are consequences.
    pub fn robinhood_record_halt(
        &mut self,
        reason: RobinhoodHaltReason,
        detail: impl Into<String>,
        now: i64,
    ) -> Result<(), LedgerError> {
        let detail = detail.into();
        self.conn.execute(
            "INSERT INTO robinhood_indexer_state (id, halt_reason, halt_detail, halted_at, updated_at)
             VALUES (0, ?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET
                halt_reason = COALESCE(robinhood_indexer_state.halt_reason, excluded.halt_reason),
                halt_detail = COALESCE(robinhood_indexer_state.halt_detail, excluded.halt_detail),
                halted_at   = COALESCE(robinhood_indexer_state.halted_at,   excluded.halted_at),
                updated_at  = excluded.updated_at",
            rusqlite::params![reason.as_str(), detail, now],
        )?;
        Ok(())
    }

    /// Clears a halt. Operator action only — nothing in the tick loop
    /// calls this, by design: a halt that could clear itself would let a
    /// restart look like a diagnosis.
    pub fn robinhood_clear_halt(&mut self, now: i64) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE robinhood_indexer_state
                SET halt_reason = NULL, halt_detail = NULL, halted_at = NULL, updated_at = ?1
             WHERE id = 0",
            [now],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- reading --

    pub fn robinhood_observation_summary(
        &self,
    ) -> Result<RobinhoodObservationSummary, LedgerError> {
        let conn = &self.conn;
        let (provisional, finalized, reorged): (i64, i64, i64) = conn.query_row(
            "SELECT
               SUM(finality = 'Provisional'),
               SUM(finality = 'Final'),
               SUM(finality = 'Reorged')
             FROM robinhood_deposit_observations",
            [],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                ))
            },
        )?;
        let highest_finalized_block: Option<i64> = conn.query_row(
            "SELECT MAX(block_number) FROM robinhood_deposit_observations WHERE finality = 'Final'",
            [],
            |r| r.get(0),
        )?;
        Ok(RobinhoodObservationSummary {
            provisional,
            finalized,
            reorged,
            highest_finalized_block: highest_finalized_block.map(|v| v as u64),
        })
    }

    /// Every observation, oldest first. Read-only visibility surface —
    /// this is what "observe disabled routes for visibility" actually
    /// means in practice, and nothing downstream of it can settle.
    pub fn robinhood_observations(&self) -> Result<Vec<RobinhoodObservationRow>, LedgerError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT id, source_contract, source_obligation_index, route, depositor, destination,
                    amount_robinhood_atomic, amount_canonical_atomic, tx_hash, log_index,
                    block_number, block_hash, finality, observed_at, finalized_at, reorged_at
             FROM robinhood_deposit_observations
             ORDER BY block_number, log_index, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let route: String = r.get(3)?;
                let route: Route = route.parse().map_err(|_| unknown_route_error(3))?;
                Ok(RobinhoodObservationRow {
                    id: r.get(0)?,
                    observation: RobinhoodDepositObservation {
                        source_contract: blob20(&r.get::<_, Vec<u8>>(1)?),
                        obligation_index: r.get::<_, i64>(2)? as u64,
                        route,
                        depositor: blob20(&r.get::<_, Vec<u8>>(4)?),
                        destination: r.get(5)?,
                        amount_robinhood_atomic: blob32(&r.get::<_, Vec<u8>>(6)?),
                        amount_canonical_atomic: r.get::<_, i64>(7)? as u64,
                        tx_hash: blob32(&r.get::<_, Vec<u8>>(8)?),
                        log_index: r.get::<_, i64>(9)? as u64,
                        block_number: r.get::<_, i64>(10)? as u64,
                        block_hash: blob32(&r.get::<_, Vec<u8>>(11)?),
                    },
                    finality: r.get(12)?,
                    observed_at: r.get(13)?,
                    finalized_at: r.get(14)?,
                    reorged_at: r.get(15)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// A route string in the database that this binary's [`Route`] does not
/// know. Only reachable if a row was written by something other than this
/// binary, since the column carries a `CHECK` — refused rather than
/// guessed at.
fn unknown_route_error(column: usize) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "route column holds a value this binary does not recognise",
        )),
    )
}

/// Inserts one observation, or proves an existing row says exactly the
/// same thing. See this module's docs for why the third outcome is an
/// error rather than a variant.
fn record_observation(
    tx: &rusqlite::Connection,
    observation: &RobinhoodDepositObservation,
    now: i64,
) -> Result<RobinhoodObservationOutcome, LedgerError> {
    let obligation_index = to_i64(observation.obligation_index, "obligation_index")?;

    // The live row for this durable identity, if any. Scoped past
    // tombstones, exactly like the unique index that enforces it — an
    // orphaned sighting must not block the canonical chain's version of
    // the same obligation.
    let existing: Option<RobinhoodDepositObservation> = tx
        .query_row(
            "SELECT source_contract, source_obligation_index, route, depositor, destination,
                    amount_robinhood_atomic, amount_canonical_atomic, tx_hash, log_index,
                    block_number, block_hash
             FROM robinhood_deposit_observations
             WHERE source_chain = 'robinhood' AND source_contract = ?1
               AND source_obligation_index = ?2 AND finality <> 'Reorged'",
            rusqlite::params![observation.source_contract.as_slice(), obligation_index],
            |r| {
                let route: String = r.get(2)?;
                Ok(RobinhoodDepositObservation {
                    source_contract: blob20(&r.get::<_, Vec<u8>>(0)?),
                    obligation_index: r.get::<_, i64>(1)? as u64,
                    // The column carries a CHECK admitting only the two
                    // inbound spellings, so a value this enum cannot parse
                    // means the row was written by something other than
                    // this binary. Refused, never defaulted.
                    route: route.parse().map_err(|_| unknown_route_error(2))?,
                    depositor: blob20(&r.get::<_, Vec<u8>>(3)?),
                    destination: r.get(4)?,
                    amount_robinhood_atomic: blob32(&r.get::<_, Vec<u8>>(5)?),
                    amount_canonical_atomic: r.get::<_, i64>(6)? as u64,
                    tx_hash: blob32(&r.get::<_, Vec<u8>>(7)?),
                    log_index: r.get::<_, i64>(8)? as u64,
                    block_number: r.get::<_, i64>(9)? as u64,
                    block_hash: blob32(&r.get::<_, Vec<u8>>(10)?),
                })
            },
        )
        .optional()?;

    if let Some(stored) = existing {
        return match first_disagreement(&stored, observation) {
            None => Ok(RobinhoodObservationOutcome::AlreadyRecorded),
            Some(conflict) => Err(LedgerError::RobinhoodObservationConflict(Box::new(
                conflict,
            ))),
        };
    }

    tx.execute(
        "INSERT INTO robinhood_deposit_observations
            (source_chain, source_contract, source_obligation_index, contract_route_id, route,
             depositor, destination, amount_robinhood_atomic, amount_canonical_atomic,
             tx_hash, log_index, block_number, block_hash, finality, observed_at, settled)
         VALUES ('robinhood', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 'Provisional', ?13, 0)",
        rusqlite::params![
            observation.source_contract.as_slice(),
            obligation_index,
            // `contract_route_id` comes from the single source of truth
            // for the wire mapping rather than being spelled out again
            // here. The decoder only ever produces inbound routes, so the
            // `expect` is unreachable by construction; it is not an
            // `unwrap_or` because a silent fallback would write a route
            // byte that the column's CHECK would then reject with a much
            // less informative error.
            observation
                .route
                .contract_route_id()
                .expect("an observation's route always has a contract route id"),
            observation.route.as_str(),
            observation.depositor.as_slice(),
            observation.destination.as_slice(),
            observation.amount_robinhood_atomic.as_slice(),
            to_i64(
                observation.amount_canonical_atomic,
                "amount_canonical_atomic"
            )?,
            observation.tx_hash.as_slice(),
            to_i64(observation.log_index, "log_index")?,
            to_i64(observation.block_number, "block_number")?,
            observation.block_hash.as_slice(),
            now,
        ],
    )?;
    Ok(RobinhoodObservationOutcome::Recorded)
}

/// The first field on which two observations of one identity disagree.
///
/// Field-by-field rather than a derived `PartialEq` on purpose: equality
/// would answer "are these the same?" while an operator staring at a
/// halted indexer needs "which fact changed?". The list is exhaustive
/// over every recorded column, and adding a column without adding it here
/// would weaken the conflict check silently — hence the destructuring
/// below, which makes that a compile error.
fn first_disagreement(
    stored: &RobinhoodDepositObservation,
    observed: &RobinhoodDepositObservation,
) -> Option<RobinhoodObservationConflict> {
    // Destructured so a new field on the struct fails to compile here
    // until it is added to the comparison list.
    let RobinhoodDepositObservation {
        source_contract,
        obligation_index,
        route,
        depositor,
        destination,
        amount_robinhood_atomic,
        amount_canonical_atomic,
        tx_hash,
        log_index,
        block_number,
        block_hash,
    } = stored;

    macro_rules! check {
        ($field:literal, $stored:expr, $observed:expr) => {
            if $stored != $observed {
                return Some(RobinhoodObservationConflict {
                    obligation_index: *obligation_index,
                    field: $field,
                    stored: format!("{:?}", $stored),
                    observed: format!("{:?}", $observed),
                });
            }
        };
    }

    check!(
        "source_contract",
        source_contract,
        &observed.source_contract
    );
    check!(
        "obligation_index",
        obligation_index,
        &observed.obligation_index
    );
    check!("route", route, &observed.route);
    check!("depositor", depositor, &observed.depositor);
    check!("destination", destination, &observed.destination);
    check!(
        "amount_robinhood_atomic",
        amount_robinhood_atomic,
        &observed.amount_robinhood_atomic
    );
    check!(
        "amount_canonical_atomic",
        amount_canonical_atomic,
        &observed.amount_canonical_atomic
    );
    check!("tx_hash", tx_hash, &observed.tx_hash);
    check!("log_index", log_index, &observed.log_index);
    check!("block_number", block_number, &observed.block_number);
    check!("block_hash", block_hash, &observed.block_hash);
    None
}

#[cfg(test)]
mod tests;
