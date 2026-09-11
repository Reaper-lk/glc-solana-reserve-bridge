# Robinhood Network — Phase G handoff (admin, recovery, health, signer)

**Status:** implementation complete on `feat/robinhood-admin`,
**uncommitted**, **not deployed**, **routes still ship disabled**, **no
production credential configured**. Phase G closes Phase F's launch
blocker **A** (production EIP-712 signer) and blocker **G** (admin
recovery / health / status), plus blockers **H** (the Goldcoin adapter's
Robinhood capability), **I** (the route-aware Goldcoin deposit pipeline)
and **J** (the Robinhood-aware refund proof) found during the phase
itself. It opens nothing.

Phase F is `docs/32-robinhood-settlement-phase-f.md`, commit `5ca1e7b`.
This document is Phase G.

---

## 1. What Phase G adds

| Area | Module | What it is |
|---|---|---|
| Signer-side policy | `service/src/signing/evm_policy.rs` | the v2 request document and the policy a custody domain evaluates it against |
| Remote EIP-712 signer | `service/src/signing/remote.rs` | `RemoteEvmAuthSigner`, protocol v2 on `/v2/` paths |
| Authorization request | `service/src/robinhood/auth.rs` | `EvmAuthRequest` — the structured thing that crosses the signer boundary |
| Endpoint redaction | `service/src/robinhood/redact.rs` | strips URLs and credentials from anything reaching an operator surface |
| Admin recovery | `service/src/robinhood/admin.rs` | ManualReview queue, tx/nonce inspection, refund and settlement assessments, halt clearance, route status, reserve report |
| Operator preflight | `service/src/robinhood/preflight.rs` | `operator_preflight` — PASS / FAIL / **UNVERIFIED** |
| Contract reads | `service/src/robinhood/calls.rs` | `limits()`, `inboundWindow()`, `outboundWindow()` |
| Health | `service/src/ops/health.rs`, `collector.rs` | Robinhood invariants, gauges and the third reserve |
| Status | `service/src/admin_api.rs` | per-route availability, decomposed by gate |
| Operator CLI | `service/src/bin/glc-admin.rs` | eight `robinhood-*` subcommands |

**Schema is unchanged at v23.** No migration, no new table, no new
column. The one ledger addition is a read-only query
(`Ledger::highest_robinhood_nonce`).

---

## 2. The production EIP-712 signer (blocker A)

### 2.1 Why the existing protocol could not be extended in place

`POST /v1/sign` takes `payload_hex` — the exact bytes to sign — and works
because a Goldcoin sighash and a Solana claim message are **parseable**:
`signing::policy::parse_claim` reads the domain tag, the action byte and
the typed fields out of the payload, and the custody domain decides on
what it read.

An EIP-712 authorization is a **32-byte hash**. There is nothing in it to
parse. A signer handed one can check its length and absolutely nothing
else — which is precisely the blind-oracle posture the 2026-09-02 incident
produced, and which `signing::policy`'s module docs exist to describe.

### 2.2 The inversion

The v2 request carries the **structured fields**, and the custody domain
**recomputes the digest from them** using `robinhood::auth` — the same
single encoder the bridge used, and the one
`contracts/test/GoldenDigests.t.sol` cross-checks against the deployed
contract. **The signer signs the digest it computed. Never the one it was
sent.**

Three properties follow:

- A caller cannot make a signer sign arbitrary bytes, because a caller
  does not supply bytes to sign.
- A caller cannot smuggle content past the policy check, because the
  fields the policy inspects are the fields the digest is derived from.
  There is no second representation to disagree with.
- A requester that disagrees with the signer about the encoding is caught
  on the first request: the document carries the requester's own
  `expected_digest`, and a mismatch is `DigestMismatch`, never something
  either side papers over.

This is enforced in the type system, not by convention:
`EvmAuthSigner::sign_digest` **no longer exists**. The trait has one
signing method and it takes an `EvmAuthRequest`. There is no method on any
signer in this codebase that accepts bare bytes to sign.

### 2.3 Wire protocol

Two NEW endpoints, on their own paths:

- `GET  {base}/v2/evm-identity` → `{"address": "0x<20 bytes>"}`
- `POST {base}/v2/sign-evm-auth` → `{"signature_hex": "<130 hex chars>"}`

`/v1/identity` and `/v1/sign` are **byte-for-byte untouched**. A deployed
signer process that implements only v1 keeps serving Goldcoin and Solana
signatures exactly as before and simply does not answer the `/v2/` paths —
which this client reports as a refusal **at connect time**, so a
deployment cannot start believing it holds a quorum it does not.

That is why the extension is a new path rather than a new field on
`/v1/sign`: **a version negotiated by adding an optional field is a
version an old signer can ignore.**

### 2.4 What the signer binds

Every field the brief required, all of them bound by the recomputed
digest and independently checked by `EvmSignerPolicy`:

action type · route · request id · obligation index · amount · recipient /
depositor · `signerEpoch` · expiry · verifying contract · EVM chain id ·
protocol chain id pair.

The policy holds, independently of the request: chain id, verifying
contract, token, allowed action bytes, allowed routes, each route's
protocol chain pair, its own amount ceiling, and its own maximum
authorization lifetime.

Amounts cross the wire as **decimal strings**. 10^18 atomic units is one
whole GLC and does not survive an IEEE-754 double; a JSON parser that
rounded one would produce a digest for a different transfer than the
operator approved.

### 2.5 Signer disagreement fails closed — and the fix that made it true

`collect_quorum` tolerates a domain that is **down**, **slow**, or
**declining under its own policy**: that is what 2-of-3 exists for. It
aborts on a domain that **answers wrong**.

Those were originally the same `SignerError::Rejected`, and a test written
for this phase found the consequence: a signer deliberately signing a
DIFFERENT authorization was silently tolerated as a liveness fault, the
collection skipped to the next domain, and a quorum formed anyway from the
honest two. The defense-in-depth layer was **downgrading the severity of
what it caught**.

`SignerError::Untrustworthy` now separates the two, and
`collect_quorum` refuses to route around it. Assembling a quorum while one
of three custody domains is demonstrably misbehaving would turn a
detectable incident into a silent one.

### 2.6 Configuration

`[[robinhood.settlement.auth_remote_signers]]` — its own list with its own
credential, deliberately **not** a reuse of the v1 signer tables: one
compromised bearer token must not authorize both a Goldcoin payout and a
Robinhood one.

Refused at config load: dev-mode with remote signers, production-mode with
local key files, a declared address that is not one of the contract's
`authorized_signers`, a declared address that is the submitter, duplicate
addresses, duplicate endpoints.

`Config::load_robinhood_auth_signers` is now async and dispatches on
`operators.mode`; in production it CONNECTS to each domain and refuses to
start on any identity that is not the address the operator declared.

**No production credential is configured by this phase.** The pilot
config template contains zero Robinhood references.

---

## 3. The RPC credential leak, closed

`robinhood::rpc::EvmRpcError::Transport` is built from
`reqwest::Error::to_string()`, and reqwest's `Display` embeds the request
URL: a connection failure renders as
`error sending request for url (https://user:pass@node.example:8545/KEY)`.

That string was published verbatim into
`RobinhoodHealthSnapshot::last_rpc_error`. `/health` has **no
authentication by design** — `ops::health`'s own module docs record why —
so this put a live RPC provider credential on a port readable by anything
that could reach it.

`robinhood::redact::Redactor` now filters every string entering the
published health state, at `record_error` and `set_halt`. Three
overlapping passes: scheme-qualified URLs (needs no configuration, so it
catches a URL this service never configured), the configured endpoint's
own literals (catches a bare hostname in a DNS error or a password echoed
alone), and residual `user:pass@host` runs.

The filter sits at the **boundary**, not at the call sites, because this
service does not build those strings — reqwest does, a JSON-RPC node does,
and their phrasing can change under a dependency bump. A rule of the form
"call sites must remember not to include the URL" holds until the day it
does not.

What survives is what an operator triages on: a typed
`RobinhoodRpcErrorClass` derived from the error's TYPE (so it is unaffected
by anything written into a message), and the message's own prose. Block
numbers, chain ids, contract addresses and JSON-RPC codes are deliberately
NOT redacted — over-redaction has a real cost.

`RobinhoodHealth::new` now takes the RPC URL solely to build the redactor.
It is never stored as a readable field.

---

## 4. Admin recovery

Every operation is a pair: a read-only assessment naming each check
individually as PASS/FAIL, and an execution that **re-runs the same checks
itself** against fresh state. The assessment is a preview, never a
precondition — an execution never trusts that one ran.

Eight `glc-admin` subcommands, documented in `docs/09-runbook.md`:

`robinhood-status` · `robinhood-manual-review-list` · `robinhood-tx-show` ·
`robinhood-nonce-status` · `robinhood-refund` · `robinhood-clear-halt` ·
`robinhood-preflight` · `robinhood-reserve`

### What is structurally absent

- **No force-complete.** Nothing marks a request Settled or moves it past
  a state it did not earn.
- **No balance movement** other than a refund whose recipient and amount
  come from the chain.
- **No nonce rewrite.** Nonce state is READ. Nothing sets, resets, skips
  or reallocates one — the allocator is the ledger's own maximum inside
  the write transaction that stores it, and editing that by hand would
  reintroduce the duplicate-broadcast window the design removes.
- **No `--destination`, no `--amount`** on any command.
- **No abandonment.** The on-chain path that closes an obligation while
  retaining a depositor's principal has no representation here.

### `begin_refund`'s production caller (blocker G)

`glc-admin robinhood-refund --config … --request-id N --note … [--execute]`.

Dry run by default. `--execute` runs the startup preflight, re-runs every
eligibility check, reads the obligation back from the chain, collects the
2-of-3 quorum, broadcasts, drives the receipt phase, and reports.

Refuses on four independent checks against four independent sources of
truth: a settlement operation exists (ledger), a Goldcoin payout
transaction exists (different ledger table), the request is not `RhnToGlc`
in `ManualReview` (request state), the obligation is not `Pending`
(chain).

**Why the CLI drives the broadcast.** The settlement daemon runs its
broadcast phase only while BOTH executable routes are open — and a refund
is exactly what an operator does when they are not. A refund that is
authorized but never sent is not a refund. The phase is not scoped to one
operation (it advances every already-authorized row, each of which
legitimately holds a minted quorum), and both the command's help text and
its output say so, so nothing happens silently.

### Halt clearance

`--expect-reason` is **required** and must equal the stored reason. This is
the check that stops "make the red light go away": an operator who has
diagnosed a chain-id mismatch names `chain_id_mismatch`, and if the stored
halt is actually a post-finality reorg they find out here rather than
after the indexer re-halts.

Additionally: refused while ANY operation is in flight (an unresolved
broadcast is verified against the indexer's view of the chain); a reorg
halt requires `--acknowledge-orphaned-finality` after reviewing the count
of folded finalized observations; a chain-id / wrong-contract halt is
cleared only if a **live preflight passes right now** — re-verified from
the chain, never asserted on the command line.

---

## 5. Health, status, reserve

**Unchanged when Robinhood is absent — as a property, not a claim.** With
no `[robinhood.indexer]` section the collector attaches nothing, and
`build_report` emits no Robinhood invariant, no Robinhood gauge and no
reserve row. Pinned by
`an_absent_robinhood_config_changes_the_report_not_at_all`, which asserts
the rendered metrics contain no `robinhood` substring at all.

Invariants (these page): indexer halted · chain-id disagreement · stalled
operations · the Robinhood reserve's own invariant and pause.

**The signer quorum pages only when a route is actually open.** With every
route closed — how this ships — an unformable quorum is a launch blocker,
and paging on it would train operators to ignore the page. With a route
open it is a live outage. It is reported as a gauge either way.

Everything else is a gauge: connected, expected/observed chain id, head,
finalized frontier, cursor, lag, seconds since success, reorgs, deepest
reorg, settlement configured, deployment verified, signers available /
required, submitter observed nonce, operations in flight / stalled, any
route open.

**Status** decomposes each Robinhood route into the gates that decide it —
service enabled, contract route enabled (`null` when unread, never
assumed), effective availability, `disabled_reason` and, separately,
`health_reason`. Merging the last two would send an operator to look at
configuration when the problem is the chain. This lives behind the
authenticated admin listener; the public `/chains` boundary is unchanged
and still gives end users one cause-agnostic message.

**Reserve** is reported as a third independent reserve, never netted
against Goldcoin or Solana. Ledger figures in canonical 8dp; on-chain
balance, encumbered reserve and both rolling-limit buckets in Robinhood
native 18dp. An unconfigured reserve reports **absent**, not empty —
`Some(zeroes)` would say "configured and empty", which is a different and
much more alarming statement.

The rolling window is a **fixed** bucket, not sliding: the whole limit
returns at once when the bucket expires. `RollingWindow::remaining`
reports the full limit past the boundary rather than a stale total,
mirroring `_consumeWindow`'s own reset.

---

## 6. Operator preflight

`operator_preflight` calls `verify` — never a second copy of its logic —
and expands its single answer into the ordered check list it actually
performed. Because `verify` short-circuits, everything after a failure
genuinely was not run and is reported **UNVERIFIED**, not assumed either
way.

It then adds what a startup gate does not check: the contract's four route
flags against what the operator expects, the submitter's reachability and
gas balance, and whether a quorum could form at all.

**UNVERIFIED is not PASS.** Every token security property is permanently
UNVERIFIED — mint authority, blocklist/freeze, transfer hooks,
fee-on-transfer, pause, proxy upgradeability. A successful `decimals()`
read establishes none of them, and reporting them as PASS because a
preflight ran would be the single most harmful thing this command could
do.

Route flags default to expecting **all four closed**, so an unexpectedly
open route is a FAIL rather than something nobody looked at.

---

## 7. `SolToRhn` / `RhnToSol` remain non-executable

> **Superseded by Phase H** (docs/35-solana-robinhood-routes-phase-h.md): both
> routes now have settlement machinery and ship disabled on every gate. The
> section below is kept as the historical record of this phase.

Unchanged and re-asserted. `Route::as_direction()` returns `None` for
both; no `Direction` value can be produced; the v23
`bridge_requests.direction` CHECK cannot spell either; the Robinhood
adapter refuses both even when fully verified; `EvmSignerPolicy` cannot
produce a consistent digest for either even when deliberately configured
to allow them. `service/tests/robinhood_route_isolation.rs` (9 tests)
passes unchanged.

---

## 8. Validation

| Check | Result |
|---|---|
| `cargo +nightly fmt -- --check` | clean |
| `cargo +nightly clippy --all-targets -- -D warnings` | clean |
| `cargo +nightly test` | **1945 passed, 0 failed, 2 ignored** (26 suites) |
| `forge test` (in `contracts/`) | **320 passed, 0 failed, 0 skipped** (19 suites) |

Phase F's baseline was 1806; Phase G adds 139 tests. The 2 ignored are the
same pre-existing long-running harnesses Phase F reported. `contracts/` is
untouched by this phase.

---

## 9. Remaining launch blockers

Phase F's blockers **B** (live-chain parameters), **C** (contract not
deployed), **D** (token security review), **E** (`libsecp256k1`
RUSTSEC-2025-0161) and **F** (reserve sizing / rolling limits) are all
unchanged — none is code this repository can discharge. **A** and **G** are
closed by this phase.

Three blockers were found during this phase, and all three are now
resolved:

### H. `GoldcoinAdapter` refused the Robinhood routes — RESOLVED

`chains::GoldcoinAdapter::capability` returned `Unavailable` for all four
Robinhood routes, and `RouteGate::ensure_enabled` requires BOTH legs of a
route to be operational. So `GlcToRhn` and `RhnToGlc` were permanently
closed by the Goldcoin leg, whatever every other gate said.

**`RhnToGlc` is fixed.** Goldcoin is that route's DESTINATION, and
settling it means building and broadcasting a vault payout —
`Orchestrator::tick_goldcoin_payouts` already sweeps every direction whose
`destination_is_goldcoin()`, which includes it, using the same payout
plan, coin selection, fee policy, 2-of-3 vault signing and broadcast a
`SolToGlc` payout uses. The capability now says so. It enables nothing:
config, the ledger's `bridge_routes` state, the contract's own
`routeEnabled`, preflight, the signer quorum, reserve availability and the
pause all still stand, each pinned by its own test.

**`GlcToRhn` was refused on the SOURCE leg**, and that residue became
blocker I below. It is now closed, and `GoldcoinAdapter::capability`
returns `Operational` for both Goldcoin<->Robinhood routes. Pinned by
`chains::tests::the_goldcoin_adapter_serves_the_glc_to_rhn_source_leg`
and, at the status surface, by
`robinhood::admin::tests::the_goldcoin_adapter_leg_no_longer_closes_either_robinhood_route`.

### I. The `GlcToRhn` Goldcoin source pipeline did not exist — RESOLVED

The Goldcoin deposit intake produced `GlcToSol` requests and nothing else.
It is now keyed on `Direction::source_is_goldcoin()` throughout, so it
serves `GlcToSol` and `GlcToRhn` as ONE pipeline rather than two.

#### What changed

| Layer | Before | Now |
|---|---|---|
| `api::BridgeApi::create_goldcoin_deposit_transfer` (was `create_glc_to_sol_transfer`) | refused any direction but `GlcToSol` | accepts either Goldcoin-sourced route; recipient parsed as the DESTINATION chain's address type |
| `Ledger::set_goldcoin_deposit_address` (was `set_glc_to_sol_deposit_address`) | `direction != GlcToSol` -> error | `!source_is_goldcoin()` -> `NotAGoldcoinSourcedRequest` |
| `Ledger::find_goldcoin_deposit_request_by_script` (was `..._glc_to_sol_...`) | `WHERE direction = 'GlcToSol'`, returned `id` | both directions, returns `(id, Direction)` |
| `Ledger::all_goldcoin_deposit_addresses` / `..._script_pubkeys` | `GlcToSol` only | both — this is what puts a `GlcToRhn` address on the node's `list_unspent` watch list |
| `Ledger::record_glc_deposit_observed` | `direction != GlcToSol` -> `NoMatchingRequest` | `!source_is_goldcoin()` -> `NoMatchingRequest` |
| `goldcoin::indexer::promote_confirming` | queried `Direction::GlcToSol` | sweeps every Goldcoin-sourced direction |
| `Ledger::goldcoin_rollback_reorg` / `detect_post_finality_reorg` | `GlcToSol` only | both — an orphaned `GlcToRhn` deposit is reverted, and a post-finality reorg over one is detected as the incident it is |
| the unfinalized-deposit UTXO exclusion (6 query sites) | `GlcToSol` only | both, via one shared `unfinalized_goldcoin_deposit_exclusion` fragment |
| `Direction::ALL` ManualReview backlog (`/health`, `ops::collector`, `glc-admin`) | the two legacy directions | every direction |

#### How the route is bound durably

A `GlcToRhn` request is created AS `GlcToRhn` by
`Ledger::create_request`'s single INSERT. Nothing in this service updates
`bridge_requests.direction` afterwards — there is no path that creates a
`GlcToSol` row and mutates it. The route is then bound to four things at
once:

- the **request row** — `direction`, plus the schema v23 CHECK that admits
  only the four spellings;
- the **deposit script/address** — derived from the request id and written
  under the partial unique index `ux_bridge_requests_deposit_script`, so
  one script maps to exactly one row and therefore to exactly one route.
  A deposit cannot select its own route: the only thing it can address is
  a script that already belongs to one request;
- the **intended Robinhood recipient** — the 20 EVM address bytes in
  `recipient`, parsed at intake with `EvmAddress` (EIP-55 checked, zero
  address refused) and re-parsed by `Settler::authorize_payout` before any
  authorization is built;
- the **quote/fee context** — the fee is computed server-side at the fixed
  protocol rate, and the net is proved exactly representable at Robinhood
  precision (`CanonicalAtomic::to_robinhood`) before any capacity is
  reserved, the same check `quote` already ran.

#### Fail-closed properties

- An unknown route name is a 400; a refused route is a 409. Neither ever
  falls back to `GlcToSol`. The `None` default applies only to an ABSENT
  field, which is what every existing client sends.
- `SolToRhn`/`RhnToSol` have no `Direction`, so they cannot reach any
  entry point in this pipeline — and the database cannot spell them.
- A recipient of the wrong chain's address type fails to parse. It is
  never stored as bytes for something else to interpret.
- The Solana rolling-volume quota is applied to `GlcToSol` only: it is a
  Solana program's window bounding the Solana reserve. A Robinhood payout
  is bounded by the contract's own `inboundWindow`, read by preflight.

#### The refund path

Closed separately, as blocker J below: the refund proof is now
route-specific rather than widened.

#### Still disabled

`Route::GlcToRhn.default_enabled()` and `Route::RhnToGlc.default_enabled()`
are both `false`. Config, the ledger's `bridge_routes` state, the
contract's own `routeEnabled`, preflight, the signer quorum, reserve
availability and the pause all still stand in front of both, each pinned
by its own test.

### J. No safe refund for a parked `GlcToRhn` deposit — RESOLVED

Opened by I. A `GlcToRhn` deposit parked in `ManualReview` could not be
refunded, because `glc-admin refund-glc` proved "no settlement has begun"
with Solana-shaped evidence — `bridge_requests.destination_txid`,
`settlement_claim_hash`, and the on-chain `DepositClaim` PDA — and a
Robinhood payout writes **none** of them. Admitting the direction through
that proof would have meant asking three columns that are NULL by
construction and reading their silence as an all-clear.

The fix is a second, route-specific proof, not a widened first one.

#### The predicate

> A `GlcToRhn` Goldcoin refund is eligible only if **no
> `robinhood_transactions` row of any kind names the request**, and no
> Robinhood deposit observation records it as the row it folded into.

`Ledger::robinhood_payout_evidence` answers it, returning a
`Vec<RobinhoodPayoutEvidence>`; **empty means proven not started**.

#### Why the mere EXISTENCE of a payout row is the whole predicate

Three properties of the Phase F lifecycle make it exact rather than
merely cautious:

1. **It is the earliest marker.** `Ledger::begin_robinhood_tx` writes the
   row in `Authorizing` *before the first custody domain is contacted*.
   Every later step — signatures, nonce allocation, signing, broadcast,
   replacement, receipt, finality, revert — requires the row to already
   exist. The nonce in particular lives on the row itself
   (`allocate_robinhood_nonce` refuses unless the row is `Authorized`);
   `evm_submitter_state` holds only an observed floor, never an
   allocation, so there is no window in which a nonce is committed but no
   row exists.
2. **It is permanent.** Nothing in this service deletes a
   `robinhood_transactions` row — verified by grep, not assumed — and
   `ux_robinhood_tx_operation` (`kind`, `request_id`) makes a second one
   impossible. The refusal is therefore reflexive and monotone: once it
   fires it never un-fires on its own.
3. **It never traps a legitimate refund.** A request is refundable only
   from `ManualReview`, and `Settler::tick_authorize` only ever picks up
   `SourceFinalized` requests. The two populations do not overlap in
   normal operation; an overlap **is** the anomaly this refuses on.

#### States that block a refund, permanently

Every one of these implies the row exists, so all of them block:

| Durable state | Blocks |
|---|---|
| `Authorizing` — row written, no signature yet | yes |
| authorization signatures persisted (1 or 2) | yes |
| `Authorized` — 2-of-3 quorum stored | yes |
| nonce allocated to the submitter | yes |
| `Signed` — raw tx + tx hash persisted | yes |
| `Broadcast` — including a transport failure whose fate is unknown | yes |
| replacement attempts recorded | yes |
| `Included` — receipt `status = 1` | yes |
| `Finalized` | yes |
| `Reverted` — receipt `status = 0` | yes |
| `ManualReview` — revert, exhausted budget, or a chain/ledger disagreement | yes |
| a `Settlement`/`Refund` row naming a Goldcoin-sourced request | yes (corruption) |
| a Robinhood deposit observation folded into it | yes (corruption) |

#### Reverted and uncertain payouts

A `Reverted` receipt is **not** an all-clear and does not re-open the
refund. The transaction consumed its nonce and its gas; whether the
contract moved value is a question for a human with the chain in front of
them, and this path never answers it by assumption. `Broadcast` is
treated identically — that state says nothing about whether a node
received the bytes, which is precisely why it must not be read as "did
not happen". Both leave the request parked for operator review.

#### Restart safety

The proof is committed database state and nothing else — no daemon
memory, no tick report, no live chain read. A restart cannot make an
in-flight payout look refundable because there is no in-memory state to
lose. `ledger::tests::the_refusal_survives_reopening_the_ledger` asserts
it against a real file-backed ledger reopened from scratch.

#### Route-specific, and mutually reinforcing

The two proofs are not merged into a weaker generic condition. Each route
still stands or falls on its own:

- `GlcToSol` -> the Solana proof (`no_destination_txid`,
  `no_settlement_claim`, plus the on-chain `DepositClaim` PDA in
  `goldcoin::refund`). **Unchanged.**
- `GlcToRhn` -> the Robinhood proof above.

Each additionally requires the *other's* evidence to be absent. A
Solana settlement column set on a `GlcToRhn` row, or a Robinhood payout
row naming a `GlcToSol` request, is a contradiction — and a refund is not
the moment to discover one. That is strictly stronger on both sides and
can only ever fire on state that should not exist.

#### Enforced, not merely reported

`glc_refund_db_checks` produces the printable view;
`Ledger::begin_goldcoin_refund` re-runs the **identical query** inside its
own write transaction, via the shared
`Ledger::robinhood_payout_evidence_in`. A payout that starts between an
operator's dry run and their execute resolves to a refusal rather than to
two payments.

#### Goldcoin safety, unchanged

The refund amount is still the full observed principal derived from
chain and cross-checked against the durable ledger witness; the
destination is still derived from the deposit's own prevout and cannot be
supplied by an operator; the one-refund-per-request rule, the
`GoldcoinReserve` pause precondition, and the vault-UTXO reservation are
all untouched.

#### No automatic refund

Refund initiation remains an explicit operator act
(`glc-admin refund-glc --execute` -> the authenticated admin endpoint).
No orchestrator or settlement tick creates a `goldcoin_refunds` row, and
a parked `GlcToRhn` request stays parked whether the route is open or
closed — pinned by
`robinhood::settlement::tests::a_parked_glc_to_rhn_request_is_never_refunded_by_a_settlement_tick`.

#### Residual, recorded rather than papered over

The `GlcToSol` proof has a chain-side witness (the `DepositClaim` PDA)
that defends against a ledger restored from a stale backup. The
`GlcToRhn` proof is **database-only**, so it does not. The contract
exposes the equivalent read — `requestExecuted(action, requestId)`,
already used by the restart path — but wiring it into the refund dry run
means a new RPC dependency and a new parameter on `dry_run_refund` /
`execute_refund`, which is a change to the shared refund surface rather
than to the Robinhood proof. Recorded as a Phase H item; not claimed
here.

---

## 10. What was NOT done

- Nothing committed, pushed or deployed.
- No route enabled, on this side or on-chain.
- No production configuration written; no credential provisioned; the
  pilot config template still contains zero Robinhood references.
- Schema unchanged at v23.
- `contracts/` unchanged.
- The public `/chains` and `/status` API boundaries unchanged.

---

## 11. Recorded for Phase H

- **Robinhood chain-side refund witness.** Add
  `requestExecuted(ACTION_PAYOUT, requestId)` to the `GlcToRhn` refund
  proof so it has the same two-independent-witnesses shape the `GlcToSol`
  proof has. See blocker J's residual note.
- **Wallet-scoped activity does not show `GlcToRhn`.**
  `Ledger::transfers_page` matches a caller's address against
  `recipient` (`GlcToSol`) and `requester` (`SolToGlc`) as **32-byte**
  values. A `GlcToRhn` recipient is a 20-byte EVM address, so it can
  never match and such a transfer will not appear in a wallet's "my
  activity" list. Not a safety issue — no value moves through that
  projection and `GET /transfers/{id}` shows the request correctly — and
  deliberately NOT fixed here: it is a UI-surface change with its own
  API-shape decisions.
- **`late_deposit_no_capacity` is still not on the refundable list.**
  `REFUNDABLE_GLC_MANUAL_REVIEW_REASONS` is unchanged at
  `["deposit_amount_mismatch"]` for both Goldcoin-sourced routes, so a
  `GlcToRhn` deposit parked for want of Robinhood reserve capacity needs
  the same operator handling a `GlcToSol` one does today. Widening the
  reason list is its own decision and was not bundled into J.
