//! The rolling-24-hour WALLET UNIQUENESS rule, for every route.
//!
//! # The rule
//!
//! On every bridge route, BOTH the source wallet and the destination
//! wallet may be used at most once inside a rolling
//! [`Ledger::WALLET_WINDOW_SECS`] (24-hour) window. A new bridge attempt
//! whose source wallet, OR whose destination wallet, already backs a
//! request created inside the window is not admitted into the normal
//! payout flow: a deposit that is already on-chain is recorded and parked
//! in `ManualReview` (refundable, never paid automatically) under
//! [`Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT`] or
//! [`Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT`]; a
//! request that has not been funded yet (`POST /transfers`) is refused
//! before any capacity is reserved. Once 24 hours have passed since the
//! prior qualifying request's `created_at`, the wallet is eligible again
//! — this is a cooldown, not permanent uniqueness.
//!
//! # One mechanism, six routes
//!
//! Before this module the rule existed on the two Goldcoin-bound routes
//! only, as three near-identical queries reading three different
//! spellings of "the wallet": `recipient` for the Goldcoin destination,
//! `requester` for a Solana source, and a join to
//! `robinhood_deposit_observations.depositor` for a Robinhood source.
//! Generalizing it meant giving the source identity one home
//! (`bridge_requests.source_wallet`, schema v28 — see that migration's
//! docs for the per-chain spelling) so that ONE query, parameterized by
//! the wallet's chain and its role, serves all six routes:
//!
//! | role        | column          | rows that consume the window                |
//! |-------------|-----------------|---------------------------------------------|
//! | source      | `source_wallet` | every direction whose SOURCE is that chain  |
//! | destination | `recipient`     | every direction whose DESTINATION is that chain |
//!
//! ([`Ledger::wallet_window_directions_sql_in`] spells the six sets, and
//! `ledger::tests` pins each literal to `Direction::source_chain`/
//! `Direction::destination_chain`.)
//!
//! # Scoped per wallet on its chain, across the routes sharing that chain
//!
//! A window belongs to an ADDRESS on ONE chain. It spans every route on
//! which that chain plays that role — a Goldcoin address that just
//! received a `SolToGlc` payout is busy for `RhnToGlc` too, and a Solana
//! wallet that just funded `SolToGlc` is busy for `SolToRhn` — because
//! the rule is a property of the wallet, not of the counterpart chain,
//! and a per-route spelling would hand a wallet one attempt per route
//! per day rather than one attempt per day. That is exactly the
//! long-standing `DESTINATION_IS_GOLDCOIN_SQL_IN` decision, applied to
//! every chain and both roles. It is strictly a superset of a per-route
//! rule: anything a same-route check would block, this blocks too.
//!
//! What it never does is pool windows ACROSS chains: a Solana pubkey and
//! an EVM address are different kinds of identity held by different key
//! material, this service cannot know whether two of them are one
//! person, and the direction predicate keeps their windows apart by
//! construction — a 20-byte EVM address is only ever compared against
//! Robinhood-scoped rows, never against a 32-byte Solana requester.
//!
//! # Which rows count
//!
//! Every row created inside the window whose `state` is not on
//! [`Ledger::RATE_LIMIT_EXCLUDED_STATES_SQL_IN`] — an EXCLUDE-list of the
//! terminal never-paid states, so `AwaitingDeposit`, `Confirming`,
//! `SourceFinalized`, every payout/settlement-in-progress state,
//! `Settled`, `ManualReview` AND the refund lifecycle all consume the
//! window, and a state added later defaults to counting.
//!
//! # Atomic with admission
//!
//! Every enforcing caller — the four folds, `create_request`, the
//! Goldcoin deposit observation and the resume paths — runs this query
//! inside the SAME `BEGIN IMMEDIATE` write transaction as the row it
//! then inserts or transitions. SQLite's write lock serializes those
//! transactions database-wide, so two attempts sharing a wallet can never
//! both observe "no blocker" and both be admitted: the second always
//! sees the first's committed row. The read-only views the API serves
//! ([`Ledger::wallet_window_retry_after`],
//! [`Ledger::route_wallet_eligibility`]) run the identical query without
//! a write lock and are purely advisory — admission re-checks for itself,
//! so a stale or bypassed pre-check can never weaken the rule.

use rusqlite::Connection;

use super::{Direction, Ledger, LedgerError};
use crate::routes::Chain;

/// Which side of a bridge request a wallet plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WalletRole {
    /// The wallet the source deposit came from
    /// (`bridge_requests.source_wallet`).
    Source,
    /// The wallet the payout goes to (`bridge_requests.recipient`).
    Destination,
}

impl WalletRole {
    pub const ALL: [WalletRole; 2] = [WalletRole::Source, WalletRole::Destination];

    /// Wire/operator spelling of the role.
    pub fn as_str(self) -> &'static str {
        match self {
            WalletRole::Source => "source",
            WalletRole::Destination => "destination",
        }
    }

    /// The `bridge_requests` column that holds a wallet in this role —
    /// hardcoded per role rather than passed in, so no caller can ever
    /// point a window query at the wrong column.
    fn column(self) -> &'static str {
        match self {
            WalletRole::Source => "source_wallet",
            WalletRole::Destination => "recipient",
        }
    }

    /// The reason a park (`manual_review_note`), a `POST /transfers`
    /// refusal and the eligibility API all name when this role's window
    /// is what blocked — read from the [`Ledger`] constants, never
    /// re-spelled.
    pub fn limit_reason(self) -> &'static str {
        match self {
            WalletRole::Source => Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
            WalletRole::Destination => Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT,
        }
    }
}

/// Which rows may block the request being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletWindowScope {
    /// A brand-new request that has no row yet (a fold, or
    /// `create_request`): every row in the window counts.
    NewRequest,
    /// A request that already has its own row — a Goldcoin-sourced
    /// request whose deposit is now being observed, with the wallets the
    /// deposit actually came from: every OTHER row in the window counts.
    /// The request's own row must not block itself, and a later sibling
    /// admitted while this one sat `Expired` (a late deposit) MUST block
    /// it — whichever of the two deposits is observed second is the one
    /// that parks.
    ExcludingRequest(i64),
    /// A `ManualReview` resume candidate: only a row ordered STRICTLY
    /// before it by `(created_at, id)` may block. See
    /// `Ledger::resume_manual_review_inbound`'s comment — without this,
    /// a later-arriving sibling (itself parked because it arrived later)
    /// would shadow-block the rightfully-next candidate and invert
    /// oldest-first draining.
    StrictPredecessorOf { created_at: i64, id: i64 },
}

/// The two windows a NEW request on one route has to clear, as the
/// instants they reopen — `None` for a leg that is not blocked (or, for
/// the source leg, not asked about). Built by
/// [`Ledger::route_wallet_eligibility`]; consumed by the folds, by
/// `create_request` and by the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteWalletEligibility {
    pub source_retry_after: Option<i64>,
    pub destination_retry_after: Option<i64>,
}

impl RouteWalletEligibility {
    /// `true` only when NEITHER window blocks.
    pub fn is_eligible(self) -> bool {
        self.blocker().is_none()
    }

    /// The single limit to report when one or both apply — SOURCE first,
    /// matching the folds' own `manual_review_note` ranking, so the one
    /// reason a UI shows is the one a real deposit would be parked under.
    /// Both limits are enforced independently regardless.
    pub fn blocker(self) -> Option<(WalletRole, i64)> {
        match (self.source_retry_after, self.destination_retry_after) {
            (Some(t), _) => Some((WalletRole::Source, t)),
            (None, Some(t)) => Some((WalletRole::Destination, t)),
            (None, None) => None,
        }
    }

    /// The reason for each blocking leg, source first — `[]` when
    /// eligible.
    pub fn blocked_reasons(self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.source_retry_after.is_some() {
            reasons.push(WalletRole::Source.limit_reason());
        }
        if self.destination_retry_after.is_some() {
            reasons.push(WalletRole::Destination.limit_reason());
        }
        reasons
    }

    /// The `manual_review_note` a fold records when this verdict blocks.
    pub fn manual_review_note(self) -> Option<&'static str> {
        self.blocker().map(|(role, _)| role.limit_reason())
    }
}

impl Ledger {
    /// The rolling window behind the wallet uniqueness rule, on every
    /// route and for both roles: 24 hours, in seconds. ONE constant for
    /// every limiter, deliberately, so the twelve (chain, role) windows
    /// are structurally unable to drift apart on the window itself.
    /// `pub` so the API can report it (`window_seconds`) rather than
    /// hardcoding `86_400`.
    pub const WALLET_WINDOW_SECS: i64 = 86_400;

    /// The rolling window's pre-generalization name, kept for the callers
    /// and documentation that grew up with the Goldcoin-bound rule. The
    /// same value as [`Self::WALLET_WINDOW_SECS`] by definition.
    pub const RECIPIENT_RATE_LIMIT_WINDOW_SECS: i64 = Self::WALLET_WINDOW_SECS;

    /// The SQL `IN` list of directions whose rows can consume a window
    /// for a wallet on `chain` in `role`: every direction whose SOURCE
    /// chain is `chain` for the source role, every direction whose
    /// DESTINATION chain is `chain` for the destination role. Literal
    /// per (chain, role) rather than built at runtime, so each set is a
    /// reviewable constant; `ledger::tests` pins every one of them to
    /// [`Direction::source_chain`]/[`Direction::destination_chain`] over
    /// `Direction::ALL`, so a seventh direction cannot be missed silently.
    ///
    /// The two Goldcoin literals are the pre-existing
    /// [`Direction::SOURCE_IS_GOLDCOIN_SQL_IN`] and
    /// [`Direction::DESTINATION_IS_GOLDCOIN_SQL_IN`], so the generalized
    /// rule is byte-for-byte the old rule on the routes that already had
    /// it.
    pub fn wallet_window_directions_sql_in(chain: Chain, role: WalletRole) -> &'static str {
        match (chain, role) {
            (Chain::Goldcoin, WalletRole::Source) => Direction::SOURCE_IS_GOLDCOIN_SQL_IN,
            (Chain::Goldcoin, WalletRole::Destination) => Direction::DESTINATION_IS_GOLDCOIN_SQL_IN,
            (Chain::Solana, WalletRole::Source) => "('SolToGlc','SolToRhn')",
            (Chain::Solana, WalletRole::Destination) => "('GlcToSol','RhnToSol')",
            (Chain::Robinhood, WalletRole::Source) => "('RhnToGlc','RhnToSol')",
            (Chain::Robinhood, WalletRole::Destination) => "('GlcToRhn','SolToRhn')",
        }
    }

    /// THE window query — the single home of the rule every enforcing
    /// path and every read-only view goes through, so none can drift on
    /// the window, the state exclude-list, the direction scope or the
    /// matching semantics (exact byte equality on the column the role
    /// names — the same bytes the source chain's own record carried, or
    /// a payout would be built from).
    ///
    /// Returns the `created_at` of the NEWEST qualifying row (the
    /// blocker whose window reopens last); `None` means "not blocked".
    /// The three `scope`s differ only in which rows are excluded; see
    /// [`WalletWindowScope`].
    pub(crate) fn wallet_window_blocker_created_at(
        conn: &Connection,
        chain: Chain,
        role: WalletRole,
        wallet: &[u8],
        now: i64,
        scope: WalletWindowScope,
    ) -> Result<Option<i64>, LedgerError> {
        if wallet.is_empty() {
            // An empty identity matches nothing and must never be
            // compared: the column CHECK forbids storing one, so no row
            // could match, but the question itself is a caller bug.
            return Ok(None);
        }
        let window_start = now - Self::WALLET_WINDOW_SECS;
        let directions = Self::wallet_window_directions_sql_in(chain, role);
        let column = role.column();
        let excluded = Self::RATE_LIMIT_EXCLUDED_STATES_SQL_IN;
        let blocker = match scope {
            WalletWindowScope::NewRequest => conn.query_row(
                &format!(
                    "SELECT MAX(created_at) FROM bridge_requests
                     WHERE direction IN {directions} AND {column} = ?1
                       AND created_at > ?2
                       AND state NOT IN {excluded}"
                ),
                rusqlite::params![wallet, window_start],
                |r| r.get(0),
            )?,
            WalletWindowScope::ExcludingRequest(own_id) => conn.query_row(
                &format!(
                    "SELECT MAX(created_at) FROM bridge_requests
                     WHERE direction IN {directions} AND {column} = ?1
                       AND created_at > ?2
                       AND id <> ?3
                       AND state NOT IN {excluded}"
                ),
                rusqlite::params![wallet, window_start, own_id],
                |r| r.get(0),
            )?,
            WalletWindowScope::StrictPredecessorOf { created_at, id } => conn.query_row(
                &format!(
                    "SELECT MAX(created_at) FROM bridge_requests
                     WHERE direction IN {directions} AND {column} = ?1
                       AND created_at > ?2
                       AND (created_at < ?3 OR (created_at = ?3 AND id < ?4))
                       AND state NOT IN {excluded}"
                ),
                rusqlite::params![wallet, window_start, created_at, id],
                |r| r.get(0),
            )?,
        };
        Ok(blocker)
    }

    /// [`Self::wallet_window_blocker_created_at`] as the instant the
    /// window reopens (`created_at + WALLET_WINDOW_SECS`), which is what
    /// every caller actually reports.
    pub(crate) fn wallet_window_retry_after_in(
        conn: &Connection,
        chain: Chain,
        role: WalletRole,
        wallet: &[u8],
        now: i64,
        scope: WalletWindowScope,
    ) -> Result<Option<i64>, LedgerError> {
        Ok(
            Self::wallet_window_blocker_created_at(conn, chain, role, wallet, now, scope)?
                .map(|created_at| created_at + Self::WALLET_WINDOW_SECS),
        )
    }

    /// Read-only answer to "may this wallet, on this chain, be used in
    /// this role for a NEW request right now?" — `Some(retry_after)`
    /// (the unix second the window reopens) when it is inside its
    /// window, `None` when eligible.
    ///
    /// Exactly the check every admission path applies to the next
    /// arriving attempt for these bytes — same query, via
    /// [`Self::wallet_window_blocker_created_at`] — surfaced without any
    /// mutation so the API/UI can warn a user BEFORE they sign a
    /// source-chain transaction that would only get parked. Purely
    /// advisory: admission re-checks at fold/create time, so a stale
    /// answer here can never bypass the limit.
    pub fn wallet_window_retry_after(
        &self,
        chain: Chain,
        role: WalletRole,
        wallet: &[u8],
        now: i64,
    ) -> Result<Option<i64>, LedgerError> {
        Self::wallet_window_retry_after_in(
            &self.conn,
            chain,
            role,
            wallet,
            now,
            WalletWindowScope::NewRequest,
        )
    }

    /// Both windows a NEW request on `direction` must clear, keyed by
    /// the direction's own source and destination chains: `source` is
    /// optional (a `POST /transfers` caller may not declare one, and a
    /// pre-check may ask about the destination alone), `destination`
    /// likewise. A `None` leg is simply not evaluated and reads as
    /// unblocked.
    pub fn route_wallet_eligibility(
        &self,
        direction: Direction,
        source: Option<&[u8]>,
        destination: Option<&[u8]>,
        now: i64,
    ) -> Result<RouteWalletEligibility, LedgerError> {
        Self::route_wallet_eligibility_in(
            &self.conn,
            direction,
            source,
            destination,
            now,
            WalletWindowScope::NewRequest,
        )
    }

    /// [`Self::route_wallet_eligibility`] inside a caller's own
    /// transaction, with an explicit scope — what the enforcing paths
    /// use so the check and the write share one write lock.
    pub(crate) fn route_wallet_eligibility_in(
        conn: &Connection,
        direction: Direction,
        source: Option<&[u8]>,
        destination: Option<&[u8]>,
        now: i64,
        scope: WalletWindowScope,
    ) -> Result<RouteWalletEligibility, LedgerError> {
        let source_retry_after = match source {
            Some(wallet) => Self::wallet_window_retry_after_in(
                conn,
                direction.source_chain(),
                WalletRole::Source,
                wallet,
                now,
                scope,
            )?,
            None => None,
        };
        let destination_retry_after = match destination {
            Some(wallet) => Self::wallet_window_retry_after_in(
                conn,
                direction.destination_chain(),
                WalletRole::Destination,
                wallet,
                now,
                scope,
            )?,
            None => None,
        };
        Ok(RouteWalletEligibility {
            source_retry_after,
            destination_retry_after,
        })
    }

    /// The Goldcoin-destination spelling of [`Self::wallet_window_retry_after`]
    /// — the pre-generalization accessor `GET /recipients/{sol,rhn}-to-glc/
    /// eligibility` and the admin API grew up with, kept as a thin
    /// wrapper over the one query. Route-agnostic across every
    /// inbound-to-Goldcoin route, exactly as before.
    pub fn goldcoin_recipient_rate_limited_until(
        &self,
        recipient: &[u8],
        now: i64,
    ) -> Result<Option<i64>, LedgerError> {
        self.wallet_window_retry_after(Chain::Goldcoin, WalletRole::Destination, recipient, now)
    }

    /// The Solana-source spelling of [`Self::wallet_window_retry_after`],
    /// keyed by the depositor's wallet (`requester`). Spans every
    /// Solana-sourced route.
    pub fn sol_to_glc_source_wallet_rate_limited_until(
        &self,
        requester: &[u8],
        now: i64,
    ) -> Result<Option<i64>, LedgerError> {
        self.wallet_window_retry_after(Chain::Solana, WalletRole::Source, requester, now)
    }

    /// The Robinhood-source spelling of [`Self::wallet_window_retry_after`],
    /// keyed by the custody contract's recorded 20-byte `depositor`.
    /// Spans every Robinhood-sourced route.
    pub fn rhn_to_glc_source_wallet_rate_limited_until(
        &self,
        depositor: &[u8; 20],
        now: i64,
    ) -> Result<Option<i64>, LedgerError> {
        self.wallet_window_retry_after(Chain::Robinhood, WalletRole::Source, depositor, now)
    }
}

/// A wallet's bytes spelled the way `chain` spells an address, for
/// error messages and operator output: the address text for Goldcoin
/// (it is stored as text), base58 for a 32-byte Solana pubkey, `0x` hex
/// for Robinhood. Anything that is not the shape the chain records
/// (a raw prevout script on Goldcoin, an undeliverable payload) is
/// shown as hex, so what an operator reads is always exactly what is
/// stored and never a guess.
pub fn render_wallet(chain: Chain, wallet: &[u8]) -> String {
    match chain {
        Chain::Goldcoin => match std::str::from_utf8(wallet) {
            Ok(text) if text.chars().all(|c| c.is_ascii_alphanumeric()) => text.to_string(),
            _ => crate::goldcoin::hex::encode(wallet),
        },
        Chain::Solana => match <[u8; 32]>::try_from(wallet) {
            Ok(key) => solana_sdk::pubkey::Pubkey::new_from_array(key).to_string(),
            Err(_) => crate::goldcoin::hex::encode(wallet),
        },
        Chain::Robinhood => format!("0x{}", crate::goldcoin::hex::encode(wallet)),
    }
}

#[cfg(test)]
mod tests;
