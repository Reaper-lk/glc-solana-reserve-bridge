use super::*;

fn write(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn solana_keypair_file(dir: &std::path::Path, name: &str) -> (PathBuf, Keypair) {
    let keypair = Keypair::new();
    let json = serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap();
    (write(dir, name, &json), keypair)
}

fn vault_key_file(dir: &std::path::Path, name: &str) -> (PathBuf, [u8; 33]) {
    let secret_key = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
    let hex = crate::goldcoin::hex::encode(&secret_key.serialize());
    (write(dir, name, &hex), pubkey)
}

/// A complete, valid config file plus the key files it references, all
/// written into `dir`. Returns the config file's path.
fn valid_config(dir: &std::path::Path) -> PathBuf {
    let (a1_path, a1) = solana_keypair_file(dir, "attest1.json");
    let (a2_path, a2) = solana_keypair_file(dir, "attest2.json");
    let (a3_path, a3) = solana_keypair_file(dir, "attest3.json");
    let (v1_path, v1) = vault_key_file(dir, "vault1.hex");
    let (v2_path, v2) = vault_key_file(dir, "vault2.hex");
    let (v3_path, v3) = vault_key_file(dir, "vault3.hex");
    let (sub_path, _sub) = solana_keypair_file(dir, "submitter.json");
    let admin = Keypair::new().pubkey();
    let mint = Keypair::new().pubkey();

    let toml = format!(
        r#"
[solana]
rpc_url = "http://127.0.0.1:8899"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18332"
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
submitter_key_path = "{sub_path}"

[service]
db_path = "/tmp/does-not-need-to-exist-for-config-loading/ledger.sqlite3"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600
"#,
        mint = mint,
        admin = admin,
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        a1_path = a1_path.display(),
        a2_path = a2_path.display(),
        a3_path = a3_path.display(),
        v1 = crate::goldcoin::hex::encode(&v1),
        v2 = crate::goldcoin::hex::encode(&v2),
        v3 = crate::goldcoin::hex::encode(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        sub_path = sub_path.display(),
    );
    write(dir, "config.toml", &toml)
}

#[test]
fn loads_a_well_formed_config_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();

    assert_eq!(
        config.goldcoin.network,
        crate::goldcoin::address::Network::Testnet
    );
    assert_eq!(config.operators.attestation_threshold, 2);
    assert_eq!(config.operators.attestation_pubkeys.len(), 3);
    assert_eq!(config.operators.vault_threshold, 2);
    assert_eq!(config.operators.vault_pubkeys.len(), 3);
    assert_eq!(
        config.service.health_bind_addr,
        "127.0.0.1:9100".parse().unwrap()
    );

    // Key files load and cross-validate cleanly against the declared
    // pubkeys.
    let signers = config.load_attestation_signers().unwrap();
    assert_eq!(signers.len(), 3);
    for (signer, expected) in signers.iter().zip(&config.operators.attestation_pubkeys) {
        assert_eq!(signer.pubkey(), *expected);
    }
    let vault_signers = config.load_vault_signers().unwrap();
    assert_eq!(vault_signers.len(), 3);
    for (signer, expected) in vault_signers.iter().zip(&config.operators.vault_pubkeys) {
        assert_eq!(signer.pubkey, *expected);
    }
    config.load_submitter().unwrap();
}

#[test]
fn missing_file_fails_closed_with_a_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = Config::load(&dir.path().join("nonexistent.toml")).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }));
}

#[test]
fn malformed_toml_fails_closed_with_a_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "config.toml", "this is not valid toml {{{");
    let err = Config::load(&path).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn non_finalized_commitment_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"commitment = "finalized""#, r#"commitment = "confirmed""#);
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "solana.commitment"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn mainnet_goldcoin_network_is_accepted() {
    // goldcoin::address now has real, verified mainnet base58check
    // version bytes (docs/16-p0-checkpoint.md) — "mainnet" must load
    // cleanly and resolve to the real mainnet Network variant, not be
    // rejected the way it was before those bytes existed.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "mainnet""#);
    std::fs::write(&path, text).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.goldcoin.network,
        crate::goldcoin::address::Network::Mainnet
    );
}

#[test]
fn testnet_goldcoin_network_resolves_to_the_same_bytes_as_regtest() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "testnet""#);
    std::fs::write(&path, text).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.goldcoin.network,
        crate::goldcoin::address::Network::Testnet
    );
}

#[test]
fn unrecognized_goldcoin_network_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "moonnet""#);
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "goldcoin.network"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn attestation_threshold_above_pubkey_count_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace("attestation_threshold = 2", "attestation_threshold = 5");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => {
            assert_eq!(field, "operators.attestation_threshold")
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn zero_vault_threshold_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace("vault_threshold = 2", "vault_threshold = 0");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "operators.vault_threshold"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn critical_reserve_not_exceeding_protected_minimum_is_rejected() {
    // Mirrors Ledger::configure_reserve's own assertion — must be caught
    // here, before anything ever reaches the ledger.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        "[reserve.solana]\nprotected_minimum = 0",
        "[reserve.solana]\nprotected_minimum = 20000000000",
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "reserve.solana"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn malformed_pubkey_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        r#"reserve_token_mint = ""#,
        r#"reserve_token_mint = "not-a-real-pubkey-XXXXX"#,
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "solana.reserve_token_mint"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn key_file_pubkey_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());

    // Overwrite the first attestation key file with an entirely different
    // keypair, so it no longer matches the pubkey the config declares at
    // that position.
    let attest1 = dir.path().join("attest1.json");
    let other = Keypair::new();
    std::fs::write(
        &attest1,
        serde_json::to_string(&other.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();

    let config = Config::load(&path).unwrap(); // parsing/validation itself doesn't touch key files
    let Err(err) = config.load_attestation_signers() else {
        panic!("expected a KeyMismatch error");
    };
    assert!(matches!(err, ConfigError::KeyMismatch { .. }));
}

#[test]
fn vault_key_file_pubkey_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());

    let vault1 = dir.path().join("vault1.hex");
    let other = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    std::fs::write(&vault1, crate::goldcoin::hex::encode(&other.serialize())).unwrap();

    let config = Config::load(&path).unwrap();
    let Err(err) = config.load_vault_signers() else {
        panic!("expected a KeyMismatch error");
    };
    assert!(matches!(err, ConfigError::KeyMismatch { .. }));
}

#[test]
fn missing_key_file_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    std::fs::remove_file(dir.path().join("attest1.json")).unwrap();

    let config = Config::load(&path).unwrap();
    let Err(err) = config.load_attestation_signers() else {
        panic!("expected a KeyFileRead error");
    };
    assert!(matches!(err, ConfigError::KeyFileRead { .. }));
}

#[test]
fn env_overrides_take_precedence_over_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());

    // SAFETY: this process-wide env mutation is scoped to this single
    // test and cleaned up before returning; `cargo test`'s default
    // single-process-many-threads model means a parallel test reading
    // unrelated env vars is unaffected, but tests touching the SAME var
    // must not run concurrently — there are none of those here.
    unsafe {
        std::env::set_var("GLC_BRIDGE_SOLANA_RPC_URL", "http://example.invalid:9999");
    }
    let config = Config::load(&path);
    unsafe {
        std::env::remove_var("GLC_BRIDGE_SOLANA_RPC_URL");
    }

    assert_eq!(
        config.unwrap().solana.rpc_url,
        "http://example.invalid:9999"
    );
}

#[test]
fn omitting_the_alert_webhook_url_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert_eq!(config.service.alert_webhook_url, None);
}

#[test]
fn a_valid_alert_webhook_url_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        "reservation_ttl_secs = 3600",
        "reservation_ttl_secs = 3600\nalert_webhook_url = \"https://hooks.example.com/glc-bridge\"",
    );
    std::fs::write(&path, text).unwrap();

    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.service.alert_webhook_url,
        Some("https://hooks.example.com/glc-bridge".to_string())
    );
}

#[test]
fn a_malformed_alert_webhook_url_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        "reservation_ttl_secs = 3600",
        "reservation_ttl_secs = 3600\nalert_webhook_url = \"not a url\"",
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "service.alert_webhook_url"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

// ------------------------------------------------------- signer mode --

#[test]
fn unrecognized_signer_mode_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace("[operators]", "[operators]\nmode = \"bogus\"");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, .. } => assert_eq!(field, "operators.mode"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn omitting_mode_defaults_to_dev() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let config = Config::load(&path).unwrap();
    assert_eq!(config.operators.mode, SignerMode::Dev);
}

#[test]
fn production_mode_refuses_to_start_with_local_attestation_key_paths_configured() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    // Production mode, but attestation_key_paths (local plaintext dev
    // signer files) are still populated — must refuse to start, even
    // though no attestation_remote_signers were added either (the
    // key-paths check fires first).
    let text = text.replace("[operators]", "[operators]\nmode = \"production\"");
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::ProductionModeForbidsLocalSigners { field } => {
            assert_eq!(field, "operators.attestation_key_paths")
        }
        other => panic!("expected ProductionModeForbidsLocalSigners, got {other:?}"),
    }
}

#[test]
fn production_mode_refuses_to_start_with_local_vault_key_paths_configured() {
    // Attestation side is fully valid production config (remote signers,
    // no key paths) so that check passes cleanly — proving the
    // vault-side guard is independently enforced, not just a duplicate
    // of the attestation-side one.
    let dir2 = tempfile::tempdir().unwrap();
    let (a1_path, a1) = solana_keypair_file(dir2.path(), "attest1.json");
    let (a2_path, a2) = solana_keypair_file(dir2.path(), "attest2.json");
    let (a3_path, a3) = solana_keypair_file(dir2.path(), "attest3.json");
    let (v1_path, v1) = vault_key_file(dir2.path(), "vault1.hex");
    let (v2_path, v2) = vault_key_file(dir2.path(), "vault2.hex");
    let (v3_path, v3) = vault_key_file(dir2.path(), "vault3.hex");
    let (sub_path, _sub) = solana_keypair_file(dir2.path(), "submitter.json");
    let admin = Keypair::new().pubkey();
    let mint = Keypair::new().pubkey();
    let toml = format!(
        r#"
[solana]
rpc_url = "http://127.0.0.1:8899"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18332"
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
mode = "production"
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_remote_signers = [
  {{ endpoint_url = "https://a1.example.com", expected_public_key = "{a1}", auth_token_env = "UNSET_A1" }},
  {{ endpoint_url = "https://a2.example.com", expected_public_key = "{a2}", auth_token_env = "UNSET_A2" }},
  {{ endpoint_url = "https://a3.example.com", expected_public_key = "{a3}", auth_token_env = "UNSET_A3" }},
]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_key_paths = ["{v1_path}", "{v2_path}", "{v3_path}"]
submitter_key_path = "{sub_path}"

[service]
db_path = "/tmp/does-not-need-to-exist-for-config-loading/ledger.sqlite3"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600
"#,
        mint = mint,
        admin = admin,
        a1 = a1.pubkey(),
        a2 = a2.pubkey(),
        a3 = a3.pubkey(),
        v1 = crate::goldcoin::hex::encode(&v1),
        v2 = crate::goldcoin::hex::encode(&v2),
        v3 = crate::goldcoin::hex::encode(&v3),
        v1_path = v1_path.display(),
        v2_path = v2_path.display(),
        v3_path = v3_path.display(),
        sub_path = sub_path.display(),
    );
    let _ = (a1_path, a2_path, a3_path);
    let path2 = write(dir2.path(), "config.toml", &toml);

    let err = Config::load(&path2).unwrap_err();
    match err {
        ConfigError::ProductionModeForbidsLocalSigners { field } => {
            assert_eq!(field, "operators.vault_key_paths")
        }
        other => panic!("expected ProductionModeForbidsLocalSigners, got {other:?}"),
    }
}

#[test]
fn dev_mode_refuses_to_start_with_remote_signers_configured() {
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let admin_pubkey_line_pos = text.find("attestation_pubkeys").unwrap();
    let (head, tail) = text.split_at(admin_pubkey_line_pos);
    let text = format!(
        "{head}attestation_remote_signers = [{{ endpoint_url = \"https://x.example.com\", \
         expected_public_key = \"11111111111111111111111111111111111111111\", \
         auth_token_env = \"UNSET\" }}]\n{tail}"
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DevModeForbidsRemoteSigners { field } => {
            assert_eq!(field, "operators.attestation_remote_signers")
        }
        other => panic!("expected DevModeForbidsRemoteSigners, got {other:?}"),
    }
}

/// A complete, valid PRODUCTION-mode config: 3 attestation + 3 vault
/// remote-signer endpoints (threshold 2 of 3 each, matching constraint
/// 9's exact production shape), pointed at `attestation_urls`/
/// `vault_urls` in order. No local key files/paths anywhere.
#[allow(clippy::too_many_arguments)]
fn production_config(
    dir: &std::path::Path,
    attestation_pubkeys: [Pubkey; 3],
    attestation_urls: [String; 3],
    vault_pubkeys: [[u8; 33]; 3],
    vault_urls: [String; 3],
) -> PathBuf {
    let (sub_path, _sub) = solana_keypair_file(dir, "submitter.json");
    let admin = Keypair::new().pubkey();
    let mint = Keypair::new().pubkey();
    let [a1, a2, a3] = attestation_pubkeys;
    let [au1, au2, au3] = attestation_urls;
    let [v1, v2, v3] = vault_pubkeys;
    let [vu1, vu2, vu3] = vault_urls;
    let toml = format!(
        r#"
[solana]
rpc_url = "http://127.0.0.1:8899"
commitment = "finalized"
reserve_token_mint = "{mint}"

[goldcoin]
network = "regtest"
rpc_url = "http://127.0.0.1:18332"
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
mode = "production"
admin_pubkey = "{admin}"
attestation_threshold = 2
attestation_pubkeys = ["{a1}", "{a2}", "{a3}"]
attestation_remote_signers = [
  {{ endpoint_url = "{au1}", expected_public_key = "{a1}", auth_token_env = "GLC_TEST_CFG_A1" }},
  {{ endpoint_url = "{au2}", expected_public_key = "{a2}", auth_token_env = "GLC_TEST_CFG_A2" }},
  {{ endpoint_url = "{au3}", expected_public_key = "{a3}", auth_token_env = "GLC_TEST_CFG_A3" }},
]
vault_threshold = 2
vault_pubkeys = ["{v1}", "{v2}", "{v3}"]
vault_remote_signers = [
  {{ endpoint_url = "{vu1}", expected_public_key = "{v1}", auth_token_env = "GLC_TEST_CFG_V1" }},
  {{ endpoint_url = "{vu2}", expected_public_key = "{v2}", auth_token_env = "GLC_TEST_CFG_V2" }},
  {{ endpoint_url = "{vu3}", expected_public_key = "{v3}", auth_token_env = "GLC_TEST_CFG_V3" }},
]
submitter_key_path = "{sub_path}"

[service]
db_path = "/tmp/does-not-need-to-exist-for-config-loading/ledger.sqlite3"
tick_interval_ms = 5000
health_bind_addr = "127.0.0.1:9100"
reservation_ttl_secs = 3600
"#,
        mint = mint,
        admin = admin,
        a1 = a1,
        a2 = a2,
        a3 = a3,
        v1 = crate::goldcoin::hex::encode(&v1),
        v2 = crate::goldcoin::hex::encode(&v2),
        v3 = crate::goldcoin::hex::encode(&v3),
        sub_path = sub_path.display(),
    );
    write(dir, "config.toml", &toml)
}

fn https_placeholder(n: u8) -> String {
    format!("https://signer-{n}.example.com")
}

#[test]
fn remote_signer_count_mismatch_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );
    // Drop one attestation_remote_signers entry, leaving 3 pubkeys but
    // only 2 endpoints.
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(
        &format!(
            "  {{ endpoint_url = \"{}\", expected_public_key = \"{}\", auth_token_env = \"GLC_TEST_CFG_A3\" }},\n",
            https_placeholder(3),
            a[2]
        ),
        "",
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::RemoteSignerCountMismatch {
            field,
            expected,
            actual,
            ..
        } => {
            assert_eq!(field, "operators.attestation_remote_signers");
            assert_eq!(expected, 3);
            assert_eq!(actual, 2);
        }
        other => panic!("expected RemoteSignerCountMismatch, got {other:?}"),
    }
}

#[test]
fn remote_signer_expected_key_mismatch_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );
    // Corrupt one vault_remote_signers entry's expected_public_key so it
    // no longer matches the positionally-corresponding vault_pubkeys
    // entry — a copy/paste-style config error. Targets ONLY the
    // `expected_public_key = "..."` occurrence (not the `vault_pubkeys`
    // array entry, which shares the same hex substring) — a naive
    // whole-file string replace on the hex value alone would corrupt
    // both identically and never produce an actual mismatch.
    let text = std::fs::read_to_string(&path).unwrap();
    let v2_hex = crate::goldcoin::hex::encode(&v[1]);
    let wrong = crate::goldcoin::hex::encode(&[9u8; 33]);
    let text = text.replace(
        &format!("expected_public_key = \"{v2_hex}\""),
        &format!("expected_public_key = \"{wrong}\""),
    );
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::RemoteSignerExpectedKeyMismatch {
            field,
            pubkeys_field,
            ..
        } => {
            assert_eq!(field, "operators.vault_remote_signers");
            assert_eq!(pubkeys_field, "operators.vault_pubkeys");
        }
        other => panic!("expected RemoteSignerExpectedKeyMismatch, got {other:?}"),
    }
}

/// Production-mode config resolution preserves the same threshold shape
/// dev mode does — proving threshold enforcement is a property of
/// `attestation_threshold`/`vault_threshold` themselves (used later, at
/// signing time, by the same code regardless of which loader produced
/// the signers — `Orchestrator` never sees `Config` at all, only the
/// already-boxed trait objects and these two numbers), not something
/// that depends on which signer-loading path was used. The live
/// network call itself (real connect, real signing, real local
/// signature verification, every error-mapping case) is exhaustively
/// covered in `signing::remote::tests` already — this test's job is
/// specifically the config-resolution wiring, not re-proving the
/// network layer.
#[test]
fn production_mode_resolves_remote_signers_and_preserves_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let attestation_urls = [
        https_placeholder(1),
        https_placeholder(2),
        https_placeholder(3),
    ];
    let vault_urls = [
        https_placeholder(4),
        https_placeholder(5),
        https_placeholder(6),
    ];
    let path = production_config(
        dir.path(),
        a,
        attestation_urls.clone(),
        v,
        vault_urls.clone(),
    );

    let config = Config::load(&path).unwrap();
    assert_eq!(config.operators.mode, SignerMode::Production);
    // Threshold fields themselves are completely unaffected by mode —
    // same values, same validation (attestation_threshold_above_pubkey_
    // count_is_rejected/zero_vault_threshold_is_rejected already prove
    // the validation logic itself is mode-independent, since resolve()
    // checks thresholds before it ever branches on mode).
    assert_eq!(config.operators.attestation_threshold, 2);
    assert_eq!(config.operators.vault_threshold, 2);
    assert_eq!(config.operators.attestation_pubkeys.len(), 3);
    assert_eq!(config.operators.vault_pubkeys.len(), 3);

    assert!(config.operators.attestation_key_paths.is_empty());
    assert!(config.operators.vault_key_paths.is_empty());
    assert_eq!(config.operators.attestation_remote_signers.len(), 3);
    assert_eq!(config.operators.vault_remote_signers.len(), 3);
    for (resolved, expected_url) in config
        .operators
        .attestation_remote_signers
        .iter()
        .zip(&attestation_urls)
    {
        assert_eq!(&resolved.endpoint_url, expected_url);
        assert_eq!(resolved.timeout, Duration::from_millis(5_000));
    }
    for (resolved, expected_url) in config
        .operators
        .vault_remote_signers
        .iter()
        .zip(&vault_urls)
    {
        assert_eq!(&resolved.endpoint_url, expected_url);
    }
}

/// Two attestation-signer slots claiming the same public key are not two
/// custody domains — they're one, counted twice. This must fail closed
/// even though every individual `RemoteSignerExpectedKeyMismatch` check
/// passes (each entry's `expected_public_key` still agrees with its own
/// positional `attestation_pubkeys` entry).
#[test]
fn duplicate_attestation_pubkey_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let shared = Pubkey::new_unique();
    let a = [shared, shared, Pubkey::new_unique()];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerPubkey {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.attestation_remote_signers");
            assert_eq!(first_index, 0);
            assert_eq!(dup_index, 1);
        }
        other => panic!("expected DuplicateRemoteSignerPubkey, got {other:?}"),
    }
}

/// Same property as `duplicate_attestation_pubkey_fails_closed`, for the
/// Goldcoin vault group.
#[test]
fn duplicate_vault_pubkey_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[7u8; 33], [8u8; 33], [8u8; 33]];
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerPubkey {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.vault_remote_signers");
            assert_eq!(first_index, 1);
            assert_eq!(dup_index, 2);
        }
        other => panic!("expected DuplicateRemoteSignerPubkey, got {other:?}"),
    }
}

/// Distinct keys behind the same `endpoint_url` still share a single
/// network/operational compromise blast radius — reject this in
/// production even though the per-entry pubkey cross-checks all pass.
#[test]
fn duplicate_attestation_endpoint_url_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let shared_url = https_placeholder(1);
    let path = production_config(
        dir.path(),
        a,
        [shared_url.clone(), shared_url, https_placeholder(3)],
        v,
        [
            https_placeholder(4),
            https_placeholder(5),
            https_placeholder(6),
        ],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerEndpoint {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.attestation_remote_signers");
            assert_eq!(first_index, 0);
            assert_eq!(dup_index, 1);
        }
        other => panic!("expected DuplicateRemoteSignerEndpoint, got {other:?}"),
    }
}

/// Same property as `duplicate_attestation_endpoint_url_fails_closed`,
/// for the Goldcoin vault group.
#[test]
fn duplicate_vault_endpoint_url_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = [
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    ];
    let v = [[1u8; 33], [2u8; 33], [3u8; 33]];
    let shared_url = https_placeholder(5);
    let path = production_config(
        dir.path(),
        a,
        [
            https_placeholder(1),
            https_placeholder(2),
            https_placeholder(3),
        ],
        v,
        [https_placeholder(4), shared_url.clone(), shared_url],
    );

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::DuplicateRemoteSignerEndpoint {
            field,
            first_index,
            dup_index,
            ..
        } => {
            assert_eq!(field, "operators.vault_remote_signers");
            assert_eq!(first_index, 1);
            assert_eq!(dup_index, 2);
        }
        other => panic!("expected DuplicateRemoteSignerEndpoint, got {other:?}"),
    }
}
