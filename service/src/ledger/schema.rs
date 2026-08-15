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

const CURRENT_SCHEMA_VERSION: i64 = 5;

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
        apply_v2(conn)?;
        apply_v3(conn)?;
        apply_v4(conn)?;
        apply_v5(conn)?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [CURRENT_SCHEMA_VERSION],
        )?;
    } else {
        if current == Some(1) {
            apply_v2(conn)?;
        }
        if current < Some(3) {
            apply_v3(conn)?;
        }
        if current < Some(4) {
            apply_v4(conn)?;
        }
        if current < Some(5) {
            apply_v5(conn)?;
        }
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION],
        )?;
    }
    // Future migrations: `if current < Some(6) { apply_v6(conn)?; }` —
    // forward-only, each step self-contained, matching the old bridge's
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

/// Phase 3: Goldcoin vault UTXO tracking and payout construction/lifecycle.
/// Table/state shapes reused from the old bridge's `vault_utxos`/
/// `withdrawal_payouts`/`withdrawal_payout_inputs` (docs/01-reuse-
/// inventory.md) — reservation lives in this DB, never in `goldcoind`'s own
/// `lockunspent`, because those locks are in-memory only and do not survive
/// a node or service restart (real-node-verified quirk, carried forward).
fn apply_v2(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE vault_utxos (
            txid              BLOB NOT NULL,
            vout              INTEGER NOT NULL,
            amount_atomic     INTEGER NOT NULL,
            script_pubkey_hex TEXT NOT NULL,
            confirmations     INTEGER NOT NULL,
            first_seen_at     INTEGER NOT NULL,
            state             TEXT NOT NULL CHECK (state IN ('Available','Reserved','Spent','Unconfirmed')),
            reserved_by       INTEGER REFERENCES bridge_requests(id),
            reserved_at       INTEGER,
            spent_by_txid     BLOB,
            PRIMARY KEY (txid, vout),
            -- `Reserved` MUST carry a reserved_by; `Spent` legitimately
            -- keeps its `reserved_by` too (audit: which request spent this
            -- outpoint) rather than clearing it, so this only enforces the
            -- direction that matters — not a full biconditional.
            CHECK (state != 'Reserved' OR reserved_by IS NOT NULL)
        );
        CREATE INDEX ix_vault_utxos_state ON vault_utxos(state);

        -- PK on request_id structurally enforces at most one payout ever
        -- built per bridge_request (docs/01-reuse-inventory.md: this and
        -- the UNIQUE below on inputs are "the actual boundary" against
        -- double-pay; everything else is optimization/observability).
        CREATE TABLE goldcoin_payouts (
            request_id            INTEGER PRIMARY KEY REFERENCES bridge_requests(id),
            commitment_hash       BLOB NOT NULL,
            payout_atomic         INTEGER NOT NULL,
            change_atomic         INTEGER NOT NULL,
            fee_atomic            INTEGER NOT NULL,
            dest_p2pkh_hash       BLOB NOT NULL,
            unsigned_tx_hex       TEXT,
            signed_tx_hex         TEXT,
            txid                  BLOB,
            state                 TEXT NOT NULL CHECK (state IN ('Built','Signed','Broadcast','Confirmed','Completed')),
            built_at              INTEGER NOT NULL,
            signed_at             INTEGER,
            broadcast_at          INTEGER,
            confirmations         INTEGER NOT NULL DEFAULT 0,
            completed_at          INTEGER
        );

        CREATE TABLE goldcoin_payout_inputs (
            request_id    INTEGER NOT NULL REFERENCES bridge_requests(id),
            input_order   INTEGER NOT NULL,
            txid          BLOB NOT NULL,
            vout          INTEGER NOT NULL,
            amount_atomic INTEGER NOT NULL,
            UNIQUE (txid, vout)
        );
        "#,
    )?;
    Ok(())
}

/// Phase 4: on-chain completion tracking for the Solana->Goldcoin leg. The
/// Goldcoin payout confirming is not the end of that leg — per
/// docs/03-architecture.md, `record_goldcoin_completion` must land on
/// Solana (threshold-attested) before the obligation is truly `Settled`,
/// so the completion fact is reconstructable from Solana chain state
/// rather than resting solely on this service's own database (same
/// rationale as the old bridge's ADR-0018, reused).
fn apply_v3(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        ALTER TABLE goldcoin_payouts ADD COLUMN mined_height INTEGER;
        ALTER TABLE goldcoin_payouts ADD COLUMN onchain_completion_signature BLOB;
        ALTER TABLE goldcoin_payouts ADD COLUMN onchain_completion_submitted_at INTEGER;
        ALTER TABLE goldcoin_payouts ADD COLUMN onchain_completed_at INTEGER;
        "#,
    )?;
    Ok(())
}

/// Phase 5: frozen attestation-claim artifacts and a signer-identity audit
/// trail (docs/06-schema.md, both specified there since Phase 0/1 but
/// unimplemented until now — `service/ops`/`glc-audit` are what first need
/// them). `attestation_records` persists the exact canonical message bytes
/// an internal signer attested to at the moment it was built, not just the
/// scalar fields it was built from — the same "freeze a copy so a later
/// audit can recompute-and-diff against something, not just re-derive in a
/// vacuum" discipline the old bridge's `StoredClaim`/`StoredPayoutIntent`
/// used (docs/01-reuse-inventory.md, `ops/audit.rs`), adapted here to this
/// bridge's two message families (`shared::claim::release_claim_message`/
/// `goldcoin_completion_message`) instead of its mint-claim format.
/// `signature_grant_log` is the identity-only audit trail from
/// docs/06-schema.md's original design — never key material, just which
/// signer identity granted which category of authorization, when, for
/// which request.
fn apply_v4(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE attestation_records (
            id                 INTEGER PRIMARY KEY,
            request_id         INTEGER NOT NULL REFERENCES bridge_requests(id),
            action_type        TEXT NOT NULL CHECK (action_type IN ('release','completion')),
            canonical_message  BLOB NOT NULL,
            message_hash       BLOB NOT NULL,
            created_at         INTEGER NOT NULL,
            UNIQUE (request_id, action_type)
        );

        CREATE TABLE signature_grant_log (
            id            INTEGER PRIMARY KEY,
            at            INTEGER NOT NULL,
            action_type   TEXT NOT NULL CHECK (action_type IN ('attestation','goldcoin_payout','governance','rebalance')),
            identity      TEXT NOT NULL,
            request_id    INTEGER,
            severity      TEXT NOT NULL CHECK (severity IN ('info','warn'))
        );
        CREATE INDEX ix_signature_grant_log_request ON signature_grant_log(request_id);
        "#,
    )?;
    Ok(())
}

/// Phase 6 (bridge fee): the 1% bridge fee and the reserve-capacity
/// accounting-unit fix it's implemented alongside (docs/20-bridge-fee.md,
/// docs/18-token-2022-support.md's flagged gap). `bridge_requests.
/// amount_atomic` is renamed to `gross_amount_atomic` and three new
/// columns persist the fee breakdown as first-class ledger values, all in
/// the ledger's canonical accounting unit (8 decimals, numerically
/// identical to Goldcoin's own native atomic unit —
/// `amount_conversion::CanonicalAtomic`), for BOTH directions:
///
///   - `gross_amount_atomic` (renamed from `amount_atomic`): what the user
///     declared/deposited, canonical.
///   - `fee_bps`: the rate actually applied to this request (always
///     `amount_conversion::BRIDGE_FEE_BPS` today; persisted per-request,
///     not read back for signing, purely audit/display — see
///     docs/20-bridge-fee.md).
///   - `fee_amount_atomic`, `net_amount_atomic`: canonical; `gross ==
///     fee + net` always holds by construction
///     (`amount_conversion::compute_fee`).
///   - `net_destination_atomic`: the same net entitlement, but in the
///     DESTINATION reserve's own native chain unit — the actual amount
///     reserved/settled against `reserve_ledger`'s capacity counters,
///     since that's what must be compared against a live, native-unit
///     chain balance read. Numerically equal to `net_amount_atomic` for
///     `SolToGlc` (destination is Goldcoin, whose native unit already is
///     canonical); a real, possibly-lossy-checked conversion for
///     `GlcToSol` (destination is the Solana reserve mint's own live
///     decimals).
///
/// `reserve_ledger.reserved_liquidity`/`pending_obligations`/
/// `settled_liquidity_total` switch, alongside this, from tracking GROSS
/// to tracking `net_destination_atomic` — the amount actually committed/
/// released, in that row's own native unit, matching `total_reserve_
/// balance`. `reserve_ledger.accrued_fees_atomic` is new: a running total
/// of fee revenue recognized at settlement, ALWAYS canonical regardless of
/// which row it's on (a deliberate, documented exception — see
/// docs/20-bridge-fee.md's "accrued-fee accounting" section for why it is
/// purely a reporting/audit figure and deliberately never subtracted from
/// `available_capacity`'s arithmetic).
fn apply_v5(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        ALTER TABLE bridge_requests RENAME COLUMN amount_atomic TO gross_amount_atomic;
        ALTER TABLE bridge_requests ADD COLUMN fee_bps INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE bridge_requests ADD COLUMN fee_amount_atomic INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE bridge_requests ADD COLUMN net_amount_atomic INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE bridge_requests ADD COLUMN net_destination_atomic INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE reserve_ledger ADD COLUMN accrued_fees_atomic INTEGER NOT NULL DEFAULT 0;
        "#,
    )?;
    Ok(())
}
