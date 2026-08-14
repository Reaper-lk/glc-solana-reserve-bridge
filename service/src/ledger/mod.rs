//! The reserve ledger: reservation/capacity accounting and the
//! bridge-request state machine (docs/04-state-machines.md,
//! docs/05-reserve-accounting.md). Owns every mutation of
//! `bridge_requests`/`reserve_ledger` — chain-observation modules
//! (`goldcoin::indexer`, `solana::indexer`) call into this module rather
//! than touching SQL directly, so the accounting invariant is enforced in
//! exactly one place.
//!
//! # Concurrency and crash safety
//!
//! SQLite serializes writers DB-wide; every mutating operation here runs
//! inside a single `BEGIN IMMEDIATE` transaction that either fully commits
//! or fully rolls back, which is what makes "reservation and settlement
//! bookkeeping" race-free per docs/05-reserve-accounting.md without a
//! separate row-lock primitive — SQLite's write lock IS the lock. A crash
//! mid-operation leaves the last COMMITted state on disk (WAL mode); there
//! is no partial-write state to recover from, and every observation-
//! processing entry point below is additionally idempotent (checked via a
//! UNIQUE constraint or an explicit already-processed check) so replaying
//! the same chain event after a restart is always safe (constraint 5).

mod schema;
mod types;

pub use types::{BridgeRequest, Direction, RequestState, ReserveDirection};

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("reserve {0:?} has not been initialized")]
    ReserveNotInitialized(ReserveDirection),
    #[error("bridge request {0} not found")]
    RequestNotFound(i64),
    #[error(
        "accounting invariant violated for {direction:?}: balance {balance} < protected_minimum \
         {protected_minimum} + reserved_liquidity {reserved_liquidity}"
    )]
    InvariantViolated {
        direction: ReserveDirection,
        balance: i64,
        protected_minimum: i64,
        reserved_liquidity: i64,
    },
}

pub struct Ledger {
    conn: Connection,
}

/// `(from_state, to_state, at, reason)` — one row of a request's audit
/// trail, per [`Ledger::state_log`].
pub type StateLogEntry = (Option<RequestState>, RequestState, i64, Option<String>);

/// Outcome of [`Ledger::create_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRequestOutcome {
    /// Capacity reserved; request created in `AwaitingDeposit`.
    Reserved { request_id: i64 },
    /// Never accept a transfer that cannot be fulfilled (docs/05): no row
    /// is created, no capacity is touched.
    InsufficientLiquidity { available_capacity: i64 },
    /// The destination reserve (or the bridge globally) is paused.
    Paused,
}

/// Outcome of [`Ledger::record_glc_deposit_observed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlcObservationOutcome {
    Recorded,
    /// Already recorded for this exact request+txid+vout (restart replay).
    AlreadyRecorded,
    /// No `AwaitingDeposit` request exists with this id/direction — the
    /// vault payment is real but unmatched. Callers should log it to
    /// [`Ledger::record_unmatched_goldcoin_deposit`] for audit rather than
    /// discard it (never silently ignore a real vault payment).
    NoMatchingRequest,
    /// Observed amount does not equal the request's reserved amount — the
    /// deposit is recorded but routed to `ManualReview` rather than
    /// silently accepted (constraint 6/10: never let an observed amount
    /// override what capacity was actually reserved for).
    AmountMismatch {
        expected: u64,
        observed: u64,
    },
}

/// Outcome of [`Ledger::fold_sol_deposit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolFoldOutcome {
    /// Capacity was available; a request was created directly in
    /// `SourceFinalized` (Solana finality is a single instant at the
    /// commitment level, unlike Goldcoin's confirmation-depth ramp — see
    /// module docs on the asymmetry).
    FoldedFinalized { request_id: i64 },
    /// Already folded for this obligation index (restart replay).
    AlreadyFolded { request_id: i64 },
    /// No pre-existing reservation is possible for this direction (the
    /// on-chain `deposit_to_reserve` instruction has no reservation-
    /// correlation parameter — see module docs) and capacity was NOT
    /// available at fold time. The deposit is real and irreversible on
    /// Solana; it is recorded in `ManualReview`, never dropped.
    FoldedManualReview { request_id: i64 },
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let conn = Connection::open(path)?;
        schema::open_and_migrate(&conn)?;
        Ok(Ledger { conn })
    }

    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        schema::open_and_migrate(&conn)?;
        Ok(Ledger { conn })
    }

    // ------------------------------------------------------------ reserve setup --

    /// Initializes (or re-parameterizes) a reserve's threshold configuration.
    /// Idempotent — safe to call at every startup with the current config.
    /// Does not touch `reserved_liquidity`/`pending_obligations`, which are
    /// derived from live `bridge_requests`, not configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_reserve(
        &mut self,
        direction: ReserveDirection,
        initial_balance: u64,
        protected_minimum: u64,
        target_reserve: u64,
        warning_reserve: u64,
        critical_reserve: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        assert!(
            critical_reserve > protected_minimum,
            "critical_reserve must exceed protected_minimum (docs/05-reserve-accounting.md)"
        );
        self.conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(direction) DO UPDATE SET
                protected_minimum = excluded.protected_minimum,
                target_reserve = excluded.target_reserve,
                warning_reserve = excluded.warning_reserve,
                critical_reserve = excluded.critical_reserve",
            rusqlite::params![
                direction,
                initial_balance as i64,
                now,
                protected_minimum as i64,
                target_reserve as i64,
                warning_reserve as i64,
                critical_reserve as i64,
            ],
        )?;
        Ok(())
    }

    /// Updates the cached live-chain balance (called by reconciliation after
    /// a real chain read — never guessed, never left stale silently: callers
    /// must pass an actually-observed balance).
    pub fn refresh_reserve_balance(
        &mut self,
        direction: ReserveDirection,
        observed_balance: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, balance_refreshed_at = ?2
             WHERE direction = ?3",
            rusqlite::params![observed_balance as i64, now, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn set_paused(
        &mut self,
        direction: ReserveDirection,
        paused: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET paused = ?1, pause_reason = ?2 WHERE direction = ?3",
            rusqlite::params![paused as i64, reason, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn is_paused(&self, direction: ReserveDirection) -> Result<bool, LedgerError> {
        let paused: i64 = self
            .conn
            .query_row(
                "SELECT paused FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(paused != 0)
    }

    /// `total_reserve_balance - protected_minimum - reserved_liquidity`
    /// (docs/05-reserve-accounting.md). Not clamped at zero deliberately:
    /// a negative value is itself diagnostic (see [`Ledger::check_invariant`]).
    pub fn available_capacity(&self, direction: ReserveDirection) -> Result<i64, LedgerError> {
        let (balance, protected_minimum, reserved) = self.reserve_row(direction)?;
        Ok(balance - protected_minimum - reserved)
    }

    fn reserve_row(&self, direction: ReserveDirection) -> Result<(i64, i64, i64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    /// Asserts `available reserves >= all releases that can currently become
    /// payable` — i.e. `total_reserve_balance >= protected_minimum +
    /// reserved_liquidity`. Called defensively by tests after every mutating
    /// operation and by reconciliation; a violation here means the ledger's
    /// own bookkeeping has diverged from what it promised, which must never
    /// happen by construction and is treated as a hard error, not a
    /// warning.
    pub fn check_invariant(&self, direction: ReserveDirection) -> Result<(), LedgerError> {
        let (balance, protected_minimum, reserved) = self.reserve_row(direction)?;
        if balance < protected_minimum + reserved {
            return Err(LedgerError::InvariantViolated {
                direction,
                balance,
                protected_minimum,
                reserved_liquidity: reserved,
            });
        }
        Ok(())
    }

    // -------------------------------------------------------------- reservation --

    /// Never accept a transfer that cannot be fulfilled: capacity check and
    /// reservation write are one atomic transaction.
    pub fn create_request(
        &mut self,
        direction: Direction,
        amount: u64,
        recipient: &[u8],
        requester: Option<[u8; 32]>,
        reservation_ttl_secs: i64,
        now: i64,
    ) -> Result<CreateRequestOutcome, LedgerError> {
        let reserve = direction.destination_reserve();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let paused: i64 = tx.query_row(
            "SELECT paused FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| r.get(0),
        )?;
        if paused != 0 {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::Paused);
        }

        let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
            "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let available = balance - protected_minimum - reserved;
        if (amount as i64) > available {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::InsufficientLiquidity {
                available_capacity: available,
            });
        }

        tx.execute(
            "INSERT INTO bridge_requests
                (direction, state, amount_atomic, recipient, requester, created_at,
                 reserved_at, reservation_expires_at, source_confirmations)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 0)",
            rusqlite::params![
                direction,
                RequestState::AwaitingDeposit,
                amount as i64,
                recipient,
                requester.map(|r| r.to_vec()),
                now,
                now + reservation_ttl_secs,
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            RequestState::LiquidityReserved,
            now,
            None,
            "system",
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::LiquidityReserved),
            RequestState::AwaitingDeposit,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1 WHERE direction = ?2",
            rusqlite::params![amount as i64, reserve],
        )?;
        tx.commit()?;
        Ok(CreateRequestOutcome::Reserved { request_id })
    }

    /// Sweeps `AwaitingDeposit`/`LiquidityReserved` requests past their
    /// `reservation_expires_at`, releasing their reserved capacity. Returns
    /// the number expired. Idempotent — a request already past `Expired`
    /// is never matched again by the `WHERE` clause.
    pub fn expire_reservations(&mut self, now: i64) -> Result<u32, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut stmt = tx.prepare(
            "SELECT id, direction, amount_atomic FROM bridge_requests
             WHERE state = 'AwaitingDeposit' AND reservation_expires_at IS NOT NULL
               AND reservation_expires_at <= ?1",
        )?;
        let rows: Vec<(i64, Direction, i64)> = stmt
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        let mut count = 0u32;
        for (id, direction, amount) in rows {
            tx.execute(
                "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
                rusqlite::params![RequestState::Expired, id],
            )?;
            log_transition(
                &tx,
                id,
                Some(RequestState::AwaitingDeposit),
                RequestState::Expired,
                now,
                Some("reservation_ttl_elapsed"),
                "system",
            )?;
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1 WHERE direction = ?2",
                rusqlite::params![amount, direction.destination_reserve()],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Operator/user cancellation before a deposit is observed. Same
    /// capacity-release effect as expiry, distinct reason.
    pub fn cancel_request(&mut self, id: i64, now: i64, note: &str) -> Result<(), LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (direction, amount, state): (Direction, i64, RequestState) = tx
            .query_row(
                "SELECT direction, amount_atomic, state FROM bridge_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(id))?;
        assert!(
            matches!(
                state,
                RequestState::LiquidityReserved | RequestState::AwaitingDeposit
            ),
            "cancel_request called on a request past reservation ({state:?}); caller bug"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = ?2 WHERE id = ?3",
            rusqlite::params![RequestState::Cancelled, note, id],
        )?;
        log_transition(
            &tx,
            id,
            Some(state),
            RequestState::Cancelled,
            now,
            Some(note),
            "operator",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1 WHERE direction = ?2",
            rusqlite::params![amount, direction.destination_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ Goldcoin leg --

    /// Looks up a request by id, for the Goldcoin indexer's OP_RETURN-
    /// encoded-id correlation (docs/01-reuse-inventory.md notes this
    /// replaces recipient-only matching to remove FIFO ambiguity).
    pub fn get_request(&self, id: i64) -> Result<Option<BridgeRequest>, LedgerError> {
        self.conn
            .query_row(SELECT_REQUEST, [id], row_to_request)
            .optional()
            .map_err(LedgerError::from)
    }

    /// Records that a candidate Goldcoin deposit binds to `request_id`.
    /// Idempotent on `(source_txid, source_vout)` — calling this twice with
    /// the same observation after a restart returns `AlreadyRecorded`
    /// rather than erroring or double-counting.
    #[allow(clippy::too_many_arguments)]
    pub fn record_glc_deposit_observed(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        vout: u32,
        observed_amount: u64,
        block_height: i64,
        block_hash: [u8; 32],
        now: i64,
    ) -> Result<GlcObservationOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(Direction, RequestState, i64, Option<Vec<u8>>)> = tx
            .query_row(
                "SELECT direction, state, amount_atomic, source_txid FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((direction, state, reserved_amount, existing_txid)) = row else {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        };

        if (state == RequestState::DepositObserved || state == RequestState::Confirming)
            && existing_txid.as_deref() == Some(txid.as_slice())
        {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::AlreadyRecorded);
        }
        if direction != Direction::GlcToSol || state != RequestState::AwaitingDeposit {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        }

        if observed_amount != reserved_amount as u64 {
            tx.execute(
                "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                    source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6
                 WHERE id = ?7",
                rusqlite::params![
                    RequestState::ManualReview,
                    txid.as_slice(),
                    vout,
                    block_height,
                    block_hash.as_slice(),
                    format!("deposit_amount_mismatch: expected {reserved_amount} observed {observed_amount}"),
                    request_id,
                ],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::AwaitingDeposit),
                RequestState::ManualReview,
                now,
                Some("deposit_amount_mismatch"),
                "system",
            )?;
            tx.commit()?;
            return Ok(GlcObservationOutcome::AmountMismatch {
                expected: reserved_amount as u64,
                observed: observed_amount,
            });
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                source_block_height = ?4, source_block_hash = ?5, source_confirmations = 1
             WHERE id = ?6",
            rusqlite::params![
                RequestState::DepositObserved,
                txid.as_slice(),
                vout,
                block_height,
                block_hash.as_slice(),
                request_id,
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::AwaitingDeposit),
            RequestState::DepositObserved,
            now,
            None,
            "system",
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::DepositObserved),
            RequestState::Confirming,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::Confirming, request_id],
        )?;
        tx.commit()?;
        Ok(GlcObservationOutcome::Recorded)
    }

    /// A real vault payment that could not be matched to any pending
    /// request — recorded for audit rather than dropped (constraint: never
    /// silently ignore a real chain observation).
    pub fn record_unmatched_goldcoin_deposit(
        &mut self,
        txid: [u8; 32],
        vout: u32,
        amount_atomic: u64,
        block_height: i64,
        reason: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS unmatched_goldcoin_deposits (
                id INTEGER PRIMARY KEY, txid BLOB NOT NULL, vout INTEGER NOT NULL,
                amount_atomic INTEGER NOT NULL, block_height INTEGER NOT NULL,
                reason TEXT NOT NULL, discovered_at INTEGER NOT NULL,
                UNIQUE(txid, vout)
             )",
            [],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO unmatched_goldcoin_deposits
                (txid, vout, amount_atomic, block_height, reason, discovered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                txid.as_slice(),
                vout,
                amount_atomic as i64,
                block_height,
                reason,
                now
            ],
        )?;
        Ok(())
    }

    /// Updates confirmation depth for a `Confirming` request; a no-op if
    /// the depth hasn't increased (idempotent under repeated ticks).
    pub fn update_glc_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE bridge_requests SET source_confirmations = ?1
             WHERE id = ?2 AND state = 'Confirming' AND source_confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        Ok(())
    }

    /// `Confirming -> SourceFinalized`: the source deposit is now treated as
    /// an irreversible fact. Moves the amount into `pending_obligations`
    /// (docs/05: committed exposure that can no longer safely expire).
    /// Idempotent: a no-op if already `SourceFinalized`.
    pub fn mark_glc_source_finalized(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(Direction, RequestState, i64)> = tx
            .query_row(
                "SELECT direction, state, amount_atomic FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((direction, state, amount)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if state == RequestState::SourceFinalized {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::Confirming,
            "mark_glc_source_finalized on unexpected state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_finalized_at = ?2 WHERE id = ?3",
            rusqlite::params![RequestState::SourceFinalized, now, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::SourceFinalized,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
            rusqlite::params![amount, direction.destination_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Pre-finality reorg: the block carrying the deposit was orphaned.
    /// Releases the source-txid claim and returns the request to
    /// `AwaitingDeposit` so a future re-observation (same or different
    /// qualifying transaction) can bind cleanly — a documented
    /// simplification of docs/04-state-machines.md's "retry via Confirming
    /// if the tx still exists" vs "AwaitingDeposit if gone" distinction:
    /// this always retries via `AwaitingDeposit`, which is safe (the next
    /// indexer tick re-discovers the deposit if it is still valid, in
    /// whichever block it ends up mined in) at the cost of one extra
    /// confirmation cycle in the same-block-different-branch case.
    /// Reserved liquidity is NOT released (the reservation is still live,
    /// just waiting for a fresh confirmation) — only the source binding is
    /// cleared. Never callable once `SourceFinalized` (irreversible by
    /// policy; see docs/10-threat-model.md's post-finality-reorg section).
    pub fn mark_glc_reorged(&mut self, request_id: i64, now: i64) -> Result<(), LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: RequestState = tx
            .query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        assert!(
            matches!(
                state,
                RequestState::DepositObserved | RequestState::Confirming
            ),
            "mark_glc_reorged called post-finality or pre-observation ({state:?}) — caller bug; \
             post-finality reorg must never auto-revert (docs/10-threat-model.md)"
        );
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::Reorged,
            now,
            Some("block_orphaned"),
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_txid = NULL, source_vout = NULL,
                source_block_height = NULL, source_block_hash = NULL, source_confirmations = 0
             WHERE id = ?2",
            rusqlite::params![RequestState::AwaitingDeposit, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::Reorged),
            RequestState::AwaitingDeposit,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------- Solana leg --

    /// Folds an observed Solana `WithdrawalObligation` (a `deposit_to_reserve`
    /// execution, seen at `finalized` commitment) into the ledger. See the
    /// module docs and [`SolFoldOutcome`] for why this direction has no
    /// pre-existing-reservation match and instead reserves/commits capacity
    /// retroactively, and why it folds directly to `SourceFinalized` (Solana
    /// finality is a single instant, unlike Goldcoin's depth ramp).
    /// Idempotent on `source_obligation_index`.
    pub fn fold_sol_deposit(
        &mut self,
        obligation_index: u64,
        amount: u64,
        requester: [u8; 32],
        recipient_glc_address: &[u8],
        now: i64,
    ) -> Result<SolFoldOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bridge_requests WHERE source_obligation_index = ?1",
                [obligation_index as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.rollback()?;
            return Ok(SolFoldOutcome::AlreadyFolded { request_id: id });
        }

        let reserve = ReserveDirection::GoldcoinReserve;
        let paused: i64 = tx.query_row(
            "SELECT paused FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| r.get(0),
        )?;
        let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
            "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let available = balance - protected_minimum - reserved;
        let capacity_ok = paused == 0 && (amount as i64) <= available;

        tx.execute(
            "INSERT INTO bridge_requests
                (direction, state, amount_atomic, recipient, requester, created_at,
                 reserved_at, source_obligation_index, source_confirmations, source_finalized_at,
                 manual_review_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 1, ?6, ?8)",
            rusqlite::params![
                Direction::SolToGlc,
                if capacity_ok {
                    RequestState::SourceFinalized
                } else {
                    RequestState::ManualReview
                },
                amount as i64,
                recipient_glc_address,
                requester.as_slice(),
                now,
                obligation_index as i64,
                if capacity_ok {
                    None
                } else {
                    Some("insufficient_capacity_at_fold")
                },
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            if capacity_ok {
                RequestState::SourceFinalized
            } else {
                RequestState::ManualReview
            },
            now,
            Some("retroactive_fold_sol_deposit"),
            "system",
        )?;

        if capacity_ok {
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                    pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
                rusqlite::params![amount as i64, reserve],
            )?;
        }
        tx.commit()?;

        Ok(if capacity_ok {
            SolFoldOutcome::FoldedFinalized { request_id }
        } else {
            SolFoldOutcome::FoldedManualReview { request_id }
        })
    }

    pub fn last_synced_obligation_count(&self) -> Result<u64, LedgerError> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT last_obligation_count FROM solana_indexer_state WHERE id = 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n as u64)
    }

    pub fn set_last_synced_obligation_count(
        &mut self,
        count: u64,
        slot: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO solana_indexer_state (id, last_obligation_count, last_checked_slot, updated_at)
             VALUES (0, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET last_obligation_count = ?1, last_checked_slot = ?2, updated_at = ?3",
            rusqlite::params![count as i64, slot as i64, now],
        )?;
        Ok(())
    }

    /// `(total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations)`
    /// — used by the reconciliation engine.
    pub fn reserve_snapshot(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u64, u64, u64, u64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => LedgerError::ReserveNotInitialized(direction),
                other => LedgerError::Sqlite(other),
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_reconciliation_finding(
        &mut self,
        direction: ReserveDirection,
        expected: i64,
        observed: i64,
        delta: i64,
        classification: &str,
        auto_paused: bool,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO reconciliation_findings
                (detected_at, direction, expected, observed, delta, classification, auto_paused)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                now,
                direction.as_str(),
                expected,
                observed,
                delta,
                classification,
                auto_paused as i64
            ],
        )?;
        Ok(())
    }

    // ---------------------------------------------------- Goldcoin chain tracking --

    /// The locally indexed tip, if any.
    pub fn goldcoin_chain_tip(&self) -> Result<Option<(i64, [u8; 32])>, LedgerError> {
        self.conn
            .query_row(
                "SELECT height, hash FROM goldcoin_indexed_blocks ORDER BY height DESC LIMIT 1",
                [],
                |r| {
                    let h: Vec<u8> = r.get(1)?;
                    Ok((r.get::<_, i64>(0)?, to_array32(&h)))
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    pub fn goldcoin_block_hash_at(&self, height: i64) -> Result<Option<[u8; 32]>, LedgerError> {
        self.conn
            .query_row(
                "SELECT hash FROM goldcoin_indexed_blocks WHERE height = ?1",
                [height],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map(|o| o.map(|v| to_array32(&v)))
            .map_err(LedgerError::from)
    }

    pub fn goldcoin_ingest_block(
        &mut self,
        height: i64,
        hash: [u8; 32],
        prev_hash: [u8; 32],
        block_time: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO goldcoin_indexed_blocks (height, hash, prev_hash, block_time, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(height) DO UPDATE SET hash = excluded.hash, prev_hash = excluded.prev_hash,
                block_time = excluded.block_time, indexed_at = excluded.indexed_at",
            rusqlite::params![
                height,
                hash.as_slice(),
                prev_hash.as_slice(),
                block_time,
                now
            ],
        )?;
        Ok(())
    }

    /// Rolls back locally indexed blocks above `fork_height`, records a
    /// reorg event, and reorgs (via [`Ledger::mark_glc_reorged`]) every
    /// active `GlcToSol` request whose source block was orphaned.
    /// `SourceFinalized`-or-later requests are never touched here — a
    /// post-finality reorg is a distinct, non-automatic incident (see
    /// `mark_glc_reorged`'s panic guard and docs/10-threat-model.md).
    pub fn goldcoin_rollback_reorg(
        &mut self,
        fork_height: i64,
        fork_hash: [u8; 32],
        old_tip_height: i64,
        old_tip_hash: [u8; 32],
        now: i64,
    ) -> Result<i64, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let affected: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM bridge_requests
                 WHERE direction = 'GlcToSol' AND state IN ('DepositObserved','Confirming')
                   AND source_block_height > ?1",
            )?;
            let rows: Result<Vec<i64>, _> = stmt.query_map([fork_height], |r| r.get(0))?.collect();
            rows?
        };
        for id in &affected {
            let state: RequestState = tx.query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
                 VALUES (?1, ?2, 'Reorged', ?3, 'block_orphaned', 'system')",
                rusqlite::params![id, state.as_str(), now],
            )?;
            tx.execute(
                "UPDATE bridge_requests SET state = 'AwaitingDeposit', source_txid = NULL,
                    source_vout = NULL, source_block_height = NULL, source_block_hash = NULL,
                    source_confirmations = 0 WHERE id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
                 VALUES (?1, 'Reorged', 'AwaitingDeposit', ?2, NULL, 'system')",
                rusqlite::params![id, now],
            )?;
        }

        tx.execute(
            "DELETE FROM goldcoin_indexed_blocks WHERE height > ?1",
            [fork_height],
        )?;
        tx.execute(
            "INSERT INTO goldcoin_reorg_events
                (detected_at, fork_height, old_tip_height, old_tip_hash, new_tip_height, new_tip_hash, orphaned_count)
             VALUES (?1, ?2, ?3, ?4, ?2, ?5, ?6)",
            rusqlite::params![now, fork_height, old_tip_height, old_tip_hash.as_slice(), fork_hash.as_slice(), affected.len() as i64],
        )?;
        tx.commit()?;
        Ok(affected.len() as i64)
    }

    pub fn goldcoin_reorg_event_count(&self) -> Result<i64, LedgerError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM goldcoin_reorg_events", [], |r| {
                r.get(0)
            })?)
    }

    // ----------------------------------------------------------------- queries --

    pub fn requests_by_state(
        &self,
        direction: Direction,
        state: RequestState,
    ) -> Result<Vec<BridgeRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_REQUEST_PREFIX} WHERE direction = ?1 AND state = ?2 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![direction, state], row_to_request)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn state_log(&self, request_id: i64) -> Result<Vec<StateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason FROM bridge_request_state_log
             WHERE request_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([request_id], |r| {
                let from: Option<String> = r.get(0)?;
                let to: String = r.get(1)?;
                Ok((
                    from.map(|s| s.parse().unwrap()),
                    to.parse().unwrap(),
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Direct SQL access for tests that need queries not otherwise exposed.
    /// Kept `pub(crate)` and test-only — production code (including
    /// `reconciliation`) should add a typed method above instead of
    /// reaching for raw SQL.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &Connection {
        &self.conn
    }
}

const SELECT_REQUEST_PREFIX: &str =
    "SELECT id, direction, state, amount_atomic, recipient, requester, \
    created_at, reserved_at, reservation_expires_at, source_txid, source_vout, \
    source_obligation_index, source_block_height, source_block_hash, source_confirmations, \
    source_finalized_at, failure_reason, manual_review_note FROM bridge_requests";
const SELECT_REQUEST: &str = "SELECT id, direction, state, amount_atomic, recipient, requester, \
    created_at, reserved_at, reservation_expires_at, source_txid, source_vout, \
    source_obligation_index, source_block_height, source_block_hash, source_confirmations, \
    source_finalized_at, failure_reason, manual_review_note FROM bridge_requests WHERE id = ?1";

fn row_to_request(r: &rusqlite::Row) -> rusqlite::Result<BridgeRequest> {
    let recipient_vec: Vec<u8> = r.get(4)?;
    let requester_vec: Option<Vec<u8>> = r.get(5)?;
    let source_txid_vec: Option<Vec<u8>> = r.get(9)?;
    let source_block_hash_vec: Option<Vec<u8>> = r.get(13)?;
    Ok(BridgeRequest {
        id: r.get(0)?,
        direction: r.get(1)?,
        state: r.get(2)?,
        amount_atomic: r.get::<_, i64>(3)? as u64,
        recipient: recipient_vec,
        requester: requester_vec.map(|v| to_array32(&v)),
        created_at: r.get(6)?,
        reserved_at: r.get(7)?,
        reservation_expires_at: r.get(8)?,
        source_txid: source_txid_vec.map(|v| to_array32(&v)),
        source_vout: r.get::<_, Option<i64>>(10)?.map(|v| v as u32),
        source_obligation_index: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        source_block_height: r.get(12)?,
        source_block_hash: source_block_hash_vec.map(|v| to_array32(&v)),
        source_confirmations: r.get(14)?,
        source_finalized_at: r.get(15)?,
        failure_reason: r.get(16)?,
        manual_review_note: r.get(17)?,
    })
}

fn to_array32(v: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = v.len().min(32);
    out[..n].copy_from_slice(&v[..n]);
    out
}

fn log_transition(
    conn: &Connection,
    request_id: i64,
    from: Option<RequestState>,
    to: RequestState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            request_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
