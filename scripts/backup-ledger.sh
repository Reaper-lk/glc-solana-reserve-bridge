#!/usr/bin/env bash
# Backs up the reserve-bridge ledger SQLite database safely, i.e. via
# sqlite3's own online ".backup" command rather than `cp`, so a backup
# taken while glc-bridge-daemon is running never captures a half-written
# page (docs/16-p0-checkpoint.md P1 item — glc-audit's own doc comment has
# always assumed a backup exists to run against; nothing produced one
# until this script).
#
# Usage:
#   backup-ledger.sh <path-to-ledger.sqlite3> <backup-directory>
#
# Writes <backup-directory>/ledger-<UTC timestamp>.sqlite3 and prints its
# path on success. Exits non-zero (and leaves no partial file behind) on
# any failure.

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <path-to-ledger.sqlite3> <backup-directory>" >&2
    exit 2
fi

db_path="$1"
backup_dir="$2"

if [ ! -f "$db_path" ]; then
    echo "error: ledger database not found: $db_path" >&2
    exit 2
fi
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "error: sqlite3 CLI not found on PATH" >&2
    exit 2
fi

mkdir -p "$backup_dir"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
dest="$backup_dir/ledger-$timestamp.sqlite3"
tmp_dest="$dest.tmp"

# .backup is sqlite3's own safe, consistent, online-backup mechanism (it
# takes SQLite's own read lock and copies page-by-page) — never a plain
# file copy, which could read a page mid-write from a live process.
sqlite3 "$db_path" ".backup '$tmp_dest'"
mv "$tmp_dest" "$dest"

echo "$dest"
