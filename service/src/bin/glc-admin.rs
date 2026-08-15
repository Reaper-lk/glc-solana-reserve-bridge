//! `glc-admin` — reserve bridge operator CLI
//! (docs/07-implementation-plan.md Phase 5). Ported CLI shape and
//! mandatory `--note` audit discipline from the old bridge's `glc-admin`
//! (docs/01-reuse-inventory.md); governance/rotation/quorum-reassignment
//! subcommands (which depended on a P2P federation transport this bridge
//! does not have — see IMPLEMENTATION_LOG.md's Phase 5 entry) are
//! deliberately not ported. What's here: local status, this service's own
//! ledger-level directional pause (independent of the on-chain pause),
//! and the on-chain admin-gated `set_paused` instruction
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

use std::path::PathBuf;

use glc_reserve_bridge_service::ledger::{
    CustodyTransitionKind, Direction, Ledger, RebalanceKind, RequestState, ReserveDirection,
};
use glc_reserve_bridge_service::ops::reserve_health;
use glc_reserve_bridge_service::rebalance;
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::confirm::{confirm_transaction, ConfirmPolicy};
use glc_reserve_bridge_service::solana::instructions::{self, PauseScope};
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

use solana_sdk::signature::{read_keypair_file, Signer};
use solana_sdk::transaction::Transaction;

const USAGE: &str = "glc-admin — reserve bridge operator CLI

STATUS
  glc-admin status --db PATH

LOCAL LEDGER PAUSE (this service's own admission gate; independent of the
on-chain pause below — see docs/09-runbook.md)
  glc-admin pause   --db PATH --direction <goldcoin|solana> --note TEXT
  glc-admin unpause --db PATH --direction <goldcoin|solana> --note TEXT

ON-CHAIN (admin-gated-immediate; requires the BridgeConfig admin's keypair)
  glc-admin show-config    --rpc-url URL
  glc-admin onchain-pause   --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT
  glc-admin onchain-unpause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT

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
        "show-config" => cmd_show_config(&args),
        "onchain-pause" => cmd_onchain_pause(&args, true),
        "onchain-unpause" => cmd_onchain_pause(&args, false),
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

fn cmd_status(args: &[String]) -> Result<(), String> {
    let db = require(args, "--db");
    let ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;

    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        match reserve_health::check(&ledger, direction) {
            Ok(s) => println!(
                "{direction:?}: balance={} protected_minimum={} reserved_liquidity={} \
                 pending_obligations={} accrued_fees={} paused={} invariant_holds={}",
                s.total_reserve_balance,
                s.protected_minimum,
                s.reserved_liquidity,
                s.pending_obligations,
                s.accrued_fees,
                s.paused,
                s.invariant_holds
            ),
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

fn cmd_local_pause(args: &[String], paused: bool) -> Result<(), String> {
    let db = require(args, "--db");
    let direction = parse_reserve_direction(require(args, "--direction"))?;
    let note = require_note(args)?;

    let mut ledger =
        Ledger::open(&PathBuf::from(db)).map_err(|e| format!("could not open {db}: {e}"))?;
    ledger
        .set_paused(direction, paused, Some(note))
        .map_err(|e| e.to_string())?;
    println!("{direction:?} local ledger pause set to {paused} (note: {note})");
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
