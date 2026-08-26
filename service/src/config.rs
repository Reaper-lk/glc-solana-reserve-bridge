//! Production configuration loading (docs/15-post-phase6-audit.md, P0 item
//! 3). A single TOML file, plus a handful of environment-variable
//! overrides for values that legitimately vary per deployment (RPC
//! endpoints/credentials) without editing a checked-in file.
//!
//! # What this does NOT do
//!
//! It does not embed or generate production secrets. Signing-key material
//! is loaded from local files whose *paths* are named here — DEV/TEST
//! POSTURE ONLY (see `signing::attestation`/`signing::goldcoin_vault`
//! module docs and docs/12-management-decisions.md item 2). A real
//! HSM/KMS-backed signer is later, explicitly-approved work
//! (docs/15-post-phase6-audit.md P2) and will replace this loading path
//! entirely, not extend it — there is deliberately no "production mode"
//! flag here that changes what this loader does; it always loads
//! plaintext key files, and that is exactly why it must never be pointed
//! at real custody keys.
//!
//! # Fail closed on anything malformed
//!
//! [`Config::load`] validates every cross-reference it can before
//! returning: threshold/pubkey-count consistency, `critical_reserve >
//! protected_minimum` (the same invariant `Ledger::configure_reserve`
//! itself enforces), and commitment/network settings restricted to what
//! this service actually implements. `goldcoin.network` accepts
//! `"regtest"`/`"testnet"`/`"mainnet"` — all three have real, verified
//! `goldcoin::address` version bytes (docs/16-p0-checkpoint.md) — and
//! nothing else; an unrecognized value fails closed rather than silently
//! defaulting. A malformed config is refused at startup, never
//! silently defaulted or partially applied. Key-file loading
//! ([`Config::load_attestation_signers`]/[`Config::load_vault_signers`]/
//! [`Config::load_submitter`]) cross-checks every loaded key's public
//! half against the pubkey this same config file declares at that
//! position, and refuses to proceed on any mismatch — the file naming a
//! set of attestation/vault pubkeys is itself a safety property (an
//! operator reading the file can see which keys are supposed to be in
//! play), and silently substituting whatever a key file actually
//! contains would quietly defeat that.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use crate::signing::attestation::DevAttestationSigner;
use crate::signing::goldcoin_vault::DevVaultSigner;
use crate::signing::remote::{RemoteAttestationSigner, RemoteSignerConfig, RemoteVaultSigner};
use crate::signing::signers::{AttestationSigner, VaultSigner};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config field `{field}`: {detail}")]
    Invalid { field: &'static str, detail: String },
    #[error("could not read key file {path}: {source}")]
    KeyFileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("key file {path} is malformed: {detail}")]
    KeyFileMalformed { path: PathBuf, detail: String },
    #[error(
        "key file {path} holds pubkey {actual}, but the config declares {expected} at the same \
         position — refusing to start with mismatched key material"
    )]
    KeyMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    /// `operators.mode = "production"` but `attestation_key_paths`/
    /// `vault_key_paths` (local plaintext dev/test signer files) are
    /// still populated — the exact refuse-to-start guard docs/22-
    /// production-readiness-review.md's P0-1 item requires: production
    /// mode must never even be capable of loading a local plaintext
    /// signer file, not just prefer not to.
    #[error(
        "operators.mode = \"production\" but {field} is non-empty — production mode refuses to \
         start with any local plaintext dev/test signer file configured"
    )]
    ProductionModeForbidsLocalSigners { field: &'static str },
    /// `operators.mode = "dev"` but remote-signer endpoints are
    /// configured — the same fail-closed discipline in the other
    /// direction: never silently ignore a configured remote signer
    /// because dev mode happened to be selected.
    #[error(
        "operators.mode = \"dev\" but {field} is non-empty — dev mode never uses remote signer \
         endpoints; remove them or set operators.mode = \"production\""
    )]
    DevModeForbidsRemoteSigners { field: &'static str },
    #[error(
        "{field} has {actual} entr{ies}, but {pubkeys_field} declares {expected} pubkey(s) — \
         these must be 1:1, same as the dev-mode key-path lists"
    )]
    RemoteSignerCountMismatch {
        field: &'static str,
        pubkeys_field: &'static str,
        expected: usize,
        actual: usize,
        ies: &'static str,
    },
    #[error(
        "{field}[{index}].expected_public_key ({declared}) does not match {pubkeys_field}[{index}] \
         ({from_pubkeys}) — these must agree; the pubkeys list and the remote-signer list are \
         cross-checked against each other by design, never silently trusted from just one"
    )]
    RemoteSignerExpectedKeyMismatch {
        field: &'static str,
        pubkeys_field: &'static str,
        index: usize,
        declared: String,
        from_pubkeys: String,
    },
    /// Two remote-signer entries in the same group claim the same
    /// `expected_public_key`. Production's 2-of-3 threshold assumes
    /// three genuinely separate custody domains (docs/02-trust-model.md,
    /// docs/12-management-decisions.md item 2) — a duplicated pubkey
    /// means at most two (or one) domains actually exist, silently
    /// weakening the threshold below what the config file claims.
    #[error(
        "{field} has duplicate expected_public_key {pubkey} at indices {first_index} and \
         {dup_index} — production custody domains must be genuinely independent; two remote \
         signers claiming the same public key are not separate custody domains, and this \
         silently weakens the configured threshold"
    )]
    DuplicateRemoteSignerPubkey {
        field: &'static str,
        pubkey: String,
        first_index: usize,
        dup_index: usize,
    },
    /// Two remote-signer entries in the same group share an
    /// `endpoint_url`. Even with distinct keys, a shared endpoint is a
    /// shared network/operational compromise blast radius, which
    /// defeats the "genuinely separate custody domain" requirement just
    /// as surely as a shared key would.
    #[error(
        "{field} has duplicate endpoint_url {endpoint_url:?} at indices {first_index} and \
         {dup_index} — production custody domains must be reachable via independent endpoints; \
         two remote-signer entries pointing at the same URL share a compromise blast radius \
         regardless of which public key each one claims"
    )]
    DuplicateRemoteSignerEndpoint {
        field: &'static str,
        endpoint_url: String,
        first_index: usize,
        dup_index: usize,
    },
    #[error("could not connect to remote signer {endpoint_url} ({field}[{index}]): {source}")]
    RemoteSignerConnect {
        field: &'static str,
        index: usize,
        endpoint_url: String,
        #[source]
        source: crate::signing::remote::RemoteSignerConfigError,
    },
}

// ------------------------------------------------------------- raw/TOML --

#[derive(Debug, Deserialize)]
struct RawConfig {
    solana: RawSolana,
    goldcoin: RawGoldcoin,
    reserve: RawReserve,
    operators: RawOperators,
    service: RawService,
}

#[derive(Debug, Deserialize)]
struct RawSolana {
    rpc_url: String,
    commitment: String,
    reserve_token_mint: String,
}

#[derive(Debug, Deserialize)]
struct RawGoldcoin {
    network: String,
    rpc_url: String,
    #[serde(default)]
    rpc_user: String,
    #[serde(default)]
    rpc_password: String,
    #[serde(default = "default_connect_timeout_ms")]
    rpc_connect_timeout_ms: u64,
    #[serde(default = "default_read_timeout_ms")]
    rpc_read_timeout_ms: u64,
    confirmation_depth: u32,
    max_reorg_depth: u32,
    required_payout_confirmations: i64,
    vault_min_confirmations: i64,
    fee_rate_per_kb: u64,
    dust_threshold: u64,
    max_inputs: usize,
    /// Target size (canonical atomic units) for each deterministic change
    /// FAN-OUT output a Goldcoin payout produces (docs/09-runbook.md's
    /// "UTXO liquidity" section) — production-aware: sized relative to the
    /// current maximum net payout, not a stale historical limit. Defaults
    /// so an existing config file with none of these four new fields keeps
    /// loading and behaving sensibly unchanged.
    #[serde(default = "default_change_fanout_target_atomic")]
    change_fanout_target_atomic: u64,
    /// Hard cap on how many change outputs one payout may ever produce.
    #[serde(default = "default_change_fanout_max_outputs")]
    change_fanout_max_outputs: usize,
    /// Mature, unreserved vault UTXOs that must remain after admitting one
    /// more SolToGlc obligation, or `Ledger::fold_sol_deposit` parks it
    /// (`utxo_liquidity_low_at_fold`) instead — see
    /// `Ledger::set_utxo_pool_thresholds`.
    #[serde(default = "default_utxo_pool_min_available_count")]
    utxo_pool_min_available_count: u32,
    /// Purely observational early-warning threshold (>=
    /// `utxo_pool_min_available_count`) — never itself gates admission.
    #[serde(default = "default_utxo_pool_warning_count")]
    utxo_pool_warning_count: u32,
    /// Production initial-checkpoint bootstrap (docs/09-runbook.md
    /// "Goldcoin indexer initial checkpoint") — used ONLY when the
    /// ledger has no indexed Goldcoin blocks yet; ignored forever after
    /// that (`goldcoin::indexer::InitialCheckpoint`'s own docs). All
    /// three fields default to "absent"/`false` so every existing
    /// dev/test/regtest config keeps working unchanged.
    #[serde(default)]
    initial_checkpoint_height: Option<i64>,
    #[serde(default)]
    initial_checkpoint_hash: Option<String>,
    #[serde(default)]
    initial_checkpoint_operator_acknowledged_no_prior_deposits: bool,
}

/// 2,500 GLC — comfortable headroom over the current 2,000 GLC max gross
/// transfer / 1,880 GLC max net payout (docs/09-runbook.md), so a single
/// future change output can usually cover the next payout outright via
/// `coin::select`'s cheap single-UTXO path, without needing a
/// multi-input combination. Revisit if `per_transfer_limit` changes
/// materially, exactly like `split-vault-utxo`'s own chunk-target default.
fn default_change_fanout_target_atomic() -> u64 {
    2_500 * 100_000_000
}
fn default_change_fanout_max_outputs() -> usize {
    10
}
/// 10 mature UTXOs — the verified-safe floor for the incident's own vault
/// shape (4,770 GLC chunks, 1,880 GLC maximum net payout, 20,000 GLC
/// protected minimum): empirically confirmed
/// (`service/tests/utxo_liquidity_production_tuning.rs::
/// test_prod_recommended_floor_10_survives_the_25_burst_with_margin`) to
/// engage backpressure with a full payout of margin before the hard
/// invariant's own 11-payout survival limit for that shape. `8` (this
/// default's previous value) was shown insufficient — it lets the hard
/// invariant breach one payout before count-based backpressure would ever
/// engage on its own
/// (`test_prod_defaults_floor_8_breaches_before_backpressure_engages`).
/// Recompute for a vault with a materially different chunk size or total
/// balance — see docs/09-runbook.md's "UTXO liquidity" tuning section.
fn default_utxo_pool_min_available_count() -> u32 {
    10
}
fn default_utxo_pool_warning_count() -> u32 {
    15
}
fn default_connect_timeout_ms() -> u64 {
    5_000
}
fn default_read_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
struct RawReserveBounds {
    protected_minimum: u64,
    target_reserve: u64,
    warning_reserve: u64,
    critical_reserve: u64,
}

#[derive(Debug, Deserialize)]
struct RawReserve {
    reconciliation_tolerance: u64,
    solana: RawReserveBounds,
    goldcoin: RawReserveBounds,
}

#[derive(Debug, Deserialize)]
struct RawOperators {
    /// `"dev"` or `"production"` — see [`SignerMode`]. Defaults to
    /// `"dev"` when omitted so every existing config (this project's own
    /// tests and any operator's config predating this field) keeps
    /// working unchanged; a real production deployment must set this
    /// explicitly, never rely on the default.
    #[serde(default = "default_signer_mode")]
    mode: String,
    admin_pubkey: String,
    attestation_threshold: usize,
    attestation_pubkeys: Vec<String>,
    #[serde(default)]
    attestation_key_paths: Vec<PathBuf>,
    #[serde(default)]
    attestation_remote_signers: Vec<RawRemoteSigner>,
    vault_threshold: u8,
    vault_pubkeys: Vec<String>,
    #[serde(default)]
    vault_key_paths: Vec<PathBuf>,
    #[serde(default)]
    vault_remote_signers: Vec<RawRemoteSigner>,
    submitter_key_path: PathBuf,
}

fn default_signer_mode() -> String {
    "dev".to_string()
}

/// One `[[operators.attestation_remote_signers]]`/
/// `[[operators.vault_remote_signers]]` TOML table — see
/// `signing::remote` module docs for the wire protocol this connects to.
/// `auth_token_env` is a NAME, never the secret itself — see that
/// module's `AuthToken` (constraint 9: no authentication secrets
/// committed to git).
#[derive(Debug, Clone, Deserialize)]
struct RawRemoteSigner {
    endpoint_url: String,
    /// Cross-checked against the positionally-matching entry in
    /// `attestation_pubkeys`/`vault_pubkeys` at `resolve()` time — see
    /// [`ConfigError::RemoteSignerExpectedKeyMismatch`]. Redundant by
    /// design: an operator can read either list and see the same
    /// identity, and a mismatch between them (a copy/paste error, a
    /// reordered list) is caught at config-load time rather than only
    /// discovered against the live endpoint.
    expected_public_key: String,
    auth_token_env: String,
    #[serde(default = "default_remote_signer_timeout_ms")]
    timeout_ms: u64,
}

fn default_remote_signer_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Deserialize)]
struct RawService {
    db_path: PathBuf,
    #[serde(default = "default_tick_interval_ms")]
    tick_interval_ms: u64,
    health_bind_addr: String,
    api_bind_addr: Option<String>,
    /// How long a reservation made by `POST /transfers`
    /// (`api::BridgeApi::create_glc_to_sol_transfer`) holds capacity
    /// before it expires if no matching deposit ever arrives
    /// (docs/12-management-decisions.md item 7 — no default asserted;
    /// operators must decide this for their own deployment).
    reservation_ttl_secs: i64,
    /// Where to POST a JSON notification when a reserve direction
    /// transitions into a pause (ops::alerting) — omit to run without
    /// outbound alerting (matches `ops::health`'s own stance: no alerting
    /// integration is mandatory, an operator's own monitoring can poll
    /// `/health` instead).
    alert_webhook_url: Option<String>,
    #[serde(default = "default_alert_poll_interval_secs")]
    alert_poll_interval_secs: u64,
    /// Bounds each individual signer call (`signing::signers` module
    /// docs) — an operational tuning knob, not a safety-critical value,
    /// so (unlike reserve/rate-limit fields) a sensible default applies
    /// when omitted rather than requiring every deployment to set it.
    #[serde(default = "default_signer_timeout_ms")]
    signer_timeout_ms: u64,
}

fn default_alert_poll_interval_secs() -> u64 {
    30
}

fn default_signer_timeout_ms() -> u64 {
    10_000
}

fn default_tick_interval_ms() -> u64 {
    5_000
}

// -------------------------------------------------------------- resolved --

#[derive(Debug, Clone)]
pub struct SolanaConfig {
    pub rpc_url: String,
    pub reserve_token_mint: Pubkey,
}

#[derive(Debug, Clone)]
pub struct GoldcoinConfig {
    /// `"regtest"`/`"testnet"` both resolve to
    /// `goldcoin::address::Network::Testnet` (verified to share identical
    /// version bytes — docs/16-p0-checkpoint.md); the config file keeps
    /// the operator-facing distinction for clarity even though the
    /// address math doesn't need it.
    pub network: crate::goldcoin::address::Network,
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_password: String,
    pub rpc_connect_timeout_ms: u64,
    pub rpc_read_timeout_ms: u64,
    pub confirmation_depth: u32,
    pub max_reorg_depth: u32,
    pub required_payout_confirmations: i64,
    pub vault_min_confirmations: i64,
    pub fee_rate_per_kb: u64,
    pub dust_threshold: u64,
    pub max_inputs: usize,
    pub change_fanout_target_atomic: u64,
    pub change_fanout_max_outputs: usize,
    pub utxo_pool_min_available_count: u32,
    pub utxo_pool_warning_count: u32,
    /// See `RawGoldcoin`'s matching fields and
    /// `goldcoin::indexer::InitialCheckpoint`'s own docs. Structurally
    /// validated here (hex format, non-negative height); the live
    /// getblockhash/above-tip checks happen at indexer bootstrap time,
    /// since only that has a chain connection.
    pub initial_checkpoint: Option<crate::goldcoin::indexer::InitialCheckpoint>,
}

#[derive(Debug, Clone, Copy)]
pub struct ReserveBounds {
    pub protected_minimum: u64,
    pub target_reserve: u64,
    pub warning_reserve: u64,
    pub critical_reserve: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ReserveConfig {
    pub reconciliation_tolerance: u64,
    pub solana: ReserveBounds,
    pub goldcoin: ReserveBounds,
}

#[derive(Debug, Clone)]
pub struct OperatorsConfig {
    pub mode: SignerMode,
    pub admin_pubkey: Pubkey,
    pub attestation_threshold: usize,
    pub attestation_pubkeys: Vec<Pubkey>,
    pub attestation_key_paths: Vec<PathBuf>,
    pub attestation_remote_signers: Vec<RemoteSignerConfig>,
    pub vault_threshold: u8,
    pub vault_pubkeys: Vec<[u8; 33]>,
    pub vault_key_paths: Vec<PathBuf>,
    pub vault_remote_signers: Vec<RemoteSignerConfig>,
    pub submitter_key_path: PathBuf,
}

/// Which signer-loading path a deployment uses — see `Config::load_signers`.
/// Deliberately just these two: there is no "mixed" mode (e.g. some
/// attestation signers local, some remote) — a deployment is either
/// entirely dev/test-posture or entirely production-posture, so an
/// operator (or reviewer) can answer "is this a real deployment" by
/// reading one field, not by auditing every signer entry individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerMode {
    Dev,
    Production,
}

/// Return type of [`Config::load_signers`] — named so call sites (and
/// clippy) don't have to spell out the full nested `Vec<Box<dyn _>>`
/// tuple.
pub type LoadedSigners = (Vec<Box<dyn AttestationSigner>>, Vec<Box<dyn VaultSigner>>);

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub db_path: PathBuf,
    pub tick_interval_ms: u64,
    /// Serves both `/health` and `/metrics` — see `ops::health::serve`,
    /// which already exposes them on one listener.
    pub health_bind_addr: SocketAddr,
    pub api_bind_addr: Option<SocketAddr>,
    pub reservation_ttl_secs: i64,
    pub alert_webhook_url: Option<String>,
    pub alert_poll_interval_secs: u64,
    pub signer_timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub solana: SolanaConfig,
    pub goldcoin: GoldcoinConfig,
    pub reserve: ReserveConfig,
    pub operators: OperatorsConfig,
    pub service: ServiceConfig,
}

impl Config {
    /// Reads `path`, applies environment-variable overrides for RPC
    /// endpoints/credentials, then validates and resolves every field.
    /// Never partially succeeds: any read/parse/validation failure is
    /// returned before any field is usable.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut raw: RawConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        apply_env_overrides(&mut raw);
        resolve(raw)
    }

    /// Loads and validates every attestation signer named in
    /// `operators.attestation_key_paths`, in order, cross-checking each
    /// one's public half against `operators.attestation_pubkeys` at the
    /// same position. DEV/TEST POSTURE ONLY — see module docs.
    pub fn load_attestation_signers(&self) -> Result<Vec<DevAttestationSigner>, ConfigError> {
        if self.operators.attestation_key_paths.len() != self.operators.attestation_pubkeys.len() {
            return Err(ConfigError::Invalid {
                field: "operators.attestation_key_paths",
                detail: format!(
                    "{} path(s) but {} declared attestation_pubkeys — these must be 1:1",
                    self.operators.attestation_key_paths.len(),
                    self.operators.attestation_pubkeys.len()
                ),
            });
        }
        self.operators
            .attestation_key_paths
            .iter()
            .zip(&self.operators.attestation_pubkeys)
            .map(|(path, expected)| {
                let keypair = read_solana_keypair_file(path)?;
                if keypair.pubkey() != *expected {
                    return Err(ConfigError::KeyMismatch {
                        path: path.clone(),
                        expected: expected.to_string(),
                        actual: keypair.pubkey().to_string(),
                    });
                }
                Ok(DevAttestationSigner { keypair })
            })
            .collect()
    }

    /// Loads and validates every vault signer named in
    /// `operators.vault_key_paths`, cross-checked against
    /// `operators.vault_pubkeys` at the same position. DEV/TEST POSTURE
    /// ONLY — see module docs.
    pub fn load_vault_signers(&self) -> Result<Vec<DevVaultSigner>, ConfigError> {
        if self.operators.vault_key_paths.len() != self.operators.vault_pubkeys.len() {
            return Err(ConfigError::Invalid {
                field: "operators.vault_key_paths",
                detail: format!(
                    "{} path(s) but {} declared vault_pubkeys — these must be 1:1",
                    self.operators.vault_key_paths.len(),
                    self.operators.vault_pubkeys.len()
                ),
            });
        }
        self.operators
            .vault_key_paths
            .iter()
            .zip(&self.operators.vault_pubkeys)
            .map(|(path, expected)| {
                let text =
                    std::fs::read_to_string(path).map_err(|source| ConfigError::KeyFileRead {
                        path: path.clone(),
                        source,
                    })?;
                let bytes: [u8; 32] =
                    crate::goldcoin::hex::decode_exact(text.trim()).map_err(|e| {
                        ConfigError::KeyFileMalformed {
                            path: path.clone(),
                            detail: format!("expected 64 hex chars (32 bytes): {e}"),
                        }
                    })?;
                let secret_key = libsecp256k1::SecretKey::parse(&bytes).map_err(|e| {
                    ConfigError::KeyFileMalformed {
                        path: path.clone(),
                        detail: format!("not a valid secp256k1 secret key: {e:?}"),
                    }
                })?;
                let pubkey =
                    libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
                if pubkey != *expected {
                    return Err(ConfigError::KeyMismatch {
                        path: path.clone(),
                        expected: crate::goldcoin::hex::encode(expected),
                        actual: crate::goldcoin::hex::encode(&pubkey),
                    });
                }
                Ok(DevVaultSigner { secret_key, pubkey })
            })
            .collect()
    }

    /// Loads the transaction fee-payer/submitter keypair. Not a custody
    /// authority (see `orchestrator::Orchestrator` docs) — no pubkey
    /// cross-check is declared for it in config, since nothing else
    /// derives trust from which key this is. DEV/TEST POSTURE ONLY.
    pub fn load_submitter(&self) -> Result<Keypair, ConfigError> {
        read_solana_keypair_file(&self.operators.submitter_key_path)
    }

    /// Connects every `operators.attestation_remote_signers` endpoint,
    /// cross-checking each one's self-reported identity against the
    /// positionally-matching `operators.attestation_pubkeys` entry
    /// (`resolve()` already checked the two config lists agree with each
    /// other; this is the live check that the actual endpoint agrees
    /// with both). PRODUCTION-CAPABLE — see `signing::remote` module
    /// docs. Only ever called when `operators.mode ==
    /// SignerMode::Production` (see [`Config::load_signers`]).
    async fn load_attestation_signers_remote(
        &self,
    ) -> Result<Vec<RemoteAttestationSigner>, ConfigError> {
        let mut signers = Vec::with_capacity(self.operators.attestation_remote_signers.len());
        for (i, (raw, expected)) in self
            .operators
            .attestation_remote_signers
            .iter()
            .zip(&self.operators.attestation_pubkeys)
            .enumerate()
        {
            let signer = RemoteAttestationSigner::connect(raw, *expected)
                .await
                .map_err(|source| ConfigError::RemoteSignerConnect {
                    field: "operators.attestation_remote_signers",
                    index: i,
                    endpoint_url: raw.endpoint_url.clone(),
                    source,
                })?;
            signers.push(signer);
        }
        Ok(signers)
    }

    /// Connects every `operators.vault_remote_signers` endpoint. See
    /// [`Config::load_attestation_signers_remote`] — identical shape,
    /// secp256k1 rather than ed25519.
    async fn load_vault_signers_remote(&self) -> Result<Vec<RemoteVaultSigner>, ConfigError> {
        let mut signers = Vec::with_capacity(self.operators.vault_remote_signers.len());
        for (i, (raw, expected)) in self
            .operators
            .vault_remote_signers
            .iter()
            .zip(&self.operators.vault_pubkeys)
            .enumerate()
        {
            let signer = RemoteVaultSigner::connect(raw, *expected)
                .await
                .map_err(|source| ConfigError::RemoteSignerConnect {
                    field: "operators.vault_remote_signers",
                    index: i,
                    endpoint_url: raw.endpoint_url.clone(),
                    source,
                })?;
            signers.push(signer);
        }
        Ok(signers)
    }

    /// The one entry point `glc-bridge-daemon` actually calls: loads
    /// attestation and vault signers appropriate to
    /// `operators.mode`, already boxed into the trait objects
    /// `Orchestrator` depends on. `resolve()` has already fail-closed on
    /// any mismatch between `mode` and which of
    /// {`*_key_paths`, `*_remote_signers`} is populated — this function
    /// only has to pick the matching loader.
    pub async fn load_signers(&self) -> Result<LoadedSigners, ConfigError> {
        match self.operators.mode {
            SignerMode::Dev => {
                let attestation = self
                    .load_attestation_signers()?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
                    .collect();
                let vault = self
                    .load_vault_signers()?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn VaultSigner>)
                    .collect();
                Ok((attestation, vault))
            }
            SignerMode::Production => {
                let attestation = self
                    .load_attestation_signers_remote()
                    .await?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
                    .collect();
                let vault = self
                    .load_vault_signers_remote()
                    .await?
                    .into_iter()
                    .map(|s| Box::new(s) as Box<dyn VaultSigner>)
                    .collect();
                Ok((attestation, vault))
            }
        }
    }
}

/// Reads a `solana-keygen`-format key file: a JSON array of the 64
/// secret+public bytes.
fn read_solana_keypair_file(path: &Path) -> Result<Keypair, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::KeyFileRead {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes: Vec<u8> =
        serde_json::from_str(&text).map_err(|e| ConfigError::KeyFileMalformed {
            path: path.to_path_buf(),
            detail: format!("expected a JSON array of 64 bytes (solana-keygen format): {e}"),
        })?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| ConfigError::KeyFileMalformed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

fn apply_env_overrides(raw: &mut RawConfig) {
    if let Ok(v) = std::env::var("GLC_BRIDGE_SOLANA_RPC_URL") {
        raw.solana.rpc_url = v;
    }
    if let Ok(v) = std::env::var("GLC_BRIDGE_GOLDCOIN_RPC_URL") {
        raw.goldcoin.rpc_url = v;
    }
    if let Ok(v) = std::env::var("GLC_BRIDGE_GOLDCOIN_RPC_USER") {
        raw.goldcoin.rpc_user = v;
    }
    if let Ok(v) = std::env::var("GLC_BRIDGE_GOLDCOIN_RPC_PASSWORD") {
        raw.goldcoin.rpc_password = v;
    }
}

fn resolve(raw: RawConfig) -> Result<Config, ConfigError> {
    if raw.solana.commitment != "finalized" {
        return Err(ConfigError::Invalid {
            field: "solana.commitment",
            detail: format!(
                "only \"finalized\" is supported — this service's RPC layer never reads at a \
                 looser commitment (solana::rpc::RealSolanaRpc), got {:?}",
                raw.solana.commitment
            ),
        });
    }
    let reserve_token_mint =
        Pubkey::from_str(&raw.solana.reserve_token_mint).map_err(|e| ConfigError::Invalid {
            field: "solana.reserve_token_mint",
            detail: e.to_string(),
        })?;

    let network = match raw.goldcoin.network.as_str() {
        "regtest" | "testnet" => crate::goldcoin::address::Network::Testnet,
        "mainnet" => crate::goldcoin::address::Network::Mainnet,
        other => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.network",
                detail: format!("expected \"regtest\", \"testnet\", or \"mainnet\", got {other:?}"),
            });
        }
    };

    let solana_bounds = resolve_bounds(&raw.reserve.solana, "reserve.solana")?;
    let goldcoin_bounds = resolve_bounds(&raw.reserve.goldcoin, "reserve.goldcoin")?;

    let mode = match raw.operators.mode.as_str() {
        "dev" => SignerMode::Dev,
        "production" => SignerMode::Production,
        other => {
            return Err(ConfigError::Invalid {
                field: "operators.mode",
                detail: format!("expected \"dev\" or \"production\", got {other:?}"),
            });
        }
    };

    let admin_pubkey =
        Pubkey::from_str(&raw.operators.admin_pubkey).map_err(|e| ConfigError::Invalid {
            field: "operators.admin_pubkey",
            detail: e.to_string(),
        })?;
    let attestation_pubkeys = raw
        .operators
        .attestation_pubkeys
        .iter()
        .map(|s| {
            Pubkey::from_str(s).map_err(|e| ConfigError::Invalid {
                field: "operators.attestation_pubkeys",
                detail: format!("{s:?}: {e}"),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if raw.operators.attestation_threshold == 0
        || raw.operators.attestation_threshold > attestation_pubkeys.len()
    {
        return Err(ConfigError::Invalid {
            field: "operators.attestation_threshold",
            detail: format!(
                "must be between 1 and the number of attestation_pubkeys ({}), got {}",
                attestation_pubkeys.len(),
                raw.operators.attestation_threshold
            ),
        });
    }
    match mode {
        SignerMode::Production => {
            if !raw.operators.attestation_key_paths.is_empty() {
                return Err(ConfigError::ProductionModeForbidsLocalSigners {
                    field: "operators.attestation_key_paths",
                });
            }
            if raw.operators.attestation_remote_signers.len() != attestation_pubkeys.len() {
                return Err(ConfigError::RemoteSignerCountMismatch {
                    field: "operators.attestation_remote_signers",
                    pubkeys_field: "operators.attestation_pubkeys",
                    expected: attestation_pubkeys.len(),
                    actual: raw.operators.attestation_remote_signers.len(),
                    ies: plural_ies(raw.operators.attestation_remote_signers.len()),
                });
            }
            for (i, (raw_signer, expected)) in raw
                .operators
                .attestation_remote_signers
                .iter()
                .zip(&attestation_pubkeys)
                .enumerate()
            {
                let declared = Pubkey::from_str(&raw_signer.expected_public_key).map_err(|e| {
                    ConfigError::Invalid {
                        field: "operators.attestation_remote_signers.expected_public_key",
                        detail: format!("{:?}: {e}", raw_signer.expected_public_key),
                    }
                })?;
                if declared != *expected {
                    return Err(ConfigError::RemoteSignerExpectedKeyMismatch {
                        field: "operators.attestation_remote_signers",
                        pubkeys_field: "operators.attestation_pubkeys",
                        index: i,
                        declared: declared.to_string(),
                        from_pubkeys: expected.to_string(),
                    });
                }
            }
            if let Some((first_index, dup_index)) = find_duplicate(&attestation_pubkeys) {
                return Err(ConfigError::DuplicateRemoteSignerPubkey {
                    field: "operators.attestation_remote_signers",
                    pubkey: attestation_pubkeys[dup_index].to_string(),
                    first_index,
                    dup_index,
                });
            }
            let attestation_endpoint_urls: Vec<&str> = raw
                .operators
                .attestation_remote_signers
                .iter()
                .map(|s| s.endpoint_url.as_str())
                .collect();
            if let Some((first_index, dup_index)) = find_duplicate(&attestation_endpoint_urls) {
                return Err(ConfigError::DuplicateRemoteSignerEndpoint {
                    field: "operators.attestation_remote_signers",
                    endpoint_url: attestation_endpoint_urls[dup_index].to_string(),
                    first_index,
                    dup_index,
                });
            }
        }
        SignerMode::Dev => {
            if !raw.operators.attestation_remote_signers.is_empty() {
                return Err(ConfigError::DevModeForbidsRemoteSigners {
                    field: "operators.attestation_remote_signers",
                });
            }
        }
    }
    let attestation_remote_signers = raw
        .operators
        .attestation_remote_signers
        .iter()
        .map(raw_remote_signer_to_config)
        .collect();

    let vault_pubkeys = raw
        .operators
        .vault_pubkeys
        .iter()
        .map(|s| {
            crate::goldcoin::hex::decode_exact::<33>(s).map_err(|e| ConfigError::Invalid {
                field: "operators.vault_pubkeys",
                detail: format!("{s:?}: {e}"),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if raw.operators.vault_threshold == 0
        || raw.operators.vault_threshold as usize > vault_pubkeys.len()
    {
        return Err(ConfigError::Invalid {
            field: "operators.vault_threshold",
            detail: format!(
                "must be between 1 and the number of vault_pubkeys ({}), got {}",
                vault_pubkeys.len(),
                raw.operators.vault_threshold
            ),
        });
    }
    match mode {
        SignerMode::Production => {
            if !raw.operators.vault_key_paths.is_empty() {
                return Err(ConfigError::ProductionModeForbidsLocalSigners {
                    field: "operators.vault_key_paths",
                });
            }
            if raw.operators.vault_remote_signers.len() != vault_pubkeys.len() {
                return Err(ConfigError::RemoteSignerCountMismatch {
                    field: "operators.vault_remote_signers",
                    pubkeys_field: "operators.vault_pubkeys",
                    expected: vault_pubkeys.len(),
                    actual: raw.operators.vault_remote_signers.len(),
                    ies: plural_ies(raw.operators.vault_remote_signers.len()),
                });
            }
            for (i, (raw_signer, expected)) in raw
                .operators
                .vault_remote_signers
                .iter()
                .zip(&vault_pubkeys)
                .enumerate()
            {
                let declared =
                    crate::goldcoin::hex::decode_exact::<33>(&raw_signer.expected_public_key)
                        .map_err(|e| ConfigError::Invalid {
                            field: "operators.vault_remote_signers.expected_public_key",
                            detail: format!("{:?}: {e}", raw_signer.expected_public_key),
                        })?;
                if declared != *expected {
                    return Err(ConfigError::RemoteSignerExpectedKeyMismatch {
                        field: "operators.vault_remote_signers",
                        pubkeys_field: "operators.vault_pubkeys",
                        index: i,
                        declared: crate::goldcoin::hex::encode(&declared),
                        from_pubkeys: crate::goldcoin::hex::encode(expected),
                    });
                }
            }
            if let Some((first_index, dup_index)) = find_duplicate(&vault_pubkeys) {
                return Err(ConfigError::DuplicateRemoteSignerPubkey {
                    field: "operators.vault_remote_signers",
                    pubkey: crate::goldcoin::hex::encode(&vault_pubkeys[dup_index]),
                    first_index,
                    dup_index,
                });
            }
            let vault_endpoint_urls: Vec<&str> = raw
                .operators
                .vault_remote_signers
                .iter()
                .map(|s| s.endpoint_url.as_str())
                .collect();
            if let Some((first_index, dup_index)) = find_duplicate(&vault_endpoint_urls) {
                return Err(ConfigError::DuplicateRemoteSignerEndpoint {
                    field: "operators.vault_remote_signers",
                    endpoint_url: vault_endpoint_urls[dup_index].to_string(),
                    first_index,
                    dup_index,
                });
            }
        }
        SignerMode::Dev => {
            if !raw.operators.vault_remote_signers.is_empty() {
                return Err(ConfigError::DevModeForbidsRemoteSigners {
                    field: "operators.vault_remote_signers",
                });
            }
        }
    }
    let vault_remote_signers = raw
        .operators
        .vault_remote_signers
        .iter()
        .map(raw_remote_signer_to_config)
        .collect();

    let health_bind_addr =
        SocketAddr::from_str(&raw.service.health_bind_addr).map_err(|e| ConfigError::Invalid {
            field: "service.health_bind_addr",
            detail: e.to_string(),
        })?;
    let api_bind_addr = raw
        .service
        .api_bind_addr
        .as_deref()
        .map(SocketAddr::from_str)
        .transpose()
        .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
            field: "service.api_bind_addr",
            detail: e.to_string(),
        })?;
    let alert_webhook_url = raw
        .service
        .alert_webhook_url
        .map(|url| {
            reqwest::Url::parse(&url)
                .map(|_| url)
                .map_err(|e| ConfigError::Invalid {
                    field: "service.alert_webhook_url",
                    detail: e.to_string(),
                })
        })
        .transpose()?;

    if raw.goldcoin.change_fanout_max_outputs == 0 {
        return Err(ConfigError::Invalid {
            field: "goldcoin.change_fanout_max_outputs",
            detail: "must be at least 1".to_string(),
        });
    }
    if raw.goldcoin.utxo_pool_warning_count < raw.goldcoin.utxo_pool_min_available_count {
        return Err(ConfigError::Invalid {
            field: "goldcoin.utxo_pool_warning_count",
            detail: format!(
                "must be >= utxo_pool_min_available_count ({}), got {}",
                raw.goldcoin.utxo_pool_min_available_count, raw.goldcoin.utxo_pool_warning_count
            ),
        });
    }

    // Goldcoin initial-checkpoint bootstrap: only structural validation
    // here (no chain connection at config-load time) — see
    // `goldcoin::indexer::InitialCheckpoint`'s own docs for the live
    // getblockhash/above-tip checks made at indexer bootstrap time.
    // `height`/`hash` must be given together or not at all: a partial
    // pair (one set, the other absent) is exactly the "malformed config"
    // case that must fail closed here rather than silently either being
    // ignored or accepted with a missing half.
    let initial_checkpoint = match (
        raw.goldcoin.initial_checkpoint_height,
        raw.goldcoin.initial_checkpoint_hash,
    ) {
        (None, None) => None,
        (Some(_), None) => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.initial_checkpoint_hash",
                detail: "initial_checkpoint_height is set but initial_checkpoint_hash is not — \
                         both must be configured together, or neither"
                    .to_string(),
            })
        }
        (None, Some(_)) => {
            return Err(ConfigError::Invalid {
                field: "goldcoin.initial_checkpoint_height",
                detail: "initial_checkpoint_hash is set but initial_checkpoint_height is not — \
                         both must be configured together, or neither"
                    .to_string(),
            })
        }
        (Some(height), Some(hash)) => {
            if height < 0 {
                return Err(ConfigError::Invalid {
                    field: "goldcoin.initial_checkpoint_height",
                    detail: format!("must be >= 0, got {height}"),
                });
            }
            if crate::goldcoin::hex::decode_exact::<32>(&hash).is_err() {
                return Err(ConfigError::Invalid {
                    field: "goldcoin.initial_checkpoint_hash",
                    detail: format!("{hash:?} is not exactly 32 bytes of hex"),
                });
            }
            Some(crate::goldcoin::indexer::InitialCheckpoint {
                height,
                hash,
                operator_acknowledged_no_prior_deposits: raw
                    .goldcoin
                    .initial_checkpoint_operator_acknowledged_no_prior_deposits,
            })
        }
    };

    Ok(Config {
        solana: SolanaConfig {
            rpc_url: raw.solana.rpc_url,
            reserve_token_mint,
        },
        goldcoin: GoldcoinConfig {
            network,
            rpc_url: raw.goldcoin.rpc_url,
            rpc_user: raw.goldcoin.rpc_user,
            rpc_password: raw.goldcoin.rpc_password,
            rpc_connect_timeout_ms: raw.goldcoin.rpc_connect_timeout_ms,
            rpc_read_timeout_ms: raw.goldcoin.rpc_read_timeout_ms,
            confirmation_depth: raw.goldcoin.confirmation_depth,
            max_reorg_depth: raw.goldcoin.max_reorg_depth,
            required_payout_confirmations: raw.goldcoin.required_payout_confirmations,
            vault_min_confirmations: raw.goldcoin.vault_min_confirmations,
            fee_rate_per_kb: raw.goldcoin.fee_rate_per_kb,
            dust_threshold: raw.goldcoin.dust_threshold,
            max_inputs: raw.goldcoin.max_inputs,
            change_fanout_target_atomic: raw.goldcoin.change_fanout_target_atomic,
            change_fanout_max_outputs: raw.goldcoin.change_fanout_max_outputs,
            utxo_pool_min_available_count: raw.goldcoin.utxo_pool_min_available_count,
            utxo_pool_warning_count: raw.goldcoin.utxo_pool_warning_count,
            initial_checkpoint,
        },
        reserve: ReserveConfig {
            reconciliation_tolerance: raw.reserve.reconciliation_tolerance,
            solana: solana_bounds,
            goldcoin: goldcoin_bounds,
        },
        operators: OperatorsConfig {
            mode,
            admin_pubkey,
            attestation_threshold: raw.operators.attestation_threshold,
            attestation_pubkeys,
            attestation_key_paths: raw.operators.attestation_key_paths,
            attestation_remote_signers,
            vault_threshold: raw.operators.vault_threshold,
            vault_pubkeys,
            vault_key_paths: raw.operators.vault_key_paths,
            vault_remote_signers,
            submitter_key_path: raw.operators.submitter_key_path,
        },
        service: ServiceConfig {
            db_path: raw.service.db_path,
            tick_interval_ms: raw.service.tick_interval_ms,
            health_bind_addr,
            api_bind_addr,
            reservation_ttl_secs: raw.service.reservation_ttl_secs,
            alert_webhook_url,
            alert_poll_interval_secs: raw.service.alert_poll_interval_secs,
            signer_timeout_ms: raw.service.signer_timeout_ms,
        },
    })
}

fn raw_remote_signer_to_config(raw: &RawRemoteSigner) -> RemoteSignerConfig {
    RemoteSignerConfig {
        endpoint_url: raw.endpoint_url.clone(),
        auth_token_env: raw.auth_token_env.clone(),
        timeout: Duration::from_millis(raw.timeout_ms),
    }
}

fn plural_ies(n: usize) -> &'static str {
    if n == 1 {
        "y"
    } else {
        "ies"
    }
}

/// Returns `Some((first_index, dup_index))` for the first repeated value
/// in `items`, or `None` if every value is distinct. Used to reject
/// remote-signer entries that share an `expected_public_key` or
/// `endpoint_url` — production custody domains must be genuinely
/// independent, and either kind of duplicate silently collapses two
/// configured domains into one.
fn find_duplicate<T: Eq + std::hash::Hash>(items: &[T]) -> Option<(usize, usize)> {
    let mut seen: std::collections::HashMap<&T, usize> = std::collections::HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Some(&first) = seen.get(item) {
            return Some((first, i));
        }
        seen.insert(item, i);
    }
    None
}

fn resolve_bounds(
    raw: &RawReserveBounds,
    field: &'static str,
) -> Result<ReserveBounds, ConfigError> {
    // Mirrors `Ledger::configure_reserve`'s own assertion exactly, so a
    // config that would panic deep inside ledger setup is instead rejected
    // here with a clear message before anything opens the database.
    if raw.critical_reserve <= raw.protected_minimum {
        return Err(ConfigError::Invalid {
            field,
            detail: format!(
                "critical_reserve ({}) must exceed protected_minimum ({}) — \
                 docs/05-reserve-accounting.md",
                raw.critical_reserve, raw.protected_minimum
            ),
        });
    }
    Ok(ReserveBounds {
        protected_minimum: raw.protected_minimum,
        target_reserve: raw.target_reserve,
        warning_reserve: raw.warning_reserve,
        critical_reserve: raw.critical_reserve,
    })
}

#[cfg(test)]
mod tests;
