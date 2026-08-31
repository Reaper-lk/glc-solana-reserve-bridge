use super::*;
use crate::ledger::Ledger;

fn ledger_with_direction(paused: bool) -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    if paused {
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("test"))
            .unwrap();
    }
    ledger
}

#[test]
fn a_healthy_reserve_reports_the_invariant_holding_and_unpaused() {
    let ledger = ledger_with_direction(false);
    let snapshot = check(&ledger, ReserveDirection::GoldcoinReserve, 0).unwrap();
    assert_eq!(snapshot.direction, ReserveDirection::GoldcoinReserve);
    assert_eq!(snapshot.total_reserve_balance, 10_000_000);
    assert_eq!(snapshot.protected_minimum, 0);
    assert!(snapshot.invariant_holds);
    assert!(!snapshot.paused);
}

#[test]
fn a_paused_reserve_is_reported_as_paused() {
    let ledger = ledger_with_direction(true);
    let snapshot = check(&ledger, ReserveDirection::GoldcoinReserve, 0).unwrap();
    assert!(snapshot.paused);
    // Pause alone does not violate the balance invariant.
    assert!(snapshot.invariant_holds);
}

#[test]
fn an_unconfigured_reserve_errors_rather_than_reporting_a_fake_healthy_snapshot() {
    let ledger = Ledger::open_in_memory().unwrap();
    let result = check(&ledger, ReserveDirection::GoldcoinReserve, 0);
    assert!(result.is_err());
}

#[test]
fn immature_vault_utxo_total_is_reported_for_goldcoin_and_excluded_from_the_balance() {
    let mut ledger = ledger_with_direction(false);
    let immature = crate::goldcoin::coin::VaultUtxo {
        txid: [0xBBu8; 32],
        vout: 0,
        amount_atomic: 9_010_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(immature, 9, "51".to_string())], 20, 1_000)
        .unwrap();

    let snapshot = check(&ledger, ReserveDirection::GoldcoinReserve, 0).unwrap();
    assert_eq!(snapshot.immature_vault_utxo_total, 9_010_000);
    // The cached reserve balance is untouched by a mere sync — this proves
    // the figure is reported ALONGSIDE the balance, never folded into it.
    assert_eq!(snapshot.total_reserve_balance, 10_000_000);
}

#[test]
fn utxo_pool_health_is_reported_for_goldcoin_and_zeroed_for_solana() {
    let mut ledger = ledger_with_direction(false);
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            5_000_000,
            0,
            2_000_000,
            1_000_000,
            500_000,
            0,
        )
        .unwrap();
    let available = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAAu8; 32],
        vout: 0,
        amount_atomic: 3_000_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(available, 20, "51".to_string())], 20, 1_000)
        .unwrap();

    let goldcoin = check(&ledger, ReserveDirection::GoldcoinReserve, 0).unwrap();
    assert_eq!(goldcoin.utxo_pool.mature_available_atomic, 3_000_000);
    assert_eq!(goldcoin.utxo_pool.available_utxo_count, 1);
    assert!(
        !goldcoin.utxo_pool_warning,
        "warning_count defaults to 0 (disabled)"
    );

    let solana = check(&ledger, ReserveDirection::SolanaReserve, 0).unwrap();
    assert_eq!(solana.utxo_pool, crate::ledger::UtxoPoolHealth::default());
    assert!(!solana.utxo_pool_warning);
}

#[test]
fn utxo_pool_warning_engages_at_the_configured_threshold() {
    let mut ledger = ledger_with_direction(false);
    ledger
        .set_utxo_pool_thresholds(ReserveDirection::GoldcoinReserve, 0, 2)
        .unwrap();
    let available = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAAu8; 32],
        vout: 0,
        amount_atomic: 3_000_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(available, 20, "51".to_string())], 20, 1_000)
        .unwrap();

    let snapshot = check(&ledger, ReserveDirection::GoldcoinReserve, 0).unwrap();
    assert_eq!(snapshot.utxo_pool.available_utxo_count, 1);
    assert!(
        snapshot.utxo_pool_warning,
        "1 available UTXO is <= the configured warning_count of 2"
    );
}

#[test]
fn immature_vault_utxo_total_is_always_zero_for_solana() {
    let mut ledger = ledger_with_direction(false);
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            5_000_000,
            0,
            2_000_000,
            1_000_000,
            500_000,
            0,
        )
        .unwrap();
    let immature = crate::goldcoin::coin::VaultUtxo {
        txid: [0xBBu8; 32],
        vout: 0,
        amount_atomic: 9_010_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(immature, 9, "51".to_string())], 20, 1_000)
        .unwrap();

    let snapshot = check(&ledger, ReserveDirection::SolanaReserve, 0).unwrap();
    assert_eq!(
        snapshot.immature_vault_utxo_total, 0,
        "Solana has no UTXO-maturity concept — this must never leak Goldcoin's immature total"
    );
}
