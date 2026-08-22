# Reserve Emergency / Rebalance Withdrawal Runbook

This is the operator procedure for an intentional, authorized withdrawal of
reserve assets on either chain — the mechanism `RESERVE_CUSTODY_AND_WITHDRAWAL.md`
found missing and this round of work implements. Read that document first
for the full custody model this procedure sits on top of.

**Scope note, updated:** this bridge now has a turnkey CLI on both chains —
`glc-rebalance-withdraw-solana` (Solana) alongside `glc-rebalance-withdraw`
(Goldcoin, unchanged from the previous round). Neither requires
hand-assembling a transaction. Both stage the same way: build/verify with
no key -> collect threshold authorization -> simulate/assemble and broadcast
only with an explicit `--execute` flag.

---

## 1. Prerequisites (both chains)

- The bridge's global pause must already be engaged:
  `glc-admin onchain-pause --rpc-url URL --keypair ADMIN_KEY --scope global --note "reserve rebalance withdrawal"`.
  Both new withdrawal paths refuse to proceed if the bridge is not paused —
  this is enforced in code, not just documented (`BridgeError::BridgeNotPaused`
  on the Solana side; `--confirm-paused` plus an optional live on-chain
  check on the Goldcoin CLI side).
- A real, agreed destination for the withdrawn funds — decided and recorded
  *before* this procedure starts, not chosen ad hoc mid-withdrawal.
- Enough of the relevant threshold's signers available and reachable: 2 of
  the 3 Solana attestation signers, and/or 2 of the 3 Goldcoin vault
  signers, depending on which chain's reserve is being withdrawn from.
- `docs/22-production-readiness-review.md` P0-6's approved pilot parameters
  at hand, so nobody has to recall `protected_minimum`/thresholds from
  memory.

## 2. Solana side — `glc-rebalance-withdraw-solana`

Three staged subcommands — `plan -> attest -> execute` — mirroring the
Goldcoin tool's shape, so no single invocation ever needs every credential
at once. See the tool's own module docs
(`service/src/bin/glc-rebalance-withdraw-solana.rs`) for the full
reasoning.

### Authorization requirements

Two independent factors, both required — see
`programs/glc-reserve-bridge/src/instructions/rebalance_withdraw.rs` module
docs for the full reasoning:

1. **Admin's signature** on the transaction (accountability — but alone,
   authorizes nothing).
2. **A threshold (2-of-3 pilot) ed25519 attestation proof** over the
   canonical `glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message`
   (protocol version, program id, attestation epoch, nonce, amount,
   destination token account, reserve mint — 138 bytes, action byte
   `0x03`, distinct from a release claim or a completion claim so no
   signature can be confused across the three), collected from the
   existing production remote-signer endpoints — **no attestation private
   key ever exists on the machine running this tool.**

### Planning procedure (`plan` — no key needed; this step IS the dry run)

```
glc-rebalance-withdraw-solana plan \
    --rpc-url https://api.mainnet-beta.solana.com \
    --destination DESTINATION_TOKEN_ACCOUNT_PUBKEY \
    --amount 5000000000 --nonce 1 \
    --reserve-mint Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump \
    --token-program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb \
    --out plan.json
```

Reads live on-chain `BridgeConfig`/`AttestationKeySet`, verifies the
reserve mint/token program (cross-checked against `--reserve-mint`/
`--token-program` if supplied — both optional, but recommended), verifies
the bridge is globally paused, verifies the destination account's mint and
owning program, verifies withdrawing `--amount` would not breach
`protected_minimum`, verifies `--nonce` has not already been used, derives
(never accepts as input) the reserve authority PDA and reserve token
account, and writes `plan.json`. Prints every address, the amount, the
reserve balance before/after, and the attestation threshold. `--nonce` is
operator-chosen and is the replay guard — pick a fresh one per withdrawal
(a monotonic counter, or the current Unix timestamp).

### Authorization procedure (`attest` — no local private key)

```
glc-rebalance-withdraw-solana attest \
    --plan plan.json --rpc-url https://api.mainnet-beta.solana.com \
    --attestation-signer ATTESTATION_PUBKEY_1,https://signer1.example/,ATTESTATION_SIGNER_1_TOKEN \
    --attestation-signer ATTESTATION_PUBKEY_2,https://signer2.example/,ATTESTATION_SIGNER_2_TOKEN \
    --out attested-plan.json
```

Re-verifies the plan file has not been tampered with (recomputes the PDAs
and claim message from the plan's own recorded nonce/amount/destination/
mint/epoch) and that live chain state still supports it, then contacts
each `--attestation-signer` (repeat once per signer, `>=` threshold)
through the existing production remote-signer client
(`RemoteAttestationSigner`/`https://.../v1/identity`+`/v1/sign` protocol —
`service/src/signing/remote.rs`) — the same architecture the bridge's own
automated release path already uses. Each endpoint's identity is verified
before it is trusted, and every returned signature is verified locally
before being written to `attested-plan.json`.

### Execution procedure (`execute` — needs only admin + submitter keypairs)

```
glc-rebalance-withdraw-solana execute \
    --attested-plan attested-plan.json --rpc-url https://api.mainnet-beta.solana.com \
    --admin-keypair /path/to/admin-keypair.json \
    --submitter-keypair /path/to/submitter-keypair.json
```

Re-verifies live state a third time, builds the ed25519-proof +
`rebalance_withdraw` transaction, and **always simulates it** — prints the
full transaction summary (every address, amount, reserve balance
before/after, signer count, admin/submitter SOL balances, estimated fee/
rent) and the simulation result (success/failure + program logs) before
ever considering a broadcast. Without `--execute`, this is the full
dry-run/simulation step — nothing is sent. Add `--execute` only once the
printed summary and simulation output have been reviewed:

```
glc-rebalance-withdraw-solana execute \
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
- **Solana `rebalance_withdraw` transaction is broadcast but fails to
  confirm**: no state changed (Solana transactions are atomic) — the
  `rebalance_withdrawal` nonce is NOT consumed; re-run `execute` (same
  attested plan) or start over with the same or a fresh nonce.
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
