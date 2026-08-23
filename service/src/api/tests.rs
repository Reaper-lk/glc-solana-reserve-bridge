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
    /// `(release/GlcToSol, deposit/SolToGlc)` `RollingVolumeWindow`
    /// account bytes — defaults to a fresh, unused (`window_total: 0`)
    /// window for each in [`build`], so existing tests that don't care
    /// about quota state see full remaining capacity, same as before this
    /// field existed.
    rolling_volume_windows: (Vec<u8>, Vec<u8>),
}

/// Mirrors `solana::accounts::tests::fake_rolling_volume_window_bytes`.
fn fake_rolling_volume_window_bytes(
    direction: u8,
    window_start: i64,
    window_total: u64,
) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(direction);
    v.extend_from_slice(&window_start.to_le_bytes());
    v.extend_from_slice(&window_total.to_le_bytes());
    v.push(4); // bump
    v.extend_from_slice(&[0u8; 16]); // reserved
    v
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
        if *pubkey == accounts::rolling_volume_window_pda(0) {
            return Ok(Some(Account {
                lamports: 1,
                data: self.rolling_volume_windows.0.clone(),
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        if *pubkey == accounts::rolling_volume_window_pda(1) {
            return Ok(Some(Account {
                lamports: 1,
                data: self.rolling_volume_windows.1.clone(),
                owner: accounts::PROGRAM_ID,
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
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
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
/// `rolling_volume_limit` deliberately far above every capacity/amount
/// figure any existing (non-quota-specific) test in this module uses, so
/// it never becomes the binding constraint by accident — quota
/// exhaustion is exercised only by tests that explicitly configure a
/// tight `rolling_volume_limit`/`rolling_volume_windows` fixture via
/// [`fake_bridge_config_bytes_with_rolling_limit`].
const TEST_DEFAULT_ROLLING_VOLUME_LIMIT: u64 = 1_000_000_000_000;

fn fake_bridge_config_bytes(
    obligation_count: u64,
    min_transfer: u64,
    per_transfer: u64,
) -> Vec<u8> {
    fake_bridge_config_bytes_with_rolling_limit(
        obligation_count,
        min_transfer,
        per_transfer,
        TEST_DEFAULT_ROLLING_VOLUME_LIMIT,
    )
}

fn fake_bridge_config_bytes_with_rolling_limit(
    obligation_count: u64,
    min_transfer: u64,
    per_transfer: u64,
    rolling_volume_limit: u64,
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
    v.extend_from_slice(&rolling_volume_limit.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
    v
}

fn build(db_path: &std::path::Path, obligation_count: u64) -> BridgeApi<FakeSolanaRpc> {
    BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes(obligation_count, 100, 1_000_000),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
    )
}

/// Like [`build`], but with an explicit `rolling_volume_limit` and each
/// direction's current `window_total` — for exercising quota-exhaustion
/// behavior deliberately, never by accident from an unrelated test's
/// capacity/amount figures.
fn build_with_rolling_volume(
    db_path: &std::path::Path,
    rolling_volume_limit: u64,
    release_window_total: u64,
    deposit_window_total: u64,
) -> BridgeApi<FakeSolanaRpc> {
    // `window_start` must be recent (close to real wall-clock `now_unix`),
    // never `0` — a `0` start would make every real bucket_age check
    // (`now - window_start`) enormous next to a 3_600s window, so
    // `rolling_volume_remaining` would always see it as an already-
    // expired/reset bucket and report full capacity regardless of
    // `window_total`, silently defeating the whole test.
    let window_start = now_unix() - 10;
    BridgeApi::new(
        db_path.to_path_buf(),
        FakeSolanaRpc {
            bridge_config: fake_bridge_config_bytes_with_rolling_limit(
                0,
                100,
                1_000_000,
                rolling_volume_limit,
            ),
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, window_start, release_window_total),
                fake_rolling_volume_window_bytes(1, window_start, deposit_window_total),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        3600,
        6,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
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

/// Items 1/3/4 of the quota-exhausted -> operator-pause -> refill ->
/// manual-unpause workflow report: quota exhaustion is a distinct,
/// independently-reported state from pause and from reserve-capacity
/// constraint, and it blocks ONLY the affected direction — the opposite
/// direction, whose own window is untouched, must remain fully reported
/// as available.
#[tokio::test]
async fn status_reports_quota_exhausted_independently_per_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    // release/GlcToSol window fully used against a 2_000_000 limit;
    // deposit/SolToGlc window untouched.
    let api = build_with_rolling_volume(&db_path, 2_000_000, 2_000_000, 0);
    let status = api.status().await.unwrap();

    assert!(!status.goldcoin_paused);
    assert!(!status.solana_paused);
    assert!(
        status.glc_to_sol_quota_exhausted,
        "GlcToSol's release window is fully used"
    );
    assert!(
        !status.sol_to_glc_quota_exhausted,
        "SolToGlc's own deposit window was never touched"
    );
    assert_eq!(status.glc_to_sol_rolling_volume_remaining, 0);
    assert_eq!(status.sol_to_glc_rolling_volume_remaining, 2_000_000);
    assert!(
        !status.glc_to_sol_available,
        "quota exhaustion alone (nothing paused, capacity otherwise fine) must still mark \
         the direction unavailable"
    );
    assert!(
        status.sol_to_glc_available,
        "the opposite direction, whose quota was never touched, must remain operational — \
         quota exhaustion blocks only the affected direction"
    );
}

/// Below the exhaustion threshold (`remaining >= min_transfer_amount`),
/// the direction must still report available — the check is "no legal
/// transfer fits", not "any volume has ever been used".
#[tokio::test]
async fn status_does_not_report_quota_exhausted_while_headroom_remains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build_with_rolling_volume(&db_path, 2_000_000, 1_000_000, 500_000);
    let status = api.status().await.unwrap();

    assert!(!status.glc_to_sol_quota_exhausted);
    assert!(!status.sol_to_glc_quota_exhausted);
    assert_eq!(status.glc_to_sol_rolling_volume_remaining, 1_000_000);
    assert_eq!(status.sol_to_glc_rolling_volume_remaining, 1_500_000);
    assert!(status.glc_to_sol_available);
    assert!(status.sol_to_glc_available);
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
            rolling_volume_windows: (
                fake_rolling_volume_window_bytes(0, 0, 0),
                fake_rolling_volume_window_bytes(1, 0, 0),
            ),
        },
        "REGTESTVAULTADDRESSXXXXXXXXXXXXX".to_string(),
        3600,
        6,
        indexer_status,
        Arc::new(crate::ops::indexer_status::IndexerStatus::new(0)),
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

// --------------------------------------------------------------- /stats --

#[tokio::test]
async fn stats_on_a_freshly_configured_ledger_reports_zero_counts_not_missing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let stats = api.stats().await.unwrap();
    assert!(!stats.goldcoin_paused);
    assert!(!stats.solana_paused);
    assert!(stats.glc_to_sol_available);
    assert!(stats.sol_to_glc_available);
    assert_eq!(stats.bridge_fee_bps, amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(stats.glc_to_sol.total_requests, 0);
    assert_eq!(stats.sol_to_glc.total_requests, 0);
    assert_eq!(stats.goldcoin_reserve.settled_volume_atomic, 0);
    assert_eq!(stats.solana_reserve.settled_volume_atomic, 0);
    assert!(!stats.goldcoin_indexer_halted);
    assert_eq!(stats.post_finality_reorg_events, 0);
}

#[tokio::test]
async fn stats_reflects_real_request_counts_by_direction_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    for _ in 0..3 {
        api.create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap();
    }
    let stats = api.stats().await.unwrap();
    assert_eq!(stats.glc_to_sol.total_requests, 3);
    assert_eq!(
        stats.glc_to_sol.in_progress_requests, 3,
        "a freshly created request is AwaitingDeposit, an active state"
    );
    assert_eq!(stats.glc_to_sol.settled_requests, 0);
    assert_eq!(stats.glc_to_sol.manual_review_requests, 0);
    assert_eq!(stats.sol_to_glc.total_requests, 0);
}

// ----------------------------------------------------- /reserves/history --

#[tokio::test]
async fn reserves_history_on_an_empty_ledger_returns_an_empty_page_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let page = api.reserves_history(None, None, 50).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn reserves_history_returns_real_reconciliation_ticks_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for (i, balance) in [10_000_000u64, 10_050_000, 10_100_000]
            .into_iter()
            .enumerate()
        {
            crate::reconciliation::reconcile(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                balance,
                1_000,
                1_000 + i as i64,
            )
            .unwrap();
        }
    }
    let api = build(&db_path, 0);
    let page = api.reserves_history(None, None, 50).await.unwrap();
    assert_eq!(page.items.len(), 3);
    // Newest first: the last reconcile() call (balance 10_100_000) leads.
    assert_eq!(page.items[0].observed_atomic, 10_100_000);
    assert_eq!(page.items[1].observed_atomic, 10_050_000);
    assert_eq!(page.items[2].observed_atomic, 10_000_000);
    assert!(
        page.items[0].id > page.items[1].id && page.items[1].id > page.items[2].id,
        "ids must be strictly descending"
    );
    assert!(page.next_cursor.is_none(), "fewer than `limit` rows exist");
    for item in &page.items {
        assert_eq!(item.direction, "SolanaReserve");
        assert_eq!(item.classification, "WITHIN_TOLERANCE");
        assert!(!item.auto_paused);
    }
}

#[tokio::test]
async fn reserves_history_filters_by_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        crate::reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::GoldcoinReserve,
            10_000_000,
            1_000,
            1_000,
        )
        .unwrap();
        crate::reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            10_000_000,
            1_000,
            1_001,
        )
        .unwrap();
    }
    let api = build(&db_path, 0);
    let page = api
        .reserves_history(Some(ReserveDirection::GoldcoinReserve), None, 50)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].direction, "GoldcoinReserve");
}

#[tokio::test]
async fn reserves_history_cursor_pagination_walks_the_full_history_without_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for i in 0..5u64 {
            crate::reconciliation::reconcile(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                10_000_000 + i * 1_000,
                1_000,
                1_000 + i as i64,
            )
            .unwrap();
        }
    }
    let api = build(&db_path, 0);
    let mut seen_ids = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = api.reserves_history(None, cursor, 2).await.unwrap();
        assert!(
            page.items.len() <= 2,
            "must never exceed the requested limit"
        );
        for item in &page.items {
            seen_ids.push(item.id);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c.parse().unwrap()),
            None => break,
        }
    }
    assert_eq!(seen_ids.len(), 5, "every row must be visited exactly once");
    let mut sorted = seen_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 5, "no id may repeat across pages");
    let mut descending = seen_ids.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        seen_ids, descending,
        "pages must compose into one strictly-descending sequence"
    );
}

#[tokio::test]
async fn reserves_history_limit_is_clamped_to_the_maximum() {
    // Clamping is an HTTP query-parsing concern (`parse_page_params`),
    // not something `ApiSource::reserves_history` itself re-enforces —
    // exercised here through the real HTTP server, the actual path a
    // client hits.
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for i in 0..(MAX_PAGE_LIMIT + 5) {
            crate::reconciliation::reconcile(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                10_000_000,
                1_000,
                1_000 + i as i64,
            )
            .unwrap();
        }
    }
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let page: Page<ReserveHistoryEntry> =
        reqwest::get(format!("{base}/reserves/history?limit=1000000"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(
        page.items.len() as u32,
        MAX_PAGE_LIMIT,
        "a limit far beyond the maximum must be clamped, not rejected or taken literally"
    );
}

#[tokio::test]
async fn reserves_history_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        crate::reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            10_000_000,
            1_000,
            1_000,
        )
        .unwrap();
    }
    // A fresh `BridgeApi` (and thus a fresh `Ledger::open` per call) is
    // exactly what a process restart looks like from this API's point of
    // view — there is no separate in-memory cache to lose.
    let api = build(&db_path, 0);
    let page = api.reserves_history(None, None, 50).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].observed_atomic, 10_000_000);
}

// ------------------------------------------------------- /explorer/events --

#[tokio::test]
async fn explorer_events_on_an_empty_ledger_returns_an_empty_page_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let page = api.explorer_events(None, None, None, 50).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn explorer_events_returns_real_state_transitions_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    // Each created transfer logs two real transitions: None->LiquidityReserved,
    // then LiquidityReserved->AwaitingDeposit (`Ledger::create_request`).
    let created = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap();

    let page = api.explorer_events(None, None, None, 50).await.unwrap();
    assert_eq!(page.items.len(), 2);
    // Newest first: AwaitingDeposit was logged after LiquidityReserved.
    assert_eq!(page.items[0].to_state, "AwaitingDeposit");
    assert_eq!(
        page.items[0].from_state.as_deref(),
        Some("LiquidityReserved")
    );
    assert_eq!(page.items[1].to_state, "LiquidityReserved");
    assert_eq!(page.items[1].from_state, None);
    for item in &page.items {
        assert_eq!(item.request_id, created.request_id);
        assert_eq!(item.direction, "GlcToSol");
    }
    assert!(page.items[0].id > page.items[1].id);
}

#[tokio::test]
async fn explorer_events_filters_by_direction_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    api.create_glc_to_sol_transfer(CreateTransferInput {
        amount_atomic: 500_000,
        recipient: Keypair::new().pubkey().to_string(),
    })
    .await
    .unwrap();

    let by_state = api
        .explorer_events(None, Some(RequestState::AwaitingDeposit), None, 50)
        .await
        .unwrap();
    assert_eq!(by_state.items.len(), 1);
    assert_eq!(by_state.items[0].to_state, "AwaitingDeposit");

    let by_direction = api
        .explorer_events(Some(Direction::SolToGlc), None, None, 50)
        .await
        .unwrap();
    assert!(
        by_direction.items.is_empty(),
        "no SolToGlc requests exist yet"
    );

    let no_match_state = api
        .explorer_events(None, Some(RequestState::Settled), None, 50)
        .await
        .unwrap();
    assert!(no_match_state.items.is_empty());
}

#[tokio::test]
async fn explorer_events_cursor_pagination_walks_without_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    for _ in 0..3 {
        api.create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap();
    }
    // 3 requests * 2 log rows each = 6 total rows.
    let mut seen_ids = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = api.explorer_events(None, None, cursor, 2).await.unwrap();
        assert!(page.items.len() <= 2);
        for item in &page.items {
            seen_ids.push(item.id);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c.parse().unwrap()),
            None => break,
        }
    }
    assert_eq!(seen_ids.len(), 6);
    let mut sorted = seen_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 6, "no id may repeat across pages");
}

#[tokio::test]
async fn explorer_events_limit_is_clamped_to_the_maximum() {
    // Same HTTP-boundary clamping property as
    // `reserves_history_limit_is_clamped_to_the_maximum`, exercised
    // through the real server rather than `ApiSource` directly.
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
                    1_000_000_000,
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
    // Each transfer logs 2 rows; comfortably exceed MAX_PAGE_LIMIT.
    for _ in 0..(MAX_PAGE_LIMIT / 2 + 5) {
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
    }
    let page: Page<ExplorerEvent> = reqwest::get(format!("{base}/explorer/events?limit=1000000"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len() as u32, MAX_PAGE_LIMIT);
}

#[tokio::test]
async fn explorer_events_never_exposes_recipient_or_operator_identity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    api.create_glc_to_sol_transfer(CreateTransferInput {
        amount_atomic: 500_000,
        recipient: Keypair::new().pubkey().to_string(),
    })
    .await
    .unwrap();
    let page = api.explorer_events(None, None, None, 50).await.unwrap();
    let raw = serde_json::to_string(&page).unwrap();
    assert!(!raw.contains("recipient"));
    assert!(!raw.contains("requester"));
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
    assert_eq!(
        err.to_string(),
        DIRECTION_UNAVAILABLE_MESSAGE,
        "the raw available-capacity number must never reach the end user — same generic \
         copy as every other direction-unavailable cause"
    );
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
    assert_eq!(err.to_string(), DIRECTION_UNAVAILABLE_MESSAGE);
}

/// Item 6 of the quota-exhausted -> operator-pause -> refill -> manual-
/// unpause workflow report: `GlcToSol`'s rolling-24h-volume quota being
/// exhausted must reject a new transfer proactively — with the exact
/// approved user-facing copy, no reference to any midnight reset or
/// automatic reopening — and must never touch off-chain reserved
/// capacity, exactly like the insufficient-liquidity and paused cases
/// above.
#[tokio::test]
async fn create_transfer_reports_quota_exhausted_with_the_exact_message_never_creates_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    // release/GlcToSol window already at its 2_000_000 limit; deposit/
    // SolToGlc window untouched — only the affected direction should be
    // rejected (asserted separately below via `/status`).
    let api = build_with_rolling_volume(&db_path, 2_000_000, 2_000_000, 0);

    let err = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::QuotaExhausted));
    assert_eq!(
        err.to_string(),
        "Bridge capacity reached for this direction.\nTransfers are temporarily paused while reserves are replenished.\nPlease check the official Telegram for reopening updates."
    );
    assert_eq!(err.to_string(), DIRECTION_UNAVAILABLE_MESSAGE);
    assert!(
        !err.to_string().to_lowercase().contains("midnight"),
        "must never claim an automatic midnight reset"
    );
    assert!(
        !err.to_string().to_lowercase().contains("automatic"),
        "must never claim automatic reopening"
    );
    // No off-chain capacity was touched: a fresh request must still see
    // it all, exactly as the insufficient-liquidity/paused cases do.
    assert_eq!(
        api.reserve().await.unwrap().solana_available_capacity,
        10_000_000
    );
}

/// A transfer that fits within remaining quota must still succeed — the
/// proactive check must reject only when it would genuinely be rejected
/// on-chain, never more conservatively than that.
#[tokio::test]
async fn create_transfer_succeeds_when_amount_fits_within_remaining_quota() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build_with_rolling_volume(&db_path, 2_000_000, 1_000_000, 0);

    let out = api
        .create_glc_to_sol_transfer(CreateTransferInput {
            amount_atomic: 500_000,
            recipient: Keypair::new().pubkey().to_string(),
        })
        .await
        .unwrap();
    assert_eq!(out.request_id, 1);
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

// -------------------------------------------------------------- /transfers (list) --

#[tokio::test]
async fn list_transfers_on_an_empty_ledger_returns_an_empty_page_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let page = api.list_transfers(None, None, None, 50).await.unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn list_transfers_filters_by_address_matching_either_recipient_or_requester() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let mine = Keypair::new().pubkey();
    let someone_else = Keypair::new().pubkey();

    api.create_glc_to_sol_transfer(CreateTransferInput {
        amount_atomic: 500_000,
        recipient: mine.to_string(),
    })
    .await
    .unwrap();
    api.create_glc_to_sol_transfer(CreateTransferInput {
        amount_atomic: 500_000,
        recipient: someone_else.to_string(),
    })
    .await
    .unwrap();

    let page = api
        .list_transfers(Some(mine.to_bytes()), None, None, 50)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].direction, "GlcToSol");
}

#[tokio::test]
async fn list_transfers_filters_by_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    api.create_glc_to_sol_transfer(CreateTransferInput {
        amount_atomic: 500_000,
        recipient: Keypair::new().pubkey().to_string(),
    })
    .await
    .unwrap();

    let matching = api
        .list_transfers(None, Some(RequestState::AwaitingDeposit), None, 50)
        .await
        .unwrap();
    assert_eq!(matching.items.len(), 1);

    let non_matching = api
        .list_transfers(None, Some(RequestState::Settled), None, 50)
        .await
        .unwrap();
    assert!(non_matching.items.is_empty());
}

#[tokio::test]
async fn list_transfers_newest_first_and_cursor_pagination_has_no_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let api = build(&db_path, 0);
    let mut created_ids = Vec::new();
    for _ in 0..5 {
        let created = api
            .create_glc_to_sol_transfer(CreateTransferInput {
                amount_atomic: 500_000,
                recipient: Keypair::new().pubkey().to_string(),
            })
            .await
            .unwrap();
        created_ids.push(created.request_id);
    }

    let mut seen_ids = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = api.list_transfers(None, None, cursor, 2).await.unwrap();
        assert!(page.items.len() <= 2);
        for item in &page.items {
            seen_ids.push(item.id);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c.parse().unwrap()),
            None => break,
        }
    }
    let mut expected = created_ids.clone();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        seen_ids, expected,
        "must visit every created transfer exactly once, newest first"
    );
}

#[tokio::test]
async fn get_transfers_list_route_returns_200_and_rejects_an_invalid_address() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = configure(dir.path());
    let (base, _tx) = spawn_real_server(&db_path, 0).await;
    let resp = reqwest::get(format!("{base}/transfers")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let resp = reqwest::get(format!("{base}/transfers?address=not-a-pubkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
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
                glc_to_sol_quota_exhausted: false,
                sol_to_glc_quota_exhausted: false,
                glc_to_sol_rolling_volume_remaining: 100_000_000,
                sol_to_glc_rolling_volume_remaining: 100_000_000,
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
    fn list_transfers(
        &self,
        _address: Option<[u8; 32]>,
        _state: Option<RequestState>,
        _cursor: Option<i64>,
        _limit: u32,
    ) -> BoxFut<'_, Result<Page<TransferView>, ApiError>> {
        Box::pin(async {
            Ok(Page {
                items: vec![],
                next_cursor: None,
                as_of: 0,
            })
        })
    }
    fn stats(&self) -> BoxFut<'_, Result<BridgeStats, ApiError>> {
        Box::pin(async {
            Ok(BridgeStats {
                goldcoin_paused: false,
                solana_paused: false,
                glc_to_sol_available: true,
                sol_to_glc_available: true,
                glc_to_sol_quota_exhausted: false,
                sol_to_glc_quota_exhausted: false,
                glc_to_sol_rolling_volume_remaining: 100_000_000,
                sol_to_glc_rolling_volume_remaining: 100_000_000,
                bridge_fee_bps: amount_conversion::BRIDGE_FEE_BPS,
                glc_to_sol: DirectionStats {
                    total_requests: 1,
                    in_progress_requests: 0,
                    settled_requests: 1,
                    manual_review_requests: 0,
                },
                sol_to_glc: DirectionStats {
                    total_requests: 0,
                    in_progress_requests: 0,
                    settled_requests: 0,
                    manual_review_requests: 0,
                },
                goldcoin_reserve: ReserveStats {
                    paused: false,
                    available_capacity: 1,
                    settled_volume_atomic: 0,
                    accrued_fees_atomic: 0,
                },
                solana_reserve: ReserveStats {
                    paused: false,
                    available_capacity: 2,
                    settled_volume_atomic: 495_000,
                    accrued_fees_atomic: 5_000,
                },
                goldcoin_indexer_halted: false,
                goldcoin_indexer_seconds_since_tick: 0,
                solana_indexer_seconds_since_tick: 0,
                post_finality_reorg_events: 0,
                as_of: 0,
            })
        })
    }
    fn reserves_history(
        &self,
        _direction: Option<ReserveDirection>,
        _cursor: Option<i64>,
        _limit: u32,
    ) -> BoxFut<'_, Result<Page<ReserveHistoryEntry>, ApiError>> {
        Box::pin(async {
            Ok(Page {
                items: vec![],
                next_cursor: None,
                as_of: 0,
            })
        })
    }
    fn explorer_events(
        &self,
        _direction: Option<Direction>,
        _state: Option<RequestState>,
        _cursor: Option<i64>,
        _limit: u32,
    ) -> BoxFut<'_, Result<Page<ExplorerEvent>, ApiError>> {
        Box::pin(async {
            Ok(Page {
                items: vec![],
                next_cursor: None,
                as_of: 0,
            })
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

// -------------------------------------------------------- pagination/validation --

#[tokio::test]
async fn get_stats_returns_200_and_json() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/stats")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: BridgeStats = resp.json().await.unwrap();
    assert_eq!(body.bridge_fee_bps, amount_conversion::BRIDGE_FEE_BPS);
}

#[tokio::test]
async fn stats_json_schema_has_the_documented_top_level_fields() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/stats")).await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    for field in [
        "goldcoin_paused",
        "solana_paused",
        "glc_to_sol_available",
        "sol_to_glc_available",
        "glc_to_sol_quota_exhausted",
        "sol_to_glc_quota_exhausted",
        "glc_to_sol_rolling_volume_remaining",
        "sol_to_glc_rolling_volume_remaining",
        "bridge_fee_bps",
        "glc_to_sol",
        "sol_to_glc",
        "goldcoin_reserve",
        "solana_reserve",
        "goldcoin_indexer_halted",
        "goldcoin_indexer_seconds_since_tick",
        "solana_indexer_seconds_since_tick",
        "post_finality_reorg_events",
        "as_of",
    ] {
        assert!(
            body.get(field).is_some(),
            "GET /stats must always carry a stable {field:?} field"
        );
    }
}

#[tokio::test]
async fn get_reserves_history_and_explorer_events_return_200() {
    let (base, _tx) = spawn_stub_server().await;
    assert_eq!(
        reqwest::get(format!("{base}/reserves/history"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        reqwest::get(format!("{base}/explorer/events"))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn reserves_history_rejects_a_non_numeric_cursor() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?cursor=not-a-number"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_rejects_a_zero_limit() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?limit=0"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_rejects_a_non_numeric_limit() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?limit=abc"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_rejects_an_unknown_direction() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?direction=bogus"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reserves_history_accepts_valid_direction_values() {
    let (base, _tx) = spawn_stub_server().await;
    for direction in ["goldcoin", "solana"] {
        let resp = reqwest::get(format!("{base}/reserves/history?direction={direction}"))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
}

#[tokio::test]
async fn explorer_events_rejects_a_non_numeric_cursor() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?cursor=not-a-number"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_rejects_a_zero_limit() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?limit=0"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_rejects_an_unknown_direction() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?direction=bogus"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_rejects_an_unknown_state() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/explorer/events?state=NotARealState"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn explorer_events_accepts_valid_direction_and_state_values() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!(
        "{base}/explorer/events?direction=GlcToSol&state=AwaitingDeposit"
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn pagination_empty_query_string_values_fall_back_to_defaults() {
    let (base, _tx) = spawn_stub_server().await;
    let resp = reqwest::get(format!("{base}/reserves/history?cursor=&limit=&direction="))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}
