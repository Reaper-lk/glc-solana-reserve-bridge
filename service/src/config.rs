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

use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use crate::signing::attestation::DevAttestationSigner;
use crate::signing::goldcoin_vault::DevVaultSigner;

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
    admin_pubkey: String,
    attestation_threshold: usize,
    attestation_pubkeys: Vec<String>,
    attestation_key_paths: Vec<PathBuf>,
    vault_threshold: u8,
    vault_pubkeys: Vec<String>,
    vault_key_paths: Vec<PathBuf>,
    submitter_key_path: PathBuf,
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
    pub admin_pubkey: Pubkey,
    pub attestation_threshold: usize,
    pub attestation_pubkeys: Vec<Pubkey>,
    pub attestation_key_paths: Vec<PathBuf>,
    pub vault_threshold: u8,
    pub vault_pubkeys: Vec<[u8; 33]>,
    pub vault_key_paths: Vec<PathBuf>,
    pub submitter_key_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub db_path: PathBuf,
    pub tick_interval_ms: u64,
    /// Serves both `/health` and `/metrics` — see `ops::health::serve`,
    /// which already exposes them on one listener.
    pub health_bind_addr: SocketAddr,
    pub api_bind_addr: Option<SocketAddr>,
    pub reservation_ttl_secs: i64,
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
        },
        reserve: ReserveConfig {
            reconciliation_tolerance: raw.reserve.reconciliation_tolerance,
            solana: solana_bounds,
            goldcoin: goldcoin_bounds,
        },
        operators: OperatorsConfig {
            admin_pubkey,
            attestation_threshold: raw.operators.attestation_threshold,
            attestation_pubkeys,
            attestation_key_paths: raw.operators.attestation_key_paths,
            vault_threshold: raw.operators.vault_threshold,
            vault_pubkeys,
            vault_key_paths: raw.operators.vault_key_paths,
            submitter_key_path: raw.operators.submitter_key_path,
        },
        service: ServiceConfig {
            db_path: raw.service.db_path,
            tick_interval_ms: raw.service.tick_interval_ms,
            health_bind_addr,
            api_bind_addr,
            reservation_ttl_secs: raw.service.reservation_ttl_secs,
        },
    })
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
