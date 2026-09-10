#!/usr/bin/env bash
# Automated tests for scripts/bridge-admin.sh.
#
# Everything here runs against a STATEFUL MOCK `glc-admin` in a temp
# directory — no real config, no real ledger, no chain, no daemon, nothing
# under /etc or /var. The mock records the exact argv of every invocation
# AND keeps the small amount of state the console verifies against (route
# flags, pause flags, admission), so a post-change verification is a real
# round trip rather than a printout.
#
# The assertions are written against the ARGV, because the interesting
# property of an operator console is not what it prints: it is WHICH
# COMMAND IT RAN, and whether it ran one at all.
#
# The safety properties under test, in the order they matter:
#
#   1. SolToRhn and RhnToSol can never be enabled, by any menu path, with
#      any input.
#   2. A state-changing action runs nothing without an exact typed
#      confirmation. Lowercase does not count.
#   3. Every action that has a dry run does it FIRST, and a dry run mutates
#      nothing.
#   4. Invalid numbers and unsafe notes are refused before any command is
#      built; human units reach glc-admin's own conversion flags.
#   5. The Robinhood half-bucket rule is glc-admin's arithmetic, never the
#      console's.
#   6. A failed post-change verification makes the session exit non-zero.
#   7. A rolling-limit reset is offered only where an official mechanism
#      exists, and reported as unsupported where it does not.
#   8. Read-only menus stay read-only.
#   9. Ctrl+C leaves no temp directory behind.
#
# Usage:  scripts/tests/bridge-admin-test.sh
# Exit:   0 all passed, 1 otherwise.

set -uo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/bridge-admin.sh"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/bridge-admin-test.XXXXXX")"
trap 'rm -rf -- "$WORK"' EXIT

PASSED=0
FAILED=0

ok()    { printf '  [ PASS ] %s\n' "$1"; PASSED=$((PASSED + 1)); }
bad()   { printf '  [ FAIL ] %s\n' "$1"; FAILED=$((FAILED + 1)); }
head_() { printf '\n== %s\n' "$1"; }

# --------------------------------------------------------------- fixtures --

MOCK_BIN="$WORK/bin/glc-admin"
MOCK_LOG="$WORK/invocations.log"
MOCK_STATE="$WORK/state"
CONFIG="$WORK/config.toml"
BAD_CONFIG="$WORK/fragment.toml"
LEDGER="$WORK/ledger.db"
KEYPAIR="$WORK/admin-keypair.json"
LOGFILE="$WORK/action.log"
BACKUP_PATH="$WORK/config.toml.bak-1757462400"
RPC_URL="http://127.0.0.1:8899"

mkdir -p "$WORK/bin" "$MOCK_STATE"

cat >"$CONFIG" <<'EOF'
[solana]
rpc_url = "http://127.0.0.1:8899"

# OPERATOR NOTE: this comment must survive every edit.
[routes]
glc_to_rhn = true
rhn_to_glc = true

[robinhood.policy]
fee_bps = 600
EOF

cat >"$BAD_CONFIG" <<'EOF'
[robinhood.policy]
fee_bps = 600
EOF

# Stands in for a keypair FILE. Its contents are irrelevant: the whole
# point is that the console only ever passes the PATH.
printf 'this file must never be read by the console\n' >"$KEYPAIR"
: >"$LEDGER"
: >"$BACKUP_PATH"

# The stateful mock.
cat >"$MOCK_BIN" <<MOCKEOF
#!/usr/bin/env bash
set -u
# The whole argv, space-joined, one line per invocation. Recorded WITHOUT
# shifting: a \`shift\` here would consume the subcommand before the
# dispatch below ever saw it.
printf '%s\n' "\$*" >> "$MOCK_LOG"

STATE="$MOCK_STATE"
cmd="\${1:-}"
shift || true

flag() {
    local want="\$1"; shift
    local i=1
    for a in "\$@"; do
        if [ "\$a" = "\$want" ]; then
            shift "\$i"; printf '%s' "\${1:-}"; return 0
        fi
        i=\$((i + 1))
    done
    return 1
}
has() {
    local want="\$1"; shift
    for a in "\$@"; do [ "\$a" = "\$want" ] && return 0; done
    return 1
}
get_state() { cat "\$STATE/\$1" 2>/dev/null || printf '%s' "\$2"; }
set_state() { printf '%s' "\$2" > "\$STATE/\$1"; }

case "\$cmd" in
--help|-h)
    cat <<'USAGE'
glc-admin — reserve bridge operator CLI (TEST MOCK)
  glc-admin status --db PATH
  glc-admin pause   --db PATH --direction <goldcoin|solana> --note TEXT
  glc-admin unpause --db PATH --direction <goldcoin|solana> --note TEXT
  glc-admin close-admission --db PATH --direction goldcoin --note TEXT
  glc-admin open-admission --db PATH --direction goldcoin --note TEXT
  glc-admin show-config    --rpc-url URL
  glc-admin show-authorities --rpc-url URL
  glc-admin rebalance-policy-show --rpc-url URL
  glc-admin rebalance-status  --db PATH
  glc-admin onchain-pause   --rpc-url URL --keypair PATH --scope SCOPE --note TEXT
  glc-admin onchain-unpause --rpc-url URL --keypair PATH --scope SCOPE --note TEXT
  glc-admin set-limit --rpc-url URL --keypair PATH --field F --value N --note TEXT
  glc-admin reset-rolling-window --rpc-url URL --keypair PATH --direction D --note TEXT
  glc-admin robinhood-status (--db PATH | --config PATH)
  glc-admin robinhood-manual-review-list (--db PATH | --config PATH)
  glc-admin robinhood-tx-show (--db PATH | --config PATH)
  glc-admin robinhood-nonce-status --config PATH
  glc-admin robinhood-clear-halt (--config PATH | --db PATH) --note TEXT
  glc-admin robinhood-preflight --config PATH
  glc-admin robinhood-routes (--db PATH | --config PATH) [--json] [--porcelain]
  glc-admin robinhood-route-enable  --db PATH --route <GlcToRhn|RhnToGlc> --note TEXT
  glc-admin robinhood-route-disable --db PATH --route <GlcToRhn|RhnToGlc> --note TEXT
  glc-admin robinhood-reserve --config PATH
  glc-admin robinhood-governance-set-limits --config PATH --note TEXT
  glc-admin robinhood-governance-pause --config PATH --scope S --paused B --note TEXT
  glc-admin robinhood-governance-route --config PATH --route R --enabled B --note TEXT
  glc-admin fees-show --config PATH [--route R] [--json] [--porcelain]
  glc-admin fees-set --config PATH --route R --fee-percent X --note TEXT
  glc-admin chain-policy-check-config --config PATH [--porcelain]
  glc-admin chain-policy-networks [--json] [--porcelain]
  glc-admin chain-policy-show --config PATH --network NAME
  glc-admin chain-policy-validate --config PATH --network NAME
  glc-admin chain-policy-apply --config PATH --network NAME --note TEXT
USAGE
    exit 0
    ;;
fees-show)
    only="\$(flag --route "\$@")" || only=""
    for r in GlcToSol SolToGlc GlcToRhn RhnToGlc; do
        [ -n "\$only" ] && [ "\$only" != "\$r" ] && continue
        case "\$r" in
            GlcToSol|SolToGlc) def=300 ;;
            *) def=600 ;;
        esac
        bps="\$(get_state "fee_\$r" "\$def")"
        pct="\$(( bps / 100 ))"
        if has --porcelain "\$@"; then
            printf 'fee\t%s\t%s\t%s%%\n' "\$r" "\$bps" "\$pct"
        else
            printf '  %-9s  %-8s  %-7s  [fees] in this config file\n' "\$r" "\$pct%" "\$bps"
        fi
    done
    has --porcelain "\$@" || {
        echo "Goldcoin Bridge — per-route fees"
        echo "SolToRhn and RhnToSol are not listed"
        echo "contract stores NO fee"
        echo "Nothing was written."
    }
    exit 0
    ;;
fees-set)
    route="\$(flag --route "\$@")" || route=""
    pct="\$(flag --fee-percent "\$@")" || pct=""
    case "\$route" in
        GlcToSol|SolToGlc|GlcToRhn|RhnToGlc) ;;
        *) echo "error: \$route has no settlement machinery in this build" >&2; exit 1 ;;
    esac
    case "\$pct" in
        ''|*[!0-9.]*) echo "error: --fee-percent: not a valid fee percentage" >&2; exit 1 ;;
    esac
    if [ "\${pct%%.*}" -ge 100 ] 2>/dev/null; then
        echo "error: fee_bps would deliver nothing" >&2; exit 1
    fi
    echo "Per-route fee change — \$route"
    echo "BEFORE: \$route"
    echo "AFTER:  \$route"
    echo "Every OTHER route, unchanged"
    if has --execute "\$@"; then
        set_state "fee_\$route" "\$(( \${pct%%.*} * 100 ))"
        echo "APPLIED."
        echo "  Backup:  $BACKUP_PATH"
        echo "  Config:  $CONFIG"
        echo "The running daemon has NOT been restarted"
        echo "In-flight requests are unaffected"
        echo "nothing on chain to reconcile"
        exit 0
    fi
    echo "DRY RUN — nothing was written."
    exit 0
    ;;
chain-policy-check-config)
    path="\$(flag --config "\$@")" || path=""
    if [ "\$path" = "$CONFIG" ]; then
        echo "OK — this is a bridge config file and the config parser loads it."
        exit 0
    fi
    echo "NOT A CONFIG FILE — this is a POLICY FRAGMENT." >&2
    exit 1
    ;;
status)
    path="\$(flag --db "\$@")" || path=""
    if [ "\$path" != "$LEDGER" ]; then
        echo "could not open \$path: not a database" >&2
        exit 1
    fi
    echo "GoldcoinReserve: balance=9260000000000 paused=\$(get_state pause_goldcoin false) admission_closed=\$(get_state admission false) invariant_holds=true"
    echo "SolanaReserve: balance=9260000000000 paused=\$(get_state pause_solana false) admission_closed=false invariant_holds=true"
    echo "ManualReview backlog: 0"
    exit 0
    ;;
pause|unpause)
    dir="\$(flag --direction "\$@")" || dir=""
    [ "\$cmd" = pause ] && v=true || v=false
    set_state "pause_\$dir" "\$v"
    echo "local pause for \$dir set to \$v"
    exit 0
    ;;
close-admission|open-admission)
    [ "\$cmd" = close-admission ] && v=true || v=false
    set_state admission "\$v"
    echo "admission_closed set to \$v"
    exit 0
    ;;
robinhood-routes)
    for r in GlcToSol SolToGlc GlcToRhn RhnToGlc SolToRhn RhnToSol; do
        case "\$r" in
            GlcToSol|SolToGlc) def=enabled ;;
            *) def=disabled ;;
        esac
        eval "v_\$r=\\"\\\$(get_state route_\$r \$def)\\""
    done
    if has --porcelain "\$@"; then
        echo "bridge_routes_table	present"
        if flag --config "\$@" >/dev/null; then echo "config_gate	available"; else echo "config_gate	not-read"; fi
        for r in GlcToSol SolToGlc GlcToRhn RhnToGlc SolToRhn RhnToSol; do
            eval "v=\\\$v_\$r"
            case "\$r" in
                GlcToRhn|RhnToGlc) settable=true; adapter=verified-at-daemon-startup ;;
                GlcToSol|SolToGlc) settable=false; adapter=operational ;;
                *) settable=false; adapter=unavailable-always ;;
            esac
            printf 'route\t%s\t%s\t1757462400\tfalse\t%s\tunknown\t%s\n' "\$r" "\$v" "\$settable" "\$adapter"
        done
        exit 0
    fi
    echo "Robinhood route state — the LEDGER gate (bridge_routes)"
    for r in GlcToSol SolToGlc GlcToRhn RhnToGlc SolToRhn RhnToSol; do
        eval "v=\\\$v_\$r"
        printf '  %-9s %s\n' "\$r" "\$v"
    done
    echo "Operator-settable in this gate: GlcToRhn, RhnToGlc — and nothing else."
    echo "WHY THE LEGACY ROUTES ARE LISTED BUT NOT CONTROLLED HERE:"
    echo "THIS IS ONE GATE OF THREE, and none of them substitutes for another:"
    exit 0
    ;;
robinhood-route-enable|robinhood-route-disable)
    route="\$(flag --route "\$@")" || route=""
    case "\$route" in
        GlcToRhn|RhnToGlc) ;;
        *) echo "error: \$route is not operator-settable" >&2; exit 1 ;;
    esac
    [ "\$cmd" = robinhood-route-enable ] && v=enabled || v=disabled
    set_state "route_\$route" "\$v"
    echo "ledger route state for \$route set to \$v"
    exit 0
    ;;
chain-policy-networks)
    if has --porcelain "\$@"; then
        printf 'solana\tfixed\tSolana\n'
        printf 'robinhood\tconfigurable\tRobinhood Network\n'
        exit 0
    fi
    echo "Bridge networks with a chain policy:"
    exit 0
    ;;
chain-policy-show)
    network="\$(flag --network "\$@")" || network=""
    if has --porcelain "\$@"; then
        printf 'network\t%s\n' "\$network"
        if [ "\$network" = robinhood ]; then
            printf 'configurable\ttrue\nconfigured\ttrue\n'
            printf 'fee_bps\t%s\n' "\$(get_state fee_bps 600)"
            printf 'per_transfer_limit\t%s\n' "\$(get_state per_limit 2000000000000)"
            printf 'rolling_daily_limit\t%s\n' "\$(get_state roll_limit 1000000000000000)"
        else
            printf 'configurable\tfalse\nconfigured\tfalse\n'
        fi
        exit 0
    fi
    echo "Goldcoin Bridge — Chain Policy"
    echo "Network: \$network"
    if [ "\$network" = robinhood ]; then
        echo "  Fee:                 6%                     (\$(get_state fee_bps 600) bps)"
        echo "  Per-transfer limit:  20,000 GLC             (\$(get_state per_limit 2000000000000) canonical 8dp)"
        echo "  24h rolling limit:   10,000,000 GLC         (\$(get_state roll_limit 1000000000000000) canonical 8dp, STRICT)"
        echo "    Requested strict 24h policy:       10,000,000 GLC"
        echo "    Recommended on-chain bucket limit:  5,000,000 GLC"
        if ! has --no-onchain "\$@"; then
            echo "On-chain enforcement (GlcRobinhoodBridge):"
            if [ "\${MOCK_POLICY_MATCH:-match}" = match ]; then
                echo "  MATCH — the deployed contract enforces exactly the configured policy."
            else
                echo "  MISMATCH — 1 disagreement(s):"
                echo "    - inboundRollingLimit is 5000000000000000000000000, expected 2500000000000000000000000"
            fi
        fi
    else
        echo "  NOT CHANGEABLE by this tool: its fee is a compiled-in constant."
    fi
    exit 0
    ;;
chain-policy-validate)
    echo "(mock) candidate policy validated; nothing was written"
    exit 0
    ;;
chain-policy-apply)
    if has --execute "\$@"; then
        fee="\$(flag --fee-percent "\$@")" && set_state fee_bps "fee-percent:\$fee"
        echo "APPLIED."
        echo "  Backup:  $BACKUP_PATH"
        echo "  Config:  $CONFIG"
        exit 0
    fi
    echo "BEFORE:"
    echo "AFTER:"
    echo "DRY RUN — nothing was written."
    exit 0
    ;;
robinhood-preflight)
    echo "[PASS      ] chain id                     matches"
    echo "[PASS      ] signer quorum                3 of 3 reachable, threshold 2"
    echo "[PASS      ] routeEnabled[GlcToRhn]       false (expected closed)"
    echo "[UNVERIFIED] token security properties    not establishable by an RPC read"
    if has --expect-route-enabled "\$@" && [ "\${MOCK_PREFLIGHT_ROUTES:-pass}" != pass ]; then
        echo "1 preflight check(s) FAILED" >&2
        exit 1
    fi
    exit 0
    ;;
robinhood-governance-route|robinhood-governance-pause|robinhood-governance-set-limits)
    echo "Robinhood governance proposal"
    echo "BEFORE:  (mock)"
    echo "AFTER:   (mock)"
    if has --execute "\$@"; then
        echo "INSTALLED and VERIFIED."
        echo "  Transaction: 0xdeadbeef"
        exit 0
    fi
    echo "DRY RUN — no custody domain was contacted."
    exit 0
    ;;
robinhood-reserve)
    echo "Robinhood reserve — ledger (canonical 8dp)"
    echo "  paused              false"
    echo "Robinhood reserve — on-chain (Robinhood native 18dp)"
    echo "  inbound  limit 5000000000000000000000000 used 0 remaining 5000000000000000000000000 (bucket resets at 1757548800)"
    echo "  outbound limit 5000000000000000000000000 used 0 remaining 5000000000000000000000000 (bucket resets at 1757548800)"
    exit 0
    ;;
robinhood-status)
    echo "Robinhood indexer"
    echo "  halted           \$(get_state halted no)"
    exit 0
    ;;
robinhood-clear-halt)
    if has --execute "\$@"; then set_state halted no; echo "halt cleared"; else echo "DRY RUN"; fi
    exit 0
    ;;
show-config)
    echo "bridge_config (mock):"
    echo "  paused (global)    = \$(get_state onchain_global false)"
    echo "  release_paused     = \$(get_state onchain_release false)"
    echo "  deposit_paused     = \$(get_state onchain_deposit false)"
    echo "  per_transfer_limit = \$(get_state per_transfer_onchain 2000000000)"
    echo "  rolling_volume_limit   = \$(get_state rolling_onchain 100000000000) (GLOBAL, per direction)"
    echo "  rolling_volume_window[release (GlcToSol)] (pda): remaining = 100000000000 quota_exhausted = false"
    exit 0
    ;;
onchain-pause|onchain-unpause)
    scope="\$(flag --scope "\$@")" || scope=""
    [ "\$cmd" = onchain-pause ] && v=true || v=false
    set_state "onchain_\$scope" "\$v"
    echo "submitted"
    exit 0
    ;;
set-limit)
    field="\$(flag --field "\$@")" || field=""
    value="\$(flag --value "\$@")" || value=""
    case "\$field" in
        per-transfer)   set_state per_transfer_onchain "\$value" ;;
        rolling-volume) set_state rolling_onchain "\$value" ;;
    esac
    echo "submitted set_limit(field=\$field, new_value=\$value)"
    exit 0
    ;;
reset-rolling-window)
    echo "submitted reset_rolling_volume_window"
    exit 0
    ;;
*)
    echo "(mock) \$cmd ok"
    exit 0
    ;;
esac
MOCKEOF
chmod +x "$MOCK_BIN"

export GLC_ADMIN="$MOCK_BIN"
export GLC_BRIDGE_ADMIN_LOG="$LOGFILE"

# ---------------------------------------------------------------- driver --

OUT=""
RC=0
reset_state() { rm -f "$MOCK_STATE"/* 2>/dev/null; : >"$MOCK_LOG"; }

drive() {
    reset_state
    OUT="$WORK/out.txt"
    printf '%s\n' "$@" | bash "$SCRIPT" --config "$CONFIG" --db "$LEDGER" \
        --rpc-url "$RPC_URL" >"$OUT" 2>&1
    RC=$?
    return 0
}

invoked()       { grep -qF -- "$1" "$MOCK_LOG"; }
invoked_count() { grep -cF -- "$1" "$MOCK_LOG" 2>/dev/null || printf '0'; }
said()          { grep -qF -- "$1" "$OUT"; }

assert_invoked() {
    if invoked "$1"; then ok "$2"; else
        bad "$2"
        printf '        expected an invocation containing: %s\n' "$1"
        printf '        actual invocations:\n'
        sed 's/^/          /' "$MOCK_LOG"
    fi
}
assert_not_invoked() {
    if invoked "$1"; then
        bad "$2"
        printf '        UNEXPECTED invocation containing: %s\n' "$1"
        sed 's/^/          /' "$MOCK_LOG"
    else ok "$2"; fi
}
assert_said() {
    if said "$1"; then ok "$2"; else
        bad "$2"
        printf '        expected output containing: %s\n' "$1"
    fi
}
assert_rc() {
    if [ "$RC" = "$1" ]; then ok "$2"; else
        bad "$2"; printf '        expected exit %s, got %s\n' "$1" "$RC"
    fi
}
# Source-level assertion: greps the script with its own comments removed.
script_code_has() {
    grep -vE '^[[:space:]]*#' "$SCRIPT" | grep -qE "$1"
}

# ================================================================= tests ==

head_ "Static checks"
if bash -n "$SCRIPT"; then ok "bash -n parses the console"; else bad "bash -n parses the console"; fi
if [ -x "$SCRIPT" ]; then ok "console is executable"; else bad "console is executable"; fi
if grep -q '^set -euo pipefail$' "$SCRIPT"; then ok "sets -euo pipefail"; else bad "sets -euo pipefail"; fi
if script_code_has '(^|[^[:alnum:]_])eval[[:space:]]'; then
    bad "never calls eval"; else ok "never calls eval"; fi
if script_code_has 'set -x|set -o xtrace'; then
    bad "never enables xtrace"; else ok "never enables xtrace"; fi
if script_code_has 'systemctl|service .* restart|kill -HUP'; then
    bad "never restarts or signals the daemon"; else ok "never restarts or signals the daemon"; fi
if script_code_has 'sqlite3'; then
    bad "never touches SQLite directly"
else ok "never touches SQLite directly (uses glc-admin robinhood-routes)"; fi
if grep -q 'mktemp' "$SCRIPT" && grep -q 'trap cleanup EXIT' "$SCRIPT"; then
    ok "uses mktemp with an EXIT trap"; else bad "uses mktemp with an EXIT trap"; fi
# The half-bucket rule must be glc-admin's arithmetic, never the console's.
if script_code_has '\$\(\( *[A-Za-z_$][A-Za-z0-9_]* */ *2'; then
    bad "never computes the contract half-bucket itself"
else ok "never computes the contract half-bucket itself"; fi
if script_code_has 'cat .*keypair|--keypair "\$\(cat|\$\(cat .*(key|token|secret)'; then
    bad "never reads a keypair or a secret file's contents"
else ok "never reads a keypair or a secret file's contents"; fi

head_ "--help works with no glc-admin and no PATH"
BASH_BIN="$(command -v bash)"
help_out="$(env -u GLC_ADMIN PATH=/nonexistent "$BASH_BIN" "$SCRIPT" --help 2>&1)"
if printf '%s' "$help_out" | grep -q 'Goldcoin Bridge — Operator Admin Console'; then
    ok "--help prints the header with no external command available"
else bad "--help prints the header with no external command available"; fi

head_ "Main menu"
drive 4
assert_said "1) Goldcoin <-> Solana"    "main menu offers Goldcoin <-> Solana"
assert_said "2) Goldcoin <-> Robinhood" "main menu offers Goldcoin <-> Robinhood"
assert_said "3) Overall bridge status"  "main menu offers overall status"
assert_said "4) Exit"                   "main menu offers Exit"
assert_not_invoked "--execute"          "exiting runs no mutation"
assert_rc 0                             "a clean session exits 0"

head_ "Overall bridge status is read-only"
drive 3 4
assert_invoked "status --db"                "runs ledger status"
assert_invoked "robinhood-status --db"      "runs Robinhood status"
assert_invoked "robinhood-routes --db"      "reads the route gates through glc-admin"
assert_invoked "chain-policy-networks"      "lists policy-governed networks"
assert_invoked "robinhood-reserve --config" "shows the Robinhood reserve + buckets"
assert_not_invoked "--execute"              "no --execute anywhere"
assert_not_invoked "chain-policy-apply"     "no config write"
assert_not_invoked "robinhood-route-enable" "no route write"
assert_rc 0                                 "a read-only session exits 0"

head_ "Route & gate state distinguishes every switch"
drive 2 1 19 4
assert_said "FOUR INDEPENDENT SWITCHES" "the Robinhood screen names four switches"
assert_said "1. CONFIG"                 "the config switch is named"
assert_said "2. LEDGER"                 "the ledger switch is named"
assert_said "3. ADAPTER"                "the adapter capability switch is named"
assert_said "4. CONTRACT"               "the contract switch is named"
assert_invoked "robinhood-routes --config $CONFIG --db $LEDGER" \
    "reads config + ledger gates via robinhood-routes"
assert_invoked "robinhood-preflight --config" "reads the contract gate via preflight"
assert_not_invoked "--execute"          "the route-state screen changes nothing"

drive 1 1 17 4
assert_said "GlcToSol and SolToGlc are NOT controlled by a route flag" \
    "the Solana screen explains the pause/admission model"
assert_said "LOCAL LEDGER PAUSE"  "local pause named"
assert_said "ADMISSION CONTROL"   "admission control named"
assert_said "ON-CHAIN PAUSE"      "on-chain pause named"
assert_not_invoked "--execute"    "the Solana route-state screen changes nothing"

head_ "SolToRhn and RhnToSol can never be enabled"
for menu_path in "2 12" "2 13" "2 16"; do
    # shellcheck disable=SC2086
    drive $menu_path SolToRhn RhnToSol 3 4 5 99 "" 19 4
    assert_not_invoked "SolToRhn" "no SolToRhn command from menu path '$menu_path'"
    assert_not_invoked "RhnToSol" "no RhnToSol command from menu path '$menu_path'"
done
drive 2 12 19 4
assert_said "SolToRhn and RhnToSol are not listed" "the route chooser says why they are absent"
if grep -vE '^[[:space:]]*#' "$SCRIPT" \
    | grep -qE '(route-enable|route-disable|governance-route).*(SolToRhn|RhnToSol)'; then
    bad "no code path names SolToRhn/RhnToSol in a route-command position"
else ok "no code path names SolToRhn/RhnToSol in a route-command position"; fi
if grep -q 'OPERATOR_ROUTES=(GlcToRhn RhnToGlc)' "$SCRIPT"; then
    ok "the operator-settable route list is exactly GlcToRhn and RhnToGlc"
else bad "the operator-settable route list is exactly GlcToRhn and RhnToGlc"; fi

head_ "Ledger route enable/disable: confirmation, then verification"
drive 2 12 1 "launch ticket OPS-1234" no 19 4
assert_not_invoked "robinhood-route-enable" "a wrong confirmation word runs nothing"
assert_said "Aborted" "the abort is reported"

drive 2 12 1 "launch ticket OPS-1234" execute 19 4
assert_not_invoked "robinhood-route-enable" "lowercase 'execute' is not accepted"

drive 2 12 1 "launch ticket OPS-1234" EXECUTE 19 4
assert_invoked "robinhood-route-enable --db $LEDGER --route GlcToRhn --note launch ticket OPS-1234" \
    "EXECUTE runs the ledger route enable, with the note"
assert_said "Step B — proposed state" "the proposed state is shown before the action"
assert_said "LEDGER switch ONLY"      "it says which switch it changes"
assert_said "VERIFIED: ledger gate for GlcToRhn is enabled" \
    "the post-change verification re-reads and PASSES"
assert_not_invoked "robinhood-governance-route" "it never touches contract governance"
assert_not_invoked "chain-policy-apply"         "it never edits the config"
assert_rc 0 "a verified change exits 0"

drive 2 13 2 "incident OPS-1300" EXECUTE 19 4
assert_invoked "robinhood-route-disable --db $LEDGER --route RhnToGlc --note incident OPS-1300" \
    "the disable path works for RhnToGlc"
assert_said "VERIFIED: ledger gate for RhnToGlc is disabled" "disable is verified too"

head_ "Per-route fees: route first, then that route's rate"
drive 2 6 19 4
assert_invoked "fees-show --config $CONFIG"  "the fee screen reads the per-route table"
assert_said "GlcToRhn"                        "each route is listed"
assert_said "SolToRhn and RhnToSol are not listed" "unpriceable routes are named as such"
assert_not_invoked "--execute"                "showing fees changes nothing"

# Route chooser -> current fee -> new fee -> dry run -> APPLY.
drive 2 7 4 -5 abc 150 100 1e3 3.14159 4 "fee change OPS-2400" no 19 4
assert_said "Which ROUTE's fee?"                  "the operator picks a route FIRST"
assert_said "not a valid percentage: '-5'"        "a negative fee is refused"
assert_said "not a valid percentage: 'abc'"       "a non-numeric fee is refused"
assert_said "not a valid percentage: '1e3'"       "an exponent is refused"
assert_said "not a valid percentage: '3.14159'"   "more than two decimal places is refused"
assert_said "would leave the user nothing (100%)" "150% is refused"
assert_said "The maximum is 99.99%"               "the maximum is stated"
assert_not_invoked "--fee-percent -5"   "the refused value never reaches glc-admin"
assert_not_invoked "--fee-percent 150"  "the over-100% value never reaches glc-admin"
assert_not_invoked "--fee-percent 100"  "exactly 100% never reaches glc-admin"
assert_invoked "--fee-percent 4"        "4% reaches glc-admin — no rebuild, no allowlist"
assert_not_invoked "--execute"          "an aborted fee change writes nothing"
assert_said "DRY RUN"                   "a dry run runs before the confirmation"
assert_said "Any rate from 0% to 99.99% is valid" "the console states the real range"

# 4% -> 400 bps, applied. The rate the old allowlist refused outright.
drive 2 7 4 4 "fee change OPS-2401" APPLY 19 4
assert_invoked "fees-set --config $CONFIG --route RhnToGlc --fee-percent 4 --note fee change OPS-2401" \
    "the change names exactly one route"
assert_invoked "--execute"                          "APPLY executes the write"
assert_said "VERIFIED: a timestamped config backup was taken" "the backup path is verified"
assert_said "$BACKUP_PATH"                          "the backup path is shown to the operator"
assert_said "A DAEMON RESTART IS REQUIRED"          "the restart requirement is stated"
assert_said "this console will not do it"           "the console does not restart anything"
assert_said "VERIFIED: RhnToGlc now prices at 400 bps" \
    "4% is verified by re-reading — the value the allowlist used to refuse"
assert_said "VERIFIED: every other route's fee was left alone" \
    "the other routes are proven untouched"
assert_rc 0 "a fully verified fee change exits 0"

head_ "A free route is possible, and asks for its own confirmation"
drive 2 7 4 0 no 19 4
assert_said "makes RhnToGlc FREE"        "0% is called out in its own words"
assert_said "Type FREE to proceed"       "0% needs a separate typed confirmation"
assert_not_invoked "fees-set"            "declining the FREE confirmation runs nothing"

drive 2 7 4 0 FREE "promotional OPS-2410" APPLY 19 4
assert_invoked "fees-set --config $CONFIG --route RhnToGlc --fee-percent 0" \
    "a confirmed 0% fee is applied"
assert_invoked "--execute"               "APPLY still executes the write"

head_ "One route's fee never moves another's"
drive 2 7 3 6 "robinhood outbound OPS-2402" APPLY 19 4
assert_invoked "fees-set --config $CONFIG --route GlcToRhn --fee-percent 6" \
    "the selected route is the one written"
# `fees-show --route <each>` IS called to draw the chooser; only the WRITE
# must name one route, so the assertion is about `fees-set` specifically.
for other in GlcToSol SolToGlc RhnToGlc; do
    assert_not_invoked "fees-set --config $CONFIG --route $other" \
        "no fees-set names $other"
done
assert_not_invoked "chain-policy-apply"           "a fee change never edits the chain policy"
assert_not_invoked "robinhood-governance"         "a fee change never proposes governance"

head_ "SolToRhn and RhnToSol are never offered a fee"
drive 2 7 SolToRhn RhnToSol 9 99 "" 19 4
assert_not_invoked "SolToRhn" "the fee chooser never names SolToRhn"
assert_not_invoked "RhnToSol" "the fee chooser never names RhnToSol"
assert_said "neither can be priced" "the fee chooser says why they are absent"

head_ "The per-chain limit flow no longer touches fees"
drive 2 8 -1 "20.123456789" "20,000" "policy OPS-2403" no 19 4
assert_said "not a valid GLC amount: '-1'"          "a negative limit is refused"
assert_said "not a valid GLC amount: '20.123456789'" "more than 8 decimal places is refused"
assert_invoked "--per-transfer-glc 20000" "thousands separators are normalised and passed through"
assert_invoked "--fee-bps 600" "the policy's existing fee_bps is carried across byte-identical"
assert_not_invoked "--fee-percent" "the limit flow never sends a fee percentage"

head_ "Strict 24h rolling limit and the half-bucket rule"
drive 2 9 10000000 "policy OPS-2404" no 19 4
assert_said "Do NOT halve it yourself" "the operator is told not to compute the bucket"
assert_said "exactly HALF of this"     "the half-bucket relationship is stated"
assert_invoked "--rolling-glc 10000000" "the STRICT figure is passed through as typed"
assert_not_invoked "--rolling-glc 5000000" "the console never halves the figure itself"
assert_said "Recommended on-chain bucket limit"  "glc-admin's derived bucket figure is shown"

head_ "Unchanged policy fields are carried across in exact machine units"
drive 2 8 20000 "limit only OPS-2405" no 19 4
assert_invoked "--rolling-daily-limit 1000000000000000" \
    "the untouched rolling limit is carried as atomic units"

head_ "Robinhood governance dry-runs before it executes"
drive 2 16 1 1 "launch ticket OPS-1234" no 19 4
if [ "$(invoked_count 'robinhood-governance-route')" = "1" ]; then
    ok "an aborted governance action runs exactly one command (the dry run)"
else
    bad "an aborted governance action runs exactly one command (the dry run)"
    sed 's/^/        /' "$MOCK_LOG"
fi
assert_not_invoked "--execute" "an aborted governance action never executes"

drive 2 16 1 1 "launch ticket OPS-1234" EXECUTE 3 19 4
first_gov="$(grep -n 'robinhood-governance-route' "$MOCK_LOG" | head -1)"
if printf '%s' "$first_gov" | grep -q -- '--execute'; then
    bad "the dry run comes before the execute"
    sed 's/^/        /' "$MOCK_LOG"
else ok "the dry run comes before the execute"; fi
assert_invoked "robinhood-governance-route --config $CONFIG --route GlcToRhn --enabled true --note launch ticket OPS-1234 --execute" \
    "the confirmed governance action executes with the exact proposal"
assert_invoked "robinhood-preflight --config $CONFIG --expect-route-enabled GlcToRhn,RhnToGlc" \
    "post-change verification checks the contract flags against a stated expectation"
assert_said "VERIFIED: contract route flags match the stated expectation" \
    "the contract-flag verification PASSES"

drive 2 15 1 1 "pausing deposits, OPS-1400" EXECUTE 19 4
assert_invoked "robinhood-governance-pause --config $CONFIG --scope deposits --paused true --note pausing deposits, OPS-1400 --execute" \
    "contract pause executes with the exact scope and flag"
assert_said "NOT the local ledger pause" "contract pause is distinguished from the local pause"

drive 2 14 n "reconcile to policy OPS-1500" EXECUTE 19 4
assert_invoked "robinhood-governance-set-limits --config $CONFIG --note reconcile to policy OPS-1500 --execute" \
    "setLimits executes carrying no figures of its own"
assert_not_invoked "--inbound-min" "no minimum override is sent unless asked for"
assert_said "VERIFIED: backend policy and deployed contract agree" "setLimits is verified after"

head_ "A failed post-change verification makes the session exit non-zero"
reset_state
OUT="$WORK/out.txt"
printf '%s\n' 2 11 20000 10000000 "policy OPS-1700" APPLY n 19 4 \
    | MOCK_POLICY_MATCH=mismatch bash "$SCRIPT" --config "$CONFIG" --db "$LEDGER" \
        --rpc-url "$RPC_URL" >"$OUT" 2>&1
RC=$?
assert_said "VERIFICATION: backend policy and deployed contract agree" \
    "a contract mismatch is reported as a verification FAILURE"
assert_said "SESSION RESULT: FAIL" "the session reports its own failure"
assert_rc 1 "the console exits non-zero when a verification failed"

reset_state
OUT="$WORK/out.txt"
printf '%s\n' 2 16 1 1 "route OPS-1701" EXECUTE 3 19 4 \
    | MOCK_PREFLIGHT_ROUTES=fail bash "$SCRIPT" --config "$CONFIG" --db "$LEDGER" \
        --rpc-url "$RPC_URL" >"$OUT" 2>&1
RC=$?
assert_said "VERIFICATION: contract route flags match the stated expectation" \
    "a failing preflight is reported as a verification FAILURE"
assert_rc 1 "a failed contract-flag verification exits non-zero"

head_ "Rolling-limit reset: only where an official mechanism exists"
drive 2 17 19 4
assert_said "NOT SUPPORTED BY THE CURRENT IMPLEMENTATION" \
    "the Robinhood bucket reset is reported as unsupported"
assert_said "There is no reset mechanism"        "it says plainly that none exists"
assert_said "will NOT fake a reset"              "it refuses to fake one by moving limits"
assert_said "bucket resets at"                   "it shows the current bucket state instead"
assert_not_invoked "--execute"                   "the unsupported reset mutates nothing"
assert_not_invoked "robinhood-governance-set-limits" "it does not touch limits as a workaround"
assert_not_invoked "reset-rolling-window"        "it does not misuse the Solana command"

drive 1 16 1 "$KEYPAIR" "maintenance OPS-1800" no 17 4
assert_not_invoked "reset-rolling-window --rpc-url" "an unconfirmed Solana reset runs nothing"
assert_said "BridgeConfig.paused is ALREADY true"  "the pause precondition is stated"

drive 1 16 1 "$KEYPAIR" "maintenance OPS-1800" EXECUTE 17 4
assert_invoked "reset-rolling-window --rpc-url $RPC_URL --keypair $KEYPAIR --direction glc-to-sol --note maintenance OPS-1800" \
    "a confirmed Solana rolling-window reset runs the official command"
assert_said "used volume     -> 0" "it explains exactly what is reset"

head_ "Pause / resume, kept distinct"
drive 1 13 1 "maintenance OPS-1801" no 17 4
assert_not_invoked "pause --db" "an unconfirmed local pause runs nothing"
drive 1 13 1 "maintenance OPS-1801" EXECUTE 17 4
assert_invoked "pause --db $LEDGER --direction goldcoin --note maintenance OPS-1801" \
    "a confirmed local pause runs with the exact direction and note"
assert_said "VERIFIED: GoldcoinReserve now reports paused=true" "the local pause is verified"

drive 1 13 2 "resuming OPS-1802" EXECUTE 17 4
assert_invoked "unpause --db $LEDGER --direction goldcoin" "resume runs unpause"

drive 1 14 1 "closing admission OPS-1803" EXECUTE 17 4
assert_invoked "close-admission --db $LEDGER --direction goldcoin --note closing admission OPS-1803" \
    "a confirmed admission close runs with the exact direction and note"
assert_said "VERIFIED: GoldcoinReserve now reports admission_closed=true" "admission is verified"
assert_said "NOT the local pause" "admission control is distinguished from the pause"

drive 1 15 1 "$KEYPAIR" "onchain pause OPS-1804" EXECUTE 17 4
assert_invoked "onchain-pause --rpc-url $RPC_URL --keypair $KEYPAIR --scope global" \
    "on-chain pause runs with the keypair PATH"
assert_said "VERIFIED: on-chain paused (global) is now true" "the on-chain pause is verified"

head_ "On-chain Solana limits"
drive 1 12 1 3000000000 "$KEYPAIR" "limit OPS-1900" EXECUTE 17 4
assert_invoked "set-limit --rpc-url $RPC_URL --keypair $KEYPAIR --field per-transfer --value 3000000000" \
    "set-limit runs with the field and value"
assert_said "ATOMIC UNITS OF THE SOLANA-SIDE MINT" "the unit caveat is stated plainly"
assert_said "VERIFIED: on-chain per_transfer_limit is now 3000000000" "the new limit is verified"

head_ "Operator notes are validated"
drive 2 12 1 "" "  " "$(printf 'bad\tnote')" "clean note OPS-2000" EXECUTE 19 4
assert_said "a note is required"                "a blank note is refused"
assert_said "notes must be printable text"      "a control character in a note is refused"
assert_invoked "--note clean note OPS-2000"     "the clean note is the one that is sent"

# Single quotes are the point: the note must reach glc-admin as literal
# text, with $(id) UNEXPANDED, so this asserts the console never
# interpolates it either.
# shellcheck disable=SC2016
drive 2 12 1 'ticket "OPS"; rm -rf /tmp/pwned && echo $(id)' EXECUTE 19 4
assert_invoked 'rm -rf /tmp/pwned' "a shell-metacharacter note is passed as ONE argv element"
if [ -e /tmp/pwned ] || said "uid="; then
    bad "a shell-metacharacter note is never interpreted"
else ok "a shell-metacharacter note is never interpreted"; fi

head_ "Keypair prompts refuse key material"
drive 1 15 1 '[123,45,67]' "" 17 4
assert_said "that looks like key MATERIAL, not a path" "pasted key material is refused"
assert_not_invoked "onchain-pause" "nothing was signed"
if grep -qF 'this file must never be read by the console' "$OUT"; then
    bad "the keypair file's contents are never printed"
else ok "the keypair file's contents are never printed"; fi

head_ "Config path validation"
reset_state
printf '%s\n' 2 4 19 4 | bash "$SCRIPT" --config "$BAD_CONFIG" --db "$LEDGER" >"$WORK/out.txt" 2>&1
RC=$?
OUT="$WORK/out.txt"
assert_said "That file cannot be used" "a policy fragment is rejected, in glc-admin's own words"
assert_not_invoked "chain-policy-show" "no action runs against a rejected config"

reset_state
printf '%s\n' 1 3 17 4 | bash "$SCRIPT" --db "$WORK/nope.db" >"$WORK/out.txt" 2>&1
OUT="$WORK/out.txt"
assert_said "no such file" "a missing ledger path is rejected"
assert_not_invoked "pause --db" "no mutation runs against a rejected ledger"

reset_state
printf '%s\n' 1 3 17 4 | bash "$SCRIPT" --db "$WORK" >"$WORK/out.txt" 2>&1
OUT="$WORK/out.txt"
assert_said "that is a directory, not a file" "a directory given as --db is rejected"

head_ "Action log records commands, never secrets"
if [ -s "$LOGFILE" ]; then
    ok "an action log was written"
    perms="$(stat -c '%a' "$LOGFILE" 2>/dev/null || printf '?')"
    if [ "$perms" = "600" ]; then ok "the action log is mode 600"; else
        bad "the action log is mode 600 (got $perms)"; fi
    if grep -qF 'this file must never be read by the console' "$LOGFILE"; then
        bad "the action log contains no keypair contents"
    else ok "the action log contains no keypair contents"; fi
    if grep -q 'cmd=' "$LOGFILE"; then ok "the action log records the command line"; else
        bad "the action log records the command line"; fi
else
    bad "an action log was written"
fi

head_ "Ctrl+C leaves no temp directory behind"
ITMP="$WORK/interrupt-tmp"
mkdir -p "$ITMP"
before="$(find "$ITMP" -maxdepth 1 -name 'glc-bridge-admin.*' 2>/dev/null | wc -l)"
fifo="$WORK/fifo"
mkfifo "$fifo"
# Job control ON for this one launch. Without it, a non-interactive shell
# starts a background child with SIGINT set to SIG_IGN, and an
# inherited-ignored signal cannot be trapped — so the test would be
# measuring bash's job-control behaviour rather than the console's cleanup.
set -m
TMPDIR="$ITMP" bash "$SCRIPT" --config "$CONFIG" --db "$LEDGER" <"$fifo" >/dev/null 2>&1 &
child=$!
set +m
exec 9>"$fifo"
created=0
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if find "$ITMP" -maxdepth 1 -name 'glc-bridge-admin.*' 2>/dev/null | grep -q .; then
        created=1; break
    fi
    sleep 0.2
done
if [ "$created" -eq 1 ]; then ok "the console created a temp directory to clean up"; else
    bad "the console created a temp directory to clean up"; fi
kill -INT "$child" 2>/dev/null || true
for _ in $(seq 1 40); do
    kill -0 "$child" 2>/dev/null || break
    sleep 0.1
done
kill -KILL "$child" 2>/dev/null || true
wait "$child" 2>/dev/null || true
exec 9>&-
after="$(find "$ITMP" -maxdepth 1 -name 'glc-bridge-admin.*' 2>/dev/null | wc -l)"
if [ "$after" -le "$before" ]; then
    ok "SIGINT removed the console's temp directory"
else
    bad "SIGINT removed the console's temp directory (before=$before after=$after)"
    find "$ITMP" -maxdepth 1 -name 'glc-bridge-admin.*' 2>/dev/null | sed 's/^/        /'
fi

head_ "Stale binary detection"
cat >"$WORK/bin/glc-admin-stale" <<'STALE'
#!/usr/bin/env bash
# A build from before the route-ledger commands landed: `status` works,
# `robinhood-route-enable` and `robinhood-routes` do not exist.
if [ "${1:-}" = "--help" ]; then
    echo "glc-admin — reserve bridge operator CLI (STALE MOCK)"
    echo "  glc-admin status --db PATH"
    exit 0
fi
if [ "${1:-}" = "status" ]; then
    echo "GoldcoinReserve: balance=0 paused=false"
    exit 0
fi
echo "unknown command: $1" >&2
exit 2
STALE
chmod +x "$WORK/bin/glc-admin-stale"
reset_state
OUT="$WORK/out.txt"
printf '%s\n' 2 12 19 4 \
    | GLC_ADMIN="$WORK/bin/glc-admin-stale" bash "$SCRIPT" --db "$LEDGER" >"$OUT" 2>&1
assert_said "is not implemented by this binary" "a stale glc-admin is detected, not blundered into"
assert_said "STALE BUILD" "the remedy is named"

# ================================================================ result ==

printf '\n%s\n' "----------------------------------------------------------------------"
printf 'bridge-admin.sh: %d passed, %d failed\n' "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ]
