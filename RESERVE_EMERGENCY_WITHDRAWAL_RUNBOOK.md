# Reserve Emergency / Rebalance Withdrawal Runbook

This is the operator procedure for an intentional, authorized withdrawal of
reserve assets on either chain — the mechanism `RESERVE_CUSTODY_AND_WITHDRAWAL.md`
found missing and this round of work implements. Read that document first
for the full custody model this procedure sits on top of.

**Scope note, stated plainly:** this round adds (1) a real, tested on-chain
Solana instruction (`rebalance_withdraw`) and (2) a real, tested Goldcoin
operator CLI (`glc-rebalance-withdraw`). It does **not** add a turnkey CLI
that submits the Solana `rebalance_withdraw` transaction — that would
require its own signer-coordination tooling (analogous to
`glc-rebalance-withdraw`'s plan/sign/broadcast split, but for Solana ed25519
signatures instead of Goldcoin ECDSA ones) which is real, separate,
untested-in-this-round work. Section 2 below gives the exact instruction
shape so a real invocation can be constructed precisely today, and names
what a future CLI would need to add.

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

## 2. Solana side — `rebalance_withdraw`

### Authorization requirements

Two independent factors, both required — see
`programs/glc-reserve-bridge/src/instructions/rebalance_withdraw.rs` module
docs for the full reasoning:

1. **Admin's signature** on the transaction (accountability — but alone,
   authorizes nothing).
2. **A threshold (2-of-3 pilot) ed25519 attestation proof**, in the
   instruction immediately preceding `rebalance_withdraw` in the same
   transaction, over the canonical
   `glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message`
   (protocol version, program id, attestation epoch, nonce, amount,
   destination token account, reserve mint — 138 bytes, action byte
   `0x03`, distinct from a release claim or a completion claim so no
   signature can be confused across the three).

### Exact instruction shape

```
Instruction: rebalance_withdraw(nonce: u64, amount: u64, attestation_epoch: u64)

Accounts:
  admin                     [signer, mut]  — must equal BridgeConfig.admin
  bridge_config             [mut]          — PDA ["bridge_config"]
  attestation_key_set                      — PDA ["attestation_key_set"]
  rebalance_withdrawal      [mut, init]    — PDA ["rebalance_withdrawal", nonce.to_le_bytes()]
  reserve_mint                             — must equal BridgeConfig.reserve_token_mint
  reserve_authority                        — PDA ["reserve_authority"]
  reserve_token_account     [mut]          — ATA(reserve_authority, reserve_mint)
  destination_token_account [mut]          — ANY real token account for reserve_mint
                                              (need not be an ATA of any particular wallet)
  instructions_sysvar                      — Sysvar1nstructions1111111111111111111111111
  token_program                            — must equal BridgeConfig.reserve_token_program
  system_program

Preceding instruction (same transaction, index -1 relative to rebalance_withdraw):
  ed25519 precompile instruction, >= attestation_key_set.threshold unique
  current attestation-key signatures over rebalance_withdraw_claim_message.
```

`nonce` is operator-chosen and is the replay guard — reusing a nonce fails
closed at account creation (`rebalance_withdrawal` PDA `init`). Pick a fresh
one per withdrawal (e.g. a monotonic counter tracked alongside the audit
log below, or the current Unix timestamp).

### Building this today (no dedicated CLI yet)

Until a dedicated tool exists (see the scope note above), construct this
with any Solana transaction-building tooling capable of: deriving the
accounts above, calling
`glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message` with
the real parameters, collecting `attestation_threshold` real signatures
over that exact message from the attestation signers' own key material
(never a locally-fabricated one), building the ed25519 precompile
instruction (`solana_sdk::ed25519_instruction::new_ed25519_instruction`-
style helper, or the same construction
`programs/glc-reserve-bridge/tests/common/mod.rs::ed25519_proof_ix` uses
for tests — same layout, real keys instead of test keys), and submitting
both instructions in one transaction with `admin` and `submitter`/fee-payer
signing.

### Verification

- Transaction confirms; `rebalance_withdrawal` PDA now exists with the
  correct `nonce`/`amount`/`destination`/`admin` (`glc-admin show-config`
  does not yet print this account — read it directly via
  `getAccountInfo` on the derived PDA and decode with
  `RebalanceWithdrawal::try_deserialize`, or add a small read script).
- `RebalanceWithdrawalExecuted` event emitted (advisory — cross-check
  against the PDA, which is authoritative).
- Reserve token account balance decreased by exactly `amount`; destination
  token account balance increased by exactly `amount`.
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

- **Solana `rebalance_withdraw` transaction fails to confirm**: no state
  changed (Solana transactions are atomic) — the `rebalance_withdrawal`
  nonce is NOT consumed; retry with the same nonce, or a fresh one, once
  the failure cause (stale attestation epoch, insufficient signatures,
  wrong token program, etc. — see the instruction's own error list) is
  fixed.
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

Both new mechanisms **preserve `protected_minimum`** exactly like normal
bridge settlement does — this is deliberate (see
`RESERVE_CUSTODY_AND_WITHDRAWAL.md`'s recommendations), not an oversight.
A withdrawal that would take the reserve below `protected_minimum` is
rejected on the Solana side (`InsufficientReserveBalance`) the same way an
ordinary release would be; the Goldcoin CLI has no on-chain floor to
enforce (Goldcoin has no program layer) but `plan` will simply fail coin
selection if the requested amount exceeds what the vault actually holds.

To withdraw genuinely everything (e.g., decommissioning the pilot
entirely), lower `protected_minimum` first, deliberately and visibly:

```
glc-admin onchain-... # (admin-gated set_limit)
```

using the existing `set_limit(ProtectedMinimum, 0)` instruction (already
built, already emits `LimitsChanged`, already audited by this session's
`RESERVE_CUSTODY_AND_WITHDRAWAL.md`) — then run the withdrawal procedures
above for the full balance. Restore `protected_minimum` to its approved
value afterward if the reserve is not actually being fully decommissioned.
