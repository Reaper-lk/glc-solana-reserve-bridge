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
2. **The signer-side policy of [28-signer-policy.md](28-signer-policy.md)
   is deployed** at all three attestation domains, with action-scoped
   credentials. This is the control that makes a stolen bridge-host
   credential insufficient, and it requires no redeploy.
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
     `propose_upgrade` → wait `upgrade_timelock_seconds` (48h at the
     approved pilot value) → `execute_upgrade`. **There is no CLI for
     those three instructions in this repository.** Stop and resolve that
     before proceeding; do not hand-assemble an upgrade transaction.

4. **A maintenance window long enough for §3–§9.** The bridge is paused
   throughout.

---

## 1. Values you must supply

Nothing in this repository invents any of these. Decide them, write them
down, and have them reviewed before the window opens.

| Placeholder | What it is | Guidance |
|---|---|---|
| `<RPC_URL>` | Solana RPC endpoint | — |
| `<TREASURY_TOKEN_ACCOUNT>` | The allowlisted destination **token account** address — not a wallet owner | Use the canonical ATA of a cold or multisig wallet **whose key never touches the bridge host**. The allowlist is only as strong as custody of the wallet behind it (docs/29 §8 item 5). |
| `<PER_WITHDRAWAL_LIMIT>` | Ceiling on one treasury withdrawal, raw atomic units (GLC = 6 decimals) | Must be > 0. |
| `<ROLLING_LIMIT>` | Ceiling on the sum of treasury withdrawals per window, raw units | Must be > 0 and **≥ `<PER_WITHDRAWAL_LIMIT>`**. This is the maximum loss per window under full host compromise — size it as a blast radius, not as a convenience. |
| `<ROLLING_WINDOW_SECONDS>` | Width of the fixed budget bucket | The bucket is fixed, not sliding: a burst spanning a boundary can reach **2 ×** `<ROLLING_LIMIT>`. Plan against that. |
| `<PAYER_KEYPAIR>` | Fee/rent payer for policy transactions | Confers **no** authority. Never the admin key. |
| `<ADMIN_KEYPAIR>` / `<SUBMITTER_KEYPAIR>` | Used only for pause/unpause and withdrawals | Not used by any policy command. |
| `<SIGNER_N>` | `PUBKEY,https://URL,AUTH_TOKEN_ENV[,TIMEOUT_MS]` per attestation domain | Supply at least `threshold` of them. Run `attest` on the approval host. |

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
    --treasury <TREASURY_TOKEN_ACCOUNT> \
    --per-withdrawal-limit <PER_WITHDRAWAL_LIMIT> \
    --rolling-limit <ROLLING_LIMIT> \
    --rolling-window-seconds <ROLLING_WINDOW_SECONDS> \
    --out policy-plan.json
```

This reads live chain state, validates the parameters against the exact
rules the program enforces, and prints the allowlist and limits for
review. **This step is the dry run.** Nothing is signed or broadcast.

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
    --expect-treasury <TREASURY_TOKEN_ACCOUNT> \
    --expect-per-withdrawal-limit <PER_WITHDRAWAL_LIMIT> \
    --expect-rolling-limit <ROLLING_LIMIT> \
    --expect-rolling-window-seconds <ROLLING_WINDOW_SECONDS>
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
2. **Over the per-withdrawal limit.** `plan` with
   `--amount` greater than `<PER_WITHDRAWAL_LIMIT>` must be refused.
3. **The retired instruction.** Any surviving tooling that calls
   `rebalance_withdraw` must fail with `RebalanceWithdrawRetired`.
4. **A legitimate withdrawal still works.** `glc-treasury-withdraw plan`
   for a within-limits amount to the allowlisted treasury must succeed and
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
    --per-withdrawal-limit <CORRECTED_PER_WITHDRAWAL_LIMIT> \
    --rolling-limit <CORRECTED_ROLLING_LIMIT> \
    --rolling-window-seconds <CORRECTED_ROLLING_WINDOW_SECONDS> \
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

Note: applying a policy change does **not** reset the rolling window. An
exhausted budget stays exhausted until it ages out — deliberately, so that
re-approving a policy cannot be used as a budget top-up.

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
