//! **Bridge settings** — persisted operator-level switches (schema v33,
//! `bridge_settings`). One key per fact, read on every use, never cached
//! across a tick, so the daemon and `glc-admin` can never disagree about
//! the value.
//!
//! # `auto_resume_manual_review` (default `false`)
//!
//! Whether the daemon's automatic recovery pass
//! (`Orchestrator::tick_auto_resume_utxo_liquidity_backlog`) may resume
//! ANY `ManualReview` row. The 2026-09-13 operator policy: a parked
//! request is FROZEN until an operator says otherwise — liquidity
//! returning, a reserve being replenished, a route reopening, admission
//! reopening or a daemon restart moves nothing. The switch only ever
//! widens what the pass CONSIDERS; it never weakens the allowlist
//! ([`Ledger::is_auto_resumable_manual_review_reason`]) or a single
//! safety check inside the resume, and it never reaches a held row
//! (`operator_hold`, `rapid_burst_hold` — [`BridgeRequest::excluded_from_auto_resume`]),
//! which is an invariant of the candidate filter itself.
//!
//! A missing row is `false`. That is the production default and the
//! value every ledger has the moment v33 lands: nothing is ever
//! "enabled by migration".
//!
//! # `manual_review_retained_cancel_enabled` (default `false`)
//!
//! The feature flag behind a CANCEL that keeps the depositor's principal
//! (`ClosureDisposition::RetainedPerTerms`). The published Terms
//! (2026-09-12) do not authorize that financial outcome, so the flag is
//! seeded from the daemon config's `[manual_review]
//! retained_cancel_enabled` (default `false`) and never settable over the
//! admin API; while it is off the disposition is refused with the reason.
//! See docs/36 §3 for the clause that has to be published first.

use rusqlite::{Connection, OptionalExtension};

use super::{Ledger, LedgerError};

/// The persisted key of the auto-resume switch.
pub const SETTING_AUTO_RESUME_MANUAL_REVIEW: &str = "auto_resume_manual_review";
/// The persisted key of the retained-principal CANCEL feature flag.
pub const SETTING_RETAINED_CANCEL_ENABLED: &str = "manual_review_retained_cancel_enabled";

/// One row of `bridge_settings`, as the admin surfaces show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSetting {
    pub key: String,
    pub value: String,
    pub updated_at: i64,
    pub updated_by: String,
}

impl Ledger {
    fn setting_in(conn: &Connection, key: &str) -> Result<Option<BridgeSetting>, LedgerError> {
        Ok(conn
            .query_row(
                "SELECT key, value, updated_at, updated_by FROM bridge_settings WHERE key = ?1",
                [key],
                |r| {
                    Ok(BridgeSetting {
                        key: r.get(0)?,
                        value: r.get(1)?,
                        updated_at: r.get(2)?,
                        updated_by: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    fn bool_setting_in(conn: &Connection, key: &str) -> Result<bool, LedgerError> {
        Ok(Self::setting_in(conn, key)?.is_some_and(|s| s.value == "true"))
    }

    fn set_bool_setting(
        &mut self,
        key: &str,
        value: bool,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if actor.trim().is_empty() {
            return Err(LedgerError::SettingWithoutActor(key.to_string()));
        }
        self.conn.execute(
            "INSERT INTO bridge_settings (key, value, updated_at, updated_by)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value, updated_at = excluded.updated_at,
                updated_by = excluded.updated_by",
            rusqlite::params![key, if value { "true" } else { "false" }, now, actor],
        )?;
        Ok(())
    }

    /// The auto-resume switch — `false` unless an operator explicitly
    /// set it. Read fresh on every call.
    pub fn manual_review_auto_resume_enabled(&self) -> Result<bool, LedgerError> {
        Self::bool_setting_in(&self.conn, SETTING_AUTO_RESUME_MANUAL_REVIEW)
    }

    /// The switch's row, for the admin surfaces (who set it, when).
    /// `None` = never set = `false`.
    pub fn manual_review_auto_resume_setting(&self) -> Result<Option<BridgeSetting>, LedgerError> {
        Self::setting_in(&self.conn, SETTING_AUTO_RESUME_MANUAL_REVIEW)
    }

    /// Sets the switch. Returns the previous value so the caller's audit
    /// row can carry `old -> new`. Idempotent: setting the current value
    /// again still records who confirmed it.
    pub fn set_manual_review_auto_resume(
        &mut self,
        enabled: bool,
        actor: &str,
        now: i64,
    ) -> Result<bool, LedgerError> {
        let before = self.manual_review_auto_resume_enabled()?;
        self.set_bool_setting(SETTING_AUTO_RESUME_MANUAL_REVIEW, enabled, actor, now)?;
        Ok(before)
    }

    /// The retained-principal CANCEL flag (config-seeded, never API-set).
    pub fn manual_review_retained_cancel_enabled(&self) -> Result<bool, LedgerError> {
        Self::bool_setting_in(&self.conn, SETTING_RETAINED_CANCEL_ENABLED)
    }

    pub(crate) fn manual_review_retained_cancel_enabled_in(
        conn: &Connection,
    ) -> Result<bool, LedgerError> {
        Self::bool_setting_in(conn, SETTING_RETAINED_CANCEL_ENABLED)
    }

    /// Seeds the flag from config at daemon startup (the same posture as
    /// `set_rapid_burst_policy`): the config is the only authority.
    pub fn seed_manual_review_retained_cancel_enabled(
        &mut self,
        enabled: bool,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.set_bool_setting(SETTING_RETAINED_CANCEL_ENABLED, enabled, "config", now)
    }
}
