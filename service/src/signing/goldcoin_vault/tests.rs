use super::*;
use crate::goldcoin::multisig;
use crate::ledger::{CreateRequestOutcome, Direction, Ledger, ReserveDirection};
use crate::signing::signers::SignerError;

/// A test double proving `independently_sign` depends only on the
/// `VaultSigner` trait, not `DevVaultSigner` concretely — and that it
/// fails closed on both an explicit signer error and a signer that never
/// responds (docs/22-production-readiness-review.md P0 "signer
/// abstraction").
struct FailingVaultSigner {
    pubkey: [u8; 33],
    behavior: FailingSignerBehavior,
}

#[derive(Clone, Copy)]
enum FailingSignerBehavior {
    Reject,
    Hang,
}

impl VaultSigner for FailingVaultSigner {
    fn public_key(&self) -> [u8; 33] {
        self.pubkey
    }

    fn sign_sighash<'a>(
        &'a self,
        _sighash: &'a [u8; 32],
    ) -> crate::signing::signers::BoxFut<'a, Result<Vec<u8>, SignerError>> {
        match self.behavior {
            FailingSignerBehavior::Reject => Box::pin(async move {
                Err(SignerError::Rejected {
                    identity: crate::goldcoin::hex::encode(&self.pubkey),
                    detail: "test double: policy refused".to_string(),
                })
            }),
            FailingSignerBehavior::Hang => Box::pin(async move {
                std::future::pending::<()>().await;
                unreachable!("must never resolve — timeout should fire first")
            }),
        }
    }
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md) — every test in this module treats `amount` as
/// Solana-native (matching `fold_sol_deposit`'s real semantics) and relies
/// on `rederive_plan`'s conversion to Goldcoin-native atomic units.
const TEST_SOLANA_DECIMALS: u8 = 6;
const TEST_SIGNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A production-scale change-fanout target relative to this module's
/// test-scale amounts means every test below naturally gets exactly one
/// change output — identical to the pre-fan-out `finalize` behavior these
/// tests were originally written against — unless a test deliberately
/// sizes its own UTXOs to exercise fan-out (see `goldcoin_payout_lifecycle
/// .rs`'s dedicated fan-out tests instead).
fn test_policy() -> crate::goldcoin::payout::PayoutPolicy {
    crate::goldcoin::payout::PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
    }
}

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
        script_pubkey_hex: vault.script_pubkey_hex(),
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

#[tokio::test]
async fn two_independent_signers_produce_an_assemblable_threshold() {
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
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let (p1, plan1, tx1) = independently_sign(
        &signers[1],
        &vault,
        &source,
        request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
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

#[tokio::test]
async fn a_single_signer_alone_cannot_reach_threshold() {
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
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let sighash = tx0.sighash_all(0, &vault.redeem_script());
    let result = multisig::assemble(&vault, &sighash, &[p0]);
    assert!(
        result.is_err(),
        "no single signer may authorize a Goldcoin payout (docs/02-trust-model.md)"
    );
}

#[tokio::test]
async fn refuses_to_sign_a_request_that_is_not_yet_source_finalized() {
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
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await;
    assert!(
        matches!(result, Err(SigningError::WrongDirection(_))),
        "a GlcToSol request must never be signed as a Goldcoin payout"
    );
}

#[tokio::test]
async fn refuses_a_request_that_does_not_exist() {
    let ledger = Ledger::open_in_memory().unwrap();
    let (vault, signers) = three_signers();
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let result = independently_sign(
        &signers[0],
        &vault,
        &source,
        999,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await;
    assert!(matches!(result, Err(SigningError::RequestNotFound(999))));
}

#[tokio::test]
async fn fails_closed_when_vault_has_insufficient_funds() {
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
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await;
    assert!(matches!(result, Err(SigningError::CoinSelection(_))));
}

#[tokio::test]
async fn fails_closed_when_the_signer_itself_rejects() {
    let (vault, _) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (ledger, request_id) = ledger_with_finalized_sol_to_glc_request(&vault, 500_000, dest);
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let signer = FailingVaultSigner {
        pubkey: [0u8; 33],
        behavior: FailingSignerBehavior::Reject,
    };

    let result = independently_sign(
        &signer,
        &vault,
        &source,
        request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(SigningError::Signer(SignerError::Rejected { .. }))
        ),
        "a signer's own refusal must propagate as a hard error, never be silently treated as \
         success or skipped — got {result:?}"
    );
}

#[tokio::test]
async fn fails_closed_when_the_signer_never_responds() {
    let (vault, _) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (ledger, request_id) = ledger_with_finalized_sol_to_glc_request(&vault, 500_000, dest);
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let signer = FailingVaultSigner {
        pubkey: [0u8; 33],
        behavior: FailingSignerBehavior::Hang,
    };

    let result = independently_sign(
        &signer,
        &vault,
        &source,
        request_id,
        0,
        &test_policy(),
        Network::Testnet,
        std::time::Duration::from_millis(50),
    )
    .await;
    assert!(
        matches!(
            result,
            Err(SigningError::Signer(SignerError::Timeout { .. }))
        ),
        "a hanging signer must be bounded by the configured timeout and fail closed, never \
         block settlement indefinitely — got {result:?}"
    );
}

// ---------------------------------------------------------------------
// Step 4: spending Goldcoin UTXOs received at per-request derived
// deposit addresses, mixed freely with legacy static-vault UTXOs, using
// the existing 2-of-3 signer system with no new keys/infrastructure.
// ---------------------------------------------------------------------

/// Like [`ledger_with_finalized_sol_to_glc_request`], but funds NOTHING —
/// each Step 4 test places exactly the UTXO(s) (legacy, derived, or a mix)
/// it needs to exercise.
fn ledger_with_finalized_sol_to_glc_request_unfunded(
    amount: u64,
    dest_addr: &str,
) -> (Ledger, i64) {
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
    let SolFoldOutcomeReExport::FoldedFinalized { request_id } =
        fold(&mut ledger, amount, dest_addr)
    else {
        panic!()
    };
    (ledger, request_id)
}

/// Creates an ordinary GLC->SOL reservation and assigns it a unique
/// derived deposit address from `root` — mirrors exactly what
/// `api::BridgeApi::create_glc_to_sol_transfer` does in production
/// (`derivation::derive_request_vault` + `Ledger::set_glc_to_sol_deposit_
/// address`). This request is unrelated to (and, realistically, would
/// almost always differ in direction from) the SolToGlc `payout_request_
/// id` whose payout ends up spending its funds — see `PayoutInputContext`'s
/// own docs on why these are never the same "request_id".
fn create_derived_deposit_request(
    ledger: &mut Ledger,
    root: &MultisigVault,
    network: Network,
) -> (i64, MultisigVault, String) {
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 1,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 1,
                net_destination_atomic: 1,
            },
            &[0xABu8; 32],
            None,
            100_000,
            0,
        )
        .unwrap()
    else {
        panic!("reservation should succeed")
    };
    let derived = derivation::derive_request_vault(root, request_id, network).unwrap();
    ledger
        .set_glc_to_sol_deposit_address(
            request_id,
            derived.address(),
            &derived.script_pubkey_hex(),
            &derived.redeem_script_hex(),
        )
        .unwrap();
    let script = derived.script_pubkey_hex();
    (request_id, derived, script)
}

/// `sync_vault_utxos` treats each call as the FULL current on-chain
/// snapshot — anything `Available`/`Unconfirmed` and NOT present in a
/// given call's `observed` list is marked `Spent` (module docs). So every
/// UTXO a test wants funded must go in ONE call, never several — calling
/// this helper more than once per test would silently "spend" whatever
/// was funded by an earlier call.
fn fund_all(ledger: &mut Ledger, entries: &[([u8; 32], u32, u64, &str)]) {
    let observed: Vec<(VaultUtxo, i64, String)> = entries
        .iter()
        .map(|(txid, vout, amount_atomic, script_pubkey_hex)| {
            (
                VaultUtxo {
                    txid: *txid,
                    vout: *vout,
                    amount_atomic: *amount_atomic,
                    script_pubkey_hex: script_pubkey_hex.to_string(),
                },
                10,
                script_pubkey_hex.to_string(),
            )
        })
        .collect();
    ledger.sync_vault_utxos(&observed, 1, 0).unwrap();
}

/// Mirrors `Orchestrator::build_and_broadcast_payout`'s real signing/
/// assembly loop exactly (independent re-derivation per input per signer,
/// then per-input `multisig::assemble` using EACH input's own resolved
/// vault) — these tests exercise the identical logic production runs, not
/// a simplified stand-in.
async fn sign_and_assemble(
    vault: &MultisigVault,
    signers: &[DevVaultSigner; 3],
    threshold: usize,
    source: &DevLedgerPayoutSource<'_>,
    payout_request_id: i64,
) -> (PayoutPlan, crate::goldcoin::tx::Transaction) {
    let (first_partial, plan, mut tx) = independently_sign(
        &signers[0],
        vault,
        source,
        payout_request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let mut partials: Vec<Vec<PartialSignature>> = vec![vec![first_partial]];
    for input_index in 1..plan.inputs.len() {
        let (partial, _, _) = independently_sign(
            &signers[0],
            vault,
            source,
            payout_request_id,
            input_index,
            &test_policy(),
            Network::Testnet,
            TEST_SIGNER_TIMEOUT,
        )
        .await
        .unwrap();
        partials.push(vec![partial]);
    }
    for signer in &signers[1..threshold] {
        for (input_index, slot) in partials.iter_mut().enumerate() {
            let (partial, _, _) = independently_sign(
                signer,
                vault,
                source,
                payout_request_id,
                input_index,
                &test_policy(),
                Network::Testnet,
                TEST_SIGNER_TIMEOUT,
            )
            .await
            .unwrap();
            slot.push(partial);
        }
    }
    for (input_index, input_partials) in partials.iter().enumerate() {
        let input_vault = &plan.input_contexts[input_index].vault;
        let sighash = tx.sighash_all(input_index, &input_vault.redeem_script());
        tx.inputs[input_index].script_sig =
            multisig::assemble(input_vault, &sighash, input_partials).unwrap();
    }
    (plan, tx)
}

fn goldcoin_amount_for(solana_amount: u64) -> u64 {
    crate::amount_conversion::solana_to_goldcoin_atomic(solana_amount, TEST_SOLANA_DECIMALS)
        .unwrap()
}

#[tokio::test]
async fn spends_one_derived_address_utxo() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (mut ledger, payout_request_id) =
        ledger_with_finalized_sol_to_glc_request_unfunded(500_000, dest);
    let (funding_request_id, derived_vault, script) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);
    fund_all(
        &mut ledger,
        &[(
            [0xCCu8; 32],
            0,
            goldcoin_amount_for(500_000) + 100_000,
            script.as_str(),
        )],
    );

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let (plan, tx) = sign_and_assemble(&vault, &signers, 2, &source, payout_request_id).await;

    assert_eq!(plan.inputs.len(), 1);
    assert_eq!(
        plan.input_contexts[0].funding_request_id,
        Some(funding_request_id)
    );
    assert_eq!(plan.input_contexts[0].vault, derived_vault);
    assert_ne!(
        plan.input_contexts[0].vault, vault,
        "a derived-address input must never be signed as if it were the root vault"
    );
    assert_eq!(tx.inputs[0].script_sig[0], 0x00);
}

#[tokio::test]
async fn spends_multiple_utxos_from_the_same_derived_address() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let payout_amount = 900_000u64;
    let (mut ledger, payout_request_id) =
        ledger_with_finalized_sol_to_glc_request_unfunded(payout_amount, dest);
    let (funding_request_id, _derived_vault, script) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);
    let needed = goldcoin_amount_for(payout_amount) + 100_000;
    // Two separate on-chain outputs paying the SAME derived address —
    // neither alone covers the payout, forcing both to be selected.
    fund_all(
        &mut ledger,
        &[
            ([0xC1u8; 32], 0, needed / 2 + 10_000, script.as_str()),
            ([0xC2u8; 32], 1, needed / 2 + 10_000, script.as_str()),
        ],
    );

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let (plan, tx) = sign_and_assemble(&vault, &signers, 2, &source, payout_request_id).await;

    assert_eq!(plan.inputs.len(), 2);
    for ctx in &plan.input_contexts {
        assert_eq!(ctx.funding_request_id, Some(funding_request_id));
    }
    assert_eq!(tx.inputs[0].script_sig[0], 0x00);
    assert_eq!(tx.inputs[1].script_sig[0], 0x00);
}

#[tokio::test]
async fn spends_utxos_from_multiple_different_derived_addresses_in_one_transaction() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let payout_amount = 900_000u64;
    let (mut ledger, payout_request_id) =
        ledger_with_finalized_sol_to_glc_request_unfunded(payout_amount, dest);
    let (funding_a, _, script_a) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);
    let (funding_b, _, script_b) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);
    assert_ne!(funding_a, funding_b);
    assert_ne!(script_a, script_b);
    let needed = goldcoin_amount_for(payout_amount) + 100_000;
    fund_all(
        &mut ledger,
        &[
            ([0xA1u8; 32], 0, needed / 2 + 10_000, script_a.as_str()),
            ([0xB1u8; 32], 0, needed / 2 + 10_000, script_b.as_str()),
        ],
    );

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let (plan, tx) = sign_and_assemble(&vault, &signers, 2, &source, payout_request_id).await;

    assert_eq!(plan.inputs.len(), 2);
    let funding_ids: std::collections::HashSet<_> = plan
        .input_contexts
        .iter()
        .map(|c| c.funding_request_id)
        .collect();
    assert_eq!(
        funding_ids,
        std::collections::HashSet::from([Some(funding_a), Some(funding_b)]),
        "each input must independently resolve to its OWN funding request, not one shared vault"
    );
    // Each input's vault differs — proof they were each signed with their
    // own distinct request-specific key, not the same one applied twice.
    assert_ne!(plan.input_contexts[0].vault, plan.input_contexts[1].vault);
    assert_eq!(tx.inputs[0].script_sig[0], 0x00);
    assert_eq!(tx.inputs[1].script_sig[0], 0x00);
}

#[tokio::test]
async fn spends_a_mix_of_legacy_and_derived_address_inputs_in_one_transaction() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let payout_amount = 900_000u64;
    let (mut ledger, payout_request_id) =
        ledger_with_finalized_sol_to_glc_request_unfunded(payout_amount, dest);
    let (funding_request_id, _, derived_script) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);
    let needed = goldcoin_amount_for(payout_amount) + 100_000;
    let legacy_script = vault.script_pubkey_hex();
    // One legacy static-vault UTXO, and one per-request derived-address
    // UTXO, spent together in the SAME payout.
    fund_all(
        &mut ledger,
        &[
            ([0xDDu8; 32], 0, needed / 2 + 10_000, legacy_script.as_str()),
            (
                [0xEEu8; 32],
                0,
                needed / 2 + 10_000,
                derived_script.as_str(),
            ),
        ],
    );

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let (plan, tx) = sign_and_assemble(&vault, &signers, 2, &source, payout_request_id).await;

    assert_eq!(plan.inputs.len(), 2);
    let legacy_count = plan
        .input_contexts
        .iter()
        .filter(|c| c.funding_request_id.is_none())
        .count();
    let derived_count = plan
        .input_contexts
        .iter()
        .filter(|c| c.funding_request_id == Some(funding_request_id))
        .count();
    assert_eq!(legacy_count, 1, "exactly one legacy static-vault input");
    assert_eq!(derived_count, 1, "exactly one derived-address input");
    assert_eq!(tx.inputs[0].script_sig[0], 0x00);
    assert_eq!(tx.inputs[1].script_sig[0], 0x00);
    // Preserving legacy spending support means the legacy input's vault
    // is literally the root vault, byte-for-byte.
    let legacy_ctx = plan
        .input_contexts
        .iter()
        .find(|c| c.funding_request_id.is_none())
        .unwrap();
    assert_eq!(legacy_ctx.vault, vault);
}

#[tokio::test]
async fn wrong_request_derivation_cannot_sign_or_spend() {
    let (vault, signers) = three_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (mut ledger, payout_request_id) =
        ledger_with_finalized_sol_to_glc_request_unfunded(500_000, dest);
    let (funding_request_id, derived_vault, script) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);
    fund_all(
        &mut ledger,
        &[(
            [0xCCu8; 32],
            0,
            goldcoin_amount_for(500_000) + 100_000,
            script.as_str(),
        )],
    );

    let source = DevLedgerPayoutSource { ledger: &ledger };
    let (_partial, plan, tx) = independently_sign(
        &signers[0],
        &vault,
        &source,
        payout_request_id,
        0,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(
        plan.input_contexts[0].funding_request_id,
        Some(funding_request_id)
    );

    let sighash = tx.sighash_all(0, &derived_vault.redeem_script());
    // The signer is deliberately made to derive with the WRONG request id
    // — a bug or a compromised orchestrator feeding a bad index, not a
    // legitimate re-derivation.
    let wrong_request_id = funding_request_id + 999_999;
    assert_ne!(wrong_request_id, funding_request_id);
    let (wrong_pubkey, wrong_sig) = signers[0]
        .sign_derived(wrong_request_id, &sighash)
        .await
        .unwrap();

    // The wrong-derivation pubkey is not even a member of the CORRECT
    // derived vault — assemble must reject it outright, not merely
    // produce an invalid signature that happens to verify against the
    // wrong key.
    assert!(derived_vault.signer_position(&wrong_pubkey).is_none());
    let err = multisig::assemble(
        &derived_vault,
        &sighash,
        &[PartialSignature {
            vault_pubkey: wrong_pubkey,
            der_signature: wrong_sig,
        }],
    );
    assert_eq!(err.unwrap_err(), multisig::MultisigError::SignerNotInVault);
}

#[tokio::test]
async fn each_signer_independently_derives_a_matching_request_specific_pubkey() {
    let (vault, signers) = three_signers();
    let request_id = 4242i64;
    let derived_vault =
        derivation::derive_request_vault(&vault, request_id, Network::Testnet).unwrap();
    let sighash = [0x99u8; 32];

    for (i, signer) in signers.iter().enumerate() {
        let (derived_pubkey, sig) = signer.sign_derived(request_id, &sighash).await.unwrap();
        assert_eq!(
            derived_vault.signer_pubkeys[i], derived_pubkey,
            "signer {i} must derive exactly the pubkey at its own position in the derived vault"
        );
        assert!(multisig::verify_partial(&derived_pubkey, &sighash, &sig));
        assert_eq!(derived_vault.signer_position(&derived_pubkey), Some(i));
    }
}

#[tokio::test]
async fn reconstructed_address_exactly_matches_the_persisted_deposit_address() {
    let (vault, _signers) = three_signers();
    let (mut ledger, _payout_request_id) = ledger_with_finalized_sol_to_glc_request_unfunded(
        500_000,
        "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
    );
    let (funding_request_id, derived_vault, _script) =
        create_derived_deposit_request(&mut ledger, &vault, Network::Testnet);

    let persisted_address: String = ledger
        .raw()
        .query_row(
            "SELECT deposit_address FROM bridge_requests WHERE id = ?1",
            [funding_request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(persisted_address, derived_vault.address());

    // Independently re-derive from scratch (as a signer would, having
    // never seen the original API-side derivation) and confirm it
    // reconstructs the byte-identical vault/address.
    let rederived =
        derivation::derive_request_vault(&vault, funding_request_id, Network::Testnet).unwrap();
    assert_eq!(rederived, derived_vault);
    assert_eq!(rederived.address(), persisted_address);
}

#[tokio::test]
async fn signing_derived_keys_requires_no_stored_child_key_state() {
    let root_secret = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
    let root_pubkey = libsecp256k1::PublicKey::from_secret_key(&root_secret).serialize_compressed();
    let sighash = [0x55u8; 32];
    let request_id = 777i64;

    // First "process": derive and sign.
    let signer_a = DevVaultSigner {
        secret_key: root_secret,
        pubkey: root_pubkey,
    };
    let (pubkey_a, sig_a) = signer_a.sign_derived(request_id, &sighash).await.unwrap();

    // Simulated restart: a brand-new signer instance built ONLY from the
    // same plaintext root secret (exactly what `config::load_vault_
    // signers` reloads on every process start) — no derived key was ever
    // persisted anywhere, in memory or on disk, between these two calls.
    let signer_b = DevVaultSigner {
        secret_key: root_secret,
        pubkey: root_pubkey,
    };
    let (pubkey_b, sig_b) = signer_b.sign_derived(request_id, &sighash).await.unwrap();

    assert_eq!(pubkey_a, pubkey_b);
    assert_eq!(
        sig_a, sig_b,
        "ECDSA signing here uses deterministic (RFC6979-style) nonces — identical inputs must \
         reproduce a byte-identical signature across a restart with no persisted child-key state"
    );
}

#[tokio::test]
async fn a_signer_that_does_not_implement_derived_signing_fails_closed() {
    let signer = FailingVaultSigner {
        pubkey: [0u8; 33],
        behavior: FailingSignerBehavior::Reject,
    };
    let sighash = [0x11u8; 32];
    let result = signer.sign_derived(42, &sighash).await;
    assert!(
        matches!(result, Err(SignerError::Rejected { .. })),
        "the trait default must fail closed for any signer that hasn't opted into per-request \
         derivation, never silently sign with the wrong key — got {result:?}"
    );
}
