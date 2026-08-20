use std::collections::HashMap;
use std::sync::Mutex;

use solana_sdk::account::Account;

use super::*;
use crate::ledger::{CreateRequestOutcome, ReserveDirection, SolFoldOutcome};
use crate::signing::signers::SignerError;
use crate::solana::rpc::SolanaRpcError;

/// A test double proving `independently_attest_release`/
/// `independently_attest_completion` depend only on the
/// `AttestationSigner` trait, not `DevAttestationSigner` concretely — and
/// that they fail closed on both an explicit signer error and a signer
/// that never responds (docs/22-production-readiness-review.md P0
/// "signer abstraction").
struct FailingAttestationSigner {
    pubkey: Pubkey,
    behavior: FailingSignerBehavior,
}

#[derive(Clone, Copy)]
enum FailingSignerBehavior {
    Reject,
    Hang,
}

impl AttestationSigner for FailingAttestationSigner {
    fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    fn sign_message<'a>(
        &'a self,
        _message: &'a [u8],
    ) -> crate::signing::signers::BoxFut<'a, Result<Signature, SignerError>> {
        match self.behavior {
            FailingSignerBehavior::Reject => Box::pin(async move {
                Err(SignerError::Rejected {
                    identity: self.pubkey.to_string(),
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

struct MockRpc {
    accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
}

impl MockRpc {
    fn new() -> Self {
        MockRpc {
            accounts: Mutex::new(HashMap::new()),
        }
    }

    fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
        self.accounts.lock().unwrap().insert(pubkey, data);
    }
}

impl SolanaRpc for MockRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(pubkey)
            .cloned()
            .map(|data| Account {
                lamports: 1,
                data,
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }))
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn get_latest_blockhash(&self) -> Result<solana_sdk::hash::Hash, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn send_transaction(
        &self,
        _tx: &solana_sdk::transaction::Transaction,
    ) -> Result<Signature, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
    async fn is_blockhash_valid(
        &self,
        _blockhash: &solana_sdk::hash::Hash,
    ) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised by attestation tests")
    }
}

/// Matches the canonical Solana GLC mint's live decimals (docs/18-token-
/// 2022-support.md) — every test in this module reads it live via a fake
/// mint account, exactly as `fetch_reserve_mint_decimals` does against a
/// real one.
const TEST_SOLANA_DECIMALS: u8 = 6;
const TEST_SIGNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A minimal, real 82-byte `spl_token::state::Mint`-shaped buffer —
/// `decode_mint_basics` only reads the base layout, so only `decimals`
/// (offset 44) needs to be meaningful here.
fn fake_mint_bytes(decimals: u8) -> Vec<u8> {
    let mut v = vec![0u8; 82];
    v[44] = decimals;
    v[45] = 1; // is_initialized
    v
}

fn fake_attestation_key_set_bytes(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Vec<u8> {
    let mut v = vec![0u8; 8]; // discriminator
    v.extend_from_slice(&epoch.to_le_bytes());
    v.push(threshold);
    v.push(1); // bump
    v.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        v.extend_from_slice(k.as_ref());
    }
    v.extend_from_slice(&[0u8; 32]); // reserved
    v
}

fn fake_bridge_config_bytes(reserve_token_mint: [u8; 32], obligation_count: u64) -> Vec<u8> {
    let mut v = vec![0u8; 8]; // discriminator
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin tag (None) — Borsh variable-length: no payload bytes follow
    v.push(0); // paused
    v.push(0); // release_paused
    v.push(0); // deposit_paused
    v.push(7); // bump
    v.extend_from_slice(&reserve_token_mint);
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3); // reserve_authority_bump
    v.extend_from_slice(&obligation_count.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
    v.extend_from_slice(&100u64.to_le_bytes()); // min_transfer_amount
    v.extend_from_slice(&1_000_000u64.to_le_bytes()); // per_transfer_limit
    v.extend_from_slice(&500u64.to_le_bytes()); // protected_minimum
    v.extend_from_slice(&2_000_000u64.to_le_bytes()); // rolling_volume_limit
    v.extend_from_slice(&3600i64.to_le_bytes()); // rolling_window_seconds
    v
}

fn fake_withdrawal_obligation_bytes(index: u64, amount: u64, glc_address: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 8]; // discriminator
    v.extend_from_slice(&index.to_le_bytes());
    v.extend_from_slice(&amount.to_le_bytes());
    v.extend_from_slice(&[5u8; 32]); // requester
    let mut addr = [0u8; 64];
    addr[..glc_address.len()].copy_from_slice(glc_address);
    v.extend_from_slice(&addr);
    v.push(glc_address.len() as u8);
    v.push(0); // status = Pending
    v.extend_from_slice(&11u64.to_le_bytes()); // requested_at_slot
    v.push(1); // protocol_version
    v.push(2); // bump
    v.extend_from_slice(&[0u8; 48]); // reserved
    v
}

fn ledger_with_both_reserves() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    // GoldcoinReserve capacity must cover the canonical (8-decimal) scale
    // of a Solana-native SolToGlc `amount` (6 decimals) once correctly
    // converted (docs/20-bridge-fee.md) — a 500_000 Solana-native deposit
    // widens to 50_000_000 canonical before the fee is even taken, well
    // beyond the pre-fee-round 10_000_000 fixture.
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
    ledger
}

/// Builds a real gross/fee/net breakdown for a GlcToSol request the same
/// way `api::create_glc_to_sol_transfer` does (docs/20-bridge-fee.md):
/// `gross` is Goldcoin-native/canonical, and `net_destination_atomic` is
/// the fee-adjusted amount converted to the reserve mint's own decimals.
fn glc_to_sol_amounts(gross: u64) -> crate::ledger::RequestAmounts {
    let fb =
        crate::amount_conversion::compute_fee(crate::amount_conversion::CanonicalAtomic(gross))
            .unwrap();
    let net_destination = fb.net.to_solana(TEST_SOLANA_DECIMALS).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: net_destination.0,
    }
}

/// Mirrors `solana::indexer::tick`'s real conversion for a SolToGlc
/// obligation (docs/20-bridge-fee.md): `amount` is Solana-native gross.
fn sol_to_glc_amounts(amount: u64) -> crate::ledger::RequestAmounts {
    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let fb = crate::amount_conversion::compute_fee(gross_canonical).unwrap();
    crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

fn finalized_glc_to_sol_request(ledger: &mut Ledger, amount: u64, recipient: [u8; 32]) -> i64 {
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            glc_to_sol_amounts(amount),
            &recipient,
            None,
            3600,
            0,
        )
        .unwrap()
    else {
        panic!("expected capacity to be reserved")
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAAu8; 32], 2, amount, 10, [0u8; 32], 0)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 0).unwrap();
    request_id
}

fn confirmed_sol_to_glc_payout(ledger: &mut Ledger, amount: u64, dest_addr: &str) -> (i64, u64) {
    let utxo = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic: amount + 100_000,
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, "51".to_string())], 1, 0)
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(amount),
            [7u8; 32],
            dest_addr.as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!("expected the deposit to fold directly to SourceFinalized")
    };

    // `amount` here is Solana-native (matches what `fold_sol_deposit` folds
    // directly from a real on-chain obligation); the payout record's own
    // `payout_atomic` is Goldcoin-native NET (after the bridge fee,
    // docs/20-bridge-fee.md), matching what
    // `signing::goldcoin_vault::rederive_plan` actually stores for a real
    // payout.
    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let payout_atomic = crate::amount_conversion::compute_fee(gross_canonical)
        .unwrap()
        .net
        .0;
    let plan = crate::goldcoin::payout::PayoutPlan {
        inputs: vec![],
        dest_p2pkh_hash: [0u8; 20],
        payout_atomic,
        change_atomic: 0,
        vault_script_pubkey: vec![],
        fee_atomic: 0,
    };
    ledger
        .record_goldcoin_payout_built(request_id, &plan, [0u8; 32], "00", 0)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, "00", 0)
        .unwrap();
    ledger
        .record_goldcoin_payout_broadcast(request_id, [0xBBu8; 32], 0)
        .unwrap();
    // tip_height 105, confirmations 6 => mined_height = 100.
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 105, 6, 0)
        .unwrap();
    (request_id, 0)
}

#[tokio::test]
async fn independently_attests_a_release_matching_the_golden_layout() {
    let mut ledger = ledger_with_both_reserves();
    let recipient = [9u8; 32];
    let request_id = finalized_glc_to_sol_request(&mut ledger, 500_000, recipient);

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(5, 2, &[signer.pubkey(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let (pubkey, signature, message) =
        independently_attest_release(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT)
            .await
            .unwrap();

    // `finalized_glc_to_sol_request` declared 500_000 Goldcoin-native
    // atomic units gross; the claim message must carry the NET (after the
    // 1% bridge fee, docs/20-bridge-fee.md) reserve-mint-live-decimal
    // equivalent, not the raw gross Goldcoin figure.
    let fb =
        crate::amount_conversion::compute_fee(crate::amount_conversion::CanonicalAtomic(500_000))
            .unwrap();
    let solana_amount = fb.net.to_solana(TEST_SOLANA_DECIMALS).unwrap().0;
    let expected = release_claim_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        5,
        &[0xAAu8; 32],
        2,
        solana_amount,
        &recipient,
        &[7u8; 32],
    );
    assert_eq!(message, expected);
    assert_eq!(pubkey, signer.pubkey());
    assert!(signature.verify(pubkey.as_ref(), &message));
}

#[tokio::test]
async fn two_independent_signers_re_derive_the_identical_release_message() {
    let mut ledger = ledger_with_both_reserves();
    let request_id = finalized_glc_to_sol_request(&mut ledger, 250_000, [3u8; 32]);
    let rpc = MockRpc::new();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(1, 2, &[Pubkey::new_unique(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([4u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([4u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let signer_a = DevAttestationSigner::generate();
    let signer_b = DevAttestationSigner::generate();
    let (pk_a, sig_a, msg_a) =
        independently_attest_release(&signer_a, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT)
            .await
            .unwrap();
    let (pk_b, sig_b, msg_b) =
        independently_attest_release(&signer_b, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT)
            .await
            .unwrap();

    assert_eq!(
        msg_a, msg_b,
        "independent re-derivation must be deterministic"
    );
    assert_ne!(pk_a, pk_b);
    assert!(sig_a.verify(pk_a.as_ref(), &msg_a));
    assert!(sig_b.verify(pk_b.as_ref(), &msg_b));
}

#[tokio::test]
async fn refuses_to_attest_a_release_that_is_not_yet_source_finalized() {
    let mut ledger = ledger_with_both_reserves();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            glc_to_sol_amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result =
        independently_attest_release(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT).await;
    assert!(matches!(
        result,
        Err(AttestationError::NotSourceFinalized(
            _,
            RequestState::AwaitingDeposit
        ))
    ));
}

#[tokio::test]
async fn refuses_to_attest_a_release_for_a_sol_to_glc_request() {
    let mut ledger = ledger_with_both_reserves();
    let (request_id, _) =
        confirmed_sol_to_glc_payout(&mut ledger, 100_000, "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef");
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result =
        independently_attest_release(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT).await;
    assert!(matches!(
        result,
        Err(AttestationError::WrongDirectionForRelease(_))
    ));
}

#[tokio::test]
async fn refuses_to_attest_a_release_for_a_request_that_does_not_exist() {
    let ledger = ledger_with_both_reserves();
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result =
        independently_attest_release(&signer, &ledger, &rpc, 999, TEST_SIGNER_TIMEOUT).await;
    assert!(matches!(
        result,
        Err(AttestationError::RequestNotFound(999))
    ));
}

#[tokio::test]
async fn independently_attests_a_completion_matching_the_golden_layout() {
    let mut ledger = ledger_with_both_reserves();
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (request_id, obligation_index) =
        confirmed_sol_to_glc_payout(&mut ledger, 500_000, dest_addr);

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(9, 2, &[signer.pubkey()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    rpc.set_account(
        accounts::withdrawal_obligation_pda(obligation_index),
        fake_withdrawal_obligation_bytes(obligation_index, 500_000, dest_addr.as_bytes()),
    );

    let (pubkey, signature, message) =
        independently_attest_completion(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT)
            .await
            .unwrap();

    // The obligation's on-chain amount (500_000, Solana-native) converts to
    // canonical, and the bridge fee is taken, to get the Goldcoin-native
    // NET figure the completion message must carry — the same value
    // `confirmed_sol_to_glc_payout` stored as `payout_atomic`
    // (docs/20-bridge-fee.md).
    let goldcoin_amount = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let dest_commitment: [u8; 32] = Sha256::digest(dest_addr.as_bytes())
        .as_slice()
        .try_into()
        .unwrap();
    let expected = goldcoin_completion_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        9,
        obligation_index,
        &[0xBBu8; 32],
        100,
        goldcoin_amount,
        &dest_commitment,
    );
    assert_eq!(message, expected);
    assert!(signature.verify(pubkey.as_ref(), &message));
}

#[tokio::test]
async fn refuses_to_attest_completion_before_the_goldcoin_payout_is_confirmed() {
    let mut ledger = ledger_with_both_reserves();
    let utxo = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic: 600_000,
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, "51".to_string())], 1, 0)
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            sol_to_glc_amounts(500_000),
            [7u8; 32],
            b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    let result =
        independently_attest_completion(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT)
            .await;
    assert!(matches!(result, Err(AttestationError::PayoutNotFound(_))));
}

#[tokio::test]
async fn refuses_to_attest_completion_when_onchain_amount_disagrees_with_the_recorded_payout() {
    let mut ledger = ledger_with_both_reserves();
    let dest_addr = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let (request_id, obligation_index) =
        confirmed_sol_to_glc_payout(&mut ledger, 500_000, dest_addr);

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(1, 2, &[signer.pubkey()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );
    // On-chain obligation disagrees with the recorded payout amount.
    rpc.set_account(
        accounts::withdrawal_obligation_pda(obligation_index),
        fake_withdrawal_obligation_bytes(obligation_index, 999_999, dest_addr.as_bytes()),
    );

    let expected_onchain = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(999_999)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let expected_recorded = crate::amount_conversion::compute_fee(
        crate::amount_conversion::SolanaAtomic(500_000)
            .to_canonical(TEST_SOLANA_DECIMALS)
            .unwrap(),
    )
    .unwrap()
    .net
    .0;
    let result =
        independently_attest_completion(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT)
            .await;
    assert!(matches!(
        result,
        Err(AttestationError::ObligationAmountMismatch {
            onchain,
            recorded,
            ..
        }) if onchain == expected_onchain && recorded == expected_recorded
    ));
}

#[tokio::test]
async fn attestation_fails_closed_when_the_stored_fee_has_been_tampered_with() {
    // Simulates a corrupted/tampered ledger row — e.g. an attempted direct-
    // database or direct-program-invocation fee bypass that manages to
    // alter what's stored without going through the normal fee-computing
    // call sites (docs/20-bridge-fee.md's fee-bypass-protection list).
    // `independently_attest_release` must recompute the fee from `gross`
    // itself and refuse to sign anything that disagrees, rather than ever
    // trusting the stored `fee_amount_atomic`/`net_amount_atomic` columns.
    let mut ledger = ledger_with_both_reserves();
    let recipient = [9u8; 32];
    let request_id = finalized_glc_to_sol_request(&mut ledger, 500_000, recipient);

    ledger
        .raw()
        .execute(
            "UPDATE bridge_requests SET fee_amount_atomic = 1, net_amount_atomic = 499999 \
             WHERE id = ?1",
            [request_id],
        )
        .unwrap();

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(5, 2, &[signer.pubkey(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let result =
        independently_attest_release(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT).await;
    assert!(
        matches!(
            result,
            Err(AttestationError::Conversion {
                source: amount_conversion::ConversionError::AccountingMismatch { .. },
                ..
            })
        ),
        "a tampered stored fee/net must never be independently attested to — got {result:?}"
    );
}

#[tokio::test]
async fn attestation_fails_closed_when_gross_ne_fee_plus_net() {
    // A subtler tamper: fee and net individually look plausible but no
    // longer sum to the recorded gross — the on-the-wire `gross ==
    // fee + net` invariant the user's spec calls out explicitly.
    let mut ledger = ledger_with_both_reserves();
    let recipient = [9u8; 32];
    let request_id = finalized_glc_to_sol_request(&mut ledger, 500_000, recipient);

    ledger
        .raw()
        .execute(
            "UPDATE bridge_requests SET net_amount_atomic = net_amount_atomic + 1 WHERE id = ?1",
            [request_id],
        )
        .unwrap();

    let rpc = MockRpc::new();
    let signer = DevAttestationSigner::generate();
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(5, 2, &[signer.pubkey(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let result =
        independently_attest_release(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT).await;
    assert!(matches!(
        result,
        Err(AttestationError::Conversion {
            source: amount_conversion::ConversionError::AccountingMismatch { .. },
            ..
        })
    ));
}

#[tokio::test]
async fn fails_closed_when_the_signer_itself_rejects() {
    let mut ledger = ledger_with_both_reserves();
    let recipient = [9u8; 32];
    let request_id = finalized_glc_to_sol_request(&mut ledger, 500_000, recipient);

    let rpc = MockRpc::new();
    let signer = FailingAttestationSigner {
        pubkey: Pubkey::new_unique(),
        behavior: FailingSignerBehavior::Reject,
    };
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(5, 2, &[signer.pubkey(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let result =
        independently_attest_release(&signer, &ledger, &rpc, request_id, TEST_SIGNER_TIMEOUT).await;
    assert!(
        matches!(
            result,
            Err(AttestationError::Signer(SignerError::Rejected { .. }))
        ),
        "a signer's own refusal must propagate as a hard error, never be silently treated as \
         success or skipped — got {result:?}"
    );
}

#[tokio::test]
async fn fails_closed_when_the_signer_never_responds() {
    let mut ledger = ledger_with_both_reserves();
    let recipient = [9u8; 32];
    let request_id = finalized_glc_to_sol_request(&mut ledger, 500_000, recipient);

    let rpc = MockRpc::new();
    let signer = FailingAttestationSigner {
        pubkey: Pubkey::new_unique(),
        behavior: FailingSignerBehavior::Hang,
    };
    rpc.set_account(
        accounts::attestation_key_set_pda(),
        fake_attestation_key_set_bytes(5, 2, &[signer.pubkey(), Pubkey::new_unique()]),
    );
    rpc.set_account(
        accounts::bridge_config_pda(),
        fake_bridge_config_bytes([7u8; 32], 0),
    );
    rpc.set_account(
        Pubkey::new_from_array([7u8; 32]),
        fake_mint_bytes(TEST_SOLANA_DECIMALS),
    );

    let result = independently_attest_release(
        &signer,
        &ledger,
        &rpc,
        request_id,
        std::time::Duration::from_millis(50),
    )
    .await;
    assert!(
        matches!(
            result,
            Err(AttestationError::Signer(SignerError::Timeout { .. }))
        ),
        "a hanging signer must be bounded by the configured timeout and fail closed, never \
         block settlement indefinitely — got {result:?}"
    );
}

/// docs/22-production-readiness-review.md P0-6: the attestation-message
/// domain separator this module signs against (`PROGRAM_ID` — imported
/// from `crate::solana::accounts`, the same constant every PDA helper
/// derives against) silently held the program's original scaffold/dev id
/// rather than its real deployed mainnet address for the entire life of
/// this codebase, until 2026-08-19. Pinned here directly, distinct from
/// `solana::accounts::tests::program_id_is_the_deployed_mainnet_address`,
/// so a regression that reintroduced a stale value specifically for
/// attestation signing (as opposed to PDA derivation) would still be
/// caught even if someone split the two uses onto separate constants in
/// the future.
#[test]
fn attestation_domain_separator_is_the_deployed_mainnet_address() {
    assert_eq!(
        PROGRAM_ID.to_bytes(),
        glc_reserve_bridge_shared::PROGRAM_ID_BYTES
    );
}
