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

const CURRENT_SCHEMA_VERSION: i64 = 19;

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

    // FORWARD-COMPATIBILITY GUARD: refuse a database written by a NEWER
    // binary than this one, instead of silently rewriting its version
    // marker downward.
    //
    // Without this, the `UPDATE schema_version SET version = ?1` at the
    // end of the migration ladder below unconditionally stamps THIS
    // binary's version onto whatever it opened — so rolling back to an
    // older binary would quietly relabel a newer database as older,
    // while that database still physically carries the newer structures
    // and rows. The tables themselves survive (every migration is
    // structurally idempotent, so rolling forward re-applies cleanly and
    // loses nothing), but the version marker would have been a lie in
    // the meantime, and any operator or audit reading it would be
    // misled. Fail loudly and refuse to touch the database at all
    // instead — a rollback that needs an older binary must be a
    // deliberate, evidenced decision (restore a pre-upgrade backup via
    // `scripts/restore-ledger.sh`), never an accident this code paves
    // over. See docs/09-runbook.md "Schema rollback".
    if let Some(current) = current {
        if current > CURRENT_SCHEMA_VERSION {
            return Err(LedgerError::SchemaTooNew {
                found: current,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
    }

    if current.is_none() {
        apply_v1(conn)?;
        apply_v2(conn)?;
        apply_v3(conn)?;
        apply_v4(conn)?;
        apply_v5(conn)?;
        apply_v6(conn)?;
        apply_v7(conn)?;
        apply_v8(conn)?;
        apply_v9(conn)?;
        apply_v10(conn)?;
        apply_v11(conn)?;
        apply_v12(conn)?;
        apply_v13(conn)?;
        apply_v14(conn)?;
        apply_v15(conn)?;
        apply_v16(conn)?;
        apply_v17(conn)?;
        apply_v18(conn)?;
        apply_v19(conn)?;
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
        if current < Some(6) {
            apply_v6(conn)?;
        }
        if current < Some(7) {
            apply_v7(conn)?;
        }
        if current < Some(8) {
            apply_v8(conn)?;
        }
        if current < Some(9) {
            apply_v9(conn)?;
        }
        if current < Some(10) {
            apply_v10(conn)?;
        }
        if current < Some(11) {
            apply_v11(conn)?;
        }
        if current < Some(12) {
            apply_v12(conn)?;
        }
        if current < Some(13) {
            apply_v13(conn)?;
        }
        if current < Some(14) {
            apply_v14(conn)?;
        }
        if current < Some(15) {
            apply_v15(conn)?;
        }
        if current < Some(16) {
            apply_v16(conn)?;
        }
        if current < Some(17) {
            apply_v17(conn)?;
        }
        if current < Some(18) {
            apply_v18(conn)?;
        }
        if current < Some(19) {
            apply_v19(conn)?;
        }
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION],
        )?;
    }

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

/// Phase 6 (bridge fee): the 3% bridge fee and the reserve-capacity
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
///   - `fee_bps`: the fee-POLICY SNAPSHOT actually applied to this
///     request (`amount_conversion::BRIDGE_FEE_BPS` at creation/fold
///     time; immutable thereafter). Read back for settlement/attestation
///     validation via `amount_conversion::verify_fee_breakdown`, which
///     recomputes fee/net AT THIS RATE and refuses on mismatch — and
///     refuses outright any rate outside
///     `amount_conversion::HISTORICAL_FEE_BPS` — so in-flight requests
///     survive a rate change without weakening fail-closed validation
///     (see docs/20-bridge-fee.md).
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

/// Rebalancing engineering layer (docs/22-production-readiness-review.md
/// P1 "rebalancing", docs/05-reserve-accounting.md's original
/// `rebalance_events` design). Structurally separate from
/// `bridge_requests`/settlement accounting by construction — no foreign
/// key to `bridge_requests`, and nothing in `Ledger::confirm_rebalance`
/// touches `reserved_liquidity`/`pending_obligations`, only
/// `total_reserve_balance` — so a reconciliation job or an auditor
/// scanning settlement records can never mistake a rebalance for a user
/// bridge transfer, matching docs/05's original design intent.
///
/// `tx_reference` is the real, external evidence of an out-of-band
/// transfer (a Goldcoin txid or a Solana signature, as plain text) an
/// operator already authorized and executed through real custody tooling
/// — this service never constructs or broadcasts that transaction itself
/// (docs/22-production-readiness-review.md: "model it as an explicit
/// externally authorized action/request"). The `UNIQUE` index on it is
/// the structural replay guard: the same real transfer can never be
/// recorded against two different rebalance requests.
fn apply_v6(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE rebalance_requests (
            id                      INTEGER PRIMARY KEY,
            direction               TEXT NOT NULL CHECK (direction IN ('GoldcoinReserve','SolanaReserve')),
            kind                    TEXT NOT NULL CHECK (kind IN ('Deposit','Withdraw')),
            amount_atomic           INTEGER NOT NULL CHECK (amount_atomic > 0),
            state                   TEXT NOT NULL,
            reason                  TEXT NOT NULL,
            requested_by            TEXT NOT NULL,
            requested_at            INTEGER NOT NULL,
            required_approvals      INTEGER NOT NULL CHECK (required_approvals > 0),
            approved_by             TEXT NOT NULL DEFAULT '[]',
            approved_at             INTEGER,
            tx_reference            TEXT,
            executed_at             INTEGER,
            observed_amount_atomic  INTEGER,
            confirmed_at            INTEGER,
            failure_reason          TEXT
        );
        CREATE UNIQUE INDEX ux_rebalance_tx_reference
            ON rebalance_requests(tx_reference)
            WHERE tx_reference IS NOT NULL;
        CREATE INDEX ix_rebalance_requests_state ON rebalance_requests(direction, state);

        -- Append-only audit trail, same discipline as bridge_request_state_log.
        CREATE TABLE rebalance_state_log (
            id             INTEGER PRIMARY KEY,
            rebalance_id   INTEGER NOT NULL REFERENCES rebalance_requests(id),
            from_state     TEXT,
            to_state       TEXT NOT NULL,
            at             INTEGER NOT NULL,
            reason         TEXT,
            actor          TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Dedicated post-finality Goldcoin reorg detection
/// (docs/22-production-readiness-review.md P1, docs/10-threat-model.md's
/// "post-finality reorg" section — previously only incidentally caught,
/// if at all, by the generic reconciliation balance-drop check). Distinct
/// from `goldcoin_reorg_events` (every reorg, routine pre-finality
/// rollbacks included): a row here exists only when a detected reorg's
/// fork point is at or below the source block of at least one
/// `GlcToSol` request that had already been told its deposit was final
/// (`bridge_requests.source_finalized_at IS NOT NULL`) — the exact
/// "previously accepted finalized observation invalidated" event the
/// threat model names as a genuine incident, never routine.
fn apply_v7(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE post_finality_reorg_events (
            id                     INTEGER PRIMARY KEY,
            detected_at            INTEGER NOT NULL,
            fork_height            INTEGER NOT NULL,
            old_tip_height         INTEGER NOT NULL,
            affected_request_ids   TEXT NOT NULL,
            auto_paused            INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )?;
    Ok(())
}

/// Generic key-rotation / vault-sweep custody-transition tooling
/// (docs/22-production-readiness-review.md P1 "key rotation / vault
/// sweep tooling", docs/09-runbook.md's "no procedure exists yet"
/// gap). Covers both `AttestationKeyRotation` (ed25519 signer set) and
/// `GoldcoinVaultSweep` (P2SH multisig vault) with one shared shape,
/// since both are fundamentally "retire an old custody identity, adopt a
/// verified new one" with the same safety requirements. Like
/// `rebalance_requests`, this NEVER records that this service itself
/// generated keys, signed anything, or broadcast a real transaction —
/// `record_custody_transition_executed` only ever records evidence
/// (`tx_reference`) of a real rotation/sweep an operator already
/// authorized and executed through real custody tooling outside this
/// system.
fn apply_v8(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE custody_transitions (
            id                      INTEGER PRIMARY KEY,
            kind                    TEXT NOT NULL CHECK (kind IN ('AttestationKeyRotation','GoldcoinVaultSweep')),
            state                   TEXT NOT NULL,
            old_identities          TEXT NOT NULL,
            new_identities          TEXT NOT NULL,
            new_threshold           INTEGER,
            reason                  TEXT NOT NULL,
            requested_by            TEXT NOT NULL,
            requested_at            INTEGER NOT NULL,
            required_approvals      INTEGER NOT NULL CHECK (required_approvals > 0),
            approved_by             TEXT NOT NULL DEFAULT '[]',
            approved_at             INTEGER,
            identity_verified_by    TEXT,
            identity_verified_at    INTEGER,
            tx_reference            TEXT,
            executed_at             INTEGER,
            confirmed_at            INTEGER,
            failure_reason          TEXT,
            rolled_back_at          INTEGER,
            rollback_reason         TEXT
        );
        CREATE UNIQUE INDEX ux_custody_transitions_tx_reference
            ON custody_transitions(tx_reference)
            WHERE tx_reference IS NOT NULL;
        CREATE INDEX ix_custody_transitions_state ON custody_transitions(kind, state);

        -- Append-only audit trail, same discipline as the other two
        -- state-machine tables above.
        CREATE TABLE custody_transition_state_log (
            id                      INTEGER PRIMARY KEY,
            transition_id           INTEGER NOT NULL REFERENCES custody_transitions(id),
            from_state              TEXT,
            to_state                TEXT NOT NULL,
            at                      INTEGER NOT NULL,
            reason                  TEXT,
            actor                   TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Unique-per-request Goldcoin deposit address (docs: the OP_RETURN-
/// replacement redesign, Step 2 of a staged rollout — Step 1 was the
/// pure derivation helper, `goldcoin::derivation`; this step is ONLY
/// schema/ledger support — no indexer, API, payout, or signer code
/// reads or writes these columns yet).
///
/// `bridge_requests.id` is reused directly as the derivation index (see
/// `goldcoin::derivation`'s own docs) — no separate index/counter
/// column is added here. All three new columns are nullable: `NULL`
/// means "this request has no per-request deposit address assigned"
/// (every existing row, and every future `SolToGlc` row, which has no
/// Goldcoin deposit step at all — direction is enforced by
/// `Ledger::set_glc_to_sol_deposit_address`, not by a schema CHECK,
/// since a request's direction can't be joined into a column
/// constraint here).
///
/// `deposit_script_pubkey_hex` (the actual on-chain P2SH scriptPubKey a
/// future indexer will match transaction outputs against) is the real
/// lookup key — the partial unique index below is the DATABASE-level
/// guarantee that two different requests can never be assigned the same
/// deposit script, structurally impossible to race past (same pattern
/// already used for `ux_bridge_requests_glc_source`/
/// `ux_custody_transitions_tx_reference` above).
/// Column-level idempotent: every `ADD COLUMN` is skipped if the column is
/// already present, and the index uses `IF NOT EXISTS`. This is deliberately
/// NOT relying solely on `schema_version`-based gating in
/// [`open_and_migrate`] to keep this migration from ever running twice — a
/// production database was found with these exact columns already present
/// (a prior rollout of this same migration) while its recorded
/// `schema_version` did not reflect it, and the un-guarded `ALTER TABLE ADD
/// COLUMN` then failed outright with `duplicate column name`, refusing to
/// start. Structural idempotency here means this function is safe to
/// invoke any number of times, regardless of what `schema_version` says —
/// it converges to the same end state either way, never errors, and never
/// drops or recreates a column that's already there.
fn apply_v9(conn: &Connection) -> Result<(), LedgerError> {
    for column in [
        "deposit_address",
        "deposit_script_pubkey_hex",
        "deposit_redeem_script_hex",
    ] {
        if !column_exists(conn, "bridge_requests", column)? {
            conn.execute(
                &format!("ALTER TABLE bridge_requests ADD COLUMN {column} TEXT"),
                [],
            )?;
        }
    }
    conn.execute_batch(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS ux_bridge_requests_deposit_script
            ON bridge_requests(deposit_script_pubkey_hex)
            WHERE deposit_script_pubkey_hex IS NOT NULL;
        "#,
    )?;
    Ok(())
}

/// Operator-triggered vault UTXO splitting audit trail
/// (`glc-admin split-vault-utxo`, docs/09-runbook.md's "Vault UTXO
/// splitting" section) — a proactive, root-vault-only counterpart to the
/// oversized-UTXO-avoidance fix in `goldcoin::coin::select`: fragments one
/// large mature vault UTXO into several smaller ones, all still owned by
/// the vault, ahead of a future payout ever needing to touch it.
///
/// `UNIQUE(source_txid, source_vout)` is the same "actual boundary against
/// double-processing the same input" pattern `goldcoin_payout_inputs`
/// already uses — a given vault outpoint can be split at most once, ever,
/// structurally, not just by an application-level check. A brand-new
/// table, so `CREATE TABLE IF NOT EXISTS` is naturally idempotent on its
/// own (unlike v9's `ALTER TABLE ADD COLUMN` case, which needed the
/// explicit `column_exists` guard above after a real production
/// `schema_version`/actual-schema desync) — still written defensively with
/// `IF NOT EXISTS` throughout so a repeat invocation, from any state, is
/// always a safe no-op.
fn apply_v10(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS vault_utxo_splits (
            id                    INTEGER PRIMARY KEY,
            source_txid           BLOB NOT NULL,
            source_vout           INTEGER NOT NULL,
            source_amount_atomic  INTEGER NOT NULL,
            chunk_count           INTEGER NOT NULL,
            chunk_target_atomic   INTEGER NOT NULL,
            fee_atomic            INTEGER NOT NULL,
            unsigned_tx_hex       TEXT NOT NULL,
            signed_tx_hex         TEXT,
            txid                  BLOB,
            state                 TEXT NOT NULL CHECK (state IN ('Built','Signed','Broadcast')),
            note                  TEXT NOT NULL,
            built_at              INTEGER NOT NULL,
            signed_at             INTEGER,
            broadcast_at          INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS ux_vault_utxo_splits_source
            ON vault_utxo_splits(source_txid, source_vout);
        "#,
    )?;
    Ok(())
}

/// Minimal, additive admission-control gate (a separate axis from the
/// existing `reserve_ledger.paused`/`pause_reason` — see
/// `Ledger::set_admission`/`is_admission_closed` and `docs/09-runbook.md`'s
/// "Admission control (Solana->Goldcoin)" section): whether NEW obligations
/// may be admitted, independent of whether payout processing of
/// already-accepted ones continues (which was, and remains, never gated by
/// either flag). `admission_closed` starts `0` (open) on every existing
/// and new row — nothing automatic ever sets it; only the operator, via
/// `glc-admin close-admission`/`open-admission`. Column-level idempotent,
/// same discipline as `apply_v9`.
fn apply_v11(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "reserve_ledger", "admission_closed")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_closed INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "admission_reason")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_reason TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Deterministic Goldcoin payout change FAN-OUT (docs/09-runbook.md's
/// "UTXO liquidity" section): a payout's change is now zero or more
/// outputs, not one lump — see `goldcoin::coin::finalize_fanout` and
/// `PayoutPlan::change_outputs`. Purely ADDITIVE: `goldcoin_payouts.
/// change_atomic` is untouched (kept as the SUM of every change output,
/// for full backward compatibility with every existing consumer of that
/// column — `Ledger::pending_destination_settlement_amount`'s existing
/// SQL needs no change at all). A row existing before this migration
/// simply has zero rows in the new table for its `request_id`; every read
/// path treats that as "one legacy change output, equal to the persisted
/// `change_atomic`" (see `Ledger::get_goldcoin_payout_full`) — never
/// backfilled, never assumed to need repair.
fn apply_v12(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS goldcoin_payout_change_outputs (
            request_id    INTEGER NOT NULL REFERENCES bridge_requests(id),
            output_order  INTEGER NOT NULL,
            amount_atomic INTEGER NOT NULL,
            PRIMARY KEY (request_id, output_order)
        );
        "#,
    )?;
    // UTXO-liquidity admission backpressure (`Ledger::fold_sol_deposit`,
    // `Ledger::set_utxo_pool_thresholds`): defaults to `0` on every existing
    // and new row, meaning "no backpressure" until an operator/startup
    // config explicitly configures it — column-level idempotent, same
    // discipline as `apply_v9`/`apply_v11`.
    if !column_exists(conn, "reserve_ledger", "utxo_pool_min_available_count")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN utxo_pool_min_available_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "utxo_pool_warning_count")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN utxo_pool_warning_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Purely an index — no data or column change. Supports the SolToGlc
/// per-recipient 24h rate-limit check (`Ledger::fold_sol_deposit`/
/// `Ledger::resume_manual_review_sol_to_glc`), which queries
/// `bridge_requests` by `(direction, recipient, created_at)` on every
/// SolToGlc fold and every resume attempt — a hot path with no existing
/// supporting index (`ix_bridge_requests_state` covers `(direction,
/// state)` only). `IF NOT EXISTS` for the same idempotent-migration
/// discipline as every other `apply_v*` here.
fn apply_v13(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS ix_bridge_requests_recipient_window
            ON bridge_requests(direction, recipient, created_at);
        "#,
    )?;
    Ok(())
}

/// v14 — 0-conf spendability for bridge-created payout change
/// (docs/09-runbook.md "Zero-conf payout change"):
///
/// `goldcoin_payout_change_outpoints` is the AUTHORITATIVE provenance
/// relation for the policy: one row per change output of a payout this
/// service itself broadcast, written in the same ledger transaction that
/// records the broadcast txid (`Ledger::record_goldcoin_payout_broadcast`
/// — change outputs are `outputs[1..]` of the payout transaction, in
/// `goldcoin_payout_change_outputs` order; the destination is always
/// output 0 and never gets a row here, so a payout whose DESTINATION
/// happens to pay a watched vault/deposit script can never be
/// misclassified as change). A vault UTXO qualifies for 0-conf spending
/// ONLY by joining this table on its exact `(txid, vout)` — never by
/// paying a vault script or appearing in a vault-touching transaction.
/// Rows are additive and survive restart; outputs broadcast BEFORE this
/// migration have no row and therefore stay on the external
/// (`vault_min_confirmations`) policy — fail closed, no backfill.
///
/// `unconfirmed_ancestor_depth` is the count of this service's OWN
/// unconfirmed ancestor payout transactions at broadcast time (1 = the
/// parent payout itself was built purely on confirmed inputs; 2 = it
/// spent depth-1 zero-conf change; ...) — an upper bound used to cap
/// unconfirmed chaining (`PayoutPolicy::zero_conf_change_max_depth`).
///
/// `vault_utxos.zero_conf_hold_reason` is a reversible per-output
/// exclusion the orchestrator sets when the parent payout transaction
/// stops being known/accepted by the configured Goldcoin node
/// (`Orchestrator::tick_validate_zero_conf_parents`) and clears when it
/// is accepted again — 0-conf eligibility requires it NULL. Vault-split
/// outputs never appear in the outpoints table (splits are recorded in
/// `vault_utxo_splits`, a different relation) and so never receive the
/// 0-conf policy.
fn apply_v14(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS goldcoin_payout_change_outpoints (
            txid                       BLOB NOT NULL,
            vout                       INTEGER NOT NULL,
            request_id                 INTEGER NOT NULL REFERENCES bridge_requests(id),
            amount_atomic              INTEGER NOT NULL,
            unconfirmed_ancestor_depth INTEGER NOT NULL,
            PRIMARY KEY (txid, vout)
        );
        "#,
    )?;
    if !column_exists(conn, "vault_utxos", "zero_conf_hold_reason")? {
        conn.execute(
            "ALTER TABLE vault_utxos ADD COLUMN zero_conf_hold_reason TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Append-only audit trail for privileged admin operations
/// (`Ledger::append_admin_audit`/`list_admin_audit`). The three existing
/// per-state-machine logs (`bridge_request_state_log`,
/// `rebalance_state_log`, `custody_transition_state_log`) only capture
/// operations that transition one of those machines; admin operations
/// that don't (pause/unpause, admission open/close) previously left their
/// mandatory `--note` in a last-write-wins `reserve_ledger` field, or
/// nowhere at all. This table records every admin mutation ATTEMPT —
/// failures included (`outcome = 'error'`), because "an operator tried
/// and was refused" is itself audit-relevant. `note` is `CHECK`ed
/// non-empty at the schema level, mirroring `glc-admin`'s own
/// `require_note` discipline, so a caller that forgets to enforce it
/// cannot write a noteless row.
fn apply_v15(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS admin_audit_log (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            at        INTEGER NOT NULL,
            actor     TEXT    NOT NULL CHECK (actor <> ''),
            action    TEXT    NOT NULL CHECK (action <> ''),
            target    TEXT,
            old_value TEXT,
            new_value TEXT,
            note      TEXT    NOT NULL CHECK (note <> ''),
            outcome   TEXT    NOT NULL CHECK (outcome IN ('success', 'error')),
            error     TEXT
        );
        CREATE INDEX IF NOT EXISTS ix_admin_audit_log_at
            ON admin_audit_log(at);
        CREATE INDEX IF NOT EXISTS ix_admin_audit_log_action
            ON admin_audit_log(action, id);
        CREATE INDEX IF NOT EXISTS ix_admin_audit_log_actor
            ON admin_audit_log(actor, id);
        "#,
    )?;

    Ok(())
}

/// v16 — the vault-UTXO-split LIFECYCLE (docs/09-runbook.md "Automatic
/// UTXO liquidity shaping"): `vault_utxo_splits` gains two terminal
/// states beyond `Broadcast` — `Confirmed` (the split transaction has at
/// least one confirmation; nothing left to drive) and `Abandoned` (the
/// split can never take effect: its source became unspendable before
/// broadcast, or the node reported the already-broadcast transaction's
/// inputs missing on a re-broadcast attempt) — plus the timestamps/reason
/// recording those transitions. The source-outpoint uniqueness guarantee
/// becomes a PARTIAL unique index excluding `Abandoned` rows: an
/// abandoned attempt keeps its full audit row forever but no longer
/// blocks a later, legitimate split of the same outpoint (the exact wedge
/// the 2026-08-30 review found: one dead row permanently disabled all
/// automatic shaping).
///
/// SQLite cannot ALTER a CHECK constraint, so this is the standard
/// rebuild-and-rename dance. `vault_utxo_splits` holds a handful of rows
/// (one per historical split), so the copy is trivially cheap. Idempotent
/// via the `abandon_reason` column probe, same discipline as v9's
/// `column_exists` guard.
fn apply_v16(conn: &Connection) -> Result<(), LedgerError> {
    if column_exists(conn, "vault_utxo_splits", "missing_inputs_since")? {
        return Ok(());
    }
    // The rebuild MUST be one atomic transaction (2026-08-30 re-review,
    // finding 3): a process killed between CREATE/DROP/RENAME would
    // otherwise leave a hybrid state the idempotency probe above cannot
    // recover — and, past the DROP, would have destroyed split history.
    // SQLite rolls an uncommitted transaction back on the next open, so a
    // kill at any point leaves the original table untouched and this
    // function simply runs again. The DROP IF EXISTS guards the one
    // remaining sliver: a leftover empty _v16 table from a pre-fix binary.
    conn.execute_batch(
        r#"
        BEGIN IMMEDIATE;
        DROP TABLE IF EXISTS vault_utxo_splits_v16;
        CREATE TABLE vault_utxo_splits_v16 (
            id                    INTEGER PRIMARY KEY,
            source_txid           BLOB NOT NULL,
            source_vout           INTEGER NOT NULL,
            source_amount_atomic  INTEGER NOT NULL,
            chunk_count           INTEGER NOT NULL,
            chunk_target_atomic   INTEGER NOT NULL,
            fee_atomic            INTEGER NOT NULL,
            unsigned_tx_hex       TEXT NOT NULL,
            signed_tx_hex         TEXT,
            txid                  BLOB,
            state                 TEXT NOT NULL CHECK (state IN ('Built','Signed','Broadcast','Confirmed','Abandoned')),
            note                  TEXT NOT NULL,
            built_at              INTEGER NOT NULL,
            signed_at             INTEGER,
            broadcast_at          INTEGER,
            confirmed_at          INTEGER,
            abandoned_at          INTEGER,
            abandon_reason        TEXT,
            -- Set the first time a re-broadcast of this split's exact
            -- bytes is refused for missing inputs; cleared when the node
            -- accepts/knows the transaction again. After a grace window
            -- (goldcoin::liquidity), accounting stops explaining the
            -- split's phantom chunks so a genuine conflicting-spend loss
            -- surfaces as the breach it is instead of being silently
            -- padded over (2026-08-31 production-readiness review, B2).
            missing_inputs_since  INTEGER,
            -- The transition facts must travel with their states.
            CHECK (state != 'Abandoned' OR abandon_reason IS NOT NULL)
        );
        INSERT INTO vault_utxo_splits_v16
            (id, source_txid, source_vout, source_amount_atomic, chunk_count,
             chunk_target_atomic, fee_atomic, unsigned_tx_hex, signed_tx_hex,
             txid, state, note, built_at, signed_at, broadcast_at)
        SELECT id, source_txid, source_vout, source_amount_atomic, chunk_count,
               chunk_target_atomic, fee_atomic, unsigned_tx_hex, signed_tx_hex,
               txid, state, note, built_at, signed_at, broadcast_at
        FROM vault_utxo_splits;
        DROP TABLE vault_utxo_splits;
        ALTER TABLE vault_utxo_splits_v16 RENAME TO vault_utxo_splits;
        CREATE UNIQUE INDEX ux_vault_utxo_splits_source
            ON vault_utxo_splits(source_txid, source_vout)
            WHERE state != 'Abandoned';
        -- The lifecycle queries filter by state (pending/broadcast sets)
        -- and match chunk rows by txid every tick.
        CREATE INDEX ix_vault_utxo_splits_state ON vault_utxo_splits(state);
        CREATE INDEX ix_vault_utxo_splits_txid ON vault_utxo_splits(txid);
        COMMIT;
        "#,
    )?;
    Ok(())
}

/// v17 — `solana_refunds`: the audited, structurally-idempotent record of
/// a ManualReview refund lifecycle (docs/09-runbook.md "ManualReview
/// refunds (Solana->Goldcoin)"). One row per refunded request, ever:
///
/// - `request_id INTEGER PRIMARY KEY` — at most one refund lifecycle per
///   bridge request, the same structural boundary `goldcoin_payouts`'
///   PRIMARY KEY provides against double-pay.
/// - `nonce` UNIQUE — the `rebalance_withdraw` nonce, derived
///   deterministically from the request id in a dedicated refund domain
///   (`Ledger::solana_refund_nonce`); its on-chain `rebalance_withdrawal`
///   PDA makes a second transfer under it impossible on chain, so a
///   restored-from-backup database still cannot double-refund.
/// - `obligation_index` UNIQUE — one refund per on-chain deposit
///   obligation, mirroring `ux_bridge_requests_sol_source`.
///
/// The refund never overwrites any original request/deposit evidence: the
/// park reason is COPIED here (`manual_review_reason`) and the request
/// row's own `manual_review_note`/source columns stay untouched.
/// `CREATE TABLE IF NOT EXISTS` keeps this structurally idempotent (the
/// v9/v16 discipline — never rely on the version gate alone).
fn apply_v17(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS solana_refunds (
            request_id                INTEGER PRIMARY KEY REFERENCES bridge_requests(id),
            obligation_index          INTEGER NOT NULL UNIQUE,
            nonce                     INTEGER NOT NULL UNIQUE,
            amount_solana_atomic      INTEGER NOT NULL CHECK (amount_solana_atomic > 0),
            requester                 BLOB NOT NULL,
            destination_token_account BLOB NOT NULL,
            reserve_mint              BLOB NOT NULL,
            token_program             BLOB NOT NULL,
            manual_review_reason      TEXT NOT NULL,
            note                      TEXT NOT NULL CHECK (note <> ''),
            created_by                TEXT NOT NULL CHECK (created_by <> ''),
            state                     TEXT NOT NULL CHECK (state IN ('Pending','Broadcast','Confirmed')),
            attestation_epoch         INTEGER,
            refund_signature          TEXT,
            -- The broadcast transaction's recent blockhash (base58). What
            -- makes crash recovery POSITIVE rather than heuristic: a
            -- rerun may only rebuild (with the SAME nonce) after
            -- observing this blockhash can no longer land AND the nonce
            -- PDA does not exist — never "it has probably expired".
            recent_blockhash          TEXT,
            created_at                INTEGER NOT NULL,
            broadcast_at              INTEGER,
            confirmed_at              INTEGER,
            -- A signature/blockhash/broadcast timestamp may only exist
            -- once the row has actually reached the state that produces
            -- it.
            CHECK (state = 'Pending' OR refund_signature IS NOT NULL),
            CHECK (state = 'Pending' OR recent_blockhash IS NOT NULL),
            CHECK (state = 'Pending' OR broadcast_at IS NOT NULL),
            CHECK (state != 'Confirmed' OR confirmed_at IS NOT NULL)
        );
        CREATE INDEX IF NOT EXISTS ix_solana_refunds_state ON solana_refunds(state);
        "#,
    )?;
    Ok(())
}

/// Confirmed-liquidity admission safety buffer for Solana->Goldcoin
/// (docs/09-runbook.md's "Confirmed-liquidity admission safety buffer"
/// section): a second, AUTOMATIC admission axis that closes SolToGlc
/// admission before confirmed unreserved Goldcoin headroom reaches the
/// hard `protected_minimum`, and reopens it only once headroom has
/// recovered to a strictly higher mark.
///
/// Four columns, all defaulting to "disabled / open" on every existing
/// and new row, so a database that never configures the buffer behaves
/// bit-identically to before this migration:
///
/// - `admission_buffer_atomic` — the close threshold. `0` means the whole
///   feature is disabled, the same short-circuit shape
///   `utxo_pool_min_available_count = 0` already uses.
/// - `admission_reopen_atomic` — the (higher) reopen threshold. The gap
///   between the two IS the hysteresis: between them the gate holds its
///   current state, which is what makes threshold flapping structurally
///   impossible rather than merely unlikely.
/// - `liquidity_admission_closed` — the gate's persisted state. Genuinely
///   stateful: at a headroom between the two thresholds the correct
///   answer depends on which side the reserve arrived from, so it cannot
///   be recomputed from the balance alone.
/// - `liquidity_admission_closed_at` — when the gate last transitioned,
///   for operator/audit visibility only; never read by any decision.
///
/// Deliberately SEPARATE from `admission_closed`/`admission_reason`
/// (v11), which remain operator-only by design (`Ledger::set_admission`'s
/// docs, docs/09-runbook.md: "no automatic reopen, and nothing
/// automatically closes it either"). Overloading that flag would destroy
/// an operator's ability to tell "I closed this" apart from "liquidity
/// closed this", and would let an automatic reopen silently undo a
/// deliberate operator closure. Column-level idempotent, same discipline
/// as `apply_v9`/`apply_v11`/`apply_v12`.
/// v19: the Goldcoin-side refund lifecycle for `GlcToSol` requests parked
/// in `ManualReview` (docs/09-runbook.md "GlcToSol ManualReview refunds").
///
/// Deliberately a SEPARATE table from `goldcoin_payouts` rather than a
/// new state on it. A payout and a refund are opposite settlements of the
/// same request and must never be representable at once;
/// `goldcoin_payouts` is keyed `request_id PRIMARY KEY`, so reusing it
/// would have made "has a payout" and "has a refund" the same query and
/// lost exactly the distinction the safety checks depend on.
///
/// Three structural guarantees, so a regression in application-level
/// checks still cannot produce a double refund:
///
/// 1. `request_id INTEGER PRIMARY KEY` — at most one refund row per
///    request, ever.
/// 2. `UNIQUE (source_txid, source_vout)` — the same deposit outpoint can
///    never be refunded through two different requests, even if the
///    request-level replay guard were somehow bypassed.
/// 3. `goldcoin_refund_inputs UNIQUE (txid, vout)` — the same vault UTXO
///    can never fund two refunds, mirroring `goldcoin_payout_inputs`,
///    which docs/01-reuse-inventory.md names as the actual double-spend
///    boundary.
///
/// `CREATE TABLE IF NOT EXISTS` keeps this structurally idempotent.
fn apply_v19(conn: &Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS goldcoin_refunds (
            request_id             INTEGER PRIMARY KEY REFERENCES bridge_requests(id),

            -- The deposit being returned, as independently re-verified
            -- against Goldcoin RPC. NOT copied from the request row
            -- alone: the executing path re-derives these and refuses on
            -- any disagreement.
            source_txid            BLOB NOT NULL,
            source_vout            INTEGER NOT NULL,

            -- The amount ACTUALLY observed on chain in that output. This
            -- is the refund principal. It is never `bridge_requests.
            -- amount_atomic` (the expected gross) and never parsed out of
            -- `manual_review_note`.
            observed_amount_atomic INTEGER NOT NULL CHECK (observed_amount_atomic > 0),

            -- The outpoint the deposit transaction SPENT, and the P2PKH
            -- destination derived from that prevout's own scriptPubKey.
            -- Recorded for audit: it is the whole evidence chain for why
            -- the refund went where it went.
            source_input_txid      BLOB NOT NULL,
            source_input_vout      INTEGER NOT NULL,
            refund_dest_p2pkh_hash BLOB NOT NULL,
            refund_dest_address    TEXT NOT NULL CHECK (refund_dest_address <> ''),

            -- The refund output value. Equal to observed_amount_atomic by
            -- policy (the vault absorbs the miner fee); the CHECK makes
            -- that a schema-level invariant rather than a convention.
            refund_amount_atomic   INTEGER NOT NULL CHECK (refund_amount_atomic > 0),
            fee_atomic             INTEGER NOT NULL CHECK (fee_atomic >= 0),

            unsigned_tx_hex        TEXT,
            signed_tx_hex          TEXT,
            txid                   BLOB,
            confirmations          INTEGER NOT NULL DEFAULT 0,

            state                  TEXT NOT NULL
                                   CHECK (state IN ('Built','Signed','Broadcast','Refunded')),

            manual_review_reason   TEXT NOT NULL,
            note                   TEXT NOT NULL CHECK (note <> ''),
            created_by             TEXT NOT NULL CHECK (created_by <> ''),

            built_at               INTEGER NOT NULL,
            signed_at              INTEGER,
            broadcast_at           INTEGER,
            refunded_at            INTEGER,

            -- Whether the stranded SolanaReserve reservation this request
            -- still held has been released. Guarded by a CHECK so it can
            -- only ever be true in the terminal state: releasing capacity
            -- while a refund might still fail would free liquidity for an
            -- obligation that is not yet actually discharged.
            reservation_released   INTEGER NOT NULL DEFAULT 0
                                   CHECK (reservation_released IN (0, 1)),

            -- ---- table constraints (must follow every column) ----
            --
            -- The recipient receives the FULL observed deposit: the vault
            -- absorbs the miner fee. A schema-level invariant, so it holds
            -- even if the application logic regressed.
            CHECK (refund_amount_atomic = observed_amount_atomic),
            -- Capacity may only be freed in the terminal state.
            CHECK (reservation_released = 0 OR state = 'Refunded'),
            -- Each artifact may exist only once its producing state has
            -- been reached.
            CHECK (state = 'Built' OR signed_tx_hex IS NOT NULL),
            CHECK (state = 'Built' OR signed_at IS NOT NULL),
            CHECK (state IN ('Built','Signed') OR txid IS NOT NULL),
            CHECK (state IN ('Built','Signed') OR broadcast_at IS NOT NULL),
            CHECK (state != 'Refunded' OR refunded_at IS NOT NULL),

            UNIQUE (source_txid, source_vout)
        );

        CREATE TABLE IF NOT EXISTS goldcoin_refund_inputs (
            request_id    INTEGER NOT NULL REFERENCES goldcoin_refunds(request_id),
            input_order   INTEGER NOT NULL,
            txid          BLOB NOT NULL,
            vout          INTEGER NOT NULL,
            amount_atomic INTEGER NOT NULL,
            PRIMARY KEY (request_id, input_order),
            UNIQUE (txid, vout)
        );

        CREATE INDEX IF NOT EXISTS ix_goldcoin_refunds_state
            ON goldcoin_refunds(state);
        "#,
    )?;
    Ok(())
}

fn apply_v18(conn: &Connection) -> Result<(), LedgerError> {
    if !column_exists(conn, "reserve_ledger", "admission_buffer_atomic")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_buffer_atomic INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "admission_reopen_atomic")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN admission_reopen_atomic INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "liquidity_admission_closed")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN liquidity_admission_closed INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "reserve_ledger", "liquidity_admission_closed_at")? {
        conn.execute(
            "ALTER TABLE reserve_ledger ADD COLUMN liquidity_admission_closed_at INTEGER",
            [],
        )?;
    }
    Ok(())
}

/// Whether `table` already has a column named `column` — `PRAGMA
/// table_info` rather than a schema-version check, so it reflects the
/// connection's REAL, current structure regardless of how it got that way
/// (a normal migration run, or an out-of-band/partial one). `pub(super)`:
/// also used by `Ledger::record_unmatched_goldcoin_deposit` to add the
/// `reconciled_at` column to `unmatched_goldcoin_deposits`, a table
/// created ad hoc outside the versioned schema-migration system above.
pub(super) fn column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, LedgerError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column);
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection at exactly schema v8 -- pre-dating the deposit-address
    /// columns -- to prove the v8 -> v9 upgrade path specifically (not
    /// just a fresh install, which every other test in this crate already
    /// exercises implicitly via `Ledger::open`/`open_in_memory`).
    fn conn_at_v8() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        apply_v1(&conn).unwrap();
        apply_v2(&conn).unwrap();
        apply_v3(&conn).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        apply_v7(&conn).unwrap();
        apply_v8(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (8);",
        )
        .unwrap();
        conn
    }

    fn insert_minimal_request(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at)
             VALUES (?1, 'GlcToSol', 'AwaitingDeposit', 12345, X'ab', 1000)",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn fresh_database_reaches_v9_with_deposit_address_columns_present_and_null() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(CURRENT_SCHEMA_VERSION, 19);

        insert_minimal_request(&conn, 1);
        let (addr, script, redeem): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT deposit_address, deposit_script_pubkey_hex, deposit_redeem_script_hex
                 FROM bridge_requests WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(addr.is_none() && script.is_none() && redeem.is_none());
    }

    #[test]
    fn upgrading_from_v8_adds_deposit_address_columns_without_losing_existing_data() {
        let conn = conn_at_v8();
        // Real pre-existing data, inserted BEFORE the v9 migration runs,
        // to prove the ALTER TABLE ADD COLUMN steps never touch it.
        insert_minimal_request(&conn, 1);

        open_and_migrate(&conn).unwrap(); // sees version=8, applies v9..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (gross, recipient, deposit_address): (i64, Vec<u8>, Option<String>) = conn
            .query_row(
                "SELECT gross_amount_atomic, recipient, deposit_address FROM bridge_requests WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            gross, 12345,
            "pre-existing data must survive the migration untouched"
        );
        assert_eq!(recipient, vec![0xab]);
        assert!(
            deposit_address.is_none(),
            "new column defaults to NULL on existing rows"
        );
    }

    #[test]
    fn upgrading_from_v8_is_idempotent_if_run_twice() {
        let conn = conn_at_v8();
        insert_minimal_request(&conn, 1);
        open_and_migrate(&conn).unwrap();
        open_and_migrate(&conn).unwrap(); // must not error re-adding columns/index
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }

    /// Regression for a real production incident: a database already
    /// carried `deposit_address`/`deposit_script_pubkey_hex`/
    /// `deposit_redeem_script_hex` (from an earlier successful rollout of
    /// this exact migration) while its recorded `schema_version` still
    /// read 8 — so `open_and_migrate` decided v9 had not run yet and
    /// re-attempted `ALTER TABLE ... ADD COLUMN`, which failed outright
    /// with `duplicate column name`, and the daemon refused to start. This
    /// builds exactly that mismatched state directly (columns present,
    /// version stuck at 8) rather than relying on `open_and_migrate`
    /// itself to have created it, since the whole point is that some
    /// earlier, different path put the database in this state.
    #[test]
    fn opens_successfully_when_deposit_address_columns_already_exist_but_schema_version_still_reads_8(
    ) {
        let conn = conn_at_v8();
        insert_minimal_request(&conn, 1);
        // Simulates the columns already having been added by an earlier
        // successful run of this migration, WITHOUT going through
        // `open_and_migrate` again here — `schema_version` is deliberately
        // left at 8, reproducing the exact desync production hit.
        conn.execute_batch(
            "ALTER TABLE bridge_requests ADD COLUMN deposit_address TEXT;
             ALTER TABLE bridge_requests ADD COLUMN deposit_script_pubkey_hex TEXT;
             ALTER TABLE bridge_requests ADD COLUMN deposit_redeem_script_hex TEXT;
             CREATE UNIQUE INDEX ux_bridge_requests_deposit_script
                 ON bridge_requests(deposit_script_pubkey_hex)
                 WHERE deposit_script_pubkey_hex IS NOT NULL;
             UPDATE bridge_requests SET deposit_address = 'preexisting' WHERE id = 1;",
        )
        .unwrap();

        open_and_migrate(&conn)
            .expect("must open successfully even though the deposit-address columns already exist");

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // The pre-existing column value must survive untouched — this is
        // not a recreate-the-column fix, just a skip-if-present one.
        let deposit_address: Option<String> = conn
            .query_row(
                "SELECT deposit_address FROM bridge_requests WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deposit_address.as_deref(), Some("preexisting"));

        // A second open (the daemon restarting again) must still be a
        // clean no-op.
        open_and_migrate(&conn).unwrap();
    }

    #[test]
    fn deposit_script_pubkey_unique_index_rejects_a_duplicate_assignment() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        insert_minimal_request(&conn, 1);
        insert_minimal_request(&conn, 2);

        conn.execute(
            "UPDATE bridge_requests SET deposit_script_pubkey_hex = 'abc' WHERE id = 1",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "UPDATE bridge_requests SET deposit_script_pubkey_hex = 'abc' WHERE id = 2",
                [],
            )
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique"),
            "expected a UNIQUE constraint violation from ux_bridge_requests_deposit_script, got: {msg}"
        );
    }

    #[test]
    fn deposit_script_pubkey_null_is_never_constrained_by_the_unique_index() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        // Both left NULL (no deposit address assigned) -- must not collide,
        // since the index is a PARTIAL index (`WHERE ... IS NOT NULL`).
        insert_minimal_request(&conn, 1);
        insert_minimal_request(&conn, 2);
    }

    fn conn_at_v9() -> Connection {
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 9", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v9_creates_the_vault_utxo_splits_table() {
        let conn = conn_at_v9();
        open_and_migrate(&conn).unwrap(); // sees version=9, applies v10..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        conn.execute(
            "INSERT INTO vault_utxo_splits
                (source_txid, source_vout, source_amount_atomic, chunk_count,
                 chunk_target_atomic, fee_atomic, unsigned_tx_hex, state, note, built_at)
             VALUES (X'ab', 0, 1000, 2, 500, 10, 'deadbeef', 'Built', 'test', 1000)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn vault_utxo_splits_source_outpoint_is_unique() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO vault_utxo_splits
                (source_txid, source_vout, source_amount_atomic, chunk_count,
                 chunk_target_atomic, fee_atomic, unsigned_tx_hex, state, note, built_at)
             VALUES (X'ab', 0, 1000, 2, 500, 10, 'deadbeef', 'Built', 'test', 1000)",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO vault_utxo_splits
                    (source_txid, source_vout, source_amount_atomic, chunk_count,
                     chunk_target_atomic, fee_atomic, unsigned_tx_hex, state, note, built_at)
                 VALUES (X'ab', 0, 1000, 2, 500, 10, 'deadbeef', 'Built', 'test again', 2000)",
                [],
            )
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique"),
            "expected a UNIQUE constraint violation from ux_vault_utxo_splits_source, got: {msg}"
        );
    }

    #[test]
    fn applying_v10_twice_is_a_safe_no_op() {
        let conn = conn_at_v9();
        apply_v10(&conn).unwrap();
        apply_v10(&conn).unwrap(); // must not error re-creating the table/index
    }

    fn conn_at_v10() -> Connection {
        let conn = conn_at_v9();
        apply_v10(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 10", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v10_adds_admission_columns_defaulting_open() {
        let conn = conn_at_v10();
        // Real pre-existing reserve_ledger data, inserted BEFORE v11 runs,
        // to prove the ALTER TABLE ADD COLUMN steps never touch it.
        conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve, paused)
             VALUES ('GoldcoinReserve', 100, 0, 0, 100, 50, 10, 1)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=10, applies v11..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (paused, admission_closed, admission_reason): (i64, i64, Option<String>) = conn
            .query_row(
                "SELECT paused, admission_closed, admission_reason FROM reserve_ledger
                 WHERE direction = 'GoldcoinReserve'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            paused, 1,
            "pre-existing paused value must survive the migration untouched"
        );
        assert_eq!(
            admission_closed, 0,
            "the new admission column defaults to open (0) on existing rows, \
             independent of the pre-existing paused value"
        );
        assert!(admission_reason.is_none());
    }

    #[test]
    fn applying_v11_twice_is_a_safe_no_op() {
        let conn = conn_at_v10();
        apply_v11(&conn).unwrap();
        apply_v11(&conn).unwrap(); // must not error re-adding the columns
    }

    fn conn_at_v11() -> Connection {
        let conn = conn_at_v10();
        apply_v11(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 11", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v11_adds_change_fanout_table_and_utxo_pool_columns_without_losing_existing_data(
    ) {
        let conn = conn_at_v11();
        // Real pre-existing reserve_ledger data, inserted BEFORE v12 runs,
        // to prove the ALTER TABLE ADD COLUMN steps never touch it — same
        // discipline as `upgrading_from_v10_adds_admission_columns_defaulting_open`.
        conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve, paused)
             VALUES ('GoldcoinReserve', 100, 0, 0, 100, 50, 10, 1)",
            [],
        )
        .unwrap();
        // A real payout record, inserted BEFORE v12 runs, whose
        // `change_atomic` must remain exactly as persisted — nothing about
        // the pre-existing single-change-amount column is touched by this
        // purely additive migration.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at)
             VALUES (1, 'SolToGlc', 'SettlementAuthorized', 100, X'AA', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (1, X'AB', 90, 9, 1, X'CD', 'Signed', 0)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=11, applies v12..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (paused, min_available, warning): (i64, i64, i64) = conn
            .query_row(
                "SELECT paused, utxo_pool_min_available_count, utxo_pool_warning_count
                 FROM reserve_ledger WHERE direction = 'GoldcoinReserve'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            paused, 1,
            "pre-existing paused value must survive the migration untouched"
        );
        assert_eq!(
            min_available, 0,
            "the new UTXO-pool columns default to 0 (backpressure disabled) on existing rows"
        );
        assert_eq!(warning, 0);

        let change_atomic: i64 = conn
            .query_row(
                "SELECT change_atomic FROM goldcoin_payouts WHERE request_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            change_atomic, 9,
            "a payout built before fan-out existed keeps its single change_atomic value untouched"
        );
        let change_output_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM goldcoin_payout_change_outputs WHERE request_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            change_output_rows, 0,
            "never backfilled — a legacy payout simply has no itemized breakdown rows"
        );

        // A second open (the daemon restarting again) must still be a
        // clean no-op.
        open_and_migrate(&conn).unwrap();
    }

    #[test]
    fn applying_v12_twice_is_a_safe_no_op() {
        let conn = conn_at_v11();
        apply_v12(&conn).unwrap();
        apply_v12(&conn).unwrap(); // must not error re-adding the table/columns
    }

    fn conn_at_v12() -> Connection {
        let conn = conn_at_v11();
        apply_v12(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 12", [])
            .unwrap();
        conn
    }

    #[test]
    fn upgrading_from_v12_adds_the_recipient_window_index_without_losing_existing_data() {
        let conn = conn_at_v12();
        // Real pre-existing data, inserted BEFORE v13 runs, to prove a
        // purely-additive index creation never touches it.
        conn.execute(
            "INSERT INTO bridge_requests
                (id, direction, state, gross_amount_atomic, recipient, created_at)
             VALUES (1, 'SolToGlc', 'SourceFinalized', 100, X'AA', 1000)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=12, applies v13..v14

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let gross: i64 = conn
            .query_row(
                "SELECT gross_amount_atomic FROM bridge_requests WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gross, 100,
            "pre-existing data must survive an index-only migration untouched"
        );

        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'ix_bridge_requests_recipient_window'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);

        // A second open (the daemon restarting again) must still be a
        // clean no-op.
        open_and_migrate(&conn).unwrap();
    }

    #[test]
    fn applying_v13_twice_is_a_safe_no_op() {
        let conn = conn_at_v12();
        apply_v13(&conn).unwrap();
        apply_v13(&conn).unwrap(); // must not error re-creating the index
    }

    #[test]
    fn v15_is_idempotent_and_admin_audit_log_enforces_its_checks() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v15(&conn).unwrap(); // must not error re-creating table/indexes

        // A well-formed row inserts.
        conn.execute(
            "INSERT INTO admin_audit_log (at, actor, action, target, old_value, new_value, note, outcome, error)
             VALUES (1, 'alice', 'pause', 'goldcoin', 'false', 'true', 'incident 42', 'success', NULL)",
            [],
        )
        .unwrap();

        // Empty note, empty actor, and an outcome outside the enum all
        // fail closed at the schema level.
        for bad in [
            "INSERT INTO admin_audit_log (at, actor, action, note, outcome)
             VALUES (1, 'alice', 'pause', '', 'success')",
            "INSERT INTO admin_audit_log (at, actor, action, note, outcome)
             VALUES (1, '', 'pause', 'note', 'success')",
            "INSERT INTO admin_audit_log (at, actor, action, note, outcome)
             VALUES (1, 'alice', 'pause', 'note', 'partial')",
        ] {
            assert!(conn.execute(bad, []).is_err(), "must reject: {bad}");
        }
    }

    #[test]
    fn a_database_newer_than_this_binary_is_refused_not_silently_downgraded() {
        // The rollback scenario: a database written by a FUTURE binary
        // (or, symmetrically, today's v18 database opened by yesterday's
        // v17 binary — same code, same guard). It must refuse, and must
        // NOT rewrite the version marker.
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [CURRENT_SCHEMA_VERSION + 1],
        )
        .unwrap();

        let err = open_and_migrate(&conn).unwrap_err();
        assert!(
            matches!(err, LedgerError::SchemaTooNew { found, supported }
                if found == CURRENT_SCHEMA_VERSION + 1 && supported == CURRENT_SCHEMA_VERSION),
            "got: {err}"
        );
        // The marker is untouched — no silent downgrade.
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION + 1);
    }

    #[test]
    fn upgrading_to_v18_adds_the_admission_buffer_columns_disabled_and_open() {
        let conn = conn_at_v12();
        // Real pre-existing reserve_ledger data, inserted BEFORE v18 runs
        // — same discipline as the v11/v12 migration tests: purely
        // additive ALTER TABLE ADD COLUMN steps must not touch it.
        conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve, paused, admission_closed)
             VALUES ('GoldcoinReserve', 100, 0, 0, 100, 50, 10, 1, 1)",
            [],
        )
        .unwrap();

        open_and_migrate(&conn).unwrap(); // sees version=12, applies v13..v18

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let (paused, admission_closed, buffer, reopen, liquidity_closed, closed_at): (
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT paused, admission_closed, admission_buffer_atomic,
                        admission_reopen_atomic, liquidity_admission_closed,
                        liquidity_admission_closed_at
                 FROM reserve_ledger WHERE direction = 'GoldcoinReserve'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(paused, 1, "pre-existing paused must survive untouched");
        assert_eq!(
            admission_closed, 1,
            "the pre-existing OPERATOR admission flag must survive untouched — the new \
             automatic gate is a separate column and never rewrites it"
        );
        assert_eq!(
            (buffer, reopen),
            (0, 0),
            "the buffer defaults to disabled on every existing row: a binary upgrade must \
             never silently start applying admission backpressure a deployment did not \
             configure"
        );
        assert_eq!(liquidity_closed, 0, "the automatic gate defaults to open");
        assert!(closed_at.is_none());
    }

    #[test]
    fn upgrading_from_v18_adds_the_goldcoin_refund_tables_without_losing_data() {
        // A database at v18 exactly as production has it (the full ladder
        // minus v19), with a real pre-existing request row — so the
        // migration is exercised as a genuine upgrade, not a fresh create.
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        apply_v10(&conn).unwrap();
        apply_v11(&conn).unwrap();
        apply_v12(&conn).unwrap();
        apply_v13(&conn).unwrap();
        apply_v14(&conn).unwrap();
        apply_v15(&conn).unwrap();
        apply_v16(&conn).unwrap();
        apply_v17(&conn).unwrap();
        apply_v18(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 18", [])
            .unwrap();
        insert_minimal_request(&conn, 7);

        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 19);

        // The pre-existing row survived.
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_requests WHERE id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);

        // Both new tables exist and start empty.
        for table in ["goldcoin_refunds", "goldcoin_refund_inputs"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} must exist and be empty after the upgrade");
        }
    }

    #[test]
    fn applying_v19_twice_is_a_safe_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v19(&conn).unwrap(); // must not error re-creating the tables
        apply_v19(&conn).unwrap();
    }

    #[test]
    fn applying_v18_twice_is_a_safe_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v18(&conn).unwrap(); // must not error re-adding the columns
        apply_v18(&conn).unwrap();
    }

    #[test]
    fn v17_is_idempotent_and_solana_refunds_enforces_its_constraints() {
        let conn = Connection::open_in_memory().unwrap();
        open_and_migrate(&conn).unwrap();
        apply_v17(&conn).unwrap(); // must not error re-creating table/index

        insert_minimal_request(&conn, 1);
        conn.execute(
            "INSERT INTO solana_refunds
                (request_id, obligation_index, nonce, amount_solana_atomic, requester,
                 destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (1, 7, 9223372036854775807, 500000, X'aa', X'bb', X'cc', X'dd',
                     'admission_closed_at_fold', 'refund 1', 'cli:test', 'Pending', 1000)",
            [],
        )
        .unwrap();

        // One refund lifecycle per request / per obligation / per nonce,
        // structurally.
        insert_minimal_request(&conn, 2);
        for bad in [
            // duplicate request_id
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (1, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
            // duplicate obligation_index
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 7, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
            // duplicate nonce
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 8, 9223372036854775807, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
            // Broadcast without a signature/blockhash
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at, broadcast_at)
             VALUES (2, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Broadcast', 1, 1)",
            // Confirmed without confirmed_at
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at, refund_signature,
                 recent_blockhash, broadcast_at)
             VALUES (2, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Confirmed', 1, 's', 'h', 1)",
            // empty note
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 8, 100, 1, X'aa', X'bb', X'cc', X'dd', 'r', '', 'a', 'Pending', 1)",
            // zero amount
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                 requester, destination_token_account, reserve_mint, token_program,
                 manual_review_reason, note, created_by, state, created_at)
             VALUES (2, 8, 100, 0, X'aa', X'bb', X'cc', X'dd', 'r', 'n', 'a', 'Pending', 1)",
        ] {
            assert!(conn.execute(bad, []).is_err(), "must reject: {bad}");
        }
    }

    #[test]
    fn upgrading_from_v16_adds_solana_refunds_without_touching_existing_data() {
        // A database at v16 exactly as production would have it (the full
        // ladder minus v17), with a real pre-existing request row.
        let conn = conn_at_v8();
        apply_v9(&conn).unwrap();
        apply_v10(&conn).unwrap();
        apply_v11(&conn).unwrap();
        apply_v12(&conn).unwrap();
        apply_v13(&conn).unwrap();
        apply_v14(&conn).unwrap();
        apply_v15(&conn).unwrap();
        apply_v16(&conn).unwrap();
        conn.execute("UPDATE schema_version SET version = 16", [])
            .unwrap();
        insert_minimal_request(&conn, 41);

        open_and_migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        // Pre-existing data untouched; the new table exists and is empty.
        let amount: i64 = conn
            .query_row(
                "SELECT gross_amount_atomic FROM bridge_requests WHERE id = 41",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(amount, 12345);
        let refunds: i64 = conn
            .query_row("SELECT COUNT(*) FROM solana_refunds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(refunds, 0);
    }
}
