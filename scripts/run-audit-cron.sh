#!/usr/bin/env bash
# Cron/systemd-timer entry point tying backup-ledger.sh and glc-audit
# together (docs/16-p0-checkpoint.md P1 item — glc-audit's own doc
# comment already says it's "designed for a cron job to page on" via its
# exit codes; nothing actually ran it on a schedule until this script).
#
# Usage:
#   run-audit-cron.sh <path-to-ledger.sqlite3> <backup-directory> [<glc-audit binary path>]
#
# Takes a fresh safe backup, then runs glc-audit against THAT backup, not
# the live database — glc-audit's own docs explain why: the point of an
# offline audit is answering whether a backup is worth restoring from,
# and running it against a backup rather than the live file means a
# corrupt-in-place live database still gets audited via what was
# successfully copied out of it.
#
# Exit code is glc-audit's own: 0 clean, 1 findings, 2 could not run
# (including this script's own setup failures) — wire this script's exit
# code directly into cron's/systemd's own failure notification, do not
# swallow it.

set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: $0 <path-to-ledger.sqlite3> <backup-directory> [<glc-audit binary path>]" >&2
    exit 2
fi

db_path="$1"
backup_dir="$2"
glc_audit_bin="${3:-glc-audit}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

backup_path="$("$script_dir/backup-ledger.sh" "$db_path" "$backup_dir")"
echo "backed up to $backup_path"

exec "$glc_audit_bin" --db "$backup_path"
