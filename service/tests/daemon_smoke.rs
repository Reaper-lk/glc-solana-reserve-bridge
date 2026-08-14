//! Real-process smoke test for `glc-bridge-daemon`
//! (docs/15-post-phase6-audit.md P0 item 1): actually spawns the compiled
//! binary against real (regtest/localnet) nodes, confirms it starts,
//! ticks, serves `/health`, and shuts down cleanly on `SIGTERM` — not
//! just the in-process `daemon::run` unit tests in `src/daemon/tests.rs`.
//!
//! Skipped (never failed) unless Phase 6 prerequisites are available —
//! see `support::phase6_prereqs`. No on-chain program bootstrap is
//! needed here: the daemon is expected to tick cleanly (with per-phase
//! errors, never a crash) even against an uninitialized program, which is
//! itself part of what this test checks.

mod support;

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use solana_sdk::signature::{Keypair, Signer};

use support::{LocalValidator, RegtestNode};

fn write_solana_keypair_file(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, Keypair) {
    let keypair = Keypair::new();
    let path = dir.join(name);
    std::fs::write(
        &path,
        serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();
    (path, keypair)
}

fn write_vault_key_file(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, [u8; 33]) {
    let secret_key = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
    let path = dir.join(name);
    std::fs::write(
        &path,
        glc_reserve_bridge_service::goldcoin::hex::encode(&secret_key.serialize()),
    )
    .unwrap();
    (path, pubkey)
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_starts_ticks_serves_health_and_shuts_down_cleanly_on_sigterm() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping daemon_starts_ticks_serves_health_and_shuts_down_cleanly_on_sigterm: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    let goldcoin = RegtestNode::start(&goldcoind, &cli);
    goldcoin.generate(101, &goldcoin.new_address());

    let upgrade_authority = Keypair::new();
    let validator = LocalValidator::start(
        &so,
        &glc_reserve_bridge_service::solana::accounts::PROGRAM_ID,
        &upgrade_authority.pubkey(),
    );
    let blocking = validator.blocking_client();
    support::airdrop(&blocking, &upgrade_authority.pubkey(), 10_000_000_000);
    // A real mint, not just a random pubkey — the daemon's startup now
    // verifies reserve_token_mint is owned by the legacy SPL Token
    // program (accounts::verify_reserve_mint_token_program) and fails
    // closed before it would tick against a mint that doesn't even
    // exist, exactly the behavior this smoke test would otherwise trip.
    let mint = support::create_throwaway_mint(&blocking, &upgrade_authority, 8);
    // The daemon's own startup check reads at `finalized` commitment
    // (RealSolanaRpc always does), which lags the `confirmed` commitment
    // `create_throwaway_mint` waited for by ~32 slots on a fresh
    // validator — wait for the mint to actually finalize, the same
    // cold-start race documented in docs/15-post-phase6-audit.md.
    let real_rpc = validator.real_rpc();
    for _ in 0..100 {
        if glc_reserve_bridge_service::solana::rpc::SolanaRpc::get_account(
            &real_rpc,
            &mint.pubkey(),
        )
        .await
        .ok()
        .flatten()
        .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let dir = tempfile::tempdir().unwrap();
    let (a1_path, a1) = write_solana_keypair_file(dir.path(), "attest1.json");
    let (a2_path, a2) = write_solana_keypair_file(dir.path(), "attest2.json");
    let (a3_path, a3) = write_solana_keypair_file(dir.path(), "attest3.json");
    let (v1_path, v1) = write_vault_key_file(dir.path(), "vault1.hex");
    let (v2_path, v2) = write_vault_key_file(dir.path(), "vault2.hex");
    let (v3_path, v3) = write_vault_key_file(dir.path(), "vault3.hex");
    let (submitter_path, _submitter) = write_solana_keypair_file(dir.path(), "submitter.json");
    let admin = Keypair::new();
    let db_path = dir.path().join("ledger.sqlite3");
    let health_port = support::free_port();
    let api_port = support::free_port();

    let config_toml = format!(
        r#"
[solana]
rpc_url = "{solana_rpc}"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "{goldcoin_rpc}"
rpc_user = "user"
rpc_password = "pass"
confirmation_depth = 3
max_reorg_depth = 50
required_payout_confirmations = 3
vault_min_confirmations = 1
fee_rate_per_kb = 100000
dust_threshold = 1000
max_inputs = 10

[reserve]
reconciliation_tolerance = 0

[reserve.solana]
protected_minimum = 0
target_reserve = 50000000000
warning_reserve = 20000000000
critical_reserve = 10000000000

[reserve.goldcoin]
protected_minimum = 0
target_reserve = 50000000000
warning_reserve = 20000000000
critical_reserve = 10000000000

[operators]
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_key_paths = ["{a1_path}", "{a2_path}", "{a3_path}"]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_key_paths = ["{v1_path}", "{v2_path}", "{v3_path}"]
submitter_key_path = "{submitter_path}"

[service]
db_path = "{db_path}"
tick_interval_ms = 200
health_bind_addr = "127.0.0.1:{health_port}"
api_bind_addr = "127.0.0.1:{api_port}"
reservation_ttl_secs = 3600
"#,
        solana_rpc = validator.rpc_url(),
        mint = mint.pubkey(),
        goldcoin_rpc = goldcoin.rpc_url(),
        admin = admin.pubkey(),
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        a1_path = a1_path.display(),
        a2_path = a2_path.display(),
        a3_path = a3_path.display(),
        v1 = glc_reserve_bridge_service::goldcoin::hex::encode(&v1),
        v2 = glc_reserve_bridge_service::goldcoin::hex::encode(&v2),
        v3 = glc_reserve_bridge_service::goldcoin::hex::encode(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        submitter_path = submitter_path.display(),
        db_path = db_path.display(),
    );
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_toml).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_glc-bridge-daemon"))
        .arg("--config")
        .arg(&config_path)
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn glc-bridge-daemon");

    // Poll /health until it responds at all (any status code) — proves
    // the process started, loaded config/keys, constructed the vault,
    // and the health server is listening, all without crashing.
    let health_url = format!("http://127.0.0.1:{health_port}/health");
    let mut responded = false;
    for _ in 0..100 {
        if let Ok(resp) = reqwest::get(&health_url).await {
            let _ = resp.text().await;
            responded = true;
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("daemon exited early with {status:?} before /health ever responded");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(responded, "daemon never served /health");

    // Give it a couple of tick intervals so the tick loop genuinely runs
    // (not just the health server), then confirm the ledger database it
    // was configured to use actually exists — proof the startup sequence
    // (config -> keys -> vault -> ledger -> orchestrator) really ran.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        db_path.exists(),
        "the daemon's configured ledger database was never created"
    );
    if let Ok(Some(status)) = child.try_wait() {
        panic!("daemon exited unexpectedly with {status:?} during normal ticking");
    }

    // The bridge API (P0 item 4) is up too — proof the whole startup
    // sequence, including the API server, actually wired together
    // correctly. `/status` reads live on-chain `BridgeConfig`, which was
    // never initialized in this test (no bootstrap_program call, by
    // design — see module docs), so a 503 here is the *correct*,
    // fail-closed answer; what matters is that the server is reachable
    // and returns well-formed JSON either way, not a crash.
    let resp = reqwest::get(format!("http://127.0.0.1:{api_port}/status"))
        .await
        .expect("bridge API must be reachable");
    let _: serde_json::Value = resp
        .json()
        .await
        .expect("status response must be valid JSON");

    // Graceful shutdown: SIGTERM, not SIGKILL — proves the signal
    // handler + shutdown-channel wiring, not just that the OS can kill
    // the process.
    let pid = child.id();
    let kill_status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("run kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself failed to run");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("daemon did not exit within 10s of SIGTERM — shutdown is not graceful");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        exit_status.success(),
        "daemon must exit 0 on a graceful SIGTERM shutdown, got {exit_status:?}"
    );

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("shutdown signal received"),
        "expected the shutdown-signal log line; stderr was:\n{stderr}"
    );
}

/// Real-process negative counterpart: a `reserve_token_mint` that exists
/// on-chain but is owned by a program other than the legacy SPL Token
/// program (here, a plain system-owned account — the same class of
/// mismatch a real Token-2022 mint would produce) must make the daemon
/// refuse to start, never reaching `/health`, never ticking. Goldcoin
/// infrastructure isn't needed here: startup fails before this daemon's
/// first Goldcoin RPC call would ever happen.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_refuses_to_start_with_a_reserve_mint_owned_by_the_wrong_program() {
    let Some((_goldcoind, _cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping daemon_refuses_to_start_with_a_reserve_mint_owned_by_the_wrong_program: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    let upgrade_authority = Keypair::new();
    let validator = LocalValidator::start(
        &so,
        &glc_reserve_bridge_service::solana::accounts::PROGRAM_ID,
        &upgrade_authority.pubkey(),
    );
    let blocking = validator.blocking_client();
    support::airdrop(&blocking, &upgrade_authority.pubkey(), 10_000_000_000);
    // A real, existing, finalized account — but a plain system account,
    // never owned by the SPL Token program. `upgrade_authority`'s own
    // account (funded by the airdrop above) works and needs no extra
    // wait: `support::airdrop` already confirms before returning, and an
    // airdropped System-owned account's owner is stable and never
    // changes, unlike the mint-creation race the happy-path test above
    // has to wait out.

    let dir = tempfile::tempdir().unwrap();
    let (a1_path, a1) = write_solana_keypair_file(dir.path(), "attest1.json");
    let (a2_path, a2) = write_solana_keypair_file(dir.path(), "attest2.json");
    let (a3_path, a3) = write_solana_keypair_file(dir.path(), "attest3.json");
    let (v1_path, v1) = write_vault_key_file(dir.path(), "vault1.hex");
    let (v2_path, v2) = write_vault_key_file(dir.path(), "vault2.hex");
    let (v3_path, v3) = write_vault_key_file(dir.path(), "vault3.hex");
    let (submitter_path, _submitter) = write_solana_keypair_file(dir.path(), "submitter.json");
    let admin = Keypair::new();
    let db_path = dir.path().join("ledger.sqlite3");
    let health_port = support::free_port();

    let config_toml = format!(
        r#"
[solana]
rpc_url = "{solana_rpc}"
commitment = "finalized"
reserve_token_mint = "{wrong_mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:1"
rpc_user = "user"
rpc_password = "pass"
confirmation_depth = 3
max_reorg_depth = 50
required_payout_confirmations = 3
vault_min_confirmations = 1
fee_rate_per_kb = 100000
dust_threshold = 1000
max_inputs = 10

[reserve]
reconciliation_tolerance = 0

[reserve.solana]
protected_minimum = 0
target_reserve = 50000000000
warning_reserve = 20000000000
critical_reserve = 10000000000

[reserve.goldcoin]
protected_minimum = 0
target_reserve = 50000000000
warning_reserve = 20000000000
critical_reserve = 10000000000

[operators]
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_key_paths = ["{a1_path}", "{a2_path}", "{a3_path}"]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_key_paths = ["{v1_path}", "{v2_path}", "{v3_path}"]
submitter_key_path = "{submitter_path}"

[service]
db_path = "{db_path}"
tick_interval_ms = 200
health_bind_addr = "127.0.0.1:{health_port}"
reservation_ttl_secs = 3600
"#,
        solana_rpc = validator.rpc_url(),
        wrong_mint = upgrade_authority.pubkey(),
        admin = admin.pubkey(),
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        a1_path = a1_path.display(),
        a2_path = a2_path.display(),
        a3_path = a3_path.display(),
        v1 = glc_reserve_bridge_service::goldcoin::hex::encode(&v1),
        v2 = glc_reserve_bridge_service::goldcoin::hex::encode(&v2),
        v3 = glc_reserve_bridge_service::goldcoin::hex::encode(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        submitter_path = submitter_path.display(),
        db_path = db_path.display(),
    );
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_toml).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_glc-bridge-daemon"))
        .arg("--config")
        .arg(&config_path)
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn glc-bridge-daemon");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "daemon should have refused to start against a wrong-token-program mint \
                 within 15s, but is still running"
            );
        }
        // Must never start serving while still deciding whether to start.
        assert!(
            reqwest::get(format!("http://127.0.0.1:{health_port}/health"))
                .await
                .is_err(),
            "the daemon must never serve /health when it is about to refuse to start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert_eq!(
        exit_status.code(),
        Some(2),
        "a startup-time fail-closed refusal must exit(2), got {exit_status:?}"
    );
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("verify the configured reserve_token_mint's token program"),
        "expected the token-program verification failure to be logged; stderr was:\n{stderr}"
    );
}
