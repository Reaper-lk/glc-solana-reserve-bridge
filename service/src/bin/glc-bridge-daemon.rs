//! `glc-bridge-daemon` — the long-running reserve bridge process
//! (docs/15-post-phase6-audit.md P0 item 1). Loads production config
//! (`glc_reserve_bridge_service::config`), wires the real Goldcoin/Solana
//! RPC clients and the configured signers into an [`Orchestrator`], and
//! drives it forever via [`glc_reserve_bridge_service::daemon::run`] —
//! reconciling both reserve directions every tick — while serving
//! `/health` and `/metrics` alongside it until `SIGINT`/`SIGTERM`.
//!
//! # Signer loading is mode-gated — see `config::Config::load_signers`
//!
//! `operators.mode` in the config file selects the entire signer-loading
//! path: `"dev"` loads local plaintext key files
//! (`config::Config::load_attestation_signers`/`load_vault_signers`) —
//! DEV/TEST POSTURE ONLY, never point this at production keys. `
//! "production"` connects to the configured remote signer endpoints
//! instead (`signing::remote`, docs/26-production-signer-deployment.md)
//! — no private key material is ever loaded into this process in that
//! mode. `Config::load` itself already refuses to resolve a config where
//! `mode` and the populated signer fields disagree (see `config.rs`
//! module docs) — this binary never has to re-check that itself, only
//! call the one mode-aware entry point (`load_signers`).
//! `load_submitter` (the Solana fee-payer key) is unaffected by `mode` —
//! see that method's own docs for why it is not a custody authority.
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

use glc_reserve_bridge_service::admin_api::{self, auth::OperatorRegistry, AdminApi};
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
    tracing::info!(mode = ?config.operators.mode, "signer mode");
    // Mode-gated: `"dev"` loads local plaintext key files, `"production"`
    // connects to the configured remote signer endpoints — never both,
    // `Config::load` already refused to resolve a config where `mode`
    // and the populated signer fields disagree (config.rs module docs,
    // signing::remote module docs). Either way this returns the trait
    // objects `Orchestrator` actually depends on — this binary never
    // names a concrete signer type.
    let (attestation_signers, vault_signers): glc_reserve_bridge_service::config::LoadedSigners =
        or_exit(config.load_signers().await, "load signers");
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
    let root_vault_for_api = vault.clone();

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
        // UTXO-liquidity admission backpressure is Goldcoin-specific
        // (docs/09-runbook.md's "UTXO liquidity" section) — SolanaReserve
        // has no vault_utxos concept, so it's left at its default (0, 0),
        // i.e. no backpressure, same as never calling this at all.
        if direction == ReserveDirection::GoldcoinReserve {
            or_exit(
                ledger.set_utxo_pool_thresholds(
                    direction,
                    config.goldcoin.utxo_pool_min_available_count,
                    config.goldcoin.utxo_pool_warning_count,
                ),
                "configure UTXO pool thresholds",
            );
            // Applied at every startup with the current configuration,
            // same idempotent shape as the thresholds above. Restart
            // therefore re-asserts the configured buffer, while the
            // open/closed admission STATE itself is durable ledger state
            // and is deliberately left exactly as it was found.
            or_exit(
                ledger.set_admission_liquidity_buffer(
                    direction,
                    config.goldcoin.admission_safety_buffer_atomic,
                    config.goldcoin.admission_reopen_buffer_atomic,
                ),
                "configure admission liquidity buffer",
            );
        }
    }

    // Operator/signer-mismatch diagnostic (PR #35 maintainer-review
    // finding 4): every independent signer's own instance of this
    // codebase must agree on `change_fanout_target_atomic`/
    // `change_fanout_max_outputs` (both fed into `PayoutPolicy`, which
    // must independently re-derive byte-identical transactions across the
    // 2-of-3 signer set — docs/09-runbook.md "UTXO liquidity") and on
    // `vault_min_confirmations` (governs when this vault's own change
    // becomes spendable again). A silent drift here — e.g. a rolling
    // deploy where one signer picked up a new config field before another
    // — doesn't corrupt anything (a divergent signer's signature simply
    // fails `multisig::assemble`'s cryptographic verification, never
    // broadcasting anything malformed), but surfaces only as an opaque
    // stuck-payout/signing failure unless an operator can directly diff
    // what each signer instance actually loaded. Logged at startup, once,
    // at INFO — cheap, always visible, never gated behind a flag.
    tracing::info!(
        utxo_pool_min_available_count = config.goldcoin.utxo_pool_min_available_count,
        utxo_pool_warning_count = config.goldcoin.utxo_pool_warning_count,
        change_fanout_target_atomic = config.goldcoin.change_fanout_target_atomic,
        change_fanout_max_outputs = config.goldcoin.change_fanout_max_outputs,
        zero_conf_change_max_depth = config.goldcoin.zero_conf_change_max_depth,
        vault_min_confirmations = config.goldcoin.vault_min_confirmations,
        max_inputs = config.goldcoin.max_inputs,
        utxo_shaping_enabled = config.goldcoin.utxo_shaping_enabled,
        utxo_shaping_target_available_count = config.goldcoin.utxo_shaping_target_available_count,
        utxo_shaping_min_source_atomic = config.goldcoin.utxo_shaping_min_source_atomic,
        utxo_shaping_max_outputs_per_split = config.goldcoin.utxo_shaping_max_outputs_per_split,
        "effective UTXO liquidity settings for this instance — compare across every independent \
         signer to catch config drift before it surfaces as a stuck payout"
    );

    let goldcoin_indexer = Indexer::new(
        goldcoin_rpc_for_indexer,
        open_ledger(&config.service.db_path),
        IndexerConfig {
            vault_script_hex: vault.script_pubkey_hex(),
            confirmation_depth: config.goldcoin.confirmation_depth,
            max_reorg_depth: config.goldcoin.max_reorg_depth,
            initial_checkpoint: config.goldcoin.initial_checkpoint.clone(),
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
        change_fanout_target_atomic: config.goldcoin.change_fanout_target_atomic,
        change_fanout_max_outputs: config.goldcoin.change_fanout_max_outputs,
        zero_conf_change_max_depth: config.goldcoin.zero_conf_change_max_depth,
        zero_conf_change_mode: config.goldcoin.zero_conf_change_mode,
        zero_conf_change_recursive_chain_limit: config
            .goldcoin
            .zero_conf_change_recursive_chain_limit,
        reconciliation_tolerance: config.reserve.reconciliation_tolerance,
        vault_min_confirmations: config.goldcoin.vault_min_confirmations,
        goldcoin_network: config.goldcoin.network,
        signer_timeout: Duration::from_millis(config.service.signer_timeout_ms),
        max_auto_resumes_per_tick: config.goldcoin.max_auto_resumes_per_tick,
        utxo_shaping_enabled: config.goldcoin.utxo_shaping_enabled,
        utxo_shaping_target_available_count: config.goldcoin.utxo_shaping_target_available_count,
        utxo_shaping_min_source_atomic: config.goldcoin.utxo_shaping_min_source_atomic,
        utxo_shaping_max_outputs_per_split: config.goldcoin.utxo_shaping_max_outputs_per_split,
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
            root_vault_for_api,
            config.goldcoin.network,
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

    // The authenticated admin control plane (admin_api module docs):
    // read-only reserve/on-chain views plus the local Ledger mutations
    // glc-admin already supports. Holds no keys — the on-chain admin
    // keypair stays CLI-only on the operator's machine. Operator tokens
    // are resolved from their env vars HERE, not in Config::load, so
    // glc-admin's --config recovery commands never need them; the daemon
    // still fails closed before serving a single request if any is
    // missing, empty, or duplicated.
    let admin_task = config.service.admin_bind_addr.map(|admin_addr| {
        let admin_source = Arc::new(AdminApi::new(
            config.service.db_path.clone(),
            RealSolanaRpc::new(config.solana.rpc_url.clone()),
        ));
        let resolved = or_exit(
            admin_api::auth::resolve_operator_tokens(&config.service.admin_operators),
            "resolve admin operator tokens",
        );
        let registry = Arc::new(or_exit(
            OperatorRegistry::new(resolved),
            "build the admin operator registry",
        ));
        let admin_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) =
                admin_api::serve(admin_addr, admin_source, registry, admin_shutdown_rx).await
            {
                tracing::error!(error = %e, "admin API exited with an error");
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
    if let Some(admin_task) = admin_task {
        let _ = admin_task.await;
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
