use super::*;

fn configured_ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
            .unwrap();
    }
    ledger
}

#[test]
fn no_alert_when_nothing_is_paused() {
    let ledger = configured_ledger();
    let mut previous = HashMap::new();
    let newly_paused = detect_new_pauses(&ledger, &mut previous).unwrap();
    assert!(newly_paused.is_empty());
}

#[test]
fn fires_exactly_once_on_the_false_to_true_transition() {
    let mut ledger = configured_ledger();
    let mut previous = HashMap::new();

    // Before the pause: no alert.
    assert!(detect_new_pauses(&ledger, &mut previous)
        .unwrap()
        .is_empty());

    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
        .unwrap();

    // Right after: exactly one alert, for the direction that changed.
    let newly_paused = detect_new_pauses(&ledger, &mut previous).unwrap();
    assert_eq!(newly_paused, vec![ReserveDirection::SolanaReserve]);

    // Still paused on the next poll: no repeated alert (edge-triggered,
    // not level-triggered — see module docs).
    let newly_paused = detect_new_pauses(&ledger, &mut previous).unwrap();
    assert!(newly_paused.is_empty());
}

#[test]
fn both_directions_are_tracked_independently() {
    let mut ledger = configured_ledger();
    let mut previous = HashMap::new();
    detect_new_pauses(&ledger, &mut previous).unwrap();

    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("test"))
        .unwrap();
    let newly_paused = detect_new_pauses(&ledger, &mut previous).unwrap();
    assert_eq!(newly_paused, vec![ReserveDirection::GoldcoinReserve]);

    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
        .unwrap();
    let newly_paused = detect_new_pauses(&ledger, &mut previous).unwrap();
    assert_eq!(newly_paused, vec![ReserveDirection::SolanaReserve]);

    // Both now paused; a further poll alerts on neither.
    assert!(detect_new_pauses(&ledger, &mut previous)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn run_stops_on_shutdown_without_a_reachable_webhook() {
    // No real HTTP server needed: the webhook URL is deliberately
    // unreachable, exercising the "delivery failed, log and keep going"
    // path (send_alert) without this test depending on network access.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                10_000_000,
                0,
                5_000_000,
                2_000_000,
                1_000_000,
                0,
            )
            .unwrap();
        ledger
            .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
            .unwrap();
    }

    let (tx, rx) = tokio::sync::watch::channel(false);
    let config = AlertConfig {
        webhook_url: "http://127.0.0.1:1/unreachable".to_string(),
        poll_interval: Duration::from_millis(10),
    };
    let handle = tokio::spawn(run(db_path, config, rx));
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run must stop promptly after a shutdown signal")
        .unwrap();
}
