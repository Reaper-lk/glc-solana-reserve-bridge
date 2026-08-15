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

use glc_reserve_bridge_service::ledger::{Direction, Ledger, RequestState, ReserveDirection};
use glc_reserve_bridge_service::ops::reserve_health;
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

Every mutating command requires --note (mandatory audit trail).";

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
                 pending_obligations={} paused={} invariant_holds={}",
                s.total_reserve_balance,
                s.protected_minimum,
                s.reserved_liquidity,
                s.pending_obligations,
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
