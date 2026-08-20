//! `glc-mainnet-bootstrap` — simulation-first, one-off production
//! bootstrap tool for the already-deployed Goldcoin Solana Reserve
//! Bridge program (docs/22-production-readiness-review.md §27
//! "Mainnet deployment requirements").
//!
//! # What this is, and is not
//!
//! This is a standalone, one-shot CLI — deliberately isolated from
//! `glc-bridge-daemon` (no shared config file, no shared runtime, no
//! shared entry point). It exists to run `initialize` and
//! `initialize_reserve_vault` — the two one-time, admin/upgrade-authority-
//! gated instructions that create the bridge's on-chain state — exactly
//! once, against the real deployed mainnet program, with every possible
//! precaution a bridge that will custody real assets deserves:
//!
//! - **Simulation by default.** Every run simulates both instructions.
//!   Nothing is ever broadcast unless `--execute` is explicitly supplied
//!   AND that specific instruction's own simulation succeeded.
//! - **No invented values.** Every economic/governance parameter
//!   (`--min-transfer-amount`, `--per-transfer-limit`,
//!   `--protected-minimum`, `--rolling-volume-limit`,
//!   `--rolling-window-seconds`, `--governance-timelock-seconds`,
//!   `--upgrade-timelock-seconds`) is a REQUIRED CLI argument with no
//!   default — this tool never hardcodes any of them, even now that
//!   docs/22-production-readiness-review.md P0-5/P0-6 records approved
//!   pilot values for all seven (see "Approved pilot bridge-policy
//!   parameters" below): the operator must still supply them explicitly,
//!   every run, from that documented source of truth.
//! - **Reuses the real instruction builders.** [`instructions::initialize`]
//!   and [`instructions::initialize_reserve_vault`]
//!   (`service/src/solana/instructions.rs`) are used completely
//!   unmodified — this tool adds a preflight/simulation harness around
//!   them, never a second implementation of the wire encoding.
//! - **Never generates or overwrites a keypair.** The deployer/upgrade-
//!   authority keypair is loaded via `read_keypair_file` — read-only —
//!   and nothing else in this binary ever calls `Keypair::new()` or
//!   writes a keypair file anywhere.
//!
//! # Why `--program-id` is a required argument even though it must equal
//! a compiled-in constant
//!
//! [`accounts::PROGRAM_ID`] is fixed at compile time (docs/22 P0-6 — see
//! that item for the incident this constant's correctness was itself
//! only recently confirmed against). Requiring the operator to also
//! state, on the command line, which program they believe they're
//! targeting — and refusing outright on any mismatch — is a deliberate,
//! cheap, independent cross-check: a copy/paste error or stale shell
//! history naming the wrong program id is caught here, in plain text,
//! before a single RPC call is made, rather than discovered later
//! against a live simulation result.
//!
//! # ⚠ THE CURRENTLY COMPILED-IN PROGRAM ID IS RETIRED — NOT A VALID
//! DEPLOYMENT TARGET ⚠
//!
//! `7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn` — the program id
//! [`accounts::PROGRAM_ID`] still holds today — was the deployed mainnet
//! program docs/22 P0-6 investigated. **That program has since been
//! permanently CLOSED and its rent reclaimed.** It no longer exists on
//! chain in any form and must never be interacted with, targeted, or
//! reused for a future deployment. See [`RETIRED_PROGRAM_IDS`] below,
//! and docs/22-production-readiness-review.md P0-6's own "Update" entry
//! for the full record.
//!
//! This build's compiled-in `declare_id!`/[`accounts::PROGRAM_ID`] has
//! been left **unchanged** (still the retired id) rather than swapped to
//! a guessed or placeholder value — the real future production program
//! id does not exist yet (it will come from a fresh `solana-keygen new`
//! run, not yet performed), and changing `declare_id!` to anything else
//! before that id exists would just replace one wrong hardcoded value
//! with another. **This means every invocation of this tool today will
//! refuse at the retired-id check below** — that is the correct,
//! intentional behavior until a real replacement id exists, not a bug.
//!
//! ## The future program-id replacement workflow (not yet performed)
//!
//! 1. Generate a new Solana program keypair.
//! 2. Obtain the new program id.
//! 3. Update `declare_id!` (`programs/glc-reserve-bridge/src/lib.rs`) and
//!    every compile-time program-id dependency — per docs/22 P0-6, that
//!    means `glc_reserve_bridge_shared::PROGRAM_ID_BYTES`
//!    (`shared/src/lib.rs`, the single authoritative source both
//!    `declare_id!` and [`accounts::PROGRAM_ID`] are checked against) and
//!    `Anchor.toml`'s `[programs.localnet]` entry.
//! 4. Rebuild the on-chain program (`anchor build`).
//! 5. Rebuild/retest the service (`cargo +nightly test`, this crate).
//! 6. Verify instruction builders, PDA derivations, and the attestation
//!    domain separator all use the new id — the exact same pin tests
//!    this fix already added (`solana::accounts::tests::
//!    program_id_is_the_deployed_mainnet_address`,
//!    `every_pda_helper_derives_against_program_id`,
//!    `solana::instructions::tests::
//!    every_builder_targets_the_deployed_mainnet_program_id`,
//!    `signing::attestation::tests::
//!    attestation_domain_separator_is_the_deployed_mainnet_address`,
//!    and the on-chain `program_id_tests`) will need their expected
//!    literal updated to the new id and will otherwise fail closed —
//!    they are not disabled by this change, they are the mechanism that
//!    enforces step 6 actually happened.
//! 7. Deploy under that NEW program id.
//! 8. Verify the deployed binary (the same read-only SHA-256/embedded-id
//!    comparison technique used for the retired program).
//! 9. Run this tool's bootstrap simulation against the new id.
//! 10. Review all production parameters — the seven bridge-policy
//!     values now have approved pilot values (see "Approved pilot
//!     bridge-policy parameters" below); re-confirm they're still the
//!     intended values before using them.
//! 11. Execute initialization only after explicit approval.
//!
//! Steps 1-2 (generating the new keypair) are explicitly **not**
//! performed by this tool or by any code in this repository — see
//! [`RETIRED_PROGRAM_IDS`]'s own docs for why this tool refuses to ever
//! generate a program keypair itself.

use std::path::PathBuf;
use std::str::FromStr;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signer};
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::solana::accounts::{self, PROGRAM_ID};
use glc_reserve_bridge_service::solana::confirm::{confirm_transaction, ConfirmPolicy};
use glc_reserve_bridge_service::solana::instructions;
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

const USAGE: &str = "glc-mainnet-bootstrap — simulation-first, one-off production bootstrap for
the already-deployed Goldcoin Solana Reserve Bridge program.

DEFAULT BEHAVIOR IS SIMULATION ONLY. Nothing is ever broadcast to the
network unless --execute is explicitly supplied, and even then only after
that specific instruction's own simulation succeeds.

USAGE
  glc-mainnet-bootstrap \\
      --rpc-url URL \\
      --program-id PUBKEY \\
      --deployer-keypair PATH \\
      --reserve-mint PUBKEY \\
      --token-program PUBKEY \\
      --attestation-keys PUBKEY,PUBKEY,PUBKEY \\
      --attestation-threshold N \\
      --governance-timelock-seconds N \\
      --min-transfer-amount RAW_UNITS \\
      --per-transfer-limit RAW_UNITS \\
      --protected-minimum RAW_UNITS \\
      --rolling-volume-limit RAW_UNITS \\
      --rolling-window-seconds N \\
      --upgrade-timelock-seconds N \\
      [--execute]

REQUIRED ARGUMENTS
  --rpc-url URL
      Solana RPC endpoint. Reads only unless --execute is supplied, in
      which case the two bootstrap transactions are also submitted here.

  --program-id PUBKEY
      The program id you believe is deployed and being targeted. Must
      equal this build's compiled-in program id
      (glc_reserve_bridge_service::solana::accounts::PROGRAM_ID) or the
      tool refuses immediately, before any RPC call — see this file's own
      module docs for why this check exists even though the value is
      already fixed at compile time.

  --deployer-keypair PATH
      Path to the LOCAL keypair file for the account you believe is the
      program's current upgrade authority. Read-only (read_keypair_file)
      — this tool never generates, overwrites, or replaces any keypair.
      Verified live against the program's actual on-chain upgrade
      authority before anything else happens.

  --reserve-mint PUBKEY
      The existing Solana GLC SPL token mint (docs/12-management-
      decisions.md item 10 — already resolved: the live mint is
      Token-2022, 6 decimals). Never assumed; the actual owning token
      program is independently verified via a live read of this mint
      account and cross-checked against --token-program below.

  --token-program PUBKEY
      Which SPL token program you believe owns --reserve-mint (legacy SPL
      Token or Token-2022). Cross-checked against a live RPC read of the
      mint account itself; refused on any mismatch, never trusted from
      this flag alone.

  --attestation-keys PUBKEY,PUBKEY,...
      The attestation-signer public keys, in the exact order they should
      be recorded on-chain, comma-separated. For the approved trust model
      (docs/02-trust-model.md, docs/12 item 1) this is 3 keys — the count
      itself is not hardcoded here, only validated against the same rules
      programs/glc-reserve-bridge/src/validation.rs enforces on-chain (see
      that module for the authoritative rules this tool's own preflight
      check is a deliberately duplicated, documented copy of).

  --attestation-threshold N
      The M in the M-of-N attestation threshold. Must be >= 2 (a
      threshold of 1 would let a single key release reserves — ruled out
      by the approved trust model) and <= the number of
      --attestation-keys supplied.

  --governance-timelock-seconds N
  --min-transfer-amount RAW_UNITS
  --per-transfer-limit RAW_UNITS
  --protected-minimum RAW_UNITS
  --rolling-volume-limit RAW_UNITS
  --rolling-window-seconds N
  --upgrade-timelock-seconds N
      Every one of these is a real production/governance decision this
      tool never hardcodes or defaults — supply the actual approved
      value for each, every run. Approved pilot values are recorded in
      docs/22-production-readiness-review.md P0-6, section 'Approved
      pilot bridge-policy parameters', and reproduced in this file's own
      EXAMPLE section below — this help text intentionally does not
      duplicate the numbers here, so there is exactly one place an
      operator can go stale against. All *-amount/*-limit/*-minimum
      values are RAW atomic units in the reserve mint's own decimals (6,
      for the live GLC mint) — e.g. a 10,000 GLC per-transfer limit is
      --per-transfer-limit 10000000000, computed by the caller, never by
      this tool.

OPTIONAL ARGUMENTS
  --execute
      Broadcast, rather than only simulate. Gated per-instruction: even
      with this flag, `initialize` is only ever submitted if its own
      simulation just succeeded, and `initialize_reserve_vault` is only
      ever submitted if `initialize` was actually confirmed on chain
      first AND initialize_reserve_vault's own simulation then succeeds.
      Omit this flag (the default) to simulate only — nothing is
      broadcast, no fees are spent, no state changes.

  -h, --help
      Print this message.

⚠ RETIRED PROGRAM ID: 7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn was the
mainnet program this tool was originally built against. IT HAS SINCE BEEN
PERMANENTLY CLOSED (rent reclaimed) and must never be targeted again —
this tool refuses immediately, before any RPC call, if --program-id names
it. This build's compiled-in program id has NOT yet been updated to a new
production id (none exists yet) — see this file's own module docs for the
full future replacement workflow. Every invocation of this tool today
will therefore refuse; that is intentional, not a bug.

WHAT THIS TOOL CHECKS BEFORE BUILDING ANY TRANSACTION
  - --program-id is not a known-retired program id (see above) — checked
    first, before any other check and before any RPC call.
  - --program-id matches this build's compiled-in program id.
  - The attestation key/threshold set passes the same validation rules
    the on-chain program itself enforces.
  - The program account and its ProgramData account both exist, are
    owned by the upgradeable BPF loader, and agree with each other.
  - --deployer-keypair's public key is the program's CURRENT real
    upgrade authority, read live from the ProgramData account.
  - --reserve-mint exists and is owned by the token program you claimed.
  - The bridge_config PDA does NOT already exist (refuses to proceed if
    the bridge is already initialized).
  - Every PDA/account either instruction will touch is derived and
    printed before any transaction is built.

EXAMPLE (simulation-only — will still refuse today; see the RETIRED
PROGRAM ID warning above. <NEW_PRODUCTION_PROGRAM_ID> is a PLACEHOLDER,
never the retired 7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn — substitute
the real future production program id once one exists. --reserve-mint/
--token-program are the live GLC Token-2022 mint (docs/12-management-
decisions.md item 10), unaffected by program redeployment. --attestation-
keys are the approved 2-of-3 pilot set. All seven bridge-policy values
below are the approved pilot parameters from docs/22-production-
readiness-review.md P0-6 — copy them from there, not from memory, in
case they're ever revised.)

  glc-mainnet-bootstrap \\
      --rpc-url https://api.mainnet-beta.solana.com \\
      --program-id <NEW_PRODUCTION_PROGRAM_ID> \\
      --deployer-keypair /path/to/your/deployer-keypair.json \\
      --reserve-mint Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump \\
      --token-program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb \\
      --attestation-keys 6b27qC3fxrReuU4hL6u8iZ9AwkdngnjDxXUPwicR8WLe,G7dJ2HiEkcfJqtPGa8gQrErLaQfdZ7hcbnA173A8Y4yL,4uYKxwpWrPDyoaxjmdmJoWYLxmq2AziNMctSjTDFmynT \\
      --attestation-threshold 2 \\
      --min-transfer-amount 100000000 \\
      --per-transfer-limit 10000000000 \\
      --protected-minimum 50000000000 \\
      --rolling-volume-limit 100000000000 \\
      --rolling-window-seconds 86400 \\
      --governance-timelock-seconds 86400 \\
      --upgrade-timelock-seconds 172800

  (no --execute — this is the simulation-only form; --execute is never
  implied and must be added explicitly, only after reviewing a
  successful simulation)
";

/// Program ids this tool must refuse to ever interact with, regardless of
/// what `--program-id` or this build's compiled-in
/// [`accounts::PROGRAM_ID`] say.
///
/// Currently holds exactly one entry: `7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn`,
/// the mainnet program docs/22-production-readiness-review.md P0-6
/// investigated — it has since been **permanently closed, its rent
/// reclaimed**. It no longer exists on chain in any form; there is
/// nothing left to read, simulate against, or upgrade. A future
/// production deployment will use a *different*, not-yet-generated
/// program id (see this file's module docs for the replacement
/// workflow) — this list exists so that id, once retired, can never be
/// silently reused or accidentally targeted again, independent of
/// whatever this build's compiled-in [`accounts::PROGRAM_ID`] constant
/// currently holds (today, that constant still *is* this retired id,
/// since no replacement exists yet — this list is what actually blocks
/// operational use, not the compiled constant, which is why this check
/// runs first and does not depend on the constant's own value).
///
/// This tool never generates a program keypair itself (see `main` docs)
/// — a new id, when one exists, must be added to
/// [`accounts::PROGRAM_ID`]/`declare_id!` (the compiled target) by a
/// separate, explicit, reviewed change, never invented here.
const RETIRED_PROGRAM_IDS: &[&str] = &["7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn"];

/// Parses [`RETIRED_PROGRAM_IDS`]' literals once. Panics on a malformed
/// literal — a typo here is a build-time bug in this file, not a
/// runtime/user-input condition.
fn retired_program_ids() -> Vec<Pubkey> {
    RETIRED_PROGRAM_IDS
        .iter()
        .map(|s| Pubkey::from_str(s).expect("RETIRED_PROGRAM_IDS entries must be valid pubkeys"))
        .collect()
}

#[derive(Debug)]
struct BootstrapConfig {
    rpc_url: String,
    program_id: Pubkey,
    deployer_keypair_path: PathBuf,
    reserve_mint: Pubkey,
    token_program: Pubkey,
    attestation_keys: Vec<Pubkey>,
    attestation_threshold: u8,
    governance_timelock_seconds: i64,
    min_transfer_amount: u64,
    per_transfer_limit: u64,
    protected_minimum: u64,
    rolling_volume_limit: u64,
    rolling_window_seconds: i64,
    upgrade_timelock_seconds: i64,
    execute: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let cfg = match parse_config(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("usage error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start async runtime: {e}");
            std::process::exit(1);
        }
    };
    match rt.block_on(run(cfg)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

// ------------------------------------------------------------- CLI parsing --

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn require<'a>(args: &'a [String], name: &str) -> Result<&'a str, String> {
    flag(args, name).ok_or_else(|| format!("missing required {name}"))
}

fn require_pubkey(args: &[String], name: &str) -> Result<Pubkey, String> {
    let raw = require(args, name)?;
    Pubkey::from_str(raw).map_err(|e| format!("{name} {raw:?} is not a valid pubkey: {e}"))
}

fn require_u64(args: &[String], name: &str) -> Result<u64, String> {
    let raw = require(args, name)?;
    raw.parse()
        .map_err(|e| format!("{name} {raw:?} must be a non-negative integer: {e}"))
}

fn require_i64(args: &[String], name: &str) -> Result<i64, String> {
    let raw = require(args, name)?;
    raw.parse()
        .map_err(|e| format!("{name} {raw:?} must be an integer: {e}"))
}

fn require_u8(args: &[String], name: &str) -> Result<u8, String> {
    let raw = require(args, name)?;
    raw.parse()
        .map_err(|e| format!("{name} {raw:?} must be a small non-negative integer: {e}"))
}

fn parse_attestation_keys(args: &[String]) -> Result<Vec<Pubkey>, String> {
    let raw = require(args, "--attestation-keys")?;
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Pubkey::from_str(s).map_err(|e| format!("--attestation-keys entry {s:?}: {e}")))
        .collect()
}

fn parse_config(args: &[String]) -> Result<BootstrapConfig, String> {
    Ok(BootstrapConfig {
        rpc_url: require(args, "--rpc-url")?.to_string(),
        program_id: require_pubkey(args, "--program-id")?,
        deployer_keypair_path: PathBuf::from(require(args, "--deployer-keypair")?),
        reserve_mint: require_pubkey(args, "--reserve-mint")?,
        token_program: require_pubkey(args, "--token-program")?,
        attestation_keys: parse_attestation_keys(args)?,
        attestation_threshold: require_u8(args, "--attestation-threshold")?,
        governance_timelock_seconds: require_i64(args, "--governance-timelock-seconds")?,
        min_transfer_amount: require_u64(args, "--min-transfer-amount")?,
        per_transfer_limit: require_u64(args, "--per-transfer-limit")?,
        protected_minimum: require_u64(args, "--protected-minimum")?,
        rolling_volume_limit: require_u64(args, "--rolling-volume-limit")?,
        rolling_window_seconds: require_i64(args, "--rolling-window-seconds")?,
        upgrade_timelock_seconds: require_i64(args, "--upgrade-timelock-seconds")?,
        execute: args.iter().any(|a| a == "--execute"),
    })
}

// ------------------------------------------------- attestation-set preflight --

/// The maximum attestation-key-set size the on-chain program accepts
/// (`programs/glc-reserve-bridge/src/constants.rs::MAX_ATTESTATION_KEYS`).
/// Service never depends on the on-chain crate (docs/08-migration-
/// strategy.md workspace-split discipline) so this is a deliberately
/// duplicated literal, not an import — kept in sync by
/// `attestation_key_set_rules_match_the_on_chain_maximum` below.
const MAX_ATTESTATION_KEYS: usize = 8;

/// The minimum permitted attestation threshold
/// (`programs/glc-reserve-bridge/src/validation.rs::MIN_THRESHOLD`) — a
/// threshold of 1 would let a single attestation key release reserves
/// alone, which the approved trust model (docs/02-trust-model.md) rules
/// out. Same deliberate duplication as `MAX_ATTESTATION_KEYS` above.
const MIN_ATTESTATION_THRESHOLD: u8 = 2;

/// Mirrors `programs/glc-reserve-bridge/src/validation.rs::
/// validate_attestation_key_set` exactly — client-side, so a
/// misconfigured attestation set is rejected here, in plain text, before
/// any RPC call, rather than only surfacing as an opaque on-chain
/// simulation error. This is deliberately duplicated logic (the service
/// crate cannot depend on the on-chain crate — see this file's own
/// `MAX_ATTESTATION_KEYS` doc comment) and MUST be kept in exact sync
/// with the on-chain rules it mirrors.
fn validate_attestation_key_set(keys: &[Pubkey], threshold: u8) -> Result<(), String> {
    if keys.is_empty() {
        return Err("--attestation-keys must not be empty".to_string());
    }
    if keys.len() > MAX_ATTESTATION_KEYS {
        return Err(format!(
            "--attestation-keys has {} entries, exceeds the on-chain maximum of {}",
            keys.len(),
            MAX_ATTESTATION_KEYS
        ));
    }
    if threshold == 0 {
        return Err("--attestation-threshold must not be zero".to_string());
    }
    if usize::from(threshold) > keys.len() {
        return Err(format!(
            "--attestation-threshold ({threshold}) exceeds the number of --attestation-keys ({})",
            keys.len()
        ));
    }
    if threshold < MIN_ATTESTATION_THRESHOLD {
        return Err(format!(
            "--attestation-threshold ({threshold}) is below the minimum permitted threshold \
             ({MIN_ATTESTATION_THRESHOLD}) — a lower threshold would let a single attestation \
             key release reserves alone, which the approved trust model rules out"
        ));
    }
    for key in keys {
        if *key == Pubkey::default() {
            return Err("--attestation-keys contains the all-zero default pubkey".to_string());
        }
    }
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            if keys[i] == keys[j] {
                return Err(format!(
                    "--attestation-keys contains a duplicate: {} appears more than once",
                    keys[i]
                ));
            }
        }
    }
    Ok(())
}

// ----------------------------------------- upgradeable-loader account decode --
//
// Hand-parsed, not via a dependency: the same technique (and same field
// offsets) independently verified against this program's real deployed
// mainnet ProgramData account during the read-only verification that
// preceded this tool (docs/22-production-readiness-review.md P0-6) —
// `UpgradeableLoaderState::Program { programdata_address }` and
// `UpgradeableLoaderState::ProgramData { slot, upgrade_authority_address }`,
// bincode-encoded: a 4-byte little-endian enum discriminant, then the
// variant's own fields (`Option<Pubkey>` as a 1-byte tag plus 32 bytes
// only if Some).

const LOADER_STATE_UNINITIALIZED: u32 = 0;
const LOADER_STATE_BUFFER: u32 = 1;
const LOADER_STATE_PROGRAM: u32 = 2;
const LOADER_STATE_PROGRAM_DATA: u32 = 3;

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32, String> {
    let b = data
        .get(offset..offset + 4)
        .ok_or_else(|| "account data truncated (u32)".to_string())?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

fn read_u64_le(data: &[u8], offset: usize) -> Result<u64, String> {
    let b = data
        .get(offset..offset + 8)
        .ok_or_else(|| "account data truncated (u64)".to_string())?;
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}

fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey, String> {
    let b = data
        .get(offset..offset + 32)
        .ok_or_else(|| "account data truncated (pubkey)".to_string())?;
    Ok(Pubkey::new_from_array(b.try_into().unwrap()))
}

/// Decodes a `Program` account (the `PROGRAM_ID` account itself, owned by
/// the upgradeable loader) and returns its `programdata_address`.
fn decode_program_account(data: &[u8]) -> Result<Pubkey, String> {
    let discriminant = read_u32_le(data, 0)?;
    if discriminant != LOADER_STATE_PROGRAM {
        let kind = match discriminant {
            LOADER_STATE_UNINITIALIZED => "Uninitialized",
            LOADER_STATE_BUFFER => "Buffer",
            LOADER_STATE_PROGRAM_DATA => "ProgramData",
            other => return Err(format!("unrecognized loader-state discriminant {other}")),
        };
        return Err(format!(
            "expected a Program account (discriminant {LOADER_STATE_PROGRAM}), got {kind}"
        ));
    }
    read_pubkey(data, 4)
}

/// Decodes a `ProgramData` account and returns `(slot, upgrade_authority)`.
fn decode_programdata_account(data: &[u8]) -> Result<(u64, Option<Pubkey>), String> {
    let discriminant = read_u32_le(data, 0)?;
    if discriminant != LOADER_STATE_PROGRAM_DATA {
        return Err(format!(
            "expected a ProgramData account (discriminant {LOADER_STATE_PROGRAM_DATA}), got \
             discriminant {discriminant}"
        ));
    }
    let slot = read_u64_le(data, 4)?;
    let option_tag = *data
        .get(12)
        .ok_or_else(|| "account data truncated (option tag)".to_string())?;
    let authority = match option_tag {
        0 => None,
        1 => Some(read_pubkey(data, 13)?),
        other => return Err(format!("malformed Option tag byte {other}")),
    };
    Ok((slot, authority))
}

// ------------------------------------------------------------------- run --

async fn run(cfg: BootstrapConfig) -> Result<(), String> {
    println!("=== glc-mainnet-bootstrap ===");
    println!(
        "mode: {}",
        if cfg.execute {
            "EXECUTE (will broadcast if simulations succeed)"
        } else {
            "SIMULATION ONLY (default — nothing will be broadcast)"
        }
    );
    println!();

    // 0. Refuse a known-retired program id FIRST — before any other
    // check, and before any RPC call — independent of what this build's
    // compiled-in PROGRAM_ID currently holds. See RETIRED_PROGRAM_IDS'
    // own docs for why this is a separate check from the compiled-
    // constant match just below, not a special case of it.
    if retired_program_ids().contains(&cfg.program_id) {
        return Err(format!(
            "--program-id {} is PERMANENTLY RETIRED — that program was closed on chain and its \
             rent reclaimed; it no longer exists in any form and must never be targeted again. \
             Refusing before making any RPC call. A future production deployment will use a \
             different, not-yet-generated program id — see this file's own module docs for the \
             replacement workflow.",
            cfg.program_id
        ));
    }

    // 1/2. --program-id must equal this build's compiled-in program id —
    // checked before any RPC call at all.
    if cfg.program_id != PROGRAM_ID {
        return Err(format!(
            "--program-id {} does not match this build's compiled-in program id {} — refusing \
             to proceed. Either you meant a different deployment, or this build is stale; do \
             not override this check.",
            cfg.program_id, PROGRAM_ID
        ));
    }
    println!("[ok] --program-id matches this build's compiled-in program id: {PROGRAM_ID}");

    // Client-side attestation-set preflight (mirrors on-chain validation.rs).
    validate_attestation_key_set(&cfg.attestation_keys, cfg.attestation_threshold)?;
    println!(
        "[ok] attestation key set passes preflight validation: {} key(s), threshold {}",
        cfg.attestation_keys.len(),
        cfg.attestation_threshold
    );

    // 3/4. Load the deployer/upgrade-authority keypair — READ ONLY.
    let deployer = read_keypair_file(&cfg.deployer_keypair_path).map_err(|e| {
        format!(
            "could not read deployer keypair at {}: {e}",
            cfg.deployer_keypair_path.display()
        )
    })?;
    println!(
        "[ok] loaded deployer keypair (read-only): {}",
        deployer.pubkey()
    );

    let rpc = RealSolanaRpc::new(cfg.rpc_url.clone());
    let raw = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::finalized());

    // 6/7. Fetch the program account and its ProgramData account; verify
    // they agree with each other and that the deployer is the CURRENT
    // real upgrade authority.
    let program_data_pda =
        solana_sdk::bpf_loader_upgradeable::get_program_data_address(&PROGRAM_ID);

    let program_account = rpc
        .get_account(&PROGRAM_ID)
        .await
        .map_err(|e| format!("RPC error fetching program account {PROGRAM_ID}: {e}"))?
        .ok_or_else(|| format!("program account {PROGRAM_ID} does not exist on this cluster"))?;
    if program_account.owner != solana_sdk::bpf_loader_upgradeable::ID {
        return Err(format!(
            "program account {PROGRAM_ID} is owned by {}, not the upgradeable BPF loader \
             ({}) — refusing to proceed",
            program_account.owner,
            solana_sdk::bpf_loader_upgradeable::ID
        ));
    }
    let decoded_program_data_address = decode_program_account(&program_account.data)?;
    if decoded_program_data_address != program_data_pda {
        return Err(format!(
            "program account {PROGRAM_ID}'s own recorded ProgramData address ({}) does not \
             match the derived ProgramData PDA ({}) — refusing to proceed",
            decoded_program_data_address, program_data_pda
        ));
    }

    let program_data_account = rpc
        .get_account(&program_data_pda)
        .await
        .map_err(|e| format!("RPC error fetching ProgramData account {program_data_pda}: {e}"))?
        .ok_or_else(|| format!("ProgramData account {program_data_pda} does not exist"))?;
    let (deployed_slot, upgrade_authority) =
        decode_programdata_account(&program_data_account.data)?;
    println!("[ok] program account verified: owner=upgradeable BPF loader, ProgramData={program_data_pda}, last deployed in slot {deployed_slot}");

    match upgrade_authority {
        None => {
            return Err(
                "the program's upgrade authority has been set to None (immutable) — no keypair \
                 can call initialize; refusing to proceed"
                    .to_string(),
            );
        }
        Some(current_authority) if current_authority != deployer.pubkey() => {
            return Err(format!(
                "the supplied deployer keypair ({}) is NOT the program's current upgrade \
                 authority — the real current upgrade authority is {} — refusing to proceed",
                deployer.pubkey(),
                current_authority
            ));
        }
        Some(_) => {
            println!(
                "[ok] supplied deployer keypair IS the program's current upgrade authority: {}",
                deployer.pubkey()
            );
        }
    }

    // 5/6 (reserve mint / token program). Independently verify via a live
    // read of the mint account itself — never trust --token-program alone.
    let mint_basics = accounts::verify_reserve_mint_token_program(&rpc, &cfg.reserve_mint)
        .await
        .map_err(|e| format!("reserve mint verification failed: {e}"))?;
    if mint_basics.token_program != cfg.token_program {
        return Err(format!(
            "--token-program {} does not match the token program actually owning \
             --reserve-mint {} on-chain ({}) — refusing to proceed",
            cfg.token_program, cfg.reserve_mint, mint_basics.token_program
        ));
    }
    println!(
        "[ok] reserve mint verified live: {} is owned by {} with {} decimals",
        cfg.reserve_mint, mint_basics.token_program, mint_basics.decimals
    );
    if mint_basics.decimals != 6 {
        println!(
            "[WARNING] reserve mint has {} decimals, not the 6 decimals the pilot GLC mint is \
             documented to have (docs/12-management-decisions.md item 10) — double-check this \
             is really the intended mint before proceeding",
            mint_basics.decimals
        );
    }

    // 10. Derive and print EVERY PDA/account either instruction will
    // touch, before building anything.
    let bridge_config_pda = accounts::bridge_config_pda();
    let attestation_key_set_pda = accounts::attestation_key_set_pda();
    let rolling_volume_window_release_pda = accounts::rolling_volume_window_pda(0);
    let rolling_volume_window_deposit_pda = accounts::rolling_volume_window_pda(1);
    let reserve_authority_pda = accounts::reserve_authority_pda();
    let reserve_vault_ata = accounts::associated_token_address(
        &reserve_authority_pda,
        &cfg.reserve_mint,
        &cfg.token_program,
    );

    println!();
    println!("=== derived accounts (nothing created yet) ===");
    println!("program                          = {PROGRAM_ID}");
    println!("program_data                     = {program_data_pda}");
    println!("bridge_config (PDA)              = {bridge_config_pda}");
    println!("attestation_key_set (PDA)        = {attestation_key_set_pda}");
    println!("rolling_volume_window[release=0] = {rolling_volume_window_release_pda}");
    println!("rolling_volume_window[deposit=1] = {rolling_volume_window_deposit_pda}");
    println!("reserve_authority (PDA)          = {reserve_authority_pda}");
    println!("reserve_vault ATA                = {reserve_vault_ata}");
    println!();

    // 11. Refuse if the bridge is already initialized.
    let bridge_config_exists = rpc
        .get_account(&bridge_config_pda)
        .await
        .map_err(|e| format!("RPC error checking bridge_config {bridge_config_pda}: {e}"))?
        .is_some();
    if bridge_config_exists {
        return Err(format!(
            "bridge_config ({bridge_config_pda}) already exists — the bridge is already \
             initialized; refusing to run initialize again"
        ));
    }
    println!("[ok] bridge_config does not yet exist — bridge is not yet initialized");

    // Payer balance / rough rent floor (item 15 — "where practical", see
    // this file's own module docs for why an exact space estimate is
    // deliberately not duplicated from the on-chain crate's own account
    // layouts: simulation below is the authoritative check).
    let payer_balance = raw
        .get_balance(&deployer.pubkey())
        .await
        .map_err(|e| format!("RPC error fetching payer balance: {e}"))?;
    let per_account_rent_floor = raw
        .get_minimum_balance_for_rent_exemption(0)
        .await
        .map_err(|e| format!("RPC error fetching rent-exemption minimum: {e}"))?;
    let new_accounts_this_run = 5u64; // bridge_config, attestation_key_set, 2x rolling_volume_window, reserve_vault ATA
    let rough_floor = per_account_rent_floor.saturating_mul(new_accounts_this_run);
    println!(
        "payer balance                    = {payer_balance} lamports ({:.9} SOL)",
        payer_balance as f64 / 1_000_000_000.0
    );
    println!(
        "rough rent floor (5 new accounts, 0-byte minimum each; actual accounts carry data and \
         cost MORE — this is a lower bound only, not an estimate of the real requirement) = \
         {rough_floor} lamports ({:.9} SOL)",
        rough_floor as f64 / 1_000_000_000.0
    );
    if payer_balance < rough_floor {
        return Err(format!(
            "payer balance ({payer_balance} lamports) is below even the roughest possible \
             floor for the {new_accounts_this_run} new accounts this run would create \
             ({rough_floor} lamports) — refusing to proceed; fund the deployer account first"
        ));
    }
    println!();

    // 12/13. Build and simulate `initialize`.
    println!("=== initialize ===");
    let initialize_ix = instructions::initialize(
        &deployer.pubkey(),
        &cfg.attestation_keys,
        cfg.attestation_threshold,
        cfg.governance_timelock_seconds,
        cfg.min_transfer_amount,
        cfg.per_transfer_limit,
        cfg.protected_minimum,
        cfg.rolling_volume_limit,
        cfg.rolling_window_seconds,
        cfg.upgrade_timelock_seconds,
    );
    print_params(&cfg, mint_basics.decimals);

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| format!("RPC error fetching latest blockhash: {e}"))?;
    let initialize_tx = Transaction::new_signed_with_payer(
        &[initialize_ix],
        Some(&deployer.pubkey()),
        &[&deployer],
        blockhash,
    );
    let fee = raw
        .get_fee_for_message(initialize_tx.message())
        .await
        .map_err(|e| format!("RPC error estimating transaction fee: {e}"))?;
    println!("estimated network fee for initialize: {fee} lamports");

    let initialize_sim = simulate(&raw, &initialize_tx).await?;
    print_simulation("initialize", &initialize_sim);
    if initialize_sim.err.is_some() {
        return Err(
            "initialize simulation failed — refusing to broadcast (see logs above); \
                     fix the underlying issue and re-run"
                .to_string(),
        );
    }
    println!("[ok] initialize simulation succeeded");
    println!();

    let mut initialize_signature = None;
    let mut initialize_actually_landed = false;
    if cfg.execute {
        println!("--execute supplied and initialize simulation succeeded — broadcasting...");
        let signature = rpc
            .send_transaction(&initialize_tx)
            .await
            .map_err(|e| format!("failed to submit initialize transaction: {e}"))?;
        println!("submitted initialize as {signature}");
        confirm_transaction(&rpc, &signature, &blockhash, ConfirmPolicy::default())
            .await
            .map_err(|e| format!("initialize transaction did not confirm: {e}"))?;
        println!("[ok] initialize CONFIRMED on chain: {signature}");
        initialize_signature = Some(signature);
        initialize_actually_landed = true;
    } else {
        println!(
            "--execute not supplied — initialize was only simulated, not broadcast. Nothing \
             has changed on chain."
        );
    }
    println!();

    // 19/20. Build and simulate `initialize_reserve_vault`, regardless of
    // whether initialize actually landed this run — see this file's own
    // module docs for why a simulate-only run still attempts this (full
    // transparency) rather than silently skipping it, and why its
    // simulation failing for the EXPECTED reason (bridge_config not
    // existing yet, because initialize was only simulated) does not by
    // itself fail this whole run.
    println!("=== initialize_reserve_vault ===");
    let vault_ix = instructions::initialize_reserve_vault(
        &deployer.pubkey(),
        &cfg.reserve_mint,
        &cfg.token_program,
    );

    let bridge_config_exists_now = rpc
        .get_account(&bridge_config_pda)
        .await
        .map_err(|e| format!("RPC error re-checking bridge_config {bridge_config_pda}: {e}"))?
        .is_some();
    let vault_precondition_expected_missing =
        !bridge_config_exists_now && !initialize_actually_landed;
    if vault_precondition_expected_missing {
        println!(
            "[note] bridge_config does not exist on chain yet (initialize was only simulated \
             this run, not broadcast) — initialize_reserve_vault's simulation below is expected \
             to fail for exactly that reason. This is not a defect in the instruction; it \
             demonstrates the instruction's shape only. Re-run with --execute to actually \
             initialize first, then this step will simulate against real state."
        );
    }

    let vault_blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| format!("RPC error fetching latest blockhash: {e}"))?;
    let vault_tx = Transaction::new_signed_with_payer(
        &[vault_ix],
        Some(&deployer.pubkey()),
        &[&deployer],
        vault_blockhash,
    );
    let vault_sim = simulate(&raw, &vault_tx).await?;
    print_simulation("initialize_reserve_vault", &vault_sim);

    if vault_sim.err.is_some() {
        if vault_precondition_expected_missing {
            println!(
                "[expected] initialize_reserve_vault simulation failed only because \
                 bridge_config does not exist yet in this simulate-only run — not treated as a \
                 failure of this tool run."
            );
        } else {
            return Err(
                "initialize_reserve_vault simulation failed for a reason OTHER than \
                 bridge_config not yet existing — refusing to broadcast (see logs above)"
                    .to_string(),
            );
        }
    } else {
        println!("[ok] initialize_reserve_vault simulation succeeded");
    }

    let mut vault_signature = None;
    if cfg.execute {
        if !initialize_actually_landed {
            println!(
                "--execute supplied, but initialize was not actually confirmed this run — \
                 refusing to broadcast initialize_reserve_vault out of order."
            );
        } else if vault_sim.err.is_some() {
            println!(
                "--execute supplied, but initialize_reserve_vault's own simulation failed — \
                 refusing to broadcast it."
            );
        } else {
            println!("broadcasting initialize_reserve_vault...");
            let signature = rpc.send_transaction(&vault_tx).await.map_err(|e| {
                format!("failed to submit initialize_reserve_vault transaction: {e}")
            })?;
            println!("submitted initialize_reserve_vault as {signature}");
            confirm_transaction(&rpc, &signature, &vault_blockhash, ConfirmPolicy::default())
                .await
                .map_err(|e| {
                    format!("initialize_reserve_vault transaction did not confirm: {e}")
                })?;
            println!("[ok] initialize_reserve_vault CONFIRMED on chain: {signature}");
            vault_signature = Some(signature);
        }
    }

    println!();
    println!("=== summary ===");
    println!("bridge_config              = {bridge_config_pda}");
    println!("attestation_key_set        = {attestation_key_set_pda}");
    println!("reserve_vault ATA          = {reserve_vault_ata}");
    println!(
        "initialize signature       = {}",
        initialize_signature
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(not broadcast — simulation only)".to_string())
    );
    println!(
        "initialize_reserve_vault sig = {}",
        vault_signature.map(|s| s.to_string()).unwrap_or_else(|| {
            "(not broadcast — simulation only, or gated by initialize)".to_string()
        })
    );

    Ok(())
}

fn print_params(cfg: &BootstrapConfig, decimals: u8) {
    let as_display_amount = |raw: u64| -> String {
        let scale = 10u64.checked_pow(decimals as u32).unwrap_or(1);
        format!(
            "{raw} raw units ({:.*} GLC, assuming the live mint's {decimals} decimals)",
            decimals as usize,
            raw as f64 / scale as f64
        )
    };
    println!(
        "attestation_keys                = {:?}",
        cfg.attestation_keys
    );
    println!(
        "attestation_threshold            = {}",
        cfg.attestation_threshold
    );
    println!(
        "governance_timelock_seconds      = {}",
        cfg.governance_timelock_seconds
    );
    println!(
        "min_transfer_amount               = {}",
        as_display_amount(cfg.min_transfer_amount)
    );
    println!(
        "per_transfer_limit                = {}",
        as_display_amount(cfg.per_transfer_limit)
    );
    println!(
        "protected_minimum                 = {}",
        as_display_amount(cfg.protected_minimum)
    );
    println!(
        "rolling_volume_limit               = {}",
        as_display_amount(cfg.rolling_volume_limit)
    );
    println!(
        "rolling_window_seconds            = {}",
        cfg.rolling_window_seconds
    );
    println!(
        "upgrade_timelock_seconds          = {}",
        cfg.upgrade_timelock_seconds
    );
}

async fn simulate(
    raw: &RpcClient,
    tx: &Transaction,
) -> Result<solana_client::rpc_response::RpcSimulateTransactionResult, String> {
    raw.simulate_transaction_with_config(
        tx,
        RpcSimulateTransactionConfig {
            sig_verify: true,
            replace_recent_blockhash: false,
            commitment: Some(CommitmentConfig::finalized()),
            ..Default::default()
        },
    )
    .await
    .map(|resp| resp.value)
    .map_err(|e| format!("RPC error while simulating: {e}"))
}

fn print_simulation(
    label: &str,
    result: &solana_client::rpc_response::RpcSimulateTransactionResult,
) {
    match &result.err {
        None => println!("{label} simulation: SUCCESS"),
        Some(e) => println!("{label} simulation: FAILED — {e}"),
    }
    if let Some(units) = result.units_consumed {
        println!("{label} compute units consumed: {units}");
    }
    println!("{label} logs:");
    match &result.logs {
        Some(logs) if !logs.is_empty() => {
            for line in logs {
                println!("  {line}");
            }
        }
        _ => println!("  (no logs returned)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> Vec<String> {
        vec![
            "glc-mainnet-bootstrap".to_string(),
            "--rpc-url".to_string(),
            "http://127.0.0.1:8899".to_string(),
            "--program-id".to_string(),
            PROGRAM_ID.to_string(),
            "--deployer-keypair".to_string(),
            "/tmp/does-not-need-to-exist-for-parsing.json".to_string(),
            "--reserve-mint".to_string(),
            Pubkey::new_unique().to_string(),
            "--token-program".to_string(),
            spl_token_2022::ID.to_string(),
            "--attestation-keys".to_string(),
            format!(
                "{},{},{}",
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique()
            ),
            "--attestation-threshold".to_string(),
            "2".to_string(),
            "--governance-timelock-seconds".to_string(),
            "3600".to_string(),
            "--min-transfer-amount".to_string(),
            "100".to_string(),
            "--per-transfer-limit".to_string(),
            "10000000000".to_string(),
            "--protected-minimum".to_string(),
            "0".to_string(),
            "--rolling-volume-limit".to_string(),
            "20000000000".to_string(),
            "--rolling-window-seconds".to_string(),
            "3600".to_string(),
            "--upgrade-timelock-seconds".to_string(),
            "86400".to_string(),
        ]
    }

    #[test]
    fn parses_a_well_formed_full_argument_set() {
        let cfg = parse_config(&base_args()).unwrap();
        assert_eq!(cfg.program_id, PROGRAM_ID);
        assert_eq!(cfg.attestation_keys.len(), 3);
        assert_eq!(cfg.attestation_threshold, 2);
        assert_eq!(cfg.per_transfer_limit, 10_000_000_000);
        assert!(!cfg.execute);
    }

    #[test]
    fn missing_required_argument_is_a_usage_error() {
        let mut args = base_args();
        // Drop --per-transfer-limit and its value.
        let idx = args
            .iter()
            .position(|a| a == "--per-transfer-limit")
            .unwrap();
        args.drain(idx..idx + 2);
        let err = parse_config(&args).unwrap_err();
        assert!(err.contains("--per-transfer-limit"), "{err}");
    }

    #[test]
    fn malformed_pubkey_argument_is_a_usage_error() {
        let mut args = base_args();
        let idx = args.iter().position(|a| a == "--program-id").unwrap();
        args[idx + 1] = "not-a-valid-pubkey".to_string();
        let err = parse_config(&args).unwrap_err();
        assert!(err.contains("--program-id"), "{err}");
    }

    // ------------------------------------------- 6-decimal raw amount handling --

    #[test]
    fn per_transfer_limit_is_never_reinterpreted_it_is_the_raw_integer_as_given() {
        // The pilot's 10,000 GLC limit at 6 decimals is 10_000_000_000 raw
        // units — this test proves the CLI parser treats --per-transfer-
        // limit as an opaque raw u64, doing no decimal scaling of its own
        // (scaling only ever happens in `print_params`'s purely cosmetic
        // display path, never on the value actually used to build the
        // instruction).
        let mut args = base_args();
        let idx = args
            .iter()
            .position(|a| a == "--per-transfer-limit")
            .unwrap();
        args[idx + 1] = "10000000000".to_string();
        let cfg = parse_config(&args).unwrap();
        assert_eq!(cfg.per_transfer_limit, 10_000_000_000);

        let ix = instructions::initialize(
            &Pubkey::new_unique(),
            &cfg.attestation_keys,
            cfg.attestation_threshold,
            cfg.governance_timelock_seconds,
            cfg.min_transfer_amount,
            cfg.per_transfer_limit,
            cfg.protected_minimum,
            cfg.rolling_volume_limit,
            cfg.rolling_window_seconds,
            cfg.upgrade_timelock_seconds,
        );
        // per_transfer_limit is the 4th u64 field encoded after the
        // discriminator(8) + key_count(4) + 3*32 keys + threshold(1) +
        // governance_timelock(8) + min_transfer_amount(8) fields — rather
        // than re-deriving that offset here (duplicating instructions.rs's
        // own already-tested layout), just prove the raw bytes contain the
        // exact 10_000_000_000 little-endian u64 unchanged, not some
        // rescaled value like 10_000 or 10_000_000_000_000_000.
        let raw_le = 10_000_000_000u64.to_le_bytes();
        assert!(
            ix.data.windows(8).any(|w| w == raw_le),
            "expected the exact raw per_transfer_limit bytes to appear unmodified in the \
             encoded instruction"
        );
    }

    #[test]
    fn zero_is_a_valid_raw_amount_not_a_parse_error() {
        let mut args = base_args();
        let idx = args
            .iter()
            .position(|a| a == "--protected-minimum")
            .unwrap();
        args[idx + 1] = "0".to_string();
        let cfg = parse_config(&args).unwrap();
        assert_eq!(cfg.protected_minimum, 0);
    }

    // --------------------------------------------------- program-id refusal --

    #[test]
    fn wrong_program_id_is_rejected_before_any_network_call() {
        let cfg = BootstrapConfig {
            rpc_url: "http://127.0.0.1:1".to_string(), // deliberately unreachable
            program_id: Pubkey::new_unique(),          // wrong on purpose
            deployer_keypair_path: PathBuf::from("/nonexistent"),
            reserve_mint: Pubkey::new_unique(),
            token_program: spl_token::ID,
            attestation_keys: vec![
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
            ],
            attestation_threshold: 2,
            governance_timelock_seconds: 1,
            min_transfer_amount: 1,
            per_transfer_limit: 1,
            protected_minimum: 0,
            rolling_volume_limit: 1,
            rolling_window_seconds: 1,
            upgrade_timelock_seconds: 1,
            execute: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(run(cfg)).unwrap_err();
        assert!(
            err.contains("does not match this build's compiled-in program id"),
            "{err}"
        );
    }

    // -------------------------------------------------------- retired program id --

    #[test]
    fn retired_program_ids_contains_the_closed_mainnet_program() {
        let ids = retired_program_ids();
        assert_eq!(
            ids,
            vec![Pubkey::from_str("7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn").unwrap()]
        );
    }

    #[test]
    fn a_retired_program_id_is_rejected_before_any_network_call() {
        let retired = retired_program_ids()[0];
        let cfg = BootstrapConfig {
            rpc_url: "http://127.0.0.1:1".to_string(), // deliberately unreachable
            program_id: retired,
            deployer_keypair_path: PathBuf::from("/nonexistent"),
            reserve_mint: Pubkey::new_unique(),
            token_program: spl_token::ID,
            attestation_keys: vec![
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
            ],
            attestation_threshold: 2,
            governance_timelock_seconds: 1,
            min_transfer_amount: 1,
            per_transfer_limit: 1,
            protected_minimum: 0,
            rolling_volume_limit: 1,
            rolling_window_seconds: 1,
            upgrade_timelock_seconds: 1,
            execute: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(run(cfg)).unwrap_err();
        assert!(err.contains("PERMANENTLY RETIRED"), "{err}");
    }

    /// Documents, and force-fails the moment it stops being true, that
    /// this build's compiled-in program id currently STILL IS the
    /// retired one — see this file's module docs, "THE CURRENTLY
    /// COMPILED-IN PROGRAM ID IS RETIRED". When a real future production
    /// program id replaces it (module docs' 11-step workflow, step 3),
    /// this test will fail — that is the intended signal to come back
    /// here, remove this test, and confirm
    /// `wrong_program_id_is_rejected_before_any_network_call`/the
    /// retired-id tests above still make sense against the new id. Not a
    /// bug to "fix" by updating the assertion without also completing
    /// that replacement.
    #[test]
    fn compiled_program_id_still_awaits_replacement_with_a_real_future_production_id() {
        assert_eq!(
            PROGRAM_ID,
            retired_program_ids()[0],
            "if this fails, the compiled-in program id has been updated — good, but make sure \
             the full replacement workflow (this file's module docs) was completed, not just \
             this one constant"
        );
    }

    // ------------------------------------------------------ threshold checks --

    #[test]
    fn threshold_below_minimum_is_rejected() {
        let keys = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let err = validate_attestation_key_set(&keys, 1).unwrap_err();
        assert!(err.contains("below the minimum"), "{err}");
    }

    #[test]
    fn threshold_above_key_count_is_rejected() {
        let keys = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let err = validate_attestation_key_set(&keys, 3).unwrap_err();
        assert!(err.contains("exceeds the number of"), "{err}");
    }

    #[test]
    fn duplicate_attestation_key_is_rejected() {
        let k = Pubkey::new_unique();
        let keys = vec![k, Pubkey::new_unique(), k];
        let err = validate_attestation_key_set(&keys, 2).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn zero_key_is_rejected() {
        let keys = vec![
            Pubkey::default(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let err = validate_attestation_key_set(&keys, 2).unwrap_err();
        assert!(err.contains("all-zero"), "{err}");
    }

    #[test]
    fn the_approved_two_of_three_pilot_configuration_passes_preflight() {
        let keys = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        assert!(validate_attestation_key_set(&keys, 2).is_ok());
    }

    /// Documents (and would catch drift in) the two literals this file
    /// deliberately duplicates from the on-chain crate — see their own
    /// doc comments for why they can't be imported instead.
    #[test]
    fn attestation_key_set_rules_match_the_on_chain_maximum() {
        assert_eq!(MAX_ATTESTATION_KEYS, 8);
        assert_eq!(MIN_ATTESTATION_THRESHOLD, 2);
    }

    // -------------------------------------------------- simulation-only / --execute gating --

    #[test]
    fn execute_flag_defaults_to_false() {
        let cfg = parse_config(&base_args()).unwrap();
        assert!(!cfg.execute, "must default to simulation-only");
    }

    #[test]
    fn execute_flag_is_recognized_when_present() {
        let mut args = base_args();
        args.push("--execute".to_string());
        let cfg = parse_config(&args).unwrap();
        assert!(cfg.execute);
    }

    #[test]
    fn help_flag_does_not_require_any_other_argument() {
        let args = ["glc-mainnet-bootstrap".to_string(), "--help".to_string()];
        assert!(args.iter().any(|a| a == "-h" || a == "--help"));
    }
}
