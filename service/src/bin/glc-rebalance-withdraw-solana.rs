//! `glc-rebalance-withdraw-solana` — turnkey operator CLI for the Solana
//! `rebalance_withdraw` instruction: an intentional, operator-initiated
//! withdrawal of GLC-Solana from the reserve, without hand-assembling a
//! transaction.
//!
//! Three staged subcommands, mirroring `glc-rebalance-withdraw`'s
//! (Goldcoin) `plan -> sign -> broadcast` shape for the same reason: no
//! single invocation should ever need every credential at once, and each
//! stage re-verifies live chain state rather than trusting the previous
//! stage's file blindly.
//!
//!   plan    — no key needed. Reads live on-chain state (BridgeConfig,
//!             AttestationKeySet, reserve balance, destination account),
//!             verifies the reserve mint/token program/pause state/
//!             protected-minimum/nonce-not-reused, derives (never
//!             accepts as input) the reserve authority PDA and reserve
//!             token account, builds the canonical domain-separated
//!             claim message, and writes a plan file. This step IS a
//!             dry run: nothing is signed.
//!   attest  — no local private key. Re-verifies the plan file has not
//!             been tampered with (recomputes the PDAs/message from the
//!             plan's own primitive fields) and that live chain state
//!             still supports it, then contacts >= threshold configured
//!             remote attestation-signer endpoints via the EXISTING
//!             production remote-signer client
//!             (`signing::remote::RemoteAttestationSigner`) — the same
//!             architecture `service::orchestrator`'s automated release
//!             path uses. No attestation private key ever exists on this
//!             host.
//!   execute — needs the local admin and submitter/fee-payer keypairs
//!             only (never attestation/vault keys — see module docs on
//!             `signing::remote` for why those never belong on this
//!             host). Re-verifies live state a third time, builds the
//!             ed25519-precompile + `rebalance_withdraw` transaction,
//!             ALWAYS simulates it first, prints a full summary
//!             (addresses, amount, resulting reserve balance, signer
//!             count, simulation outcome), and broadcasts ONLY if
//!             `--execute` is explicitly supplied AND the simulation
//!             succeeded. Without `--execute`, this is still a dry run.
//!
//! Fee payer is always the submitter — never the reserve authority PDA
//! (which cannot sign anything; it has no keypair by construction). The
//! `rebalance_withdrawal` record's rent is paid by `admin` (the same
//! account the on-chain instruction requires to co-sign), per
//! `programs/glc-reserve-bridge/src/instructions/rebalance_withdraw.rs`'s
//! own account context.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signature, Signer};
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::signing::remote::{RemoteAttestationSigner, RemoteSignerConfig};
use glc_reserve_bridge_service::signing::signers::AttestationSigner;
use glc_reserve_bridge_service::solana::accounts::{self, PROGRAM_ID};
use glc_reserve_bridge_service::solana::ed25519;
use glc_reserve_bridge_service::solana::instructions;
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

const PROTOCOL_VERSION: u8 = 1;

const USAGE: &str =
    "glc-rebalance-withdraw-solana — turnkey Solana reserve withdrawal, operator CLI

Three staged subcommands so no single invocation needs every credential at
once — see this file's own module docs for why.

PLAN (no key needed; reads and verifies live chain state; this step IS the dry run)
  glc-rebalance-withdraw-solana plan \\
      --rpc-url URL --destination PUBKEY --amount N --nonce N \\
      [--reserve-mint PUBKEY] [--token-program PUBKEY] \\
      --out plan.json

ATTEST (no local private key; contacts remote signer endpoints)
  glc-rebalance-withdraw-solana attest \\
      --plan plan.json --rpc-url URL \\
      --attestation-signer PUBKEY,https://URL,AUTH_TOKEN_ENV_VAR[,TIMEOUT_MS] \\
      [--attestation-signer ... (repeat, >= threshold)] \\
      --out attested-plan.json

EXECUTE (needs only admin + submitter keypairs; always simulates first)
  glc-rebalance-withdraw-solana execute \\
      --attested-plan attested-plan.json --rpc-url URL \\
      --admin-keypair PATH --submitter-keypair PATH \\
      [--execute]

  --execute
      Without this flag, execute builds, verifies, and simulates the
      transaction and prints the result — nothing is broadcast. With it,
      the transaction is also submitted, but ONLY if the simulation that
      just ran succeeded.

See RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md for the full operator
procedure, including how to withdraw the entire withdrawable reserve.";

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn flags_all<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.iter()
        .zip(args.iter().skip(1))
        .filter(|(a, _)| *a == name)
        .map(|(_, v)| v.as_str())
        .collect()
}

fn require<'a>(args: &'a [String], name: &str) -> Result<&'a str, String> {
    flag(args, name).ok_or_else(|| format!("missing required {name}"))
}

fn require_pubkey(args: &[String], name: &str) -> Result<Pubkey, String> {
    let raw = require(args, name)?;
    Pubkey::from_str(raw).map_err(|e| format!("{name} {raw:?} is not a valid pubkey: {e}"))
}

fn require_u64(args: &[String], name: &str) -> Result<u64, String> {
    require(args, name)?
        .parse()
        .map_err(|e| format!("{name} must be a non-negative integer: {e}"))
}

fn optional_pubkey(args: &[String], name: &str) -> Result<Option<Pubkey>, String> {
    match flag(args, name) {
        None => Ok(None),
        Some(raw) => {
            Ok(Some(Pubkey::from_str(raw).map_err(|e| {
                format!("{name} {raw:?} is not a valid pubkey: {e}")
            })?))
        }
    }
}

// ------------------------------------------------------------- file formats --

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PlanFile {
    program_id: String,
    reserve_mint: String,
    token_program: String,
    reserve_authority: String,
    reserve_token_account: String,
    destination_token_account: String,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
    attestation_threshold: u8,
    protected_minimum: u64,
    reserve_balance_before: u64,
    message_hex: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AttestationEntry {
    pubkey: String,
    signature_hex: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AttestedPlanFile {
    plan: PlanFile,
    attestations: Vec<AttestationEntry>,
}

// ------------------------------------------------------ pure verification --
//
// Every function below takes already-fetched/decoded values, does no I/O,
// and is directly unit-testable without a live RPC or network.

#[derive(Debug, PartialEq, Eq)]
enum VerifyError {
    ReserveNotConfigured,
    ReserveMintMismatch { expected: Pubkey, actual: Pubkey },
    TokenProgramMismatch { expected: Pubkey, actual: Pubkey },
    BridgeNotPaused,
    ProtectedMinimumViolation { available: u64, requested: u64 },
    NonceAlreadyUsed { nonce: u64 },
    DestinationWrongMint { expected: Pubkey, actual: Pubkey },
    DestinationWrongOwner { expected: Pubkey, actual: Pubkey },
    ZeroAmount,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::ReserveNotConfigured => {
                write!(f, "reserve vault is not configured on this deployment (reserve_token_mint is the default/zero pubkey)")
            }
            VerifyError::ReserveMintMismatch { expected, actual } => write!(
                f,
                "reserve mint mismatch: live on-chain BridgeConfig has {expected}, but {actual} was supplied/expected"
            ),
            VerifyError::TokenProgramMismatch { expected, actual } => write!(
                f,
                "token program mismatch: live on-chain BridgeConfig has {expected}, but {actual} was supplied/expected"
            ),
            VerifyError::BridgeNotPaused => write!(
                f,
                "REFUSING — live on-chain BridgeConfig.paused is false. Pause the bridge first: \
                 glc-admin onchain-pause --scope global ..."
            ),
            VerifyError::ProtectedMinimumViolation {
                available,
                requested,
            } => write!(
                f,
                "REFUSING — withdrawing {requested} would breach protected_minimum; only \
                 {available} is available above the floor"
            ),
            VerifyError::NonceAlreadyUsed { nonce } => write!(
                f,
                "REFUSING — nonce {nonce} has already been used for a rebalance withdrawal \
                 (the rebalance_withdrawal PDA for it already exists on chain)"
            ),
            VerifyError::DestinationWrongMint { expected, actual } => write!(
                f,
                "REFUSING — destination token account's mint is {actual}, expected {expected} \
                 (the configured reserve mint)"
            ),
            VerifyError::DestinationWrongOwner { expected, actual } => write!(
                f,
                "REFUSING — destination token account is owned by program {actual}, expected \
                 {expected} (the configured reserve token program)"
            ),
            VerifyError::ZeroAmount => write!(f, "amount must be greater than zero"),
        }
    }
}

/// Verifies the reserve is configured and, if the operator supplied
/// `--reserve-mint`/`--token-program` as an explicit expectation, that it
/// matches live on-chain state exactly — never blindly trusting either
/// the operator's flag or the chain alone.
fn verify_reserve_configuration(
    live_reserve_mint: Pubkey,
    live_token_program: Pubkey,
    expected_reserve_mint: Option<Pubkey>,
    expected_token_program: Option<Pubkey>,
) -> Result<(), VerifyError> {
    if live_reserve_mint == Pubkey::default() || live_token_program == Pubkey::default() {
        return Err(VerifyError::ReserveNotConfigured);
    }
    if let Some(expected) = expected_reserve_mint {
        if expected != live_reserve_mint {
            return Err(VerifyError::ReserveMintMismatch {
                expected: live_reserve_mint,
                actual: expected,
            });
        }
    }
    if let Some(expected) = expected_token_program {
        if expected != live_token_program {
            return Err(VerifyError::TokenProgramMismatch {
                expected: live_token_program,
                actual: expected,
            });
        }
    }
    Ok(())
}

/// The core pre-flight safety check every stage (plan, attest, execute)
/// re-runs against its own freshly-fetched live state — mirrors exactly
/// what the on-chain instruction itself enforces
/// (`BridgeError::BridgeNotPaused`/`InsufficientReserveBalance`), so a
/// violation is caught here, clearly, before ever contacting a signer or
/// the network, not discovered only as an opaque on-chain rejection.
fn verify_withdrawal_is_currently_valid(
    amount: u64,
    paused: bool,
    reserve_balance: u64,
    protected_minimum: u64,
    nonce_pda_exists: bool,
    nonce: u64,
) -> Result<(), VerifyError> {
    if amount == 0 {
        return Err(VerifyError::ZeroAmount);
    }
    if !paused {
        return Err(VerifyError::BridgeNotPaused);
    }
    let required_floor = amount.saturating_add(protected_minimum);
    if reserve_balance < required_floor {
        return Err(VerifyError::ProtectedMinimumViolation {
            available: reserve_balance.saturating_sub(protected_minimum),
            requested: amount,
        });
    }
    if nonce_pda_exists {
        return Err(VerifyError::NonceAlreadyUsed { nonce });
    }
    Ok(())
}

/// Verifies the destination token account actually belongs to the
/// reserve mint and is owned by the configured token program — this is
/// what structurally rules out "wrong mint" and "wrong token program"
/// destinations, independent of whatever the on-chain instruction's own
/// account constraints would also catch.
fn verify_destination(
    destination_mint: Pubkey,
    destination_owner_program: Pubkey,
    expected_mint: Pubkey,
    expected_token_program: Pubkey,
) -> Result<(), VerifyError> {
    if destination_mint != expected_mint {
        return Err(VerifyError::DestinationWrongMint {
            expected: expected_mint,
            actual: destination_mint,
        });
    }
    if destination_owner_program != expected_token_program {
        return Err(VerifyError::DestinationWrongOwner {
            expected: expected_token_program,
            actual: destination_owner_program,
        });
    }
    Ok(())
}

/// Recomputes every derived field in `plan` from its own primitive
/// fields (reserve_mint, token_program, nonce, amount, destination,
/// attestation_epoch) and refuses if the plan file's RECORDED values
/// don't match — the same tamper-detection discipline
/// `glc-rebalance-withdraw`'s `sign` subcommand applies to the Goldcoin
/// plan file, applied here to the reserve authority PDA, the reserve
/// token account, and the canonical claim message. A plan file is
/// evidence to be independently re-checked, never a trusted instruction.
fn verify_plan_not_tampered(plan: &PlanFile) -> Result<(), String> {
    let reserve_mint = Pubkey::from_str(&plan.reserve_mint)
        .map_err(|e| format!("plan file reserve_mint is invalid: {e}"))?;
    let token_program = Pubkey::from_str(&plan.token_program)
        .map_err(|e| format!("plan file token_program is invalid: {e}"))?;
    let destination = Pubkey::from_str(&plan.destination_token_account)
        .map_err(|e| format!("plan file destination_token_account is invalid: {e}"))?;
    let program_id = Pubkey::from_str(&plan.program_id)
        .map_err(|e| format!("plan file program_id is invalid: {e}"))?;
    if program_id != PROGRAM_ID {
        return Err(format!(
            "plan file program_id {program_id} does not match this build's compiled-in program \
             id {PROGRAM_ID} — refusing to proceed with a plan built for a different program"
        ));
    }

    let expected_reserve_authority = accounts::reserve_authority_pda();
    if plan.reserve_authority != expected_reserve_authority.to_string() {
        return Err(format!(
            "TAMPER DETECTED — plan file's recorded reserve_authority ({}) does not match the \
             freshly re-derived PDA ({expected_reserve_authority})",
            plan.reserve_authority
        ));
    }
    let expected_reserve_token_account = accounts::associated_token_address(
        &expected_reserve_authority,
        &reserve_mint,
        &token_program,
    );
    if plan.reserve_token_account != expected_reserve_token_account.to_string() {
        return Err(format!(
            "TAMPER DETECTED — plan file's recorded reserve_token_account ({}) does not match \
             the freshly re-derived address ({expected_reserve_token_account})",
            plan.reserve_token_account
        ));
    }

    let expected_message = glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        plan.attestation_epoch,
        plan.nonce,
        plan.amount,
        &destination.to_bytes(),
        &reserve_mint.to_bytes(),
    );
    let expected_message_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&expected_message);
    if plan.message_hex != expected_message_hex {
        return Err(
            "TAMPER DETECTED — plan file's recorded message_hex does not match what its own \
             recorded nonce/amount/destination/mint/epoch actually produce; the file may be \
             corrupted or tampered with"
                .to_string(),
        );
    }
    Ok(())
}

/// Filters `entries` down to only those that are (a) a current attestation
/// key and (b) carry a signature that actually verifies against that key
/// and `message`, deduplicating by pubkey — the same discipline
/// `programs/glc-reserve-bridge/src/verification.rs::count_unique_attestation_signers`
/// enforces on-chain, run here client-side so a bad entry is caught with a
/// clear message before ever being submitted. Fails if fewer than
/// `threshold` valid, unique signatures remain.
fn collect_valid_attestations(
    entries: &[AttestationEntry],
    message: &[u8],
    current_keys: &[Pubkey],
    threshold: u8,
) -> Result<Vec<(Pubkey, Signature)>, String> {
    let mut valid: Vec<(Pubkey, Signature)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let pubkey = Pubkey::from_str(&entry.pubkey).map_err(|e| {
            format!(
                "attestation entry has an invalid pubkey {:?}: {e}",
                entry.pubkey
            )
        })?;
        if !current_keys.contains(&pubkey) {
            eprintln!(
                "warning: ignoring attestation from {pubkey} — not a current attestation key"
            );
            continue;
        }
        if !seen.insert(pubkey) {
            eprintln!("warning: ignoring duplicate attestation from {pubkey}");
            continue;
        }
        let sig_bytes = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&entry.signature_hex)
            .map_err(|e| {
                format!("attestation entry from {pubkey} has invalid signature hex: {e}")
            })?;
        let sig_array: [u8; 64] = sig_bytes.as_slice().try_into().map_err(|_| {
            format!(
                "attestation entry from {pubkey} has a {}-byte signature, expected 64",
                sig_bytes.len()
            )
        })?;
        let signature = Signature::from(sig_array);
        if !signature.verify(pubkey.as_ref(), message) {
            eprintln!(
                "warning: ignoring attestation from {pubkey} — signature does not verify \
                 against the expected claim message (SIGNER MISMATCH)"
            );
            continue;
        }
        valid.push((pubkey, signature));
    }
    if valid.len() < threshold as usize {
        return Err(format!(
            "only {} valid attestation signature(s) collected, but the plan requires a \
             threshold of {threshold}",
            valid.len()
        ));
    }
    Ok(valid)
}

/// Builds the two-instruction transaction (ed25519 proof, then
/// `rebalance_withdraw`) — pure construction, no signing, no I/O.
fn build_instructions(
    plan: &PlanFile,
    attestations: &[(Pubkey, Signature)],
    admin: &Pubkey,
) -> Result<Vec<Instruction>, String> {
    let reserve_mint = Pubkey::from_str(&plan.reserve_mint).map_err(|e| e.to_string())?;
    let token_program = Pubkey::from_str(&plan.token_program).map_err(|e| e.to_string())?;
    let destination =
        Pubkey::from_str(&plan.destination_token_account).map_err(|e| e.to_string())?;
    let message = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex)
        .map_err(|e| e.to_string())?;

    let ed25519_ix = ed25519::build_attestation_proof(attestations, &message);
    let rebalance_ix = instructions::rebalance_withdraw(
        admin,
        &reserve_mint,
        &token_program,
        &destination,
        plan.nonce,
        plan.amount,
        plan.attestation_epoch,
    );
    Ok(vec![ed25519_ix, rebalance_ix])
}

// ------------------------------------------------------------------ main --

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let Some(cmd) = args.get(1) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let rt = tokio::runtime::Runtime::new().expect("could not start async runtime");
    let result = match cmd.as_str() {
        "plan" => rt.block_on(cmd_plan(&args)),
        "attest" => rt.block_on(cmd_attest(&args)),
        "execute" => rt.block_on(cmd_execute(&args)),
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn fetch_and_decode_token_account<R: SolanaRpc>(
    rpc: &R,
    pubkey: &Pubkey,
    label: &str,
) -> Result<(Pubkey, Pubkey, u64), String> {
    let account = rpc
        .get_account(pubkey)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{label} {pubkey} does not exist on this cluster"))?;
    let mint = accounts::decode_token_account_mint(&account.data).map_err(|e| e.to_string())?;
    let amount = accounts::decode_token_account_amount(&account.data).map_err(|e| e.to_string())?;
    Ok((mint, account.owner, amount))
}

// -------------------------------------------------------------------- plan --

async fn cmd_plan(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url")?.to_string();
    let destination = require_pubkey(args, "--destination")?;
    let amount = require_u64(args, "--amount")?;
    let nonce = require_u64(args, "--nonce")?;
    let expected_reserve_mint = optional_pubkey(args, "--reserve-mint")?;
    let expected_token_program = optional_pubkey(args, "--token-program")?;
    let out_path = PathBuf::from(require(args, "--out")?);

    let rpc = RealSolanaRpc::new(rpc_url);

    let config_pda = accounts::bridge_config_pda();
    let config_account = rpc
        .get_account(&config_pda)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("bridge_config does not exist at {config_pda} — not initialized"))?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;

    verify_reserve_configuration(
        config.reserve_token_mint,
        config.reserve_token_program,
        expected_reserve_mint,
        expected_token_program,
    )
    .map_err(|e| e.to_string())?;

    let key_set_pda = accounts::attestation_key_set_pda();
    let key_set_account = rpc
        .get_account(&key_set_pda)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("attestation_key_set does not exist at {key_set_pda}"))?;
    let key_set =
        accounts::decode_attestation_key_set(&key_set_account.data).map_err(|e| e.to_string())?;

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_token_account = accounts::associated_token_address(
        &reserve_authority,
        &config.reserve_token_mint,
        &config.reserve_token_program,
    );
    let (_, _, reserve_balance) =
        fetch_and_decode_token_account(&rpc, &reserve_token_account, "reserve token account")
            .await?;

    let (dest_mint, dest_owner, _dest_balance) =
        fetch_and_decode_token_account(&rpc, &destination, "destination token account").await?;
    verify_destination(
        dest_mint,
        dest_owner,
        config.reserve_token_mint,
        config.reserve_token_program,
    )
    .map_err(|e| e.to_string())?;

    let rebalance_withdrawal_pda = accounts::rebalance_withdrawal_pda(nonce);
    let nonce_pda_exists = rpc
        .get_account(&rebalance_withdrawal_pda)
        .await
        .map_err(|e| e.to_string())?
        .is_some();

    verify_withdrawal_is_currently_valid(
        amount,
        config.paused,
        reserve_balance,
        config.protected_minimum,
        nonce_pda_exists,
        nonce,
    )
    .map_err(|e| e.to_string())?;

    let message = glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        key_set.epoch,
        nonce,
        amount,
        &destination.to_bytes(),
        &config.reserve_token_mint.to_bytes(),
    );

    let plan = PlanFile {
        program_id: PROGRAM_ID.to_string(),
        reserve_mint: config.reserve_token_mint.to_string(),
        token_program: config.reserve_token_program.to_string(),
        reserve_authority: reserve_authority.to_string(),
        reserve_token_account: reserve_token_account.to_string(),
        destination_token_account: destination.to_string(),
        nonce,
        amount,
        attestation_epoch: key_set.epoch,
        attestation_threshold: key_set.threshold,
        protected_minimum: config.protected_minimum,
        reserve_balance_before: reserve_balance,
        message_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&message),
    };
    let json = serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?;
    std::fs::write(&out_path, &json).map_err(|e| format!("writing {out_path:?}: {e}"))?;

    println!("Plan written to {}", out_path.display());
    println!("  program id                = {PROGRAM_ID}");
    println!(
        "  reserve mint              = {}",
        config.reserve_token_mint
    );
    println!(
        "  token program             = {}",
        config.reserve_token_program
    );
    println!("  reserve authority (PDA)   = {reserve_authority}");
    println!("  reserve token account     = {reserve_token_account}");
    println!("  destination token account = {destination}");
    println!("  amount                    = {amount}");
    println!("  reserve balance (before)  = {reserve_balance}");
    println!("  reserve balance (after)   = {}", reserve_balance - amount);
    println!("  protected_minimum         = {}", config.protected_minimum);
    println!("  bridge paused             = {}", config.paused);
    println!(
        "  attestation threshold     = {} of {}",
        key_set.threshold,
        key_set.keys.len()
    );
    println!("\nThis plan is NOT signed and NOT broadcast. Run `attest` next.");
    Ok(())
}

// ------------------------------------------------------------------ attest --

async fn cmd_attest(args: &[String]) -> Result<(), String> {
    let plan_path = PathBuf::from(require(args, "--plan")?);
    let rpc_url = require(args, "--rpc-url")?.to_string();
    let out_path = PathBuf::from(require(args, "--out")?);
    let signer_specs = flags_all(args, "--attestation-signer");
    if signer_specs.is_empty() {
        return Err("at least one --attestation-signer is required".to_string());
    }

    let plan_json = std::fs::read_to_string(&plan_path)
        .map_err(|e| format!("reading plan file {plan_path:?}: {e}"))?;
    let plan: PlanFile = serde_json::from_str(&plan_json)
        .map_err(|e| format!("plan file {plan_path:?} is not valid: {e}"))?;
    verify_plan_not_tampered(&plan)?;

    // Re-verify live state before ever contacting a signer endpoint — a
    // stale or now-invalid plan should not waste a real network round
    // trip to a custody domain, let alone collect a signature over it.
    let rpc = RealSolanaRpc::new(rpc_url);
    let reserve_mint = Pubkey::from_str(&plan.reserve_mint).map_err(|e| e.to_string())?;
    let token_program = Pubkey::from_str(&plan.token_program).map_err(|e| e.to_string())?;
    let reserve_token_account =
        Pubkey::from_str(&plan.reserve_token_account).map_err(|e| e.to_string())?;
    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config does not exist")?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
    verify_reserve_configuration(
        config.reserve_token_mint,
        config.reserve_token_program,
        Some(reserve_mint),
        Some(token_program),
    )
    .map_err(|e| e.to_string())?;
    let (_, _, reserve_balance) =
        fetch_and_decode_token_account(&rpc, &reserve_token_account, "reserve token account")
            .await?;
    let nonce_pda_exists = rpc
        .get_account(&accounts::rebalance_withdrawal_pda(plan.nonce))
        .await
        .map_err(|e| e.to_string())?
        .is_some();
    verify_withdrawal_is_currently_valid(
        plan.amount,
        config.paused,
        reserve_balance,
        config.protected_minimum,
        nonce_pda_exists,
        plan.nonce,
    )
    .map_err(|e| e.to_string())?;

    let message = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex)
        .map_err(|e| e.to_string())?;

    let mut attestations = Vec::new();
    for spec in &signer_specs {
        let parts: Vec<&str> = spec.splitn(4, ',').collect();
        if parts.len() < 3 {
            return Err(format!(
                "--attestation-signer {spec:?} must be PUBKEY,URL,AUTH_TOKEN_ENV[,TIMEOUT_MS]"
            ));
        }
        let expected_pubkey = Pubkey::from_str(parts[0])
            .map_err(|e| format!("--attestation-signer pubkey {:?}: {e}", parts[0]))?;
        let timeout_ms: u64 = parts
            .get(3)
            .map(|s| s.parse())
            .transpose()
            .map_err(|e| format!("--attestation-signer timeout_ms: {e}"))?
            .unwrap_or(10_000);
        let config = RemoteSignerConfig {
            endpoint_url: parts[1].to_string(),
            auth_token_env: parts[2].to_string(),
            timeout: Duration::from_millis(timeout_ms),
        };
        match RemoteAttestationSigner::connect(&config, expected_pubkey).await {
            Ok(signer) => {
                match tokio::time::timeout(
                    Duration::from_millis(timeout_ms),
                    signer.sign_message(&message),
                )
                .await
                {
                    Ok(Ok(signature)) => {
                        println!("  signed by {expected_pubkey}");
                        attestations.push(AttestationEntry {
                            pubkey: expected_pubkey.to_string(),
                            signature_hex: glc_reserve_bridge_service::goldcoin::hex::encode(
                                signature.as_ref(),
                            ),
                        });
                    }
                    Ok(Err(e)) => {
                        eprintln!("warning: signer {expected_pubkey} refused/failed: {e}")
                    }
                    Err(_) => eprintln!("warning: signer {expected_pubkey} timed out"),
                }
            }
            Err(e) => eprintln!("warning: could not connect to signer {expected_pubkey}: {e}"),
        }
    }

    if attestations.len() < plan.attestation_threshold as usize {
        return Err(format!(
            "only {} of the required {} attestation signatures were collected",
            attestations.len(),
            plan.attestation_threshold
        ));
    }

    let attested = AttestedPlanFile {
        plan: plan.clone(),
        attestations,
    };
    let json = serde_json::to_string_pretty(&attested).map_err(|e| e.to_string())?;
    std::fs::write(&out_path, &json).map_err(|e| format!("writing {out_path:?}: {e}"))?;
    println!(
        "\nAttested plan written to {} — {} of {} required signatures collected. Run `execute` next.",
        out_path.display(),
        attested.attestations.len(),
        plan.attestation_threshold
    );
    Ok(())
}

// ----------------------------------------------------------------- execute --

async fn cmd_execute(args: &[String]) -> Result<(), String> {
    let attested_path = PathBuf::from(require(args, "--attested-plan")?);
    let rpc_url = require(args, "--rpc-url")?.to_string();
    let admin_keypair_path = require(args, "--admin-keypair")?;
    let submitter_keypair_path = require(args, "--submitter-keypair")?;
    let do_execute = args.iter().any(|a| a == "--execute");

    let json = std::fs::read_to_string(&attested_path)
        .map_err(|e| format!("reading {attested_path:?}: {e}"))?;
    let attested: AttestedPlanFile =
        serde_json::from_str(&json).map_err(|e| format!("{attested_path:?} is not valid: {e}"))?;

    let admin = read_keypair_file(admin_keypair_path)
        .map_err(|e| format!("reading admin keypair {admin_keypair_path:?}: {e}"))?;
    let submitter = read_keypair_file(submitter_keypair_path)
        .map_err(|e| format!("reading submitter keypair {submitter_keypair_path:?}: {e}"))?;

    let rpc = RealSolanaRpc::new(rpc_url);
    let outcome = execute_withdrawal(&rpc, &attested, &admin, &submitter, do_execute).await?;
    debug_assert!(
        outcome.simulation_succeeded,
        "execute_withdrawal only ever returns Ok after a successful simulation"
    );
    if outcome.broadcast {
        println!("Broadcast: {}", outcome.signature.unwrap());
    } else {
        println!("\n--execute not supplied — this was a dry run. Nothing was broadcast.");
    }
    Ok(())
}

/// Outcome of a single execute attempt — exposed as a typed result (not
/// just printed output) specifically so `cmd_execute` and the offline
/// tests below can both assert on it: whether simulation succeeded and,
/// separately, whether a broadcast actually happened.
#[derive(Debug)]
struct ExecuteOutcome {
    simulation_succeeded: bool,
    broadcast: bool,
    signature: Option<String>,
}

/// The full verify -> build -> simulate -> (maybe) broadcast pipeline,
/// generic over [`SolanaRpc`] so it can run against a real cluster
/// (`cmd_execute`) or an in-memory mock (tests). Never broadcasts unless
/// `do_execute` is true AND simulation just succeeded — both checked
/// here, in this order, not left to the caller to get right.
async fn execute_withdrawal<R: SolanaRpc>(
    rpc: &R,
    attested: &AttestedPlanFile,
    admin: &solana_sdk::signature::Keypair,
    submitter: &solana_sdk::signature::Keypair,
    do_execute: bool,
) -> Result<ExecuteOutcome, String> {
    let plan = &attested.plan;
    verify_plan_not_tampered(plan)?;

    let reserve_mint = Pubkey::from_str(&plan.reserve_mint).map_err(|e| e.to_string())?;
    let token_program = Pubkey::from_str(&plan.token_program).map_err(|e| e.to_string())?;
    let reserve_token_account =
        Pubkey::from_str(&plan.reserve_token_account).map_err(|e| e.to_string())?;
    let destination =
        Pubkey::from_str(&plan.destination_token_account).map_err(|e| e.to_string())?;

    // Third independent live-state check, immediately before building the
    // real transaction — see module docs.
    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config does not exist")?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
    verify_reserve_configuration(
        config.reserve_token_mint,
        config.reserve_token_program,
        Some(reserve_mint),
        Some(token_program),
    )
    .map_err(|e| e.to_string())?;
    let key_set_account = rpc
        .get_account(&accounts::attestation_key_set_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("attestation_key_set does not exist")?;
    let key_set =
        accounts::decode_attestation_key_set(&key_set_account.data).map_err(|e| e.to_string())?;
    let (_, _, reserve_balance) =
        fetch_and_decode_token_account(rpc, &reserve_token_account, "reserve token account")
            .await?;
    let nonce_pda_exists = rpc
        .get_account(&accounts::rebalance_withdrawal_pda(plan.nonce))
        .await
        .map_err(|e| e.to_string())?
        .is_some();
    verify_withdrawal_is_currently_valid(
        plan.amount,
        config.paused,
        reserve_balance,
        config.protected_minimum,
        nonce_pda_exists,
        plan.nonce,
    )
    .map_err(|e| e.to_string())?;

    let message = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex)
        .map_err(|e| e.to_string())?;
    let attestations = collect_valid_attestations(
        &attested.attestations,
        &message,
        &key_set.keys,
        key_set.threshold,
    )?;

    let instructions = build_instructions(plan, &attestations, &admin.pubkey())?;

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&submitter.pubkey()),
        &[submitter, admin],
        blockhash,
    );

    let admin_lamports = rpc
        .get_account(&admin.pubkey())
        .await
        .map_err(|e| e.to_string())?
        .map(|a| a.lamports)
        .unwrap_or(0);
    let submitter_lamports = rpc
        .get_account(&submitter.pubkey())
        .await
        .map_err(|e| e.to_string())?
        .map(|a| a.lamports)
        .unwrap_or(0);

    println!("Transaction summary:");
    println!("  program id                = {PROGRAM_ID}");
    println!("  reserve mint              = {reserve_mint}");
    println!("  token program             = {token_program}");
    println!("  reserve authority (PDA)   = {}", plan.reserve_authority);
    println!("  reserve token account     = {reserve_token_account}");
    println!("  destination token account = {destination}");
    println!("  amount                    = {}", plan.amount);
    println!("  reserve balance (before)  = {reserve_balance}");
    println!(
        "  reserve balance (after)   = {}",
        reserve_balance - plan.amount
    );
    println!(
        "  attestation signers used  = {} of {} required",
        attestations.len(),
        key_set.threshold
    );
    println!(
        "  admin       = {} ({admin_lamports} lamports)",
        admin.pubkey()
    );
    println!(
        "  submitter (fee payer) = {} ({submitter_lamports} lamports) — NOT the reserve PDA",
        submitter.pubkey()
    );
    println!(
        "  estimated cost: ~10000 lamports transaction fee (2 signatures) from submitter; a \
         small rent-exempt deposit (RebalanceWithdrawal record, well under 0.01 SOL) from admin"
    );

    let simulation = rpc
        .simulate_transaction(&tx)
        .await
        .map_err(|e| e.to_string())?;
    println!("\nSimulation result:");
    if let Some(err) = &simulation.err {
        println!("  FAILED: {err}");
        for log in &simulation.logs {
            println!("    {log}");
        }
        return Err(
            "simulation failed — refusing to broadcast, --execute is ignored even if supplied"
                .to_string(),
        );
    }
    println!(
        "  succeeded (units consumed: {:?})",
        simulation.units_consumed
    );
    for log in &simulation.logs {
        println!("    {log}");
    }

    if !do_execute {
        return Ok(ExecuteOutcome {
            simulation_succeeded: true,
            broadcast: false,
            signature: None,
        });
    }

    println!("\n--execute supplied and simulation succeeded — broadcasting...");
    let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
    Ok(ExecuteOutcome {
        simulation_succeeded: true,
        broadcast: true,
        signature: Some(signature.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glc_reserve_bridge_service::solana::rpc::{SimulationOutcome, SolanaRpcError};
    use solana_sdk::account::Account;
    use solana_sdk::hash::Hash;
    use solana_sdk::signature::{Keypair, Signature as Sig};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory `SolanaRpc` for exercising the full `execute_withdrawal`
    /// pipeline end to end (plan verification, live-state checks,
    /// transaction construction, simulation, and the broadcast gate)
    /// without any real network or cluster.
    struct MockRpc {
        accounts: HashMap<Pubkey, Account>,
        simulate_err: Option<String>,
        sent: Mutex<Vec<Transaction>>,
    }

    impl SolanaRpc for MockRpc {
        async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
            Ok(self.accounts.get(pubkey).cloned())
        }
        async fn get_multiple_accounts(
            &self,
            _pubkeys: &[Pubkey],
        ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
            unimplemented!("not exercised by these tests")
        }
        async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
            Ok(1)
        }
        async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
            Ok(Hash::new_unique())
        }
        async fn send_transaction(&self, tx: &Transaction) -> Result<Sig, SolanaRpcError> {
            self.sent.lock().unwrap().push(tx.clone());
            Ok(tx.signatures[0])
        }
        async fn simulate_transaction(
            &self,
            _tx: &Transaction,
        ) -> Result<SimulationOutcome, SolanaRpcError> {
            Ok(SimulationOutcome {
                err: self.simulate_err.clone(),
                logs: vec!["Program log: simulated".to_string()],
                units_consumed: Some(12_345),
            })
        }
        async fn get_signature_status(
            &self,
            _signature: &Sig,
        ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
            unimplemented!("not exercised by these tests")
        }
        async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn fake_token_account(mint: Pubkey, owner_program: Pubkey, amount: u64) -> Account {
        let mut data = vec![0u8; 165];
        data[0..32].copy_from_slice(mint.as_ref());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        Account {
            lamports: 1,
            data,
            owner: owner_program,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Matches `accounts::decode_bridge_config`'s exact byte offsets.
    fn fake_bridge_config_account(
        paused: bool,
        reserve_mint: Pubkey,
        token_program: Pubkey,
        protected_minimum: u64,
    ) -> Account {
        let mut data = vec![0u8; 8]; // discriminator
        data.push(1); // protocol_version
        data.extend_from_slice(Pubkey::new_unique().as_ref()); // admin
        data.push(0); // pending_admin tag (None)
        data.push(paused as u8);
        data.push(0); // release_paused
        data.push(0); // deposit_paused
        data.push(0); // bump
        data.extend_from_slice(reserve_mint.as_ref());
        data.extend_from_slice(token_program.as_ref());
        data.push(0); // reserve_authority_bump
        data.extend_from_slice(&0u64.to_le_bytes()); // obligation_count
        data.extend_from_slice(&3600i64.to_le_bytes()); // governance_timelock_seconds
        data.extend_from_slice(&100u64.to_le_bytes()); // min_transfer_amount
        data.extend_from_slice(&10_000_000u64.to_le_bytes()); // per_transfer_limit
        data.extend_from_slice(&protected_minimum.to_le_bytes());
        Account {
            lamports: 1,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Matches `accounts::decode_attestation_key_set`'s exact byte offsets.
    fn fake_attestation_key_set_account(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Account {
        let mut data = vec![0u8; 8]; // discriminator
        data.extend_from_slice(&epoch.to_le_bytes());
        data.push(threshold);
        data.push(0); // bump
        data.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for k in keys {
            data.extend_from_slice(k.as_ref());
        }
        Account {
            lamports: 1,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Builds a full, consistent mock environment (live on-chain state +
    /// a matching plan/attested-plan) for the `execute_withdrawal`
    /// end-to-end tests — everything genuinely lines up (same mint,
    /// token program, PDAs, epoch) the way a real `plan` -> `attest`
    /// run would produce.
    fn end_to_end_fixture() -> (MockRpc, AttestedPlanFile, Keypair, Keypair) {
        let (attestation_keys, attestation_pubkeys) = keypair_pubkeys(3);
        let reserve_mint = Pubkey::new_unique();
        let token_program = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let reserve_authority = accounts::reserve_authority_pda();
        let reserve_token_account =
            accounts::associated_token_address(&reserve_authority, &reserve_mint, &token_program);
        let nonce = 7u64;
        let amount = 5_000u64;
        let epoch = 0u64;
        let protected_minimum = 1_000u64;
        let reserve_balance = 100_000u64;

        let message = glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message(
            PROTOCOL_VERSION,
            &PROGRAM_ID.to_bytes(),
            epoch,
            nonce,
            amount,
            &destination.to_bytes(),
            &reserve_mint.to_bytes(),
        );
        let plan = PlanFile {
            program_id: PROGRAM_ID.to_string(),
            reserve_mint: reserve_mint.to_string(),
            token_program: token_program.to_string(),
            reserve_authority: reserve_authority.to_string(),
            reserve_token_account: reserve_token_account.to_string(),
            destination_token_account: destination.to_string(),
            nonce,
            amount,
            attestation_epoch: epoch,
            attestation_threshold: 2,
            protected_minimum,
            reserve_balance_before: reserve_balance,
            message_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&message),
        };
        let attestations: Vec<AttestationEntry> = attestation_keys[..2]
            .iter()
            .map(|k| {
                let sig = k.sign_message(&message);
                AttestationEntry {
                    pubkey: k.pubkey().to_string(),
                    signature_hex: glc_reserve_bridge_service::goldcoin::hex::encode(sig.as_ref()),
                }
            })
            .collect();
        let attested = AttestedPlanFile { plan, attestations };

        let mut accounts_map = HashMap::new();
        accounts_map.insert(
            accounts::bridge_config_pda(),
            fake_bridge_config_account(true, reserve_mint, token_program, protected_minimum),
        );
        accounts_map.insert(
            accounts::attestation_key_set_pda(),
            fake_attestation_key_set_account(epoch, 2, &attestation_pubkeys),
        );
        accounts_map.insert(
            reserve_token_account,
            fake_token_account(reserve_mint, token_program, reserve_balance),
        );
        // rebalance_withdrawal PDA for this nonce deliberately absent (not yet used).

        let rpc = MockRpc {
            accounts: accounts_map,
            simulate_err: None,
            sent: Mutex::new(Vec::new()),
        };
        let admin = Keypair::new();
        let submitter = Keypair::new();
        (rpc, attested, admin, submitter)
    }

    #[tokio::test]
    async fn dry_run_succeeds_without_broadcasting() {
        let (rpc, attested, admin, submitter) = end_to_end_fixture();
        let outcome = execute_withdrawal(&rpc, &attested, &admin, &submitter, false)
            .await
            .unwrap();
        assert!(outcome.simulation_succeeded);
        assert!(!outcome.broadcast);
        assert!(outcome.signature.is_none());
        assert!(
            rpc.sent.lock().unwrap().is_empty(),
            "a dry run must never call send_transaction"
        );
    }

    #[tokio::test]
    async fn execute_requires_explicit_flag_to_broadcast() {
        let (rpc, attested, admin, submitter) = end_to_end_fixture();
        // Same fixture, only `do_execute` differs.
        let dry_run = execute_withdrawal(&rpc, &attested, &admin, &submitter, false)
            .await
            .unwrap();
        assert!(!dry_run.broadcast);
        assert_eq!(rpc.sent.lock().unwrap().len(), 0);

        let executed = execute_withdrawal(&rpc, &attested, &admin, &submitter, true)
            .await
            .unwrap();
        assert!(executed.broadcast);
        assert!(executed.signature.is_some());
        assert_eq!(
            rpc.sent.lock().unwrap().len(),
            1,
            "with --execute, exactly one transaction must be sent"
        );
    }

    #[tokio::test]
    async fn simulation_failure_blocks_broadcast_even_with_execute_flag() {
        let (mut rpc, attested, admin, submitter) = end_to_end_fixture();
        rpc.simulate_err = Some("custom program error: 0x1770".to_string());
        let result = execute_withdrawal(&rpc, &attested, &admin, &submitter, true).await;
        assert!(result.is_err());
        assert!(
            rpc.sent.lock().unwrap().is_empty(),
            "a failed simulation must never be followed by a broadcast, even with --execute"
        );
    }

    #[tokio::test]
    async fn live_unpaused_bridge_blocks_execute_end_to_end() {
        let (mut rpc, attested, admin, submitter) = end_to_end_fixture();
        let reserve_mint = Pubkey::from_str(&attested.plan.reserve_mint).unwrap();
        let token_program = Pubkey::from_str(&attested.plan.token_program).unwrap();
        rpc.accounts.insert(
            accounts::bridge_config_pda(),
            fake_bridge_config_account(
                false,
                reserve_mint,
                token_program,
                attested.plan.protected_minimum,
            ),
        );
        let result = execute_withdrawal(&rpc, &attested, &admin, &submitter, false).await;
        assert!(result.is_err());
        assert!(rpc.sent.lock().unwrap().is_empty());
    }

    fn keypair_pubkeys(n: usize) -> (Vec<Keypair>, Vec<Pubkey>) {
        let keys: Vec<Keypair> = (0..n).map(|_| Keypair::new()).collect();
        let pubkeys = keys.iter().map(|k| k.pubkey()).collect();
        (keys, pubkeys)
    }

    fn synthetic_plan() -> (PlanFile, Vec<Keypair>) {
        let (keys, pubkeys) = keypair_pubkeys(3);
        let reserve_mint = Pubkey::new_unique();
        let token_program = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let reserve_authority = accounts::reserve_authority_pda();
        let reserve_token_account =
            accounts::associated_token_address(&reserve_authority, &reserve_mint, &token_program);
        let nonce = 1u64;
        let amount = 5_000u64;
        let epoch = 0u64;
        let message = glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message(
            PROTOCOL_VERSION,
            &PROGRAM_ID.to_bytes(),
            epoch,
            nonce,
            amount,
            &destination.to_bytes(),
            &reserve_mint.to_bytes(),
        );
        let plan = PlanFile {
            program_id: PROGRAM_ID.to_string(),
            reserve_mint: reserve_mint.to_string(),
            token_program: token_program.to_string(),
            reserve_authority: reserve_authority.to_string(),
            reserve_token_account: reserve_token_account.to_string(),
            destination_token_account: destination.to_string(),
            nonce,
            amount,
            attestation_epoch: epoch,
            attestation_threshold: 2,
            protected_minimum: 1_000,
            reserve_balance_before: 100_000,
            message_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&message),
        };
        let _ = pubkeys;
        (plan, keys)
    }

    fn attest_offline(plan: &PlanFile, keys: &[Keypair]) -> Vec<AttestationEntry> {
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        keys.iter()
            .map(|k| {
                let sig = k.sign_message(&message);
                AttestationEntry {
                    pubkey: k.pubkey().to_string(),
                    signature_hex: glc_reserve_bridge_service::goldcoin::hex::encode(sig.as_ref()),
                }
            })
            .collect()
    }

    #[test]
    fn valid_plan_passes_tamper_check() {
        let (plan, _keys) = synthetic_plan();
        assert!(verify_plan_not_tampered(&plan).is_ok());
    }

    #[test]
    fn wrong_reserve_pda_is_rejected() {
        let (mut plan, _keys) = synthetic_plan();
        plan.reserve_authority = Pubkey::new_unique().to_string();
        let err = verify_plan_not_tampered(&plan).unwrap_err();
        assert!(err.contains("TAMPER DETECTED"), "got: {err}");
        assert!(err.contains("reserve_authority"), "got: {err}");
    }

    #[test]
    fn wrong_destination_token_account_is_rejected() {
        // Tampering the destination after message_hex was computed must
        // change what the message recomputes to, and be caught.
        let (mut plan, _keys) = synthetic_plan();
        plan.destination_token_account = Pubkey::new_unique().to_string();
        let err = verify_plan_not_tampered(&plan).unwrap_err();
        assert!(err.contains("TAMPER DETECTED"), "got: {err}");
    }

    #[test]
    fn unpaused_bridge_is_rejected() {
        let err = verify_withdrawal_is_currently_valid(5_000, false, 100_000, 1_000, false, 1)
            .unwrap_err();
        assert_eq!(err, VerifyError::BridgeNotPaused);
    }

    #[test]
    fn protected_minimum_violation_is_rejected() {
        // balance 6_000, protected_minimum 1_000 -> only 5_000 available;
        // requesting 5_001 must fail.
        let err =
            verify_withdrawal_is_currently_valid(5_001, true, 6_000, 1_000, false, 1).unwrap_err();
        assert!(matches!(err, VerifyError::ProtectedMinimumViolation { .. }));
    }

    #[test]
    fn exact_protected_minimum_boundary_is_allowed() {
        assert!(verify_withdrawal_is_currently_valid(5_000, true, 6_000, 1_000, false, 1).is_ok());
    }

    #[test]
    fn replayed_nonce_is_rejected() {
        let err = verify_withdrawal_is_currently_valid(5_000, true, 100_000, 1_000, true, 42)
            .unwrap_err();
        assert_eq!(err, VerifyError::NonceAlreadyUsed { nonce: 42 });
    }

    #[test]
    fn wrong_mint_destination_is_rejected() {
        let expected_mint = Pubkey::new_unique();
        let expected_program = Pubkey::new_unique();
        let wrong_mint = Pubkey::new_unique();
        let err = verify_destination(
            wrong_mint,
            expected_program,
            expected_mint,
            expected_program,
        )
        .unwrap_err();
        assert!(matches!(err, VerifyError::DestinationWrongMint { .. }));
    }

    #[test]
    fn wrong_token_program_destination_is_rejected() {
        let expected_mint = Pubkey::new_unique();
        let expected_program = Pubkey::new_unique();
        let wrong_program = Pubkey::new_unique();
        let err = verify_destination(
            expected_mint,
            wrong_program,
            expected_mint,
            expected_program,
        )
        .unwrap_err();
        assert!(matches!(err, VerifyError::DestinationWrongOwner { .. }));
    }

    #[test]
    fn reserve_configuration_mismatch_is_rejected() {
        let live_mint = Pubkey::new_unique();
        let live_program = Pubkey::new_unique();
        let wrong_mint = Pubkey::new_unique();
        let err = verify_reserve_configuration(live_mint, live_program, Some(wrong_mint), None)
            .unwrap_err();
        assert!(matches!(err, VerifyError::ReserveMintMismatch { .. }));

        let wrong_program = Pubkey::new_unique();
        let err = verify_reserve_configuration(live_mint, live_program, None, Some(wrong_program))
            .unwrap_err();
        assert!(matches!(err, VerifyError::TokenProgramMismatch { .. }));
    }

    #[test]
    fn unconfigured_reserve_is_rejected() {
        let err = verify_reserve_configuration(Pubkey::default(), Pubkey::default(), None, None)
            .unwrap_err();
        assert_eq!(err, VerifyError::ReserveNotConfigured);
    }

    #[test]
    fn threshold_not_met_is_rejected() {
        let (plan, keys) = synthetic_plan();
        let entries = attest_offline(&plan, &keys[..1]); // only 1 of 3
        let key_set_keys: Vec<Pubkey> = keys.iter().map(|k| k.pubkey()).collect();
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let err = collect_valid_attestations(
            &entries,
            &message,
            &key_set_keys,
            plan.attestation_threshold,
        )
        .unwrap_err();
        assert!(err.contains("threshold"), "got: {err}");
    }

    #[test]
    fn signer_mismatch_is_rejected_and_excluded() {
        let (plan, keys) = synthetic_plan();
        let mut entries = attest_offline(&plan, &keys[..2]);
        // A signature from a key NOT in the current attestation key set.
        let outsider = Keypair::new();
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let outsider_sig = outsider.sign_message(&message);
        entries.push(AttestationEntry {
            pubkey: outsider.pubkey().to_string(),
            signature_hex: glc_reserve_bridge_service::goldcoin::hex::encode(outsider_sig.as_ref()),
        });
        let key_set_keys: Vec<Pubkey> = keys.iter().map(|k| k.pubkey()).collect();
        let valid = collect_valid_attestations(
            &entries,
            &message,
            &key_set_keys,
            plan.attestation_threshold,
        )
        .unwrap();
        // Only the 2 real attestation-key signatures count; the outsider is excluded.
        assert_eq!(valid.len(), 2);
    }

    #[test]
    fn two_of_three_attestations_meet_threshold() {
        let (plan, keys) = synthetic_plan();
        let entries = attest_offline(&plan, &keys[..2]);
        let key_set_keys: Vec<Pubkey> = keys.iter().map(|k| k.pubkey()).collect();
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let valid = collect_valid_attestations(
            &entries,
            &message,
            &key_set_keys,
            plan.attestation_threshold,
        )
        .unwrap();
        assert_eq!(valid.len(), 2);
    }

    #[test]
    fn successful_transaction_construction() {
        let (plan, keys) = synthetic_plan();
        let entries = attest_offline(&plan, &keys[..2]);
        let key_set_keys: Vec<Pubkey> = keys.iter().map(|k| k.pubkey()).collect();
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let attestations = collect_valid_attestations(
            &entries,
            &message,
            &key_set_keys,
            plan.attestation_threshold,
        )
        .unwrap();
        let admin = Pubkey::new_unique();
        let instructions = build_instructions(&plan, &attestations, &admin).unwrap();
        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[0].program_id, solana_sdk::ed25519_program::ID);
        assert_eq!(instructions[1].program_id, PROGRAM_ID);
        // admin is the first (signer) account on the rebalance_withdraw instruction.
        assert_eq!(instructions[1].accounts[0].pubkey, admin);
        assert!(instructions[1].accounts[0].is_signer);
    }

    #[test]
    fn zero_amount_is_rejected() {
        let err =
            verify_withdrawal_is_currently_valid(0, true, 100_000, 1_000, false, 1).unwrap_err();
        assert_eq!(err, VerifyError::ZeroAmount);
    }
}
