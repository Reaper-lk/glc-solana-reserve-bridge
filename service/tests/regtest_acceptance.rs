//! Phase 6 real-node acceptance rehearsal (docs/13-phase6-readiness-audit.md):
//! exercises the full bridge end-to-end against a real `goldcoind` regtest
//! node and a real `solana-test-validator` with this repository's own
//! compiled program deployed — no mocks anywhere in the path under test.
//!
//! Skipped (never failed) unless `GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN` are set
//! and the on-chain program has been built and `solana-test-validator` is
//! on `PATH` — see `support::phase6_prereqs`. Every keypair, database, and
//! node datadir is fresh and thrown away at the end of each test; nothing
//! here ever touches a non-loopback address or a real wallet.

mod support;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use glc_reserve_bridge_service::amount_conversion;
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::indexer::{Indexer, IndexerConfig};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{
    CreateRequestOutcome, Direction, Ledger, RequestAmounts, RequestState, ReserveDirection,
};
use glc_reserve_bridge_service::orchestrator::{Orchestrator, OrchestratorConfig};
use glc_reserve_bridge_service::signing::attestation::DevAttestationSigner;
use glc_reserve_bridge_service::signing::goldcoin_vault::DevVaultSigner;
use glc_reserve_bridge_service::signing::signers::{AttestationSigner, VaultSigner};
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::indexer::SolanaIndexer;
use glc_reserve_bridge_service::solana::rpc::SolanaRpc;

use support::{LocalValidator, RegtestNode};

/// Matches the canonical Solana GLC mint's real, verified decimals
/// (docs/18-token-2022-support.md) — deliberately NOT the same as
/// Goldcoin's own native-chain decimals (8, `amount_conversion::
/// GOLDCOIN_DECIMALS`), so these real-node tests genuinely exercise the
/// cross-chain amount conversion under the real mismatch, not a
/// degenerate case where both chains happen to agree.
const SOLANA_GLC_DECIMALS: u8 = 6;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn base_orchestrator_config() -> OrchestratorConfig {
    OrchestratorConfig {
        attestation_threshold: 2,
        vault_threshold: 2,
        required_goldcoin_confirmations: 3,
        // Real regtest node's relay policy rejected the old value
        // (1_000, i.e. 0.00001 GLC/kB) as "insufficient priority" for a
        // transaction spending freshly-generated (low-priority) inputs —
        // a real fee-policy fact this real-node test caught. Bumped well
        // above the node's minimum relay fee.
        fee_rate_per_kb: 100_000,
        dust_threshold: 1_000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        reconciliation_tolerance: 0,
        vault_min_confirmations: 1,
        goldcoin_network: Network::Testnet,
        signer_timeout: std::time::Duration::from_secs(5),
    }
}

fn box_vault_signers(signers: Vec<DevVaultSigner>) -> Vec<Box<dyn VaultSigner>> {
    signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect()
}

fn box_attestation_signers(signers: Vec<DevAttestationSigner>) -> Vec<Box<dyn AttestationSigner>> {
    signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn AttestationSigner>)
        .collect()
}

fn three_vault_signers() -> (MultisigVault, Vec<DevVaultSigner>) {
    let signers = vec![
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

/// Real gross/fee/net breakdown for a GlcToSol request, mirroring
/// `api::create_glc_to_sol_transfer`'s own computation (docs/20-bridge-
/// fee.md): `gross` is Goldcoin-native/canonical, and
/// `net_destination_atomic` is the fee-adjusted amount converted to the
/// reserve mint's own decimals.
fn glc_to_sol_amounts(gross: u64, solana_decimals: u8) -> RequestAmounts {
    let fb = amount_conversion::compute_fee(amount_conversion::CanonicalAtomic(gross)).unwrap();
    let net_destination = fb.net.to_solana(solana_decimals).unwrap();
    RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: net_destination.0,
    }
}

fn three_attestation_signers() -> (Vec<Pubkey>, Vec<DevAttestationSigner>) {
    let signers = vec![
        DevAttestationSigner::generate(),
        DevAttestationSigner::generate(),
        DevAttestationSigner::generate(),
    ];
    let pubkeys = signers.iter().map(|s| s.pubkey()).collect();
    (pubkeys, signers)
}

/// Runs `orchestrator.tick` up to `max_ticks` times (sleeping briefly
/// between each so real chain/validator state has a chance to move),
/// stopping early once `done` reports true. Returns the last tick's
/// non-empty error list, if any — never panics on its own, so callers can
/// assert with the full context of what was still failing.
async fn tick_until<GR, SR>(
    orchestrator: &mut Orchestrator<GR, SR>,
    max_ticks: u32,
    mut done: impl FnMut(&Orchestrator<GR, SR>) -> bool,
) -> Vec<String>
where
    GR: glc_reserve_bridge_service::goldcoin::indexer::GoldcoinRpc,
    SR: glc_reserve_bridge_service::solana::rpc::SolanaRpc,
{
    let mut all_errors = Vec::new();
    for _ in 0..max_ticks {
        let report = orchestrator.tick(now_unix()).await;
        all_errors.extend(report.errors);
        if done(orchestrator) {
            return Vec::new();
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    all_errors
}

#[tokio::test(flavor = "multi_thread")]
async fn glc_to_sol_release_settles_end_to_end_on_real_nodes() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping glc_to_sol_release_settles_end_to_end_on_real_nodes: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    // ---- Goldcoin regtest: vault + spendable coin ----
    let goldcoin = RegtestNode::start(&goldcoind, &cli);
    let (vault, vault_signers) = three_vault_signers();
    goldcoin.import_vault(vault.address(), &vault.redeem_script_hex());
    let miner_address = goldcoin.new_address();
    goldcoin.generate(101, &miner_address); // 100 for coinbase maturity + 1 spendable

    // ---- Solana localnet: program, throwaway mint, reserve funding ----
    let upgrade_authority = Keypair::new();
    let validator = LocalValidator::start(&so, &accounts::PROGRAM_ID, &upgrade_authority.pubkey());
    let blocking = validator.blocking_client();
    support::airdrop(&blocking, &upgrade_authority.pubkey(), 10_000_000_000);

    let (attestation_pubkeys, attestation_signers) = three_attestation_signers();
    // The canonical Solana GLC mint is Token-2022 (docs/18-token-2022-
    // support.md) — this rehearsal uses a real Token-2022 throwaway mint,
    // carrying the same MetadataPointer extension the real mint has, to
    // prove genuine end-to-end compatibility on a real validator.
    let token_program = spl_token_2022::ID;
    let mint = support::create_throwaway_token2022_mint(
        &blocking,
        &upgrade_authority,
        SOLANA_GLC_DECIMALS,
    );
    support::bootstrap_program(
        &blocking,
        &upgrade_authority,
        &attestation_pubkeys,
        2,
        &mint.pubkey(),
        &token_program,
    );

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_ata =
        accounts::associated_token_address(&reserve_authority, &mint.pubkey(), &token_program);
    support::mint_to(
        &blocking,
        &upgrade_authority,
        &mint.pubkey(),
        &token_program,
        &reserve_ata,
        &upgrade_authority,
        100_000_000_000,
    );
    // See `support::wait_for_finalized_balance`: reconciliation reads at
    // `finalized`, which lags `confirmed` (used above) by ~32 slots — wait
    // for the funding itself to finalize before the ledger's configured
    // baseline and the orchestrator's reconciliation can observe it.
    support::wait_for_finalized_balance(&validator.real_rpc(), &reserve_ata, 100_000_000_000).await;

    let recipient = Keypair::new();
    let recipient_ata = support::create_ata(
        &blocking,
        &upgrade_authority,
        &recipient.pubkey(),
        &mint.pubkey(),
        &token_program,
    );

    let submitter = Keypair::new();
    support::airdrop(&blocking, &submitter.pubkey(), 10_000_000_000);

    // ---- Ledger + orchestrator ----
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let amount_atomic = 250_000_000u64; // 2.5 GLC at 8 decimals
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                100_000_000_000, // matches the real reserve balance minted on-chain below
                0,
                50_000_000_000,
                20_000_000_000,
                10_000_000_000,
                now_unix(),
            )
            .unwrap();
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                glc_to_sol_amounts(amount_atomic, SOLANA_GLC_DECIMALS),
                &recipient.pubkey().to_bytes(),
                None,
                3_600,
                now_unix(),
            )
            .unwrap()
        else {
            panic!("expected capacity to be reserved")
        };
        request_id
    };

    let goldcoin_indexer = Indexer::new(
        goldcoin.rpc_client(),
        Ledger::open(&db_path).unwrap(),
        IndexerConfig {
            vault_script_hex: vault.script_pubkey_hex(),
            confirmation_depth: 3,
            max_reorg_depth: 50,
            initial_checkpoint: None,
        },
    );
    let solana_indexer = SolanaIndexer::new(validator.real_rpc(), Ledger::open(&db_path).unwrap());
    let ledger = Ledger::open(&db_path).unwrap();

    let mut orchestrator = Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        ledger,
        goldcoin.rpc_client(),
        validator.real_rpc(),
        vault.clone(),
        box_vault_signers(vault_signers),
        box_attestation_signers(attestation_signers),
        submitter,
        base_orchestrator_config(),
        now_unix(),
    );

    // ---- Real deposit ----
    let txid_hex = goldcoin.send_deposit_with_binding(vault.address(), 2.5, request_id);
    goldcoin.generate(5, &miner_address); // comfortably past confirmation_depth

    let errors = tick_until(&mut orchestrator, 150, |o| {
        o.ledger()
            .get_request(request_id)
            .unwrap()
            .map(|r| r.state == RequestState::Settled)
            .unwrap_or(false)
    })
    .await;

    let req = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        req.state,
        RequestState::Settled,
        "release never settled; last tick errors: {errors:?}"
    );
    assert_eq!(
        req.source_txid
            .map(|t| glc_reserve_bridge_service::goldcoin::hex::encode(&t)),
        Some(txid_hex.clone()),
        "settled request's recorded source txid must match the real deposit"
    );

    // `amount_atomic` was declared Goldcoin-native gross (matching the real
    // deposit); the real Solana transfer moves the NET amount (after the
    // 6% bridge fee, docs/20-bridge-fee.md), converted to the mint's own
    // live decimals (docs/18-token-2022-support.md's conversion policy).
    let expected_solana_amount =
        glc_to_sol_amounts(amount_atomic, SOLANA_GLC_DECIMALS).net_destination_atomic;
    let recipient_balance = support::token_balance(&blocking, &recipient_ata);
    assert_eq!(
        recipient_balance, expected_solana_amount,
        "recipient's real SPL balance must equal exactly the deposited amount minus the 6% \
         bridge fee, converted to the mint's own decimals — 1:1 on the underlying GLC \
         denomination before the service fee"
    );
    let settled = orchestrator
        .ledger()
        .settled_liquidity(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(settled, expected_solana_amount);
}

#[tokio::test(flavor = "multi_thread")]
async fn sol_to_glc_payout_settles_end_to_end_on_real_nodes() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping sol_to_glc_payout_settles_end_to_end_on_real_nodes: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    // ---- Goldcoin regtest: vault, spendable coin, and the vault funded for real ----
    let goldcoin = RegtestNode::start(&goldcoind, &cli);
    let (vault, vault_signers) = three_vault_signers();
    goldcoin.import_vault(vault.address(), &vault.redeem_script_hex());
    let miner_address = goldcoin.new_address();
    goldcoin.generate(101, &miner_address);
    // Fund the vault via a regular (non-coinbase) send so it only needs a
    // handful of confirmations, not 100-block coinbase maturity.
    goldcoin.cli(&["sendtoaddress", vault.address(), "5"]);
    goldcoin.generate(3, &miner_address);
    let destination_address = goldcoin.new_address();

    // ---- Solana localnet: program, throwaway mint, user funding ----
    let upgrade_authority = Keypair::new();
    let validator = LocalValidator::start(&so, &accounts::PROGRAM_ID, &upgrade_authority.pubkey());
    let blocking = validator.blocking_client();
    support::airdrop(&blocking, &upgrade_authority.pubkey(), 10_000_000_000);

    let (attestation_pubkeys, attestation_signers) = three_attestation_signers();
    // Real Token-2022 throwaway mint — see the matching comment in
    // `glc_to_sol_release_settles_end_to_end_on_real_nodes`.
    let token_program = spl_token_2022::ID;
    let mint = support::create_throwaway_token2022_mint(
        &blocking,
        &upgrade_authority,
        SOLANA_GLC_DECIMALS,
    );
    support::bootstrap_program(
        &blocking,
        &upgrade_authority,
        &attestation_pubkeys,
        2,
        &mint.pubkey(),
        &token_program,
    );

    let user = Keypair::new();
    support::airdrop(&blocking, &user.pubkey(), 10_000_000_000);
    let user_ata = support::create_ata(
        &blocking,
        &upgrade_authority,
        &user.pubkey(),
        &mint.pubkey(),
        &token_program,
    );
    let amount_atomic = 1_500_000u64; // 1.5 GLC at the canonical mint's 6 decimals
    support::mint_to(
        &blocking,
        &upgrade_authority,
        &mint.pubkey(),
        &token_program,
        &user_ata,
        &upgrade_authority,
        amount_atomic,
    );

    let submitter = Keypair::new();
    support::airdrop(&blocking, &submitter.pubkey(), 10_000_000_000);

    // ---- Ledger + orchestrator ----
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::GoldcoinReserve,
                500_000_000, // matches the real vault funding above (5 GLC)
                0,
                200_000_000,
                100_000_000,
                50_000_000,
                now_unix(),
            )
            .unwrap();
    }

    let goldcoin_indexer = Indexer::new(
        goldcoin.rpc_client(),
        Ledger::open(&db_path).unwrap(),
        IndexerConfig {
            vault_script_hex: vault.script_pubkey_hex(),
            confirmation_depth: 3,
            max_reorg_depth: 50,
            initial_checkpoint: None,
        },
    );
    let solana_indexer = SolanaIndexer::new(validator.real_rpc(), Ledger::open(&db_path).unwrap());
    let ledger = Ledger::open(&db_path).unwrap();

    let mut orchestrator = Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        ledger,
        goldcoin.rpc_client(),
        validator.real_rpc(),
        vault.clone(),
        box_vault_signers(vault_signers),
        box_attestation_signers(attestation_signers),
        submitter,
        base_orchestrator_config(),
        now_unix(),
    );

    // ---- Real deposit_to_reserve: the user's own signed SPL transfer ----
    let deposit_ix = glc_reserve_bridge_service::solana::instructions::deposit_to_reserve(
        &user.pubkey(),
        &mint.pubkey(),
        &token_program,
        0, // first obligation on a freshly bootstrapped program
        amount_atomic,
        destination_address.as_bytes(),
    );
    let blockhash = blocking.get_latest_blockhash().unwrap();
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[deposit_ix],
        Some(&user.pubkey()),
        &[&user],
        blockhash,
    );
    blocking
        .send_and_confirm_transaction(&tx)
        .expect("real deposit_to_reserve transaction must land");

    // ---- Drive settlement ----
    // A real payout transaction only accrues confirmations if someone
    // keeps mining — unlike the GLC->SOL direction (where the deposit was
    // already mined before ticking starts), the vault payout is broadcast
    // *during* the tick loop, so this loop must also keep advancing the
    // Goldcoin chain, exactly as a real regtest miner would.
    let mut errors = Vec::new();
    for i in 0..150u32 {
        let report = orchestrator.tick(now_unix()).await;
        errors.extend(report.errors);
        if i % 2 == 0 {
            goldcoin.generate(1, &miner_address);
        }
        let settled = orchestrator
            .ledger()
            .requests_by_state(Direction::SolToGlc, RequestState::Settled)
            .map(|r| !r.is_empty())
            .unwrap_or(false);
        if settled {
            errors.clear();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let settled_requests = orchestrator
        .ledger()
        .requests_by_state(Direction::SolToGlc, RequestState::Settled)
        .unwrap();
    if settled_requests.is_empty() {
        for state in [
            RequestState::LiquidityReserved,
            RequestState::SourceFinalized,
            RequestState::SettlementAuthorized,
            RequestState::DestinationSubmitted,
            RequestState::DestinationConfirmed,
            RequestState::ManualReview,
            RequestState::Failed,
        ] {
            let reqs = orchestrator
                .ledger()
                .requests_by_state(Direction::SolToGlc, state)
                .unwrap();
            if !reqs.is_empty() {
                eprintln!(
                    "DIAG: {} request(s) in state {state:?}: {reqs:?}",
                    reqs.len()
                );
            }
        }
        eprintln!(
            "DIAG: last_synced_obligation_count = {:?}",
            orchestrator.ledger().last_synced_obligation_count()
        );
    }
    assert_eq!(
        settled_requests.len(),
        1,
        "payout never settled; last tick errors: {errors:?}"
    );
    // `amount_atomic` is Solana-native gross; the ledger's own
    // `gross_amount_atomic`/`net_amount_atomic` are canonical
    // (docs/20-bridge-fee.md) — widen `amount_atomic` to canonical and take
    // the 6% bridge fee the same way `solana::indexer::tick` does.
    let gross_canonical = amount_conversion::SolanaAtomic(amount_atomic)
        .to_canonical(SOLANA_GLC_DECIMALS)
        .unwrap();
    let fee_breakdown = amount_conversion::compute_fee(gross_canonical).unwrap();
    assert_eq!(settled_requests[0].gross_amount_atomic, gross_canonical.0);
    assert_eq!(settled_requests[0].net_amount_atomic, fee_breakdown.net.0);

    // The real Goldcoin payout moves the NET Goldcoin-native (8-decimal)
    // amount, after the 6% bridge fee — 1:1 on the underlying GLC
    // denomination before the service fee (docs/20-bridge-fee.md).
    let expected_goldcoin_atomic = fee_breakdown.net.0;
    let dest_balance_glc = goldcoin.confirmed_balance_of(&destination_address);
    let dest_balance_atomic = (dest_balance_glc * 100_000_000.0).round() as u64;
    assert_eq!(
        dest_balance_atomic, expected_goldcoin_atomic,
        "destination's real regtest balance must equal exactly the requested amount minus the \
         6% bridge fee, converted to Goldcoin's own decimals"
    );
    let settled = orchestrator
        .ledger()
        .settled_liquidity(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(settled, expected_goldcoin_atomic);
}

/// Covers, against real nodes in a single session (to keep total setup
/// cost bounded — see docs/13-phase6-readiness-audit.md item 6):
///
/// 1. **Double-release rejection**: after a real release settles, a second
///    `release_from_reserve` for the identical `(txid, vout)` — signed by
///    the same real attestation signers over the identical, independently
///    re-derivable claim message — is rejected by the real on-chain
///    `DepositClaim` replay guard, not merely by this service's own
///    ledger bookkeeping.
/// 2. **Reconciliation after settlement**: reconciling against the real
///    post-settlement on-chain reserve balance classifies `WithinTolerance`.
/// 3. **Crash + restart recovery**: a second, independent request is
///    carried partway through settlement, the orchestrator is dropped
///    (simulating a crash) and rebuilt fresh against the same on-disk
///    ledger and the same real nodes, and settlement completes correctly
///    with no double-processing.
#[tokio::test(flavor = "multi_thread")]
async fn double_release_crash_restart_and_reconciliation_on_real_nodes() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping double_release_crash_restart_and_reconciliation_on_real_nodes: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    let goldcoin = RegtestNode::start(&goldcoind, &cli);
    let (vault, vault_signers) = three_vault_signers();
    goldcoin.import_vault(vault.address(), &vault.redeem_script_hex());
    let miner_address = goldcoin.new_address();
    goldcoin.generate(101, &miner_address);

    let upgrade_authority = Keypair::new();
    let validator = LocalValidator::start(&so, &accounts::PROGRAM_ID, &upgrade_authority.pubkey());
    let blocking = validator.blocking_client();
    support::airdrop(&blocking, &upgrade_authority.pubkey(), 10_000_000_000);

    let (attestation_pubkeys, attestation_signers) = three_attestation_signers();
    // Real Token-2022 throwaway mint — see the matching comment in
    // `glc_to_sol_release_settles_end_to_end_on_real_nodes`. This test in
    // particular is what covers "restart/recovery after Token-2022
    // settlement" (Task 4's adversarial matrix).
    let token_program = spl_token_2022::ID;
    let mint = support::create_throwaway_token2022_mint(
        &blocking,
        &upgrade_authority,
        SOLANA_GLC_DECIMALS,
    );
    support::bootstrap_program(
        &blocking,
        &upgrade_authority,
        &attestation_pubkeys,
        2,
        &mint.pubkey(),
        &token_program,
    );

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_ata =
        accounts::associated_token_address(&reserve_authority, &mint.pubkey(), &token_program);
    support::mint_to(
        &blocking,
        &upgrade_authority,
        &mint.pubkey(),
        &token_program,
        &reserve_ata,
        &upgrade_authority,
        100_000_000_000,
    );
    // See `support::wait_for_finalized_balance`: without this, the first
    // reconciliation tick can observe the pre-funding (zero) balance at
    // `finalized` commitment, misclassify it as an unexplained drop from
    // the ledger's configured 100B baseline, and permanently pause the
    // reserve (reconciliation never auto-unpauses) before anything settles.
    support::wait_for_finalized_balance(&validator.real_rpc(), &reserve_ata, 100_000_000_000).await;

    let recipient = Keypair::new();
    let recipient_ata = support::create_ata(
        &blocking,
        &upgrade_authority,
        &recipient.pubkey(),
        &mint.pubkey(),
        &token_program,
    );
    let submitter = Keypair::new();
    support::airdrop(&blocking, &submitter.pubkey(), 10_000_000_000);

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let amount_atomic = 200_000_000u64; // 2.0 GLC
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                100_000_000_000,
                0,
                50_000_000_000,
                20_000_000_000,
                10_000_000_000,
                now_unix(),
            )
            .unwrap();
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                glc_to_sol_amounts(amount_atomic, SOLANA_GLC_DECIMALS),
                &recipient.pubkey().to_bytes(),
                None,
                3_600,
                now_unix(),
            )
            .unwrap()
        else {
            panic!("expected capacity to be reserved")
        };
        request_id
    };

    let build_orchestrator = || {
        let goldcoin_indexer = Indexer::new(
            goldcoin.rpc_client(),
            Ledger::open(&db_path).unwrap(),
            IndexerConfig {
                vault_script_hex: vault.script_pubkey_hex(),
                confirmation_depth: 3,
                max_reorg_depth: 50,
                initial_checkpoint: None,
            },
        );
        let solana_indexer =
            SolanaIndexer::new(validator.real_rpc(), Ledger::open(&db_path).unwrap());
        let ledger = Ledger::open(&db_path).unwrap();
        Orchestrator::new(
            goldcoin_indexer,
            solana_indexer,
            ledger,
            goldcoin.rpc_client(),
            validator.real_rpc(),
            vault.clone(),
            box_vault_signers(
                vault_signers
                    .iter()
                    .map(|s| DevVaultSigner {
                        secret_key: s.secret_key,
                        pubkey: s.pubkey,
                    })
                    .collect(),
            ),
            box_attestation_signers(
                attestation_signers
                    .iter()
                    .map(|s| DevAttestationSigner {
                        keypair: Keypair::try_from(s.keypair.to_bytes().as_slice()).unwrap(),
                    })
                    .collect(),
            ),
            Keypair::try_from(submitter.to_bytes().as_slice()).unwrap(),
            base_orchestrator_config(),
            now_unix(),
        )
    };

    let mut orchestrator = build_orchestrator();

    // ---- Settle the first request normally ----
    let txid_hex = goldcoin.send_deposit_with_binding(vault.address(), 2.0, request_id);
    goldcoin.generate(5, &miner_address);
    let errors = tick_until(&mut orchestrator, 150, |o| {
        o.ledger()
            .get_request(request_id)
            .unwrap()
            .map(|r| r.state == RequestState::Settled)
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        orchestrator
            .ledger()
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Settled,
        "baseline release never settled; errors: {errors:?}"
    );

    // ---- 1. Double-release: a second attempt for the same (txid, vout) ----
    // Rebuilds the identical, independently-derivable claim message (real
    // signers, real live epoch/mint reads) and submits it directly,
    // bypassing this service's own ledger-state guard, to prove the
    // ON-CHAIN DepositClaim replay guard — not just this service's
    // bookkeeping — is what actually stops a second release.
    let txid: [u8; 32] =
        glc_reserve_bridge_service::goldcoin::hex::decode_exact(&txid_hex).unwrap();
    // Never assume vout 0 — `fundrawtransaction` chooses the change
    // position, so the real deposit's vout genuinely varies (docs/goldcoin-
    // rpc-notes.md). The settled request's own recorded vout is the source
    // of truth for what the original, successful release actually claimed.
    let vout = orchestrator
        .ledger()
        .get_request(request_id)
        .unwrap()
        .unwrap()
        .source_vout
        .unwrap();
    let key_set = validator
        .real_rpc()
        .get_account(&accounts::attestation_key_set_pda())
        .await
        .unwrap()
        .unwrap();
    let key_set = accounts::decode_attestation_key_set(&key_set.data).unwrap();
    // Must be the exact claim the original, successful release carried —
    // the NET amount (after the 6% bridge fee, docs/20-bridge-fee.md),
    // converted to the reserve mint's live decimals
    // (docs/18-token-2022-support.md), not the raw Goldcoin-native gross
    // figure, so this genuinely replays the identical on-chain claim
    // rather than merely failing for an unrelated reason (a wrong amount
    // would already fail signature verification before ever reaching the
    // DepositClaim replay guard this sub-test means to prove).
    let solana_amount =
        glc_to_sol_amounts(amount_atomic, SOLANA_GLC_DECIMALS).net_destination_atomic;
    let message = glc_reserve_bridge_shared::claim::release_claim_message(
        1,
        &accounts::PROGRAM_ID.to_bytes(),
        key_set.epoch,
        &txid,
        vout,
        solana_amount,
        &recipient.pubkey().to_bytes(),
        &mint.pubkey().to_bytes(),
    );
    let sigs: Vec<_> = attestation_signers[..2]
        .iter()
        .map(|s| glc_reserve_bridge_service::solana::ed25519::sign(&s.keypair, &message))
        .collect();
    let proof_ix =
        glc_reserve_bridge_service::solana::ed25519::build_attestation_proof(&sigs, &message);
    let release_ix = glc_reserve_bridge_service::solana::instructions::release_from_reserve(
        &submitter.pubkey(),
        &mint.pubkey(),
        &token_program,
        &recipient.pubkey(),
        txid,
        vout,
        solana_amount,
        key_set.epoch,
    );
    let blockhash = blocking.get_latest_blockhash().unwrap();
    let replay_tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[proof_ix, release_ix],
        Some(&submitter.pubkey()),
        &[&submitter],
        blockhash,
    );
    let replay_result = blocking.send_and_confirm_transaction(&replay_tx);
    assert!(
        replay_result.is_err(),
        "a second release_from_reserve for an already-claimed deposit must be rejected on-chain"
    );
    let recipient_balance_after_replay_attempt = support::token_balance(&blocking, &recipient_ata);
    assert_eq!(
        recipient_balance_after_replay_attempt, solana_amount,
        "a rejected replay must never move additional funds — 1:1 accounting must hold exactly"
    );

    // ---- 2. Reconciliation against the real post-settlement balance ----
    let report = orchestrator.tick(now_unix()).await;
    let reconciliation = report
        .solana_reconciliation
        .expect("reconciliation must run every tick")
        .expect("reconciliation must succeed against a real, reachable reserve account");
    assert_eq!(
        reconciliation.classification,
        glc_reserve_bridge_service::reconciliation::Classification::WithinTolerance,
        "post-settlement reconciliation against the real on-chain balance must find no breach"
    );

    // ---- 3. Crash + restart recovery, on a second, independent request ----
    let request_id_2 = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let outcome = ledger
            .create_request(
                Direction::GlcToSol,
                glc_to_sol_amounts(amount_atomic, SOLANA_GLC_DECIMALS),
                &recipient.pubkey().to_bytes(),
                None,
                3_600,
                now_unix(),
            )
            .unwrap();
        let CreateRequestOutcome::Reserved { request_id } = outcome else {
            panic!("expected capacity to be reserved for the second request, got {outcome:?}")
        };
        request_id
    };
    let txid2_hex = goldcoin.send_deposit_with_binding(vault.address(), 2.0, request_id_2);
    goldcoin.generate(5, &miner_address);
    // Carry it partway (through SourceFinalized, possibly further) then
    // simulate a crash by dropping the orchestrator entirely.
    for _ in 0..20 {
        orchestrator.tick(now_unix()).await;
        if orchestrator
            .ledger()
            .get_request(request_id_2)
            .unwrap()
            .map(|r| {
                r.state != RequestState::LiquidityReserved
                    && r.state != RequestState::AwaitingDeposit
            })
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    drop(orchestrator); // simulated crash

    // Restart: a fresh orchestrator, same on-disk ledger, same real nodes.
    let mut orchestrator = build_orchestrator();
    let errors = tick_until(&mut orchestrator, 150, |o| {
        o.ledger()
            .get_request(request_id_2)
            .unwrap()
            .map(|r| r.state == RequestState::Settled)
            .unwrap_or(false)
    })
    .await;
    let req2 = orchestrator
        .ledger()
        .get_request(request_id_2)
        .unwrap()
        .unwrap();
    assert_eq!(
        req2.state,
        RequestState::Settled,
        "restarted orchestrator must complete settlement; errors: {errors:?}"
    );
    assert_eq!(
        req2.source_txid
            .map(|t| glc_reserve_bridge_service::goldcoin::hex::encode(&t)),
        Some(txid2_hex)
    );

    let total_recipient_balance = support::token_balance(&blocking, &recipient_ata);
    assert_eq!(
        total_recipient_balance,
        solana_amount * 2,
        "exactly two settlements' worth of funds, no more (no double-processing across the restart, \
         no leakage from the rejected replay attempt)"
    );
    let settled = orchestrator
        .ledger()
        .settled_liquidity(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(settled, solana_amount * 2);
}
