//! `glc-rebalance-policy` — create, inspect, and govern the on-chain
//! `RebalancePolicy`: the treasury-destination allowlist that bounds
//! `treasury_withdraw`.
//!
//! # Why this binary exists
//!
//! After the 2026-09-02 incident, `rebalance_withdraw` was retired and
//! `treasury_withdraw` took its place — but `treasury_withdraw` fails
//! closed until a `RebalancePolicy` exists, because a policy-less bridge
//! has no allowlisted destination and therefore no authorized withdrawal.
//! Creating that policy is a mandatory migration step, and it is
//! authorized by a THRESHOLD ATTESTATION over a governance message, never
//! by `BridgeConfig.admin`. This tool is the supported way to produce one.
//!
//! # Why the admin key is nowhere in this file
//!
//! Every instruction here (`initialize_rebalance_policy`,
//! `propose_rebalance_policy`, `cancel_rebalance_policy`) is authorized
//! solely by a threshold proof. The keypair this tool loads is a fee
//! payer / rent recipient and confers NO authority. That is the whole
//! point: an allowlist a single admin can edit is not an allowlist, so
//! the admin key must not be able to create or change one either. Do not
//! add an `--admin-keypair` flag; there is nothing for it to authorize.
//!
//! # Three stages, three hosts
//!
//! The same separation `glc-treasury-withdraw` uses, for the same reason —
//! the incident happened because the admin key and the attestation signer
//! credentials were resident on one host:
//!
//!   plan    — no key of any kind. Reads and verifies live chain state,
//!             validates the proposed parameters against the exact rules
//!             the program itself enforces, and writes the canonical
//!             governance message to a plan file. This step IS the dry
//!             run for parameter review.
//!   attest  — RUN ON THE APPROVAL HOST. No local private key; contacts
//!             the remote attestation signer endpoints. Re-verifies live
//!             state before spending a network round trip on a custody
//!             domain.
//!   execute — needs only a fee-payer keypair. Re-verifies live state a
//!             third time, rebuilds the message from the plan's own typed
//!             fields and refuses if it does not match the recorded bytes,
//!             ALWAYS simulates, and broadcasts ONLY if `--execute` is
//!             supplied AND the simulation succeeded.
//!
//! # Fail-closed posture
//!
//! Every stage re-derives rather than trusts. `verify_plan_not_tampered`
//! recomputes both the parameter commitment and the governance message
//! from the plan file's own declared fields: editing a treasury address in
//! the JSON without also forging a SHA-256 preimage produces a
//! mismatch and a refusal, and the attestations were over the untampered
//! bytes anyway. Parameters are validated client-side against the same
//! rules `validate_rebalance_policy` enforces on chain, so a bad policy is
//! refused before any custody domain is asked to sign it.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::signing::remote::{RemoteAttestationSigner, RemoteSignerConfig};
use glc_reserve_bridge_service::signing::signers::AttestationSigner;
use glc_reserve_bridge_service::solana::accounts::{
    self, PendingRebalancePolicySnapshot, RebalancePolicySnapshot, MAX_TREASURY_DESTINATIONS,
    PROGRAM_ID,
};
use glc_reserve_bridge_service::solana::ed25519;
use glc_reserve_bridge_service::solana::instructions;
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};
use glc_reserve_bridge_shared::governance::{
    cancel_params, governance_message, rebalance_policy_params, ACTION_CANCEL_REBALANCE_POLICY,
    ACTION_INITIALIZE_REBALANCE_POLICY, ACTION_PROPOSE_REBALANCE_POLICY,
};

/// Unchanged by the reserve-withdrawal hardening: the claim families grew
/// by ADDING action bytes, so settlement signatures in flight across the
/// upgrade are unaffected (docs/29 section 6). Pinned by a test below.
const PROTOCOL_VERSION: u8 = 1;

const USAGE: &str = "glc-rebalance-policy — govern the reserve treasury allowlist

Authorized by THRESHOLD ATTESTATION only. There is deliberately no
--admin-keypair flag anywhere in this tool: the admin key cannot create or
change the allowlist, which is the control that contains a compromised
admin. The keypair below only pays fees/rent and confers no authority.

PLAN (no key needed; reads and verifies live chain state; this IS the dry run)
  glc-rebalance-policy plan \\
      --rpc-url URL --action init \\
      --treasury PUBKEY [--treasury PUBKEY ...] \\
      [--reserve-mint PUBKEY] [--token-program PUBKEY] \\
      --out plan.json

  --action init
      One-time creation of the policy. Fails if a policy already exists.
  --action propose
      Queues a REPLACEMENT policy behind the governance timelock. Requires
      an existing policy and no already-pending change. Takes the same
      --treasury arguments as init.
  --action cancel
      Discards the pending policy change. Takes no parameter flags; binds
      the pending change's exact eta, so the approval cannot be replayed
      against a later re-proposal.

  --treasury PUBKEY
      An allowlisted destination TOKEN ACCOUNT address (not a wallet
      owner). Repeat for up to 4. ORDER IS SIGNIFICANT — it is committed
      to by the attestation and is the order stored on chain.

  The allowlist is the WHOLE policy. There is deliberately no amount
  ceiling, rate limit or rolling budget to configure: fixing WHERE the
  reserve may pay is the control, and capping HOW MUCH would only
  constrain legitimate treasury operations. `protected_minimum`
  (governed via `glc-admin set-limit`) remains the one accounting floor.

ATTEST (no local private key; contacts remote signer endpoints)
  RUN THIS ON THE APPROVAL HOST, NOT THE BRIDGE HOST.
  glc-rebalance-policy attest \\
      --plan plan.json --rpc-url URL \\
      --attestation-signer PUBKEY,https://URL,AUTH_TOKEN_ENV_VAR[,TIMEOUT_MS] \\
      [--attestation-signer ... (repeat, >= threshold)] \\
      --out attested-plan.json

EXECUTE (needs only a fee-payer keypair; always simulates first)
  glc-rebalance-policy execute \\
      --attested-plan attested-plan.json --rpc-url URL \\
      --payer-keypair PATH \\
      [--execute]

  --execute
      Without this flag, execute builds, verifies, and simulates the
      transaction and prints the result — nothing is broadcast. With it,
      the transaction is also submitted, but ONLY if the simulation that
      just ran succeeded.

APPLY (permissionless; carries no attestation — the proposal was the approval)
  glc-rebalance-policy apply \\
      --rpc-url URL --payer-keypair PATH \\
      [--reserve-mint PUBKEY] [--token-program PUBKEY] \\
      [--execute]

      Runs execute_rebalance_policy on a pending change whose timelock has
      elapsed. Same simulate-first/--execute gate as above.

VERIFY (read-only; exits non-zero on ANY mismatch)
  glc-rebalance-policy verify \\
      --rpc-url URL \\
      --expect-treasury PUBKEY [--expect-treasury PUBKEY ...]

      Asserts the live on-chain policy is EXACTLY what you intended,
      including allowlist order. Run this after execute, before unpausing.

See RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md for the full operator
procedure, and `glc-admin rebalance-policy-show` for a human-readable
view of the current policy and any queued change.";

// ------------------------------------------------------------ arg helpers --

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

fn optional_pubkey(args: &[String], name: &str) -> Result<Option<Pubkey>, String> {
    match flag(args, name) {
        Some(v) => {
            Ok(Some(Pubkey::from_str(v).map_err(|e| {
                format!("{name} {v:?} is not a valid pubkey: {e}")
            })?))
        }
        None => Ok(None),
    }
}

fn parse_pubkeys(values: &[&str], name: &str) -> Result<Vec<Pubkey>, String> {
    values
        .iter()
        .map(|v| {
            Pubkey::from_str(v).map_err(|e| format!("{name} {v:?} is not a valid pubkey: {e}"))
        })
        .collect()
}

fn plan_treasury_strs(plan: &PlanFile) -> Vec<&str> {
    plan.treasuries.iter().map(|s| s.as_str()).collect()
}

// ----------------------------------------------------------- plan file I/O --

/// Which governance action a plan authorizes. Serialized by name so a plan
/// file is self-describing and an operator can see at a glance what they
/// are about to approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PolicyAction {
    Init,
    Propose,
    Cancel,
}

impl PolicyAction {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "init" => Ok(Self::Init),
            "propose" => Ok(Self::Propose),
            "cancel" => Ok(Self::Cancel),
            other => Err(format!(
                "--action {other:?} is not one of init, propose, cancel"
            )),
        }
    }

    /// The governance action byte this action signs under. Distinct per
    /// action on purpose: an approval to CREATE the first policy must
    /// never be replayable as an approval to REPLACE an existing one.
    fn action_byte(self) -> u8 {
        match self {
            Self::Init => ACTION_INITIALIZE_REBALANCE_POLICY,
            Self::Propose => ACTION_PROPOSE_REBALANCE_POLICY,
            Self::Cancel => ACTION_CANCEL_REBALANCE_POLICY,
        }
    }

    fn takes_parameters(self) -> bool {
        matches!(self, Self::Init | Self::Propose)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Propose => "propose",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PlanFile {
    action: PolicyAction,
    program_id: String,
    reserve_mint: String,
    token_program: String,
    reserve_authority: String,
    reserve_token_account: String,
    attestation_epoch: u64,
    attestation_threshold: u8,
    protocol_version: u8,
    /// Proposed allowlist, in the exact order committed to. Empty for
    /// `cancel`.
    treasuries: Vec<String>,
    /// The pending change's `eta` this cancellation binds. Zero (and
    /// unused) for `init`/`propose`.
    pending_eta: i64,
    /// The live `RebalancePolicy.version` at plan time, if a policy
    /// exists. Informational for the operator report; never trusted.
    current_policy_version: Option<u64>,
    params_commitment_hex: String,
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
// Everything below takes already-fetched values, does no I/O, and is
// directly unit-testable without a live RPC or network.

/// Client-side mirror of the on-chain `validate_rebalance_policy`
/// (`programs/glc-reserve-bridge/src/validation.rs`). The program is
/// authoritative; this exists so an invalid policy is refused before a
/// custody domain is ever asked to sign it, with a message that names the
/// specific rule broken rather than an opaque instruction error.
fn validate_policy_params(
    treasuries: &[Pubkey],
    reserve_token_account: &Pubkey,
) -> Result<(), String> {
    if treasuries.is_empty() {
        return Err(
            "at least one --treasury is required: an empty allowlist is refused on \
                    chain (it would be unusable, not safer)"
                .to_string(),
        );
    }
    if treasuries.len() > MAX_TREASURY_DESTINATIONS {
        return Err(format!(
            "{} treasuries supplied but at most {MAX_TREASURY_DESTINATIONS} are allowed",
            treasuries.len()
        ));
    }
    for (i, t) in treasuries.iter().enumerate() {
        if *t == Pubkey::default() {
            return Err("a treasury destination may not be the all-zero pubkey".to_string());
        }
        if t == reserve_token_account {
            return Err(format!(
                "treasury destination {t} IS the reserve token account — allowlisting the \
                 reserve itself is refused on chain"
            ));
        }
        if treasuries[..i].contains(t) {
            return Err(format!("treasury destination {t} is listed more than once"));
        }
    }
    Ok(())
}

/// Recomputes the parameter commitment and the governance message from the
/// plan file's own typed fields and refuses if either disagrees with the
/// recorded bytes.
///
/// This is the anti-tamper check: a plan file edited to name a different
/// treasury no longer hashes to its recorded commitment, and the attestations were collected over the ORIGINAL
/// message regardless. Run at every stage after `plan`.
fn verify_plan_not_tampered(plan: &PlanFile) -> Result<(), String> {
    if plan.program_id != PROGRAM_ID.to_string() {
        return Err(format!(
            "plan targets program {} but this build is compiled against {PROGRAM_ID}",
            plan.program_id
        ));
    }
    if plan.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "plan declares protocol version {} but this build expects {PROTOCOL_VERSION}",
            plan.protocol_version
        ));
    }
    if plan.action.takes_parameters() == plan.treasuries.is_empty() {
        return Err(format!(
            "plan action {:?} is inconsistent with its treasury list",
            plan.action.as_str()
        ));
    }

    let params = if plan.action.takes_parameters() {
        let treasuries = parse_pubkeys(&plan_treasury_strs(plan), "plan treasury")?;
        let raw: Vec<[u8; 32]> = treasuries.iter().map(|t| t.to_bytes()).collect();
        rebalance_policy_params(&raw)
    } else {
        cancel_params(ACTION_PROPOSE_REBALANCE_POLICY, plan.pending_eta)
    };

    let commitment = solana_sdk::hash::hash(&params).to_bytes();
    let commitment_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&commitment);
    if commitment_hex != plan.params_commitment_hex {
        return Err(
            "PLAN TAMPERED: the recorded parameter commitment does not match the plan's own \
             declared parameters. Re-run `plan`; do not attempt to repair the file by hand."
                .to_string(),
        );
    }

    let expected = governance_message(
        plan.protocol_version,
        &PROGRAM_ID.to_bytes(),
        plan.attestation_epoch,
        plan.action.action_byte(),
        &commitment,
    );
    let expected_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&expected);
    if expected_hex != plan.message_hex {
        return Err(
            "PLAN TAMPERED: the recorded governance message does not match the message derived \
             from the plan's own fields. Re-run `plan`."
                .to_string(),
        );
    }
    Ok(())
}

/// Refuses a plan whose attestation epoch is no longer current. A key
/// rotation invalidates every governance signature by construction (the
/// epoch is inside the signed bytes); catching it here turns an opaque
/// on-chain signature mismatch into an explanation.
fn verify_epoch_is_current(plan: &PlanFile, live_epoch: u64) -> Result<(), String> {
    if plan.attestation_epoch != live_epoch {
        return Err(format!(
            "plan was built under attestation epoch {} but the live epoch is {live_epoch} — \
             the attestation keys rotated, so every signature over this plan is now void. \
             Re-run `plan`.",
            plan.attestation_epoch
        ));
    }
    Ok(())
}

/// Refuses an action whose on-chain preconditions no longer hold. Mirrors
/// the account-level constraints (`init` on a singleton, "policy must
/// already exist", "a pending change must exist") so the operator gets a
/// sentence instead of an Anchor error code.
fn verify_action_preconditions(
    action: PolicyAction,
    policy_exists: bool,
    pending_exists: bool,
) -> Result<(), String> {
    match action {
        PolicyAction::Init if policy_exists => Err(
            "a RebalancePolicy ALREADY EXISTS — initialization is one-time and this call would \
             fail on chain at account creation. To change an existing policy use \
             `--action propose`, which goes through the governance timelock."
                .to_string(),
        ),
        PolicyAction::Propose if !policy_exists => Err(
            "no RebalancePolicy exists yet — propose replaces an existing policy. Use \
             `--action init` to create the first one."
                .to_string(),
        ),
        PolicyAction::Propose if pending_exists => Err(
            "a policy change is ALREADY PENDING — the pending slot is a singleton so a \
             compromised quorum cannot queue a backlog. Wait for it to execute, or cancel it \
             with `--action cancel` first."
                .to_string(),
        ),
        PolicyAction::Cancel if !pending_exists => {
            Err("no pending policy change exists, so there is nothing to cancel.".to_string())
        }
        _ => Ok(()),
    }
}

/// Compares a live on-chain policy against the values the operator says
/// they intended. Order-sensitive on the allowlist, because order is what
/// the attestation committed to.
fn verify_policy_matches(
    policy: &RebalancePolicySnapshot,
    expect_treasuries: &[Pubkey],
) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    if policy.treasuries != expect_treasuries {
        problems.push(format!(
            "allowlist mismatch (order is significant):\n    on chain = {:?}\n    expected = {:?}",
            policy
                .treasuries
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>(),
            expect_treasuries
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>(),
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Validates and de-duplicates collected attestation signatures against
/// the CURRENT on-chain key set, verifying each one over the exact
/// governance message before it is ever placed in a transaction.
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
                 against the expected governance message (SIGNER MISMATCH)"
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

/// Builds the two-instruction transaction (ed25519 proof, then the
/// governance instruction) — pure construction, no signing, no I/O.
///
/// The proof must immediately precede the governance instruction: the
/// program reads it at relative index -1.
fn build_instructions(
    plan: &PlanFile,
    attestations: &[(Pubkey, Signature)],
    payer: &Pubkey,
) -> Result<Vec<Instruction>, String> {
    let reserve_mint = Pubkey::from_str(&plan.reserve_mint).map_err(|e| e.to_string())?;
    let token_program = Pubkey::from_str(&plan.token_program).map_err(|e| e.to_string())?;
    let message = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex)
        .map_err(|e| e.to_string())?;
    let treasuries = parse_pubkeys(&plan_treasury_strs(plan), "plan treasury")?;

    let ed25519_ix = ed25519::build_attestation_proof(attestations, &message);
    let governance_ix = match plan.action {
        PolicyAction::Init => instructions::initialize_rebalance_policy(
            payer,
            &reserve_mint,
            &token_program,
            &treasuries,
        ),
        PolicyAction::Propose => instructions::propose_rebalance_policy(
            payer,
            &reserve_mint,
            &token_program,
            &treasuries,
        ),
        PolicyAction::Cancel => instructions::cancel_rebalance_policy(payer),
    };
    Ok(vec![ed25519_ix, governance_ix])
}

// ------------------------------------------------------------ fetch helpers --

struct LiveState {
    reserve_mint: Pubkey,
    token_program: Pubkey,
    reserve_authority: Pubkey,
    reserve_token_account: Pubkey,
    epoch: u64,
    threshold: u8,
    keys: Vec<Pubkey>,
    policy: Option<RebalancePolicySnapshot>,
    pending: Option<PendingRebalancePolicySnapshot>,
}

async fn fetch_live_state<R: SolanaRpc>(
    rpc: &R,
    expect_mint: Option<Pubkey>,
    expect_token_program: Option<Pubkey>,
) -> Result<LiveState, String> {
    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config does not exist — this bridge is not initialized")?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
    if config.reserve_token_mint == Pubkey::default()
        || config.reserve_token_program == Pubkey::default()
    {
        return Err(
            "the reserve vault is not configured yet (reserve mint/token program are \
                    unset) — run initialize_reserve_vault first"
                .to_string(),
        );
    }
    if let Some(m) = expect_mint {
        if m != config.reserve_token_mint {
            return Err(format!(
                "--reserve-mint {m} does not match the configured reserve mint {}",
                config.reserve_token_mint
            ));
        }
    }
    if let Some(tp) = expect_token_program {
        if tp != config.reserve_token_program {
            return Err(format!(
                "--token-program {tp} does not match the configured token program {}",
                config.reserve_token_program
            ));
        }
    }

    let key_set_account = rpc
        .get_account(&accounts::attestation_key_set_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("attestation_key_set does not exist")?;
    let key_set =
        accounts::decode_attestation_key_set(&key_set_account.data).map_err(|e| e.to_string())?;

    let policy = match rpc
        .get_account(&accounts::rebalance_policy_pda())
        .await
        .map_err(|e| e.to_string())?
    {
        Some(a) => Some(accounts::decode_rebalance_policy(&a.data).map_err(|e| e.to_string())?),
        None => None,
    };
    let pending = match rpc
        .get_account(&accounts::pending_rebalance_policy_pda())
        .await
        .map_err(|e| e.to_string())?
    {
        Some(a) => {
            Some(accounts::decode_pending_rebalance_policy(&a.data).map_err(|e| e.to_string())?)
        }
        None => None,
    };

    let reserve_authority = accounts::reserve_authority_pda();
    let reserve_token_account = accounts::associated_token_address(
        &reserve_authority,
        &config.reserve_token_mint,
        &config.reserve_token_program,
    );
    Ok(LiveState {
        reserve_mint: config.reserve_token_mint,
        token_program: config.reserve_token_program,
        reserve_authority,
        reserve_token_account,
        epoch: key_set.epoch,
        threshold: key_set.threshold,
        keys: key_set.keys,
        policy,
        pending,
    })
}

// ------------------------------------------------------------------- main --

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
        "apply" => rt.block_on(cmd_apply(&args)),
        "verify" => rt.block_on(cmd_verify(&args)),
        other => {
            eprintln!("unknown subcommand {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("\nerror: {e}");
        std::process::exit(1);
    }
}

// -------------------------------------------------------------------- plan --

async fn cmd_plan(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url")?.to_string();
    let action = PolicyAction::parse(require(args, "--action")?)?;
    let out_path = PathBuf::from(require(args, "--out")?);
    let expect_mint = optional_pubkey(args, "--reserve-mint")?;
    let expect_token_program = optional_pubkey(args, "--token-program")?;

    let rpc = RealSolanaRpc::new(rpc_url);
    let live = fetch_live_state(&rpc, expect_mint, expect_token_program).await?;
    verify_action_preconditions(action, live.policy.is_some(), live.pending.is_some())?;

    let (treasuries, pending_eta) = if action.takes_parameters() {
        let treasuries = parse_pubkeys(&flags_all(args, "--treasury"), "--treasury")?;
        validate_policy_params(&treasuries, &live.reserve_token_account)?;
        (treasuries, 0)
    } else {
        // `verify_action_preconditions` already proved this exists.
        let pending = live
            .pending
            .as_ref()
            .ok_or("no pending policy change to cancel")?;
        (Vec::new(), pending.eta)
    };

    let params = if action.takes_parameters() {
        let raw: Vec<[u8; 32]> = treasuries.iter().map(|t| t.to_bytes()).collect();
        rebalance_policy_params(&raw)
    } else {
        cancel_params(ACTION_PROPOSE_REBALANCE_POLICY, pending_eta)
    };
    let commitment = solana_sdk::hash::hash(&params).to_bytes();
    let message = governance_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        live.epoch,
        action.action_byte(),
        &commitment,
    );

    let plan = PlanFile {
        action,
        program_id: PROGRAM_ID.to_string(),
        reserve_mint: live.reserve_mint.to_string(),
        token_program: live.token_program.to_string(),
        reserve_authority: live.reserve_authority.to_string(),
        reserve_token_account: live.reserve_token_account.to_string(),
        attestation_epoch: live.epoch,
        attestation_threshold: live.threshold,
        protocol_version: PROTOCOL_VERSION,
        treasuries: treasuries.iter().map(|t| t.to_string()).collect(),
        pending_eta,
        current_policy_version: live.policy.as_ref().map(|p| p.version),
        params_commitment_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&commitment),
        message_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&message),
    };
    // Self-check: prove the file we are about to write passes the same
    // tamper check every later stage applies to it.
    verify_plan_not_tampered(&plan)?;

    println!("Rebalance-policy plan ({}):", action.as_str());
    println!("  program id                = {PROGRAM_ID}");
    println!("  reserve mint              = {}", live.reserve_mint);
    println!("  token program             = {}", live.token_program);
    println!(
        "  reserve token account     = {}",
        live.reserve_token_account
    );
    println!("  attestation epoch         = {}", live.epoch);
    println!(
        "  attestation threshold     = {} of {}",
        live.threshold,
        live.keys.len()
    );
    match &live.policy {
        Some(p) => println!(
            "  current policy            = version {}, {} treasury/treasuries",
            p.version,
            p.treasuries.len()
        ),
        None => println!(
            "  current policy            = NONE (treasury_withdraw fails closed for every \
             destination until this plan is executed)"
        ),
    }
    if action.takes_parameters() {
        println!("\n  PROPOSED ALLOWLIST (order is committed to by the attestation):");
        for (i, t) in treasuries.iter().enumerate() {
            println!("    [{i}] {t}");
        }
    } else {
        println!("\n  CANCELLING the pending change with eta = {pending_eta}");
    }
    println!(
        "\n  governance message        = {} bytes, action byte 0x{:02x}",
        message.len(),
        action.action_byte()
    );
    println!(
        "  parameter commitment      = {}",
        plan.params_commitment_hex
    );

    let json = serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?;
    std::fs::write(&out_path, &json).map_err(|e| format!("writing {out_path:?}: {e}"))?;
    println!(
        "\nPlan written to {}. Nothing has been signed or broadcast. Review the allowlist and \
         limits above, then run `attest` ON THE APPROVAL HOST.",
        out_path.display()
    );
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

    // Re-verify live state before contacting a signer endpoint — a stale
    // or now-invalid plan must not cost a custody domain a round trip, let
    // alone collect a signature over it.
    let rpc = RealSolanaRpc::new(rpc_url);
    let live = fetch_live_state(&rpc, None, None).await?;
    verify_epoch_is_current(&plan, live.epoch)?;
    verify_action_preconditions(plan.action, live.policy.is_some(), live.pending.is_some())?;
    if plan.action.takes_parameters() {
        let treasuries = parse_pubkeys(&plan_treasury_strs(&plan), "plan treasury")?;
        validate_policy_params(&treasuries, &live.reserve_token_account)?;
    } else {
        let pending = live.pending.as_ref().ok_or("no pending policy change")?;
        if pending.eta != plan.pending_eta {
            return Err(format!(
                "the pending change's eta is now {} but this plan cancels eta {} — the queued \
                 change was replaced. Re-run `plan`.",
                pending.eta, plan.pending_eta
            ));
        }
    }

    let message = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex)
        .map_err(|e| e.to_string())?;
    println!(
        "Requesting {} attestation signature(s) over a {}-byte governance message (action {})...",
        plan.attestation_threshold,
        message.len(),
        plan.action.as_str()
    );

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
        "\nAttested plan written to {} — {} of {} required signatures collected. Run `execute` \
         next.",
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
    let payer_keypair_path = require(args, "--payer-keypair")?;
    let do_execute = args.iter().any(|a| a == "--execute");

    let json = std::fs::read_to_string(&attested_path)
        .map_err(|e| format!("reading {attested_path:?}: {e}"))?;
    let attested: AttestedPlanFile =
        serde_json::from_str(&json).map_err(|e| format!("{attested_path:?} is not valid: {e}"))?;
    let payer = read_keypair_file(payer_keypair_path)
        .map_err(|e| format!("reading payer keypair {payer_keypair_path:?}: {e}"))?;

    let rpc = RealSolanaRpc::new(rpc_url);
    let outcome = execute_governance(&rpc, &attested, &payer, do_execute).await?;
    debug_assert!(
        outcome.simulation_succeeded,
        "execute_governance only ever returns Ok after a successful simulation"
    );
    if outcome.broadcast {
        println!("Broadcast: {}", outcome.signature.unwrap());
        println!(
            "\nVerify the result before unpausing:\n  glc-rebalance-policy verify --rpc-url URL \
             --expect-treasury ..."
        );
    } else {
        println!("\n--execute not supplied — this was a dry run. Nothing was broadcast.");
    }
    Ok(())
}

/// Outcome of a single execute attempt — typed rather than merely printed
/// so both `cmd_execute` and the offline tests can assert on it.
#[derive(Debug)]
struct ExecuteOutcome {
    simulation_succeeded: bool,
    broadcast: bool,
    signature: Option<String>,
}

/// The full verify -> build -> simulate -> (maybe) broadcast pipeline,
/// generic over [`SolanaRpc`]. Never broadcasts unless `do_execute` is
/// true AND simulation just succeeded — both checked here, in this order,
/// not left to the caller to get right.
async fn execute_governance<R: SolanaRpc>(
    rpc: &R,
    attested: &AttestedPlanFile,
    payer: &Keypair,
    do_execute: bool,
) -> Result<ExecuteOutcome, String> {
    let plan = &attested.plan;
    verify_plan_not_tampered(plan)?;

    // Third independent live-state check, immediately before building the
    // real transaction.
    let live = fetch_live_state(rpc, None, None).await?;
    verify_epoch_is_current(plan, live.epoch)?;
    verify_action_preconditions(plan.action, live.policy.is_some(), live.pending.is_some())?;
    if plan.action.takes_parameters() {
        let treasuries = parse_pubkeys(&plan_treasury_strs(plan), "plan treasury")?;
        validate_policy_params(&treasuries, &live.reserve_token_account)?;
    }

    let message = glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex)
        .map_err(|e| e.to_string())?;
    let attestations =
        collect_valid_attestations(&attested.attestations, &message, &live.keys, live.threshold)?;
    let instructions = build_instructions(plan, &attestations, &payer.pubkey())?;

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );

    let payer_lamports = rpc
        .get_account(&payer.pubkey())
        .await
        .map_err(|e| e.to_string())?
        .map(|a| a.lamports)
        .unwrap_or(0);

    println!("Transaction summary ({}):", plan.action.as_str());
    println!("  program id                = {PROGRAM_ID}");
    println!("  reserve mint              = {}", plan.reserve_mint);
    println!("  token program             = {}", plan.token_program);
    println!(
        "  reserve token account     = {}",
        plan.reserve_token_account
    );
    println!("  attestation epoch         = {}", plan.attestation_epoch);
    if plan.action.takes_parameters() {
        println!("  ALLOWLIST (ordered):");
        for (i, t) in plan.treasuries.iter().enumerate() {
            println!("    [{i}] {t}");
        }
    } else {
        println!("  cancelling pending eta    = {}", plan.pending_eta);
    }
    println!(
        "  attestation signers used  = {} of {} required",
        attestations.len(),
        live.threshold
    );
    println!(
        "  payer (fee + rent)        = {} ({payer_lamports} lamports) — confers NO authority",
        payer.pubkey()
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

// ------------------------------------------------------------------- apply --

async fn cmd_apply(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url")?.to_string();
    let payer_keypair_path = require(args, "--payer-keypair")?;
    let expect_mint = optional_pubkey(args, "--reserve-mint")?;
    let expect_token_program = optional_pubkey(args, "--token-program")?;
    let do_execute = args.iter().any(|a| a == "--execute");

    let payer = read_keypair_file(payer_keypair_path)
        .map_err(|e| format!("reading payer keypair {payer_keypair_path:?}: {e}"))?;
    let rpc = RealSolanaRpc::new(rpc_url);
    let live = fetch_live_state(&rpc, expect_mint, expect_token_program).await?;

    let pending = live.pending.as_ref().ok_or(
        "no pending policy change exists — nothing to apply. Queue one with `plan --action \
         propose` first.",
    )?;
    if live.policy.is_none() {
        return Err("no RebalancePolicy exists to update".to_string());
    }
    if pending.proposed_under_epoch != live.epoch {
        return Err(format!(
            "the pending change was proposed under attestation epoch {} but the live epoch is \
             {} — a key rotation voids every queued change. Cancel it and re-propose.",
            pending.proposed_under_epoch, live.epoch
        ));
    }

    println!("Pending policy change:");
    println!("  eta                       = {}", pending.eta);
    println!(
        "  proposed under epoch      = {}",
        pending.proposed_under_epoch
    );
    println!("  ALLOWLIST (ordered):");
    for (i, t) in pending.treasuries.iter().enumerate() {
        println!("    [{i}] {t}");
    }

    let ix = instructions::execute_rebalance_policy(
        &payer.pubkey(),
        &live.reserve_mint,
        &live.token_program,
    );
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);

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
            "simulation failed — refusing to broadcast (the timelock may not have elapsed yet)"
                .to_string(),
        );
    }
    println!(
        "  succeeded (units consumed: {:?})",
        simulation.units_consumed
    );

    if !do_execute {
        println!("\n--execute not supplied — this was a dry run. Nothing was broadcast.");
        return Ok(());
    }
    println!("\n--execute supplied and simulation succeeded — broadcasting...");
    let signature = rpc.send_transaction(&tx).await.map_err(|e| e.to_string())?;
    println!("Broadcast: {signature}");
    Ok(())
}

// ------------------------------------------------------------------ verify --

async fn cmd_verify(args: &[String]) -> Result<(), String> {
    let rpc_url = require(args, "--rpc-url")?.to_string();
    let expect_treasuries =
        parse_pubkeys(&flags_all(args, "--expect-treasury"), "--expect-treasury")?;
    if expect_treasuries.is_empty() {
        return Err("at least one --expect-treasury is required".to_string());
    }
    let rpc = RealSolanaRpc::new(rpc_url);
    let live = fetch_live_state(&rpc, None, None).await?;
    let policy = live.policy.as_ref().ok_or(
        "NO RebalancePolicy EXISTS — treasury_withdraw fails closed for every destination. \
         Initialization has not been performed.",
    )?;

    println!("On-chain RebalancePolicy:");
    println!("  version                   = {}", policy.version);
    println!("  ALLOWLIST (ordered):");
    for (i, t) in policy.treasuries.iter().enumerate() {
        println!("    [{i}] {t}");
    }

    if let Some(p) = &live.pending {
        println!(
            "\n  WARNING: a policy change is PENDING (eta {}). If you did not queue this, \
             treat it as an incident and cancel it with `plan --action cancel`.",
            p.eta
        );
    }

    match verify_policy_matches(policy, &expect_treasuries) {
        Ok(()) => {
            println!("\nVERIFIED: the live policy matches every expected value exactly.");
            Ok(())
        }
        Err(problems) => {
            println!("\nMISMATCH — the live policy is NOT what you expected:");
            for p in &problems {
                println!("  - {p}");
            }
            Err(format!(
                "{} mismatch(es); policy NOT verified",
                problems.len()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    // Offline tests for `glc-rebalance-policy`.
    //
    // Every test here runs without a network or a cluster: the pure
    // verification functions are called directly, and the full
    // plan -> attest -> execute pipeline runs against an in-memory
    // `MockRpc`. What the on-chain program enforces authoritatively is
    // tested in `programs/glc-reserve-bridge/tests/rebalance_policy.rs`;
    // these tests cover the TOOLING's own obligations — that it refuses a
    // tampered plan, refuses a stale epoch, refuses parameters the program
    // would reject, never broadcasts without `--execute`, and builds the
    // exact account ordering the program expects.

    use super::*;
    use glc_reserve_bridge_service::solana::rpc::{SimulationOutcome, SolanaRpcError};
    use solana_sdk::account::Account;
    use solana_sdk::hash::Hash;
    use solana_sdk::signature::{Keypair, Signature as Sig};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const EPOCH: u64 = 7;
    const THRESHOLD: u8 = 2;

    // ------------------------------------------------------------- fixtures --

    struct MockRpc {
        accounts: HashMap<Pubkey, Account>,
        simulate_err: Option<String>,
        sent: Mutex<Vec<Transaction>>,
    }

    impl MockRpc {
        fn new() -> Self {
            Self {
                accounts: HashMap::new(),
                simulate_err: None,
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
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

    fn program_account(data: Vec<u8>) -> Account {
        Account {
            lamports: 1_000_000_000,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Matches `accounts::decode_bridge_config`'s exact layout, including the
    /// VARIABLE-LENGTH `Option<Pubkey>` pending_admin (1-byte `None` tag here).
    fn fake_bridge_config(mint: &Pubkey, token_program: &Pubkey) -> Account {
        let mut data = vec![0u8; 8]; // discriminator
        data.push(1); // protocol_version
        data.extend_from_slice(Pubkey::new_unique().as_ref()); // admin
        data.push(0); // pending_admin = None
        data.push(1); // paused
        data.push(0); // release_paused
        data.push(0); // deposit_paused
        data.push(0); // bump
        data.extend_from_slice(mint.as_ref());
        data.extend_from_slice(token_program.as_ref());
        data.push(0); // reserve_authority_bump
        data.extend_from_slice(&0u64.to_le_bytes()); // obligation_count
        data.extend_from_slice(&86_400i64.to_le_bytes()); // governance_timelock_seconds
        data.extend_from_slice(&100_000_000u64.to_le_bytes()); // min_transfer_amount
        data.extend_from_slice(&20_000_000_000u64.to_le_bytes()); // per_transfer_limit
        data.extend_from_slice(&20_000_000_000u64.to_le_bytes()); // protected_minimum
        data.extend_from_slice(&100_000_000_000u64.to_le_bytes()); // rolling_volume_limit
        data.extend_from_slice(&86_400i64.to_le_bytes()); // rolling_window_seconds
        program_account(data)
    }

    /// Matches `accounts::decode_attestation_key_set`: epoch u64 | threshold
    /// u8 | bump u8 | Vec<Pubkey> (u32 LE length prefix).
    fn fake_key_set(epoch: u64, threshold: u8, keys: &[Pubkey]) -> Account {
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&epoch.to_le_bytes());
        data.push(threshold);
        data.push(0); // bump
        data.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for k in keys {
            data.extend_from_slice(k.as_ref());
        }
        program_account(data)
    }

    /// Matches `accounts::decode_rebalance_policy`.
    fn fake_policy(version: u64, treasuries: &[Pubkey]) -> Account {
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&version.to_le_bytes());
        data.push(0); // bump
        data.push(treasuries.len() as u8);
        for i in 0..MAX_TREASURY_DESTINATIONS {
            match treasuries.get(i) {
                Some(t) => data.extend_from_slice(t.as_ref()),
                None => data.extend_from_slice(Pubkey::default().as_ref()),
            }
        }
        data.extend_from_slice(&[0u8; 64]); // reserved
        program_account(data)
    }

    /// Matches `accounts::decode_pending_rebalance_policy`.
    fn fake_pending(eta: i64, epoch: u64, treasuries: &[Pubkey]) -> Account {
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&epoch.to_le_bytes());
        data.extend_from_slice(&eta.to_le_bytes());
        data.push(treasuries.len() as u8);
        for i in 0..MAX_TREASURY_DESTINATIONS {
            match treasuries.get(i) {
                Some(t) => data.extend_from_slice(t.as_ref()),
                None => data.extend_from_slice(Pubkey::default().as_ref()),
            }
        }
        data.push(0); // bump
        data.extend_from_slice(&[0u8; 32]); // reserved
        program_account(data)
    }

    struct Fixture {
        rpc: MockRpc,
        mint: Pubkey,
        token_program: Pubkey,
        treasury: Pubkey,
        signers: Vec<Keypair>,
    }

    fn spl_token_program() -> Pubkey {
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
    }

    /// A bridge with a configured reserve, a 2-of-3 attestation key set, and
    /// NO policy yet — the exact state the migration starts from.
    fn fixture_without_policy() -> Fixture {
        let mint = Pubkey::new_unique();
        let token_program = spl_token_program();
        let signers: Vec<Keypair> = (0..3).map(|_| Keypair::new()).collect();
        let keys: Vec<Pubkey> = signers.iter().map(|k| k.pubkey()).collect();

        let mut rpc = MockRpc::new();
        rpc.accounts.insert(
            accounts::bridge_config_pda(),
            fake_bridge_config(&mint, &token_program),
        );
        rpc.accounts.insert(
            accounts::attestation_key_set_pda(),
            fake_key_set(EPOCH, THRESHOLD, &keys),
        );
        Fixture {
            rpc,
            mint,
            token_program,
            treasury: Pubkey::new_unique(),
            signers,
        }
    }

    /// Builds a plan exactly as `cmd_plan` does, without any RPC.
    fn make_plan(
        fx: &Fixture,
        action: PolicyAction,
        treasuries: &[Pubkey],
        epoch: u64,
    ) -> PlanFile {
        let reserve_authority = accounts::reserve_authority_pda();
        let reserve_token_account =
            accounts::associated_token_address(&reserve_authority, &fx.mint, &fx.token_program);
        let raw: Vec<[u8; 32]> = treasuries.iter().map(|t| t.to_bytes()).collect();
        let params = rebalance_policy_params(&raw);
        let commitment = solana_sdk::hash::hash(&params).to_bytes();
        let message = governance_message(
            PROTOCOL_VERSION,
            &PROGRAM_ID.to_bytes(),
            epoch,
            action.action_byte(),
            &commitment,
        );
        PlanFile {
            action,
            program_id: PROGRAM_ID.to_string(),
            reserve_mint: fx.mint.to_string(),
            token_program: fx.token_program.to_string(),
            reserve_authority: reserve_authority.to_string(),
            reserve_token_account: reserve_token_account.to_string(),
            attestation_epoch: epoch,
            attestation_threshold: THRESHOLD,
            protocol_version: PROTOCOL_VERSION,
            treasuries: treasuries.iter().map(|t| t.to_string()).collect(),
            pending_eta: 0,
            current_policy_version: None,
            params_commitment_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&commitment),
            message_hex: glc_reserve_bridge_service::goldcoin::hex::encode(&message),
        }
    }

    fn sign_plan(plan: &PlanFile, signers: &[&Keypair]) -> AttestedPlanFile {
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let attestations = signers
            .iter()
            .map(|k| {
                let sig = k.sign_message(&message);
                AttestationEntry {
                    pubkey: k.pubkey().to_string(),
                    signature_hex: glc_reserve_bridge_service::goldcoin::hex::encode(sig.as_ref()),
                }
            })
            .collect();
        AttestedPlanFile {
            plan: plan.clone(),
            attestations,
        }
    }

    // -------------------------------------------- successful initialization --

    #[tokio::test]
    async fn a_threshold_attested_initialization_simulates_and_broadcasts() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[1]]);
        let payer = Keypair::new();

        let outcome = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect("a correctly attested initialization must succeed");

        assert!(outcome.simulation_succeeded);
        assert!(outcome.broadcast, "--execute was supplied");
        assert_eq!(fx.rpc.sent_count(), 1);

        let sent = fx.rpc.sent.lock().unwrap();
        let ixs = &sent[0].message.instructions;
        assert_eq!(ixs.len(), 2, "ed25519 proof + initialize_rebalance_policy");
    }

    #[test]
    fn the_built_transaction_puts_the_proof_immediately_before_the_governance_instruction() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let attestations: Vec<(Pubkey, Signature)> = fx.signers[..2]
            .iter()
            .map(|k| (k.pubkey(), k.sign_message(&message)))
            .collect();
        let payer = Pubkey::new_unique();

        let ixs = build_instructions(&plan, &attestations, &payer).unwrap();
        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0].program_id, solana_sdk::ed25519_program::ID);
        assert_eq!(ixs[1].program_id, PROGRAM_ID);
        // Account order must match `InitializeRebalancePolicy` exactly.
        assert_eq!(ixs[1].accounts[0].pubkey, payer);
        assert!(ixs[1].accounts[0].is_signer && ixs[1].accounts[0].is_writable);
        assert_eq!(ixs[1].accounts[1].pubkey, accounts::bridge_config_pda());
        assert_eq!(
            ixs[1].accounts[2].pubkey,
            accounts::attestation_key_set_pda()
        );
        assert_eq!(ixs[1].accounts[3].pubkey, accounts::rebalance_policy_pda());
        assert!(ixs[1].accounts[3].is_writable, "policy is created here");
    }

    /// The admin key appears nowhere in a policy transaction. This is the
    /// property that keeps allowlist creation outside a compromised admin's
    /// blast radius, so it is asserted directly rather than left implicit.
    #[test]
    fn no_policy_instruction_requires_the_admin_to_sign() {
        let fx = fixture_without_policy();
        let payer = Pubkey::new_unique();
        for action in [
            PolicyAction::Init,
            PolicyAction::Propose,
            PolicyAction::Cancel,
        ] {
            let mut plan = make_plan(&fx, action, &[fx.treasury], EPOCH);
            let message =
                glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
            let attestations: Vec<(Pubkey, Signature)> = fx.signers[..2]
                .iter()
                .map(|k| (k.pubkey(), k.sign_message(&message)))
                .collect();
            // `cancel` carries no treasuries on the wire.
            if action == PolicyAction::Cancel {
                plan.treasuries.clear();
            }
            let ixs = build_instructions(&plan, &attestations, &payer).unwrap();
            let signers: Vec<Pubkey> = ixs[1]
                .accounts
                .iter()
                .filter(|a| a.is_signer)
                .map(|a| a.pubkey)
                .collect();
            assert_eq!(
                signers,
                vec![payer],
                "{} must require exactly one signer — the fee payer, which confers no authority",
                action.as_str()
            );
        }
    }

    // ---------------------------------------------- insufficient signatures --

    #[tokio::test]
    async fn a_single_signature_below_the_threshold_is_refused() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0]]);
        let payer = Keypair::new();

        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("one of two required signatures must not be enough");
        assert!(err.contains("threshold"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0, "nothing may be broadcast");
    }

    #[tokio::test]
    async fn an_initialization_with_no_attestations_at_all_is_refused() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = AttestedPlanFile {
            plan,
            attestations: Vec::new(),
        };
        let payer = Keypair::new();

        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("an unattested initialization must be refused");
        assert!(err.contains("threshold"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0);
    }

    /// A signature from a key that is not in the current on-chain set is
    /// discarded, not counted — so an attacker's own keypair cannot make up
    /// the threshold.
    #[tokio::test]
    async fn signatures_from_non_attestation_keys_do_not_count_toward_the_threshold() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let stranger = Keypair::new();
        let attested = sign_plan(&plan, &[&fx.signers[0], &stranger]);
        let payer = Keypair::new();

        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("a stranger's signature must not complete the threshold");
        assert!(err.contains("threshold"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0);
    }

    /// The same key signing twice is one signature, not two.
    #[test]
    fn a_duplicated_signature_counts_once() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[0]]);
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&plan.message_hex).unwrap();
        let keys: Vec<Pubkey> = fx.signers.iter().map(|k| k.pubkey()).collect();

        let err = collect_valid_attestations(&attested.attestations, &message, &keys, THRESHOLD)
            .expect_err("one key signing twice is still one signer");
        assert!(err.contains("threshold"), "unexpected error: {err}");
    }

    // ---------------------------------------------------- replay resistance --

    /// A governance attestation binds the attestation-key epoch. Once the keys
    /// rotate, every signature collected under the old epoch is void — the
    /// tooling says so rather than letting it fail opaquely on chain.
    #[tokio::test]
    async fn a_governance_attestation_from_a_previous_epoch_is_refused() {
        let mut fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[1]]);

        // The keys rotate: same keys, new epoch.
        let keys: Vec<Pubkey> = fx.signers.iter().map(|k| k.pubkey()).collect();
        fx.rpc.accounts.insert(
            accounts::attestation_key_set_pda(),
            fake_key_set(EPOCH + 1, THRESHOLD, &keys),
        );

        let payer = Keypair::new();
        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("an attestation from a rotated-away epoch must be refused");
        assert!(err.contains("epoch"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0);
    }

    /// An approval to CREATE the first policy must never be replayable as an
    /// approval to REPLACE an existing one: the two use different action
    /// bytes, so the message an init attestation covers is not the message a
    /// propose requires.
    #[test]
    fn an_init_approval_does_not_produce_a_valid_propose_message() {
        let fx = fixture_without_policy();
        let init = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let propose = make_plan(&fx, PolicyAction::Propose, &[fx.treasury], EPOCH);

        assert_ne!(
            init.message_hex, propose.message_hex,
            "identical parameters under different actions must still produce different messages"
        );
        let message =
            glc_reserve_bridge_service::goldcoin::hex::decode_vec(&propose.message_hex).unwrap();
        let attested_init = sign_plan(&init, &[&fx.signers[0], &fx.signers[1]]);
        let keys: Vec<Pubkey> = fx.signers.iter().map(|k| k.pubkey()).collect();
        let err =
            collect_valid_attestations(&attested_init.attestations, &message, &keys, THRESHOLD)
                .expect_err("an init attestation must not authorize a propose");
        assert!(err.contains("threshold"), "unexpected error: {err}");
    }

    /// Retargeting a signed plan at different parameters is caught by the
    /// commitment before any signature is even examined.
    #[test]
    fn editing_the_treasury_in_a_signed_plan_is_detected() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let mut tampered = plan.clone();
        tampered.treasuries = vec![Pubkey::new_unique().to_string()];

        let err =
            verify_plan_not_tampered(&tampered).expect_err("a swapped treasury must be caught");
        assert!(err.contains("TAMPERED"), "unexpected error: {err}");
    }

    /// Recomputing the commitment is not enough on its own: the governance
    /// message must also match, or a plan could be re-pointed at another
    /// action/epoch while keeping a self-consistent commitment.
    #[test]
    fn editing_the_recorded_message_is_detected_even_with_a_consistent_commitment() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let other = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH + 5);
        let mut tampered = plan.clone();
        tampered.message_hex = other.message_hex;

        let err =
            verify_plan_not_tampered(&tampered).expect_err("a swapped message must be caught");
        assert!(err.contains("TAMPERED"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn a_tampered_plan_is_refused_before_any_transaction_is_built() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let mut attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[1]]);
        attested.plan.treasuries = vec![Pubkey::new_unique().to_string()];

        let payer = Keypair::new();
        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("a tampered attested plan must be refused");
        assert!(err.contains("TAMPERED"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0);
    }

    // ------------------------------------------------------- dry-run posture --

    #[tokio::test]
    async fn without_execute_nothing_is_broadcast() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[1]]);
        let payer = Keypair::new();

        let outcome = execute_governance(&fx.rpc, &attested, &payer, false)
            .await
            .expect("a dry run over a valid plan still simulates successfully");

        assert!(outcome.simulation_succeeded);
        assert!(!outcome.broadcast, "no --execute means no broadcast");
        assert!(outcome.signature.is_none());
        assert_eq!(fx.rpc.sent_count(), 0, "the dry run changed no state");
    }

    #[tokio::test]
    async fn a_failed_simulation_is_never_broadcast_even_with_execute() {
        let mut fx = fixture_without_policy();
        fx.rpc.simulate_err = Some("custom program error: 0x1771".to_string());
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[1]]);
        let payer = Keypair::new();

        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("a failed simulation must block the broadcast");
        assert!(err.contains("simulation failed"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0);
    }

    // -------------------------------------------------- action preconditions --

    #[tokio::test]
    async fn initializing_when_a_policy_already_exists_is_refused() {
        let mut fx = fixture_without_policy();
        fx.rpc.accounts.insert(
            accounts::rebalance_policy_pda(),
            fake_policy(0, &[fx.treasury]),
        );
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let attested = sign_plan(&plan, &[&fx.signers[0], &fx.signers[1]]);
        let payer = Keypair::new();

        let err = execute_governance(&fx.rpc, &attested, &payer, true)
            .await
            .expect_err("initialization is one-time");
        assert!(err.contains("ALREADY EXISTS"), "unexpected error: {err}");
        assert_eq!(fx.rpc.sent_count(), 0);
    }

    #[test]
    fn proposing_without_an_existing_policy_is_refused() {
        let err = verify_action_preconditions(PolicyAction::Propose, false, false)
            .expect_err("propose replaces an existing policy");
        assert!(err.contains("no RebalancePolicy exists"), "got: {err}");
    }

    #[test]
    fn a_second_pending_change_is_refused() {
        let err = verify_action_preconditions(PolicyAction::Propose, true, true)
            .expect_err("the pending slot is a singleton");
        assert!(err.contains("ALREADY PENDING"), "got: {err}");
    }

    #[test]
    fn cancelling_with_nothing_pending_is_refused() {
        let err = verify_action_preconditions(PolicyAction::Cancel, true, false)
            .expect_err("nothing to cancel");
        assert!(err.contains("nothing to cancel"), "got: {err}");
    }

    #[test]
    fn the_normal_migration_preconditions_are_accepted() {
        verify_action_preconditions(PolicyAction::Init, false, false).unwrap();
        verify_action_preconditions(PolicyAction::Propose, true, false).unwrap();
        verify_action_preconditions(PolicyAction::Cancel, true, true).unwrap();
    }

    // ----------------------------------------------------- parameter validation --

    #[test]
    fn an_empty_allowlist_is_refused() {
        let reserve = Pubkey::new_unique();
        let err = validate_policy_params(&[], &reserve).expect_err("empty allowlist");
        assert!(err.contains("at least one --treasury"), "got: {err}");
    }

    #[test]
    fn more_than_four_treasuries_are_refused() {
        let reserve = Pubkey::new_unique();
        let many: Vec<Pubkey> = (0..5).map(|_| Pubkey::new_unique()).collect();
        let err = validate_policy_params(&many, &reserve).expect_err("too many");
        assert!(err.contains("at most"), "got: {err}");
    }

    #[test]
    fn allowlisting_the_reserve_itself_is_refused() {
        let reserve = Pubkey::new_unique();
        let err = validate_policy_params(&[reserve], &reserve)
            .expect_err("the reserve may not be its own treasury");
        assert!(err.contains("IS the reserve token account"), "got: {err}");
    }

    #[test]
    fn a_duplicate_treasury_is_refused() {
        let reserve = Pubkey::new_unique();
        let t = Pubkey::new_unique();
        let err = validate_policy_params(&[t, t], &reserve).expect_err("duplicate");
        assert!(err.contains("more than once"), "got: {err}");
    }

    #[test]
    fn the_all_zero_treasury_is_refused() {
        let reserve = Pubkey::new_unique();
        let err =
            validate_policy_params(&[Pubkey::default()], &reserve).expect_err("all-zero pubkey");
        assert!(err.contains("all-zero"), "got: {err}");
    }

    /// The allowlist is the whole policy: a valid list of destinations is
    /// a valid policy, with no amount, rate or window parameter to get
    /// wrong alongside it.
    #[test]
    fn the_production_shape_of_policy_passes_validation() {
        let reserve = Pubkey::new_unique();
        let t = Pubkey::new_unique();
        validate_policy_params(&[t], &reserve)
            .expect("a single canonical treasury is the production policy");
    }

    // ------------------------------------------------------------------ verify --

    fn snapshot(treasuries: Vec<Pubkey>) -> RebalancePolicySnapshot {
        RebalancePolicySnapshot {
            version: 0,
            treasuries,
        }
    }

    #[test]
    fn verify_accepts_an_exact_match() {
        let t = Pubkey::new_unique();
        let policy = snapshot(vec![t]);
        verify_policy_matches(&policy, &[t]).expect("an exact match must verify");
    }

    #[test]
    fn verify_rejects_a_different_treasury() {
        let t = Pubkey::new_unique();
        let policy = snapshot(vec![t]);
        let problems = verify_policy_matches(&policy, &[Pubkey::new_unique()])
            .expect_err("a different treasury must not verify");
        assert!(problems[0].contains("allowlist mismatch"));
    }

    /// Allowlist ORDER is what the attestation committed to, so a reordered
    /// list is a different policy and must not silently verify.
    #[test]
    fn verify_is_order_sensitive() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let policy = snapshot(vec![a, b]);
        assert!(verify_policy_matches(&policy, &[b, a]).is_err());
    }

    /// A shrinking allowlist is a mismatch too — a policy that lost an
    /// entry must never verify against the list that still names it.
    #[test]
    fn verify_rejects_a_shorter_allowlist() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let policy = snapshot(vec![a]);
        let problems =
            verify_policy_matches(&policy, &[a, b]).expect_err("a shorter list must not verify");
        assert_eq!(problems.len(), 1, "got: {problems:?}");
    }

    // --------------------------------------------------------------- plumbing --

    #[test]
    fn the_protocol_version_matches_the_shared_claim_families() {
        // docs/29 section 6: PROTOCOL_VERSION stays 1 across this change.
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn the_three_governance_actions_use_distinct_action_bytes() {
        let bytes = [
            PolicyAction::Init.action_byte(),
            PolicyAction::Propose.action_byte(),
            PolicyAction::Cancel.action_byte(),
        ];
        let unique: std::collections::HashSet<u8> = bytes.iter().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "each action must be independently approvable"
        );
        // And none of them may collide with the withdrawal claim families.
        assert!(!bytes.contains(&glc_reserve_bridge_shared::claim::ACTION_TREASURY_WITHDRAW));
        assert!(!bytes.contains(&glc_reserve_bridge_shared::claim::ACTION_REFUND_WITHDRAW));
    }

    /// A governance message and a withdrawal claim can never be confused: they
    /// use different domain tags and different lengths, so a signature over
    /// one is not a signature over the other.
    #[test]
    fn governance_messages_are_distinct_from_withdrawal_claims() {
        let commitment = [7u8; 32];
        let gov = governance_message(
            PROTOCOL_VERSION,
            &PROGRAM_ID.to_bytes(),
            EPOCH,
            ACTION_INITIALIZE_REBALANCE_POLICY,
            &commitment,
        );
        let treasury_claim = glc_reserve_bridge_shared::claim::treasury_withdraw_claim_message(
            PROTOCOL_VERSION,
            &PROGRAM_ID.to_bytes(),
            EPOCH,
            1,
            1,
            &[1u8; 32],
            &[2u8; 32],
            &[3u8; 32],
            0,
        );
        assert_ne!(gov.len(), treasury_claim.len());
        assert_ne!(&gov[0..16], &treasury_claim[0..16], "domain tags differ");
    }

    #[test]
    fn the_action_flag_rejects_unknown_actions() {
        assert!(PolicyAction::parse("withdraw").is_err());
        assert!(PolicyAction::parse("").is_err());
        assert_eq!(PolicyAction::parse("init").unwrap(), PolicyAction::Init);
        assert_eq!(
            PolicyAction::parse("propose").unwrap(),
            PolicyAction::Propose
        );
        assert_eq!(PolicyAction::parse("cancel").unwrap(), PolicyAction::Cancel);
    }

    #[test]
    fn a_plan_survives_a_json_round_trip_unchanged() {
        let fx = fixture_without_policy();
        let plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let back: PlanFile = serde_json::from_str(&json).unwrap();
        verify_plan_not_tampered(&back).expect("a round-tripped plan must still verify");
        assert_eq!(back.message_hex, plan.message_hex);
    }

    #[test]
    fn a_plan_for_another_program_id_is_refused() {
        let fx = fixture_without_policy();
        let mut plan = make_plan(&fx, PolicyAction::Init, &[fx.treasury], EPOCH);
        plan.program_id = Pubkey::new_unique().to_string();
        let err = verify_plan_not_tampered(&plan).expect_err("wrong deployment");
        assert!(err.contains("this build is compiled against"), "got: {err}");
    }

    #[tokio::test]
    async fn a_pending_change_is_decoded_and_its_eta_is_what_cancel_binds() {
        let mut fx = fixture_without_policy();
        fx.rpc.accounts.insert(
            accounts::rebalance_policy_pda(),
            fake_policy(0, &[fx.treasury]),
        );
        fx.rpc.accounts.insert(
            accounts::pending_rebalance_policy_pda(),
            fake_pending(1_800_000_000, EPOCH, &[fx.treasury]),
        );
        let live = fetch_live_state(&fx.rpc, None, None).await.unwrap();
        assert_eq!(live.pending.as_ref().unwrap().eta, 1_800_000_000);
        assert!(live.policy.is_some());
        verify_action_preconditions(PolicyAction::Cancel, true, true).unwrap();
    }
}
