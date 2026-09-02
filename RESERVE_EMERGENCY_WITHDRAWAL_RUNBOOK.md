# Reserve Emergency / Rebalance Withdrawal Runbook

This is the operator procedure for an intentional, authorized withdrawal of
reserve assets on either chain — the mechanism `RESERVE_CUSTODY_AND_WITHDRAWAL.md`
found missing and this round of work implements. Read that document first
for the full custody model this procedure sits on top of.

**Scope note, updated:** this bridge now has a turnkey CLI on both chains —
`glc-treasury-withdraw` (Solana) alongside `glc-rebalance-withdraw`
(Goldcoin, unchanged from the previous round). Neither requires
hand-assembling a transaction. Both stage the same way: build/verify with
no key -> collect threshold authorization -> simulate/assemble and broadcast
only with an explicit `--execute` flag.

---

## 0. READ THIS FIRST — what changed on 2026-09-02, and why

The Solana tool was called `glc-rebalance-withdraw-solana` and took a
`--destination PUBKEY` naming any token account of the reserve mint. **That
command no longer exists and that flag no longer exists.**

On 2026-09-02 someone with access to an authenticated production shell used
exactly this procedure, exactly as written, to withdraw reserve funds to an
account they controlled. They had the admin keypair and the attestation
signer credentials because both were resident on the bridge host. Every
control worked; the signatures were genuine. The destination was simply
theirs. Full analysis:
[docs/29-reserve-withdrawal-hardening.md](docs/29-reserve-withdrawal-hardening.md).

Three things are different now:

1. **The destination comes from the chain, not from you.** The on-chain
   `RebalancePolicy` holds an allowlist of treasury token accounts. Changing
   it requires a threshold of attestation keys plus a public timelock, and
   cannot be done from the bridge host at all. `glc-treasury-withdraw` has
   no `--destination`; with one allowlisted treasury (the production
   posture) it needs no destination input whatsoever.
2. **There is a limit.** A dedicated rolling budget across a fixed window,
   governed by threshold attestation plus the timelock. It is the ONLY
   amount restriction: a single withdrawal may consume the entire
   remaining budget. `BridgeConfig.per_transfer_limit`
   never applied to this path and still does not; these are separate fields
   in a separate account that the admin key cannot edit.
3. **The stages run on different machines.** `attest` belongs on the
   approval host, and the credentials that reach the attestation signers
   belong only there. See
   [docs/28-signer-policy.md](docs/28-signer-policy.md) for the signer-side
   policy that makes a stolen bridge-host credential insufficient even if
   this separation is violated.

If you type `glc-rebalance-withdraw-solana` and get "command not found",
that is this document working. Read §2.

---

## 1. Prerequisites (both chains)

- The bridge's global pause must already be engaged:
  `glc-admin onchain-pause --rpc-url URL --keypair ADMIN_KEY --scope global --note "reserve rebalance withdrawal"`.
  Both new withdrawal paths refuse to proceed if the bridge is not paused —
  this is enforced in code, not just documented (`BridgeError::BridgeNotPaused`
  on the Solana side; `--confirm-paused` plus an optional live on-chain
  check on the Goldcoin CLI side).
- **Solana:** the destination is not yours to choose — it must already be in
  the on-chain `RebalancePolicy` allowlist. Confirm what is allowlisted, and
  the current limits and remaining rolling budget, before you start:
  `glc-admin rebalance-policy-show --rpc-url URL`. If the treasury you
  intend to use is not listed, that is a governance action (threshold
  attestation + timelock), not something to resolve during a withdrawal.
- **Goldcoin:** a real, agreed destination for the withdrawn funds — decided
  and recorded *before* this procedure starts, not chosen ad hoc
  mid-withdrawal. (The Goldcoin side has no on-chain allowlist; this remains
  an operator discipline there, and is tracked as follow-up work.)
- Know who holds what: `glc-admin show-authorities --rpc-url URL` prints
  `BridgeConfig.admin`, any pending handover, and the program's real upgrade
  authority. If it warns that those are the same key, stop and escalate —
  see docs/29 §8.
- Enough of the relevant threshold's signers available and reachable: 2 of
  the 3 Solana attestation signers, and/or 2 of the 3 Goldcoin vault
  signers, depending on which chain's reserve is being withdrawn from.
- `docs/22-production-readiness-review.md` P0-6's approved pilot parameters
  at hand, so nobody has to recall `protected_minimum`/thresholds from
  memory.

## 2. Solana side — `glc-treasury-withdraw`

Three staged subcommands — `plan -> attest -> execute` — mirroring the
Goldcoin tool's shape, so no single invocation ever needs every credential
at once. **Run them on two different machines** (see §0): `plan` and
`execute` on the bridge host, `attest` on the approval host. See the tool's
own module docs (`service/src/bin/glc-treasury-withdraw.rs`) for the full
reasoning.

### Authorization requirements

Four independent requirements, all enforced on chain — see
`programs/glc-reserve-bridge/src/instructions/treasury_withdraw.rs` module
docs for the full reasoning:

1. **Admin's signature** on the transaction (accountability — but alone,
   authorizes nothing).
2. **A threshold (2-of-3 pilot) ed25519 attestation proof** over the
   canonical `glc_reserve_bridge_shared::claim::treasury_withdraw_claim_message`
   (protocol version, program id, attestation epoch, nonce, amount,
   destination, reserve mint, source reserve token account, and the
   `RebalancePolicy.version` — 178 bytes, action byte `0x05`, distinct in
   both action and length from every other claim family so no signature can
   be confused across them), collected from the existing production
   remote-signer endpoints — **no attestation private key ever exists on
   any machine running this tool.**
3. **An allowlisted destination.** The destination must appear verbatim in
   the on-chain `RebalancePolicy`. This is the control that would have
   stopped the 2026-09-02 incident: it depends on no host, no credential and
   no decision made at withdrawal time.
4. **Within the budget.** The withdrawal must not take the current
   window's total past the policy's rolling budget. There is no separate
   per-withdrawal ceiling.

Requirements 1 and 2 were both satisfied during the incident. Do not treat
them as sufficient.

### Planning procedure (`plan` — no key needed; this step IS the dry run)

```
glc-treasury-withdraw plan \
    --rpc-url https://api.mainnet-beta.solana.com \
    --amount 5000000000 --nonce 1 \
    --reserve-mint Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump \
    --token-program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb \
    --out plan.json
```

There is no `--destination`. With exactly one allowlisted treasury — the
production posture — the tool uses it. If the allowlist ever holds more than
one (a staged treasury rotation), add `--treasury PUBKEY` to say which; it is
checked against the allowlist, not trusted.

Reads live on-chain `BridgeConfig`/`AttestationKeySet`/`RebalancePolicy`,
verifies the reserve mint/token program (cross-checked against
`--reserve-mint`/`--token-program` if supplied — both optional, but
recommended), verifies the bridge is globally paused, verifies the
destination is allowlisted, verifies the amount is within the remaining
rolling budget, verifies the destination account's
mint and owning program, verifies withdrawing `--amount` would not breach
`protected_minimum`, verifies `--nonce` has not already been used and is not
in the refund namespace, derives (never accepts as input) the reserve
authority PDA and reserve token account, and writes `plan.json`. Prints
every address, the amount, the reserve balance before/after, the policy
version and allowlist, both limits, and the attestation threshold.

`--nonce` is operator-chosen and is the replay guard — pick a fresh one per
withdrawal (a monotonic counter, or the current Unix timestamp). It must be
below 2^63: the high half of the nonce space is reserved for ManualReview
refunds, and the program now enforces that split.

If the policy account does not exist at all, this step refuses and says so.
That is the safe state, not a broken one — no allowlisted destination means
no authorized withdrawal.

### Authorization procedure (`attest` — no local private key)

**Run this on the approval host.** The credentials named by
`--attestation-signer` must not exist on the bridge host; that co-residency
is what made the incident possible.

```
glc-treasury-withdraw attest \
    --plan plan.json --rpc-url https://api.mainnet-beta.solana.com \
    --attestation-signer ATTESTATION_PUBKEY_1,https://signer1.example/,ATTESTATION_SIGNER_1_TOKEN \
    --attestation-signer ATTESTATION_PUBKEY_2,https://signer2.example/,ATTESTATION_SIGNER_2_TOKEN \
    --out attested-plan.json
```

Re-verifies the plan file has not been tampered with (recomputes the PDAs
and claim message from the plan's own recorded nonce/amount/destination/
mint/epoch/policy-version), that live chain state still supports it, and
that the live `RebalancePolicy` is still at the version the plan was built
under — if governance has changed the allowlist or the limits since `plan`
ran, the attestations would be void the moment they were signed, so it
refuses rather than troubling three custody domains. Then contacts each
`--attestation-signer` (repeat once per signer, `>=` threshold) through the
existing production remote-signer client
(`RemoteAttestationSigner`/`https://.../v1/identity`+`/v1/sign` protocol —
`service/src/signing/remote.rs`) — the same architecture the bridge's own
automated release path already uses. Each endpoint's identity is verified
before it is trusted, and every returned signature is verified locally
before being written to `attested-plan.json`.

A signer running the policy in [docs/28-signer-policy.md](docs/28-signer-policy.md)
will independently refuse a destination it has not separately agreed to. If
one refuses, do not route around it — find out why.

### Execution procedure (`execute` — needs only admin + submitter keypairs)

```
glc-treasury-withdraw execute \
    --attested-plan attested-plan.json --rpc-url https://api.mainnet-beta.solana.com \
    --admin-keypair /path/to/admin-keypair.json \
    --submitter-keypair /path/to/submitter-keypair.json
```

Re-verifies live state a third time — including the policy version and the
allowlist once more — builds the ed25519-proof + `treasury_withdraw`
transaction, and **always simulates it** — prints the
full transaction summary (every address, amount, reserve balance
before/after, signer count, admin/submitter SOL balances, estimated fee/
rent) and the simulation result (success/failure + program logs) before
ever considering a broadcast. Without `--execute`, this is the full
dry-run/simulation step — nothing is sent. Add `--execute` only once the
printed summary and simulation output have been reviewed:

```
glc-treasury-withdraw execute \
    --attested-plan attested-plan.json --rpc-url https://api.mainnet-beta.solana.com \
    --admin-keypair /path/to/admin-keypair.json \
    --submitter-keypair /path/to/submitter-keypair.json \
    --execute
```

A failed simulation blocks the broadcast outright, even with `--execute`
supplied — this is enforced in code (`execute_withdrawal`'s own control
flow, covered by the `simulation_failure_blocks_broadcast_even_with_execute_flag`
test), not left to operator discipline alone.

### Verification

- `execute`'s printed signature; poll it for confirmation same as any
  other Solana transaction (`solana confirm SIGNATURE`, or
  `getSignatureStatuses`).
- Reserve token account balance decreased by exactly `amount`; destination
  token account balance increased by exactly `amount` — both already
  printed as "before"/"after" by `execute`, confirm against the real
  post-confirmation balances.
- `rebalance_withdrawal` PDA now exists (the replay guard/audit record) —
  read it directly via `getAccountInfo` on the derived PDA if you need the
  recorded `nonce`/`amount`/`destination`/`admin` fields.
- `protected_minimum` was preserved automatically (the instruction cannot
  execute otherwise) — no separate check needed, but worth confirming the
  resulting balance against `docs/09-runbook.md`'s reserve-bounds table for
  operational awareness.

## 3. Goldcoin side — `glc-rebalance-withdraw`

Three separate subcommands, run by (potentially) three separate people on
three separate machines — see the tool's own module docs
(`service/src/bin/glc-rebalance-withdraw.rs`) for why a single monolithic
command was deliberately not built.

### Dry-run procedure (`plan` — needs no key)

```
glc-rebalance-withdraw plan \
    --rpc-url URL --rpc-user USER --rpc-password PASS \
    --vault-pubkeys HEX1,HEX2,HEX3 --vault-threshold 2 --network mainnet \
    --destination GOLDCOIN_ADDRESS --amount-atomic N \
    --fee-rate-per-kb 100000 --dust-threshold 1000 --max-inputs 10 \
    --min-confirmations 20 --out plan.json
```

Queries live vault UTXOs, selects inputs, builds and self-verifies the
unsigned transaction (`payout::verify_payout_tx`), and writes `plan.json`.
Prints a human-readable summary (inputs, amount, change, fee, and the
conservation identity `inputs == payout + change + fee`). **Nothing is
signed or broadcast at this step** — distribute `plan.json` to every vault
signer for independent review before anyone signs.

### Authorization / signing procedure (`sign` — one key per invocation)

Each of at least `vault_threshold` (2 of 3, pilot) signers, independently,
on their own machine:

```
glc-rebalance-withdraw sign --plan plan.json --key-path MY_VAULT_KEY.hex --out partial-1.json
```

`sign` re-verifies the plan's conservation property and that the plan
file's recorded transaction bytes actually match its own recorded
inputs/destination/amount **before** producing a signature — it refuses to
sign a plan that doesn't check out, and refuses a plan whose recorded
pubkey isn't actually one of the vault's configured signers. Exchange the
resulting `partial-N.json` files out of band (however this bridge's
custody domains already communicate — the file contains only a public key
and signatures, never key material).

### Execution procedure (`broadcast`)

```
glc-rebalance-withdraw broadcast \
    --plan plan.json --partials partial-1.json,partial-2.json \
    --rpc-url URL --rpc-user USER --rpc-password PASS \
    --confirm-paused --solana-rpc-url SOLANA_URL \
    [--execute]
```

Without `--execute`: assembles and prints the fully-signed transaction hex
and its txid — still a dry run, nothing sent. With `--execute`: also
broadcasts via `sendrawtransaction`. `--confirm-paused` is mandatory
regardless; `--solana-rpc-url`, if supplied, additionally reads the live
on-chain `BridgeConfig.paused` and refuses to proceed if it is not
actually `true` — do not rely on `--confirm-paused` alone if this flag is
available to you.

### Verification

- `broadcast`'s printed txid; poll `goldcoin-cli gettransaction <txid>` (or
  `listunspent` on the destination) until it reaches
  `vault_min_confirmations` (approved pilot value: 20).
- Vault's remaining solvable balance (`goldcoin-cli listunspent <N> 9999999 '["<vault address>"]'`)
  decreased by exactly `amount + fee` (fee leaves the vault; change, if
  any, returns to it).

## 4. Reconciliation after withdrawal (both chains)

- Record the withdrawal as evidence in the existing off-chain ledger so
  reconciliation doesn't misclassify the change as an unexplained breach:
  `glc-admin rebalance-propose --db PATH --direction <goldcoin|solana> --kind withdraw --amount N --by IDENTITY --required-approvals N --note "..."`,
  collect `rebalance-approve`s, then
  `glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TXID_OR_SIGNATURE`,
  then `glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N`
  once independently confirmed on-chain/on-Goldcoin.
- Cross-check the new on-chain/on-vault balance against
  `docs/09-runbook.md`'s reserve-bounds table (`target_reserve`/
  `warning_reserve`/`critical_reserve`/`protected_minimum`) — a large
  withdrawal may cross a band and should trigger the corresponding
  operator response from that table.
- Update whatever external accounting/audit record tracks total reserve
  value under management.

## 5. Rollback / failure handling

- **Solana `execute` fails at simulation**: nothing is broadcast (enforced
  in code, not just by convention) — read the printed simulation error and
  program logs, fix the cause (stale attestation epoch, insufficient
  signatures, wrong token program, bridge no longer paused, etc. — see the
  instruction's own error list and `execute`'s own live-state re-checks),
  and re-run `execute` (or re-run `plan`/`attest` from scratch if the
  underlying parameters need to change).
- **Solana `treasury_withdraw` transaction is broadcast but fails to
  confirm**: no state changed (Solana transactions are atomic) — the
  `rebalance_withdrawal` nonce is NOT consumed and the rolling budget is
  NOT charged; re-run `execute` (same attested plan) or start over with the
  same or a fresh nonce.
- **Solana withdrawal fails with `DestinationNotAllowlisted`**: the
  destination is not in the on-chain `RebalancePolicy`. This is not
  something to work around from the bridge host — it cannot be. Either the
  intended treasury genuinely is not allowlisted (a governance action:
  threshold attestation + timelock, see
  docs/29-reserve-withdrawal-hardening.md §4), or someone is attempting a
  withdrawal that should not happen. Treat an unexpected occurrence as an
  incident.
- **Solana withdrawal fails with `ExceedsRebalancePerWithdrawalLimit` or
  `ExceedsRebalanceRollingLimit`**: the amount is above the governed
  ceiling, or this window's budget is spent. Reduce the amount, wait for the
  window to age out (`glc-admin rebalance-policy-show` prints the remaining
  budget), or raise the limit through governance. `glc-admin
  reset-rolling-window` does **not** apply here — it only touches the two
  settlement directions, deliberately.
- **Solana withdrawal fails with `RebalanceWithdrawRetired`**: you are
  running pre-2026-09-02 tooling, or replaying an old transaction. The
  unrestricted withdrawal instruction is gone; use `glc-treasury-withdraw`.
- **Goldcoin `plan` becomes stale** (a selected UTXO gets spent by
  something else before signing completes, or vault balance changes):
  `sign`'s own re-verification of `unsigned_tx_hex` against the plan
  file's recorded inputs will still succeed (it only checks internal
  consistency, not current chain state) — but `broadcast`'s eventual
  `sendrawtransaction` will fail if an input is already spent, in which
  case the transaction never confirms, no funds move, and no assembled
  transaction can be replayed with stale inputs. Re-run `plan` fresh.
- **`broadcast` without `--execute` shows something wrong**: nothing has
  been sent — discard the plan/partial files, fix the input parameters,
  and start over from `plan`.
- **Partial signatures don't reach threshold**: `broadcast` refuses to
  assemble at all (checked explicitly before touching the network) — no
  partial transaction can ever be broadcast accidentally.
- Any of the above: nothing about the bridge's normal pause/settlement
  state is affected — un-pause (`glc-admin onchain-unpause --scope global`)
  only once the operator is satisfied the withdrawal is fully resolved
  (succeeded and reconciled, or abandoned and confirmed not to have moved
  anything).

## 6. Withdrawing the full reserve, if ever needed

Both mechanisms **preserve `protected_minimum`** exactly like normal
bridge settlement does — this is deliberate (see
`RESERVE_CUSTODY_AND_WITHDRAWAL.md`'s recommendations), not an oversight.
A withdrawal that would take the reserve below `protected_minimum` is
rejected — on the Solana side both client-side (`plan`/`attest`/`execute`
all independently check this) and on-chain (`InsufficientReserveBalance`
— the instruction cannot execute otherwise even if every client-side check
were somehow bypassed); the Goldcoin CLI has no on-chain floor to enforce
(Goldcoin has no program layer) but `plan` will simply fail coin selection
if the requested amount exceeds what the vault actually holds.

**Withdrawing up to the current usable amount (reserve balance minus
`protected_minimum`)** needs no special procedure — just pass that exact
amount to `plan`/`glc-rebalance-withdraw plan` as usual; `execute`'s
printed "reserve balance (after)" line will show it landing exactly at
`protected_minimum`.

**To withdraw genuinely everything** (e.g., decommissioning the pilot
entirely), lower `protected_minimum` first, deliberately and visibly:

```
glc-admin onchain-... # (admin-gated set_limit; on-chain field ProtectedMinimum, new_value 0)
```

using the existing `set_limit(ProtectedMinimum, 0)` instruction (already
built, already emits `LimitsChanged`, already audited by this session's
`RESERVE_CUSTODY_AND_WITHDRAWAL.md`) — then run the withdrawal procedures
above (Solana: `plan`/`attest`/`execute` with `--amount` equal to the full
reserve balance; Goldcoin: `plan`/`sign`/`broadcast` with
`--amount-atomic` equal to the full solvable vault balance minus the
intended fee). Restore `protected_minimum` to its approved value
afterward if the reserve is not actually being fully decommissioned.
