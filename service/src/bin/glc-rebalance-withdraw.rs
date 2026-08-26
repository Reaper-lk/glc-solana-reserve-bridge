//! `glc-rebalance-withdraw` — operator tool for an intentional Goldcoin
//! reserve rebalance withdrawal from the multisig vault to an explicit,
//! operator-supplied destination address.
//!
//! **Deliberately three separate subcommands, not one monolithic
//! "withdraw" command**, because a single command that holds every vault
//! signer's key at once would reintroduce exactly the single-point-of-
//! compromise this bridge's whole design otherwise avoids
//! (`service/src/goldcoin/vault.rs`: "no single key may authorize a
//! payout"). Real `vault_threshold`-of-N operational security means each
//! signer runs `sign` independently, on their own machine, with only
//! their own key — never all keys in one process:
//!
//!   plan      — queries live Goldcoin chain state, builds and verifies a
//!               proposed withdrawal (destination, amount, UTXOs, fee,
//!               change) against the real vault balance, and writes it to
//!               a plan file. Needs no key at all — any operator or
//!               observer can run this and independently check the
//!               numbers before anyone signs anything. This step IS the
//!               dry run: nothing is signed or broadcast here.
//!   sign      — one vault signer, with only their own key, re-verifies a
//!               plan file's conservation property (never blindly signs a
//!               plan it didn't check itself) and produces one partial
//!               signature per input.
//!   broadcast — assembles >= `vault_threshold` partial-signature files
//!               into the final signed transaction and prints it. Without
//!               `--execute`, this is still a dry run: the fully-signed
//!               transaction hex and its would-be txid are printed, but
//!               nothing is sent to the network. `--execute` is required,
//!               explicit, and separate from assembling the signatures.
//!
//! Every subcommand reuses the exact same conservation-checked primitives
//! the automated bridge payout path uses
//! (`service::goldcoin::payout::{build_unsigned_tx, verify_payout_tx}`,
//! `service::goldcoin::coin::{select, finalize}`,
//! `service::goldcoin::multisig::{verify_partial, assemble}`) — this tool
//! does not reimplement transaction construction or signature verification,
//! it drives the same code the daemon's own settlement path already relies
//! on, for an operator-initiated destination instead of a bridge
//! obligation's recorded one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use glc_reserve_bridge_service::goldcoin::address::{self, Network};
use glc_reserve_bridge_service::goldcoin::coin::{self, VaultUtxo};
use glc_reserve_bridge_service::goldcoin::hex;
use glc_reserve_bridge_service::goldcoin::multisig::{self, PartialSignature};
use glc_reserve_bridge_service::goldcoin::payout::{self, PayoutInputContext, PayoutPlan};
use glc_reserve_bridge_service::goldcoin::rpc::{RpcClient, RpcConfig};
use glc_reserve_bridge_service::goldcoin::tx::Transaction;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

const USAGE: &str = "glc-rebalance-withdraw — intentional Goldcoin reserve withdrawal, operator CLI

Three separate subcommands so no single invocation ever needs more than one
vault signer's key — see this file's own module docs for why.

PLAN (no key needed; queries live chain state; this step IS the dry run)
  glc-rebalance-withdraw plan \\
      --rpc-url URL --rpc-user USER --rpc-password PASS \\
      --vault-pubkeys HEX,HEX,HEX --vault-threshold N --network <mainnet|testnet> \\
      --destination GOLDCOIN_ADDRESS --amount-atomic N \\
      --fee-rate-per-kb N --dust-threshold N --max-inputs N \\
      --min-confirmations N --out PLAN_FILE.json

SIGN (one vault signer, one key, run independently per signer)
  glc-rebalance-withdraw sign --plan PLAN_FILE.json --key-path SECRET_KEY_HEX_FILE --out PARTIAL_FILE.json

BROADCAST (assemble >= threshold partials; --execute required to actually send)
  glc-rebalance-withdraw broadcast \\
      --plan PLAN_FILE.json --partials PARTIAL1.json,PARTIAL2.json[,...] \\
      --rpc-url URL --rpc-user USER --rpc-password PASS \\
      --confirm-paused [--solana-rpc-url URL] [--execute]

  --confirm-paused
      Mandatory explicit operator acknowledgement that the bridge has
      already been paused (via glc-admin onchain-pause --scope global) —
      this tool does not pause anything itself.
  --solana-rpc-url URL
      Optional but recommended: if supplied, broadcast additionally reads
      the live on-chain BridgeConfig and refuses to proceed if
      config.paused is not actually true, rather than trusting
      --confirm-paused alone.
  --execute
      Without this flag, broadcast only assembles and prints the fully-
      signed transaction (hex + would-be txid) — nothing is sent to the
      Goldcoin network. With it, the assembled transaction is also
      submitted via sendrawtransaction.

See RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md for the full operator
procedure, including how signers exchange plan/partial files out of band.";

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn require<'a>(args: &'a [String], name: &str) -> Result<&'a str, String> {
    flag(args, name).ok_or_else(|| format!("missing required {name}"))
}

fn require_u64(args: &[String], name: &str) -> Result<u64, String> {
    require(args, name)?
        .parse()
        .map_err(|e| format!("{name} must be a non-negative integer: {e}"))
}

fn require_i64(args: &[String], name: &str) -> Result<i64, String> {
    require(args, name)?
        .parse()
        .map_err(|e| format!("{name} must be an integer: {e}"))
}

fn require_usize(args: &[String], name: &str) -> Result<usize, String> {
    require(args, name)?
        .parse()
        .map_err(|e| format!("{name} must be a non-negative integer: {e}"))
}

fn require_u8(args: &[String], name: &str) -> Result<u8, String> {
    require(args, name)?
        .parse()
        .map_err(|e| format!("{name} must be a small non-negative integer: {e}"))
}

fn parse_network(args: &[String]) -> Result<Network, String> {
    match require(args, "--network")? {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" | "regtest" => Ok(Network::Testnet),
        other => Err(format!(
            "--network {other:?} must be one of: mainnet, testnet, regtest"
        )),
    }
}

fn parse_vault_pubkeys(args: &[String]) -> Result<Vec<[u8; 33]>, String> {
    require(args, "--vault-pubkeys")?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| hex::decode_exact::<33>(s).map_err(|e| format!("--vault-pubkeys entry: {e}")))
        .collect()
}

fn rpc_config(args: &[String]) -> Result<RpcConfig, String> {
    Ok(RpcConfig {
        url: require(args, "--rpc-url")?.to_string(),
        user: require(args, "--rpc-user")?.to_string(),
        password: require(args, "--rpc-password")?.to_string(),
        connect_timeout_ms: 5_000,
        read_timeout_ms: 30_000,
    })
}

fn build_vault(args: &[String]) -> Result<MultisigVault, String> {
    let pubkeys = parse_vault_pubkeys(args)?;
    let threshold = require_u8(args, "--vault-threshold")?;
    let network = parse_network(args)?;
    MultisigVault::new(pubkeys, threshold, network).map_err(|e| e.to_string())
}

// ------------------------------------------------------------- file formats --

/// On-disk plan format: everything a signer needs to independently
/// re-verify the exact transaction being proposed, plus the sighash each
/// input actually needs signed (recomputed and checked by `sign`, never
/// trusted from this file alone).
#[derive(Debug, Serialize, Deserialize)]
struct PlanFile {
    vault_pubkeys_hex: Vec<String>,
    vault_threshold: u8,
    network: String,
    destination: String,
    amount_atomic: u64,
    inputs: Vec<PlanInput>,
    change_atomic: u64,
    vault_script_pubkey_hex: String,
    fee_atomic: u64,
    unsigned_tx_hex: String,
    sighashes_hex: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PlanInput {
    txid_hex: String,
    vout: u32,
    amount_atomic: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PartialFile {
    vault_pubkey_hex: String,
    /// One DER signature per transaction input, same order as the plan's
    /// `inputs`.
    signatures_hex: Vec<String>,
}

fn plan_to_payout_plan(plan: &PlanFile) -> Result<PayoutPlan, String> {
    let network = match plan.network.as_str() {
        "mainnet" => Network::Mainnet,
        _ => Network::Testnet,
    };
    let dest_p2pkh_hash = address::decode_p2pkh(&plan.destination, network)
        .map_err(|e| format!("plan file destination is invalid: {e}"))?;
    let inputs = plan
        .inputs
        .iter()
        .map(|i| {
            Ok(VaultUtxo {
                txid: hex::decode_exact::<32>(&i.txid_hex)?,
                vout: i.vout,
                amount_atomic: i.amount_atomic,
                script_pubkey_hex: plan.vault_script_pubkey_hex.clone(),
            })
        })
        .collect::<Result<Vec<_>, hex::HexError>>()
        .map_err(|e| format!("plan file input txid: {e}"))?;
    // This tool only ever plans/broadcasts a spend from the single
    // operator-supplied vault — every input is a legacy static-vault
    // input.
    let plan_vault_pubkeys = plan
        .vault_pubkeys_hex
        .iter()
        .map(|p| hex::decode_exact::<33>(p))
        .collect::<Result<Vec<_>, hex::HexError>>()
        .map_err(|e| format!("plan file vault pubkey: {e}"))?;
    let plan_vault = MultisigVault::new(plan_vault_pubkeys, plan.vault_threshold, network)
        .map_err(|e| format!("plan file vault reconstruction: {e}"))?;
    let input_contexts = vec![
        PayoutInputContext {
            vault: plan_vault,
            funding_request_id: None,
        };
        inputs.len()
    ];
    Ok(PayoutPlan {
        inputs,
        input_contexts,
        dest_p2pkh_hash,
        payout_atomic: plan.amount_atomic,
        // This tool always plans a single change output (never fan-out —
        // that's specific to the automatic SolToGlc payout path); the
        // on-disk plan file format's `change_atomic` field name is kept
        // stable rather than renamed.
        change_outputs: if plan.change_atomic > 0 {
            vec![plan.change_atomic]
        } else {
            Vec::new()
        },
        vault_script_pubkey: hex::decode_vec(&plan.vault_script_pubkey_hex)
            .map_err(|e| format!("plan file vault script: {e}"))?,
        fee_atomic: plan.fee_atomic,
    })
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
        "sign" => cmd_sign(&args),
        "broadcast" => rt.block_on(cmd_broadcast(&args)),
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

// -------------------------------------------------------------------- plan --

async fn cmd_plan(args: &[String]) -> Result<(), String> {
    let vault = build_vault(args)?;
    let network = parse_network(args)?;
    let destination = require(args, "--destination")?.to_string();
    let amount_atomic = require_u64(args, "--amount-atomic")?;
    let fee_rate_per_kb = require_u64(args, "--fee-rate-per-kb")?;
    let dust_threshold = require_u64(args, "--dust-threshold")?;
    let max_inputs = require_usize(args, "--max-inputs")?;
    let min_confirmations = require_i64(args, "--min-confirmations")?;
    let out_path = PathBuf::from(require(args, "--out")?);

    address::decode_p2pkh(&destination, network)
        .map_err(|e| format!("--destination {destination:?} is not a valid Goldcoin address for the selected network: {e}"))?;

    let rpc = RpcClient::new(&rpc_config(args)?).map_err(|e| e.to_string())?;
    let entries = rpc
        .list_unspent(min_confirmations, &[vault.address().to_string()])
        .await
        .map_err(|e| e.to_string())?;
    let mut candidates: Vec<VaultUtxo> = entries
        .into_iter()
        .filter(|e| e.solvable)
        .map(|e| {
            Ok(VaultUtxo {
                txid: hex::decode_exact::<32>(&e.txid)
                    .map_err(|err| format!("node returned an unparseable txid: {err}"))?,
                vout: e.vout,
                amount_atomic: (e.amount * 100_000_000.0).round() as u64,
                script_pubkey_hex: e.script_pub_key.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    // `coin::select`'s own documented precondition.
    candidates.sort_by(|a, b| {
        b.amount_atomic
            .cmp(&a.amount_atomic)
            .then_with(|| a.txid.cmp(&b.txid))
            .then_with(|| a.vout.cmp(&b.vout))
    });

    let selection = coin::select(
        &candidates,
        amount_atomic,
        fee_rate_per_kb,
        vault.threshold,
        vault.redeem_script().len(),
        max_inputs,
    )
    .map_err(|e| e.to_string())?;
    let (change_atomic, fee_atomic) = coin::finalize(&selection, amount_atomic, dust_threshold);

    let dest_p2pkh_hash = address::decode_p2pkh(&destination, network).unwrap();
    // This tool only ever spends from the single operator-supplied vault
    // address (its own `list_unspent` call above never queries any
    // per-request derived deposit address) — every input is a legacy
    // static-vault input, signed with the root vault/key exactly as
    // before per-request addresses existed.
    let input_contexts = vec![
        PayoutInputContext {
            vault: vault.clone(),
            funding_request_id: None,
        };
        selection.selected.len()
    ];
    let plan = PayoutPlan {
        inputs: selection.selected.clone(),
        input_contexts,
        dest_p2pkh_hash,
        payout_atomic: amount_atomic,
        change_outputs: if change_atomic > 0 {
            vec![change_atomic]
        } else {
            Vec::new()
        },
        vault_script_pubkey: vault.script_pubkey(),
        fee_atomic,
    };
    let unsigned_tx = payout::build_unsigned_tx(&plan);
    payout::verify_payout_tx(&unsigned_tx, &plan).map_err(|e| {
        format!("internal error: freshly-built plan failed its own conservation check: {e}")
    })?;

    let redeem_script = vault.redeem_script();
    let sighashes: Vec<String> = (0..unsigned_tx.inputs.len())
        .map(|i| hex::encode(&unsigned_tx.sighash_all(i, &redeem_script)))
        .collect();

    let plan_file = PlanFile {
        vault_pubkeys_hex: vault
            .signer_pubkeys
            .iter()
            .map(|pk| hex::encode(pk))
            .collect(),
        vault_threshold: vault.threshold,
        network: match network {
            Network::Mainnet => "mainnet".to_string(),
            Network::Testnet => "testnet".to_string(),
        },
        destination: destination.clone(),
        amount_atomic,
        inputs: selection
            .selected
            .iter()
            .map(|u| PlanInput {
                txid_hex: hex::encode(&u.txid),
                vout: u.vout,
                amount_atomic: u.amount_atomic,
            })
            .collect(),
        change_atomic,
        vault_script_pubkey_hex: hex::encode(&vault.script_pubkey()),
        fee_atomic,
        unsigned_tx_hex: hex::encode(&unsigned_tx.serialize()),
        sighashes_hex: sighashes,
    };
    let json = serde_json::to_string_pretty(&plan_file).map_err(|e| e.to_string())?;
    std::fs::write(&out_path, &json).map_err(|e| format!("writing {out_path:?}: {e}"))?;

    println!("Plan written to {}", out_path.display());
    println!("  vault address        = {}", vault.address());
    println!("  destination          = {destination}");
    println!("  amount (atomic)      = {amount_atomic}");
    println!("  inputs selected      = {}", plan.inputs.len());
    println!("  total input value    = {}", selection.total_selected);
    println!("  change (atomic)      = {change_atomic}");
    println!("  fee (atomic)         = {fee_atomic}");
    println!(
        "  conservation check   = {} == {} + {} + {} (verified)",
        selection.total_selected, amount_atomic, change_atomic, fee_atomic
    );
    println!(
        "\nThis plan is NOT signed and NOT broadcast. Distribute this file to each vault \
         signer for independent review, then have >= {} of them run `sign` on their own \
         machine with their own key.",
        vault.threshold
    );
    Ok(())
}

// -------------------------------------------------------------------- sign --

fn cmd_sign(args: &[String]) -> Result<(), String> {
    let plan_path = PathBuf::from(require(args, "--plan")?);
    let key_path = PathBuf::from(require(args, "--key-path")?);
    let out_path = PathBuf::from(require(args, "--out")?);

    let plan_json = std::fs::read_to_string(&plan_path)
        .map_err(|e| format!("reading plan file {plan_path:?}: {e}"))?;
    let plan_file: PlanFile = serde_json::from_str(&plan_json)
        .map_err(|e| format!("plan file {plan_path:?} is not valid: {e}"))?;

    // Never blindly sign a handed-in plan: independently rebuild it from
    // its own recorded inputs/destination/amount and re-run the same
    // conservation check `plan` already ran, exactly the discipline
    // `service::signing::goldcoin_vault::independently_sign` uses for the
    // automated bridge payout path.
    let payout_plan = plan_to_payout_plan(&plan_file)?;
    let unsigned_tx = payout::build_unsigned_tx(&payout_plan);
    payout::verify_payout_tx(&unsigned_tx, &payout_plan).map_err(|e| {
        format!(
            "REFUSING TO SIGN — plan file fails its own conservation check, do not trust it: {e}"
        )
    })?;
    if hex::encode(&unsigned_tx.serialize()) != plan_file.unsigned_tx_hex {
        return Err(
            "REFUSING TO SIGN — the plan file's recorded transaction bytes do not match what \
             its own recorded inputs/destination/amount actually produce; the file may be \
             corrupted or tampered with"
                .to_string(),
        );
    }

    let key_hex = std::fs::read_to_string(&key_path)
        .map_err(|e| format!("reading key file {key_path:?}: {e}"))?;
    let secret_bytes = hex::decode_exact::<32>(key_hex.trim())
        .map_err(|e| format!("key file {key_path:?} is not a valid 32-byte hex secret key: {e}"))?;
    let secret_key = libsecp256k1::SecretKey::parse(&secret_bytes)
        .map_err(|e| format!("key file {key_path:?} is not a valid secp256k1 secret key: {e}"))?;
    let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret_key).serialize_compressed();
    if vault_position(&plan_file.vault_pubkeys_hex, &pubkey).is_none() {
        return Err(
            "REFUSING TO SIGN — this key's public key is not one of the plan's configured vault \
             signers"
                .to_string(),
        );
    }

    let redeem_script = MultisigVault::new(
        plan_file
            .vault_pubkeys_hex
            .iter()
            .map(|s| hex::decode_exact::<33>(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?,
        plan_file.vault_threshold,
        Network::Mainnet, // network only affects the address encoding, not the redeem script
    )
    .map_err(|e| e.to_string())?
    .redeem_script();

    let mut signatures_hex = Vec::with_capacity(unsigned_tx.inputs.len());
    for i in 0..unsigned_tx.inputs.len() {
        let sighash = unsigned_tx.sighash_all(i, &redeem_script);
        signatures_hex.push(hex::encode(&multisig::sign_low_s(&sighash, &secret_key)));
    }

    let partial = PartialFile {
        vault_pubkey_hex: hex::encode(&pubkey),
        signatures_hex,
    };
    let json = serde_json::to_string_pretty(&partial).map_err(|e| e.to_string())?;
    std::fs::write(&out_path, &json).map_err(|e| format!("writing {out_path:?}: {e}"))?;

    println!("Partial signature written to {}", out_path.display());
    println!("  signer pubkey = {}", partial.vault_pubkey_hex);
    println!(
        "  plan verified independently before signing: destination={}, amount_atomic={}, \
         inputs={}, fee_atomic={}",
        plan_file.destination,
        plan_file.amount_atomic,
        plan_file.inputs.len(),
        plan_file.fee_atomic
    );
    Ok(())
}

fn vault_position(pubkeys_hex: &[String], pubkey: &[u8; 33]) -> Option<usize> {
    let target = hex::encode(pubkey);
    pubkeys_hex.iter().position(|p| p == &target)
}

// -------------------------------------------------------------- broadcast --

async fn cmd_broadcast(args: &[String]) -> Result<(), String> {
    if !args.iter().any(|a| a == "--confirm-paused") {
        return Err(
            "--confirm-paused is required: explicitly confirm the bridge has already been \
             paused (glc-admin onchain-pause --scope global) before assembling a rebalance \
             withdrawal. This tool does not pause anything itself."
                .to_string(),
        );
    }
    if let Some(solana_rpc_url) = flag(args, "--solana-rpc-url") {
        let rpc = RealSolanaRpc::new(solana_rpc_url.to_string());
        let pda = accounts::bridge_config_pda();
        let account = rpc
            .get_account(&pda)
            .await
            .map_err(|e| format!("checking on-chain pause state: {e}"))?
            .ok_or_else(|| format!("bridge_config does not exist at {pda} on this cluster"))?;
        let config = accounts::decode_bridge_config(&account.data).map_err(|e| e.to_string())?;
        if !config.paused {
            return Err(format!(
                "REFUSING TO BROADCAST — live on-chain BridgeConfig.paused is false at {pda}. \
                 Pause the bridge first: glc-admin onchain-pause --scope global ..."
            ));
        }
        println!("Verified live: BridgeConfig.paused = true at {pda}");
    } else {
        println!(
            "--solana-rpc-url not supplied — proceeding on --confirm-paused alone, without an \
             independent live check. Supplying --solana-rpc-url is recommended."
        );
    }

    let plan_path = PathBuf::from(require(args, "--plan")?);
    let plan_json = std::fs::read_to_string(&plan_path)
        .map_err(|e| format!("reading plan file {plan_path:?}: {e}"))?;
    let plan_file: PlanFile = serde_json::from_str(&plan_json)
        .map_err(|e| format!("plan file {plan_path:?} is not valid: {e}"))?;
    let payout_plan = plan_to_payout_plan(&plan_file)?;
    let unsigned_tx = payout::build_unsigned_tx(&payout_plan);
    payout::verify_payout_tx(&unsigned_tx, &payout_plan)
        .map_err(|e| format!("plan file fails its own conservation check: {e}"))?;

    let partial_paths: Vec<PathBuf> = require(args, "--partials")?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let mut partial_files = Vec::with_capacity(partial_paths.len());
    for p in &partial_paths {
        let json = std::fs::read_to_string(p).map_err(|e| format!("reading {p:?}: {e}"))?;
        let partial: PartialFile =
            serde_json::from_str(&json).map_err(|e| format!("{p:?} is not valid: {e}"))?;
        partial_files.push(partial);
    }
    if partial_files.len() < plan_file.vault_threshold as usize {
        return Err(format!(
            "only {} partial-signature file(s) supplied, but the plan requires a threshold of {}",
            partial_files.len(),
            plan_file.vault_threshold
        ));
    }

    let signed_tx = assemble_signed_tx(&plan_file, &unsigned_tx, &partial_files)?;
    let signed_hex = hex::encode(&signed_tx.serialize());
    let txid = hex::encode(&signed_tx.txid());
    println!("Assembled transaction:");
    println!("  txid          = {txid}");
    println!("  destination   = {}", plan_file.destination);
    println!("  amount_atomic = {}", plan_file.amount_atomic);
    println!("  fee_atomic    = {}", plan_file.fee_atomic);
    println!("  raw hex       = {signed_hex}");

    if !args.iter().any(|a| a == "--execute") {
        println!(
            "\n--execute not supplied — the transaction above was assembled and verified but \
             NOT broadcast. Re-run with --execute to actually send it."
        );
        return Ok(());
    }

    let rpc = RpcClient::new(&rpc_config(args)?).map_err(|e| e.to_string())?;
    let outcome = rpc
        .send_raw_transaction(&signed_hex)
        .await
        .map_err(|e| e.to_string())?;
    println!("\n--execute supplied — broadcast outcome: {outcome:?}");
    Ok(())
}

/// Assembles the final scriptSig for every input from >= threshold partial
/// signature files, using the exact same `multisig::assemble` (dup-signer
/// rejection, per-input signature verification, threshold-met check) the
/// automated bridge payout path relies on. Pure — no RPC, no I/O — so it is
/// directly unit-testable offline.
fn assemble_signed_tx(
    plan_file: &PlanFile,
    unsigned_tx: &Transaction,
    partial_files: &[PartialFile],
) -> Result<Transaction, String> {
    let vault = MultisigVault::new(
        plan_file
            .vault_pubkeys_hex
            .iter()
            .map(|s| hex::decode_exact::<33>(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?,
        plan_file.vault_threshold,
        match plan_file.network.as_str() {
            "mainnet" => Network::Mainnet,
            _ => Network::Testnet,
        },
    )
    .map_err(|e| e.to_string())?;

    let redeem_script = vault.redeem_script();
    let mut signed_tx = unsigned_tx.clone();
    for i in 0..signed_tx.inputs.len() {
        let sighash = signed_tx.sighash_all(i, &redeem_script);
        let mut partials_for_input: Vec<PartialSignature> = Vec::new();
        for pf in partial_files {
            let vault_pubkey = hex::decode_exact::<33>(&pf.vault_pubkey_hex)
                .map_err(|e| format!("partial file has an invalid pubkey: {e}"))?;
            let der_signature = hex::decode_vec(
                pf.signatures_hex
                    .get(i)
                    .ok_or_else(|| format!("partial file is missing a signature for input {i}"))?,
            )
            .map_err(|e| format!("partial file has an invalid signature: {e}"))?;
            partials_for_input.push(PartialSignature {
                vault_pubkey,
                der_signature,
            });
        }
        let script_sig = multisig::assemble(&vault, &sighash, &partials_for_input)
            .map_err(|e| format!("assembling input {i}: {e}"))?;
        signed_tx.inputs[i].script_sig = script_sig;
    }
    Ok(signed_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_key() -> (libsecp256k1::SecretKey, [u8; 33]) {
        let secret = libsecp256k1::SecretKey::random(&mut rand::rngs::OsRng);
        let pubkey = libsecp256k1::PublicKey::from_secret_key(&secret).serialize_compressed();
        (secret, pubkey)
    }

    /// Builds a synthetic 2-of-3 plan (no RPC, no real chain data) covering
    /// the full plan -> sign -> assemble round trip this tool drives.
    fn synthetic_plan() -> (PlanFile, [libsecp256k1::SecretKey; 3]) {
        let keys = [dev_key(), dev_key(), dev_key()];
        let pubkeys: Vec<[u8; 33]> = keys.iter().map(|(_, pk)| *pk).collect();
        let vault = MultisigVault::new(pubkeys, 2, Network::Testnet).unwrap();

        let dest_hash = [0x42u8; 20];
        let dest_addr = glc_reserve_bridge_service::goldcoin::address::encode_p2pkh(
            &dest_hash,
            Network::Testnet,
        );
        let inputs = vec![VaultUtxo {
            txid: [0x11u8; 32],
            vout: 0,
            amount_atomic: 10_000,
            script_pubkey_hex: vault.script_pubkey_hex(),
        }];
        let plan = PayoutPlan {
            inputs: inputs.clone(),
            input_contexts: vec![PayoutInputContext {
                vault: vault.clone(),
                funding_request_id: None,
            }],
            dest_p2pkh_hash: dest_hash,
            payout_atomic: 5_000,
            change_outputs: vec![4_000],
            vault_script_pubkey: vault.script_pubkey(),
            fee_atomic: 1_000,
        };
        let unsigned_tx = payout::build_unsigned_tx(&plan);
        payout::verify_payout_tx(&unsigned_tx, &plan).unwrap();
        let redeem_script = vault.redeem_script();
        let sighashes_hex = (0..unsigned_tx.inputs.len())
            .map(|i| hex::encode(&unsigned_tx.sighash_all(i, &redeem_script)))
            .collect();

        let plan_file = PlanFile {
            vault_pubkeys_hex: vault
                .signer_pubkeys
                .iter()
                .map(|pk| hex::encode(pk))
                .collect(),
            vault_threshold: vault.threshold,
            network: "testnet".to_string(),
            destination: dest_addr,
            amount_atomic: 5_000,
            inputs: inputs
                .iter()
                .map(|u| PlanInput {
                    txid_hex: hex::encode(&u.txid),
                    vout: u.vout,
                    amount_atomic: u.amount_atomic,
                })
                .collect(),
            change_atomic: 4_000,
            vault_script_pubkey_hex: hex::encode(&vault.script_pubkey()),
            fee_atomic: 1_000,
            unsigned_tx_hex: hex::encode(&unsigned_tx.serialize()),
            sighashes_hex,
        };
        (plan_file, [keys[0].0, keys[1].0, keys[2].0])
    }

    fn sign_offline(plan_file: &PlanFile, secret: &libsecp256k1::SecretKey) -> PartialFile {
        let payout_plan = plan_to_payout_plan(plan_file).unwrap();
        let unsigned_tx = payout::build_unsigned_tx(&payout_plan);
        payout::verify_payout_tx(&unsigned_tx, &payout_plan).unwrap();
        let vault = MultisigVault::new(
            plan_file
                .vault_pubkeys_hex
                .iter()
                .map(|s| hex::decode_exact::<33>(s).unwrap())
                .collect(),
            plan_file.vault_threshold,
            Network::Testnet,
        )
        .unwrap();
        let redeem_script = vault.redeem_script();
        let pubkey = libsecp256k1::PublicKey::from_secret_key(secret).serialize_compressed();
        let signatures_hex = (0..unsigned_tx.inputs.len())
            .map(|i| {
                let sighash = unsigned_tx.sighash_all(i, &redeem_script);
                hex::encode(&multisig::sign_low_s(&sighash, secret))
            })
            .collect();
        PartialFile {
            vault_pubkey_hex: hex::encode(&pubkey),
            signatures_hex,
        }
    }

    #[test]
    fn plan_round_trips_through_plan_to_payout_plan() {
        let (plan_file, _keys) = synthetic_plan();
        let payout_plan = plan_to_payout_plan(&plan_file).unwrap();
        assert_eq!(payout_plan.payout_atomic, 5_000);
        assert_eq!(payout_plan.change_outputs, vec![4_000]);
        assert_eq!(payout_plan.fee_atomic, 1_000);
        assert_eq!(payout_plan.inputs.len(), 1);
        // Conservation must hold: inputs == payout + change + fee.
        let unsigned_tx = payout::build_unsigned_tx(&payout_plan);
        payout::verify_payout_tx(&unsigned_tx, &payout_plan).unwrap();
    }

    #[test]
    fn two_of_three_partials_assemble_into_a_valid_signed_transaction() {
        let (plan_file, keys) = synthetic_plan();
        let payout_plan = plan_to_payout_plan(&plan_file).unwrap();
        let unsigned_tx = payout::build_unsigned_tx(&payout_plan);

        let p1 = sign_offline(&plan_file, &keys[0]);
        let p2 = sign_offline(&plan_file, &keys[1]);
        let signed = assemble_signed_tx(&plan_file, &unsigned_tx, &[p1, p2]).unwrap();

        // The assembled scriptSig must actually verify against the vault's
        // own redeem script and each partial's signature — proven by
        // `multisig::assemble` itself succeeding (it verifies every
        // signature before assembling), not just by the call not panicking.
        assert!(!signed.inputs[0].script_sig.is_empty());
    }

    #[test]
    fn only_one_of_three_partials_is_rejected() {
        let (plan_file, keys) = synthetic_plan();
        let payout_plan = plan_to_payout_plan(&plan_file).unwrap();
        let unsigned_tx = payout::build_unsigned_tx(&payout_plan);

        let p1 = sign_offline(&plan_file, &keys[0]);
        let result = assemble_signed_tx(&plan_file, &unsigned_tx, &[p1]);
        assert!(
            result.is_err(),
            "assembling below the vault's own threshold must fail"
        );
    }

    #[test]
    fn a_tampered_plan_file_is_rejected_by_sign() {
        // Simulates `sign`'s own tamper check: the plan file's recorded
        // unsigned_tx_hex must match what its own inputs/destination/amount
        // actually produce.
        let (mut plan_file, _keys) = synthetic_plan();
        plan_file.amount_atomic = 999_999; // tampered after unsigned_tx_hex was computed
        let payout_plan = plan_to_payout_plan(&plan_file).unwrap();
        let unsigned_tx = payout::build_unsigned_tx(&payout_plan);
        assert_ne!(
            hex::encode(&unsigned_tx.serialize()),
            plan_file.unsigned_tx_hex,
            "a tampered amount must change the recomputed transaction bytes"
        );
    }

    #[test]
    fn vault_position_finds_a_configured_signer_and_rejects_an_outsider() {
        let (plan_file, keys) = synthetic_plan();
        let pubkey = libsecp256k1::PublicKey::from_secret_key(&keys[0]).serialize_compressed();
        assert!(vault_position(&plan_file.vault_pubkeys_hex, &pubkey).is_some());

        let (_outsider_secret, outsider_pubkey) = dev_key();
        assert!(vault_position(&plan_file.vault_pubkeys_hex, &outsider_pubkey).is_none());
    }
}
