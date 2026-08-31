//! Regression tests for the `glc-admin open-admission` UTXO-liquidity
//! fix (the follow-up flagged, but not fixed, alongside PR #35's
//! maintainer-review finding 2).
//!
//! `open-admission` used to refuse only on the value-based hard reserve
//! invariant (`Ledger::check_invariant`) before reopening admission for
//! new Solana->Goldcoin obligations — never the count-based
//! `utxo_pool_min_available_count` gate `fold_sol_deposit` applies to a
//! brand-new obligation. Reopening admission the moment value accounting
//! looked sufficient, while the mature UTXO count was still at or below
//! the floor, could immediately re-admit exactly the demand backpressure
//! exists to hold back.
//!
//! Fixed: `Ledger::check_utxo_liquidity_for_admission` — the same
//! count-based check, additive to (never a replacement for)
//! `check_invariant` — is now called by `glc-admin open-admission`
//! (`service/src/bin/glc-admin.rs::cmd_admission`) before reopening
//! admission. It is `&self` (read-only) by construction: it cannot
//! mutate anything, so "no state changes on refusal" is true by
//! definition, not merely by convention — Test B verifies this directly
//! anyway, since a real regression could still occur if a future edit
//! moved a mutation earlier than the check.
//!
//! Tests A-D correspond exactly to the four regression requirements from
//! the fix request:
//! - A: `open-admission` (i.e. `check_utxo_liquidity_for_admission`)
//!   refuses at floor=10.
//! - B: no state changes occur on refusal.
//! - C: after internal change matures and count rises above 10,
//!   `open-admission` succeeds.
//! - D: missing config uses the safe default 10 — proven end to end here
//!   by exercising the check at that exact value; the config-loading half
//!   (a TOML file omitting `utxo_pool_min_available_count` resolves to
//!   `10`) is proven directly by
//!   `service/src/config/tests.rs::missing_utxo_liquidity_config_defaults_to_the_verified_safe_floor`.

use std::collections::BTreeMap;

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, LedgerError, ReserveDirection, SolFoldOutcome};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    DevLedgerPayoutSource, DevVaultSigner, IndependentPayoutSource,
};

const DEST_ADDR: &str = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
const GLC: u64 = 100_000_000;
const MIN_CONFIRMATIONS: i64 = 6;
/// The shipped, config-level default (`default_utxo_pool_min_available_count`
/// in `service/src/config.rs`), proven to load from a config file missing
/// the field by
/// `service/src/config/tests.rs::missing_utxo_liquidity_config_defaults_to_the_verified_safe_floor`.
const FLOOR: u32 = 10;

fn test_vault() -> MultisigVault {
    let signers = [
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap()
}

fn policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * GLC,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth: 0,
    }
}

fn amounts_for_gross_glc(gross_glc: u64) -> glc_reserve_bridge_service::ledger::RequestAmounts {
    let fb = compute_fee(CanonicalAtomic(gross_glc * GLC)).unwrap();
    glc_reserve_bridge_service::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

fn configure_reserve(ledger: &mut Ledger, total_balance_glc: u64, min_available_count: u32) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            total_balance_glc * GLC,
            20_000 * GLC,
            100_000 * GLC,
            50_000 * GLC,
            30_000 * GLC,
            0,
        )
        .unwrap();
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
        .set_utxo_pool_thresholds(
            ReserveDirection::GoldcoinReserve,
            min_available_count,
            min_available_count + 5,
        )
        .unwrap();
}

/// See `utxo_liquidity_incident.rs`'s module docs for why a full running
/// snapshot (never an incremental update) must be passed to
/// `Ledger::sync_vault_utxos` every time.
struct ChainView {
    entries: BTreeMap<([u8; 32], u32), (VaultUtxo, i64)>,
}

impl ChainView {
    fn new() -> Self {
        ChainView {
            entries: BTreeMap::new(),
        }
    }

    fn observe(&mut self, utxo: VaultUtxo, confirmations: i64) {
        self.entries
            .insert((utxo.txid, utxo.vout), (utxo, confirmations));
    }

    fn bump_confirmations(&mut self, txid: [u8; 32], confirmations: i64) {
        for ((t, _), (_, conf)) in self.entries.iter_mut() {
            if *t == txid {
                *conf = confirmations;
            }
        }
    }

    fn sync(&self, ledger: &mut Ledger, vault: &MultisigVault, now: i64) {
        let observed: Vec<_> = self
            .entries
            .values()
            .map(|(utxo, conf)| (utxo.clone(), *conf, vault.script_pubkey_hex()))
            .collect();
        ledger
            .sync_vault_utxos(&observed, MIN_CONFIRMATIONS, now)
            .unwrap();
    }
}

fn seed_mature_utxos(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    count: u8,
    amount_glc: u64,
) -> u64 {
    for i in 0..count {
        let mut txid = [0u8; 32];
        txid[0] = 0xE0;
        txid[1] = i;
        view.observe(
            VaultUtxo {
                txid,
                vout: 0,
                amount_atomic: amount_glc * GLC,
                script_pubkey_hex: vault.script_pubkey_hex(),
            },
            20,
        );
    }
    view.sync(ledger, vault, 0);
    count as u64 * amount_glc
}

fn txid_for(obligation_index: u64) -> [u8; 32] {
    let mut txid = [0u8; 32];
    txid[0] = 0xF0;
    txid[24..32].copy_from_slice(&obligation_index.to_be_bytes());
    txid
}

fn admit_and_broadcast_one(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    obligation_index: u64,
    gross_glc: u64,
    now: i64,
) -> SolFoldOutcome {
    let outcome = ledger
        .fold_sol_deposit(
            obligation_index,
            amounts_for_gross_glc(gross_glc),
            [7u8; 32],
            DEST_ADDR.as_bytes(),
            now,
        )
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        return outcome;
    };

    let source = DevLedgerPayoutSource { ledger };
    let plan = source
        .rederive_plan(request_id, vault, &policy(), Network::Testnet)
        .unwrap();
    ledger
        .reserve_vault_utxos(request_id, &plan.inputs, 0, now)
        .unwrap();
    ledger
        .record_goldcoin_payout_built(request_id, &plan, [0x77u8; 32], "00", now)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, "00", now)
        .unwrap();
    let txid = txid_for(obligation_index);
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, now)
        .unwrap();

    for input in &plan.inputs {
        view.entries.remove(&(input.txid, input.vout));
    }
    for (i, &amount_atomic) in plan.change_outputs.iter().enumerate() {
        view.observe(
            VaultUtxo {
                txid,
                vout: (i + 1) as u32,
                amount_atomic,
                script_pubkey_hex: vault.script_pubkey_hex(),
            },
            1, // immature: below MIN_CONFIRMATIONS
        );
    }
    view.sync(ledger, vault, now);
    outcome
}

/// Shared setup: seed `FLOOR + 1` mature UTXOs, admit one obligation so the
/// pool sits exactly at the floor, and close admission first (so "opening"
/// it back up is a meaningful, real transition, not a no-op).
fn setup_at_the_floor_with_admission_closed() -> (MultisigVault, Ledger, ChainView) {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, FLOOR as u8 + 1, 4_770);
    configure_reserve(&mut ledger, total, FLOOR);

    let outcome = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 2_000, 100);
    assert!(matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }));
    assert_eq!(
        ledger.available_vault_utxos().unwrap().len(),
        FLOOR as usize,
        "one admission must bring the pool exactly to the floor"
    );

    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("maintenance"))
        .unwrap();
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    (vault, ledger, view)
}

/// Test A: `check_utxo_liquidity_for_admission` (and therefore
/// `open-admission`) refuses at floor=10.
#[tokio::test]
async fn test_a_open_admission_refuses_at_the_floor() {
    let (_vault, ledger, _view) = setup_at_the_floor_with_admission_closed();

    // The hard invariant holds fine on its own — proving this refusal is
    // a SEPARATE, additive check, never a weakening of the existing one.
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .expect("the hard invariant must independently hold in this scenario");

    let err = ledger
        .check_utxo_liquidity_for_admission(ReserveDirection::GoldcoinReserve, 0)
        .unwrap_err();
    match err {
        LedgerError::UtxoLiquidityLowForAdmission {
            direction,
            available_utxo_count,
            min_available_count,
            own_unconfirmed_change_atomic,
        } => {
            assert_eq!(direction, ReserveDirection::GoldcoinReserve);
            assert_eq!(available_utxo_count, FLOOR as i64);
            assert_eq!(min_available_count, FLOOR as i64);
            assert!(
                own_unconfirmed_change_atomic > 0,
                "the error must name the known internal change, not just the count"
            );
        }
        other => panic!("expected UtxoLiquidityLowForAdmission, got {other:?}"),
    }
}

/// Test B: no state changes occur on refusal. `check_utxo_liquidity_for_admission`
/// takes `&self` — structurally incapable of mutating anything — verified
/// directly anyway against every piece of state a real `open-admission`
/// call could plausibly have touched.
#[tokio::test]
async fn test_b_no_state_changes_occur_on_refusal() {
    let (_vault, ledger, _view) = setup_at_the_floor_with_admission_closed();

    let before_snapshot = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let before_admission_closed = ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let before_paused = ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap();
    let before_available = ledger.available_vault_utxos().unwrap().len();

    // Call it several times, exactly like a retried CLI invocation.
    for _ in 0..3 {
        let _ = ledger.check_utxo_liquidity_for_admission(ReserveDirection::GoldcoinReserve, 0);
    }

    assert_eq!(
        ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        before_snapshot
    );
    assert_eq!(
        ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        before_admission_closed,
        "admission must remain closed — a refused check must never open it"
    );
    assert_eq!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        before_paused
    );
    assert_eq!(
        ledger.available_vault_utxos().unwrap().len(),
        before_available
    );
}

/// Test C: after the triggering payout's internal change matures to 6
/// confirmations and the count rises above the floor, `open-admission`
/// succeeds — checked, then actually performed via `set_admission`,
/// exactly mirroring `cmd_admission`'s real sequence.
#[tokio::test]
async fn test_c_open_admission_succeeds_after_recovery() {
    let (vault, mut ledger, mut view) = setup_at_the_floor_with_admission_closed();

    // Still refused before maturity.
    assert!(matches!(
        ledger.check_utxo_liquidity_for_admission(ReserveDirection::GoldcoinReserve, 0),
        Err(LedgerError::UtxoLiquidityLowForAdmission { .. })
    ));

    let txid0 = txid_for(0);
    view.bump_confirmations(txid0, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 300);

    let available_after_maturity = ledger.available_vault_utxos().unwrap().len();
    assert!(
        available_after_maturity > FLOOR as usize,
        "matured change must count as available again: {available_after_maturity}"
    );

    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
    ledger
        .check_utxo_liquidity_for_admission(ReserveDirection::GoldcoinReserve, 0)
        .expect("must succeed once the pool has recovered past the floor");

    // Perform the actual reopen, exactly like `cmd_admission` does once
    // both checks pass.
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("recovered"))
        .unwrap();
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// Test D: missing config uses the safe default 10 — the admission-check
/// half. `service/src/config/tests.rs::
/// missing_utxo_liquidity_config_defaults_to_the_verified_safe_floor`
/// proves a config file omitting `utxo_pool_min_available_count` loads
/// `10`; this proves that exact value, threaded through
/// `Ledger::set_utxo_pool_thresholds` exactly as `glc-bridge-daemon` does
/// at startup, makes `check_utxo_liquidity_for_admission` refuse at
/// precisely the floor and succeed one UTXO above it — never silently
/// disabled, never off by one.
#[tokio::test]
async fn test_d_missing_config_uses_the_safe_default_of_10() {
    // Exactly at the default floor (10 available, 10 configured): refused.
    let vault = test_vault();
    let mut ledger_at_floor = Ledger::open_in_memory().unwrap();
    let mut view_at_floor = ChainView::new();
    let total = seed_mature_utxos(
        &mut ledger_at_floor,
        &mut view_at_floor,
        &vault,
        FLOOR as u8,
        4_770,
    );
    configure_reserve(&mut ledger_at_floor, total, FLOOR);
    assert_eq!(
        ledger_at_floor.available_vault_utxos().unwrap().len(),
        FLOOR as usize
    );
    assert!(
        matches!(
            ledger_at_floor
                .check_utxo_liquidity_for_admission(ReserveDirection::GoldcoinReserve, 0),
            Err(LedgerError::UtxoLiquidityLowForAdmission { .. })
        ),
        "exactly at the default floor (10 available, 10 configured) must refuse"
    );

    // One more than the default floor (11 available, 10 configured): succeeds.
    let mut ledger_above_floor = Ledger::open_in_memory().unwrap();
    let mut view_above_floor = ChainView::new();
    let total_above = seed_mature_utxos(
        &mut ledger_above_floor,
        &mut view_above_floor,
        &vault,
        FLOOR as u8 + 1,
        4_770,
    );
    configure_reserve(&mut ledger_above_floor, total_above, FLOOR);
    assert_eq!(
        ledger_above_floor.available_vault_utxos().unwrap().len(),
        FLOOR as usize + 1
    );
    ledger_above_floor
        .check_utxo_liquidity_for_admission(ReserveDirection::GoldcoinReserve, 0)
        .expect("one more than the default floor must succeed");
}
