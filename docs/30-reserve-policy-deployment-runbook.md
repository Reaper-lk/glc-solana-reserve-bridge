# Reserve-withdrawal hardening — production deployment runbook

Command-by-command procedure for deploying the change described in
[29-reserve-withdrawal-hardening.md](29-reserve-withdrawal-hardening.md):
the program upgrade that retires `rebalance_withdraw`, the service/CLI
release that replaces it, and the one-time `RebalancePolicy` creation
without which `treasury_withdraw` refuses every destination.

Every command below exists in this repository. Where a value is a
production decision, it is written as a `<PLACEHOLDER>` and listed in
§1 — those are the only things an operator supplies.

> **Nothing in this runbook has been executed.** No program has been
> upgraded, no policy account exists, and no production state has been
> modified by the change this runbook deploys.

---

## 0. Preconditions

Do not start until all of these are true.

1. **Runbook phases 1–3 of `RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md` are
   complete.** They need no program change and already close the incident
   path. This deployment is defence in depth, not the mitigation.
2. **Signer-side policy ([28-signer-policy.md](28-signer-policy.md)) —
   NOT being deployed in this window, by decision.** It is the control
   that makes a stolen bridge-host credential insufficient, and it needs
   no redeploy, so it can land later independently. Deploying without it
   is a deliberate, accepted gap: the on-chain allowlist still confines a
   compromised host to the approved treasury, but there is no on-chain
   amount bound behind it (docs/29 §7, F-4), so the signer-side ceiling is
   the only amount check in the system and it is not yet deployed. Track
   it as the highest-value follow-up.
3. **The program upgrade authority is decided.** Run:

   ```
   glc-admin show-authorities --rpc-url <RPC_URL>
   ```

   This prints `BridgeConfig.admin`, any pending admin handover, the real
   BPF-loader upgrade authority, and **whether the on-chain upgrade
   timelock is armed**. Read that last line before continuing:

   - **Timelock NOT armed** — the upgrade is a direct
     `solana program deploy` by the upgrade authority (§4 below).
   - **Timelock ARMED** — the upgrade must go through
     `propose_upgrade` → wait `upgrade_timelock_seconds` → `execute_upgrade`.
     **There is no CLI for those three instructions in this repository.**
     Stop; do not hand-assemble an upgrade transaction.

   **Determined 2026-09-02: NOT ARMED.** ProgramData
   `268AcDD4tYvmxJ2HfP6npzbni2NEepidLjXqpPtJ2zgx` reports upgrade authority
   `9LdtdQsyBfj6Kof5badHjYU3PeBBxvG9ZxKjn2ZXZ1cM`, which is not the
   program's `upgrade_authority` PDA
   (`2zzAgjCs18EPhKaqid6YCTHCW4immrEj938iaawWxDku`). The direct
   `solana program deploy` path in §4 therefore applies. Re-check with
   `solana program show` before the window opens.

4. **A maintenance window long enough for §3–§9.** The bridge is paused
   throughout.

---

## 1. Values you must supply

Nothing in this repository invents any of these. Decide them, write them
down, and have them reviewed before the window opens.

| Value | Setting | Status |
|---|---|---|
| Treasury token account | `3GQC9sZHdBCjxrZyn4tevb7wbv24oSViGtEdrfYU87Vd` | **CONFIRMED** |
| Per-withdrawal limit | *does not exist* | Removed from the design |
| Rolling withdrawal budget / window | *does not exist* | Removed from the design |

The allowlist is the whole policy. There is deliberately **no amount
ceiling, rate limit or rolling budget** on a treasury withdrawal: a single
withdrawal may move the entire reserve to the allowlisted treasury.
`BridgeConfig.protected_minimum` (set through `glc-admin set-limit`, not
through this tool) remains the one on-chain accounting floor, and each
custody domain's own ceiling (`docs/28-signer-policy.md` §3) is the only
amount check on the approval path. See `docs/29-reserve-withdrawal-hardening.md`
§7, F-4 for why.

Still to supply, per run:

| Placeholder | What it is |
|---|---|
| `<RPC_URL>` | Solana RPC endpoint |
| `<PROGRAM_ID>` | `6tmLSP2j2thito2RpByqgfKHuVRSLcNd9c5FkrLJMjja` |
| `<PAYER_KEYPAIR>` | Fee/rent payer for policy transactions. Confers **no** authority; never the admin key. |
| `<ADMIN_KEYPAIR>` / `<SUBMITTER_KEYPAIR>` | Pause/unpause and withdrawals only. Not used by any policy command. |
| `<UPGRADE_AUTHORITY_KEYPAIR>` | `9LdtdQsyBfj6Kof5badHjYU3PeBBxvG9ZxKjn2ZXZ1cM` — unchanged for this deployment. |
| `<SIGNER_N>` | `PUBKEY,https://URL,AUTH_TOKEN_ENV[,TIMEOUT_MS]` per attestation domain, at least `threshold` of them. Existing keys/tokens unchanged. |

> **Pre-flight the treasury address.** `initialize_rebalance_policy` takes
> the allowlist as plain pubkeys, so the program cannot verify at creation
> time that the address is a live token account of the reserve mint. A
> wrong address initializes fine and then refuses every withdrawal, and
> correcting it costs a full 24 h governance cycle. Before §5, confirm:
>
> ```
> solana account 3GQC9sZHdBCjxrZyn4tevb7wbv24oSViGtEdrfYU87Vd --url <RPC_URL>
> spl-token account-info --address 3GQC9sZHdBCjxrZyn4tevb7wbv24oSViGtEdrfYU87Vd --url <RPC_URL>
> ```
>
> It must exist, be a token account of the reserve mint, be owned by the
> configured token program, and its OWNER must be a wallet whose key never
> touches the bridge host.

Reference for the existing seven bridge-policy parameters:
`22-production-readiness-review.md` P0-6, "Approved pilot bridge-policy
parameters".

> Note: that table lists the rolling volume limit as 100,000 GLC while the
> 2026-08-21 update text in the same section implies 50,000 GLC. That is
> the bridge's *release* window rather than this policy's window, so it
> does not block this deployment — but resolve the inconsistency before
> anyone copies values out of that table.

---

## 2. Backups

```
scripts/backup-ledger.sh
```

Then record the pre-deployment on-chain state, so §9 has something to
compare against:

```
glc-admin show-config       --rpc-url <RPC_URL> | tee pre-deploy-config.txt
glc-admin show-authorities  --rpc-url <RPC_URL> | tee pre-deploy-authorities.txt
glc-admin rebalance-policy-show --rpc-url <RPC_URL> | tee pre-deploy-policy.txt
```

Before the upgrade, `rebalance-policy-show` reports that no policy exists.
That is expected and is the safe state.

Record the currently deployed program binary hash for rollback:

```
solana program dump <PROGRAM_ID> pre-deploy-program.so --url <RPC_URL>
sha256sum pre-deploy-program.so | tee pre-deploy-program.sha256
```

---

## 3. Global pause

```
glc-admin onchain-pause --rpc-url <RPC_URL> \
    --keypair <ADMIN_KEYPAIR> \
    --scope global \
    --note "reserve-withdrawal hardening deployment (docs/30)"
```

Confirm:

```
glc-admin show-config --rpc-url <RPC_URL>
```

`paused` must be `true` before continuing. Both `treasury_withdraw` and
`refund_withdraw` require it, and the upgrade must not race a settlement.

---

## 4. Pre-deployment checks and program upgrade

Build and verify locally first:

```
anchor build
sha256sum target/deploy/glc_reserve_bridge.so
```

**Timelock not armed** — direct upgrade by the upgrade authority:

```
solana program deploy target/deploy/glc_reserve_bridge.so \
    --program-id <PROGRAM_ID> \
    --upgrade-authority <UPGRADE_AUTHORITY_KEYPAIR> \
    --url <RPC_URL>
```

**Timelock armed** — stop; see §0 item 3.

Verify the deployed binary matches what you built:

```
solana program dump <PROGRAM_ID> deployed.so --url <RPC_URL>
sha256sum deployed.so
```

At this point `rebalance_withdraw` is retired and no policy exists yet, so
**no operator withdrawal path is available**. This is intended and safe —
and it is why §5 is not optional.

---

## 5. RebalancePolicy initialization

Three stages, three hosts. Authorized by threshold attestation; no admin
key is involved at any point.

### 5a. Plan — bridge host or any host, no key

```
glc-rebalance-policy plan \
    --rpc-url <RPC_URL> \
    --action init \
    --treasury 3GQC9sZHdBCjxrZyn4tevb7wbv24oSViGtEdrfYU87Vd \
    --out policy-plan.json
```

This reads live chain state, validates the parameters against the exact
rules the program enforces, and prints the allowlist for review. **This step is the dry run.** Nothing is signed or broadcast.

Read the printed allowlist back against §1 before continuing. If more than
one treasury is listed, note that **order is significant** — it is what
the attestation commits to.

### 5b. Attest — APPROVAL HOST, no local private key

```
glc-rebalance-policy attest \
    --plan policy-plan.json \
    --rpc-url <RPC_URL> \
    --attestation-signer <SIGNER_1> \
    --attestation-signer <SIGNER_2> \
    --out policy-attested.json
```

Re-verifies live state before contacting any custody domain, then collects
threshold signatures over the 90-byte governance message (action `0x09`).
Refuses if fewer than `threshold` signatures come back.

### 5c. Execute — dry run first

```
glc-rebalance-policy execute \
    --attested-plan policy-attested.json \
    --rpc-url <RPC_URL> \
    --payer-keypair <PAYER_KEYPAIR>
```

No `--execute`: builds, re-verifies live state a third time, re-derives the
message from the plan's own fields, and **simulates only**. Read the
simulation result.

Then, only if the dry run succeeded and the printed summary is exactly
what you intend:

```
glc-rebalance-policy execute \
    --attested-plan policy-attested.json \
    --rpc-url <RPC_URL> \
    --payer-keypair <PAYER_KEYPAIR> \
    --execute
```

Broadcasts only if its own simulation just succeeded.

---

## 6. CLI / service installation

The program upgrade and the service release are **not independently
deployable**: `service/src/solana/refund.rs` now calls `refund_withdraw`
rather than the retired `rebalance_withdraw`, so a program upgraded
without the service (or the reverse) breaks refunds.

Install the new binaries on the bridge host in the same window:

```
cd service && cargo +nightly build --release --bins
```

Deploy `glc-admin`, `glc-bridge-daemon`, `glc-treasury-withdraw`,
`glc-rebalance-policy`, restart the daemon, and confirm:

```
glc-treasury-withdraw --help    # must exist
glc-rebalance-withdraw-solana   # must be "command not found" — renamed on purpose
```

---

## 7. Verification

```
glc-rebalance-policy verify \
    --rpc-url <RPC_URL> \
    --expect-treasury 3GQC9sZHdBCjxrZyn4tevb7wbv24oSViGtEdrfYU87Vd
```

Exits non-zero on **any** mismatch, allowlist order included. Do not
proceed past a mismatch.

Then:

```
glc-admin rebalance-policy-show --rpc-url <RPC_URL>
glc-admin show-authorities --rpc-url <RPC_URL>
```

`rebalance-policy-show` must report **no pending change**. A queued change
you did not create is an incident — cancel it with
`glc-rebalance-policy plan --action cancel` (§8) before doing anything
else.

### Adversarial checks (all must FAIL)

Still paused, confirm the controls are live. Each of these is a dry run —
none carries `--execute`, so none can move anything even if a check were
missing.

1. **Unallowlisted destination.** `glc-treasury-withdraw plan` with a
   `--treasury` that is not on the allowlist must be refused by `plan`
   itself, before any signer is contacted.
2. **Below `protected_minimum`.** `plan` with an `--amount` that would
   take the reserve below `protected_minimum` must be refused. This is the
   only on-chain amount rule there is — there is no per-withdrawal
   ceiling, no rate limit and no rolling budget.
3. **The retired instruction.** Any surviving tooling that calls
   `rebalance_withdraw` must fail with `RebalanceWithdrawRetired`.
4. **A legitimate withdrawal still works.** `glc-treasury-withdraw plan`
   for an ordinary amount to the allowlisted treasury must succeed and
   print a sane summary. Do not attest or execute it.

---

## 8. Rollback

**Before the program upgrade (§4):** unpause (§9) and stop. Nothing has
changed.

**After the upgrade, before policy initialization:** the reserve is paused
and no withdrawal path exists. Either complete §5, or roll the program
back:

```
solana program deploy pre-deploy-program.so \
    --program-id <PROGRAM_ID> \
    --upgrade-authority <UPGRADE_AUTHORITY_KEYPAIR> \
    --url <RPC_URL>
sha256sum pre-deploy-program.sha256   # compare against the redeployed binary
```

Roll the service binaries back in the same window — the refund path is
coupled to the program (§6).

**After policy initialization:** the policy account cannot be deleted, and
initialization cannot be repeated. A wrong policy is corrected forward,
through the timelock, never by re-initializing:

```
glc-rebalance-policy plan --rpc-url <RPC_URL> --action propose \
    --treasury <CORRECTED_TREASURY> \
    --out policy-fix-plan.json

glc-rebalance-policy attest --plan policy-fix-plan.json --rpc-url <RPC_URL> \
    --attestation-signer <SIGNER_1> --attestation-signer <SIGNER_2> \
    --out policy-fix-attested.json

glc-rebalance-policy execute --attested-plan policy-fix-attested.json \
    --rpc-url <RPC_URL> --payer-keypair <PAYER_KEYPAIR> [--execute]
```

Wait `governance_timelock_seconds`, then apply:

```
glc-rebalance-policy apply --rpc-url <RPC_URL> \
    --payer-keypair <PAYER_KEYPAIR> [--execute]
```

To abandon a queued change instead (this is also the response to a change
you did not queue):

```
glc-rebalance-policy plan --rpc-url <RPC_URL> --action cancel \
    --out policy-cancel-plan.json
glc-rebalance-policy attest --plan policy-cancel-plan.json --rpc-url <RPC_URL> \
    --attestation-signer <SIGNER_1> --attestation-signer <SIGNER_2> \
    --out policy-cancel-attested.json
glc-rebalance-policy execute --attested-plan policy-cancel-attested.json \
    --rpc-url <RPC_URL> --payer-keypair <PAYER_KEYPAIR> [--execute]
```

---

## 9. Final unpause

Only after §7 verification has fully passed:

```
glc-admin onchain-unpause --rpc-url <RPC_URL> \
    --keypair <ADMIN_KEYPAIR> \
    --scope global \
    --note "reserve-withdrawal hardening deployment complete (docs/30)"

glc-admin show-config --rpc-url <RPC_URL>
```

Confirm `paused` is `false` and monitor the first settlements through both
directions.

---

## 10. Still outstanding after this deployment

This runbook bounds the damage. It does not fix the deployment. From
[29-reserve-withdrawal-hardening.md](29-reserve-withdrawal-hardening.md)
§8, and none of it is something a code change can do:

1. **Move the program upgrade authority off the bridge host** to a
   hardware or multisig key. Until then, the upgrade authority can replace
   the program and every control here is downstream of one key on one
   machine.
2. **Rotate `BridgeConfig.admin`** to a key that never touches the bridge
   host, separate from the upgrade authority
   (`glc-admin transfer-admin` → `glc-admin accept-admin`).
3. **Deploy signer-side policy and action-scoped credentials** at all
   three attestation domains — §0 item 2, and the highest-value item here.
4. **Move the attestation credentials to an approval host** and adopt the
   three-host `plan` → `attest` → `execute` split for real.
5. **Confirm treasury custody.** The allowlist is only as strong as the
   wallet behind the allowlisted token account.
6. **Revoke and reissue every credential** assumed compromised: admin,
   submitter, deployer, both sets of bearer tokens, and the Goldcoin RPC
   credentials. Rebuild the host.
