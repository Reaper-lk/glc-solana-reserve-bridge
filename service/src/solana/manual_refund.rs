//! Manual (out-of-band) Solana refunds — the CHAIN half.
//!
//! The deployed program does not dispatch `refund_withdraw`
//! ([`crate::solana::program_compat`]), so the parked Solana-sourced
//! backlog is refunded from a SEPARATELY FUNDED operator wallet that this
//! service never holds. Three steps, three tools, one JSON contract:
//!
//! 1. **Export** (`glc-admin manual-refund-export`, read-only): for every
//!    `ManualReview` request the ledger says may be refunded on Solana
//!    ([`crate::ledger::Ledger::manual_solana_refund_eligibility`]) this
//!    module reads the on-chain `WithdrawalObligation` and refuses any
//!    disagreement — requester, amount, status — exactly as the in-band
//!    refund path does ([`crate::solana::refund::build_refund_plan`]).
//!    What comes out is a [`RefundBatch`]: recipient = the obligation's
//!    requester, amount = the obligation's `amount` (which must equal the
//!    stored canonical gross narrowed to the mint's decimals), mint = the
//!    on-chain reserve mint (which must equal the configured one). Nothing
//!    here is typed by an operator; there is no override flag.
//! 2. **Send** (`solana-manual-refund-batch`, a standalone binary): builds
//!    ONE transaction per request — `[create ATA (idempotent), memo naming
//!    the batch and request, transfer_checked(amount, decimals)]` — signs
//!    it with the operator's wallet, records the signature in
//!    [`RefundResults`] BEFORE broadcasting, and waits for `finalized`.
//! 3. **Import** (`glc-admin manual-refund-import`): reads every
//!    `finalized` result's transaction back from the cluster at
//!    `finalized` commitment ([`decode_landed_refund`]), verifies it
//!    against a FRESH expectation ([`verify_landed_refund`]: memo,
//!    recipient token account, mint, decimals, exact amount, the wallet as
//!    fee payer and authority, the recipient's post-minus-pre balance), and
//!    only then records it and closes the request as
//!    `refunded_out_of_band` with the signature as the reference
//!    ([`crate::ledger::Ledger::record_manual_solana_refund`]).
//!
//! The memo is what binds a signature to a request ON CHAIN: a transaction
//! whose memo names another request, or none, proves nothing about this
//! one, however well its amount matches.

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::instruction::Instruction;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status_client_types::option_serializer::OptionSerializer;
use solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta;

use crate::amount_conversion::CanonicalAtomic;
use crate::ledger::{BridgeRequest, Ledger, ManualSolanaRefundInputs};
use crate::solana::accounts;
use crate::solana::rpc::{SolanaRpc, SolanaTransactionLookup};

pub const BATCH_SCHEMA: &str = "glc-manual-solana-refund-batch/1";
pub const RESULTS_SCHEMA: &str = "glc-manual-solana-refund-results/1";
/// The SPL Memo program (v2).
pub const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const MEMO_PREFIX: &str = "glc-manual-refund";
/// One signature's base fee, lamports.
pub const BASE_FEE_LAMPORTS: u64 = 5_000;
/// Rent for a Token-2022 ATA carrying the `ImmutableOwner` extension
/// (170 bytes) / a legacy 165-byte account — what a missing recipient
/// ATA costs the refund wallet.
pub fn ata_rent_lamports(token_program: &Pubkey) -> u64 {
    let size = if *token_program == spl_token_2022::ID {
        170
    } else {
        165
    };
    solana_sdk::rent::Rent::default().minimum_balance(size)
}

pub fn memo_program_id() -> Pubkey {
    Pubkey::from_str(MEMO_PROGRAM).expect("constant memo program id")
}

/// The on-chain memo of `request_id`'s refund in `batch_id`.
pub fn memo_text(batch_id: &str, request_id: i64) -> String {
    format!("{MEMO_PREFIX}:{batch_id}:{request_id}")
}

/// `amount` in the mint's decimals, rendered `"50000.000000"`.
pub fn format_amount(amount: u64, decimals: u8) -> String {
    let scale = 10u64.pow(u32::from(decimals));
    if decimals == 0 {
        return amount.to_string();
    }
    format!(
        "{}.{:0width$}",
        amount / scale,
        amount % scale,
        width = usize::from(decimals)
    )
}

// ----------------------------------------------------------------- JSON --

/// `refund-backlog.json` — the export's output and the sender's input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundBatch {
    pub schema: String,
    pub batch_id: String,
    pub exported_at: i64,
    pub exported_by: String,
    /// Always `"solana"`.
    pub network: String,
    pub rpc_url: String,
    pub ledger_path: String,
    pub mint: String,
    pub mint_decimals: u8,
    pub token_program: String,
    pub token_symbol: String,
    pub request_count: usize,
    pub excluded_count: usize,
    /// Sum over `requests`, mint units (string, like every atomic amount
    /// on the wire — docs/31).
    pub total_amount_atomic: String,
    pub total_amount_display: String,
    /// `request_count * base fee + missing ATAs * rent`.
    pub estimated_sol_lamports: u64,
    pub estimated_sol_display: String,
    pub requests: Vec<RefundBatchEntry>,
    pub excluded: Vec<ExcludedEntry>,
    /// [`batch_digest`] over `requests` — the sender refuses a file whose
    /// entries no longer match it.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundBatchEntry {
    pub request_id: i64,
    pub route: String,
    pub state: String,
    pub manual_review_reason: Option<String>,
    pub created_at: i64,
    /// The on-chain `WithdrawalObligation` index — the deposit record.
    pub source_obligation_index: u64,
    /// The depositor wallet (obligation requester == stored requester).
    pub recipient: String,
    /// ATA(recipient, mint, token program) — derived, never supplied.
    pub recipient_token_account: String,
    pub recipient_token_account_exists: bool,
    pub mint: String,
    /// Mint units.
    pub amount_atomic: String,
    pub amount_display: String,
    /// Canonical 8-decimal units (the request's gross deposit).
    pub amount_canonical_atomic: String,
    pub memo: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedEntry {
    pub request_id: i64,
    pub route: String,
    pub state: String,
    pub gross_amount_canonical_atomic: String,
    pub reason: String,
}

/// `refund-results.json` — the sender's durable per-request record and
/// the import's input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundResults {
    pub schema: String,
    pub batch_id: String,
    pub batch_digest: String,
    pub network: String,
    pub rpc_url: String,
    pub mint: String,
    pub mint_decimals: u8,
    pub token_program: String,
    /// The sending wallet's public key.
    pub refund_wallet: String,
    pub started_at: i64,
    pub updated_at: i64,
    pub results: Vec<RefundResultEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    /// Not sent.
    Pending,
    /// Signed and recorded; broadcast attempted; fate not yet known.
    /// A rerun RESOLVES this before it sends anything else.
    Submitted,
    /// Landed and verified at `finalized` commitment.
    Finalized,
    /// Demonstrably did not move funds (expired unseen, or landed and
    /// failed on chain). Stays here until rerun with `--retry-failed`.
    Failed,
}

impl ResultStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ResultStatus::Pending => "pending",
            ResultStatus::Submitted => "submitted",
            ResultStatus::Finalized => "finalized",
            ResultStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundResultEntry {
    pub request_id: i64,
    pub recipient: String,
    pub recipient_token_account: String,
    pub mint: String,
    pub amount_atomic: String,
    pub amount_display: String,
    pub memo: String,
    pub refund_wallet: String,
    pub status: ResultStatus,
    pub tx_signature: Option<String>,
    pub recent_blockhash: Option<String>,
    pub slot: Option<u64>,
    pub block_time: Option<i64>,
    pub submitted_at: Option<i64>,
    pub finalized_at: Option<i64>,
    pub error: Option<String>,
}

impl RefundResults {
    /// A fresh results file for `batch`: every request `pending`.
    pub fn new_for(batch: &RefundBatch, refund_wallet: &Pubkey, now: i64) -> RefundResults {
        RefundResults {
            schema: RESULTS_SCHEMA.to_string(),
            batch_id: batch.batch_id.clone(),
            batch_digest: batch.digest.clone(),
            network: batch.network.clone(),
            rpc_url: batch.rpc_url.clone(),
            mint: batch.mint.clone(),
            mint_decimals: batch.mint_decimals,
            token_program: batch.token_program.clone(),
            refund_wallet: refund_wallet.to_string(),
            started_at: now,
            updated_at: now,
            results: batch
                .requests
                .iter()
                .map(|e| RefundResultEntry {
                    request_id: e.request_id,
                    recipient: e.recipient.clone(),
                    recipient_token_account: e.recipient_token_account.clone(),
                    mint: e.mint.clone(),
                    amount_atomic: e.amount_atomic.clone(),
                    amount_display: e.amount_display.clone(),
                    memo: e.memo.clone(),
                    refund_wallet: refund_wallet.to_string(),
                    status: ResultStatus::Pending,
                    tx_signature: None,
                    recent_blockhash: None,
                    slot: None,
                    block_time: None,
                    submitted_at: None,
                    finalized_at: None,
                    error: None,
                })
                .collect(),
        }
    }

    /// Refuses a results file that does not belong to `batch` or whose
    /// per-request figures drifted from it.
    pub fn verify_matches(&self, batch: &RefundBatch, wallet: &Pubkey) -> Result<(), String> {
        if self.schema != RESULTS_SCHEMA {
            return Err(format!(
                "results schema is {:?}, expected {RESULTS_SCHEMA:?}",
                self.schema
            ));
        }
        if self.batch_id != batch.batch_id {
            return Err(format!(
                "results file belongs to batch {} but the input is batch {}",
                self.batch_id, batch.batch_id
            ));
        }
        if self.batch_digest != batch.digest {
            return Err(
                "results file was produced from a different version of this batch \
                        (digest mismatch)"
                    .to_string(),
            );
        }
        if self.refund_wallet != wallet.to_string() {
            return Err(format!(
                "results file was produced with wallet {} but the supplied keypair is {}",
                self.refund_wallet, wallet
            ));
        }
        if self.results.len() != batch.requests.len() {
            return Err("results file and batch disagree on the request count".to_string());
        }
        for (r, b) in self.results.iter().zip(&batch.requests) {
            if r.request_id != b.request_id
                || r.recipient != b.recipient
                || r.recipient_token_account != b.recipient_token_account
                || r.amount_atomic != b.amount_atomic
                || r.mint != b.mint
                || r.memo != b.memo
            {
                return Err(format!(
                    "results entry for request {} does not match the batch entry",
                    r.request_id
                ));
            }
        }
        Ok(())
    }
}

/// SHA-256 over `request_id|recipient|mint|amount_atomic\n` per entry,
/// ascending request id — reproducible from any language.
pub fn batch_digest(entries: &[RefundBatchEntry]) -> String {
    let mut lines: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "{}|{}|{}|{}\n",
                e.request_id, e.recipient, e.mint, e.amount_atomic
            )
        })
        .collect();
    lines.sort();
    let mut h = Sha256::new();
    for l in lines {
        h.update(l.as_bytes());
    }
    hex_lower(&h.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl RefundBatch {
    /// Structural checks a consumer runs before trusting the file.
    pub fn verify(&self) -> Result<(), String> {
        if self.schema != BATCH_SCHEMA {
            return Err(format!(
                "batch schema is {:?}, expected {BATCH_SCHEMA:?}",
                self.schema
            ));
        }
        if self.network != "solana" {
            return Err(format!(
                "batch network is {:?}, expected solana",
                self.network
            ));
        }
        if self.digest != batch_digest(&self.requests) {
            return Err(
                "batch digest does not match its entries — the file was edited after \
                        export; re-export"
                    .to_string(),
            );
        }
        let mut ids = BTreeSet::new();
        for e in &self.requests {
            if !ids.insert(e.request_id) {
                return Err(format!("request {} appears twice", e.request_id));
            }
            if e.mint != self.mint {
                return Err(format!(
                    "request {}: mint differs from the batch mint",
                    e.request_id
                ));
            }
            if e.memo != memo_text(&self.batch_id, e.request_id) {
                return Err(format!(
                    "request {}: memo does not name this batch/request",
                    e.request_id
                ));
            }
            e.amount()?;
            Pubkey::from_str(&e.recipient)
                .map_err(|_| format!("request {}: malformed recipient", e.request_id))?;
        }
        Ok(())
    }
}

impl RefundBatchEntry {
    pub fn amount(&self) -> Result<u64, String> {
        self.amount_atomic
            .parse::<u64>()
            .ok()
            .filter(|a| *a > 0)
            .ok_or_else(|| format!("request {}: malformed amount_atomic", self.request_id))
    }
}

// ---------------------------------------------------------- expectation --

/// What the refund of one request MUST look like — assembled from the
/// ledger row plus fresh `finalized` chain reads, every cross-check
/// already enforced (a mismatch is an `Err`, never a value in here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundExpectation {
    pub request_id: i64,
    pub obligation_index: u64,
    pub requester: Pubkey,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub mint_decimals: u8,
    pub recipient_token_account: Pubkey,
    pub recipient_token_account_exists: bool,
    /// Mint units — the obligation's own `amount`.
    pub amount_atomic: u64,
    pub amount_canonical_atomic: u64,
}

/// Builds the expectation for `request` — read-only, fail-closed.
/// `configured_mint` is `[solana].reserve_token_mint`; it must equal the
/// on-chain `BridgeConfig.reserve_token_mint`.
pub async fn chain_expectation<R: SolanaRpc>(
    rpc: &R,
    request: &BridgeRequest,
    configured_mint: &Pubkey,
) -> Result<RefundExpectation, String> {
    if !request.direction.source_is_solana() {
        return Err(format!(
            "request {} is {}, not Solana-sourced",
            request.id,
            request.direction.as_str()
        ));
    }
    let obligation_index = request
        .source_obligation_index
        .ok_or_else(|| format!("request {} has no source_obligation_index", request.id))?;
    let stored_requester = request
        .requester
        .ok_or_else(|| format!("request {} has no requester recorded", request.id))?;

    let config_account = rpc
        .get_account(&accounts::bridge_config_pda())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("bridge_config does not exist on this cluster")?;
    let config = accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
    if config.reserve_token_mint == Pubkey::default()
        || config.reserve_token_program == Pubkey::default()
    {
        return Err("reserve vault is not configured on this deployment".to_string());
    }
    if config.reserve_token_mint != *configured_mint {
        return Err(format!(
            "REFUSING — configured reserve_token_mint {configured_mint} is not the on-chain \
             reserve mint {}",
            config.reserve_token_mint
        ));
    }

    let obligation_pda = accounts::withdrawal_obligation_pda(obligation_index);
    let obligation_account = rpc
        .get_account(&obligation_pda)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "withdrawal obligation #{obligation_index} does not exist at {obligation_pda} — \
                 refusing (the finalized deposit record is the refund's ground truth)"
            )
        })?;
    let obligation = accounts::decode_withdrawal_obligation(&obligation_account.data)
        .map_err(|e| e.to_string())?;
    if obligation.index != obligation_index {
        return Err(format!(
            "obligation PDA {obligation_pda} decodes to index {}, expected {obligation_index}",
            obligation.index
        ));
    }
    if obligation.requester.to_bytes() != stored_requester {
        return Err(format!(
            "REFUSING — stored requester does not match the on-chain obligation's requester \
             ({}); the database and chain disagree about the original sender",
            obligation.requester
        ));
    }
    if obligation.status != accounts::WITHDRAWAL_STATUS_PENDING {
        return Err(format!(
            "REFUSING — on-chain obligation #{obligation_index} status is {} ({}), not Pending: \
             it already reached a terminal outcome on chain (a competing settlement or refund)",
            obligation.status,
            accounts::withdrawal_status_name(obligation.status)
        ));
    }

    let mint_account = rpc
        .get_account(&config.reserve_token_mint)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("reserve mint {} does not exist", config.reserve_token_mint))?;
    if mint_account.owner != config.reserve_token_program {
        return Err(format!(
            "REFUSING — reserve mint {} is owned by {}, not the configured reserve token \
             program {}",
            config.reserve_token_mint, mint_account.owner, config.reserve_token_program
        ));
    }
    let mint_decimals = accounts::decode_mint_basics(&mint_account.data)
        .map_err(|e| e.to_string())?
        .decimals;
    let expected_native = CanonicalAtomic(request.gross_amount_atomic)
        .to_solana(mint_decimals)
        .map_err(|e| format!("stored canonical gross does not narrow exactly: {e}"))?;
    if expected_native.0 != obligation.amount {
        return Err(format!(
            "REFUSING — stored gross ({} canonical -> {} native) does not equal the on-chain \
             deposited amount ({} native)",
            request.gross_amount_atomic, expected_native.0, obligation.amount
        ));
    }
    if obligation.amount == 0 {
        return Err("REFUSING — the on-chain obligation amount is zero".to_string());
    }

    let requester = obligation.requester;
    let recipient_token_account = accounts::associated_token_address(
        &requester,
        &config.reserve_token_mint,
        &config.reserve_token_program,
    );
    let recipient_token_account_exists = match rpc
        .get_account(&recipient_token_account)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(account) => {
            let dest_mint =
                accounts::decode_token_account_mint(&account.data).map_err(|e| e.to_string())?;
            if dest_mint != config.reserve_token_mint {
                return Err(format!(
                    "REFUSING — recipient token account {recipient_token_account} has mint \
                     {dest_mint}, expected the reserve mint {}",
                    config.reserve_token_mint
                ));
            }
            if account.owner != config.reserve_token_program {
                return Err(format!(
                    "REFUSING — recipient token account {recipient_token_account} is owned by \
                     program {}, expected {}",
                    account.owner, config.reserve_token_program
                ));
            }
            true
        }
        None => false,
    };

    Ok(RefundExpectation {
        request_id: request.id,
        obligation_index,
        requester,
        mint: config.reserve_token_mint,
        token_program: config.reserve_token_program,
        mint_decimals,
        recipient_token_account,
        recipient_token_account_exists,
        amount_atomic: obligation.amount,
        amount_canonical_atomic: request.gross_amount_atomic,
    })
}

// ---------------------------------------------------------------- export --

/// What the export produced, plus the per-request refusals.
#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub batch: RefundBatch,
    pub manual_review_total: usize,
}

/// `manual-refund-export`'s body. `ids`: `None` = every `ManualReview`
/// request; `Some` = exactly these (each still checked). Read-only: no
/// ledger write, no signer, no broadcast. Ineligible requests are
/// REPORTED (`batch.excluded`), never silently dropped, and never
/// included.
#[allow(clippy::too_many_arguments)]
pub async fn export_backlog<R: SolanaRpc>(
    rpc: &R,
    ledger: &Ledger,
    ids: Option<Vec<i64>>,
    configured_mint: &Pubkey,
    rpc_url: &str,
    ledger_path: &str,
    actor: &str,
    now: i64,
) -> Result<ExportOutcome, String> {
    let manual_review = ledger
        .manual_review_request_ids()
        .map_err(|e| e.to_string())?;
    let candidates: Vec<i64> = match ids {
        Some(explicit) => {
            let mut v = explicit;
            v.sort_unstable();
            v.dedup();
            v
        }
        None => manual_review.clone(),
    };
    let batch_id = format!("mrb-{}", batch_stamp(now, &candidates));
    let mut requests = Vec::new();
    let mut excluded = Vec::new();
    let mut mint_decimals: Option<u8> = None;
    let mut token_program: Option<Pubkey> = None;
    let mut total: u64 = 0;
    let mut missing_atas: usize = 0;
    for id in candidates {
        let request = match ledger.manual_solana_refund_eligibility(id, now) {
            Ok(r) => r,
            Err(e) => {
                let (route, state, gross) =
                    match ledger.get_request(id).map_err(|e| e.to_string())? {
                        Some(r) => (
                            r.direction.as_str().to_string(),
                            r.state.as_str().to_string(),
                            r.gross_amount_atomic.to_string(),
                        ),
                        None => ("?".to_string(), "?".to_string(), "0".to_string()),
                    };
                excluded.push(ExcludedEntry {
                    request_id: id,
                    route,
                    state,
                    gross_amount_canonical_atomic: gross,
                    reason: match e {
                        crate::ledger::LedgerError::ManualReviewNotRecoverable {
                            detail, ..
                        } => detail,
                        other => other.to_string(),
                    },
                });
                continue;
            }
        };
        let exp = match chain_expectation(rpc, &request, configured_mint).await {
            Ok(e) => e,
            Err(reason) => {
                excluded.push(ExcludedEntry {
                    request_id: id,
                    route: request.direction.as_str().to_string(),
                    state: request.state.as_str().to_string(),
                    gross_amount_canonical_atomic: request.gross_amount_atomic.to_string(),
                    reason,
                });
                continue;
            }
        };
        match (mint_decimals, token_program) {
            (None, None) => {
                mint_decimals = Some(exp.mint_decimals);
                token_program = Some(exp.token_program);
            }
            (Some(d), Some(p)) if d == exp.mint_decimals && p == exp.token_program => {}
            _ => {
                return Err(format!(
                    "request {id}: mint decimals/token program differ from earlier requests in \
                     this batch — ambiguous chain state, refusing"
                ));
            }
        }
        total = total
            .checked_add(exp.amount_atomic)
            .ok_or("total amount overflows u64")?;
        if !exp.recipient_token_account_exists {
            missing_atas += 1;
        }
        requests.push(RefundBatchEntry {
            request_id: id,
            route: request.direction.as_str().to_string(),
            state: request.state.as_str().to_string(),
            manual_review_reason: request.manual_review_note.clone(),
            created_at: request.created_at,
            source_obligation_index: exp.obligation_index,
            recipient: exp.requester.to_string(),
            recipient_token_account: exp.recipient_token_account.to_string(),
            recipient_token_account_exists: exp.recipient_token_account_exists,
            mint: exp.mint.to_string(),
            amount_atomic: exp.amount_atomic.to_string(),
            amount_display: format_amount(exp.amount_atomic, exp.mint_decimals),
            amount_canonical_atomic: exp.amount_canonical_atomic.to_string(),
            memo: memo_text(&batch_id, id),
        });
    }
    // With no eligible request the mint facts are read directly so the
    // report still names them.
    let (decimals, program) = match (mint_decimals, token_program) {
        (Some(d), Some(p)) => (d, p),
        _ => {
            let config_account = rpc
                .get_account(&accounts::bridge_config_pda())
                .await
                .map_err(|e| e.to_string())?
                .ok_or("bridge_config does not exist on this cluster")?;
            let config =
                accounts::decode_bridge_config(&config_account.data).map_err(|e| e.to_string())?;
            let d = accounts::fetch_reserve_mint_decimals(rpc, configured_mint)
                .await
                .map_err(|e| e.to_string())?;
            (d, config.reserve_token_program)
        }
    };
    let estimated_sol_lamports = (requests.len() as u64) * BASE_FEE_LAMPORTS
        + (missing_atas as u64) * ata_rent_lamports(&program);
    let digest = batch_digest(&requests);
    let batch = RefundBatch {
        schema: BATCH_SCHEMA.to_string(),
        batch_id,
        exported_at: now,
        exported_by: actor.to_string(),
        network: "solana".to_string(),
        rpc_url: rpc_url.to_string(),
        ledger_path: ledger_path.to_string(),
        mint: configured_mint.to_string(),
        mint_decimals: decimals,
        token_program: program.to_string(),
        token_symbol: "GLC".to_string(),
        request_count: requests.len(),
        excluded_count: excluded.len(),
        total_amount_atomic: total.to_string(),
        total_amount_display: format_amount(total, decimals),
        estimated_sol_lamports,
        estimated_sol_display: format_amount(estimated_sol_lamports, 9),
        requests,
        excluded,
        digest,
    };
    Ok(ExportOutcome {
        batch,
        manual_review_total: manual_review.len(),
    })
}

/// `YYYYMMDDThhmmssZ-<8 hex of sha256(now, ids)>` — unique per export.
fn batch_stamp(now: i64, ids: &[i64]) -> String {
    let mut h = Sha256::new();
    h.update(now.to_le_bytes());
    for id in ids {
        h.update(id.to_le_bytes());
    }
    let d = h.finalize();
    format!("{}-{}", utc_stamp(now), hex_lower(&d[..4]))
}

/// Compact UTC stamp without a chrono dependency (unix seconds ->
/// `YYYYMMDDThhmmssZ`; proleptic Gregorian, civil-from-days).
pub fn utc_stamp(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ------------------------------------------------------ transaction shape --

/// The three instructions of one refund, in order. `payer` is the refund
/// wallet: it funds the (idempotent) ATA creation, signs the memo, and is
/// the `transfer_checked` authority over ITS OWN ATA.
pub fn build_refund_instructions(
    refund_wallet: &Pubkey,
    recipient: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    decimals: u8,
    amount: u64,
    memo: &str,
) -> Result<Vec<Instruction>, String> {
    let source = accounts::associated_token_address(refund_wallet, mint, token_program);
    let destination = accounts::associated_token_address(recipient, mint, token_program);
    let create_ata =
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            refund_wallet,
            recipient,
            mint,
            token_program,
        );
    let memo_ix = Instruction {
        program_id: memo_program_id(),
        accounts: vec![solana_sdk::instruction::AccountMeta::new_readonly(
            *refund_wallet,
            true,
        )],
        data: memo.as_bytes().to_vec(),
    };
    let transfer = spl_token_2022::instruction::transfer_checked(
        token_program,
        &source,
        mint,
        &destination,
        refund_wallet,
        &[],
        amount,
        decimals,
    )
    .map_err(|e| format!("building transfer_checked: {e}"))?;
    Ok(vec![create_ata, memo_ix, transfer])
}

// ------------------------------------------------------- landed refund --

/// What a finalized transaction PROVES, decoded from its bytes and its
/// token-balance meta — never from a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedRefund {
    pub signature: Signature,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub fee_payer: Pubkey,
    pub memo: String,
    pub token_program: Pubkey,
    pub source_token_account: Pubkey,
    pub destination_token_account: Pubkey,
    pub mint: Pubkey,
    pub authority: Pubkey,
    pub amount: u64,
    pub decimals: u8,
    /// The destination's `post - pre` token balance from the transaction
    /// meta, when the RPC supplied both.
    pub destination_delta: Option<u64>,
    /// The mint the meta reports for the destination balance.
    pub destination_meta_mint: Option<Pubkey>,
}

/// Decodes a `getTransaction` response into the one refund shape this
/// module sends. Refuses (fail-closed) anything else: an on-chain error,
/// an undecodable transaction, address-table lookups, more than one
/// signer, an instruction from a program other than {ATA, memo, token},
/// zero or several memos, zero or several transfers, a non-`transfer_checked`
/// token instruction.
pub fn decode_landed_refund(
    signature: &Signature,
    tx: &EncodedConfirmedTransactionWithStatusMeta,
) -> Result<LandedRefund, String> {
    let meta = tx
        .transaction
        .meta
        .as_ref()
        .ok_or("transaction has no meta (RPC returned no status) — ambiguous, refusing")?;
    if let Some(err) = &meta.err {
        return Err(format!("transaction landed but FAILED on chain: {err:?}"));
    }
    let decoded = tx
        .transaction
        .transaction
        .decode()
        .ok_or("transaction could not be decoded from the RPC encoding")?;
    if decoded.signatures.first() != Some(signature) {
        return Err("decoded transaction's first signature is not the requested one".to_string());
    }
    let message: &VersionedMessage = &decoded.message;
    if message
        .address_table_lookups()
        .map(|l| !l.is_empty())
        .unwrap_or(false)
    {
        return Err(
            "transaction uses address-table lookups — not a refund this tool sent".to_string(),
        );
    }
    if message.header().num_required_signatures != 1 {
        return Err(format!(
            "transaction has {} required signers, expected exactly 1",
            message.header().num_required_signatures
        ));
    }
    let keys = message.static_account_keys();
    let fee_payer = *keys.first().ok_or("transaction has no accounts")?;
    let memo_program = memo_program_id();
    let mut memo: Option<String> = None;
    let mut transfer: Option<(Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, u64, u8)> = None;
    for ix in message.instructions() {
        let program = *keys
            .get(usize::from(ix.program_id_index))
            .ok_or("instruction names an account index out of range")?;
        if program == spl_associated_token_account::ID {
            continue;
        }
        if program == memo_program {
            if memo.is_some() {
                return Err("transaction carries more than one memo".to_string());
            }
            memo = Some(
                String::from_utf8(ix.data.clone()).map_err(|_| "memo is not UTF-8".to_string())?,
            );
            continue;
        }
        if program == spl_token::ID || program == spl_token_2022::ID {
            let parsed = spl_token_2022::instruction::TokenInstruction::unpack(&ix.data)
                .map_err(|e| format!("token instruction does not unpack: {e}"))?;
            let (amount, decimals) = match parsed {
                spl_token_2022::instruction::TokenInstruction::TransferChecked {
                    amount,
                    decimals,
                } => (amount, decimals),
                other => {
                    return Err(format!(
                        "token instruction is {:?}, expected TransferChecked",
                        std::mem::discriminant(&other)
                    ))
                }
            };
            if transfer.is_some() {
                return Err("transaction carries more than one token transfer".to_string());
            }
            let acct = |i: usize| -> Result<Pubkey, String> {
                let idx = *ix
                    .accounts
                    .get(i)
                    .ok_or("transfer_checked has too few accounts")?;
                keys.get(usize::from(idx))
                    .copied()
                    .ok_or_else(|| "transfer_checked account index out of range".to_string())
            };
            transfer = Some((
                program,
                acct(0)?,
                acct(1)?,
                acct(2)?,
                acct(3)?,
                amount,
                decimals,
            ));
            continue;
        }
        return Err(format!(
            "transaction invokes program {program}, which a refund never does"
        ));
    }
    let memo = memo.ok_or("transaction carries no memo — it cannot be bound to a request")?;
    let (token_program, source, mint, destination, authority, amount, decimals) =
        transfer.ok_or("transaction carries no token transfer")?;

    // Destination balance delta from the meta, when supplied.
    let dest_index = keys.iter().position(|k| *k == destination);
    let balance_of = |balances: &OptionSerializer<Vec<_>>| -> Option<(u64, Pubkey)> {
        let list: Option<&Vec<solana_transaction_status_client_types::UiTransactionTokenBalance>> =
            match balances {
                OptionSerializer::Some(v) => Some(v),
                _ => None,
            };
        let idx = dest_index?;
        let entry = list?.iter().find(|b| usize::from(b.account_index) == idx)?;
        let amount = entry.ui_token_amount.amount.parse::<u64>().ok()?;
        let mint = Pubkey::from_str(&entry.mint).ok()?;
        Some((amount, mint))
    };
    let pre = balance_of(&meta.pre_token_balances);
    let post = balance_of(&meta.post_token_balances);
    let (destination_delta, destination_meta_mint) = match (pre, post) {
        (Some((pre_amt, pre_mint)), Some((post_amt, post_mint))) => {
            if pre_mint != post_mint {
                return Err("destination token balance meta names two different mints".to_string());
            }
            (post_amt.checked_sub(pre_amt), Some(post_mint))
        }
        // A freshly created ATA has no pre-balance entry: pre = 0.
        (None, Some((post_amt, post_mint))) => (Some(post_amt), Some(post_mint)),
        _ => (None, None),
    };

    Ok(LandedRefund {
        signature: *signature,
        slot: tx.slot,
        block_time: tx.block_time,
        fee_payer,
        memo,
        token_program,
        source_token_account: source,
        destination_token_account: destination,
        mint,
        authority,
        amount,
        decimals,
        destination_delta,
        destination_meta_mint,
    })
}

/// The exact checks that make a landed transaction THE refund of one
/// request. `refund_wallet`: the wallet the manifest claims sent it.
pub fn verify_landed_refund(
    landed: &LandedRefund,
    expected: &RefundExpectation,
    batch_id: &str,
    refund_wallet: &Pubkey,
) -> Result<(), String> {
    let want_memo = memo_text(batch_id, expected.request_id);
    if landed.memo != want_memo {
        return Err(format!(
            "memo is {:?}, expected {want_memo:?} — this transaction is not bound to request {}",
            landed.memo, expected.request_id
        ));
    }
    if landed.token_program != expected.token_program {
        return Err(format!(
            "token program is {}, expected {}",
            landed.token_program, expected.token_program
        ));
    }
    if landed.mint != expected.mint {
        return Err(format!(
            "mint is {}, expected the reserve mint {}",
            landed.mint, expected.mint
        ));
    }
    if landed.destination_token_account != expected.recipient_token_account {
        return Err(format!(
            "destination token account is {}, expected {} (ATA of the request's requester {})",
            landed.destination_token_account, expected.recipient_token_account, expected.requester
        ));
    }
    if landed.amount != expected.amount_atomic {
        return Err(format!(
            "amount is {}, expected exactly {}",
            landed.amount, expected.amount_atomic
        ));
    }
    if landed.decimals != expected.mint_decimals {
        return Err(format!(
            "decimals is {}, expected {}",
            landed.decimals, expected.mint_decimals
        ));
    }
    if landed.fee_payer != *refund_wallet || landed.authority != *refund_wallet {
        return Err(format!(
            "fee payer {} / authority {} is not the declared refund wallet {refund_wallet}",
            landed.fee_payer, landed.authority
        ));
    }
    let wallet_ata =
        accounts::associated_token_address(refund_wallet, &expected.mint, &expected.token_program);
    if landed.source_token_account != wallet_ata {
        return Err(format!(
            "source token account {} is not the refund wallet's ATA {wallet_ata}",
            landed.source_token_account
        ));
    }
    match landed.destination_delta {
        Some(delta) if delta == expected.amount_atomic => {}
        Some(delta) => {
            return Err(format!(
                "destination balance rose by {delta}, expected {} — ambiguous, refusing",
                expected.amount_atomic
            ))
        }
        None => {
            return Err(
                "the RPC supplied no destination token-balance meta — cannot confirm the \
                 credited amount; refusing (retry against a full RPC node)"
                    .to_string(),
            )
        }
    }
    if let Some(meta_mint) = landed.destination_meta_mint {
        if meta_mint != expected.mint {
            return Err(format!(
                "destination balance meta names mint {meta_mint}, expected {}",
                expected.mint
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- import --

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportVerdict {
    /// Dry run: every check passed; `--execute` would record and close.
    WouldImport,
    /// Recorded and closed (this run).
    Imported { audit_id: i64 },
    /// Already recorded with this signature — zero writes.
    AlreadyImported,
    /// Not a `finalized` result — left alone (stays `ManualReview`).
    Skipped(String),
    /// A check failed — nothing written for this request.
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    pub request_id: i64,
    pub tx_signature: Option<String>,
    pub amount_display: String,
    pub verdict: ImportVerdict,
}

/// Verified facts of one landed refund, ready to record.
#[derive(Debug, Clone)]
pub struct VerifiedImport {
    pub request_id: i64,
    pub inputs: ManualSolanaRefundInputs,
}

/// Verifies ONE finalized result against the ledger and the cluster —
/// everything except the write. Shared by dry run and execute so the two
/// can never diverge in what they check.
pub async fn verify_result_entry<R: SolanaRpc + SolanaTransactionLookup>(
    rpc: &R,
    ledger: &Ledger,
    results: &RefundResults,
    entry: &RefundResultEntry,
    configured_mint: &Pubkey,
    now: i64,
) -> Result<VerifiedImport, String> {
    let signature_str = entry
        .tx_signature
        .as_deref()
        .ok_or("finalized result has no tx_signature")?;
    let signature: Signature = signature_str
        .parse()
        .map_err(|e| format!("malformed tx_signature: {e}"))?;
    let refund_wallet = Pubkey::from_str(&results.refund_wallet)
        .map_err(|_| "malformed refund_wallet in results".to_string())?;
    if entry.refund_wallet != results.refund_wallet {
        return Err("entry's refund_wallet differs from the file's".to_string());
    }
    let request = ledger
        .manual_solana_refund_eligibility(entry.request_id, now)
        .map_err(|e| match e {
            crate::ledger::LedgerError::ManualReviewNotRecoverable { detail, .. } => detail,
            other => other.to_string(),
        })?;
    let expected = chain_expectation(rpc, &request, configured_mint).await?;
    // The manifest must agree with the fresh expectation — a manifest
    // edited after export is refused even before the chain is asked.
    if entry.recipient != expected.requester.to_string() {
        return Err(format!(
            "manifest recipient {} is not the request's requester {}",
            entry.recipient, expected.requester
        ));
    }
    if entry.amount_atomic != expected.amount_atomic.to_string() {
        return Err(format!(
            "manifest amount {} is not the authoritative amount {}",
            entry.amount_atomic, expected.amount_atomic
        ));
    }
    if entry.mint != expected.mint.to_string() || results.mint != expected.mint.to_string() {
        return Err("manifest mint is not the reserve mint".to_string());
    }
    let tx = rpc
        .get_finalized_transaction(&signature)
        .await
        .map_err(|e| format!("reading transaction {signature}: {e}"))?
        .ok_or_else(|| {
            format!(
                "transaction {signature} is not known at finalized commitment — not proven; \
                 nothing recorded"
            )
        })?;
    let landed = decode_landed_refund(&signature, &tx)?;
    verify_landed_refund(&landed, &expected, &results.batch_id, &refund_wallet)?;
    Ok(VerifiedImport {
        request_id: entry.request_id,
        inputs: ManualSolanaRefundInputs {
            batch_id: results.batch_id.clone(),
            mint: expected.mint.to_bytes(),
            refund_wallet: refund_wallet.to_bytes(),
            recipient: expected.requester.to_bytes(),
            recipient_token_account: expected.recipient_token_account.to_bytes(),
            amount_atomic: expected.amount_atomic,
            amount_canonical_atomic: expected.amount_canonical_atomic,
            tx_signature: signature_str.to_string(),
            slot: landed.slot,
            block_time: landed.block_time,
            submitted_at: entry.submitted_at,
            finalized_at: entry.finalized_at,
        },
    })
}

/// `manual-refund-import`'s body. Dry run unless `execute`; per-request
/// verdicts, never all-or-nothing (one refused request does not block a
/// verified one, and every verdict is printed).
#[allow(clippy::too_many_arguments)]
pub async fn import_results<R: SolanaRpc + SolanaTransactionLookup>(
    rpc: &R,
    ledger: &mut Ledger,
    results: &RefundResults,
    configured_mint: &Pubkey,
    execute: bool,
    note: &str,
    actor: &str,
    now: i64,
) -> Result<Vec<ImportRow>, String> {
    if results.schema != RESULTS_SCHEMA {
        return Err(format!(
            "results schema is {:?}, expected {RESULTS_SCHEMA:?}",
            results.schema
        ));
    }
    if results.network != "solana" {
        return Err(format!(
            "results network is {:?}, expected solana",
            results.network
        ));
    }
    // Duplicate signatures / request ids within the file: refuse those
    // entries outright.
    let mut sig_count: HashMap<&str, usize> = HashMap::new();
    let mut id_count: HashMap<i64, usize> = HashMap::new();
    for e in &results.results {
        if let Some(s) = e.tx_signature.as_deref() {
            *sig_count.entry(s).or_default() += 1;
        }
        *id_count.entry(e.request_id).or_default() += 1;
    }
    let mut rows = Vec::with_capacity(results.results.len());
    for entry in &results.results {
        let row = |verdict| ImportRow {
            request_id: entry.request_id,
            tx_signature: entry.tx_signature.clone(),
            amount_display: entry.amount_display.clone(),
            verdict,
        };
        if entry.status != ResultStatus::Finalized {
            rows.push(row(ImportVerdict::Skipped(format!(
                "status={}{}",
                entry.status.as_str(),
                entry
                    .error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            ))));
            continue;
        }
        if id_count.get(&entry.request_id).copied().unwrap_or(0) > 1 {
            rows.push(row(ImportVerdict::Refused(
                "request id appears more than once in the results file".to_string(),
            )));
            continue;
        }
        if let Some(s) = entry.tx_signature.as_deref() {
            if sig_count.get(s).copied().unwrap_or(0) > 1 {
                rows.push(row(ImportVerdict::Refused(
                    "signature appears more than once in the results file".to_string(),
                )));
                continue;
            }
            // Idempotent re-import: the same signature already recorded
            // for this request.
            match ledger
                .get_manual_solana_refund(entry.request_id)
                .map_err(|e| e.to_string())?
            {
                Some(existing) if existing.tx_signature == s => {
                    rows.push(row(ImportVerdict::AlreadyImported));
                    continue;
                }
                _ => {}
            }
        }
        let verified =
            match verify_result_entry(rpc, ledger, results, entry, configured_mint, now).await {
                Ok(v) => v,
                Err(reason) => {
                    rows.push(row(ImportVerdict::Refused(reason)));
                    continue;
                }
            };
        if !execute {
            rows.push(row(ImportVerdict::WouldImport));
            continue;
        }
        match crate::admin_api::audited_manual_refund_import(
            ledger,
            verified.request_id,
            &verified.inputs,
            note,
            actor,
        ) {
            Ok((crate::ledger::ManualRefundRecordOutcome::Recorded(..), receipt)) => {
                rows.push(row(ImportVerdict::Imported {
                    audit_id: receipt.audit_id,
                }));
            }
            Ok((crate::ledger::ManualRefundRecordOutcome::AlreadyRecorded(_), _)) => {
                rows.push(row(ImportVerdict::AlreadyImported));
            }
            Err(e) => rows.push(row(ImportVerdict::Refused(e.to_string()))),
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
