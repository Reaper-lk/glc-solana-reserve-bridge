#!/usr/bin/env bash
# Restores a reserve-bridge ledger from a backup produced by
# backup-ledger.sh (docs/16-p0-checkpoint.md P1 item).
#
# Usage:
#   restore-ledger.sh <backup-file> <destination-path>
#
# Refuses to overwrite an existing destination file — move it aside
# first if you genuinely mean to replace a live database. Verifies the
# backup with `PRAGMA integrity_check` before installing it; a backup
# that fails that check is left in place untouched and this script exits
# non-zero rather than installing something already known to be corrupt.

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <backup-file> <destination-path>" >&2
    exit 2
fi

backup_file="$1"
dest_path="$2"

if [ ! -f "$backup_file" ]; then
    echo "error: backup file not found: $backup_file" >&2
    exit 2
fi
if [ -e "$dest_path" ]; then
    echo "error: destination already exists, refusing to overwrite: $dest_path" >&2
    echo "       move it aside first if you genuinely mean to replace it" >&2
    exit 2
fi
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "error: sqlite3 CLI not found on PATH" >&2
    exit 2
fi

result="$(sqlite3 "$backup_file" "PRAGMA integrity_check;")"
if [ "$result" != "ok" ]; then
    echo "error: backup failed PRAGMA integrity_check, refusing to restore it: $result" >&2
    exit 2
fi

mkdir -p "$(dirname "$dest_path")"
cp "$backup_file" "$dest_path"
echo "restored $backup_file -> $dest_path"
