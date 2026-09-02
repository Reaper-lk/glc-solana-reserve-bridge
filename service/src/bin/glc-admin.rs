//! `glc-admin` — reserve bridge operator CLI
//! (docs/07-implementation-plan.md Phase 5). Ported CLI shape and
//! mandatory `--note` audit discipline from the old bridge's `glc-admin`
//! (docs/01-reuse-inventory.md); governance/rotation/quorum-reassignment
//! subcommands (which depended on a P2P federation transport this bridge
//! does not have — see IMPLEMENTATION_LOG.md's Phase 5 entry) are
//! deliberately not ported. What's here: local status, this service's own
//! ledger-level directional pause and admission control (both independent
//! of the on-chain pause — see docs/09-runbook.md's "Admission control
//! (Solana->Goldcoin)" section for how the two local axes differ), and the
//! on-chain admin-gated `set_paused` instruction
//! (docs/12-management-decisions.md/IMPLEMENTATION_LOG.md's Phase 2
//! scoping decision: pause is admin-gated-immediate, not threshold-gated —
//! only attestation-key rotation gets that treatment).
//!
//! **Not yet built** (explicitly deferred, not silently missing): staged
//! multi-operator approval for attestation-key rotation, and the Goldcoin
//! vault sweep-to-fresh-vault compromise-response procedure
//! (docs/09-runbook.md's "Key compromise response"). Both need real
//! program/vault support this phase didn't build; see
//! IMPLEMENTATION_LOG.md.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glc_reserve_bridge_service::admin_api::{
    audited_resume_manual_review, audited_set_admission, audited_set_local_pause,
};
use glc_reserve_bridge_service::config::Config;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::payout_recovery::{
    recover_stuck_goldcoin_payout, RecoveryOutcome,
};
use glc_reserve_bridge_service::goldcoin::rpc::{
    RpcClient as GoldcoinRpcClient, RpcConfig as GoldcoinRpcConfig,
};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::goldcoin::{hex, liquidity, split};
use glc_reserve_bridge_service::ledger::{
    CustodyTransitionKind, Direction, Ledger, PendingVaultUtxoSplit, RebalanceKind,
    ReconcileUnmatchedDepositOutcome, RequestState, ReserveDirection, ResumeManualReviewOutcome,
};
use glc_reserve_bridge_service::ops::reserve_health;
use glc_reserve_bridge_service::rebalance;
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::confirm::{confirm_transaction, ConfirmPolicy};
use glc_reserve_bridge_service::solana::instructions::{
    self, LimitField, PauseScope, RollingWindowDirection,
};
use glc_reserve_bridge_service::solana::refund::{self, RefundExecuteOutcome};
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

use solana_sdk::signature::{read_keypair_file, Signer};
use solana_sdk::transaction::Transaction;

const USAGE: &str = "glc-admin — reserve bridge operator CLI

STATUS
  glc-admin status --db PATH

LOCAL LEDGER PAUSE (this service's own directional pause; independent of
the on-chain pause below and of admission control further down — see
docs/09-runbook.md)
  glc-admin pause   --db PATH --direction <goldcoin|solana> --note TEXT
  glc-admin unpause --db PATH --direction <goldcoin|solana> --note TEXT

LOCAL ADMISSION CONTROL (Solana->Goldcoin only, for now: whether a NEWLY
observed on-chain SolToGlc obligation is admitted into normal processing,
versus parked to ManualReview — separate from the pause above, which keeps
working exactly as it did before this existed. Already-accepted obligations
(anything already SourceFinalized or later) are NEVER affected by this —
payout processing has never been gated by either flag and still isn't; this
only ever blocks a NEW obligation from being admitted. See docs/09-runbook.md
'Admission control (Solana->Goldcoin)'.)
  glc-admin close-admission --db PATH --direction goldcoin --note TEXT
      Always allowed. New SolToGlc obligations fold into ManualReview
      instead of SourceFinalized until re-opened. Never automatic — only
      this command ever closes admission, and nothing ever auto-reopens it.
  glc-admin open-admission --db PATH --direction goldcoin --note TEXT
      Refuses unconditionally (no override) unless the GoldcoinReserve hard
      invariant currently holds (balance >= protected_minimum +
      reserved_liquidity) — never re-opens admission onto an already-broken
      reserve — and unless the automatic confirmed-liquidity gate has
      already reopened (confirmed headroom back at or above the configured
      reopen threshold). See docs/09-runbook.md 'Confirmed-liquidity
      admission safety buffer'; `status` prints both figures.

MANUAL REVIEW RECOVERY (Solana->Goldcoin only: resumes a request that
fold_sol_deposit itself parked in ManualReview because admission was
closed, the reserve was paused, or capacity was insufficient at that exact
moment — never a request in ManualReview for any other reason. Admission
may remain CLOSED; this never admits anything new, it only unblocks
something already accepted. Idempotent, and never creates a second
obligation — it transitions the existing request in place. See
docs/09-runbook.md 'Admission control (Solana->Goldcoin)'.)
  glc-admin resume-manual-review --db PATH --request-id N --note TEXT
      Refuses (no override) unless: the request is SolToGlc and currently
      ManualReview; its manual_review_note is one of the known fold-time
      reasons; its source deposit is already finalized; it has no Goldcoin
      payout row or destination transaction yet; and resuming it would not
      breach the GoldcoinReserve invariant. On success, moves the request
      ManualReview -> SourceFinalized and reserves its capacity, exactly as
      a successful fold would have — normal processing (unaffected by this
      command) picks it up from there. Refuses outright any request with a
      refund lifecycle (RefundPending/RefundBroadcast/Refunded, or any
      solana_refunds row) — a refund, once begun, is permanent.

MANUAL REVIEW REFUND (Solana->Goldcoin only: returns a fold-parked
deposit to the ORIGINAL Solana depositor via the on-chain
rebalance_withdraw instruction — admin signature + 2-of-3 threshold
attestation + on-chain global pause + protected minimum + per-request
nonce replay guard, none of it weakened. The destination is ALWAYS the
canonical Token-2022 ATA of the on-chain WithdrawalObligation.requester +
the configured reserve mint — derived, never accepted as input; there is
deliberately no --destination flag. The refund amount is exactly the
gross deposited amount (the SolToGlc bridge fee only accrues at
settlement, which a refunded request never reaches). Once the lifecycle
begins the request is permanently ineligible for resume-manual-review and
for any Goldcoin payout. See docs/09-runbook.md 'ManualReview refunds
(Solana->Goldcoin)'.)
  glc-admin refund-manual-review --config PATH --request-id N --note TEXT \\
      [--keypair ADMIN_KEYPAIR] [--execute]
      --config points at the same config file glc-bridge-daemon uses (the
      ledger path, Solana RPC URL, submitter keypair path, and attestation
      signer endpoints all come from it — --db alone is not enough).
      Without --execute: STRICT READ-ONLY DRY RUN — prints the request,
      the original deposit (obligation index/PDA — the bridge stores no
      deposit tx signature; the finalized obligation account IS the
      verified deposit record), the derived destination, amounts,
      reserve balance before/after, protected minimum, and every safety
      check individually. Contacts no signer, loads no keypair, writes
      nothing, broadcasts nothing.
      With --execute (requires --keypair, the on-chain admin keypair):
      re-runs every check against fresh state, requires the bridge
      ALREADY globally paused (on-chain enforced; re-checked immediately
      before simulation — this command never pauses or unpauses on its
      own), collects threshold attestations, ALWAYS simulates first,
      broadcasts only on simulation success, and confirms at finalized
      commitment before marking the request Refunded. Safe to re-run at
      any point: an already-Refunded request reports its transaction and
      exits successfully; a broadcast-but-unconfirmed refund is checked/
      finalized/rebuilt under the SAME nonce — a second transfer for the
      same request can never land (on-chain PDA replay guard).
      Eligible ManualReview reasons (conservative whitelist; everything
      else refused): admission_closed_at_fold, reserve_paused_at_fold,
      insufficient_capacity_at_fold, utxo_liquidity_low_at_fold,
      liquidity_buffer_low_at_fold, recipient_rate_limited,
      source_wallet_rate_limited.
  glc-admin refund-list --db PATH [--open-only]
      Read-only listing of every refund lifecycle (or only the not-yet-
      Confirmed ones with --open-only).

UNMATCHED DEPOSIT RECONCILIATION (goldcoin::indexer recognizes an internal
vault-split output live going forward — see 'Vault UTXO splitting' below —
but a row already recorded as unmatched before that recognition existed
stays recorded until explicitly reconciled. Never deletes anything.)
  glc-admin reconcile-unmatched-deposit --db PATH --txid TXID --vout N --note TEXT
      Refuses (no override) unless (txid, vout, amount) exactly matches an
      expected output of a known Broadcast vault split — the identical
      check the indexer itself applies live. Marks the row reconciled;
      idempotent on an already-reconciled row.

ON-CHAIN (admin-gated-immediate; requires the BridgeConfig admin's keypair)
  glc-admin show-config    --rpc-url URL
  glc-admin onchain-pause   --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT
  glc-admin onchain-unpause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT
  glc-admin set-limit --rpc-url URL --keypair PATH \\
      --field <min-transfer|per-transfer|protected-minimum|rolling-volume> \\
      --value N --note TEXT
      Calls the on-chain set_limit instruction (admin-gated-immediate,
      same posture as onchain-pause above — see
      programs/glc-reserve-bridge/src/instructions/admin.rs module docs).
      --value is the new limit in atomic units of the Solana-side mint.
  glc-admin reset-rolling-window --rpc-url URL --keypair PATH \\
      --direction <glc-to-sol|sol-to-glc> --note TEXT
      Administrative override of the rolling-volume anti-drain protection:
      manually reopens the selected direction's 24h volume window (used
      volume -> 0, remaining -> the full configured rolling_volume_limit,
      quota_exhausted -> false) without waiting out its remainder. Refuses
      on-chain unless BridgeConfig.paused is already true — global pause
      first, then this, per docs/09-runbook.md's maintenance sequence.
      glc-to-sol resets the RELEASE window; sol-to-glc resets the DEPOSIT
      window. Does not require the individual direction's own pause, and
      never touches reserve balances, obligations, limits, or the other
      direction's window. Use only after verifying reserve/accounting
      state — see the runbook before running this in production.

GOLDCOIN PAYOUT RECOVERY (a payout stuck in Signed state after its
broadcast was rejected — e.g. request #8, Goldcoin RPC -26 'non-canonical
signature'. Never invoked automatically: Orchestrator::tick_goldcoin_
payouts always skips a request that already has a goldcoin_payouts row.
Reuses the exact same independent multi-signer signing path a normal
payout build uses — never rebroadcasts the stored signed_tx_hex verbatim,
never selects a new UTXO, never builds a second payout row. Safe to
re-run: a payout already Broadcast/Confirmed/Completed is reported and
left untouched.)
  glc-admin retry-goldcoin-payout --config PATH --request-id N --note TEXT
      --config points at the same config file glc-bridge-daemon uses
      (needs the configured vault signers + Goldcoin RPC, not just the
      ledger — see config.rs); --db alone is not enough for this command.

VAULT UTXO SPLITTING (proactively fragments one large mature root-vault
UTXO into several smaller ones, all still paying the vault's own script —
docs/09-runbook.md 'Vault UTXO splitting'. Answers the case where
coin::select correctly avoids an oversized UTXO when smaller ones exist,
but has no smaller ones to choose from. Uses the exact same 2-of-3 vault
signer path every payout uses; never exposes signer secrets. Idempotent —
a source outpoint that has already been split is reported and left alone,
never split twice. The full plan (source UTXO, output count, per-output
amount, fee, and the resulting mature-reserve effect) is always printed
BEFORE any signer is contacted, in both dry-run and --execute runs. The
reserve-safety check — the split must never itself drop mature reserve
below protected_minimum + pending_obligations — is unconditional: there is
no flag to override a failed check.)
  glc-admin split-vault-utxo --config PATH --txid TXID --vout N \\
      [--chunk-target-atomic N] --note TEXT [--execute] [--abandon]
      --txid/--vout name the exact mature root-vault UTXO to split (found
      via glc-admin status / direct ledger inspection) — never auto-picked.
      --chunk-target-atomic defaults to the config's own canonical
      change_fanout_target_atomic — one payout-chunk sizing for the whole
      service; pass it only for a deliberate one-off.
      Without --execute: prints the plan and safety check, contacts no
      signer, broadcasts nothing (dry run). With --execute: prints the
      same plan, then signs (real signer calls) and broadcasts it. A
      failed safety check refuses in both modes, with no override.
      If the outpoint already has a live split, --execute drives ITS
      lifecycle instead (resume a Built/Signed row, confirmation-check or
      re-broadcast a Broadcast row) — same code the daemon runs.
      --abandon (with --execute): operator-decided abandonment of a
      not-yet-Confirmed split the lifecycle cannot finish — audit row
      kept, source outpoint released. Refused for Confirmed splits.

REBALANCING (docs/22-production-readiness-review.md P1 'rebalancing'; this
service NEVER signs or broadcasts a fund-moving transaction itself — every
real transfer is authorized and executed entirely out of band, through
whatever real custody tooling holds the actual keys, and only ever
RECORDED here as evidence after the fact)
  glc-admin rebalance-status  --db PATH
      Read-only imbalance assessment for both reserves against their own
      configured target/warning/critical thresholds.
  glc-admin rebalance-list --db PATH [--direction <goldcoin|solana>] [--open-only]
  glc-admin rebalance-propose --db PATH --direction <goldcoin|solana> \\
      --kind <deposit|withdraw> --amount N --by IDENTITY \\
      --required-approvals N --note TEXT
  glc-admin rebalance-approve --db PATH --id N --by IDENTITY
  glc-admin rebalance-reject  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin rebalance-cancel  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT
      Records evidence of a real transfer already authorized and executed
      outside this system — never constructs or broadcasts one itself.
  glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N
  glc-admin rebalance-fail    --db PATH --id N --by IDENTITY --note TEXT

KEY ROTATION / VAULT SWEEP (docs/22-production-readiness-review.md P1 'key
rotation / vault sweep tooling'; generic tooling for retiring an old
attestation-signer set or Goldcoin vault identity in favor of a verified new
one. Like rebalancing, this service NEVER generates keys, signs, or executes
a real rotation/sweep itself — every real transition is authorized and
performed entirely out of band, and only ever RECORDED here as evidence
after the fact. Execution additionally requires the relevant reserve(s)
already paused: GoldcoinReserve for a vault sweep, both reserves for an
attestation-key rotation)
  glc-admin custody-list --db PATH [--kind <attestation-rotation|vault-sweep>] [--open-only]
  glc-admin custody-propose --db PATH --kind <attestation-rotation|vault-sweep> \\
      --old-identities CSV --new-identities CSV [--new-threshold N] \\
      --by IDENTITY --required-approvals N --note TEXT
  glc-admin custody-verify-identity --db PATH --id N --by IDENTITY
      Records that --by independently verified the claimed new identity.
      Required before any approval can be recorded.
  glc-admin custody-approve --db PATH --id N --by IDENTITY
  glc-admin custody-reject  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin custody-cancel  --db PATH --id N --by IDENTITY --note TEXT
  glc-admin custody-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT
      Records evidence of a real rotation/sweep already authorized and
      executed outside this system — never performs one itself. Fails if
      the relevant reserve(s) are not already paused.
  glc-admin custody-confirm --db PATH --id N --by IDENTITY
  glc-admin custody-fail    --db PATH --id N --by IDENTITY --note TEXT
  glc-admin custody-rollback --db PATH --id N --by IDENTITY --note TEXT
      Records that a Failed transition's effect was reverted back to the
      old identity out of band — never performs the rollback itself.

Every mutating command requires --note (mandatory audit trail), except
rebalance-approve/-record-executed/-confirm and
custody-verify-identity/-approve/-record-executed/-confirm, which record
--by instead (a note is redundant with the approver/executor identity
itself).";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let Some(cmd) = args.get(1) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let result = match cmd.as_str() {
        "status" => cmd_status(&args),
        "pause" => cmd_local_pause(&args, true),
        "unpause" => cmd_local_pause(&args, false),
        "close-admission" => cmd_admission(&args, true),
        "open-admission" => cmd_admission(&args, false),
        "resume-manual-review" => cmd_resume_manual_review(&args),
        "refund-manual-review" => cmd_refund_manual_review(&args),
        "refund-list" => cmd_refund_list(&args),
        "reconcile-unmatched-deposit" => cmd_reconcile_unmatched_deposit(&args),
        "show-config" => cmd_show_config(&args),
        "onchain-pause" => cmd_onchain_pause(&args, true),
        "onchain-unpause" => cmd_onchain_pause(&args, false),
        "set-limit" => cmd_set_limit(&args),
        "reset-rolling-window" => cmd_reset_rolling_window(&args),
        "retry-goldcoin-payout" => cmd_retry_goldcoin_payout(&args),
        "split-vault-utxo" => cmd_split_vault_utxo(&args),
        "rebalance-status" => cmd_rebalance_status(&args),
        "rebalance-list" => cmd_rebalance_list(&args),
        "rebalance-propose" => cmd_rebalance_propose(&args),
        "rebalance-approve" => cmd_rebalance_approve(&args),
        "rebalance-reject" => cmd_rebalance_reject(&args),
        "rebalance-cancel" => cmd_rebalance_cancel(&args),
        "rebalance-record-executed" => cmd_rebalance_record_executed(&args),
        "rebalance-confirm" => cmd_rebalance_confirm(&args),
        "rebalance-fail" => cmd_rebalance_fail(&args),
        "custody-list" => cmd_custody_list(&args),
        "custody-propose" => cmd_custody_propose(&args),
        "custody-verify-identity" => cmd_custody_verify_identity(&args),
        "custody-approve" => cmd_custody_approve(&args),
        "custody-reject" => cmd_custody_reject(&args),
        "custody-cancel" => cmd_custody_cancel(&args),
        "custody-record-executed" => cmd_custody_record_executed(&args),
        "custody-confirm" => cmd_custody_confirm(&args),
        "custody-fail" => cmd_custody_fail(&args),
        "custody-rollback" => cmd_custody_rollback(&args),
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// Missing/malformed arguments are a usage error (exit 2), distinct from a
/// failed operation (exit 1) — same distinction `glc-audit` draws.
fn require<'a>(args: &'a [String], name: &str) -> &'a str {
    flag(args, name).unwrap_or_else(|| {
        eprintln!("missing required {name}\n\n{USAGE}");
        std::process::exit(2);
    })
}

fn require_note(args: &[String]) -> Result<&str, String> {
    match flag(args, "--note") {
        Some(n) if !n.trim().is_empty() => Ok(n),
        _ => Err("--note is required and must be non-empty (mandatory audit trail)".to_string()),
    }
}

fn parse_reserve_direction(s: &str) -> Result<ReserveDirection, String> {
    match s {
        "goldcoin" => Ok(ReserveDirection::GoldcoinReserve),
        "solana" => Ok(ReserveDirection::SolanaReserve),
        other => Err(format!(
            "unknown --direction {other} (expected goldcoin|solana)"
        )),
    }
}

fn parse_rebalance_kind(s: &str) -> Result<RebalanceKind, String> {
    match s {
        "deposit" => Ok(RebalanceKind::Deposit),
        "withdraw" => Ok(RebalanceKind::Withdraw),
        other => Err(format!(
            "unknown --kind {other} (expected deposit|withdraw)"
        )),
    }
}

fn parse_custody_kind(s: &str) -> Result<CustodyTransitionKind, String> {
    match s {
        "attestation-rotation" => Ok(CustodyTransitionKind::AttestationKeyRotation),
        "vault-sweep" => Ok(CustodyTransitionKind::GoldcoinVaultSweep),
        other => Err(format!(
            "unknown --kind {other} (expected attestation-rotation|vault-sweep)"
        )),
    }
}

fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn require_u64(args: &[String], name: &str) -> Result<u64, String> {
    require(args, name)
        .parse()
        .map_err(|e| format!("{name} must be a non-negative integer: {e}"))
}

fn require_i64(args: &[String], name: &str) -> Result<i64, String> {
    require(args, name)
        .parse()
        .map_err(|e| format!("{name} must be an integer: {e}"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_pause_scope(s: &str) -> Result<PauseScope, String> {
    match s {
        "global" => Ok(PauseScope::Global),
        "release" => Ok(PauseScope::Release),
        "deposit" => Ok(PauseScope::Deposit),
        other => Err(format!(
            "unknown --scope {other} (expected global|release|deposit)"
        )),
    }
}

fn parse_limit_field(s: &str) -> Result<LimitField, String> {
    match s {
        "min-transfer" => Ok(LimitField::MinTransferAmount),
        "per-transfer" => Ok(LimitField::PerTransferLimit),
        "protected-minimum" => Ok(LimitField::ProtectedMinimum),
        "rolling-volume" => Ok(LimitField::RollingVolumeLimit),
        other => Err(format!(
            "unknown --field {other} (expected min-transfer|per-transfer|protected-minimum|rolling-volume)"
        )),
    }
}

/// `glc-to-sol` = the RELEASE rolling-volume window (Goldcoin deposit ->
/// Solana reserve release); `sol-to-glc` = the DEPOSIT rolling-volume
/// window (Solana deposit -> Goldcoin reserve release) — the exact mapping
/// `programs/glc-reserve-bridge/src/instructions/initialize.rs` sets up
/// (`release_volume_window.direction = GoldcoinToSolana`,
/// `deposit_volume_window.direction = SolanaToGoldcoin`).
fn parse_rolling_window_direction(s: &str) -> Result<RollingWindowDirection, String> {
    match s {
        "glc-to-sol" => Ok(RollingWindowDirection::GoldcoinToSolana),
        "sol-to-glc" => Ok(RollingWindowDirection::SolanaToGoldcoin),
        other => Err(format!(
            "unknown --direction {other} (expected glc-to-sol|sol-to-glc)"
        )),
    }
}

fn cmd_status(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;

    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        match reserve_health::check(&ledger, direction, now_unix()) {
            Ok(s) => {
                println!(
                    "{direction:?}: balance={} protected_minimum={} reserved_liquidity={} \
                     pending_obligations={} accrued_fees={} immature_vault_utxo_total={} paused={} \
                     admission_closed={} invariant_holds={}",
                    s.total_reserve_balance,
                    s.protected_minimum,
                    s.reserved_liquidity,
                    s.pending_obligations,
                    s.accrued_fees,
                    s.immature_vault_utxo_total,
                    s.paused,
                    s.admission_closed,
                    s.invariant_holds
                );
                // The AUTOMATIC confirmed-liquidity gate, on its own line
                // and never folded into `admission_closed` above: an
                // operator must be able to tell "I closed this" apart
                // from "liquidity closed this", because the remedies are
                // completely different (open-admission vs. wait for
                // headroom to recover / add reserves). Goldcoin-only —
                // SolToGlc admission is the only thing it governs — and
                // silent when the buffer is disabled on this deployment.
                if direction == ReserveDirection::GoldcoinReserve && s.admission_buffer_atomic > 0 {
                    println!(
                        "  Admission liquidity: confirmed_headroom={} buffer={} reopen_at={} \
                         liquidity_admission_closed={}{}",
                        s.confirmed_admission_headroom,
                        s.admission_buffer_atomic,
                        s.admission_reopen_atomic,
                        s.liquidity_admission_closed,
                        if s.liquidity_admission_closed {
                            " — NEW SolToGlc deposits are parking in ManualReview; \
                             already-accepted obligations continue processing normally, and \
                             admission reopens automatically once confirmed headroom reaches \
                             reopen_at"
                        } else {
                            ""
                        }
                    );
                }
                // UTXO liquidity (docs/09-runbook.md "UTXO liquidity"):
                // reported as four distinct figures so a temporarily
                // immature payout change never reads as "reserves
                // disappeared" — the value is accounted for, just not yet
                // spendable. Solana has no UTXO-pool concept, so this line
                // is Goldcoin-only.
                if direction == ReserveDirection::GoldcoinReserve {
                    println!(
                        "  UTXO liquidity: reserve_value={} mature_spendable_capacity={} \
                         ({} UTXOs) temporarily_immature_internal_change={} ({} UTXOs){}",
                        s.total_reserve_balance,
                        s.utxo_pool.mature_available_atomic,
                        s.utxo_pool.available_utxo_count,
                        s.utxo_pool.own_unconfirmed_change_atomic,
                        s.utxo_pool.unconfirmed_change_utxo_count,
                        if s.utxo_pool_warning {
                            " — WARNING: mature UTXO pool is thin; this recovers automatically \
                             once payout change matures, but is worth an operator's attention"
                        } else {
                            ""
                        }
                    );
                    // Distinct from BOTH figures above: 0-conf-spendable
                    // bridge-created payout change is NOT confirmed
                    // reserve liquidity and is never counted in
                    // mature_spendable_capacity — shown separately so it
                    // can't be mistaken for it (docs/09-runbook.md
                    // "Zero-conf payout change").
                    println!(
                        "  zero-conf payout change (policy candidates, not confirmed liquidity): \
                         {} ({} UTXOs, {} on parent-validation hold)",
                        s.utxo_pool.zero_conf_change_candidate_atomic,
                        s.utxo_pool.zero_conf_change_candidate_count,
                        s.utxo_pool.zero_conf_change_held_count,
                    );
                }
            }
            Err(e) => println!("{direction:?}: not configured ({e})"),
        }
    }

    let manual_review: usize = [Direction::GlcToSol, Direction::SolToGlc]
        .iter()
        .map(|&d| {
            ledger
                .requests_by_state(d, RequestState::ManualReview)
                .map(|r| r.len())
                .unwrap_or(0)
        })
        .sum();
    println!("ManualReview backlog: {manual_review}");

    match ledger.post_finality_reorg_event_count() {
        Ok(0) => {}
        Ok(n) => println!(
            "WARNING: {n} post-finality reorg event(s) recorded — see \
             post_finality_reorg_events; both reserves are paused if any of these have not \
             yet been cleared by an operator"
        ),
        Err(e) => println!("could not read post_finality_reorg_events: {e}"),
    }

    Ok(())
}

/// The audit-log actor identity for this CLI invocation: `cli:<user>`,
/// so admin_audit_log rows distinguish SSH/CLI mutations from admin-API
/// ones while still naming the person (docs/27-admin-control-plane.md).
fn cli_actor() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("cli:{user}")
}

fn cmd_local_pause(args: &[String], paused: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_reserve_direction(require(args, "--direction"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    // Through the shared audited implementation, so a CLI pause leaves
    // the same admin_audit_log row (actor `cli:<user>`) an admin-API
    // pause would — one audit trail regardless of surface.
    audited_set_local_pause(&mut ledger, direction, paused, note, &cli_actor())
        .map_err(|e| e.to_string())?;
    println!("{direction:?} local ledger pause set to {paused} (note: {note})");
    Ok(())
}

/// Admission control (docs/09-runbook.md "Admission control
/// (Solana->Goldcoin)") — a separate axis from [`cmd_local_pause`] above.
/// Scoped to `--direction goldcoin` only for now: it is what
/// `Ledger::fold_sol_deposit` (the SolToGlc admission decision) actually
/// checks; `solana`/GlcToSol admission is unaffected by this command and
/// continues to depend only on the existing local pause.
fn cmd_admission(args: &[String], closing: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_reserve_direction(require(args, "--direction"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;

    // The direction restriction, the open path's two independent safety
    // checks (hard reserve invariant + count-based UTXO-liquidity gate),
    // and the audit row all live in the shared
    // `admin_api::audited_set_admission` — one implementation for the
    // CLI and the HTTP surface, so neither the checks nor the audit
    // trail can drift between them.
    audited_set_admission(&mut ledger, direction, closing, note, &cli_actor())
        .map_err(|e| e.to_string())?;
    println!(
        "{direction:?} admission {} (note: {note})",
        if closing { "closed" } else { "opened" }
    );
    Ok(())
}

/// Resumes a `SolToGlc` request stuck in `ManualReview` purely because it
/// was parked by `fold_sol_deposit`'s admission/pause/capacity gate — see
/// `Ledger::resume_manual_review_sol_to_glc`'s docs for the exact
/// preconditions and safety checks (unconditional, no override). Never
/// touches signer, confirmation, pause, quota, or admission logic —
/// admission may remain closed; this only ever unblocks something already
/// accepted, never admits anything new.
fn cmd_resume_manual_review(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    // Shared audited implementation: same safety checks, same audit row,
    // and the REAL invoking identity (`cli:<user>`) recorded in both the
    // admin audit log and bridge_request_state_log — never a hardcoded
    // placeholder actor.
    let (outcome, _receipt) =
        audited_resume_manual_review(&mut ledger, request_id, note, &cli_actor())
            .map_err(|e| e.to_string())?;
    match outcome {
        ResumeManualReviewOutcome::Resumed => {
            println!(
                "request {request_id}: resumed ManualReview -> SourceFinalized, capacity reserved (note: {note})"
            );
        }
        ResumeManualReviewOutcome::AlreadyResumed { state } => {
            println!(
                "request {request_id}: already resumed (state={state:?}) — nothing to do, no mutation performed"
            );
        }
    }
    Ok(())
}

/// ManualReview refund — see the USAGE banner and docs/09-runbook.md
/// "ManualReview refunds (Solana->Goldcoin)". Without `--execute` this is
/// a strict read-only dry run (no signer contact, no keypair load, no
/// database write, no broadcast); with it, the full guarded pipeline in
/// `solana::refund::execute_refund` runs, re-checking everything against
/// fresh state first.
fn cmd_refund_manual_review(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");
    // The admin keypair is only touched on --execute; a dry run must not
    // require (or read) any key material at all.
    let admin_keypair_path = if execute {
        Some(require(args, "--keypair").to_string())
    } else {
        None
    };

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(config.solana.rpc_url.clone());
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| {
            format!(
                "could not open ledger {}: {e}",
                config.service.db_path.display()
            )
        })?;

        let report = refund::dry_run_refund(&rpc, &ledger, request_id).await?;
        print_refund_report(&report);

        if !execute {
            println!(
                "\n--execute not supplied — DRY RUN ONLY: no signer was contacted, no keypair \
                 was loaded, nothing was written, nothing was broadcast."
            );
            return Ok(());
        }

        let admin_keypair_path = admin_keypair_path.expect("checked above");
        let admin = read_keypair_file(&admin_keypair_path)
            .map_err(|e| format!("could not read keypair {admin_keypair_path}: {e}"))?;
        let submitter = config.load_submitter().map_err(|e| e.to_string())?;
        let (attestation_signers, _vault_signers) =
            config.load_signers().await.map_err(|e| e.to_string())?;

        let outcome = refund::execute_refund(
            &rpc,
            &mut ledger,
            &attestation_signers,
            &admin,
            &submitter,
            request_id,
            note,
            &cli_actor(),
            ConfirmPolicy::default(),
        )
        .await?;
        match outcome {
            RefundExecuteOutcome::AlreadyRefunded { signature } => {
                println!(
                    "request {request_id}: already Refunded (tx {}) — nothing to do, no \
                     mutation performed",
                    signature.as_deref().unwrap_or("<unrecorded>")
                );
            }
            RefundExecuteOutcome::Confirmed { signature } => {
                println!(
                    "request {request_id}: refund CONFIRMED at finalized commitment (tx \
                     {signature}) — request is now Refunded, permanently closed (note: {note})"
                );
            }
        }
        Ok(())
    })
}

fn print_refund_report(report: &refund::RefundDryRunReport) {
    let request = &report.request;
    println!("Refund review for request {}:", request.id);
    println!("  state                     = {:?}", request.state);
    println!(
        "  manual review reason      = {}",
        report
            .db_checks
            .manual_review_reason
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  gross deposited (canonical, 8 dec) = {}",
        request.gross_amount_atomic
    );
    match &report.plan {
        Ok(plan) => {
            println!(
                "  original deposit          = WithdrawalObligation #{} at {} (finalized, \
                 on-chain; the bridge stores no deposit tx signature — this obligation account \
                 IS the verified deposit record)",
                plan.obligation_index, plan.obligation_pda
            );
            println!("  original sender (owner)   = {}", plan.requester);
            println!(
                "  source token account      = {} (the deposit's canonical ATA — same account \
                 the refund returns to)",
                plan.destination_token_account
            );
            println!(
                "  refund destination        = {} ({})",
                plan.destination_token_account,
                if plan.destination_exists {
                    "exists"
                } else {
                    "missing — will be created idempotently at execute, submitter-paid"
                }
            );
            println!("  reserve mint              = {}", plan.reserve_mint);
            println!(
                "  token program             = {} (Token-2022 per BridgeConfig)",
                plan.token_program
            );
            println!(
                "  refund amount (native, {} dec) = {} — exact gross deposit; no fee applies \
                 (SolToGlc fees accrue only at settlement, never reached)",
                plan.mint_decimals, plan.amount_solana_atomic
            );
            println!(
                "  refund nonce              = {:#x} (refund domain | request id; PDA {})",
                plan.nonce, plan.nonce_pda
            );
            println!("  reserve balance (before)  = {}", plan.reserve_balance);
            println!(
                "  reserve balance (after)   = {}",
                plan.reserve_balance
                    .saturating_sub(plan.amount_solana_atomic)
            );
            println!("  protected minimum         = {}", plan.protected_minimum);
            println!(
                "  bridge globally paused    = {} (required at execute)",
                plan.bridge_paused
            );
            println!(
                "  attestation               = {} of {} keys required, epoch {}",
                plan.attestation_threshold,
                plan.attestation_keys.len(),
                plan.attestation_epoch
            );
        }
        Err(e) => println!("  chain-side verification   = FAILED: {e}"),
    }
    println!(
        "  Goldcoin payout exists    = {}",
        !report.db_checks.no_goldcoin_payout
    );
    match &report.refund {
        Some(r) => println!(
            "  prior refund              = state {} (nonce {:#x}, tx {})",
            r.state.as_str(),
            r.nonce,
            r.refund_signature.as_deref().unwrap_or("<none>")
        ),
        None => println!("  prior refund              = none"),
    }
    println!("\n  Safety checks:");
    for check in &report.checks {
        println!(
            "    [{}] {}{}",
            if check.ok { "PASS" } else { "FAIL" },
            check.name,
            if check.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", check.detail)
            }
        );
    }
    println!(
        "\n  overall: {}",
        if report.already_refunded {
            "ALREADY REFUNDED — terminal; --execute would report the existing transaction and \
             change nothing"
        } else if report.would_execute {
            "ELIGIBLE — --execute would proceed (all checks re-run against fresh state first)"
        } else if report.eligible_ignoring_pause {
            "ELIGIBLE, PENDING GLOBAL PAUSE — every request-level check passes. Engage the \
             on-chain global pause (glc-admin onchain-pause --scope global --note ...), then \
             rerun with --execute; unpause explicitly afterwards"
        } else {
            "NOT ELIGIBLE — --execute would refuse (no override exists)"
        }
    );
}

/// Read-only refund visibility — never mutates anything.
fn cmd_refund_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let refunds = ledger
        .list_solana_refunds(open_only)
        .map_err(|e| e.to_string())?;
    if refunds.is_empty() {
        println!(
            "no {}refund lifecycles recorded",
            if open_only { "open " } else { "" }
        );
        return Ok(());
    }
    for r in refunds {
        println!(
            "request {}: {} — obligation #{}, amount {} native, requester {}, destination {}, \
             nonce {:#x}, tx {}, reason {}, by {} (note: {})",
            r.request_id,
            r.state.as_str(),
            r.obligation_index,
            r.amount_solana_atomic,
            solana_sdk::pubkey::Pubkey::from(r.requester),
            solana_sdk::pubkey::Pubkey::from(r.destination_token_account),
            r.nonce,
            r.refund_signature.as_deref().unwrap_or("<none>"),
            r.manual_review_reason,
            r.created_by,
            r.note,
        );
    }
    Ok(())
}

/// Retroactively marks an `unmatched_goldcoin_deposits` row reconciled —
/// for rows recorded before `goldcoin::indexer` learned to recognize vault
/// split outputs (docs/09-runbook.md "Vault UTXO splitting"). Never
/// deletes the row; refuses (no override) unless it exactly matches a
/// known `Broadcast` split's expected output, the same check the indexer
/// itself now applies live going forward.
fn cmd_reconcile_unmatched_deposit(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let txid_hex = require(args, "--txid");
    let vout: u32 = require(args, "--vout")
        .parse()
        .map_err(|e| format!("--vout must be a non-negative integer: {e}"))?;
    let note = require_note(args)?;
    let txid: [u8; 32] = hex::decode_exact(txid_hex)
        .map_err(|e| format!("--txid must be 64 hex characters: {e}"))?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let outcome = ledger
        .reconcile_unmatched_goldcoin_deposit(txid, vout, note, now_unix())
        .map_err(|e| e.to_string())?;
    match outcome {
        ReconcileUnmatchedDepositOutcome::Reconciled => {
            println!(
                "unmatched deposit {txid_hex}:{vout} marked reconciled (note: {note}) — row kept for audit, not deleted"
            );
        }
        ReconcileUnmatchedDepositOutcome::AlreadyReconciled => {
            println!(
                "unmatched deposit {txid_hex}:{vout} was already reconciled — nothing to do, no mutation performed"
            );
        }
    }
    Ok(())
}

fn cmd_show_config(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let pda = accounts::bridge_config_pda();
        let account = rpc
            .get_account(&pda)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!("bridge_config does not exist at {pda} — not initialized on this cluster")
            })?;
        let config = accounts::decode_bridge_config(&account.data).map_err(|e| e.to_string())?;
        println!("bridge_config ({pda}):");
        println!("  paused (global)    = {}", config.paused);
        println!("  release_paused     = {}", config.release_paused);
        println!("  deposit_paused     = {}", config.deposit_paused);
        println!("  reserve_token_mint = {}", config.reserve_token_mint);
        println!("  reserve_token_program = {}", config.reserve_token_program);
        println!("  obligation_count   = {}", config.obligation_count);
        println!("  protected_minimum  = {}", config.protected_minimum);
        println!("  per_transfer_limit = {}", config.per_transfer_limit);
        println!(
            "  rolling_volume_limit   = {} (GLOBAL, per direction — one field bounds both; \
             see docs/09-runbook.md 2026-08-22 update)",
            config.rolling_volume_limit
        );
        println!(
            "  rolling_window_seconds = {}",
            config.rolling_window_seconds
        );

        // Rolling-24h-volume quota, read live per direction — a read-only
        // projection (never itself a pause; see `accounts::
        // rolling_volume_remaining`'s docs). Never auto-clears on its
        // own reset alone requiring operator action: the window resets
        // on its own at the next bucket boundary regardless of anything
        // an operator does; only an explicit onchain-pause/-unpause ever
        // needs a human.
        let now = now_unix();
        for (label, direction_byte) in [("release (GlcToSol)", 0u8), ("deposit (SolToGlc)", 1u8)]
        {
            let window_pda = accounts::rolling_volume_window_pda(direction_byte);
            match rpc.get_account(&window_pda).await {
                Ok(Some(account)) => match accounts::decode_rolling_volume_window(&account.data) {
                    Ok(window) => {
                        let remaining = accounts::rolling_volume_remaining(
                            config.rolling_volume_limit,
                            config.rolling_window_seconds,
                            window,
                            now,
                        );
                        let exhausted = remaining < config.min_transfer_amount;
                        println!(
                            "  rolling_volume_window[{label}] ({window_pda}): remaining = {remaining} \
                             quota_exhausted = {exhausted}"
                        );
                    }
                    Err(e) => println!(
                        "  rolling_volume_window[{label}] ({window_pda}): could not decode: {e}"
                    ),
                },
                Ok(None) => println!(
                    "  rolling_volume_window[{label}] ({window_pda}): does not exist yet"
                ),
                Err(e) => println!(
                    "  rolling_volume_window[{label}] ({window_pda}): could not read: {e}"
                ),
            }
        }
        Ok(())
    })
}

fn cmd_onchain_pause(args: &[String], paused: bool) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let scope = parse_pause_scope(require(args, "--scope"))?;
    let note = require_note(args)?;
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::set_paused(&admin.pubkey(), scope, paused);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted set_paused(scope={scope:?}, paused={paused}) as {signature} (note: {note})"
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");
        Ok(())
    })
}

fn cmd_set_limit(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let field = parse_limit_field(require(args, "--field"))?;
    let new_value = require_u64(args, "--value")?;
    let note = require_note(args)?;
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::set_limit(&admin.pubkey(), field, new_value);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted set_limit(field={field:?}, new_value={new_value}) as {signature} (note: {note})"
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");
        Ok(())
    })
}

/// Administrative override of the rolling-volume anti-drain protection —
/// see the USAGE banner and `programs/glc-reserve-bridge/src/instructions/
/// admin.rs`'s `reset_rolling_volume_window` doc comment for the full rule
/// (admin-gated, requires `BridgeConfig.paused` already `true`, touches
/// only the selected direction's window). `--note` is required and, same
/// as every other on-chain command here, is recorded only in this
/// command's own printed output and the transaction history itself — this
/// CLI has no separate local audit-log file for on-chain actions.
fn cmd_reset_rolling_window(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url");
    let keypair_path = require(args, "--keypair");
    let direction = parse_rolling_window_direction(require(args, "--direction"))?;
    let note = require_note(args)?;
    let admin = read_keypair_file(keypair_path)
        .map_err(|e| format!("could not read keypair {keypair_path}: {e}"))?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.to_string());
        let ix = instructions::reset_rolling_volume_window(&admin.pubkey(), direction);
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&admin.pubkey()), &[&admin], blockhash);
        let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
        println!(
            "submitted reset_rolling_volume_window(direction={direction:?}) as {signature} (note: {note})"
        );
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| e.to_string())?;
        println!("confirmed.");
        Ok(())
    })
}

// ---------------------------------------------- goldcoin payout recovery --
//
// Unlike rebalancing/key-rotation above, this command DOES sign and
// broadcast a real transaction — but never a NEW one: it only completes a
// payout `Orchestrator::build_and_broadcast_payout` already independently
// signed and left stuck after a broadcast rejection
// (`goldcoin::payout_recovery` module docs). This is why it needs
// `--config`, not just `--db`: broadcasting requires the same configured
// vault signers and Goldcoin RPC the daemon itself uses, loaded exactly
// the same mode-gated way (`Config::load_signers`).

fn cmd_retry_goldcoin_payout(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let request_id = require_i64(args, "--request-id")?;
    let note = require_note(args)?;

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let (_attestation_signers, vault_signers) =
            config.load_signers().await.map_err(|e| e.to_string())?;
        let vault = MultisigVault::new(
            config.operators.vault_pubkeys.clone(),
            config.operators.vault_threshold,
            config.goldcoin.network,
        )
        .map_err(|e| e.to_string())?;
        let goldcoin_rpc = GoldcoinRpcClient::new(&GoldcoinRpcConfig {
            url: config.goldcoin.rpc_url.clone(),
            user: config.goldcoin.rpc_user.clone(),
            password: config.goldcoin.rpc_password.clone(),
            connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
            read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
        })
        .map_err(|e| e.to_string())?;
        let mut ledger =
            Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;

        let previous = ledger
            .get_goldcoin_payout_full(request_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no Goldcoin payout record exists for request {request_id}"))?;
        println!(
            "request {request_id}: current payout state = {} (note: {note})",
            previous.state
        );

        let policy = glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy {
            fee_rate_per_kb: config.goldcoin.fee_rate_per_kb,
            dust_threshold: config.goldcoin.dust_threshold,
            max_inputs: config.goldcoin.max_inputs,
            change_fanout_target_atomic: config.goldcoin.change_fanout_target_atomic,
            change_fanout_max_outputs: config.goldcoin.change_fanout_max_outputs,
            zero_conf_change_max_depth: config.goldcoin.zero_conf_change_max_depth,
            zero_conf_change_mode: config.goldcoin.zero_conf_change_mode,
            zero_conf_change_recursive_chain_limit: config.goldcoin.zero_conf_change_recursive_chain_limit,
        };
        let outcome = recover_stuck_goldcoin_payout(
            &mut ledger,
            &vault,
            &vault_signers,
            &goldcoin_rpc,
            request_id,
            config.operators.vault_threshold as usize,
            &policy,
            config.goldcoin.network,
            Duration::from_millis(config.service.signer_timeout_ms),
            now_unix(),
        )
        .await
        .map_err(|e| e.to_string())?;

        match outcome {
            RecoveryOutcome::AlreadyDone { state } => {
                println!(
                    "request {request_id}: payout is already {state} — nothing to do, no mutation performed"
                );
            }
            RecoveryOutcome::Broadcast {
                txid,
                resigned_hex_changed,
            } => {
                println!(
                    "request {request_id}: recovered and broadcast, txid = {}",
                    glc_reserve_bridge_service::goldcoin::hex::encode(&txid)
                );
                if resigned_hex_changed {
                    println!(
                        "  the re-signed transaction differs from what was previously stored — \
                         the original broadcast likely failed due to the non-canonical (high-S) \
                         signature this recovery corrects."
                    );
                } else {
                    println!(
                        "  WARNING: the re-signed transaction is BYTE-IDENTICAL to what was \
                         previously stored. Re-signing did not change anything, so if the \
                         original broadcast was rejected, that rejection likely has a cause \
                         OTHER than signature canonicalization — investigate before assuming \
                         this fix alone resolves it for future requests."
                    );
                }
            }
        }
        Ok(())
    })
}

// ------------------------------------------------------- vault UTXO splitting --
//
// Proactively fragments one large mature root-vault UTXO into several
// smaller ones, all still paying the vault's own script (never a derived
// or external destination) — see `goldcoin::split`/`signing::
// goldcoin_split` module docs and docs/09-runbook.md's "Vault UTXO
// splitting" section. Reuses the same `--config`-based wiring
// `cmd_retry_goldcoin_payout` above uses, for the identical reason: this
// needs the configured vault signers + Goldcoin RPC, not just the ledger.

// The chunk-target default is the config's own canonical
// `change_fanout_target_atomic` — ONE payout-chunk sizing for the whole
// service (2026-08-30 review: a hardcoded 12,500 GLC here silently
// diverged from the retuned 5,000 GLC canonical target). `--chunk-target-
// atomic` still overrides it for a deliberate one-off.

#[allow(clippy::too_many_arguments)]
fn print_split_plan(
    txid_hex: &str,
    vout: u32,
    plan: &split::SplitPlan,
    chunk_target_atomic: u64,
    current_mature_reserve: u64,
    protected_minimum: u64,
    pending_obligations: u64,
    mature_reserve_during_window: u64,
    reserve_after_fee: u64,
    required_floor: u64,
    safety_ok: bool,
) {
    println!("Split plan");
    println!("  source UTXO:                 {txid_hex}:{vout}");
    println!(
        "  source amount (atomic):      {}",
        plan.source.amount_atomic
    );
    println!("  chunk target (atomic):       {chunk_target_atomic}");
    println!("  outputs:                     {}", plan.output_count());
    for (i, amount) in plan.output_amounts.iter().enumerate() {
        println!("    output[{i}] (atomic):        {amount}");
    }
    println!("  total fee (atomic):          {}", plan.fee_atomic);
    println!(
        "  destination (all outputs):   vault script {}",
        hex::encode(&plan.vault_script_pubkey)
    );
    println!();
    println!("Reserve effect");
    println!("  current mature reserve (atomic):      {current_mature_reserve}");
    println!("  protected minimum (atomic):           {protected_minimum}");
    println!("  pending obligations (atomic):         {pending_obligations}");
    println!("  mature reserve during maturity window (atomic): {mature_reserve_during_window}");
    println!("  reserve value after fee (atomic):     {reserve_after_fee}");
    println!("  required floor (atomic):              {required_floor}");
    println!(
        "  safety check:                         {}",
        if safety_ok { "PASS" } else { "FAIL" }
    );
}

fn cmd_split_vault_utxo(args: &[String]) -> Result<(), String> {
    let config_path = require(args, "--config");
    let txid_hex = require(args, "--txid").to_string();
    let vout: u32 = require(args, "--vout")
        .parse()
        .map_err(|e| format!("--vout must be a non-negative integer: {e}"))?;
    let chunk_target_override: Option<u64> =
        match flag(args, "--chunk-target-atomic") {
            Some(s) => Some(s.parse().map_err(|e| {
                format!("--chunk-target-atomic must be a non-negative integer: {e}")
            })?),
            None => None,
        };
    let note = require_note(args)?;
    let execute = args.iter().any(|a| a == "--execute");
    let abandon = args.iter().any(|a| a == "--abandon");
    if abandon && !execute {
        return Err("--abandon requires --execute (abandonment is a mutation)".to_string());
    }

    let txid: [u8; 32] = hex::decode_exact(&txid_hex)
        .map_err(|e| format!("--txid must be 64 hex characters: {e}"))?;

    let config = Config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let chunk_target_atomic =
        chunk_target_override.unwrap_or(config.goldcoin.change_fanout_target_atomic);
    // (Checked again after the ledger lookup: --abandon with NO live
    // split row is an error, never a fall-through into building a fresh
    // split — a command whose intent is to walk away from a transaction
    // must be structurally incapable of creating one. 2026-08-30
    // third-pass review, finding 3.)

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let (_attestation_signers, vault_signers) =
            config.load_signers().await.map_err(|e| e.to_string())?;
        let vault = MultisigVault::new(
            config.operators.vault_pubkeys.clone(),
            config.operators.vault_threshold,
            config.goldcoin.network,
        )
        .map_err(|e| e.to_string())?;
        let goldcoin_rpc = GoldcoinRpcClient::new(&GoldcoinRpcConfig {
            url: config.goldcoin.rpc_url.clone(),
            user: config.goldcoin.rpc_user.clone(),
            password: config.goldcoin.rpc_password.clone(),
            connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
            read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
        })
        .map_err(|e| e.to_string())?;
        let mut ledger = Ledger::open(&config.service.db_path).map_err(|e| e.to_string())?;

        // A LIVE (non-Abandoned) split row for this outpoint means the
        // lifecycle already owns it: this command never builds a second
        // one, but it never falsely reports a pending row as finished
        // either (the 2026-08-30 review found `Built` rows reported as
        // "already split — nothing to do", permanently strandable with
        // shaping disabled). `Built`/`Signed` resume, `Broadcast` runs
        // the same confirm/re-broadcast/abandon maintenance the daemon
        // tick runs, `Confirmed` is genuinely done — all through the
        // IDENTICAL `goldcoin::liquidity` lifecycle functions the daemon
        // uses, never a parallel implementation. An `Abandoned` prior
        // attempt does not appear here at all: the outpoint may be split
        // afresh below.
        if let Some(existing) = ledger
            .get_vault_utxo_split(txid, vout)
            .map_err(|e| e.to_string())?
        {
            if abandon {
                // The deliberate, operator-decided release valve for a
                // split the automatic lifecycle cannot finish. Refused
                // outright for a Confirmed split — that one already
                // happened.
                if existing.state == "Confirmed" {
                    return Err(format!(
                        "split #{} is Confirmed — a completed split cannot be abandoned",
                        existing.id
                    ));
                }
                // Any split with SIGNED BYTES (Signed or Broadcast) is
                // only abandonable when the node DEFINITELY does not
                // know its transaction (2026-08-31 production-readiness
                // review, B1/H1): a Signed row can mean "broadcast
                // pre-crash, bookkeeping never recorded", so its txid is
                // derived from the stored bytes exactly as the daemon's
                // resume does; and an RPC failure NEVER means "absent" —
                // it fails closed and refuses. Only a Built row (nothing
                // was ever signed, no transaction can exist) skips the
                // probe.
                let probe_txid: Option<[u8; 32]> = match existing.state.as_str() {
                    "Built" => None,
                    _ => match (existing.txid, existing.signed_tx_hex.as_deref()) {
                        (Some(t), _) => Some(t),
                        (None, Some(signed_hex)) => {
                            let bytes = hex::decode_vec(signed_hex).map_err(|e| {
                                format!(
                                    "split #{}: stored signed_tx_hex is not valid hex ({e}) — \
                                     refusing to abandon what cannot be probed",
                                    existing.id
                                )
                            })?;
                            Some(
                                glc_reserve_bridge_service::goldcoin::tx::txid_of_serialized(
                                    &bytes,
                                ),
                            )
                        }
                        (None, None) => {
                            return Err(format!(
                                "split #{} is {} but has no txid or signed bytes — refusing \
                                 to abandon inconsistent state",
                                existing.id, existing.state
                            ));
                        }
                    },
                };
                if let Some(t_hex) = probe_txid.map(|t| hex::encode(&t)) {
                    match liquidity::probe_transaction(&goldcoin_rpc, &t_hex).await {
                        liquidity::TxProbe::Absent => {} // provably unknown: abandonable
                        liquidity::TxProbe::Known => {
                            return Err(format!(
                                "split #{} ({}): the node still knows its transaction \
                                 ({t_hex}) — it can confirm at any moment; refusing to \
                                 abandon live in-flight value. If it is stuck, wait for \
                                 eviction or investigate the transaction itself.",
                                existing.id, existing.state
                            ));
                        }
                        liquidity::TxProbe::Unknown(e) => {
                            return Err(format!(
                                "split #{} ({}): cannot verify whether the node knows \
                                 transaction {t_hex} ({e}) — refusing to abandon on \
                                 uncertainty; retry when the node is reachable",
                                existing.id, existing.state
                            ));
                        }
                    }
                }
                ledger
                    .abandon_vault_utxo_split(
                        existing.id,
                        &format!("operator abandon via split-vault-utxo: {note}"),
                        // The derived txid is persisted onto the row so
                        // the daemon's re-adoption watch covers this
                        // abandonment even for Signed rows (final
                        // review, finding 4).
                        probe_txid,
                        now_unix(),
                    )
                    .map_err(|e| e.to_string())?;
                println!(
                    "split #{} ({}) ABANDONED by operator decision — audit row kept; any \
                     phantom chunk rows were marked Spent. {}",
                    existing.id,
                    existing.state,
                    if existing.state == "Built" {
                        "The source outpoint is released back to the pool."
                    } else {
                        "The source outpoint stays Spent — its signed spender could resurface. \
                         If the node reports the transaction within the next 24h the daemon \
                         re-adopts the split automatically; after that, recovery of \
                         chain-resurrected value is a reserve-custody runbook decision."
                    }
                );
                return Ok(());
            }
            match existing.state.as_str() {
                "Confirmed" => {
                    println!(
                        "vault UTXO {txid_hex}:{vout} was already split and the split confirmed \
                         (split #{}, txid={}, {} chunk(s)) — nothing to do (note: {note})",
                        existing.id,
                        existing
                            .txid
                            .map(|t| hex::encode(&t))
                            .unwrap_or_else(|| "<none>".to_string()),
                        existing.chunk_count
                    );
                    return Ok(());
                }
                "Broadcast" => {
                    if !execute {
                        println!(
                            "split #{} for {txid_hex}:{vout} is Broadcast and awaiting \
                             confirmation — re-run with --execute to run lifecycle maintenance \
                             (confirmation check / eviction re-broadcast) now; the daemon's \
                             shaping tick does the same automatically",
                            existing.id
                        );
                        return Ok(());
                    }
                    let mut outcome = liquidity::ShapingOutcome::default();
                    liquidity::maintain_broadcast_splits(
                        &mut ledger,
                        &goldcoin_rpc,
                        Some(existing.id),
                        config.goldcoin.vault_min_confirmations,
                        &mut outcome,
                        now_unix(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    print_lifecycle_outcome(&outcome);
                    return Ok(());
                }
                "Built" | "Signed" => {
                    if !execute {
                        println!(
                            "split #{} for {txid_hex}:{vout} is {} but not yet Broadcast — \
                             re-run with --execute to resume it ({}); the daemon's shaping tick \
                             does the same automatically",
                            existing.id,
                            existing.state,
                            if existing.state == "Signed" {
                                "re-submits the EXACT already-signed transaction, no new signer \
                                 round-trip"
                            } else {
                                "re-signs the exact persisted plan through the independent \
                                 2-of-3 path"
                            }
                        );
                        return Ok(());
                    }
                    println!(
                        "split #{} for {txid_hex}:{vout} is {} — resuming (note: {note})...",
                        existing.id, existing.state
                    );
                    let pending = PendingVaultUtxoSplit {
                        id: existing.id,
                        source_txid: txid,
                        source_vout: vout,
                        state: existing.state.clone(),
                    };
                    let mut outcome = liquidity::ShapingOutcome::default();
                    liquidity::resume_pending_split(
                        &mut ledger,
                        &goldcoin_rpc,
                        &vault,
                        &vault_signers,
                        config.operators.vault_threshold as usize,
                        Duration::from_millis(config.service.signer_timeout_ms),
                        &pending,
                        &mut outcome,
                        now_unix(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    print_lifecycle_outcome(&outcome);
                    return Ok(());
                }
                other => {
                    return Err(format!(
                        "split #{} for {txid_hex}:{vout} is in unexpected state {other} — \
                         refusing to act",
                        existing.id
                    ));
                }
            }
        }

        if abandon {
            return Err(format!(
                "--abandon: no live split exists for {txid_hex}:{vout} (it may already be \
                 Abandoned) — refusing to do anything else under an abandon command"
            ));
        }

        // The full plan — source UTXO, output count, per-output amount,
        // fee, and the resulting mature-reserve effect — is computed and
        // printed here directly, BEFORE any signer is ever contacted, in
        // both dry-run and --execute runs. Uses exactly the same checks
        // `LedgerSplitSource` below independently re-runs per signer, so
        // what's printed here is exactly what gets signed.
        let row = ledger
            .get_vault_utxo(txid, vout)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no vault UTXO {txid_hex}:{vout} is known to this ledger"))?;
        if row.state != "Available" {
            return Err(format!(
                "vault UTXO {txid_hex}:{vout} is not available to split — state is {}, not Available",
                row.state
            ));
        }
        if !row
            .script_pubkey_hex
            .eq_ignore_ascii_case(&vault.script_pubkey_hex())
        {
            return Err(format!(
                "vault UTXO {txid_hex}:{vout} does not belong to the root vault — splitting is \
                 refused for a per-request derived deposit address"
            ));
        }
        let source = VaultUtxo {
            txid,
            vout,
            amount_atomic: row.amount_atomic,
            script_pubkey_hex: row.script_pubkey_hex,
        };
        // Same output-count bound the daemon applies (2026-08-31 final
        // review, finding 9): a very large source splits into at most
        // `utxo_shaping_max_outputs_per_split` correspondingly larger
        // chunks — never a hundreds-of-outputs transaction the network
        // would refuse after signing.
        let effective_chunk_target = source
            .amount_atomic
            .div_ceil(config.goldcoin.utxo_shaping_max_outputs_per_split as u64)
            .max(chunk_target_atomic);
        if effective_chunk_target != chunk_target_atomic {
            println!(
                "note: chunk target raised {chunk_target_atomic} -> {effective_chunk_target} \
                 atomic to respect the {}-output cap (each chunk is itself a later split \
                 candidate)",
                config.goldcoin.utxo_shaping_max_outputs_per_split
            );
        }
        let chunk_target_atomic = effective_chunk_target;
        let plan = split::plan_split(
            &source,
            &vault,
            chunk_target_atomic,
            config.goldcoin.fee_rate_per_kb,
        )
        .map_err(|e| e.to_string())?;

        let (current_mature_reserve, protected_minimum, _reserved_liquidity, pending_obligations) =
            ledger
                .reserve_snapshot(ReserveDirection::GoldcoinReserve)
                .map_err(|e| e.to_string())?;
        // Solvency-invariant-aligned check (2026-08-30, see
        // `signing::goldcoin_split::LedgerSplitSource` — the exact same
        // formula every signer independently re-runs): only the network
        // fee genuinely leaves the vault; every chunk output pays the
        // vault's own script and is ledger-tracked as known internal
        // value from broadcast. `mature_reserve_during_window` is printed
        // for operator awareness (how much stays individually spendable
        // while the chunks mature), but the refusal itself is on
        // `reserve_after_fee`.
        let mature_reserve_during_window =
            current_mature_reserve.saturating_sub(source.amount_atomic);
        let reserve_after_fee = current_mature_reserve.saturating_sub(plan.fee_atomic);
        let required_floor = protected_minimum + pending_obligations;
        let safety_ok = reserve_after_fee >= required_floor;

        print_split_plan(
            &txid_hex,
            vout,
            &plan,
            chunk_target_atomic,
            current_mature_reserve,
            protected_minimum,
            pending_obligations,
            mature_reserve_during_window,
            reserve_after_fee,
            required_floor,
            safety_ok,
        );

        if !safety_ok {
            println!(
                "\nRefused: this split would drop reserve value below the required floor. \
                 No signer was contacted. No transaction was broadcast. There is no override \
                 for this check."
            );
            return Err(format!(
                "refusing unsafe split: reserve_after_fee={reserve_after_fee} < \
                 required_floor={required_floor} (protected_minimum + pending_obligations)"
            ));
        }
        // Payout-liveness guard, identical to the daemon's and equally
        // non-overridable (2026-08-30 third-pass review, finding 5):
        // splitting takes the source's full value out of the MATURE pool
        // for the chunks' maturity window, and already-admitted
        // obligations need mature liquidity now — the rest of the pool
        // must cover them without this UTXO.
        let mature_total: u64 = ledger
            .available_vault_utxos()
            .map_err(|e| e.to_string())?
            .iter()
            .map(|u| u.amount_atomic)
            .sum();
        if mature_total.saturating_sub(source.amount_atomic) < pending_obligations {
            println!(
                "\nRefused: splitting this UTXO would leave the mature pool below the \
                 {pending_obligations} atomic units of already-admitted obligations — payouts \
                 keep first claim on mature liquidity. No signer was contacted. There is no \
                 override; retry once obligations drain or change matures."
            );
            return Err(format!(
                "refusing split for payout liveness: mature pool without this UTXO = {} < \
                 pending_obligations = {pending_obligations}",
                mature_total.saturating_sub(source.amount_atomic)
            ));
        }

        if !execute {
            println!(
                "\n--execute not supplied — plan assembled and safety-checked only. No signer \
                 was contacted. No transaction was broadcast. Re-run with --execute to sign and \
                 broadcast this exact plan."
            );
            return Ok(());
        }

        let threshold = config.operators.vault_threshold as usize;
        println!(
            "\n--execute supplied — claiming the source outpoint, then contacting {threshold} \
             of {} vault signers (the claim, not the signing order, is what makes a concurrent \
             daemon payout unable to race this source)...",
            vault_signers.len()
        );
        match liquidity::execute_fresh_split(
            &mut ledger,
            &goldcoin_rpc,
            &vault,
            &vault_signers,
            threshold,
            Duration::from_millis(config.service.signer_timeout_ms),
            &source,
            chunk_target_atomic,
            config.goldcoin.fee_rate_per_kb,
            note,
            now_unix(),
        )
        .await
        .map_err(|e| e.to_string())?
        {
            liquidity::FreshSplitOutcome::Broadcast { txid: broadcast_txid } => {
                println!(
                    "broadcast outcome: Accepted, txid = {}",
                    hex::encode(&broadcast_txid)
                );
                Ok(())
            }
            liquidity::FreshSplitOutcome::RefusedFloor {
                reserve_after_fee,
                required_floor,
            } => Err(format!(
                "refusing unsafe split: reserve_after_fee={reserve_after_fee} < \
                 required_floor={required_floor} (protected_minimum + pending_obligations)"
            )),
            liquidity::FreshSplitOutcome::Abandoned { split_id, reason } => Err(format!(
                "split #{split_id} could not proceed and was abandoned ({reason}) — the source \
                 outpoint is released; investigate, then re-run if appropriate"
            )),
            liquidity::FreshSplitOutcome::Deferred { split_id, reason } => Err(format!(
                "split #{split_id} was signed but its broadcast was refused ({reason}) — the \
                 row remains Signed and the daemon's resume path (or a re-run of this command) \
                 will drive it; --abandon --execute is the deliberate walk-away"
            )),
        }
    })
}

/// Prints what a `goldcoin::liquidity` lifecycle call actually did, in
/// the CLI's own voice.
fn print_lifecycle_outcome(
    outcome: &glc_reserve_bridge_service::goldcoin::liquidity::ShapingOutcome,
) {
    for id in &outcome.confirmed_split_ids {
        println!("split #{id}: first confirmation observed — marked Confirmed");
    }
    if let Some(txid) = outcome.rebroadcast_split_txid {
        println!(
            "re-broadcast evicted split transaction: txid = {}",
            hex::encode(&txid)
        );
    }
    if let Some(txid) = outcome.resumed_split_txid {
        println!("resumed split to Broadcast: txid = {}", hex::encode(&txid));
    }
    if let Some((id, reason)) = &outcome.abandoned_split {
        println!(
            "split #{id} ABANDONED: {reason} — its source outpoint is released; the audit row \
             is kept"
        );
    }
    if let Some(err) = &outcome.lifecycle_error {
        println!("lifecycle error (state unchanged, safe to retry): {err}");
    }
    if outcome.confirmed_split_ids.is_empty()
        && outcome.rebroadcast_split_txid.is_none()
        && outcome.resumed_split_txid.is_none()
        && outcome.abandoned_split.is_none()
        && outcome.lifecycle_error.is_none()
    {
        println!("no lifecycle action was needed");
    }
}

// -------------------------------------------------------------- rebalancing --
//
// This service never signs or broadcasts a fund-moving transaction for a
// rebalance — see the module-level docs on `ledger::Ledger`'s rebalance
// methods and docs/22-production-readiness-review.md. Every command here
// either reads state, records an approval/decision, or records EVIDENCE
// of a transfer an operator already executed through real custody
// tooling outside this system.

fn cmd_rebalance_status(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        match rebalance::assess(&ledger, direction) {
            Ok(a) => {
                println!(
                    "{direction:?}: severity={:?} balance={} target={} warning={} critical={} \
                     protected_minimum={}",
                    a.severity,
                    a.total_reserve_balance,
                    a.target_reserve,
                    a.warning_reserve,
                    a.critical_reserve,
                    a.protected_minimum
                );
                if let Some(suggested) = a.suggested_deposit_atomic {
                    println!(
                        "  suggested: a Deposit of {suggested} would restore target_reserve \
                         (sizing only — propose explicitly with rebalance-propose)"
                    );
                }
            }
            Err(e) => println!("{direction:?}: not configured ({e})"),
        }
    }
    Ok(())
}

fn cmd_rebalance_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = flag(args, "--direction")
        .map(parse_reserve_direction)
        .transpose()?;
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let requests = ledger
        .list_rebalances(direction, open_only)
        .map_err(|e| e.to_string())?;
    if requests.is_empty() {
        println!("no rebalance requests found");
    }
    for r in requests {
        println!(
            "#{} {:?} {:?} amount={} state={:?} reason={:?} requested_by={} approvals={}/{}{}",
            r.id,
            r.direction,
            r.kind,
            r.amount_atomic,
            r.state,
            r.reason,
            r.requested_by,
            r.approved_by.len(),
            r.required_approvals,
            r.tx_reference
                .map(|t| format!(" tx_reference={t}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn cmd_rebalance_propose(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_reserve_direction(require(args, "--direction"))?;
    let kind = parse_rebalance_kind(require(args, "--kind"))?;
    let amount = require_u64(args, "--amount")?;
    let by = require(args, "--by");
    let required_approvals: u32 = require(args, "--required-approvals")
        .parse()
        .map_err(|e| format!("--required-approvals must be a positive integer: {e}"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let id = ledger
        .propose_rebalance(
            direction,
            kind,
            amount,
            note,
            by,
            required_approvals,
            now_unix(),
        )
        .map_err(|e| e.to_string())?;
    println!("proposed rebalance #{id}: {direction:?} {kind:?} {amount} (requires {required_approvals} approval(s))");
    Ok(())
}

fn cmd_rebalance_approve(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let outcome = ledger
        .approve_rebalance(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id}: {outcome:?}");
    Ok(())
}

fn cmd_rebalance_reject(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .reject_rebalance(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} rejected (note: {note})");
    Ok(())
}

fn cmd_rebalance_cancel(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .cancel_rebalance(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} cancelled (note: {note})");
    Ok(())
}

fn cmd_rebalance_record_executed(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let tx_reference = require(args, "--tx-reference");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .record_rebalance_executed(id, tx_reference, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} recorded executed (tx_reference: {tx_reference}) — this command did NOT construct or broadcast any transaction");
    Ok(())
}

fn cmd_rebalance_confirm(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let observed_amount = require_u64(args, "--observed-amount")?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .confirm_rebalance(id, observed_amount, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} confirmed (observed_amount={observed_amount})");
    Ok(())
}

fn cmd_rebalance_fail(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .fail_rebalance(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("rebalance #{id} marked failed (note: {note})");
    Ok(())
}

// ----------------------------------------------------- key rotation / vault sweep --
//
// This service never generates keys, signs, or executes a real
// rotation/sweep for a custody transition — see the module-level docs on
// `ledger::Ledger`'s custody-transition methods and
// docs/22-production-readiness-review.md. Every command here either
// reads state, records a verification/approval/decision, or records
// EVIDENCE of a rotation/sweep an operator already executed through real
// custody tooling outside this system.

fn cmd_custody_list(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let kind = flag(args, "--kind").map(parse_custody_kind).transpose()?;
    let open_only = args.iter().any(|a| a == "--open-only");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let transitions = ledger
        .list_custody_transitions(kind, open_only)
        .map_err(|e| e.to_string())?;
    if transitions.is_empty() {
        println!("no custody transitions found");
    }
    for t in transitions {
        println!(
            "#{} {:?} state={:?} old={:?} new={:?}{} reason={:?} requested_by={} approvals={}/{}{}",
            t.id,
            t.kind,
            t.state,
            t.old_identities,
            t.new_identities,
            t.new_threshold
                .map(|n| format!(" new_threshold={n}"))
                .unwrap_or_default(),
            t.reason,
            t.requested_by,
            t.approved_by.len(),
            t.required_approvals,
            t.tx_reference
                .map(|tx| format!(" tx_reference={tx}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn cmd_custody_propose(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let kind = parse_custody_kind(require(args, "--kind"))?;
    let old_identities = parse_csv(require(args, "--old-identities"));
    let new_identities = parse_csv(require(args, "--new-identities"));
    let new_threshold = flag(args, "--new-threshold")
        .map(|s| {
            s.parse::<u32>()
                .map_err(|e| format!("--new-threshold must be a positive integer: {e}"))
        })
        .transpose()?;
    let by = require(args, "--by");
    let required_approvals: u32 = require(args, "--required-approvals")
        .parse()
        .map_err(|e| format!("--required-approvals must be a positive integer: {e}"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let id = ledger
        .propose_custody_transition(
            kind,
            &old_identities,
            &new_identities,
            new_threshold,
            note,
            by,
            required_approvals,
            now_unix(),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "proposed custody transition #{id}: {kind:?} (requires {required_approvals} approval(s))"
    );
    Ok(())
}

fn cmd_custody_verify_identity(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .verify_new_identity(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id}: new identity verified by {by}");
    Ok(())
}

fn cmd_custody_approve(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    let outcome = ledger
        .approve_custody_transition(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id}: {outcome:?}");
    Ok(())
}

fn cmd_custody_reject(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .reject_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} rejected (note: {note})");
    Ok(())
}

fn cmd_custody_cancel(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .cancel_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} cancelled (note: {note})");
    Ok(())
}

fn cmd_custody_record_executed(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let tx_reference = require(args, "--tx-reference");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .record_custody_transition_executed(id, tx_reference, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} recorded executed (tx_reference: {tx_reference}) — this command did NOT perform any rotation/sweep");
    Ok(())
}

fn cmd_custody_confirm(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .confirm_custody_transition(id, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} confirmed");
    Ok(())
}

fn cmd_custody_fail(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .fail_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} marked failed (note: {note})");
    Ok(())
}

fn cmd_custody_rollback(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let id = require_i64(args, "--id")?;
    let by = require(args, "--by");
    let note = require_note(args)?;
    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .rollback_custody_transition(id, note, by, now_unix())
        .map_err(|e| e.to_string())?;
    println!("custody transition #{id} marked rolled back (note: {note})");
    Ok(())
}
