//! Operator-triggered recovery for a Goldcoin vault UTXO split stuck in
//! the `Signed` state after its broadcast attempt failed to reach a
//! definitive answer — e.g. a transport/decode error contacting Goldcoin
//! RPC. Real production incident this closes: splits #4 and #5 stuck in
//! `Signed` with a valid `signed_tx_hex`, `txid` and `broadcast_at` both
//! `NULL`, and their source UTXOs still `Available` — after "transport
//! error contacting Goldcoin RPC: error decoding response body" aborted
//! the original `split-vault-utxo --execute` run partway through. Every
//! later re-run of that exact command was, until this module, a
//! guaranteed no-op forever: `cmd_split_vault_utxo`'s own idempotency
//! check found the existing `Signed` row and reported "already split,
//! nothing to do" without ever looking at its state.
//!
//! `glc-admin split-vault-utxo --execute` reaches
//! [`recover_stuck_vault_utxo_split`] automatically when it finds an
//! existing `Signed` row for the requested source outpoint — see its
//! call site in `bin/glc-admin.rs`.
//!
//! # Reuses the exact stored `signed_tx_hex` verbatim — never rebuilds, never re-signs
//!
//! Unlike [`crate::goldcoin::payout_recovery`] (which deliberately
//! re-signs, because the rejection it recovers from — a
//! non-canonical-signature policy check — is plausibly baked into the
//! stored signature bytes themselves), this module's failure mode is
//! different in kind: the ORIGINAL broadcast attempt never got a
//! definitive answer from the node at all (a transport/decode failure,
//! not a validation rejection). Nothing about that suggests the signed
//! transaction itself is bad — only that its first delivery attempt
//! didn't complete. Re-signing here would risk producing a different
//! transaction for no benefit (and would needlessly re-contact the
//! signer threshold); this recovery re-submits the IDENTICAL
//! previously-signed bytes, unconditionally.
//!
//! # Never creates a second split for the same source outpoint
//!
//! `vault_utxo_splits(source_txid, source_vout)` has a structural
//! `UNIQUE` index (`ux_vault_utxo_splits_source`, `ledger::schema`); this
//! module only ever reads the existing row
//! ([`Ledger::get_vault_utxo_split`]) and moves it `Signed -> Broadcast`
//! via [`Ledger::record_vault_utxo_split_broadcast`], which is itself
//! idempotent — a repeat call once already `Broadcast` is a safe no-op.
//! Never a raw SQL mutation, never an `INSERT`.
//!
//! # The recovered txid is always independently computed, never trusted from the RPC
//!
//! Same discipline `payout_recovery` and the fresh-build path in
//! `cmd_split_vault_utxo` already follow: the RPC's own self-reported
//! txid string (on `BroadcastOutcome::Accepted`) is never used.
//! [`crate::goldcoin::tx::txid_of_serialized`] computes it directly from
//! the exact bytes this call submitted — the only bytes this service
//! actually controls and can vouch for.

use crate::goldcoin::hex::HexError;
use crate::goldcoin::indexer::GoldcoinRpc;
use crate::goldcoin::rpc::{call_with_retry, BroadcastOutcome, RpcError};
use crate::goldcoin::tx::txid_of_serialized;
use crate::ledger::{Ledger, LedgerError};

#[derive(Debug, thiserror::Error)]
pub enum SplitRecoveryError {
    #[error(
        "no vault UTXO split exists for {}:{vout}",
        crate::goldcoin::hex::encode(txid)
    )]
    NotFound { txid: [u8; 32], vout: u32 },
    /// Only a `Signed` split is ever recovered here — a `Built` split (no
    /// `signed_tx_hex` yet) would need re-signing, which is explicitly
    /// out of scope for this recovery path; a `Broadcast` split is
    /// reported via [`SplitRecoveryOutcome::AlreadyDone`], not this error.
    #[error(
        "split #{0} is in state {1}, not Signed — this recovery path only resumes a broadcast, \
         it never builds or signs"
    )]
    NotRecoverable(i64, String),
    /// Structurally should never happen — `record_vault_utxo_split_signed`
    /// always sets `signed_tx_hex` in the same update that sets
    /// `state = 'Signed'` — checked explicitly anyway rather than
    /// `.unwrap()`, so a future schema/write-path bug fails loudly with a
    /// clear message instead of a panic.
    #[error("invariant violated: split #{0} is Signed but has no signed_tx_hex")]
    MissingSignedHex(i64),
    #[error("signed_tx_hex for split #{0} is not valid hex: {1}")]
    BadHex(i64, HexError),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("goldcoin rpc error: {0}")]
    Rpc(#[from] RpcError),
    #[error(
        "Goldcoin RPC reports missing inputs while recovering split #{0} — the source UTXO is \
         no longer spendable on-chain; needs operator investigation, never silently retried"
    )]
    BroadcastConflict(i64),
}

/// Outcome of a recovery attempt that did not error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitRecoveryOutcome {
    /// The stored `signed_tx_hex` was successfully (re-)submitted this
    /// call, or the node reported it already known/mined.
    Broadcast { split_id: i64, txid: [u8; 32] },
    /// The split had already reached `Broadcast` before this call did
    /// anything — a safe, non-mutating no-op, safe to call again (crash/
    /// restart idempotency).
    AlreadyDone { split_id: i64, state: String },
}

/// Recovers the vault UTXO split for `(source_txid, source_vout)` if, and
/// only if, it is genuinely stuck in `Signed`. Idempotent: safe to call
/// repeatedly, including after it has already succeeded (returns
/// [`SplitRecoveryOutcome::AlreadyDone`] without mutating anything once
/// the split has reached `Broadcast`), and safe across a crash/restart —
/// every fact this function needs is re-read fresh from the ledger on
/// every call, nothing is cached in memory across invocations.
pub async fn recover_stuck_vault_utxo_split<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    source_txid: [u8; 32],
    source_vout: u32,
    now: i64,
) -> Result<SplitRecoveryOutcome, SplitRecoveryError> {
    let split = ledger
        .get_vault_utxo_split(source_txid, source_vout)?
        .ok_or(SplitRecoveryError::NotFound {
            txid: source_txid,
            vout: source_vout,
        })?;
    match split.state.as_str() {
        "Broadcast" => {
            return Ok(SplitRecoveryOutcome::AlreadyDone {
                split_id: split.id,
                state: split.state,
            });
        }
        "Signed" => {}
        other => {
            return Err(SplitRecoveryError::NotRecoverable(
                split.id,
                other.to_string(),
            ));
        }
    }
    let signed_hex = split
        .signed_tx_hex
        .clone()
        .ok_or(SplitRecoveryError::MissingSignedHex(split.id))?;
    let raw = crate::goldcoin::hex::decode_vec(&signed_hex)
        .map_err(|e| SplitRecoveryError::BadHex(split.id, e))?;

    match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(&signed_hex)).await? {
        BroadcastOutcome::Accepted { .. }
        | BroadcastOutcome::AlreadyInChain
        | BroadcastOutcome::AlreadyInMempool => {
            let txid = txid_of_serialized(&raw);
            ledger.record_vault_utxo_split_broadcast(split.id, txid, now)?;
            Ok(SplitRecoveryOutcome::Broadcast {
                split_id: split.id,
                txid,
            })
        }
        BroadcastOutcome::MissingInputs => Err(SplitRecoveryError::BroadcastConflict(split.id)),
    }
}

#[cfg(test)]
mod tests;
