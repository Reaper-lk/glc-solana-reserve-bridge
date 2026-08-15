use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction as SolanaTx;

use super::*;
use crate::ledger::ReserveDirection;
use crate::solana::rpc::SolanaRpcError;

struct FakeSolanaRpc {
    bridge_config: Vec<u8>,
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md); `fake_bridge_config_bytes`'s `reserve_token_mint` is
/// always `[9u8; 32]`, so `FakeSolanaRpc` serves a fake mint account there
/// for `fetch_reserve_mint_decimals`'s live read (docs/20-bridge-fee.md).
const TEST_SOLANA_DECIMALS: u8 = 6;

/// A minimal, real 82-byte `spl_token::state::Mint`-shaped buffer — see
/// the matching helper in `signing::attestation::tests`.
fn fake_mint_bytes(decimals: u8) -> Vec<u8> {
    let mut v = vec![0u8; 82];
    v[44] = decimals;
    v[45] = 1; // is_initialized
    v
}

impl SolanaRpc for FakeSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        if *pubkey == accounts::bridge_config_pda() {
            return Ok(Some(Account {
                lamports: 1,
                data: self.bridge_config.clone(),
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        if *pubkey == Pubkey::new_from_array([9u8; 32]) {
            return Ok(Some(Account {
                lamports: 1,
                data: fake_mint_bytes(TEST_SOLANA_DECIMALS),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        Ok(None)
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!()
    }
    async fn send_transaction(&self, _tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!()
    }
}

/// Mirrors `solana::accounts::tests::fake_bridge_config_bytes`'s layout —
/// duplicated here (small, self-contained) rather than reused across a
/// private-module boundary.
fn fake_bridge_config_bytes(
    obligation_count: u64,
    min_transfer: u64,
    per_transfer: u64,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin: None
    v.push(0); // paused
    v.push(0); // release_paused
    v.push(0); // deposit_paused
    v.push(7); // bump
    v.extend_from_slice(&[9u8; 32]); // reserve_token_mint
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3); // reserve_authority_bump
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
    v.extend_from_slice(&min_transfer.to_le_bytes());
    v.extend_from_slice(&per_transfer.to_le_bytes());
    v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
    v.extend_from_slice(&2_000_000u64.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v
}

fn build(db_path: &std::path::Path, obligation_count: u64) -> BridgeApi<FakeSolanaRpc> {
    BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(obligation_count, 100, 1_000_000),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
    )
}

fn configure(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("ledger.sqlite3");
    let mut ledger = Ledger::open(&db_path).unwrap();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
            .unwrap();
    }
    db_path
}

#[tokio::test]
async fn status_reports_pause_state_and_next_obligation_index() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 42);

    let status = api.status().await.unwrap();
    assert!(!status.goldcoin_paused);
    assert!(!status.solana_paused);
    assert_eq!(status.next_solana_obligation_index, 42);
    assert_eq!(status.vault_address, "REGTESTVAULTADDRESSXXXXXXXXXXXXX");
}

#[tokio::test]
async fn status_reflects_a_paused_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("test"))
            .unwrap();
    }
    let api = build(&db_path, 0);
    let status = api.status().await.unwrap();
    assert!(status.goldcoin_paused);
    assert!(!status.solana_paused);
}

#[tokio::test]
async fn limits_reflects_the_live_bridge_config() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let limits = api.limits().await.unwrap();
    assert_eq!(limits.min_transfer_amount, 100);
    assert_eq!(limits.per_transfer_limit, 1_000_000);
    assert_eq!(
        limits.bridge_fee_bps,
        amount_conversion::BRIDGE_FEE_BPS,
        "the fee rate must be the fixed protocol constant, discoverable without a quote"
    );
}

#[tokio::test]
async fn status_reports_direction_availability_reflecting_pause_and_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let status = api.status().await.unwrap();
    assert!(status.glc_to_sol_available);
    assert!(status.sol_to_glc_available);

    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // GlcToSol's destination is the Solana reserve.
        ledger
            .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
            .unwrap();
    }
    let status = api.status().await.unwrap();
    assert!(
        !status.glc_to_sol_available,
        "pausing the destination reserve must mark that direction unavailable"
    );
    assert!(
        status.sol_to_glc_available,
        "the other direction's destination reserve is untouched"
    );
}

#[tokio::test]
async fn status_reports_a_direction_unavailable_when_destination_capacity_is_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            // balance == protected_minimum: zero available capacity, but
            // not paused. critical_reserve must still exceed
            // protected_minimum (docs/05-reserve-accounting.md).
            ledger
                .configure_reserve(direction, 1_000, 1_000, 5_000, 2_000, 1_001, 0)
                .unwrap();
        }
    }
    let api = build(&db_path, 0);
    let status = api.status().await.unwrap();
    assert!(!status.goldcoin_paused);
    assert!(!status.solana_paused);
    assert!(
        !status.glc_to_sol_available,
        "zero available capacity must mark the direction unavailable even though nothing is paused"
    );
    assert!(!status.sol_to_glc_available);
}

#[tokio::test]
async fn health_reports_healthy_when_nothing_is_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let health = api.health().await.unwrap();
    assert!(health.healthy);
    assert!(!health.goldcoin_indexer_halted);
    assert_eq!(health.manual_review_backlog, 0);
    assert_eq!(health.post_finality_reorg_events, 0);
}

#[tokio::test]
async fn health_reports_unhealthy_when_the_goldcoin_indexer_is_halted() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let indexer_status = Arc::new(crate::ops::indexer_status::IndexerStatus::new(0));
    indexer_status.record_halt(7);
    let api = BridgeApi::new(
        db_path,
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(0, 100, 1_000_000),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        3600,
        6,
        indexer_status,
    );
    let health = api.health().await.unwrap();
    assert!(!health.healthy);
    assert!(health.goldcoin_indexer_halted);
}

#[tokio::test]
async fn health_reports_unhealthy_after_a_post_finality_reorg_event() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .record_post_finality_reorg(5, 12, &[1, 2], 1_000)
            .unwrap();
    }
    let api = build(&db_path, 0);
    let health = api.health().await.unwrap();
    assert!(!health.healthy);
    assert_eq!(health.post_finality_reorg_events, 1);
    // Non-sensitive: the affected request ids and fork/tip heights are
    // never part of the public response, only the count.
}

#[tokio::test]
async fn reserve_reports_available_capacity_per_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let reserve = api.reserve().await.unwrap();
    // balance(10_000_000) - protected_minimum(0) - reserved(0)
    assert_eq!(reserve.goldcoin_available_capacity, 10_000_000);
    assert_eq!(reserve.solana_available_capacity, 10_000_000);
}

#[tokio::test]
async fn create_transfer_reserves_capacity_and_returns_deposit_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let recipient = Keypair::new().pubkey();
    let output = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: recipient.to_string(),
        })
        .await
        .unwrap();
    assert!(output.request_id > 0);
    assert_eq!(
        output.deposit_vault_address,
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX"
    );
    assert_eq!(output.deposit_binding_hex.len(), 64); // 32 bytes, hex-encoded

    let reserve = api.reserve().await.unwrap();
    // Capacity is reserved on the NET destination payout, in the
    // destination's own decimals (docs/20-bridge-fee.md): 500_000 gross -
    // 1% fee = 495_000 net canonical (8 decimals), /100 to the mint's
    // 6-decimal precision = 4_950.
    assert_eq!(reserve.solana_available_capacity, 10_000_000 - 4_950);
}

#[tokio::test]
async fn create_transfer_rejects_an_invalid_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: "not-a-valid-pubkey".to_string(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

#[tokio::test]
async fn create_transfer_rejects_a_zero_amount() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 0,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::BadRequest(_)));
}

#[tokio::test]
async fn create_transfer_reports_insufficient_liquidity_never_creates_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let err = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            // Even after the bridge fee and the 8->6 decimal shrink
            // (docs/20-bridge-fee.md), this remains far beyond the
            // configured 10_000_000 available capacity.
            amount_atomic: 2_000_000_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InsufficientLiquidity { .. }));
    // No capacity was touched: a fresh request must still see it all.
    assert_eq!(
        api.reserve().await.unwrap().solana_available_capacity,
        10_000_000
    );
}

#[tokio::test]
async fn create_transfer_fails_closed_on_a_paused_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
            .unwrap();
    }
    let api = build(&db_path, 0);

    let err = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Paused));
}

#[tokio::test]
async fn get_transfer_returns_none_for_an_unknown_id() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    assert!(api.get_transfer(999).await.unwrap().is_none());
}

#[tokio::test]
async fn get_transfer_reflects_a_just_created_request() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);

    let recipient = Keypair::new().pubkey();
    let created = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: recipient.to_string(),
        })
        .await
        .unwrap();

    let view = api.get_transfer(created.request_id).await.unwrap().unwrap();
    assert_eq!(view.id, created.request_id);
    assert_eq!(view.direction, "GlcToSol");
    assert_eq!(view.state, "AwaitingDeposit");
    assert_eq!(view.gross_amount_atomic, 500_000);
    assert_eq!(view.fee_bps, amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(view.fee_amount_atomic, 5_000);
    assert_eq!(view.net_amount_atomic, 495_000);
    assert!(view.source_txid.is_none());
    assert!(view.destination_txid.is_none());
    assert!(view.failure_reason.is_none());
    assert_eq!(
        view.required_source_confirmations,
        Some(6),
        "GlcToSol progress must be renderable against the configured confirmation depth"
    );
}

#[tokio::test]
async fn client_supplied_fee_fields_in_the_request_body_are_silently_ignored() {
    // `CreateTransferInput` has no fee/net field at all — there is nothing
    // for a client to submit that could bypass or alter the fee
    // (docs/20-bridge-fee.md: "never trust gross, fee or net calculations
    // supplied by the UI"). This proves it holds at the real HTTP/JSON
    // boundary too, not just at the Rust type level: a raw JSON body
    // smuggling `fee_bps`/`fee_amount_atomic`/`net_amount_atomic` fields
    // alongside the real ones is silently ignored by serde (no
    // `deny_unknown_fields`), and the server computes the real 1% fee
    // regardless of what the client tried to claim.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let (base, _tx) = spawn_real_server(&db_path, 0).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .json(&serde_json::json!({
            "amount_atomic": 500_000,
            "recipient": Keypair::new().pubkey().to_string(),
            // Attempted client-side fee bypass/manipulation:
            "fee_bps": 0,
            "fee_amount_atomic": 0,
            "net_amount_atomic": 500_000,
            "gross_amount_atomic": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let created: CreateTransferOutput = resp.json().await.unwrap();

    let view: TransferView = reqwest::get(format!("{base}/transfers/{}", created.request_id))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        view.gross_amount_atomic, 500_000,
        "gross must be exactly what the server itself received, never a client-claimed value"
    );
    assert_eq!(
        view.fee_bps,
        amount_conversion::BRIDGE_FEE_BPS,
        "fee_bps must always be the real protocol rate, never the client-submitted 0"
    );
    assert_eq!(
        view.fee_amount_atomic, 5_000,
        "the real 1% fee must be charged regardless of a client-submitted fee_amount_atomic of 0"
    );
    assert_eq!(
        view.net_amount_atomic, 495_000,
        "net must reflect the real fee, never the client-submitted (unreduced) net"
    );
}

async fn spawn_real_server(
    db_path: &std::path::Path,
    obligation_count: u64,
) -> (String, tokio::sync::watch::Sender<bool>) {
    let port = free_port().await;
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let api = Arc::new(build(db_path, obligation_count));
    tokio::spawn(async move {
        let _ = serve(addr, api, rx).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if reqwest::get(format!("{base}/status")).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, tx)
}

#[tokio::test]
async fn concurrent_post_transfers_never_oversubscribe_capacity() {
    // The same concurrency property `adversarial.rs`'s
    // `ten_concurrent_shaped_reservations_never_oversubscribe_capacity`
    // proves at the `Ledger` level, exercised here through the real HTTP
    // API — SQLite's own `BEGIN IMMEDIATE` transactions are what actually
    // make this safe (see `Ledger::create_request`), and this confirms
    // that guarantee survives being reached over the network with many
    // real concurrent connections rather than in-process calls.
    // A gross of 1_000_000 canonical costs 10_000 in fee (exact, no
    // rounding: 1_000_000 is a multiple of 10_000, see
    // `glc_to_sol_amounts`-style derivations elsewhere in this crate),
    // leaving 990_000 net canonical, which converts exactly to 9_900 at
    // the (6-decimal) reserve mint's precision (docs/20-bridge-fee.md).
    // Configure capacity to exactly 10 * 9_900 so the "exactly N succeed,
    // capacity fully and exactly consumed" property still holds under the
    // real fee math, not just the pre-fee 1:1 numbers.
    const NET_DESTINATION_PER_REQUEST: u64 = 9_900;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(
                    direction,
                    NET_DESTINATION_PER_REQUEST * 10,
                    0,
                    5_000_000,
                    2_000_000,
                    1_000_000,
                    0,
                )
                .unwrap();
        }
    }
    let (base, _tx) = spawn_real_server(&db_path, 0).await;

    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for _ in 0..20 {
        let client = client.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            client
                .post(format!("{base}/transfers"))
                .json(&CreateTransferInput {
                    amount_atomic: 1_000_000,
                    recipient: Keypair::new().pubkey().to_string(),
                })
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut created = 0;
    let mut rejected = 0;
    for h in handles {
        match h.await.unwrap() {
            reqwest::StatusCode::CREATED => created += 1,
            reqwest::StatusCode::CONFLICT => rejected += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(created, 10, "exactly capacity/amount requests must succeed");
    assert_eq!(
        rejected, 10,
        "the rest must be cleanly rejected, never oversubscribed"
    );

    let reserve: ReserveAvailability = reqwest::get(format!("{base}/reserve"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reserve.solana_available_capacity, 0,
        "capacity must be fully and exactly accounted for, no double-reservation and no leakage"
    );
}

/// A tiny, fully in-memory [`ApiSource`] for exercising `handle`'s routing
/// and status-code mapping without a real ledger/RPC.
struct StubSource;

impl ApiSource for StubSource {
    fn status(&self) -> BoxFut<'_, Result<BridgeStatus, ApiError>> {
        Box::pin(async {
            Ok(BridgeStatus {
                goldcoin_paused: false,
                solana_paused: false,
                vault_address: "V".into(),
                next_solana_obligation_index: 0,
                glc_to_sol_available: true,
                sol_to_glc_available: true,
            })
        })
    }
    fn limits(&self) -> BoxFut<'_, Result<TransferLimits, ApiError>> {
        Box::pin(async {
            Ok(TransferLimits {
                min_transfer_amount: 1,
                per_transfer_limit: 2,
                bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
            })
        })
    }
    fn health(&self) -> BoxFut<'_, Result<PublicHealth, ApiError>> {
        Box::pin(async {
            Ok(PublicHealth {
                healthy: true,
                goldcoin_indexer_halted: false,
                manual_review_backlog: 0,
                post_finality_reorg_events: 0,
            })
        })
    }
    fn reserve(&self) -> BoxFut<'_, Result<ReserveAvailability, ApiError>> {
        Box::pin(async {
            Ok(ReserveAvailability {
                goldcoin_available_capacity: 1,
                solana_available_capacity: 2,
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
            Ok(CreateTransferOutput {
                request_id: 7,
                deposit_vault_address: "V".into(),
                deposit_binding_hex: "00".repeat(32),
            })
        })
    }
    fn get_transfer(&self, id: i64) -> BoxFut<'_, Result<Option<TransferView>, ApiError>> {
        Box::pin(async move {
            if id == 7 {
                Ok(Some(TransferView {
                    id: 7,
                    direction: "GlcToSol".to_string(),
                    state: "AwaitingDeposit".to_string(),
                    gross_amount_atomic: 500_000,
                    fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                    fee_amount_atomic: 5_000,
                    net_amount_atomic: 495_000,
                    created_at: 0,
                    source_txid: None,
                    source_confirmations: 0,
                    required_source_confirmations: Some(6),
                    destination_txid: None,
                    failure_reason: None,
                }))
            } else {
                Ok(None)
            }
        })
    }
    fn quote(&self, input: QuoteInput) -> BoxFut<'_, Result<QuoteOutput, ApiError>> {
        Box::pin(async move {
            if input.gross_amount == 0 {
                return Err(ApiError::BadRequest("gross_amount must be > 0".into()));
            }
            Ok(QuoteOutput {
                direction: input.direction,
                gross_amount: input.gross_amount,
                gross_display_amount: "0.00500000".to_string(),
                fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                fee_amount: 5_000,
                fee_display_amount: "0.00005000".to_string(),
                net_amount: 495_000,
                net_display_amount: "0.00495000".to_string(),
                source_decimals: 8,
                destination_decimals: 6,
                source_asset: "GLC (Goldcoin)".to_string(),
                destination_asset: "GLC (Solana)".to_string(),
            })
        })
    }
}

// ------------------------------------------------------------- HTTP routing --
//
// Routing/status-code behavior is exercised against a real server on a
// real (ephemeral) localhost port — `hyper::body::Incoming` isn't
// user-constructible, so a raw-`Request` unit test isn't an option; this
// is the same "spawn the real thing, hit it over HTTP" approach
// tests/daemon_smoke.rs uses for the whole process, just in-process and
// fast here since only this one server needs to run.

async fn free_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn spawn_stub_server() -> (String, tokio::sync::watch::Sender<bool>) {
    let port = free_port().await;
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = serve(addr, Arc::new(StubSource), rx).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if reqwest::get(format!("{base}/status")).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, tx)
}

#[tokio::test]
async fn unknown_path_is_404() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_status_returns_200_and_json() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/status")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: BridgeStatus = resp.json().await.unwrap();
    assert!(!body.goldcoin_paused);
}

#[tokio::test]
async fn get_limits_and_reserve_return_200() {
    let (base, _tx) = spawn_stub_server().await;
    assert_eq!(
        reqwest::get(format!("{base}/limits"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        reqwest::get(format!("{base}/reserve"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn get_health_returns_200() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: PublicHealth = resp.json().await.unwrap();
    assert!(body.healthy);
}

#[tokio::test]
async fn post_transfers_with_malformed_body_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_transfers_with_a_business_rule_violation_maps_to_400() {
    let (base, _tx) = spawn_stub_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: 0,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_transfers_with_a_valid_body_is_201() {
    let (base, _tx) = spawn_stub_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/transfers"))
        .json(&CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let body: CreateTransferOutput = resp.json().await.unwrap();
    assert_eq!(body.request_id, 7);
}

#[tokio::test]
async fn get_transfers_by_id_round_trips() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers/7")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: TransferView = resp.json().await.unwrap();
    assert_eq!(body.id, 7);
}

#[tokio::test]
async fn get_transfers_by_unknown_id_is_404() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers/9999"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_transfers_with_a_non_numeric_id_is_400() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/transfers/not-a-number"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn shutdown_signal_stops_the_server() {
    let (base, tx) = spawn_stub_server().await;
    tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        reqwest::get(format!("{base}/status")).await.is_err(),
        "the server must stop accepting connections after shutdown"
    );
}
