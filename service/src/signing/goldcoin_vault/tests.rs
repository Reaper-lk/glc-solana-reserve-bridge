use super::*;
use crate::goldcoin::multisig;
use crate::ledger::{CreateRequestOutcome, Direction, Ledger, ReserveDirection};

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md) — every test in this module treats `amount` as
/// Solana-native (matching `fold_sol_deposit`'s real semantics) and relies
/// on `rederive_plan`'s conversion to Goldcoin-native atomic units.
const TEST_SOLANA_DECIMALS: u8 = 6;

fn three_signers() -> (MultisigVault, [DevVaultSigner; 3]) {
    let signers = [
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap();
    (vault, signers)
}

fn ledger_with_finalized_sol_to_glc_request(
    vault: &MultisigVault,
    amount: u64,
    dest_addr: &str,
) -> (Ledger, i64) {
    let mut ledger = Ledger::open_in_memory().unwrap();
    // GoldcoinReserve capacity must cover the canonical (8-decimal) scale
    // of a Solana-native `amount` (6 decimals) once correctly converted
    // (docs/20-bridge-fee.md) — a 500_000 Solana-native deposit widens to
    // 50_000_000 canonical before the fee is even taken, well beyond the
    // pre-fee-round 10_000_000 fixture.
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            100_000_000,
            0,
            50_000_000,
            20_000_000,
            10_000_000,
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

    // Fund the vault with a UTXO comfortably covering the Goldcoin-native
    // equivalent of `amount` (Solana-native) + fee.
    let goldcoin_atomic =
        crate::amount_conversion::solana_to_goldcoin_atomic(amount, TEST_SOLANA_DECIMALS).unwrap();
    let utxo = VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic: goldcoin_atomic + 100_000,
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
        .unwrap();

    let SolFoldOutcomeReExport::FoldedFinalized { request_id } =
        fold(&mut ledger, amount, dest_addr)
    else {
        panic!()
    };
    (ledger, request_id)
}

// Thin re-export so this test module doesn't need a second `use` of the
// same enum under a different path.
use crate::ledger::SolFoldOutcome as SolFoldOutcomeReExport;

fn fold(ledger: &mut Ledger, amount: u64, dest_addr: &str) -> SolFoldOutcomeReExport {
    // Mirrors `solana::indexer::tick`'s real conversion (docs/20-bridge-
    // fee.md): `amount` is Solana-native gross; widen to canonical, take
    // the bridge fee, and the net (Goldcoin-native destination is already
    // canonical) is what actually reserves capacity.
    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let fb = crate::amount_conversion::compute_fee(gross_canonical).unwrap();
    let amounts = crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    };
    ledger
        .fold_sol_deposit(0, amounts, [7u8; 32], dest_addr.as_bytes(), 0)
        .unwrap()
}

#[test]
fn two_independent_signers_produce_an_assemblable_threshold() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (ledger, request_id) = ledger_with_finalized_sol_to_glc_request(&vault, 500_000, dest);
    let source = DevLedgerPayoutSource { ledger: &ledger };

    let (p0, plan0, tx0) = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_id,
        0,
        1000,
        1000,
        10,
        Network::Testnet,
    )
    .unwrap();
    let (p1, plan1, tx1) = independently_sign(
        &signers[1],
        &vault,
        &source,
        request_id,
        0,
        1000,
        1000,
        10,
        Network::Testnet,
    )
    .unwrap();

    // Both signers independently re-derived the IDENTICAL plan/transaction
    // — the whole point of independent re-derivation with deterministic
    // coin selection.
    assert_eq!(plan0, plan1);
    assert_eq!(tx0, tx1);

    let sighash = tx0.sighash_all(0, &vault.redeem_script());
    let script_sig = multisig::assemble(&vault, &sighash, &[p0, p1]).unwrap();
    assert_eq!(script_sig[0], 0x00);
}

#[test]
fn a_single_signer_alone_cannot_reach_threshold() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (ledger, request_id) = ledger_with_finalized_sol_to_glc_request(&vault, 500_000, dest);
    let source = DevLedgerPayoutSource { ledger: &ledger };

    let (p0, _, tx0) = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_id,
        0,
        1000,
        1000,
        10,
        Network::Testnet,
    )
    .unwrap();
    let sighash = tx0.sighash_all(0, &vault.redeem_script());
    let result = multisig::assemble(&vault, &sighash, &[p0]);
    assert!(
        result.is_err(),
        "no single signer may authorize a Goldcoin payout (docs/02-trust-model.md)"
    );
}

#[test]
fn refuses_to_sign_a_request_that_is_not_yet_source_finalized() {
    let mut ledger = Ledger::open_in_memory().unwrap();
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
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 100_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 100_000,
                net_destination_atomic: 100_000,
            },
            &[1u8; 32],
            None,
            3600,
            0,
        )
        .unwrap()
    else {
        panic!()
    };

    let (vault, signers) = three_signers();
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let result = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_id,
        0,
        1000,
        1000,
        10,
        Network::Testnet,
    );
    assert!(
        matches!(result, Err(SigningError::WrongDirection(_))),
        "a GlcToSol request must never be signed as a Goldcoin payout"
    );
}

#[test]
fn refuses_a_request_that_does_not_exist() {
    let ledger = Ledger::open_in_memory().unwrap();
    let (vault, signers) = three_signers();
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let result = independently_sign(
        &signers[0],
        &vault,
        &source,
        999,
        0,
        1000,
        1000,
        10,
        Network::Testnet,
    );
    assert!(matches!(result, Err(SigningError::RequestNotFound(999))));
}

#[test]
fn fails_closed_when_vault_has_insufficient_funds() {
    let (vault, signers) = three_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            100_000_000,
            0,
            50_000_000,
            20_000_000,
            10_000_000,
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
    // No vault UTXOs synced at all.
    let SolFoldOutcomeReExport::FoldedFinalized { request_id } =
        fold(&mut ledger, 500_000, "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef")
    else {
        panic!()
    };

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let result = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_id,
        0,
        1000,
        1000,
        10,
        Network::Testnet,
    );
    assert!(matches!(result, Err(SigningError::CoinSelection(_))));
}
