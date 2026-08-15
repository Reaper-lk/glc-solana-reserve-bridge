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

use crate::amount_conversion;
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
    /// Canonical units (docs/20-bridge-fee.md) — what the user declared/
    /// deposited, before the bridge fee.
    pub gross_amount_atomic: u64,
    pub fee_bps: u64,
    /// Canonical units.
    pub fee_amount_atomic: u64,
    /// Canonical units — the real-world GLC entitlement delivered.
    pub net_amount_atomic: u64,
    pub created_at: i64,
    pub source_txid: Option<String>,
    pub source_confirmations: i64,
    pub destination_txid: Option<String>,
    pub failure_reason: Option<String>,
}

/// Caller input for `GET /quote`: how much GROSS the caller intends to
/// bridge, in the ledger's canonical accounting unit (8 decimals,
/// docs/20-bridge-fee.md), regardless of direction. A future UI converts
/// a user-typed decimal GLC amount to this unit itself (`* 10^8`) before
/// calling — kept as one single, unambiguous unit here rather than one
/// that varies by direction, consistent with `CreateTransferInput`.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteInput {
    pub direction: String,
    pub gross_amount: u64,
}

/// Server-authoritative bridge quote. The UI displaying "You bridge: X
/// GLC / Bridge fee (1%): Y GLC / You receive: Z GLC" must source X/Y/Z
/// from here, never compute them itself (docs/20-bridge-fee.md) — this
/// endpoint runs the exact same `amount_conversion::compute_fee` the
/// server uses to actually build a settlement, so a quote can never
/// promise something a real transfer would compute differently.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteOutput {
    pub direction: String,
    /// Canonical units.
    pub gross_amount: u64,
    /// Human-readable GLC, computed via checked integer arithmetic (never
    /// a float) — e.g. `"12.34500000"`.
    pub gross_display_amount: String,
    pub fee_bps: u64,
    /// Canonical units.
    pub fee_amount: u64,
    pub fee_display_amount: String,
    /// Canonical units — the real-world GLC entitlement, before the
    /// destination chain's own decimal precision is applied.
    pub net_amount: u64,
    pub net_display_amount: String,
    /// The SOURCE chain's own atomic-unit decimals for this direction.
    pub source_decimals: u8,
    /// The DESTINATION chain's own atomic-unit decimals for this
    /// direction — what `net_amount` is actually converted to and
    /// released as, on-chain.
    pub destination_decimals: u8,
    pub source_asset: String,
    pub destination_asset: String,
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
    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>>;
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
            // `input.amount_atomic` is the caller-declared GROSS amount,
            // canonical units (Goldcoin-native) — the fee/net breakdown is
            // computed authoritatively HERE, server-side, at the fixed
            // protocol rate; nothing about it is accepted from the caller
            // (docs/20-bridge-fee.md: "never trust gross, fee or net
            // calculations supplied by the UI"). `CreateTransferInput` has
            // no fee/net field for exactly this reason — there is nothing
            // for a client to submit that could bypass or alter the fee.
            let fee_breakdown = amount_conversion::compute_fee(amount_conversion::CanonicalAtomic(
                input.amount_atomic,
            ))
            .map_err(|e| ApiError::BadRequest(format!("invalid amount: {e}")))?;
            let config = self.fetch_bridge_config().await?;
            let solana_decimals =
                accounts::fetch_reserve_mint_decimals(&self.solana_rpc, &config.reserve_token_mint)
                    .await
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
            let net_destination = fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                ApiError::BadRequest(format!(
                    "amount {} cannot be represented exactly after the bridge fee at the \
                     reserve mint's {solana_decimals}-decimal precision: {e}",
                    input.amount_atomic
                ))
            })?;
            let amounts = crate::ledger::RequestAmounts {
                gross_atomic: fee_breakdown.gross.0,
                fee_bps: fee_breakdown.fee_bps,
                fee_atomic: fee_breakdown.fee.0,
                net_atomic: fee_breakdown.net.0,
                net_destination_atomic: net_destination.0,
            };
            let mut ledger = self.open_ledger()?;
            let now = now_unix();
            let outcome = ledger.create_request(
                Direction::GlcToSol,
                amounts,
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
                gross_amount_atomic: request.gross_amount_atomic,
                fee_bps: request.fee_bps,
                fee_amount_atomic: request.fee_amount_atomic,
                net_amount_atomic: request.net_amount_atomic,
                created_at: request.created_at,
                source_txid: request.source_txid.map(|t| glc_hex::encode(&t)),
                source_confirmations: request.source_confirmations,
                destination_txid,
                failure_reason: request.failure_reason,
            }))
        })
    }

    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>> {
        Box::pin(async move {
            let direction: Direction = input
                .direction
                .parse()
                .map_err(|e: String| ApiError::BadRequest(e))?;
            if input.gross_amount == 0 {
                return Err(ApiError::BadRequest("gross_amount must be > 0".into()));
            }
            let config = self.fetch_bridge_config().await?;
            let solana_decimals =
                accounts::fetch_reserve_mint_decimals(&self.solana_rpc, &config.reserve_token_mint)
                    .await
                    .map_err(|e| ApiError::Upstream(e.to_string()))?;
            let goldcoin_decimals = amount_conversion::GOLDCOIN_DECIMALS as u8;
            let (source_decimals, destination_decimals, source_asset, destination_asset) =
                match direction {
                    Direction::GlcToSol => (
                        goldcoin_decimals,
                        solana_decimals,
                        "GLC (Goldcoin)",
                        "GLC (Solana)",
                    ),
                    Direction::SolToGlc => (
                        solana_decimals,
                        goldcoin_decimals,
                        "GLC (Solana)",
                        "GLC (Goldcoin)",
                    ),
                };
            let fee_breakdown = amount_conversion::compute_fee(amount_conversion::CanonicalAtomic(
                input.gross_amount,
            ))
            .map_err(|e| ApiError::BadRequest(format!("invalid amount: {e}")))?;
            // Confirms the net entitlement is actually deliverable at the
            // destination chain's real precision — a quote must never
            // promise an amount a real transfer would then reject
            // (docs/20-bridge-fee.md).
            match direction {
                Direction::GlcToSol => {
                    fee_breakdown.net.to_solana(solana_decimals).map_err(|e| {
                        ApiError::BadRequest(format!(
                            "amount {} cannot be represented exactly after the bridge fee at \
                             the reserve mint's {solana_decimals}-decimal precision: {e}",
                            input.gross_amount
                        ))
                    })?;
                }
                Direction::SolToGlc => {} // canonical == Goldcoin-native; always exact
            }
            Ok(QuoteOutput {
                direction: input.direction,
                gross_amount: fee_breakdown.gross.0,
                gross_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.gross.0,
                    goldcoin_decimals,
                ),
                fee_bps: fee_breakdown.fee_bps,
                fee_amount: fee_breakdown.fee.0,
                fee_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.fee.0,
                    goldcoin_decimals,
                ),
                net_amount: fee_breakdown.net.0,
                net_display_amount: format_atomic_as_decimal_string(
                    fee_breakdown.net.0,
                    goldcoin_decimals,
                ),
                source_decimals,
                destination_decimals,
                source_asset: source_asset.to_string(),
                destination_asset: destination_asset.to_string(),
            })
        })
    }
}

/// Renders an atomic amount as a fixed-point decimal string via checked
/// integer arithmetic only — never a float (docs/20-bridge-fee.md).
fn format_atomic_as_decimal_string(atomic: u64, decimals: u8) -> String {
    let scale = 10u64.pow(u32::from(decimals));
    let whole = atomic / scale;
    let frac = atomic % scale;
    format!("{whole}.{frac:0width$}", width = decimals as usize)
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
        (&Method::POST, "/quote") => {
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
            match serde_json::from_slice::<QuoteInput>(&body) {
                Ok(input) => match source.quote(input).await {
                    Ok(v) => json_response(StatusCode::OK, &v),
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
