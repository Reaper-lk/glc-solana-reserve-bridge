# Database Schema

Builds directly on the old bridge's SQLite schema (`relayer/src/glc/db.rs`, `withdrawal_db.rs`) per the reuse inventory — tables carried over largely unchanged are marked **(reused)**; new tables have no old-repo analog.

## Chain-tracking (per chain: Goldcoin, Solana)

```sql
-- (reused, per chain instance)
CREATE TABLE indexed_blocks (
  height      INTEGER PRIMARY KEY,
  hash        BLOB NOT NULL UNIQUE,
  prev_hash   BLOB NOT NULL,
  block_time  INTEGER NOT NULL,
  indexed_at  INTEGER NOT NULL
);

-- (reused)
CREATE TABLE chain_state (
  id          INTEGER PRIMARY KEY CHECK (id = 0),
  tip_height  INTEGER NOT NULL,
  tip_hash    BLOB NOT NULL
);

-- (reused)
CREATE TABLE reorg_events (
  id                INTEGER PRIMARY KEY,
  detected_at       INTEGER NOT NULL,
  fork_height       INTEGER NOT NULL,
  old_tip_height    INTEGER NOT NULL,
  old_tip_hash      BLOB NOT NULL,
  new_tip_height    INTEGER NOT NULL,
  new_tip_hash      BLOB NOT NULL,
  orphaned_count    INTEGER NOT NULL
);
```

## Bridge requests (new — the reservation/state-machine ledger; no old-repo analog since the old bridge had no concept of promised-but-unsettled liquidity)

```sql
CREATE TABLE bridge_requests (
  id                    INTEGER PRIMARY KEY,
  direction             TEXT NOT NULL CHECK (direction IN ('GLC_TO_SOL','SOL_TO_GLC')),
  state                 TEXT NOT NULL,
  amount_atomic         INTEGER NOT NULL CHECK (amount_atomic > 0),
  recipient             BLOB NOT NULL,          -- destination-chain address/pubkey
  requester             BLOB,                    -- source-chain identity, if known at creation
  created_at            INTEGER NOT NULL,
  reserved_at           INTEGER,
  reservation_expires_at INTEGER,
  source_txid           BLOB,                    -- set once DepositObserved
  source_vout           INTEGER,                 -- Goldcoin leg only
  source_confirmations  INTEGER NOT NULL DEFAULT 0,
  source_finalized_at   INTEGER,
  settlement_claim_hash BLOB,                     -- canonical claim commitment, set at SettlementAuthorized
  destination_txid      BLOB,
  destination_confirmations INTEGER NOT NULL DEFAULT 0,
  settled_at            INTEGER,
  failure_reason        TEXT,
  manual_review_note    TEXT
);

CREATE UNIQUE INDEX ux_bridge_requests_source
  ON bridge_requests(direction, source_txid, source_vout)
  WHERE source_txid IS NOT NULL;
```

```sql
-- (reused pattern from deposit_state_log / withdrawal_state_log — append-only audit trail)
CREATE TABLE bridge_request_state_log (
  id            INTEGER PRIMARY KEY,
  request_id    INTEGER NOT NULL REFERENCES bridge_requests(id),
  from_state    TEXT,
  to_state      TEXT NOT NULL,
  at            INTEGER NOT NULL,
  reason        TEXT,
  actor         TEXT NOT NULL       -- 'system' | signer identity | operator identity
);
```

## Settlement authorization / replay guard

The Goldcoin→Solana direction's replay guard lives primarily on-chain (`DepositClaim` PDA); this table is a local mirror for reconciliation, not the source of truth for that direction. The Solana→Goldcoin direction's replay guard has **no on-chain backstop**, so this table's UNIQUE constraint *is* the enforcement mechanism for that direction — see [02-trust-model.md](02-trust-model.md) asymmetry note.

```sql
CREATE TABLE settlement_authorizations (
  request_id        INTEGER PRIMARY KEY REFERENCES bridge_requests(id),
  claim_message      BLOB NOT NULL,      -- canonical signed message
  claim_hash         BLOB NOT NULL UNIQUE,
  signer_identities  TEXT NOT NULL,      -- JSON array of attestation/vault-signer identities that attested
  threshold_met_at   INTEGER NOT NULL
);

-- The actual replay-guard enforcement point for SOL_TO_GLC (no on-chain equivalent exists):
CREATE UNIQUE INDEX ux_settlement_source_signature
  ON bridge_requests(source_txid)
  WHERE direction = 'SOL_TO_GLC' AND source_txid IS NOT NULL;
```

## Goldcoin vault (reused near-verbatim from `withdrawal_db.rs`)

```sql
-- (reused)
CREATE TABLE vault_utxos (
  txid              BLOB NOT NULL,
  vout              INTEGER NOT NULL,
  txid_hex          TEXT NOT NULL,
  amount_atomic     INTEGER NOT NULL,
  script_pubkey_hex TEXT NOT NULL,
  confirmations     INTEGER NOT NULL,
  first_seen_at     INTEGER NOT NULL,
  state             TEXT NOT NULL CHECK (state IN ('Available','Reserved','Spent','Unconfirmed')),
  reserved_by       INTEGER,             -- bridge_requests.id
  reserved_at       INTEGER,
  spent_by_txid_hex TEXT,
  PRIMARY KEY (txid, vout)
);

-- (reused)
CREATE TABLE goldcoin_payouts (
  request_id          INTEGER PRIMARY KEY REFERENCES bridge_requests(id),
  commitment_hash     BLOB NOT NULL,
  fee_atomic          INTEGER NOT NULL,
  payout_atomic       INTEGER NOT NULL,
  change_atomic       INTEGER,
  change_address      TEXT,
  unsigned_tx_hex     TEXT,
  signed_tx_hex       TEXT,
  txid_hex            TEXT,
  built_at            INTEGER,
  signed_at           INTEGER,
  broadcast_at        INTEGER,
  mined_block_hash    BLOB,
  mined_height        INTEGER,
  confirmations       INTEGER NOT NULL DEFAULT 0,
  completed_at        INTEGER,
  onchain_completion_signature TEXT,
  vault_script_hash   BLOB,
  signer_indices      TEXT              -- JSON array; internal custody-domain identifiers, not federation members
);

-- (reused)
CREATE TABLE goldcoin_payout_inputs (
  request_id    INTEGER NOT NULL REFERENCES bridge_requests(id),
  input_order   INTEGER NOT NULL,
  txid          BLOB NOT NULL,
  vout          INTEGER NOT NULL,
  amount_atomic INTEGER NOT NULL,
  UNIQUE (txid, vout)   -- structural never-double-spend-an-outpoint guarantee
);
```

Dropped from the old schema (federation-specific, no reserve-model equivalent): `claim_artifacts` (mint-claim message), `withdrawal_quorum_history`, `reconciled_payouts` (multi-operator payout adoption).

## Reserve ledger (new — see [05-reserve-accounting.md](05-reserve-accounting.md))

```sql
CREATE TABLE reserve_ledger (
  direction              TEXT PRIMARY KEY CHECK (direction IN ('GOLDCOIN_RESERVE','SOLANA_RESERVE')),
  total_reserve_balance  INTEGER NOT NULL,   -- cached, refreshed by reconciliation job
  balance_refreshed_at   INTEGER NOT NULL,
  protected_minimum      INTEGER NOT NULL,
  target_reserve         INTEGER NOT NULL,
  warning_reserve        INTEGER NOT NULL,
  critical_reserve       INTEGER NOT NULL,
  reserved_liquidity     INTEGER NOT NULL DEFAULT 0,
  pending_obligations    INTEGER NOT NULL DEFAULT 0,
  settled_liquidity_total INTEGER NOT NULL DEFAULT 0,
  paused                 INTEGER NOT NULL DEFAULT 0,
  pause_reason           TEXT,
  CHECK (critical_reserve > protected_minimum)
);
```

```sql
-- (new) rebalance events — structurally separate from bridge_requests, see 05-reserve-accounting.md
CREATE TABLE rebalance_events (
  id            INTEGER PRIMARY KEY,
  direction     TEXT NOT NULL,
  kind          TEXT NOT NULL CHECK (kind IN ('DEPOSIT','WITHDRAW')),
  amount_atomic INTEGER NOT NULL,
  tx_reference  TEXT NOT NULL,
  operator      TEXT NOT NULL,
  note          TEXT NOT NULL,          -- mandatory, reused audit discipline
  approved_by   TEXT NOT NULL,          -- JSON array of approving custody-domain identities
  executed_at   INTEGER NOT NULL
);
```

## Reconciliation & audit (reused pattern)

```sql
-- (reused pattern from ops/solvency.rs findings)
CREATE TABLE reconciliation_findings (
  id            INTEGER PRIMARY KEY,
  detected_at   INTEGER NOT NULL,
  direction     TEXT NOT NULL,
  expected      INTEGER NOT NULL,
  observed      INTEGER NOT NULL,
  delta         INTEGER NOT NULL,
  classification TEXT NOT NULL,   -- 'WITHIN_TOLERANCE' | 'IN_FLIGHT_EXPLAINED' | 'BREACH'
  auto_paused   INTEGER NOT NULL DEFAULT 0,
  resolved_at   INTEGER,
  resolution_note TEXT
);

-- (reused pattern from audit_log.rs)
CREATE TABLE signature_grant_log (
  id            INTEGER PRIMARY KEY,
  at            INTEGER NOT NULL,
  action_type   TEXT NOT NULL,     -- 'attestation' | 'goldcoin_payout' | 'governance' | 'rebalance'
  identity      TEXT NOT NULL,     -- signer identity, never key material
  request_id    INTEGER,
  severity      TEXT NOT NULL CHECK (severity IN ('info','warn'))
);
```

```sql
-- v15 (admin control plane, docs/27-admin-control-plane.md): append-only
-- audit trail for privileged admin operations that don't transition one
-- of the request/rebalance/custody state machines. Written for every
-- mutation ATTEMPT (refusals included) by the admin API, under the
-- operator identity its bearer token resolved to.
CREATE TABLE admin_audit_log (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  at        INTEGER NOT NULL,
  actor     TEXT    NOT NULL CHECK (actor <> ''),
  action    TEXT    NOT NULL CHECK (action <> ''),
  target    TEXT,               -- direction / request id / rebalance id
  old_value TEXT,
  new_value TEXT,
  note      TEXT    NOT NULL CHECK (note <> ''),
  outcome   TEXT    NOT NULL CHECK (outcome IN ('success','error')),
  error     TEXT
);
```

## Migration notes

Schema versioning follows the old bridge's numbered-migration convention (`db.rs`/`withdrawal_db.rs` used sequential `schema v1..v7` migrations applied at startup). This repo starts fresh at `v1` with the tables above — there is no live data to migrate from the old repo, so "migration" here means schema evolution within this repo going forward, not data migration from the old system. See [08-migration-strategy.md](08-migration-strategy.md).
