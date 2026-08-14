use super::*;
use crate::ledger::CreateRequestOutcome;

fn temp_db_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    (dir, path)
}

fn collector_at(path: &std::path::Path) -> OpsCollector {
    OpsCollector::new(
        path.to_path_buf(),
        Arc::new(IndexerStatus::new(0)),
        Arc::new(IndexerStatus::new(0)),
    )
}

#[tokio::test]
async fn a_freshly_configured_ledger_reports_healthy() {
    let (_dir, path) = temp_db_path();
    {
        let mut ledger = Ledger::open(&path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
                .unwrap();
        }
    }
    let report = collector_at(&path).report().await;
    assert!(report.healthy(), "{}", report.text());
}

#[tokio::test]
async fn a_manual_review_backlog_is_counted_across_both_directions() {
    let (_dir, path) = temp_db_path();
    {
        let mut ledger = Ledger::open(&path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
                .unwrap();
        }
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(Direction::GlcToSol, 500_000, &[1u8; 32], None, 3600, 0)
            .unwrap()
        else {
            panic!()
        };
        // A wrong observed amount routes this request to ManualReview
        // rather than silently accepting or dropping it.
        ledger
            .record_glc_deposit_observed(request_id, [0xAAu8; 32], 0, 999_999, 10, [0u8; 32], 0)
            .unwrap();
    }
    let report = collector_at(&path).report().await;
    assert!(!report.healthy());
    assert!(report
        .text()
        .contains("BREACH no_manual_review_backlog: 1 request(s)"));
}

#[tokio::test]
async fn an_unopenable_database_reports_503_empty_rather_than_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let missing_dir_path = dir.path().join("no-such-subdir").join("ledger.sqlite3");
    let report = collector_at(&missing_dir_path).report().await;
    assert!(!report.healthy());
    assert!(report.invariants.is_empty());
}

#[tokio::test]
async fn a_halted_goldcoin_indexer_status_is_reflected_in_the_report() {
    let (_dir, path) = temp_db_path();
    {
        let mut ledger = Ledger::open(&path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
                .unwrap();
        }
    }
    let goldcoin_status = Arc::new(IndexerStatus::new(0));
    goldcoin_status.record_halt(42);
    let collector = OpsCollector::new(path, goldcoin_status, Arc::new(IndexerStatus::new(0)));
    let report = collector.report().await;
    assert!(!report.healthy());
    assert!(report.text().contains("attempted 42"));
}
