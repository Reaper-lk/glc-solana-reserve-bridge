//! The Robinhood indexer's internal health state.
//!
//! # Why this exists at all
//!
//! `ops::indexer_status` records the reason in its own module docs: the
//! old bridge shipped a Goldcoin indexer that could halt permanently, and
//! the halt was invisible because it lived in process-local memory that
//! nothing outside the tick loop could read. A process that stays alive
//! for liveness probes looks healthy from every angle an operator
//! monitors, while deposits silently stop being observed.
//!
//! That mistake is not repeated here. Everything the tick loop learns —
//! whether it is even configured, whether it reached the endpoint, what
//! chain id it expected versus got, the head, the finality frontier, the
//! cursor, the resulting lag, when it last succeeded, the last RPC error,
//! and any halt — is published to a shared [`RobinhoodHealth`] the moment
//! it is known.
//!
//! # Why it is a mutex rather than a bag of atomics
//!
//! [`crate::ops::indexer_status::IndexerStatus`] uses atomics because
//! every field it holds is an independent scalar. This one is not: an
//! observed chain id is only meaningful beside the expected one, and a
//! cursor beside the head it was measured against. Publishing those
//! through separate atomics would let a reader interleave a fresh head
//! with a stale cursor and compute a lag that never existed. One lock
//! around one struct makes every snapshot internally consistent, which is
//! what an operator is actually reading.
//!
//! The lock is held for the duration of a field copy and nothing else —
//! never across an await, never across an RPC call.
//!
//! # Nothing published here carries the endpoint's identity
//!
//! Every string that enters this state passes through
//! [`crate::robinhood::redact::Redactor`] on the way in, at
//! [`RobinhoodHealth::record_error`] and [`RobinhoodHealth::set_halt`].
//! That is not belt-and-braces around careful call sites; it is the only
//! thing standing between an unauthenticated `/health` port and a live
//! RPC credential.
//!
//! The concrete leak it closes: `rpc::EvmRpcError::Transport` is built
//! from `reqwest::Error::to_string()`, whose `Display` embeds the request
//! URL — `user:pass@` and all. Publishing that verbatim would have put an
//! RPC provider's API key on an endpoint whose module docs state plainly
//! that it has no authentication. See [`crate::robinhood::redact`] for
//! why the filter sits here rather than at the call sites that build
//! those strings.
//!
//! What survives redaction is the part an operator actually needs: a
//! typed [`RobinhoodRpcErrorClass`] saying WHICH kind of failure this was,
//! and the message's own prose with only endpoint identity removed.
//!
//! # It reports; it does not decide
//!
//! Nothing here gates anything. The halt that actually stops the indexer
//! is the PERSISTED one in `robinhood_indexer_state` (see
//! `crate::ledger::RobinhoodHaltReason`), which survives a restart;
//! [`RobinhoodHealthSnapshot::halt`] is that fact mirrored for reading.
//! An in-memory flag as the source of truth would let a process bounce
//! look like a fix.

use std::sync::{Arc, Mutex};

use crate::evm::EvmChainId;
use crate::ledger::{RobinhoodHalt, RobinhoodObservationSummary};
use crate::robinhood::redact::Redactor;

/// Which kind of failure the last failed tick hit.
///
/// Carried alongside the redacted message because redaction necessarily
/// costs some detail, and "what sort of problem is this" is the part an
/// operator triages on. A class is derived from the error's own TYPE, not
/// by matching on its text, so it is unaffected by anything a node or a
/// dependency writes into a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobinhoodRpcErrorClass {
    /// The endpoint could not be reached at all: connection refused, DNS
    /// failure, TLS failure, timeout. The class whose underlying message
    /// carries the URL, and therefore the reason this enum exists.
    Transport,
    /// The endpoint answered with a JSON-RPC error object. It was
    /// reached; it declined or failed.
    RpcMethod,
    /// The endpoint answered with something that is not a well-formed
    /// response to what was asked.
    MalformedResponse,
    /// A log carrying the `DepositCreated` topic could not be decoded.
    Decode,
    /// The node contradicted itself or the chain — a missing block, a log
    /// whose block hash does not match the live block, an event from the
    /// wrong contract.
    ChainDisagreement,
    /// A local ledger failure during a tick. Not the endpoint's fault at
    /// all, and worth distinguishing so an operator does not go looking
    /// at the network.
    Ledger,
}

impl RobinhoodRpcErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            RobinhoodRpcErrorClass::Transport => "transport",
            RobinhoodRpcErrorClass::RpcMethod => "rpc_method",
            RobinhoodRpcErrorClass::MalformedResponse => "malformed_response",
            RobinhoodRpcErrorClass::Decode => "decode",
            RobinhoodRpcErrorClass::ChainDisagreement => "chain_disagreement",
            RobinhoodRpcErrorClass::Ledger => "ledger",
        }
    }

    /// Whether a failure of this class means the endpoint was actually
    /// reached. Only [`RobinhoodRpcErrorClass::Transport`] means it was
    /// not: a node that answered with a definitive error was reached, and
    /// reporting that as disconnected would point an operator at the
    /// network when the problem is the answer.
    pub fn reached_endpoint(self) -> bool {
        !matches!(self, RobinhoodRpcErrorClass::Transport)
    }
}

/// One internally consistent reading of the indexer's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RobinhoodHealthSnapshot {
    /// Whether a `[robinhood.indexer]` section was present at startup.
    /// `false` means no client was built and no socket was ever opened —
    /// see [`RobinhoodHealth::unconfigured`].
    pub configured: bool,
    /// Whether the LAST attempted tick reached the endpoint. Not a
    /// long-lived connection state — this client is stateless HTTP — but
    /// the honest question an operator is asking.
    pub connected: bool,
    /// The chain id this deployment requires.
    pub expected_chain_id: Option<u64>,
    /// The chain id the endpoint last reported. A disagreement with
    /// `expected_chain_id` is a halt, not a warning.
    pub observed_chain_id: Option<u64>,
    /// The endpoint's `eth_blockNumber` at the last successful read.
    pub head_block: Option<u64>,
    /// The highest block whose contents this service would treat as
    /// irreversible at that head: `head - confirmation_depth + 1`.
    /// `None` when the chain is not yet that tall.
    pub finalized_block: Option<u64>,
    /// The durable scan cursor — the highest anchored block. `None` means
    /// nothing has been scanned yet and the configured start block is
    /// what the next tick will use.
    pub cursor_block: Option<u64>,
    /// `head - cursor`, in blocks. The number an alert threshold would
    /// actually be written against.
    pub lag_blocks: Option<u64>,
    /// Unix seconds of the last tick that completed without erroring.
    /// Seeded from process start, so the first scrape does not read as
    /// decades of silence — the same choice `IndexerStatus::new` makes.
    pub last_success_unix: Option<i64>,
    /// The last RPC-layer failure, and when. Kept after a later success
    /// rather than cleared, so a flapping endpoint is still visible; the
    /// pairing with `last_success_unix` is what says whether it is
    /// current.
    ///
    /// **Redacted.** This string has passed through
    /// [`crate::robinhood::redact::Redactor`] and cannot contain the RPC
    /// URL, its host, or any credential embedded in it — see this
    /// module's docs. Do not populate it by any route other than
    /// [`RobinhoodHealth::record_error`].
    pub last_rpc_error: Option<String>,
    /// What KIND of failure it was, derived from the error's type rather
    /// than its text. Survives redaction intact.
    pub last_rpc_error_class: Option<RobinhoodRpcErrorClass>,
    pub last_rpc_error_unix: Option<i64>,
    /// The persisted halt, if any. Robinhood-local: it stops this indexer
    /// and pauses no reserve.
    pub halt: Option<RobinhoodHalt>,
    /// How many reorgs this process has reconciled, and the deepest of
    /// them — the warning an operator wants BEFORE a reorg reaches
    /// finality depth, since that is the failure.
    pub reorgs_reconciled: u64,
    pub deepest_reorg_blocks: u64,
    /// Observation counts straight from the ledger.
    pub observations: RobinhoodObservationSummary,
}

/// The shared, mutable state behind [`RobinhoodHealthSnapshot`]. Cloneable
/// as an `Arc` so the tick loop and any reader hold the same one.
#[derive(Debug)]
pub struct RobinhoodHealth {
    state: Mutex<RobinhoodHealthSnapshot>,
    /// Applied to every string on its way INTO `state`. Held here rather
    /// than passed per call so there is no way to record an error without
    /// it — see this module's docs.
    ///
    /// Outside the mutex on purpose: it is immutable for the lifetime of
    /// the process, and redacting inside the lock would hold it across a
    /// string allocation for no reason.
    redactor: Redactor,
}

impl RobinhoodHealth {
    /// A configured indexer's initial state: known expected chain id,
    /// nothing observed yet.
    ///
    /// `rpc_url` is taken so the endpoint's own literals can be stripped
    /// from anything published later. It is used ONLY to build the
    /// [`Redactor`] and is never stored as a snapshot field, so there is
    /// no reader of this type that can get it back out.
    pub fn new(
        expected_chain_id: EvmChainId,
        rpc_url: &str,
        started_at: i64,
    ) -> Arc<RobinhoodHealth> {
        Arc::new(RobinhoodHealth {
            state: Mutex::new(RobinhoodHealthSnapshot {
                configured: true,
                expected_chain_id: Some(expected_chain_id.get()),
                last_success_unix: Some(started_at),
                ..RobinhoodHealthSnapshot::default()
            }),
            redactor: Redactor::for_endpoint(rpc_url),
        })
    }

    /// The state of a deployment with no `[robinhood.indexer]` section:
    /// `configured: false` and every other field empty, forever.
    ///
    /// Exists so a reader gets the same shape either way and can say
    /// "not configured" rather than having to distinguish an absent
    /// health object from an unhealthy one. Nothing ever mutates it,
    /// because with no config there is no tick loop to do so.
    pub fn unconfigured() -> Arc<RobinhoodHealth> {
        Arc::new(RobinhoodHealth {
            state: Mutex::new(RobinhoodHealthSnapshot::default()),
            // No configured endpoint means no endpoint literals to
            // strip — but passes 1 and 3 still run, because "this
            // deployment has no Robinhood endpoint" does not mean nothing
            // can hand it a string containing somebody else's.
            redactor: Redactor::none(),
        })
    }

    /// An internally consistent copy of every field.
    pub fn snapshot(&self) -> RobinhoodHealthSnapshot {
        self.with(|s| s.clone())
    }

    /// Records a completed, error-free tick and everything it learned.
    #[allow(clippy::too_many_arguments)]
    pub fn record_tick(
        &self,
        observed_chain_id: EvmChainId,
        head_block: u64,
        finalized_block: Option<u64>,
        cursor_block: Option<u64>,
        observations: RobinhoodObservationSummary,
        now: i64,
    ) {
        self.with(|s| {
            s.connected = true;
            s.observed_chain_id = Some(observed_chain_id.get());
            s.head_block = Some(head_block);
            s.finalized_block = finalized_block;
            s.cursor_block = cursor_block;
            // Saturating: a cursor above the head is not a negative lag,
            // it is a chain that has gone backwards, and the reorg path —
            // not an arithmetic underflow here — is what deals with it.
            s.lag_blocks = cursor_block.map(|cursor| head_block.saturating_sub(cursor));
            s.observations = observations;
            s.last_success_unix = Some(now);
        });
    }

    /// Records a failed tick, REDACTED.
    ///
    /// `connected` drops only for a transport-level failure: a node that
    /// answered with a definitive error was reached, and reporting it as
    /// disconnected would point an operator at the network when the
    /// problem is the answer. That follows from `class` rather than being
    /// a separate parameter a caller could get wrong.
    ///
    /// The message is redacted here, on the way in, so the published
    /// state never holds an unredacted string even transiently — see this
    /// module's docs and [`crate::robinhood::redact`]. There is
    /// deliberately no unredacted variant of this method.
    pub fn record_error(&self, class: RobinhoodRpcErrorClass, error: &str, now: i64) {
        let redacted = self.redactor.apply(error);
        self.with(|s| {
            s.connected = class.reached_endpoint();
            s.last_rpc_error = Some(redacted);
            s.last_rpc_error_class = Some(class);
            s.last_rpc_error_unix = Some(now);
        });
    }

    /// Mirrors the persisted halt (or its absence) into the snapshot,
    /// with its detail redacted.
    ///
    /// No halt detail this service constructs contains an endpoint URL
    /// today. It is redacted anyway, because "no current call site does
    /// X" is not a property that survives the next call site, and a halt
    /// detail reaches exactly the same unauthenticated surface the RPC
    /// error does.
    pub fn set_halt(&self, halt: Option<RobinhoodHalt>) {
        let halt = halt.map(|h| RobinhoodHalt {
            detail: self.redactor.apply(&h.detail),
            ..h
        });
        self.with(|s| s.halt = halt);
    }

    /// Records a reconciled reorg. Keeps the DEEPEST seen, not the latest:
    /// a 40-block reorg an hour ago is what an operator needs to know
    /// about, and a later 1-block reorg must not erase it — the same rule
    /// `IndexerStatus::record_reorg` follows.
    pub fn record_reorg(&self, depth_blocks: u64) {
        self.with(|s| {
            s.reorgs_reconciled = s.reorgs_reconciled.saturating_add(1);
            s.deepest_reorg_blocks = s.deepest_reorg_blocks.max(depth_blocks);
        });
    }

    /// Runs `f` under the lock.
    ///
    /// A poisoned lock is recovered from rather than propagated: this
    /// state is a report, and every writer below is a short field copy
    /// that cannot leave a half-updated struct behind. Turning an
    /// unrelated panic elsewhere into a permanently unreadable health
    /// surface would be the opposite of what this module is for.
    fn with<T>(&self, f: impl FnOnce(&mut RobinhoodHealthSnapshot) -> T) -> T {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}

#[cfg(test)]
mod tests;
