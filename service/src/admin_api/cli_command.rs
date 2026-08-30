//! Server-side generation of the exact `glc-admin` command an authorized
//! operator should run over SSH for the on-chain admin actions this API
//! deliberately cannot execute (the admin keypair is CLI-only — see the
//! module docs of [`crate::admin_api`]).
//!
//! Why server-side rather than in the admin UI: the 6-decimal
//! mint-atomic / 8-decimal Goldcoin-atomic conversions and the live
//! "current value" reads already exist in this crate, in the exact code
//! the daemon itself trusts (`amount_conversion`, `solana::accounts`) —
//! duplicating that arithmetic in TypeScript is precisely the
//! unit-confusion bug class this bridge's typed-unit discipline exists
//! to prevent, and an old→new preview is only trustworthy if it decodes
//! the same account bytes the daemon decodes.
//!
//! Everything here is a PURE function over already-fetched state: no RPC,
//! no ledger, no execution. The output is a string for a human to review
//! and run; nothing in this crate ever executes it. RPC URL and keypair
//! path are deliberately placeholders — the RPC URL this daemon uses may
//! embed provider credentials, and the keypair path exists only on the
//! operator's own machine.

use serde::{Deserialize, Serialize};

use super::OnchainView;

/// Placeholder the operator substitutes with their own RPC endpoint.
pub const RPC_URL_PLACEHOLDER: &str = "<RPC_URL>";
/// Placeholder for the operator-held admin keypair path — this service
/// never knows a real value for it, by design.
pub const KEYPAIR_PLACEHOLDER: &str = "<ADMIN_KEYPAIR_PATH>";
/// Placeholder for the mandatory operator note.
pub const NOTE_PLACEHOLDER: &str = "<NOTE>";

/// Every action here requires the on-chain admin keypair, which stays on
/// the operator's machine — the UI renders this label on each of them.
pub const CLI_APPROVAL_REQUIRED: &str = "CLI approval required";

const MINT_DECIMALS: u8 = 6;

#[derive(Debug, Deserialize)]
pub struct CliCommandInput {
    /// `onchain-pause` | `onchain-unpause` | `set-limit` |
    /// `reset-rolling-window` — exactly the `glc-admin` subcommand names.
    pub action: String,
    /// For `onchain-pause`/`onchain-unpause`: `global`|`release`|`deposit`.
    pub scope: Option<String>,
    /// For `set-limit`: `min-transfer`|`per-transfer`|`protected-minimum`|
    /// `rolling-volume`.
    pub field: Option<String>,
    /// For `set-limit`: the new value as a decimal GLC string (e.g.
    /// `"20000"` or `"20000.5"`), converted server-side to 6-decimal
    /// mint-atomic units.
    pub value_glc: Option<String>,
    /// For `reset-rolling-window`: `glc-to-sol`|`sol-to-glc`.
    pub direction: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ValueDisplay {
    /// 6-decimal mint-atomic units — the unit `set_limit` takes on-chain.
    pub atomic: u64,
    /// The same value in whole GLC, fixed-point decimal string.
    pub display_glc: String,
}

#[derive(Debug, Serialize)]
pub struct CliCommandView {
    /// The full command line for the operator to review and run. Never
    /// executed by anything in this service.
    pub command: String,
    pub old_value: Option<ValueDisplay>,
    pub new_value: Option<ValueDisplay>,
    /// Human summary when old/new aren't a single number (pauses, window
    /// resets).
    pub summary: String,
    pub unit: &'static str,
    pub label: &'static str,
}

/// Formats `atomic` with `decimals` fractional digits as a fixed-point
/// decimal string — pure integer arithmetic, trailing fractional zeros
/// trimmed (but never the integer part).
pub fn format_atomic_as_decimal_string(atomic: u64, decimals: u8) -> String {
    let scale = 10u64.pow(u32::from(decimals));
    let whole = atomic / scale;
    let frac = atomic % scale;
    if frac == 0 {
        return whole.to_string();
    }
    let frac_str = format!("{frac:0width$}", width = decimals as usize);
    let trimmed = frac_str.trim_end_matches('0');
    format!("{whole}.{trimmed}")
}

/// Parses a decimal GLC string into 6-decimal mint-atomic units — pure
/// integer/string arithmetic, no floating point (a float parse of
/// `"20000.000001"` is exactly the rounding bug this exists to avoid).
/// Rejects: empty, sign characters, more than 6 fractional digits,
/// non-digits, and values that overflow u64.
pub fn parse_glc_to_mint_atomic(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("value_glc must not be empty".to_string());
    }
    let mut parts = value.splitn(2, '.');
    let whole_str = parts.next().unwrap_or("");
    let frac_str = parts.next().unwrap_or("");
    if whole_str.is_empty() && frac_str.is_empty() {
        return Err("value_glc must be a decimal number".to_string());
    }
    if !whole_str.chars().all(|c| c.is_ascii_digit())
        || !frac_str.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "value_glc must be an unsigned decimal number, got {value:?}"
        ));
    }
    if frac_str.len() > MINT_DECIMALS as usize {
        return Err(format!(
            "value_glc has more than {MINT_DECIMALS} fractional digits — the Solana mint cannot represent it"
        ));
    }
    let scale = 10u64.pow(u32::from(MINT_DECIMALS));
    let whole: u64 = if whole_str.is_empty() {
        0
    } else {
        whole_str
            .parse()
            .map_err(|_| format!("value_glc integer part out of range: {whole_str:?}"))?
    };
    let mut frac: u64 = if frac_str.is_empty() {
        0
    } else {
        frac_str
            .parse()
            .map_err(|_| format!("value_glc fractional part out of range: {frac_str:?}"))?
    };
    frac *= 10u64.pow(u32::from(MINT_DECIMALS) - frac_str.len() as u32);
    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(|| format!("value_glc out of range: {value:?}"))
}

fn value_display(atomic: u64) -> ValueDisplay {
    ValueDisplay {
        atomic,
        display_glc: format_atomic_as_decimal_string(atomic, MINT_DECIMALS),
    }
}

/// The `glc-admin` subcommand names this module can emit — kept in one
/// place so the drift-guard test below can assert every one of them
/// exists in `glc-admin`'s real dispatch table.
pub const GENERATED_SUBCOMMANDS: [&str; 4] = [
    "onchain-pause",
    "onchain-unpause",
    "set-limit",
    "reset-rolling-window",
];

pub fn generate(input: &CliCommandInput, onchain: &OnchainView) -> Result<CliCommandView, String> {
    match input.action.as_str() {
        "onchain-pause" | "onchain-unpause" => {
            let pausing = input.action == "onchain-pause";
            let scope = input
                .scope
                .as_deref()
                .ok_or_else(|| "scope is required for onchain-pause/onchain-unpause".to_string())?;
            let current = match scope {
                "global" => onchain.paused,
                "release" => onchain.release_paused,
                "deposit" => onchain.deposit_paused,
                other => {
                    return Err(format!(
                        "unknown scope {other:?} (expected global|release|deposit)"
                    ))
                }
            };
            Ok(CliCommandView {
                command: format!(
                    "glc-admin {} --rpc-url {RPC_URL_PLACEHOLDER} --keypair {KEYPAIR_PLACEHOLDER} --scope {scope} --note '{NOTE_PLACEHOLDER}'",
                    input.action
                ),
                old_value: None,
                new_value: None,
                summary: format!("{scope} pause: currently {current}, would become {pausing}"),
                unit: "flag",
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        "set-limit" => {
            let field = input
                .field
                .as_deref()
                .ok_or_else(|| "field is required for set-limit".to_string())?;
            let old_atomic = match field {
                "min-transfer" => onchain.min_transfer_amount,
                "per-transfer" => onchain.per_transfer_limit,
                "protected-minimum" => onchain.protected_minimum,
                "rolling-volume" => onchain.rolling_volume_limit,
                other => {
                    return Err(format!(
                        "unknown field {other:?} (expected min-transfer|per-transfer|protected-minimum|rolling-volume)"
                    ))
                }
            };
            let value_glc = input
                .value_glc
                .as_deref()
                .ok_or_else(|| "value_glc is required for set-limit".to_string())?;
            let new_atomic = parse_glc_to_mint_atomic(value_glc)?;
            // Mirror the two on-chain validations (`set_limit` rejects a
            // zero per-transfer or rolling-volume limit) so the operator
            // is told before ever building the transaction.
            if new_atomic == 0 && matches!(field, "per-transfer" | "rolling-volume") {
                return Err(format!("{field} cannot be set to 0 (on-chain ZeroAmount check)"));
            }
            let old = value_display(old_atomic);
            let new = value_display(new_atomic);
            let summary = format!(
                "{field}: {} GLC ({} atomic) -> {} GLC ({} atomic)",
                old.display_glc, old.atomic, new.display_glc, new.atomic
            );
            Ok(CliCommandView {
                command: format!(
                    "glc-admin set-limit --rpc-url {RPC_URL_PLACEHOLDER} --keypair {KEYPAIR_PLACEHOLDER} --field {field} --value {new_atomic} --note '{NOTE_PLACEHOLDER}'"
                ),
                old_value: Some(old),
                new_value: Some(new),
                summary,
                unit: "6-decimal Solana mint atomic units",
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        "reset-rolling-window" => {
            let direction = input
                .direction
                .as_deref()
                .ok_or_else(|| "direction is required for reset-rolling-window".to_string())?;
            if !matches!(direction, "glc-to-sol" | "sol-to-glc") {
                return Err(format!(
                    "unknown direction {direction:?} (expected glc-to-sol|sol-to-glc)"
                ));
            }
            let window = onchain
                .rolling_windows
                .iter()
                .find(|w| w.window == direction)
                .ok_or_else(|| "rolling window state unavailable".to_string())?;
            Ok(CliCommandView {
                command: format!(
                    "glc-admin reset-rolling-window --rpc-url {RPC_URL_PLACEHOLDER} --keypair {KEYPAIR_PLACEHOLDER} --direction {direction} --note '{NOTE_PLACEHOLDER}'"
                ),
                old_value: Some(value_display(window.remaining)),
                new_value: Some(value_display(onchain.rolling_volume_limit)),
                summary: format!(
                    "{direction} window: {} GLC remaining of {} GLC -> reset to full capacity",
                    format_atomic_as_decimal_string(window.remaining, MINT_DECIMALS),
                    format_atomic_as_decimal_string(onchain.rolling_volume_limit, MINT_DECIMALS),
                ),
                unit: "6-decimal Solana mint atomic units",
                label: CLI_APPROVAL_REQUIRED,
            })
        }
        other => Err(format!(
            "unknown action {other:?} (expected onchain-pause|onchain-unpause|set-limit|reset-rolling-window)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_api::RollingWindowView;

    fn onchain() -> OnchainView {
        OnchainView {
            paused: false,
            release_paused: true,
            deposit_paused: false,
            min_transfer_amount: 100_000_000,
            per_transfer_limit: 10_000_000_000,
            protected_minimum: 20_000_000_000,
            rolling_volume_limit: 100_000_000_000,
            rolling_window_seconds: 86_400,
            obligation_count: 7,
            rolling_windows: vec![
                RollingWindowView {
                    window: "glc-to-sol".to_string(),
                    window_start: 0,
                    window_total: 25_000_000_000,
                    remaining: 75_000_000_000,
                },
                RollingWindowView {
                    window: "sol-to-glc".to_string(),
                    window_start: 0,
                    window_total: 0,
                    remaining: 100_000_000_000,
                },
            ],
        }
    }

    #[test]
    fn twenty_thousand_glc_converts_to_the_exact_6dp_atomic_value() {
        assert_eq!(parse_glc_to_mint_atomic("20000"), Ok(20_000_000_000));
        assert_eq!(parse_glc_to_mint_atomic("20000.000000"), Ok(20_000_000_000));
        assert_eq!(parse_glc_to_mint_atomic("0.000001"), Ok(1));
        assert_eq!(parse_glc_to_mint_atomic("100"), Ok(100_000_000));
        assert_eq!(parse_glc_to_mint_atomic("20000.5"), Ok(20_000_500_000));
    }

    #[test]
    fn conversion_fails_closed_on_malformed_input() {
        assert!(parse_glc_to_mint_atomic("").is_err());
        assert!(parse_glc_to_mint_atomic("-5").is_err());
        assert!(parse_glc_to_mint_atomic("+5").is_err());
        assert!(parse_glc_to_mint_atomic("1e6").is_err());
        assert!(parse_glc_to_mint_atomic("0.0000001").is_err(), "7 dp");
        assert!(parse_glc_to_mint_atomic(".").is_err());
        assert!(
            parse_glc_to_mint_atomic("18446744073709551616").is_err(),
            "overflow"
        );
        assert!(parse_glc_to_mint_atomic("20 000").is_err());
    }

    #[test]
    fn formatting_round_trips_and_never_uses_floats() {
        assert_eq!(format_atomic_as_decimal_string(20_000_000_000, 6), "20000");
        assert_eq!(
            format_atomic_as_decimal_string(20_000_500_000, 6),
            "20000.5"
        );
        assert_eq!(format_atomic_as_decimal_string(1, 6), "0.000001");
        assert_eq!(format_atomic_as_decimal_string(0, 6), "0");
        assert_eq!(format_atomic_as_decimal_string(300, 2), "3");
    }

    #[test]
    fn set_limit_command_carries_the_atomic_value_and_old_new_preview() {
        let view = generate(
            &CliCommandInput {
                action: "set-limit".to_string(),
                scope: None,
                field: Some("per-transfer".to_string()),
                value_glc: Some("20000".to_string()),
                direction: None,
            },
            &onchain(),
        )
        .unwrap();
        assert_eq!(
            view.command,
            "glc-admin set-limit --rpc-url <RPC_URL> --keypair <ADMIN_KEYPAIR_PATH> --field per-transfer --value 20000000000 --note '<NOTE>'"
        );
        assert_eq!(
            view.old_value,
            Some(ValueDisplay {
                atomic: 10_000_000_000,
                display_glc: "10000".to_string()
            })
        );
        assert_eq!(
            view.new_value,
            Some(ValueDisplay {
                atomic: 20_000_000_000,
                display_glc: "20000".to_string()
            })
        );
        assert_eq!(view.label, CLI_APPROVAL_REQUIRED);
    }

    #[test]
    fn set_limit_mirrors_the_onchain_zero_checks() {
        for field in ["per-transfer", "rolling-volume"] {
            let err = generate(
                &CliCommandInput {
                    action: "set-limit".to_string(),
                    scope: None,
                    field: Some(field.to_string()),
                    value_glc: Some("0".to_string()),
                    direction: None,
                },
                &onchain(),
            )
            .unwrap_err();
            assert!(err.contains("ZeroAmount"), "{err}");
        }
        // min-transfer and protected-minimum have no such on-chain check.
        assert!(generate(
            &CliCommandInput {
                action: "set-limit".to_string(),
                scope: None,
                field: Some("min-transfer".to_string()),
                value_glc: Some("0".to_string()),
                direction: None,
            },
            &onchain(),
        )
        .is_ok());
    }

    #[test]
    fn pause_command_reports_current_and_target_state() {
        let view = generate(
            &CliCommandInput {
                action: "onchain-unpause".to_string(),
                scope: Some("release".to_string()),
                field: None,
                value_glc: None,
                direction: None,
            },
            &onchain(),
        )
        .unwrap();
        assert!(view.command.starts_with("glc-admin onchain-unpause "));
        assert!(view.command.contains("--scope release"));
        assert_eq!(
            view.summary,
            "release pause: currently true, would become false"
        );
    }

    #[test]
    fn reset_rolling_window_previews_remaining_vs_full_capacity() {
        let view = generate(
            &CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("glc-to-sol".to_string()),
            },
            &onchain(),
        )
        .unwrap();
        assert!(view.command.contains("--direction glc-to-sol"));
        assert_eq!(view.old_value.unwrap().atomic, 75_000_000_000);
        assert_eq!(view.new_value.unwrap().atomic, 100_000_000_000);
    }

    #[test]
    fn unknown_actions_scopes_fields_and_directions_are_rejected() {
        let base = onchain();
        assert!(generate(
            &CliCommandInput {
                action: "set-paused".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: None
            },
            &base
        )
        .is_err());
        assert!(generate(
            &CliCommandInput {
                action: "onchain-pause".to_string(),
                scope: Some("everything".to_string()),
                field: None,
                value_glc: None,
                direction: None
            },
            &base
        )
        .is_err());
        assert!(generate(
            &CliCommandInput {
                action: "reset-rolling-window".to_string(),
                scope: None,
                field: None,
                value_glc: None,
                direction: Some("both".to_string())
            },
            &base
        )
        .is_err());
    }

    /// Drift guard, mirroring `service/tests/runbook_commands.rs`'s
    /// discipline: every subcommand this module can emit must exist as a
    /// literal match arm in `glc-admin`'s dispatch table, so a renamed
    /// CLI command cannot leave this generator emitting a command that no
    /// longer runs.
    #[test]
    fn every_generated_subcommand_exists_in_glc_admins_dispatch_table() {
        let glc_admin_src = include_str!("../bin/glc-admin.rs");
        for subcommand in GENERATED_SUBCOMMANDS {
            let needle = format!("\"{subcommand}\" =>");
            assert!(
                glc_admin_src.contains(&needle),
                "glc-admin.rs has no dispatch arm for {subcommand:?}"
            );
        }
    }
}
