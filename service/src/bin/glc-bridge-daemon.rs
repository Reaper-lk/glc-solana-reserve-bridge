//! `glc-bridge-daemon` — the long-running reserve bridge process
//! (docs/15-post-phase6-audit.md P0 item 1). Loads production config
//! (`glc_reserve_bridge_service::config`), wires the real Goldcoin/Solana
//! RPC clients and this phase's DEV/TEST-posture signers into an
//! [`Orchestrator`], and drives it forever via
//! [`glc_reserve_bridge_service::daemon::run`] — reconciling both reserve
//! directions every tick — while serving `/health` and `/metrics`
//! alongside it until `SIGINT`/`SIGTERM`.
//!
//! # DEV/TEST KEY POSTURE ONLY
//!
//! Signing-key material is loaded from local plaintext files
//! (`config::Config::load_attestation_signers`/`load_vault_signers`/
//! `load_submitter`) — see those modules' docs and
//! docs/12-management-decisions.md item 2. Do not point this at
//! production keys or endpoints; the HSM/KMS-backed replacement for this
//! loading path is later, explicitly-approved work
//! (docs/15-post-phase6-audit.md P2).
//!
//! # Startup fails closed
//!
//! Every step below — config parsing, key loading and cross-validation,
//! vault construction, opening the ledger — exits non-zero with a clear
//! message rather than starting in a partially-configured state. Nothing
//! here guesses a default for a value the operator didn't provide.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glc_reserve_bridge_service::api::{self, BridgeApi};
use glc_reserve_bridge_service::config::Config;
use glc_reserve_bridge_service::daemon::{self, DaemonLoopConfig};
use glc_reserve_bridge_service::goldcoin::indexer::{Indexer, IndexerConfig};
use glc_reserve_bridge_service::goldcoin::rpc::{
    RpcClient as GoldcoinRpcClient, RpcConfig as GoldcoinRpcConfig,
};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, ReserveDirection};
use glc_reserve_bridge_service::ops::{self, collector::OpsCollector, health};
use glc_reserve_bridge_service::orchestrator::{Orchestrator, OrchestratorConfig};
use glc_reserve_bridge_service::signing::signers::{AttestationSigner, VaultSigner};
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::indexer::SolanaIndexer;
use glc_reserve_bridge_service::solana::rpc::RealSolanaRpc;

const USAGE: &str = "glc-bridge-daemon — long-running reserve bridge process

  glc-bridge-daemon --config PATH

Drives Orchestrator::tick() on an interval, reconciling both the Goldcoin
and Solana reserve directions every tick, and serves /health and /metrics
until SIGINT/SIGTERM (graceful — a tick in progress always finishes).

DEV/TEST KEY POSTURE ONLY (docs/12-management-decisions.md item 2) — do
not point this at production keys or endpoints.";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every startup step below either succeeds or exits(2) with a logged
/// reason — this is the one place that pattern lives, so `main` reads as
/// a plain sequence of steps rather than a wall of repeated match arms.
fn or_exit<T, E: std::fmt::Display>(result: Result<T, E>, doing: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "could not {doing} — refusing to start");
            std::process::exit(2);
        }
    }
}

fn open_ledger(path: &Path) -> Ledger {
    or_exit(Ledger::open(path), "open the ledger database")
}

fn goldcoin_rpc_config(config: &Config) -> GoldcoinRpcConfig {
    GoldcoinRpcConfig {
        url: config.goldcoin.rpc_url.clone(),
        user: config.goldcoin.rpc_user.clone(),
        password: config.goldcoin.rpc_password.clone(),
        connect_timeout_ms: config.goldcoin.rpc_connect_timeout_ms,
        read_timeout_ms: config.goldcoin.rpc_read_timeout_ms,
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Daemon convention: logs to stderr, stdout kept free for any
        // future structured output an operator might pipe elsewhere.
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let Some(config_path) = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
    else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let config = or_exit(Config::load(Path::new(config_path)), "load config");
    // `config.load_*_signers` returns this phase's DEV/TEST-posture
    // concrete signer type (see config.rs module docs); boxed into the
    // trait objects `Orchestrator` actually depends on here, at the one
    // place a real HSM/KMS-backed loader would produce
    // `Vec<Box<dyn VaultSigner>>`/`Vec<Box<dyn AttestationSigner>>`
    // directly instead (docs/22-production-readiness-review.md).
    let attestation_signers: Vec<Box<dyn AttestationSigner>> = or_exit(
        config.load_attestation_signers(),
        "load attestation signers",
    )
    .into_iter()
    .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
    .collect();
    let vault_signers: Vec<Box<dyn VaultSigner>> =
        or_exit(config.load_vault_signers(), "load vault signers")
            .into_iter()
            .map(|s| Box::new(s) as Box<dyn VaultSigner>)
            .collect();
    let submitter = or_exit(config.load_submitter(), "load the submitter key");
    let vault = or_exit(
        MultisigVault::new(
            config.operators.vault_pubkeys.clone(),
            config.operators.vault_threshold,
            config.goldcoin.network,
        ),
        "construct the vault from configured signer pubkeys",
    );
    tracing::info!(vault_address = %vault.address(), "vault constructed from configured signer set");
    let vault_address = vault.address().to_string();

    let goldcoin_cfg = goldcoin_rpc_config(&config);
    let goldcoin_rpc_for_indexer = or_exit(
        GoldcoinRpcClient::new(&goldcoin_cfg),
        "construct the Goldcoin RPC client",
    );
    let goldcoin_rpc_for_orchestrator = or_exit(
        GoldcoinRpcClient::new(&goldcoin_cfg),
        "construct the Goldcoin RPC client",
    );

    // Fail closed before anything else touches the chain: the configured
    // reserve mint must be owned by a supported SPL token program (legacy
    // SPL Token or Token-2022 — docs/18-token-2022-support.md), and, if
    // it's Token-2022, every extension it carries must be on the
    // explicitly reviewed allowlist (accounts::verify_reserve_mint_token_
    // program's docs explain why an unreviewed extension isn't just "not
    // tested" but actually unsafe to assume — extensions like transfer
    // fees/hooks would silently break the 1:1 reserve invariant). The
    // on-chain program's own constraints and `crate::token_extensions`
    // would already reject a bad mint/program/extension at the first
    // instruction that touched it, but failing here is clearer and
    // earlier — before any indexer/orchestrator wiring, let alone a real
    // transfer, is ever attempted.
    let mint_basics = or_exit(
        accounts::verify_reserve_mint_token_program(
            &RealSolanaRpc::new(config.solana.rpc_url.clone()),
            &config.solana.reserve_token_mint,
        )
        .await,
        "verify the configured reserve_token_mint's token program",
    );
    tracing::info!(
        decimals = mint_basics.decimals,
        supply = mint_basics.supply,
        mint_authority = ?mint_basics.mint_authority,
        freeze_authority = ?mint_basics.freeze_authority,
        token_program = %mint_basics.token_program,
        "reserve_token_mint verified: supported token program, extensions reviewed, decimals read live on every transfer"
    );

    // Idempotent every startup: only the bounds (protected_minimum/
    // target/warning/critical) are (re)applied from config on every run —
    // `Ledger::configure_reserve`'s `ON CONFLICT` never touches an
    // existing total_reserve_balance. `initial_balance: 0` here is
    // deliberate, not a placeholder to fix later: it is only ever
    // consulted the very first time a direction is configured, and
    // reconciliation's very next tick overwrites it with a real observed
    // balance regardless. Seeding anything other than 0 would risk baking
    // in a stale guess; 0 can only ever look like a balance *increase*
    // (never flagged as a breach — see `reconciliation::reconcile`),
    // never a false "unexplained drop" — the same cold-start reasoning
    // docs/15-post-phase6-audit.md documents for the equivalent race the
    // Phase 6 real-node tests hit.
    for (direction, bounds) in [
        (ReserveDirection::SolanaReserve, config.reserve.solana),
        (ReserveDirection::GoldcoinReserve, config.reserve.goldcoin),
    ] {
        let mut ledger = open_ledger(&config.service.db_path);
        or_exit(
            ledger.configure_reserve(
                direction,
                0,
                bounds.protected_minimum,
                bounds.target_reserve,
                bounds.warning_reserve,
                bounds.critical_reserve,
                now_unix(),
            ),
            "configure reserve bounds",
        );
    }

    let goldcoin_indexer = Indexer::new(
        goldcoin_rpc_for_indexer,
        open_ledger(&config.service.db_path),
        IndexerConfig {
            vault_script_hex: vault.script_pubkey_hex(),
            confirmation_depth: config.goldcoin.confirmation_depth,
            max_reorg_depth: config.goldcoin.max_reorg_depth,
        },
    );
    let solana_indexer = SolanaIndexer::new(
        RealSolanaRpc::new(config.solana.rpc_url.clone()),
        open_ledger(&config.service.db_path),
    );

    let orchestrator_config = OrchestratorConfig {
        attestation_threshold: config.operators.attestation_threshold,
        vault_threshold: config.operators.vault_threshold as usize,
        required_goldcoin_confirmations: config.goldcoin.required_payout_confirmations,
        fee_rate_per_kb: config.goldcoin.fee_rate_per_kb,
        dust_threshold: config.goldcoin.dust_threshold,
        max_inputs: config.goldcoin.max_inputs,
        reconciliation_tolerance: config.reserve.reconciliation_tolerance,
        vault_min_confirmations: config.goldcoin.vault_min_confirmations,
        goldcoin_network: config.goldcoin.network,
        signer_timeout: Duration::from_millis(config.service.signer_timeout_ms),
    };

    let mut orchestrator = Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        open_ledger(&config.service.db_path),
        goldcoin_rpc_for_orchestrator,
        RealSolanaRpc::new(config.solana.rpc_url.clone()),
        vault,
        vault_signers,
        attestation_signers,
        submitter,
        orchestrator_config,
        now_unix(),
    );

    let collector = Arc::new(OpsCollector::new(
        config.service.db_path.clone(),
        orchestrator.goldcoin_indexer_status(),
        orchestrator.solana_indexer_status(),
    ));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let health_addr: SocketAddr = config.service.health_bind_addr;
    let health_shutdown_rx = shutdown_rx.clone();
    let health_task = tokio::spawn(async move {
        if let Err(e) = health::serve(health_addr, collector, health_shutdown_rx).await {
            tracing::error!(error = %e, "health/metrics endpoint exited with an error");
        }
    });

    let api_task = config.service.api_bind_addr.map(|api_addr| {
        let api_source = Arc::new(BridgeApi::new(
            config.service.db_path.clone(),
            RealSolanaRpc::new(config.solana.rpc_url.clone()),
            vault_address,
            config.service.reservation_ttl_secs,
            i64::from(config.goldcoin.confirmation_depth),
            orchestrator.goldcoin_indexer_status(),
            orchestrator.solana_indexer_status(),
        ));
        let api_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = api::serve(api_addr, api_source, api_shutdown_rx).await {
                tracing::error!(error = %e, "bridge API exited with an error");
            }
        })
    });

    let alert_task = config.service.alert_webhook_url.clone().map(|webhook_url| {
        let alert_config = ops::alerting::AlertConfig {
            webhook_url,
            poll_interval: Duration::from_secs(config.service.alert_poll_interval_secs),
        };
        let alert_shutdown_rx = shutdown_rx.clone();
        let db_path = config.service.db_path.clone();
        tokio::spawn(ops::alerting::run(db_path, alert_config, alert_shutdown_rx))
    });

    let signal_shutdown_tx = shutdown_tx.clone();
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown signal received");
        let _ = signal_shutdown_tx.send(true);
    });

    let loop_config = DaemonLoopConfig {
        tick_interval: Duration::from_millis(config.service.tick_interval_ms),
        max_backoff: Duration::from_secs(60),
    };
    tracing::info!(
        tick_interval_ms = config.service.tick_interval_ms,
        health_addr = %health_addr,
        "glc-bridge-daemon starting"
    );
    let ticks_run = daemon::run(&mut orchestrator, loop_config, shutdown_rx, now_unix).await;
    tracing::info!(ticks_run, "tick loop stopped; shutting down");

    // The tick loop only stops once `shutdown_tx` is set — by the signal
    // task above in the normal case — so the health server is already
    // unwinding; make sure it's set regardless (e.g. if `run` somehow
    // returned on its own) and wait for the health task to actually exit
    // before this process does.
    let _ = shutdown_tx.send(true);
    let _ = health_task.await;
    if let Some(api_task) = api_task {
        let _ = api_task.await;
    }
    if let Some(alert_task) = alert_task {
        let _ = alert_task.await;
    }
    signal_task.abort();
}

/// Waits for either `SIGTERM` or `Ctrl+C` (`SIGINT`), whichever comes
/// first — both are graceful-shutdown requests to this process.
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            std::future::pending::<()>().await;
            return;
        };
        sigterm.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
