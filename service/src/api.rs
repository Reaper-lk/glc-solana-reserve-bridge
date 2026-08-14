//! The minimal HTTP surface a client (the bridge frontend, or any other
//! caller) needs to submit and track a bridge request
//! (docs/15-post-phase6-audit.md P0 item 4). Before this module, the only
//! way to reach `Ledger::create_request`/`Ledger::get_request` was a
//! direct in-process Rust call or raw SQL — there was no network-facing
//! way for anything external to interact with the bridge at all.
//!
//! # What this exposes, and what it deliberately does not
//!
//! Five read/write operations, matched one-to-one to what an external
//! caller actually needs: bridge status, transfer limits, reserve
//! availability, creating a GLC -> Solana transfer (which requires
//! reserving capacity and handing back deposit instructions), and looking
//! up a transfer's lifecycle by id.
//!
//! It never exposes: custody keys or any signing material (this module
//! never touches [`crate::signing`]), privileged admin operations (pause/
//! unpause/limit changes stay on `glc-admin`, gated by possession of the
//! admin keypair), or infrastructure detail (RPC URLs, database paths,
//! indexer internals — that is what `ops::health` is for, and that
//! endpoint's own docs already say to bind it privately for exactly this
//! reason). Reserve figures here are limited to *available capacity* — a
//! derived, bounded number ("how much can currently move") — not the raw
//! `total_reserve_balance`/`protected_minimum`/`reserved_liquidity`
//! breakdown `ops::health` reports for an operator audience.
//!
//! # Solana -> GLC has no "create" step here
//!
//! A GLC -> Solana transfer must reserve capacity and obtain a
//! request-bound deposit address before any Goldcoin transaction can
//! reference it (the request id is embedded in the deposit's own binding
//! — see [`crate::goldcoin::deposit::encode_request_binding`]), so
//! `POST /transfers` exists for that direction. A Solana -> Goldcoin
//! transfer works the other way around: the user calls
//! `deposit_to_reserve` directly on-chain themselves (a plain SPL
//! transfer plus this bridge's own instruction, requiring no interaction
//! with this service beforehand), and this service's Solana indexer picks
//! it up automatically. There is nothing to "create" here for that
//! direction — `GET /status`'s `next_solana_obligation_index` is the one
//! piece of information a caller needs to construct that transaction
//! themselves.
//!
//! # No federation-era or wrapped-token language
//!
//! This is a reserve-backed bridge, not a federated one, and it does not
//! wrap or mint anything (docs/15-post-phase6-audit.md §4/§20) — there is
//! deliberately no `/federation`, `/federation/rounds`, or similarly
//! shaped endpoint here, even though a pre-existing frontend built against
//! the old bridge expects some. Connecting that frontend to this service
//! is later integration work, not something this module should paper
//! over by inventing federation-shaped responses that don't correspond to
//! anything this bridge actually does.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::goldcoin::deposit::encode_request_binding;
use crate::goldcoin::hex as glc_hex;
use crate::ledger::{CreateRequestOutcome, Direction, Ledger, LedgerError, ReserveDirection};
use crate::solana::accounts;
use crate::solana::rpc::SolanaRpc;

#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeStatus {
    pub goldcoin_paused: bool,
    pub solana_paused: bool,
    pub vault_address: String,
    pub next_solana_obligation_index: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferLimits {
    pub min_transfer_amount: u64,
    pub per_transfer_limit: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReserveAvailability {
    pub goldcoin_available_capacity: i64,
    pub solana_available_capacity: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTransferInput {
    pub amount_atomic: u64,
    /// Base58 Solana pubkey the released funds should be sent to.
    pub recipient: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTransferOutput {
    pub request_id: i64,
    /// Send Goldcoin to this address to fund the transfer.
    pub deposit_vault_address: String,
    /// Hex-encoded 32 bytes that must appear as an `OP_RETURN` output on
    /// the deposit transaction — this is what binds the deposit to
    /// `request_id` (see
    /// [`crate::goldcoin::deposit::encode_request_binding`]). This
    /// service never constructs the deposit transaction itself; building
    /// and broadcasting it is the caller's own wallet's job.
    pub deposit_binding_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransferView {
    pub id: i64,
    pub direction: String,
    pub state: String,
    pub amount_atomic: u64,
    pub created_at: i64,
    pub source_txid: Option<String>,
    pub source_confirmations: i64,
    pub destination_txid: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("the destination reserve cannot currently cover this amount (available: {available})")]
    InsufficientLiquidity { available: i64 },
    #[error("the destination reserve is paused")]
    Paused,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("could not read live chain state: {0}")]
    Upstream(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::InsufficientLiquidity { .. } | ApiError::Paused => StatusCode::CONFLICT,
            ApiError::Ledger(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Upstream(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything the HTTP layer needs; implemented once against the real
/// ledger/chain ([`BridgeApi`]) and mockable for tests.
pub trait ApiSource: Send + Sync + 'static {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>>;
    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>>;
    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>>;
    fn create_glc_to_sol_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>>;
    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>>;
}

/// The concrete [`ApiSource`]: a fresh [`Ledger`] connection per call
/// (same concurrency model as [`crate::ops::collector::OpsCollector`] —
/// SQLite's own `BEGIN IMMEDIATE` transactions are the real safety
/// boundary, not a single shared in-process handle) plus a live chain
/// read for the handful of fields ([`BridgeStatus`]/[`TransferLimits`])
/// that only the on-chain `BridgeConfig` actually knows.
pub struct BridgeApi<SR: SolanaRpc> {
    db_path: PathBuf,
    solana_rpc: SR,
    vault_address: String,
    reservation_ttl_secs: i64,
}

impl<SR: SolanaRpc> BridgeApi<SR> {
    pub fn new(
        db_path: PathBuf,
        solana_rpc: SR,
        vault_address: String,
        reservation_ttl_secs: i64,
    ) -> Self {
        BridgeApi {
            db_path,
            solana_rpc,
            vault_address,
            reservation_ttl_secs,
        }
    }

    fn open_ledger(&self) -> Result<Ledger, ApiError> {
        Ok(Ledger::open(&self.db_path)?)
    }

    async fn fetch_bridge_config(&self) -> Result<accounts::BridgeConfigSnapshot, ApiError> {
        let account = self
            .solana_rpc
            .get_account(&accounts::bridge_config_pda())
            .await
            .map_err(|e| ApiError::Upstream(e.to_string()))?
            .ok_or_else(|| ApiError::Upstream("bridge_config account does not exist yet".into()))?;
        accounts::decode_bridge_config(&account.data).map_err(|e| ApiError::Upstream(e.to_string()))
    }
}

impl<SR: SolanaRpc + Send + Sync + 'static> ApiSource for BridgeApi<SR> {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let config = self.fetch_bridge_config().await?;
            Ok(BridgeStatus {
                goldcoin_paused: ledger.is_paused(ReserveDirection::GoldcoinReserve)?,
                solana_paused: ledger.is_paused(ReserveDirection::SolanaReserve)?,
                vault_address: self.vault_address.clone(),
                next_solana_obligation_index: config.obligation_count,
            })
        })
    }

    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>> {
        Box::pin(async move {
            let config = self.fetch_bridge_config().await?;
            Ok(TransferLimits {
                min_transfer_amount: config.min_transfer_amount,
                per_transfer_limit: config.per_transfer_limit,
            })
        })
    }

    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            Ok(ReserveAvailability {
                goldcoin_available_capacity: ledger
                    .available_capacity(ReserveDirection::GoldcoinReserve)?,
                solana_available_capacity: ledger
                    .available_capacity(ReserveDirection::SolanaReserve)?,
            })
        })
    }

    fn create_glc_to_sol_transfer(
        &self,
        input: CreateTransferInput,
    ) -> BoxFut<'_, Result<CreateTransferOutput, ApiError>> {
        Box::pin(async move {
            if input.amount_atomic == 0 {
                return Err(ApiError::BadRequest("amount_atomic must be > 0".into()));
            }
            let recipient = input
                .recipient
                .parse::<Pubkey>()
                .map_err(|e| ApiError::BadRequest(format!("invalid recipient: {e}")))?;
            let mut ledger = self.open_ledger()?;
            let now = now_unix();
            let outcome = ledger.create_request(
                Direction::GlcToSol,
                input.amount_atomic,
                &recipient.to_bytes(),
                None,
                self.reservation_ttl_secs,
                now,
            )?;
            match outcome {
                CreateRequestOutcome::Reserved { request_id } => Ok(CreateTransferOutput {
                    request_id,
                    deposit_vault_address: self.vault_address.clone(),
                    deposit_binding_hex: glc_hex::encode(&encode_request_binding(request_id)),
                }),
                CreateRequestOutcome::InsufficientLiquidity { available_capacity } => {
                    Err(ApiError::InsufficientLiquidity {
                        available: available_capacity,
                    })
                }
                CreateRequestOutcome::Paused => Err(ApiError::Paused),
            }
        })
    }

    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>> {
        Box::pin(async move {
            let ledger = self.open_ledger()?;
            let Some(request) = ledger.get_request(id)? else {
                return Ok(None);
            };
            let destination_txid = ledger
                .get_destination_txid(id)?
                .map(|bytes| glc_hex::encode(&bytes));
            Ok(Some(TransferView {
                id: request.id,
                direction: request.direction.as_str().to_string(),
                state: request.state.as_str().to_string(),
                amount_atomic: request.amount_atomic,
                created_at: request.created_at,
                source_txid: request.source_txid.map(|t| glc_hex::encode(&t)),
                source_confirmations: request.source_confirmations,
                destination_txid,
                failure_reason: request.failure_reason,
            }))
        })
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .expect("well-formed response")
}

fn error_response(err: ApiError) -> Response<Full<Bytes>> {
    json_response(
        err.status(),
        &ErrorBody {
            error: err.to_string(),
        },
    )
}

async fn handle<S: ApiSource>(
    req: Request<hyper::body::Incoming>,
    source: Arc<S>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = match (&method, path.as_str()) {
        (&Method::GET, "/status") => match source.status().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/limits") => match source.limits().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::GET, "/reserve") => match source.reserve().await {
            Ok(v) => json_response(StatusCode::OK, &v),
            Err(e) => error_response(e),
        },
        (&Method::POST, "/transfers") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorBody {
                            error: "could not read request body".into(),
                        },
                    ))
                }
            };
            match serde_json::from_slice::<CreateTransferInput>(&body) {
                Ok(input) => match source.create_glc_to_sol_transfer(input).await {
                    Ok(v) => json_response(StatusCode::CREATED, &v),
                    Err(e) => error_response(e),
                },
                Err(e) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody {
                        error: format!("malformed request body: {e}"),
                    },
                ),
            }
        }
        (&Method::GET, p) if p.starts_with("/transfers/") => {
            let id_str = &p["/transfers/".len()..];
            match id_str.parse::<i64>() {
                Ok(id) => match source.get_transfer(id).await {
                    Ok(Some(v)) => json_response(StatusCode::OK, &v),
                    Ok(None) => json_response(
                        StatusCode::NOT_FOUND,
                        &ErrorBody {
                            error: format!("no transfer with id {id}"),
                        },
                    ),
                    Err(e) => error_response(e),
                },
                Err(_) => json_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorBody {
                        error: "transfer id must be an integer".into(),
                    },
                ),
            }
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            &ErrorBody {
                error: "not found".into(),
            },
        ),
    };
    Ok(response)
}

/// Serves the bridge API until `shutdown` fires. No authentication and no
/// TLS termination here (same posture as [`crate::ops::health::serve`]) —
/// run this behind a reverse proxy that provides both if it is ever
/// reachable from outside a trusted network.
pub async fn serve<S: ApiSource>(
    addr: SocketAddr,
    source: Arc<S>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "bridge API listening");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("bridge API: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "bridge API: accept failed");
                        continue;
                    }
                };
                let source = Arc::clone(&source);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(req, Arc::clone(&source)));
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%peer, error = %e, "bridge API connection ended");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
