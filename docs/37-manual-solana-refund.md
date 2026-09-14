# Manual (out-of-band) Solana refunds — 2026-09-14

**Status:** implemented (PR "manual-solana-refund-export-import", schema
v35). Nothing here sends funds from a bridge key.

## 1. Why

After the 2026-09-13 remediation (docs/36) a backlog of Solana-sourced
deposits (`SolToGlc`, `SolToRhn`) sits in `ManualReview`. The in-band
refund path, `glc-admin refund-manual-review`, needs the on-chain
`refund_withdraw` instruction, and the deployed program (slot
442649805, sha256 `76c0b517…`) does not dispatch it
(`solana::program_compat`). Upgrading the program is a separate,
gated act. The operator refunds the backlog instead from a **separately
funded Solana wallet** — created, funded and kept entirely outside the
bridge (no config key, no daemon access, no path stored anywhere) —
and the bridge's only jobs are to say **who is owed exactly what**,
and afterwards to **verify and record** what landed.

## 2. The three steps

| step | tool | touches | default |
|---|---|---|---|
| export | `glc-admin manual-refund-export` | ledger (read), RPC (read) | read-only always |
| send | `solana-manual-refund-batch` (standalone binary) | RPC, the operator's wallet | dry run; `--execute` sends |
| import | `glc-admin manual-refund-import` | RPC (read), ledger (write with `--execute`) | dry run; `--execute` closes |

Two JSON files carry the contract between them: `refund-backlog.json`
(export → sender) and `refund-results.json` (sender → import). Both
are printed in full by their producers; both are re-verified by their
consumers.

### 2.1 Export — the backlog report

```
sudo glc-admin manual-refund-export --config /etc/glc-bridge/config.toml \
    --output /root/refunds/refund-backlog.json
```

Candidates: every request in `ManualReview` (or exactly the given
`--request-id N` list). For each, the ledger side
(`Ledger::manual_solana_refund_eligibility`) refuses unless:

- state is `ManualReview`;
- the route is **Solana-sourced** (`SolToGlc`/`SolToRhn`). A
  `RhnToGlc`/`RhnToSol`/`GlcToSol`/`GlcToRhn` park is refunded on
  Robinhood/Goldcoin by its own tooling; its row names no authoritative
  Solana refund destination (a `RhnToSol` row's `recipient` is where the
  PAYOUT was to go, not where the principal came from). It is never
  converted;
- `requester` and `source_obligation_index` are recorded;
- no destination txid, no `goldcoin_payouts` row of any state, no
  `robinhood_transactions` row of any kind;
- no refund lifecycle (`solana_refunds` / `goldcoin_refunds` / Robinhood
  refund) — including a `Pending` one that never broadcast (see §6);
- no closure, no earlier `manual_solana_refunds` row;
- no `process` operator decision; a rapid-burst hold has passed
  `review_after`.

Then the chain side (`solana::manual_refund::chain_expectation`, all
reads at `finalized`):

- `BridgeConfig.reserve_token_mint` equals `[solana].reserve_token_mint`;
- the `WithdrawalObligation` at `source_obligation_index` exists, its
  `requester` equals the stored requester, its `amount` equals the
  stored canonical gross narrowed to the mint's live decimals, and its
  status is `Pending` (anything else is a settlement or refund that
  already happened on chain);
- the mint is owned by the recorded token program (Token-2022 in
  production); the recipient's ATA, if it exists, holds that mint under
  that program.

**Recipient = the obligation's requester. Amount = the obligation's
amount.** Nothing is typed by an operator and there is no override
flag. Every refusal is printed under `EXCLUDED` with its reason and
written to the file's `excluded` list; the batch contains only the
eligible rows. The report also prints totals: ManualReview count,
refundable count, excluded count, total GLC, estimated SOL
(`requests × 5000 lamports + missing ATAs × rent`).

`refund-backlog.json` (abridged):

```json
{
  "schema": "glc-manual-solana-refund-batch/1",
  "batch_id": "mrb-20260914T153320Z-1f2e3d4c",
  "exported_at": 1789400000, "exported_by": "cli:root",
  "network": "solana", "rpc_url": "https://api.mainnet-beta.solana.com",
  "ledger_path": "/var/lib/glc-bridge/ledger.db",
  "mint": "Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump", "mint_decimals": 6,
  "token_program": "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", "token_symbol": "GLC",
  "request_count": 51, "excluded_count": 15,
  "total_amount_atomic": "2550000000000", "total_amount_display": "2550000.000000",
  "estimated_sol_lamports": 255000, "estimated_sol_display": "0.000255000",
  "requests": [
    {
      "request_id": 4361, "route": "SolToGlc", "state": "ManualReview",
      "manual_review_reason": "liquidity_buffer_low_at_fold", "created_at": 1789300000,
      "source_obligation_index": 4180,
      "recipient": "<depositor wallet>", "recipient_token_account": "<its GLC ATA>",
      "recipient_token_account_exists": true,
      "mint": "Hn6K…pump", "amount_atomic": "50000000000", "amount_display": "50000.000000",
      "amount_canonical_atomic": "5000000000000",
      "memo": "glc-manual-refund:mrb-20260914T153320Z-1f2e3d4c:4361"
    }
  ],
  "excluded": [
    { "request_id": 4381, "route": "RhnToGlc", "state": "ManualReview",
      "gross_amount_canonical_atomic": "2000000000000",
      "reason": "route RhnToGlc is not Solana-sourced: …" }
  ],
  "digest": "<sha256 over request_id|recipient|mint|amount_atomic lines>"
}
```

### 2.2 Send — the standalone sender

```
solana-manual-refund-batch --input refund-backlog.json \
    --keypair /secure/refund-wallet.json --output refund-results.json          # dry run
solana-manual-refund-batch --input refund-backlog.json \
    --keypair /secure/refund-wallet.json --output refund-results.json --execute
```

Built from this repository (`service/src/bin/solana-manual-refund-batch.rs`)
but needing only an RPC URL and the keypair path: no ledger, no config,
no bridge key. The keypair's secret is never printed. Before anything:
the batch's schema/digest/memos are verified; the mint's owner and
decimals are read fresh; the wallet's GLC ATA balance must cover every
outstanding request and its SOL must cover fees + rent for every
missing recipient ATA + 0.001 SOL margin; every
`recipient_token_account` is re-derived from `(recipient, mint,
program)` and must match.

Per request, ONE transaction: `[create_associated_token_account_idempotent
(payer = wallet), memo "glc-manual-refund:<batch>:<request>",
transfer_checked(wallet ATA → recipient ATA, amount, decimals)]`,
signed by the wallet only. The signature and blockhash are written to
`refund-results.json` (`status: submitted`) **before** `sendTransaction`;
then the sender waits for `finalized`, reads the transaction back and
verifies it is exactly this request's refund, and records
`finalized` with slot and block time. The file is rewritten atomically
after every state change.

States: `pending` → `submitted` → `finalized` | `failed`. Recovery on
rerun: a `submitted` entry is resolved from the cluster first — landed
OK → verified and finalized; landed and failed → `failed`; unseen with
its blockhash still valid → waited on; unseen after expiry (plus a
45 s grace) → `failed`. Only after every earlier entry is resolved does
the run send the next `pending` one. `failed` entries are rebuilt only
with `--retry-failed`, and even then the old signature is re-checked
first. Any AMBIGUOUS outcome (RPC error, confirmation timeout, a landed
transaction that does not verify) stops the run with the entry still
`submitted` and nothing after it sent. **A request whose recorded
transaction could still land is never sent again.**

`refund-results.json` (one entry):

```json
{
  "schema": "glc-manual-solana-refund-results/1",
  "batch_id": "mrb-20260914T153320Z-1f2e3d4c", "batch_digest": "<as exported>",
  "network": "solana", "rpc_url": "…", "mint": "Hn6K…pump", "mint_decimals": 6,
  "token_program": "Tokenz…", "refund_wallet": "<wallet pubkey>",
  "started_at": 1789400100, "updated_at": 1789400400,
  "results": [
    { "request_id": 4361, "recipient": "<depositor>", "recipient_token_account": "<ATA>",
      "mint": "Hn6K…pump", "amount_atomic": "50000000000", "amount_display": "50000.000000",
      "memo": "glc-manual-refund:mrb-…:4361", "refund_wallet": "<wallet>",
      "status": "finalized", "tx_signature": "<base58 signature>",
      "recent_blockhash": "<base58>", "slot": 446700123, "block_time": 1789400300,
      "submitted_at": 1789400290, "finalized_at": 1789400312, "error": null }
  ]
}
```

### 2.3 Import — verify on chain, then close

```
sudo glc-admin manual-refund-import --config /etc/glc-bridge/config.toml \
    --input refund-results.json --note "backlog batch 1"            # dry run
sudo glc-admin manual-refund-import --config /etc/glc-bridge/config.toml \
    --input refund-results.json --note "backlog batch 1" --execute
```

For every `finalized` entry, in this order, all fail-closed:

1. duplicates inside the file (request id or signature) → refused;
2. same signature already recorded for this request → `ALREADY_IMPORTED`
   (zero writes);
3. the ledger eligibility check of §2.1 again, live;
4. the chain expectation of §2.1 again, live; the manifest's recipient,
   amount and mint must equal it;
5. `getTransaction(signature)` at `finalized`: must exist, must have
   succeeded, must decode; exactly one signer (the declared wallet, as
   fee payer), no address-table lookups, only ATA/memo/token-program
   instructions, exactly one memo naming this batch and request,
   exactly one `transfer_checked` from the wallet's ATA to the
   requester's ATA of the reserve mint for exactly the expected amount
   and decimals with the wallet as authority, and the destination's
   post-minus-pre token balance from the transaction meta equal to
   that amount under that mint.

Only then, with `--execute`: `Ledger::record_manual_solana_refund`
writes the `manual_solana_refunds` row and the `refunded_out_of_band`
closure (reference = the signature) in ONE transaction, audited as
`manual_refund_import` (target = request id; new_value carries
signature, batch, wallet, amount, slot). Verdicts per request:
`IMPORTED` / `WOULD_IMPORT` / `ALREADY_IMPORTED` / `SKIPPED` (not
`finalized` — the request stays `ManualReview`) / `REFUSED` (reason).
One refused request never blocks a verified one.

## 3. Schema v35 — `manual_solana_refunds`

```
id, request_id UNIQUE → bridge_requests, batch_id, network CHECK 'solana',
mint(32), refund_wallet(32), recipient(32), recipient_token_account(32),
amount_atomic > 0 (mint units), amount_canonical_atomic > 0 (8dp),
tx_signature UNIQUE, slot, block_time, submitted_at, finalized_at,
imported_at, imported_by, note
```

Pure addition; `Ledger::open` migrates 34 → 35 (the daemon on restart,
or the first `glc-admin --db` call — so install the daemon first, as
always).

## 4. What users and operators see

Public `GET /transfers/{id}` and `GET /transfers` (state `Closed`):

```json
"manual_refund": {
  "status": "MANUALLY_REFUNDED", "network": "solana",
  "refund_amount_atomic": "5000000000000", "refund_amount_native_atomic": "50000000000",
  "mint": "Hn6K…pump", "tx_signature": "<base58>", "slot": 446700123,
  "refunded_at": 1789400300, "imported_at": 1789400500
}
```

`refunded_at` is the block time when the cluster supplied one, else the
sender's observed finality, else the import time. No party address is
exposed (this surface never carries one); the signature is the
explorer-link target. A UI renders: **MANUALLY REFUNDED · Network:
Solana · Refund amount: 50 000 GLC · Transaction ID: <signature> ·
Refunded at: <time>**.

Admin API: `GET /manual-review/closures` entries gain `manual_refund`
(wallet, batch, recipient, amounts, slot, importer, note);
`GET /manual-refunds` lists every recorded refund. `glc-admin
manual-refund-list` is the CLI equivalent. The admin console change
(rendering these fields) is a separate PR in the admin-ui repository.

## 5. Security posture

- The refund wallet is the operator's, created and funded outside this
  repository; hold only what one batch needs (GLC and SOL). The bridge
  stores its PUBLIC key on each imported row, read off the finalized
  transaction — never the path, never the secret.
- The sender never prints or logs key material; it runs with a
  `0600` keypair file the operator controls.
- Every amount and recipient is derived from the on-chain obligation
  and cross-checked with the ledger row; every landed transaction is
  verified from its bytes and its balance meta before it can close a
  request. A transaction for the wrong request, amount, mint,
  recipient or wallet is refused however well anything else matches.
- One request → at most one recorded refund (`request_id UNIQUE`); one
  transaction → at most one request (`tx_signature UNIQUE`).

## 6. Known non-candidates (as of 2026-09-14)

- **4140 and 4185** (`SolToGlc`, 50 000 GLC each) are in
  `RefundPending` with a `solana_refunds` row in `Pending` — an in-band
  refund that was begun and could never broadcast (program lacks
  `refund_withdraw`). They are NOT `ManualReview` and carry a refund
  lifecycle, so the export refuses them by design. Returning them to
  `ManualReview` (abandoning a never-broadcast in-band refund) needs
  its own audited operation and an explicit decision; it is not part of
  this workflow.
- Every `RhnToGlc` / `RhnToSol` park (Robinhood-sourced): refunded on
  Robinhood by `robinhood-refund`; listed under `EXCLUDED`.
