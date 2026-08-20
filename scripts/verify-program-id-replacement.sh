#!/usr/bin/env bash
# Read-only verification for the "future production program-id
# replacement" workflow (docs/22-production-readiness-review.md P0-6).
#
# Run this AFTER manually replacing the program id throughout the
# codebase (declare_id!, glc-reserve-bridge-shared::PROGRAM_ID_BYTES,
# Anchor.toml, and the pin-test literals — see that workflow for the
# full checklist) and BEFORE deploying. It does two independent things:
#
#   1. Fails closed if either permanently retired program id
#      (BnCFcMaZ..., the original scaffold/dev id that was never
#      deployed, or 7h2zSJuq..., the mainnet program that WAS deployed
#      and has since been permanently closed with its rent reclaimed)
#      appears anywhere in an operational `.rs`/`.toml` file outside its
#      one legitimate, permanent home — the RETIRED_PROGRAM_IDS denylist
#      in service/src/bin/glc-mainnet-bootstrap.rs and its own pin test
#      in service/src/solana/accounts.rs. Either id appearing anywhere
#      else in compiled code would mean the replacement was incomplete.
#      (Documentation/.md files are deliberately NOT covered by this
#      hard check — docs/13 and docs/22 both intentionally retain
#      historical references to these ids as an accurate incident
#      record, which a human, not a grep, should judge.)
#   2. Fails closed if declare_id! (programs/glc-reserve-bridge/src/
#      lib.rs) and Anchor.toml's [programs.localnet] entry disagree with
#      each other — the two places that must each carry their own
#      literal (Anchor's declare_id!/Anchor.toml tooling can't reference
#      a shared Rust constant) and so can silently drift apart from one
#      another even though both individually still build.
#
# This script does NOT check glc-reserve-bridge-shared::PROGRAM_ID_BYTES
# against declare_id! — that's `cargo test`'s job
# (program_id_tests::program_id_matches_shared_source_of_truth in
# programs/glc-reserve-bridge/src/lib.rs), already exercised by the
# `cargo test`/`anchor build` step this script tells you to run next; a
# bash re-implementation of base58 decoding here would just be a second,
# less-trustworthy copy of that same check.
#
# Usage:
#   scripts/verify-program-id-replacement.sh
#
# Exits non-zero and prints exactly what's wrong on any failure; prints
# a short pass summary and exits 0 if everything checked out.

set -euo pipefail

repo_root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$repo_root"

# The two ids that must never appear in operational code again.
retired_ids=(
    "7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn"
    "BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY"
)

# Per-(id, file) allowlist — deliberately NOT per-file alone, since a
# file can legitimately keep one retired id forever (e.g. the scaffold
# id in accounts.rs's negative-comparison test) while another literal in
# that SAME file (e.g. the mainnet id in a pin test) is exactly what
# this script exists to catch if it's ever left stale after a real
# replacement. Format: "<retired-id>|<allowed-file>".
allowed=(
    "7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn|service/src/bin/glc-mainnet-bootstrap.rs"   # RETIRED_PROGRAM_IDS denylist + its own pin test — must keep this id forever
    "BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY|service/src/solana/accounts.rs"              # every_pda_helper_derives_against_program_id's negative-comparison check against the old scaffold id
    "BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY|Anchor.toml"                                 # permanent historical comment explaining the scaffold-id incident, not an operational value
    "BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY|shared/src/lib.rs"                            # same permanent historical comment, in PROGRAM_ID_BYTES's own incident writeup
)

fail=0

for id in "${retired_ids[@]}"; do
    while IFS=: read -r file _rest; do
        [ -z "${file:-}" ] && continue
        file="${file#./}"
        is_allowed=0
        for pair in "${allowed[@]}"; do
            allowed_id="${pair%%|*}"
            allowed_file="${pair##*|}"
            if [ "$id" = "$allowed_id" ] && [ "$file" = "$allowed_file" ]; then
                is_allowed=1
                break
            fi
        done
        if [ "$is_allowed" -eq 0 ]; then
            echo "FAIL: retired program id $id found in operational file: $file" >&2
            fail=1
        fi
    done < <(grep -rn --include="*.rs" --include="*.toml" -F "$id" . 2>/dev/null | grep -v '^\./target/')
done

declare_id_line="$(grep -oE 'declare_id!\("[1-9A-HJ-NP-Za-km-z]+"\)' programs/glc-reserve-bridge/src/lib.rs || true)"
declare_id="$(echo "$declare_id_line" | grep -oE '"[^"]+"' | tr -d '"')"
anchor_toml_id="$(grep -E '^glc_reserve_bridge = ' Anchor.toml | sed -E 's/^glc_reserve_bridge = "([^"]+)"/\1/')"

if [ -z "$declare_id" ]; then
    echo "FAIL: could not find declare_id!(\"...\") in programs/glc-reserve-bridge/src/lib.rs" >&2
    fail=1
fi
if [ -z "$anchor_toml_id" ]; then
    echo "FAIL: could not find glc_reserve_bridge = \"...\" under [programs.localnet] in Anchor.toml" >&2
    fail=1
fi
if [ -n "$declare_id" ] && [ -n "$anchor_toml_id" ] && [ "$declare_id" != "$anchor_toml_id" ]; then
    echo "FAIL: declare_id! ($declare_id) does not match Anchor.toml's [programs.localnet] entry ($anchor_toml_id)" >&2
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo >&2
    echo "One or more checks failed — see above. Do not deploy until every" >&2
    echo "FAIL line is resolved." >&2
    exit 1
fi

echo "OK: no retired program id found outside its permanent denylist location."
echo "OK: declare_id! and Anchor.toml's [programs.localnet] agree ($declare_id)."
echo
echo "This script does not check glc-reserve-bridge-shared::PROGRAM_ID_BYTES"
echo "or the pin-test literals in service/src/solana/{accounts,instructions}.rs"
echo "and programs/glc-reserve-bridge/src/lib.rs::program_id_tests — run"
echo "'anchor build && cargo test' at the repo root next; those tests fail"
echo "closed on any remaining disagreement."
