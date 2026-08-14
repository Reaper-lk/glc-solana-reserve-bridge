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

    assert_eq!(config.goldcoin.network, GoldcoinNetwork::Regtest);
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
fn mainnet_goldcoin_network_fails_closed_not_silently_wrong() {
    // goldcoin::address has no mainnet base58check version bytes yet
    // (docs/15-post-phase6-audit.md) — selecting "mainnet" must refuse to
    // start rather than silently derive a regtest-formatted address for a
    // production vault.
    let dir = tempfile::tempdir().unwrap();
    let path = valid_config(dir.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.replace(r#"network = "regtest""#, r#"network = "mainnet""#);
    std::fs::write(&path, text).unwrap();

    let err = Config::load(&path).unwrap_err();
    match err {
        ConfigError::Invalid { field, detail } => {
            assert_eq!(field, "goldcoin.network");
            assert!(detail.contains("regtest"));
        }
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
