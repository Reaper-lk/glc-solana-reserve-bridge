# 34 — Robinhood reserve withdrawal: audit, parity design, and the blocker

**Status: NOT IMPLEMENTED — blocked on an on-chain change.**

This document is the result of tracing the existing Solana reserve
withdrawal end to end and then auditing the Robinhood contract, ABI,
backend and `glc-admin` for an equivalent. The conclusion is stated first
because it decides everything else:

> `GlcRobinhoodBridge` exposes **no reserve-withdrawal entry point**, and
> it is **not upgradeable**. No change confined to this repository's
> off-chain code can produce an executable Robinhood reserve withdrawal.
> Building one in the backend would produce a command that authorizes,
> signs, broadcasts and then reverts — or worse, a ledger row asserting a
> transfer that never happened.

What was therefore implemented instead is the *negative* half of the
requirement: making it structurally impossible to record a Robinhood
rebalance withdrawal that nothing could execute. See §7.

---

## 1. The existing Solana reserve withdrawal, end to end

### 1.1 On-chain

`programs/glc-reserve-bridge/src/instructions/treasury_withdraw.rs` —
`treasury_withdraw(nonce, amount, attestation_epoch)`.

It replaced the retired `rebalance_withdraw`, whose destination was
operator-supplied and which was used to drain the reserve on 2026-09-02.
Twelve checks, in enforcement order:

| # | check | mechanism |
|---|-------|-----------|
| 1 | bridge is GLOBALLY paused | `require!(config.paused)` |
| 2 | `admin` signed | `Signer<'info>` + `bridge_config.admin == admin.key()` |
| 3 | attestation epoch is current | `attestation_epoch == key_set.epoch` |
| 4 | `amount > 0` | `require!` |
| 5 | nonce is in the treasury namespace | `nonce & NONCE_DOMAIN_REFUND == 0` |
| 6 | policy account exists and is self-consistent | Anchor `Account` deser + `treasury_count` bounds |
| 7 | destination is verbatim in the allowlist | `policy.is_allowlisted(&destination)` |
| 8 | destination is not the reserve itself | explicit compare |
| 9 | mint / token-account extensions re-reviewed | `validate_*_extensions` |
| 10 | `protected_minimum` preserved | `enforce_protected_minimum` |
| 11 | ≥ threshold unique attestation signatures | ed25519 precompile in the *immediately preceding* instruction, over the canonical claim |
| 12 | per-nonce replay guard | `init` on the `rebalance_withdrawal` PDA — reuse fails at account creation |

The signed claim
(`shared::claim::treasury_withdraw_claim_message`) binds protocol version,
program id, attestation epoch, nonce, amount, destination, reserve mint,
source reserve token account, **and the `RebalancePolicy.version`** — so an
attestation collected while a treasury was allowlisted cannot be replayed
after governance removes it.

There is deliberately **no per-withdrawal amount ceiling** and no rate
limit. `protected_minimum` is the one accounting floor.

### 1.2 Destination policy

`RebalancePolicy` (PDA, `SEED_REBALANCE_POLICY`) holds an allowlist of up
to `MAX_TREASURY_DESTINATIONS` treasury token accounts plus a monotonic
`version`. Editing it requires a threshold of attestation keys plus a
public timelock (`instructions/rebalance_policy.rs`,
`upgrade_timelock.rs`) and **cannot be done from the bridge host at all**.

A missing policy account fails closed: no policy ⇒ no allowlisted
destination ⇒ no authorized withdrawal. There is no
"policy absent implies unrestricted" branch anywhere.

### 1.3 Operator CLI — `glc-treasury-withdraw`

Three staged subcommands so no single invocation holds every credential:

```
plan     --rpc-url --amount --nonce [--treasury] [--reserve-mint] [--token-program] --out plan.json
attest   --plan --rpc-url --attestation-signer PUBKEY,URL,ENV[,MS] (xN) --out attested.json
execute  --attested-plan --rpc-url --admin-keypair --submitter-keypair [--execute]
```

- **`plan`** needs no key. Reads live `BridgeConfig`, `AttestationKeySet`,
  reserve balance, `RebalancePolicy`, destination token account and the
  nonce PDA; runs every policy check; derives (never accepts) the reserve
  authority PDA and reserve ATA; builds the claim; writes `plan.json`.
  **This step IS the dry run.**
- **`attest`** holds no private key. Re-verifies the plan against its own
  primitive fields (`verify_plan_not_tampered` recomputes both PDAs and
  the claim message), re-fetches live state, re-checks the policy version,
  then calls ≥ threshold *remote* signer endpoints via
  `signing::remote::RemoteAttestationSigner`. Documented to run on the
  approval host, not the bridge host.
- **`execute`** needs only admin + submitter keypairs. Third independent
  live-state check, `collect_valid_attestations` (verifies each signature
  locally against the claim and against the current key set, dedupes),
  builds `[ed25519_proof, treasury_withdraw]`, **always simulates**, and
  broadcasts only if `--execute` was passed **and** simulation succeeded.

There is **no `--destination` flag**. It was removed, not deprecated;
`--treasury` only disambiguates among already-allowlisted entries and is
checked against the allowlist rather than trusted.

### 1.4 Where Solana stops short

Two gaps, both relevant to the parity brief:

- **`execute` exits 0 on broadcast, not on confirmation.** It returns
  immediately after `send_transaction` and prints the signature. It never
  polls `get_signature_status`, never observes finality, and cannot detect
  an on-chain failure. Exit 0 currently means "submitted".
- **No machine-readable output.** Every stage prints prose; a wrapper must
  grep it. `plan.json` / `attested-plan.json` are files, not a result.

Robinhood's own settlement path (`RobinhoodTxState`,
`Settler::tick_receipts`) is strictly better on both counts — see §5.

### 1.5 Ledger side

`rebalance_requests` (propose → approve → record-executed → confirm/fail)
is an **evidence record of an out-of-band action**, not an executor.
`glc-admin rebalance-propose|approve|reject|cancel|record-executed|confirm|fail`
and `POST /rebalances` drive it. `record_rebalance_executed` takes an
operator-supplied `tx_reference`; nothing verifies it against a chain.

`service/src/rebalance.rs::assess` classifies a reserve against its own
configured bands and suggests a size. `assess_both` covers Goldcoin and
Solana only.

### 1.6 The Goldcoin path, for contrast

`glc-rebalance-withdraw plan|sign|broadcast` (UTXO multisig vault,
`vault_threshold`-of-N, one signer's key per invocation). Its
`--confirm-paused` is an operator assertion; `--solana-rpc-url` optionally
upgrades it to an authoritative on-chain read. That optionality is a
weakness the brief explicitly calls out, and it is **not** a pattern to
copy.

---

## 2. Audit of current Robinhood support

Searched `contracts/src`, the backend and `glc-admin` for `withdraw`,
`reserveWithdraw`, `treasury`, `rebalance`, `rescue`, `sweep`,
`emergencyWithdraw`, `adminTransfer`, `transferReserve`.

### 2.1 Every token outflow in the contract

There are exactly three `TOKEN.safeTransfer` sites in
`contracts/src/GlcRobinhoodBridge.sol`:

| line | function | destination | amount | operator-chosen? |
|------|----------|-------------|--------|------------------|
| 896 | `executePayout` | `req.recipient`, bound into the signed `PayoutAuth` | `req.amount` | recipient is the bridge user's, from an observed Goldcoin/Solana deposit |
| 944 | `executeRefund` | `req.recipient`, compared against the obligation's **own recorded `depositor`** | the obligation's own recorded `amount`, compared exactly | no |
| 1377 | `finalizeMigration` | the committed successor contract | `balanceOf(address(this))` — **all of it** | no: successor only, after 48h, vetoable |

The three categories the brief asks to distinguish:

1. **normal `GlcToRhn` user payout** — `executePayout`, `ACTION_PAYOUT`.
   Backend: `RobinhoodTxKind::Payout`.
2. **`RhnToGlc` refund** — `executeRefund`, `ACTION_REFUND`. Backend:
   `RobinhoodTxKind::Refund`, `robinhood::refund::begin_refund`, CLI
   `glc-admin robinhood-refund`. Its module docs open with "It is
   emphatically not a withdrawal."
3. **general reserve / treasury withdrawal** — **does not exist**, at any
   layer.

### 2.2 What is structurally missing

| layer | Solana has | Robinhood has |
|-------|-----------|---------------|
| on-chain entry point | `treasury_withdraw` | — |
| action discriminator | `WITHDRAWAL_CLASS_TREASURY` | `ACTION_*` runs `0x01`–`0x0B`; no withdrawal value |
| destination allowlist | `RebalancePolicy` PDA + timelock | — |
| signed claim type | `treasury_withdraw_claim_message` | `PAYOUT_TYPE` / `REFUND_TYPE` / `SETTLEMENT_TYPE` only |
| signer policy arm | `signing::policy` | `signing::evm_policy::evaluate` matches a **closed** set of three actions and returns `UnknownAction` otherwise |
| ledger lifecycle | `RebalanceWithdrawal` PDA | `RobinhoodTxKind` has exactly three variants, CHECK-constrained in schema v23 |
| operator CLI | `glc-treasury-withdraw` | — |

### 2.3 Upgradeability

From the contract's own header:

> There is no owner, no admin, no proxy and no upgrade path.

Confirmed by inspection: no proxy imports, no `initialize`, no
`upgradeTo`, no delegatecall, no storage gap. The only privileged
operations are 2-of-3 EIP-712 authorizations over a fixed action set, and
the only unilateral single-key power is the guardians' pause.

`docs/robinhood/mainnet-status.json` records the deployed address as
`0x1753dDA0256A2cB10B44497ACeA9650A1422f440`, `status: "disabled"`,
`reserve_configured: false`. No RPC read was performed against it (this
task is forbidden from touching production); the conclusion rests on the
source of the contract this repository was written against, which contains
no such function to deploy.

### 2.4 Answer

**No.** The deployed Robinhood contract does not support a safe reserve
withdrawal, and no combination of existing entry points approximates one:

- `executeRefund` cannot: recipient and amount are the contract's own
  recorded obligation values, compared exactly, and it requires a
  `Pending` obligation to consume.
- `finalizeMigration` cannot: it moves 100% of the balance, only to a
  contract implementing `IGlcReserveBridgeSuccessor` with a matching
  `token()` and `bridgeProtocolId()`, only after both directions are
  paused, only 48h after commitment, only with zero outstanding refund
  liability, and any one guardian can veto it at any point up to
  execution. It is terminal for the contract.

Attempting a "treasury withdrawal" through migration would mean deploying
a shell successor contract to receive the whole reserve — which is a
migration, with all of a migration's consequences, dressed up as a
rebalance. It is not a substitute and must not be used as one.

---

## 3. The smallest contract change that would lift this

A new authorized action, mirroring `treasury_withdraw`'s shape in
EIP-712 form. Roughly 60 lines of Solidity:

```solidity
uint8 public constant ACTION_TREASURY_WITHDRAW = 0x0C;   // next free value

/// Immutable, set at construction. NOT a storage variable and NOT
/// settable: the whole point is that the destination is fixed before the
/// contract holds anything, exactly as the Solana RebalancePolicy fixes
/// it out of reach of the bridge host.
address public immutable TREASURY;

struct TreasuryWithdrawRequest {
    bytes32 requestId;
    uint256 amount;        // Robinhood 18dp atomic
    uint64  signerEpoch;
    uint64  expiry;
}

function executeTreasuryWithdraw(
    TreasuryWithdrawRequest calldata req,
    bytes[] calldata signatures
) external nonReentrant {
    if (!depositsPaused || !payoutsPaused) revert MigrationRequiresPause();
    _requireCanonicalAmount(req.amount);                 // exact 1e10 multiple
    _authorize(ACTION_TREASURY_WITHDRAW, _twStructHash(req), req.signerEpoch,
               req.expiry, signatures);                  // 2-of-3 EIP-712
    _consumeRequest(ACTION_TREASURY_WITHDRAW, req.requestId);  // replay guard
    _requireSpendableReserve(req.amount, _limits.protectedMinReserve);
    TOKEN.safeTransfer(TREASURY, req.amount);            // NO destination arg
    emit TreasuryWithdrawExecuted(req.requestId, TREASURY, req.amount);
}
```

Design points that are not optional:

- **`TREASURY` is `immutable`, not a parameter and not storage.** This is
  the Robinhood equivalent of the Solana allowlist, and it is stronger:
  Solana's allowlist is governed-mutable behind a timelock, whereas an
  immutable is unreachable even by a full signer-set compromise. If a
  mutable allowlist is wanted instead, it needs its own timelock and veto,
  i.e. considerably more than 60 lines.
- **Reuses `_requireSpendableReserve`**, so the withdrawal cannot eat
  `protectedMinReserve` or any unsettled depositor's principal — the
  existing on-chain accounting, not a second implementation.
- **Requires both directions paused**, matching `treasury_withdraw`'s
  global-pause requirement and making the check authoritative and
  on-chain, not an operator flag.
- **New action byte** ⇒ a signature for any existing action can never
  verify as a withdrawal, and vice versa.
- Deliberately **no per-call ceiling**, matching the Solana rationale.

### 3.1 Deployment consequence

The contract is not upgradeable, so this is a **redeployment**, and the
existing reserve must reach the new contract by the only route that
exists: `commitMigration` → 48h → settle/refund every obligation to zero
→ `finalizeMigration`. Concretely:

1. Write and audit `GlcRobinhoodBridgeV2` (V1 + the above).
2. Pause both directions on V1 (2-of-3, or a guardian).
3. Settle or refund every `Pending` obligation until
   `outstandingRefundableCount == 0 && outstandingRefundablePrincipal == 0`.
4. `commitMigration(V2)`, wait 48h, guardian window open throughout.
5. `finalizeMigration` — the whole reserve moves to V2; V1 is terminal.
6. Re-point `[robinhood.indexer]` / `[robinhood.settlement]`
   `bridge_contract`, re-run `glc-admin robinhood-preflight`, re-enable
   routes.

Because the reserve is currently `status: "disabled"` and
`reserve_configured: false`, doing this **before** launch costs nothing
but time. Doing it after launch costs a full migration. That asymmetry is
the single most actionable finding in this document.

**Nothing was deployed, and no deployment is proposed here.**

---

## 4. The parity design, for when the entry point exists

Recorded now so the contract change ships with a known off-chain
counterpart. None of it is implemented.

### 4.1 Ledger

A **fourth** `RobinhoodTxKind::TreasuryWithdraw`, not a reuse of `Payout`
or `Refund`. Those rows are keyed to a `request_id` of a bridge request
and carry fee accounting; a withdrawal has neither. Schema v24 adds the
kind to the existing CHECK constraints and to `kind.action()`.

The existing `RobinhoodTxState` machine is already the right one and needs
no new states:

```
Authorizing → Authorized → Signed → Broadcast → Included → Finalized
                                          ↘ Reverted → ManualReview
```

`Authorizing` is written **before the first signer is asked**, so a crash
mid-collection resumes the same payload. `Signed` is where the nonce is
allocated, by the ledger's own maximum inside the same write transaction
that stores it. `is_in_flight()` (`Broadcast | Included`) is what forbids
replacing a committed nonce with different bytes.

### 4.2 Nonce / idempotency

Identical to the payout and refund paths, which is the point of reusing
them: one operation row, one nonce, allocated once, persisted before
broadcast. A re-run resumes the same row — `Authorizing` re-collects,
`Signed` re-broadcasts the *identical bytes*, `Broadcast`/`Included`
polls the receipt. A revert is terminal and is **not** retried under a
fresh nonce; it goes to `ManualReview` by explicit decision.

### 4.3 Exit status and machine-readable output

`glc-admin robinhood-treasury-withdraw --config PATH --amount-glc N [--execute] [--json]`:

- dry run by default — writes nothing, signs nothing, broadcasts nothing,
  allocates no nonce;
- `--execute` re-runs every check against fresh state, verifies the
  **authoritative on-chain pause** by `eth_call` (never a
  `--confirm-paused` flag), gathers 2-of-3 EIP-712 authorization through
  `signing::evm_policy` (structured payload, never a raw digest), calls
  `eth_call` to simulate, broadcasts only on success, then drives
  `tick_receipts` to finality;
- **exit 0 only on `Finalized`**; non-zero on `Reverted`, `ManualReview`,
  or any unresolved in-flight state;
- `--json` emits `{operation_id, state, tx_hash, nonce, amount_atomic,
  amount_glc, destination, receipt_status, confirmations,
  required_confirmations, success, failure_reason, reserve_before,
  reserve_after, encumbered, protected_minimum}`.

Note this is **stricter than `glc-treasury-withdraw` (Solana) is today**;
§1.4. Solana should be brought up to it, which is a separate change to a
separate binary and is not made here.

### 4.4 Amounts

18dp atomic at the contract boundary, always. The low-level path speaks
`RobinhoodAtomic` (`u128`, no arithmetic against `CanonicalAtomic`,
enforced by the type system). A `--amount-glc` wrapper converts through
the existing exactness-checked
`RobinhoodAtomic::from_canonical`, rejects any value that is not an exact
multiple of `CANONICAL_SCALE = 1e10`, and prints both the human GLC figure
and the 18dp atomic figure before execution. `service/src/amount_conversion/robinhood.rs`
already implements all of this and needs no change.

### 4.5 Reserve safety

`_requireSpendableReserve` on chain; off chain, refuse unless
`balanceOf(bridge) − amount ≥ protectedMinReserve + outstandingRefundablePrincipal`,
read live via `BridgeReader::encumbered_reserve` and
`TokenReader::balance_of`, plus the ledger's own
`RobinhoodReserve` bands. Both, independently, because the Solana path
checks locally *and* on chain for the same reason.

---

## 5. Solana vs Robinhood — what cannot be identical

| aspect | Solana | Robinhood | why |
|--------|--------|-----------|-----|
| authorization | ed25519 attestation in a preceding precompile instruction | 2-of-3 EIP-712 signatures passed as `bytes[]` | different VMs; no precompile-instruction pattern on EVM |
| replay guard | `init` on a per-nonce PDA (account creation) | `_executedRequest[keccak(action,requestId)]` mapping | no PDAs on EVM |
| destination policy | mutable allowlist behind a governance timelock | **immutable** `TREASURY` recommended | EVM has no cheap timelocked-governance primitive here, and an immutable is strictly stronger |
| pause scope | one global `config.paused` | two flags, `depositsPaused` + `payoutsPaused`, both required | the contract has no single global pause |
| amount unit | 8dp (canonical) `u64` | 18dp `u128` | token decimals |
| finality | slot confirmation | receipt `status` + confirmation depth | different chains |
| stage separation | three binaries/hosts (`plan`/`attest`/`execute`) | one command driving remote signers | Robinhood signers are already remote HTTP/KMS endpoints; there is no local-key stage to separate out |

---

## 6. Pause requirement (authoritative)

For a Robinhood reserve withdrawal, the required state is
**`depositsPaused == true` AND `payoutsPaused == true`, read live from the
contract by `eth_call`** — the same pair `commitMigration` already
demands, and the closest available analogue to Solana's global
`config.paused`.

The local ledger-side `RobinhoodReserve.paused` gate
(`glc-admin robinhood-local-pause`) is **not** sufficient and must not be
treated as the requirement: it gates `GlcToRhn` admission in this service
and has no bearing on what the contract will accept.

No `--confirm-paused` flag. The check is mandatory, on-chain, and
re-evaluated at execution time.

---

## 7. What was actually changed

Since the executable path does not exist, the implementation is confined
to making the non-existence enforced rather than merely true.

**`glc-admin rebalance-propose --direction` still accepts only
`goldcoin|solana`.** Per the brief, `robinhood` is added *only once the
executable path genuinely exists*.

### 7.1 What was already true

`rebalance_requests.direction` carries

```sql
CHECK (direction IN ('GoldcoinReserve','SolanaReserve'))
```

and schema v23 — the migration that introduced Robinhood — widened
`reserve_ledger`'s *identical* CHECK to admit `RobinhoodReserve` while
pointedly leaving this one alone. The Robinhood reserve is **accounted**,
never **rebalanced** through this table. That predates this work and is
the strongest layer present; it was discovered during implementation, not
assumed.

### 7.2 What was added

1. **`Ledger::propose_rebalance`** now refuses
   `(RobinhoodReserve, Withdraw)` with the named
   `LedgerError::RobinhoodWithdrawalNotExecutable` *before* reaching the
   CHECK. This is not redundancy with §7.1; it does two things the CHECK
   cannot:
   - it refuses with a **reason** — an operator hitting
     `SqliteFailure(ConstraintViolation)` learns nothing about the
     on-chain blocker;
   - it **survives the migration that lifts the CHECK**. Widening that
     constraint is a plausible, well-intentioned edit, and v23 is the
     precedent for doing exactly that to a sibling table. If it happens
     before the contract gains a withdrawal entry point, the withdraw path
     must stay closed.

   Scoped to `Withdraw` deliberately: a `Deposit` is refused by the CHECK
   for a reason that is about this table's scope, not about the contract,
   and attaching the contract explanation to it would put a wrong reason
   on a right answer.

2. **`glc-admin`'s `parse_rebalance_direction`** names `robinhood`
   explicitly and refuses it with the on-chain reason and a pointer here,
   rather than letting it fall through to "unknown direction" — which
   reads like an unbuilt parser arm and invites someone to go add it.
   `pause`/`unpause`/`*-admission` keep their own
   `parse_reserve_direction`, whose right answer for `robinhood` is
   `robinhood-local-pause` — a different question, which is why they are
   two functions.

3. **`admin_api`'s `parse_rebalance_direction`** does the same for
   `POST /rebalances`, returning 400 with the reason instead of a 409 from
   deeper down.

The state all three exist to prevent: `propose_rebalance` →
`approve_rebalance` → `record_rebalance_executed` accepting a
`RobinhoodReserve` `Withdraw` and marking it `Executed` against an
operator-supplied `tx_reference` that cannot correspond to any real
withdrawal. That row would assert a reserve movement the chain never
performed, and reconciliation compares the ledger against
`balanceOf(bridge)`.

Tests: `service/tests/robinhood_rebalance_direction.rs` (12).

### Untouched

`GlcToRhn` payouts, `RhnToGlc` refunds, Robinhood settlement, the local
Robinhood pause, all Solana reserve-withdrawal code, and every contract
file. No production state was read or written; nothing was deployed.

---

## 8. Recommended next steps

1. **Decide whether a Robinhood reserve withdrawal is required at all.**
   With `SolToRhn`/`RhnToSol` disabled and the route pre-launch, the
   reserve's only outflows are user payouts and refunds. A treasury
   withdrawal is an operational-recovery capability, not a bridge
   function.
2. If it is required, **add `executeTreasuryWithdraw` before launch**
   (§3), while redeployment is free.
3. Independently of Robinhood, **fix `glc-treasury-withdraw`'s exit
   contract** (§1.4): exit 0 currently means "broadcast", not
   "confirmed", and there is no machine-readable result. Robinhood's
   settlement state machine is the model to copy.
