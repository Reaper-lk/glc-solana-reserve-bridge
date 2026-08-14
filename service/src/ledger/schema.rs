//! SQLite schema for the reserve ledger and chain indexers
//! (docs/06-schema.md). Single-file embedded database (rusqlite, bundled
//! SQLite) — same persistence choice the old bridge made for the same
//! reason (docs/01-reuse-inventory.md): a transactional, crash-safe,
//! zero-external-dependency store the indexer and ledger can share.
//!
//! Migrations are forward-only and numbered, applied at startup — same
//! discipline the old bridge's `db.rs` used. This repository starts fresh
//! at schema version 1 (docs/08-migration-strategy.md: there is no live
//! system to migrate data from).

use rusqlite::Connection;

use super::LedgerError;

const CURRENT_SCHEMA_VERSION: i64 = 1;

pub fn open_and_migrate(conn: &Connection) -> Result<(), LedgerError> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(LedgerError::from)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(LedgerError::from)?;

    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    let current: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
            r.get(0)
        })
        .ok();

    if current.is_none() {
        apply_v1(conn)?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [CURRENT_SCHEMA_VERSION],
        )?;
    }
    // Future migrations: `else if current < Some(2) { apply_v2(conn)?; ... }`
    // — forward-only, each step self-contained, matching the old bridge's
    // migration discipline.

    Ok(())
}

fn apply_v1(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        -- ---------------------------------------------------- Goldcoin chain tracking --
        CREATE TABLE goldcoin_indexed_blocks (
            height      INTEGER PRIMARY KEY,
            hash        BLOB NOT NULL UNIQUE,
            prev_hash   BLOB NOT NULL,
            block_time  INTEGER NOT NULL,
            indexed_at  INTEGER NOT NULL
        );

        CREATE TABLE goldcoin_reorg_events (
            id              INTEGER PRIMARY KEY,
            detected_at     INTEGER NOT NULL,
            fork_height     INTEGER NOT NULL,
            old_tip_height  INTEGER NOT NULL,
            old_tip_hash    BLOB NOT NULL,
            new_tip_height  INTEGER NOT NULL,
            new_tip_hash    BLOB NOT NULL,
            orphaned_count  INTEGER NOT NULL
        );

        -- ----------------------------------------------------- Solana chain tracking --
        -- Singleton: what the Solana indexer last observed, at finalized
        -- commitment. No block-level reorg tracking is needed here (unlike
        -- Goldcoin) because finalized commitment does not reorg in normal
        -- operation (docs/03-architecture.md).
        CREATE TABLE solana_indexer_state (
            id                     INTEGER PRIMARY KEY CHECK (id = 0),
            last_obligation_count  INTEGER NOT NULL DEFAULT 0,
            last_checked_slot      INTEGER NOT NULL DEFAULT 0,
            updated_at             INTEGER NOT NULL
        );

        -- ------------------------------------------------------------ bridge_requests --
        -- The single state-machine table spanning both directions
        -- (docs/04-state-machines.md, docs/06-schema.md).
        CREATE TABLE bridge_requests (
            id                          INTEGER PRIMARY KEY,
            direction                   TEXT NOT NULL CHECK (direction IN ('GlcToSol','SolToGlc')),
            state                       TEXT NOT NULL,
            amount_atomic               INTEGER NOT NULL CHECK (amount_atomic > 0),
            recipient                   BLOB NOT NULL,
            requester                   BLOB,
            created_at                  INTEGER NOT NULL,
            reserved_at                 INTEGER,
            reservation_expires_at      INTEGER,
            -- Goldcoin leg identity (GlcToSol source, or SolToGlc destination
            -- payout — recorded once known):
            source_txid                 BLOB,
            source_vout                 INTEGER,
            -- Solana leg identity (SolToGlc source): the WithdrawalObligation
            -- index is the canonical identifier — see goldcoin/indexer.rs and
            -- solana/indexer.rs module docs for why no separate "signature"
            -- field is needed.
            source_obligation_index     INTEGER,
            source_block_height         INTEGER,
            source_block_hash           BLOB,
            source_confirmations        INTEGER NOT NULL DEFAULT 0,
            source_finalized_at         INTEGER,
            settlement_claim_hash       BLOB,
            destination_txid            BLOB,
            destination_confirmations   INTEGER NOT NULL DEFAULT 0,
            settled_at                  INTEGER,
            failure_reason              TEXT,
            manual_review_note          TEXT
        );

        -- Replay guard (constraint 5), enforced structurally per direction:
        CREATE UNIQUE INDEX ux_bridge_requests_glc_source
            ON bridge_requests(source_txid, source_vout)
            WHERE source_txid IS NOT NULL;
        CREATE UNIQUE INDEX ux_bridge_requests_sol_source
            ON bridge_requests(source_obligation_index)
            WHERE source_obligation_index IS NOT NULL;

        CREATE INDEX ix_bridge_requests_state ON bridge_requests(direction, state);

        -- Append-only audit trail, same discipline as the old bridge's
        -- deposit_state_log/withdrawal_state_log.
        CREATE TABLE bridge_request_state_log (
            id          INTEGER PRIMARY KEY,
            request_id  INTEGER NOT NULL REFERENCES bridge_requests(id),
            from_state  TEXT,
            to_state    TEXT NOT NULL,
            at          INTEGER NOT NULL,
            reason      TEXT,
            actor       TEXT NOT NULL
        );

        -- --------------------------------------------------------------- reserve_ledger --
        CREATE TABLE reserve_ledger (
            direction                  TEXT PRIMARY KEY CHECK (direction IN ('GoldcoinReserve','SolanaReserve')),
            total_reserve_balance      INTEGER NOT NULL,
            balance_refreshed_at       INTEGER NOT NULL,
            protected_minimum          INTEGER NOT NULL,
            target_reserve             INTEGER NOT NULL,
            warning_reserve            INTEGER NOT NULL,
            critical_reserve           INTEGER NOT NULL,
            reserved_liquidity         INTEGER NOT NULL DEFAULT 0,
            pending_obligations        INTEGER NOT NULL DEFAULT 0,
            settled_liquidity_total    INTEGER NOT NULL DEFAULT 0,
            paused                     INTEGER NOT NULL DEFAULT 0,
            pause_reason               TEXT,
            CHECK (critical_reserve > protected_minimum)
        );

        -- ------------------------------------------------------- reconciliation_findings --
        CREATE TABLE reconciliation_findings (
            id              INTEGER PRIMARY KEY,
            detected_at     INTEGER NOT NULL,
            direction       TEXT NOT NULL,
            expected        INTEGER NOT NULL,
            observed        INTEGER NOT NULL,
            delta           INTEGER NOT NULL,
            classification  TEXT NOT NULL,
            auto_paused     INTEGER NOT NULL DEFAULT 0,
            resolved_at     INTEGER,
            resolution_note TEXT
        );
        "#,
    )?;
    Ok(())
}
