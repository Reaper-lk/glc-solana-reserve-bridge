//! `solana-manual-refund-batch` — the STANDALONE sender of the manual
//! (out-of-band) Solana refund workflow (docs/37-manual-solana-refund.md).
//!
//! Reads `refund-backlog.json` (from `glc-admin manual-refund-export`),
//! signs ONE transaction per request with an operator-supplied wallet
//! keypair — a wallet that is separate from every bridge key, funded
//! separately, and whose path is given on this command line only (never
//! stored anywhere by this tool or the bridge) — and writes
//! `refund-results.json`, durably, after EVERY state change.
//!
//! Touches no ledger, no config, no bridge key. Needs only an RPC URL.
//!
//! # Crash safety, stated plainly
//!
//! - A request's signature is written to the results file BEFORE the
//!   transaction is handed to the network. A crash anywhere after that
//!   point leaves a `submitted` entry that a rerun RESOLVES (reads the
//!   signature's fate back from the cluster) before it sends anything
//!   else. A rerun never builds a second transaction for a request whose
//!   recorded one could still land.
//! - `finalized` entries are never touched again. `failed` entries
//!   (demonstrably did not move funds: expired unseen, or landed and
//!   failed on chain) are retried only with `--retry-failed`, and even
//!   then the old signature is re-checked first.
//! - Any AMBIGUOUS outcome (RPC error, confirmation timeout) stops the
//!   run with the entry still `submitted`; nothing after it is sent.
//!
//! # Default is a dry run
//!
//! Without `--execute` this verifies the file, the wallet, the mint, the
//! balances and every recipient, prints the plan, and sends nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::confirm::{
    confirm_transaction, ConfirmFailure, ConfirmPolicy,
};
use glc_reserve_bridge_service::solana::manual_refund::{
    self, RefundBatch, RefundBatchEntry, RefundExpectation, RefundResultEntry, RefundResults,
    ResultStatus,
};
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc, SolanaTransactionLookup};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signature, Signer};
use solana_sdk::transaction::Transaction;

const USAGE: &str = "usage:
  solana-manual-refund-batch --input refund-backlog.json --keypair /secure/refund-wallet.json \\
      --output refund-results.json [--rpc-url URL] [--execute] [--retry-failed] \\
      [--confirm-timeout-secs N]

  Default: DRY RUN — verifies everything and prints the plan; sends nothing.
  --execute       sign and broadcast one transaction per request, waiting
                  for `finalized` on each; results are written after every
                  state change.
  --retry-failed  rebuild entries whose earlier transaction demonstrably
                  did not move funds (after re-checking that transaction).
  --rpc-url       overrides the batch's recorded RPC URL.

The keypair file is a solana-keygen JSON array. Its secret is never printed.
";

/// Safety margin above the estimated SOL cost.
const SOL_MARGIN_LAMPORTS: u64 = 1_000_000;
/// After a recorded blockhash expires, keep asking about the signature
/// this long before calling it dead (inclusion just before expiry can
/// finalize a little after it).
const POST_EXPIRY_GRACE: Duration = Duration::from_secs(45);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn require<'a>(args: &'a [String], name: &str) -> &'a str {
    flag(args, name).unwrap_or_else(|| {
        eprintln!("missing required {name}\n\n{USAGE}");
        std::process::exit(2);
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn run(args: &[String]) -> Result<(), String> {
    let input = require(args, "--input");
    let keypair_path = require(args, "--keypair");
    let output = PathBuf::from(require(args, "--output"));
    let execute = args.iter().any(|a| a == "--execute");
    let retry_failed = args.iter().any(|a| a == "--retry-failed");
    let confirm_timeout = flag(args, "--confirm-timeout-secs")
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| format!("--confirm-timeout-secs: {e}"))
        })
        .transpose()?
        .unwrap_or(120);

    // ---- the batch ---------------------------------------------------
    let text = std::fs::read_to_string(input).map_err(|e| format!("reading {input}: {e}"))?;
    let batch: RefundBatch =
        serde_json::from_str(&text).map_err(|e| format!("parsing {input}: {e}"))?;
    batch.verify()?;
    let mint: Pubkey = batch.mint.parse().map_err(|_| "malformed batch mint")?;
    let token_program: Pubkey = batch
        .token_program
        .parse()
        .map_err(|_| "malformed batch token_program")?;
    let rpc_url = flag(args, "--rpc-url")
        .map(str::to_string)
        .unwrap_or_else(|| batch.rpc_url.clone());

    // ---- the wallet (public half only ever leaves this scope) ---------
    let keypair = read_keypair_file(keypair_path)
        .map_err(|e| format!("reading keypair {keypair_path}: {e}"))?;
    let wallet = keypair.pubkey();

    // ---- results: resume or start ------------------------------------
    let now = now_unix();
    let mut results = if output.exists() {
        let t = std::fs::read_to_string(&output)
            .map_err(|e| format!("reading {}: {e}", output.display()))?;
        let r: RefundResults =
            serde_json::from_str(&t).map_err(|e| format!("parsing {}: {e}", output.display()))?;
        r.verify_matches(&batch, &wallet)?;
        println!("resuming {} (started {})", output.display(), r.started_at);
        r
    } else {
        RefundResults::new_for(&batch, &wallet, now)
    };
    if execute {
        write_results(&output, &results)?;
    }

    println!("{}", if execute { "EXECUTE" } else { "DRY RUN" });
    println!(
        "  batch          {} ({} request(s), digest {})",
        batch.batch_id, batch.request_count, batch.digest
    );
    println!("  rpc            {rpc_url}");
    println!("  refund wallet  {wallet}");
    println!(
        "  mint           {mint} ({} decimals, program {token_program})",
        batch.mint_decimals
    );

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let rpc = RealSolanaRpc::new(rpc_url.clone());
        let policy = ConfirmPolicy {
            deadline: Duration::from_secs(confirm_timeout),
            poll_interval: Duration::from_millis(1_000),
        };

        // ---- mint / balances / recipients, fresh -----------------------
        let mint_account = rpc
            .get_account(&mint)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("mint does not exist on this cluster")?;
        if mint_account.owner != token_program {
            return Err(format!(
                "mint {mint} is owned by {}, not the batch's token program {token_program}",
                mint_account.owner
            ));
        }
        let decimals = accounts::decode_mint_basics(&mint_account.data)
            .map_err(|e| e.to_string())?
            .decimals;
        if decimals != batch.mint_decimals {
            return Err(format!(
                "mint has {decimals} decimals, the batch says {}",
                batch.mint_decimals
            ));
        }
        let wallet_ata = accounts::associated_token_address(&wallet, &mint, &token_program);
        let glc_balance = token_balance(&rpc, &wallet_ata, &mint, &token_program).await?;
        let sol_balance = rpc
            .get_account(&wallet)
            .await
            .map_err(|e| e.to_string())?
            .map(|a| a.lamports)
            .unwrap_or(0);

        let outstanding: Vec<usize> = results
            .results
            .iter()
            .enumerate()
            .filter(|(_, r)| match r.status {
                ResultStatus::Pending | ResultStatus::Submitted => true,
                ResultStatus::Failed => retry_failed,
                ResultStatus::Finalized => false,
            })
            .map(|(i, _)| i)
            .collect();
        let mut glc_needed: u64 = 0;
        let mut missing_atas = 0u64;
        for &i in &outstanding {
            let entry = &batch.requests[i];
            glc_needed = glc_needed
                .checked_add(entry.amount()?)
                .ok_or("GLC total overflows")?;
            let ata: Pubkey = entry
                .recipient_token_account
                .parse()
                .map_err(|_| "malformed recipient_token_account")?;
            let recipient: Pubkey = entry.recipient.parse().map_err(|_| "malformed recipient")?;
            let derived = accounts::associated_token_address(&recipient, &mint, &token_program);
            if derived != ata {
                return Err(format!(
                    "request {}: recipient_token_account {ata} is not ATA({recipient}, {mint}) = \
                     {derived} — refusing the whole batch",
                    entry.request_id
                ));
            }
            if rpc
                .get_account(&ata)
                .await
                .map_err(|e| e.to_string())?
                .is_none()
            {
                missing_atas += 1;
            }
        }
        let sol_needed = (outstanding.len() as u64) * manual_refund::BASE_FEE_LAMPORTS
            + missing_atas * manual_refund::ata_rent_lamports(&token_program)
            + SOL_MARGIN_LAMPORTS;
        println!(
            "  wallet GLC     {} (needed for {} outstanding: {})",
            manual_refund::format_amount(glc_balance, decimals),
            outstanding.len(),
            manual_refund::format_amount(glc_needed, decimals)
        );
        println!(
            "  wallet SOL     {} (needed incl. margin: {}; {} recipient ATA(s) to create)",
            manual_refund::format_amount(sol_balance, 9),
            manual_refund::format_amount(sol_needed, 9),
            missing_atas
        );
        let mut refusals = Vec::new();
        if glc_balance < glc_needed {
            refusals.push(format!(
                "insufficient GLC in {wallet_ata}: have {glc_balance}, need {glc_needed}"
            ));
        }
        if sol_balance < sol_needed {
            refusals.push(format!(
                "insufficient SOL in {wallet}: have {sol_balance} lamports, need {sol_needed}"
            ));
        }
        println!();
        println!(
            "{:>8}  {:<10} {:<44} {:>18}  signature",
            "request", "status", "recipient", "amount GLC"
        );
        for (r, b) in results.results.iter().zip(&batch.requests) {
            println!(
                "{:>8}  {:<10} {:<44} {:>18}  {}",
                r.request_id,
                r.status.as_str(),
                b.recipient,
                b.amount_display,
                r.tx_signature.as_deref().unwrap_or("-")
            );
        }
        println!();
        if !refusals.is_empty() {
            for r in &refusals {
                println!("REFUSED: {r}");
            }
            return Err("balance checks failed — nothing sent".to_string());
        }
        if !execute {
            println!(
                "DRY RUN — {} request(s) would be sent, {} already finalized, {} failed{}. \
                 Nothing sent.",
                outstanding.len(),
                results
                    .results
                    .iter()
                    .filter(|r| r.status == ResultStatus::Finalized)
                    .count(),
                results
                    .results
                    .iter()
                    .filter(|r| r.status == ResultStatus::Failed)
                    .count(),
                if retry_failed { " (retrying)" } else { "" }
            );
            return Ok(());
        }

        // ---- execute, one request at a time ----------------------------
        for i in outstanding {
            let entry = batch.requests[i].clone();
            let expected = expectation_from_entry(&entry, &mint, &token_program, decimals)?;
            let status = results.results[i].status;
            match status {
                ResultStatus::Submitted => {
                    println!(
                        "request {}: resolving recorded transaction …",
                        entry.request_id
                    );
                    let stop = resolve_submitted(
                        &rpc,
                        &mut results,
                        i,
                        &batch,
                        &expected,
                        &wallet,
                        &output,
                        policy,
                    )
                    .await?;
                    if stop {
                        return Err(format!(
                            "request {}: outcome still undetermined — stopping; rerun to continue \
                             recovery. Nothing after it was sent",
                            entry.request_id
                        ));
                    }
                    // A resolved `failed` entry is retried below only on
                    // --retry-failed; a `finalized` one is done.
                    if results.results[i].status != ResultStatus::Failed || !retry_failed {
                        continue;
                    }
                }
                ResultStatus::Failed => {
                    // Re-check the old signature before rebuilding, even
                    // though `failed` means it demonstrably did not land.
                    if let Some(sig) = results.results[i].tx_signature.clone() {
                        let sig: Signature = sig.parse().map_err(|e| format!("{e}"))?;
                        if let Some(Ok(())) = rpc
                            .get_signature_status(&sig)
                            .await
                            .map_err(|e| e.to_string())?
                        {
                            println!(
                                "request {}: the earlier transaction {sig} HAS landed after all — \
                                 verifying instead of resending",
                                entry.request_id
                            );
                            finalize_from_chain(
                                &rpc,
                                &mut results,
                                i,
                                &batch,
                                &expected,
                                &wallet,
                                &output,
                                &sig,
                            )
                            .await?;
                            continue;
                        }
                    }
                }
                ResultStatus::Pending => {}
                ResultStatus::Finalized => continue,
            }

            // Fresh send.
            let fresh_balance = token_balance(&rpc, &wallet_ata, &mint, &token_program).await?;
            if fresh_balance < expected.amount_atomic {
                return Err(format!(
                    "request {}: wallet GLC balance {fresh_balance} < {} — stopping",
                    entry.request_id, expected.amount_atomic
                ));
            }
            let blockhash = rpc
                .get_latest_blockhash()
                .await
                .map_err(|e| e.to_string())?;
            let ixs = manual_refund::build_refund_instructions(
                &wallet,
                &expected.requester,
                &mint,
                &token_program,
                decimals,
                expected.amount_atomic,
                &entry.memo,
            )?;
            let tx =
                Transaction::new_signed_with_payer(&ixs, Some(&wallet), &[&keypair], blockhash);
            let signature = tx.signatures[0];
            {
                let r = &mut results.results[i];
                r.status = ResultStatus::Submitted;
                r.tx_signature = Some(signature.to_string());
                r.recent_blockhash = Some(blockhash.to_string());
                r.submitted_at = Some(now_unix());
                r.error = None;
                r.slot = None;
                r.finalized_at = None;
                r.block_time = None;
            }
            // Durable BEFORE the network sees it.
            write_results(&output, &results)?;
            println!(
                "request {}: sending {} GLC to {} — {signature}",
                entry.request_id, entry.amount_display, entry.recipient
            );
            if let Err(e) = rpc.send_transaction(&tx).await {
                // Ambiguous: it may or may not have reached the network.
                // Leave `submitted`; a rerun resolves it.
                return Err(format!(
                    "request {}: send failed ({e}) — entry left `submitted`; rerun to resolve it \
                     before anything else is sent",
                    entry.request_id
                ));
            }
            match confirm_transaction(&rpc, &signature, &blockhash, policy).await {
                Ok(()) => {
                    finalize_from_chain(
                        &rpc,
                        &mut results,
                        i,
                        &batch,
                        &expected,
                        &wallet,
                        &output,
                        &signature,
                    )
                    .await?;
                }
                Err(ConfirmFailure::Rejected { reason, .. }) => {
                    mark_failed(
                        &mut results,
                        i,
                        &output,
                        format!("landed but failed on chain: {reason}"),
                    )?;
                    println!("request {}: FAILED on chain — {reason}", entry.request_id);
                }
                Err(ConfirmFailure::Expired { .. }) => {
                    if wait_post_expiry(&rpc, &signature).await? {
                        finalize_from_chain(
                            &rpc,
                            &mut results,
                            i,
                            &batch,
                            &expected,
                            &wallet,
                            &output,
                            &signature,
                        )
                        .await?;
                    } else {
                        mark_failed(
                            &mut results,
                            i,
                            &output,
                            "expired before it landed".to_string(),
                        )?;
                        println!(
                            "request {}: expired unseen — recorded as failed",
                            entry.request_id
                        );
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "request {}: {e} — entry left `submitted`; rerun to resolve it before \
                         anything else is sent",
                        entry.request_id
                    ));
                }
            }
        }
        let finalized = results
            .results
            .iter()
            .filter(|r| r.status == ResultStatus::Finalized)
            .count();
        let failed = results
            .results
            .iter()
            .filter(|r| r.status == ResultStatus::Failed)
            .count();
        println!();
        println!(
            "done: {finalized} finalized, {failed} failed, {} pending/submitted — {}",
            results.results.len() - finalized - failed,
            output.display()
        );
        Ok(())
    })
}

fn expectation_from_entry(
    entry: &RefundBatchEntry,
    mint: &Pubkey,
    token_program: &Pubkey,
    decimals: u8,
) -> Result<RefundExpectation, String> {
    let requester: Pubkey = entry.recipient.parse().map_err(|_| "malformed recipient")?;
    let ata: Pubkey = entry
        .recipient_token_account
        .parse()
        .map_err(|_| "malformed recipient_token_account")?;
    Ok(RefundExpectation {
        request_id: entry.request_id,
        obligation_index: entry.source_obligation_index,
        requester,
        mint: *mint,
        token_program: *token_program,
        mint_decimals: decimals,
        recipient_token_account: ata,
        recipient_token_account_exists: entry.recipient_token_account_exists,
        amount_atomic: entry.amount()?,
        amount_canonical_atomic: entry
            .amount_canonical_atomic
            .parse()
            .map_err(|_| "malformed amount_canonical_atomic")?,
    })
}

async fn token_balance(
    rpc: &RealSolanaRpc,
    ata: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<u64, String> {
    match rpc.get_account(ata).await.map_err(|e| e.to_string())? {
        None => Ok(0),
        Some(a) => {
            if a.owner != *token_program {
                return Err(format!(
                    "token account {ata} is owned by {}, not {token_program}",
                    a.owner
                ));
            }
            let m = accounts::decode_token_account_mint(&a.data).map_err(|e| e.to_string())?;
            if m != *mint {
                return Err(format!("token account {ata} holds mint {m}, not {mint}"));
            }
            accounts::decode_token_account_amount(&a.data).map_err(|e| e.to_string())
        }
    }
}

/// Resolves a `submitted` entry from the cluster. Returns `Ok(true)` when
/// the outcome is still undetermined (the caller must stop).
#[allow(clippy::too_many_arguments)]
async fn resolve_submitted(
    rpc: &RealSolanaRpc,
    results: &mut RefundResults,
    i: usize,
    batch: &RefundBatch,
    expected: &RefundExpectation,
    wallet: &Pubkey,
    output: &Path,
    policy: ConfirmPolicy,
) -> Result<bool, String> {
    let sig: Signature = results.results[i]
        .tx_signature
        .as_deref()
        .ok_or("submitted entry has no signature")?
        .parse()
        .map_err(|e| format!("recorded signature: {e}"))?;
    let blockhash: Hash = results.results[i]
        .recent_blockhash
        .as_deref()
        .ok_or("submitted entry has no blockhash")?
        .parse()
        .map_err(|e| format!("recorded blockhash: {e}"))?;
    match rpc
        .get_signature_status(&sig)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(Ok(())) => {
            finalize_from_chain(rpc, results, i, batch, expected, wallet, output, &sig).await?;
            Ok(false)
        }
        Some(Err(reason)) => {
            mark_failed(
                results,
                i,
                output,
                format!("landed but failed on chain: {reason}"),
            )?;
            Ok(false)
        }
        None => {
            let landable = rpc
                .is_blockhash_valid(&blockhash)
                .await
                .map_err(|e| e.to_string())?;
            if landable {
                match confirm_transaction(rpc, &sig, &blockhash, policy).await {
                    Ok(()) => {
                        finalize_from_chain(rpc, results, i, batch, expected, wallet, output, &sig)
                            .await?;
                        Ok(false)
                    }
                    Err(ConfirmFailure::Rejected { reason, .. }) => {
                        mark_failed(
                            results,
                            i,
                            output,
                            format!("landed but failed on chain: {reason}"),
                        )?;
                        Ok(false)
                    }
                    Err(ConfirmFailure::Expired { .. }) => {
                        if wait_post_expiry(rpc, &sig).await? {
                            finalize_from_chain(
                                rpc, results, i, batch, expected, wallet, output, &sig,
                            )
                            .await?;
                        } else {
                            mark_failed(
                                results,
                                i,
                                output,
                                "expired before it landed".to_string(),
                            )?;
                        }
                        Ok(false)
                    }
                    Err(_) => Ok(true),
                }
            } else if wait_post_expiry(rpc, &sig).await? {
                finalize_from_chain(rpc, results, i, batch, expected, wallet, output, &sig).await?;
                Ok(false)
            } else {
                mark_failed(results, i, output, "expired before it landed".to_string())?;
                Ok(false)
            }
        }
    }
}

/// After a blockhash is reported expired, keeps asking for the
/// signature for [`POST_EXPIRY_GRACE`]. `Ok(true)` = it landed (success).
/// A landed FAILURE is reported as an error so nothing is rebuilt on it
/// without a rerun's fresh look.
async fn wait_post_expiry(rpc: &RealSolanaRpc, sig: &Signature) -> Result<bool, String> {
    let started = std::time::Instant::now();
    loop {
        match rpc
            .get_signature_status(sig)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(Ok(())) => return Ok(true),
            Some(Err(reason)) => {
                return Err(format!(
                    "{sig} landed after its blockhash expired and FAILED on chain ({reason}); \
                     rerun to record it"
                ))
            }
            None => {}
        }
        if started.elapsed() >= POST_EXPIRY_GRACE {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Reads the landed transaction back, verifies it IS this request's
/// refund, and records `finalized`. A landed transaction that does not
/// verify is a stop-the-world anomaly, not a retry.
#[allow(clippy::too_many_arguments)]
async fn finalize_from_chain(
    rpc: &RealSolanaRpc,
    results: &mut RefundResults,
    i: usize,
    batch: &RefundBatch,
    expected: &RefundExpectation,
    wallet: &Pubkey,
    output: &Path,
    sig: &Signature,
) -> Result<(), String> {
    let tx = rpc
        .get_finalized_transaction(sig)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "request {}: {sig} reports finalized success but getTransaction returns nothing — \
                 entry left as is; rerun",
                expected.request_id
            )
        })?;
    let landed = manual_refund::decode_landed_refund(sig, &tx).map_err(|e| {
        format!(
            "request {}: CRITICAL — landed transaction {sig} does not decode as a refund: {e}",
            expected.request_id
        )
    })?;
    manual_refund::verify_landed_refund(&landed, expected, &batch.batch_id, wallet).map_err(
        |e| {
            format!(
            "request {}: CRITICAL — landed transaction {sig} is not this request's refund: {e}. \
             Nothing further sent; investigate before rerunning",
            expected.request_id
        )
        },
    )?;
    let r: &mut RefundResultEntry = &mut results.results[i];
    r.status = ResultStatus::Finalized;
    r.slot = Some(landed.slot);
    r.block_time = landed.block_time;
    r.finalized_at = Some(now_unix());
    r.error = None;
    write_results(output, results)?;
    println!(
        "request {}: FINALIZED in slot {} — {sig}",
        expected.request_id, landed.slot
    );
    Ok(())
}

fn mark_failed(
    results: &mut RefundResults,
    i: usize,
    output: &Path,
    error: String,
) -> Result<(), String> {
    let r = &mut results.results[i];
    r.status = ResultStatus::Failed;
    r.error = Some(error);
    write_results(output, results)
}

/// Atomic: write next to the target, then rename over it.
fn write_results(path: &Path, results: &RefundResults) -> Result<(), String> {
    let mut results = results.clone();
    results.updated_at = now_unix();
    let json = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("renaming {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}
