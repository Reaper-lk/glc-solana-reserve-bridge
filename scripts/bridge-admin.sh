#!/usr/bin/env bash
# Goldcoin Bridge — Operator Admin Console.
#
# An interactive operations console for the bridge: an inspector AND a safe
# way to make the changes an operator actually needs to make. It draws
# menus, asks questions, shows exactly what it is about to run, and demands
# a typed confirmation before anything changes.
#
# It does NOT re-implement bridge logic. Every fee conversion, every limit
# check, every policy edit, every governance proposal and every ledger
# write happens inside `glc-admin`, behind the same types and the same
# config parser the daemon itself uses. A shell script that re-implemented
# any of them would be a second set of rules, and the second set is always
# the one that is wrong.
#
# WHAT THIS CONSOLE NEVER DOES
#   - read, print, accept or transport a secret, a token or a private key
#   - enable a route as a side effect of any other action
#   - enable SolToRhn or RhnToSol (structurally non-executable; refused
#     here before a command is built, and refused again by glc-admin)
#   - weaken the 2-of-3 governance quorum
#   - restart, reload or signal the daemon
#   - write a config file or a database itself
#   - read or write SQLite directly, or contract storage directly
#   - `eval` anything, or enable xtrace
#
# THE ROUTE GATES ARE FOUR DIFFERENT SWITCHES. This console never pretends
# otherwise — see "Route & gate state" in either network menu:
#   1. BACKEND/CONFIG  `[routes]` in the bridge config file.
#   2. LEDGER          `bridge_routes` (schema v24), written only by
#                      `glc-admin robinhood-route-enable/-disable` and read
#                      by `glc-admin robinhood-routes`.
#   3. ADAPTER         chain-adapter capability, decided in the DAEMON's
#                      process by its startup preflight. No file read can
#                      establish it, and nothing here guesses at it.
#   4. CONTRACT        GlcRobinhoodBridge's own `routeEnabled`, read by
#                      `robinhood-preflight`, set only by
#                      `robinhood-governance-route` under a 2-of-3 quorum.
# Goldcoin<->Solana routes use NONE of these as their control: theirs are
# the local ledger pause, admission control, and the on-chain pause flags.
#
# ROBINHOOD 24h LIMITS. The backend policy figure is a STRICT 24h rolling
# limit. The contract's window is a FIXED bucket whose reachable worst case
# is 2x, so the number that belongs on chain is exactly HALF the policy
# figure. glc-admin derives that halving itself, everywhere it matters;
# this console never asks an operator to do the arithmetic and never
# accepts a hand-computed bucket figure.
#
# UNITS. Operators type human values — `6`, `1.5`, `20000`, `10000000`.
# Conversion to canonical 8dp (and onward to Robinhood-native 18dp) happens
# inside glc-admin, via its `--fee-percent` / `--per-transfer-glc` /
# `--rolling-glc` flags. The one place this console cannot offer human
# units is the Solana on-chain `set-limit`, whose values are atomic units
# of the Solana-side mint at that mint's own live decimals — that action
# says so, and says why.
#
# SECRETS. Bearer tokens for the admin control plane, the attestation and
# vault signers, and the Robinhood custody domains are resolved by the
# daemon and by glc-admin from ENVIRONMENT VARIABLES NAMED IN THE CONFIG
# FILE (`token_env`, `auth_token_env`) — docs/26-production-signer-
# deployment.md, docs/09-runbook.md. That is the only mechanism this
# console uses. It never reads a secret, never prompts for one, never puts
# one on a command line, and never writes one to its log.
#
# Usage:
#   scripts/bridge-admin.sh [--config PATH] [--db PATH] [--rpc-url URL]
#   scripts/bridge-admin.sh --help
#
# Exit status: 0 when nothing failed; 1 when any post-change verification
# in the session did not PASS.
#
# Environment:
#   GLC_ADMIN              path to the glc-admin binary (default: PATH,
#                          then service/target/{release,debug}/glc-admin)
#   GLC_BRIDGE_CONFIG      default --config
#   GLC_BRIDGE_DB          default --db
#   GLC_SOLANA_RPC_URL     default --rpc-url (Solana on-chain reads/writes)
#   GLC_BRIDGE_ENV_FILE    a root-owned env file holding signer bearer
#                          tokens. NEVER sourced automatically: the console
#                          asks first, sources it with tracing off and no
#                          echo, and reports only HOW MANY variables it
#                          defined — never a name, never a value.
#   GLC_BRIDGE_ADMIN_LOG   action log (default ~/.glc-bridge-admin.log,
#                          created mode 600; never contains a secret)
#   GLC_ADMIN_OPERATOR     operator identity for the authenticated admin
#                          control plane, if your deployment uses it. Set
#                          it in your shell; this console neither sets it
#                          nor reads its token.

set -euo pipefail
IFS=$' \t\n'

# Prints this file's own header block: every leading `#` comment line after
# the shebang, stopping at the first line that is not one. Read out of the
# file rather than duplicated, so --help cannot drift from the
# documentation above it — and written in pure bash, so asking this console
# what it does needs no external command at all.
print_header() {
    local line first=1
    while IFS= read -r line; do
        if [ "$first" = 1 ]; then first=0; continue; fi
        case "$line" in
            '#')    printf '\n' ;;
            '# '*)  printf '%s\n' "${line#\# }" ;;
            '#'*)   printf '%s\n' "${line#\#}" ;;
            *)      break ;;
        esac
    done < "${BASH_SOURCE[0]}"
}

# `--help` is answered before a temp directory, a trap or a binary search
# exists, so it has no prerequisite and leaves nothing behind.
for __arg in "$@"; do
    if [ "$__arg" = "-h" ] || [ "$__arg" = "--help" ]; then
        print_header
        exit 0
    fi
done
unset __arg

# ------------------------------------------------------------------ temp --
#
# One temp directory, created before anything else can fail, removed by an
# EXIT trap that fires on a normal exit, on an error, and on Ctrl+C alike.
# No other path here writes a temp file, so there is nothing else a signal
# could leave behind.

TMPROOT=""
cleanup() {
    local status=$?
    if [ -n "$TMPROOT" ] && [ -d "$TMPROOT" ]; then
        rm -rf -- "$TMPROOT"
    fi
    return "$status"
}
on_interrupt() {
    printf '\n\nInterrupted. Nothing in progress was executed.\n' >&2
    exit 130
}
trap cleanup EXIT
trap on_interrupt INT
trap on_interrupt TERM
trap on_interrupt HUP

TMPROOT="$(mktemp -d "${TMPDIR:-/tmp}/glc-bridge-admin.XXXXXX")"
chmod 700 "$TMPROOT"

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# --------------------------------------------------------------- display --

BOLD="" DIM="" RESET=""
if [ -t 1 ]; then
    BOLD=$'\033[1m'
    DIM=$'\033[2m'
    RESET=$'\033[0m'
fi

rule() { printf '%s\n' "----------------------------------------------------------------------"; }
hdr() {
    echo
    rule
    printf '%s%s%s\n' "$BOLD" "$1" "$RESET"
    rule
}
say()  { printf '%s\n' "$*"; }
note() { printf '%s%s%s\n' "$DIM" "$*" "$RESET"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
err()  { printf 'error: %s\n' "$*" >&2; }
pass() { printf '[ PASS ] %s\n' "$*"; }
fail() { printf '[ FAIL ] %s\n' "$*"; }

# Reads one line into the variable NAMED by $2. Returns non-zero on EOF so
# the console exits cleanly when stdin closes rather than looping forever.
#
# The locals are deliberately `__ask_`-prefixed: `printf -v "$2"` assigns by
# NAME, so a local sharing the caller's variable name would shadow it and
# the caller would silently read an empty string back. For a fee prompt
# that is the worst possible failure.
ask() {
    local __ask_prompt="$1" __ask_var="$2" __ask_line
    printf '%s' "$__ask_prompt" >&2
    IFS= read -r __ask_line || return 1
    printf -v "$__ask_var" '%s' "$__ask_line"
}

# Typed confirmation. The word must match EXACTLY — no case folding, no
# trimming, no "y" shortcut. Returns 0 only on an exact match.
confirm_typed() {
    local word="$1" reassurance="$2" answer
    echo
    ask "Type ${word} to proceed, anything else to abort: " answer || return 1
    if [ "$answer" != "$word" ]; then
        say "Aborted — nothing was executed. ${reassurance}"
        return 1
    fi
    return 0
}

# ---------------------------------------------------- verification tally --
#
# A post-change verification that does not PASS must make the SESSION fail,
# not merely print a line an operator can scroll past. Every failure is
# counted, listed again at exit, and turns this console's own exit status
# non-zero.

VERIFY_FAILURES=0
VERIFY_FAILED_LABELS=()

record_verify() {
    local label="$1" status="$2" detail="${3:-}"
    if [ "$status" -eq 0 ]; then
        pass "VERIFIED: $label"
        return 0
    fi
    fail "VERIFICATION: $label${detail:+ — $detail}"
    VERIFY_FAILURES=$((VERIFY_FAILURES + 1))
    VERIFY_FAILED_LABELS+=("$label${detail:+ — $detail}")
    return 1
}

finish() {
    if [ "$VERIFY_FAILURES" -gt 0 ]; then
        hdr "SESSION RESULT: FAIL"
        say "$VERIFY_FAILURES post-change verification(s) did not pass:"
        local label
        for label in "${VERIFY_FAILED_LABELS[@]}"; do
            say "  - $label"
        done
        echo
        say "Investigate before treating any of the above changes as complete."
        exit 1
    fi
    exit 0
}

# ------------------------------------------------------------------- log --
#
# One line per glc-admin invocation, and nothing else. The argv this console
# builds never carries a token, a key, a keypair's CONTENTS or any other
# secret — every secret in this system is resolved from an env var named in
# the config, by the callee, and is never passed as an argument — so
# logging the command line verbatim is safe.

LOG="${GLC_BRIDGE_ADMIN_LOG:-${HOME:-/tmp}/.glc-bridge-admin.log}"
LOG_ENABLED=1
init_log() {
    if ! (umask 077; : >> "$LOG") 2>/dev/null; then
        LOG_ENABLED=0
        warn "cannot write the action log at ${LOG} — continuing without it."
    fi
}
log_action() {
    [ "$LOG_ENABLED" -eq 1 ] || return 0
    local label="$1" status="$2"; shift 2
    {
        printf '%s user=%s exit=%s action=%q cmd=%q' \
            "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "${USER:-${LOGNAME:-unknown}}" \
            "$status" "$label" "$GLC_ADMIN_BIN"
        printf ' %q' "$@"
        printf '\n'
    } >> "$LOG" 2>/dev/null || LOG_ENABLED=0
}

# --------------------------------------------------------- glc-admin bin --

find_glc_admin() {
    if [ -n "${GLC_ADMIN:-}" ]; then
        printf '%s' "$GLC_ADMIN"
        return 0
    fi
    if command -v glc-admin >/dev/null 2>&1; then
        command -v glc-admin
        return 0
    fi
    local candidate
    for candidate in \
        "$repo_root/service/target/release/glc-admin" \
        "$repo_root/service/target/debug/glc-admin"; do
        if [ -x "$candidate" ]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

# Which subcommands THIS binary actually implements, read out of its own
# --help rather than assumed. A stale build is a real hazard: the
# route-ledger commands landed on 2026-09-10 and `robinhood-routes` later
# still, so an older binary silently lacks them. Menu items whose command
# is missing say so instead of failing halfway through a guided flow.
ADMIN_HELP=""
probe_admin() {
    ADMIN_HELP="$TMPROOT/glc-admin-help.txt"
    if ! "$GLC_ADMIN_BIN" --help >"$ADMIN_HELP" 2>&1; then
        err "'$GLC_ADMIN_BIN --help' failed — that binary is not usable."
        exit 2
    fi
}
admin_has() {
    grep -qE "^[[:space:]]*glc-admin $1([[:space:]]|\$)" "$ADMIN_HELP"
}
require_cmd() {
    if admin_has "$1"; then
        return 0
    fi
    echo
    fail "'glc-admin $1' is not implemented by this binary."
    say "  Binary: $GLC_ADMIN_BIN"
    say "  This is almost always a STALE BUILD. Rebuild it:"
    say "      cd service && cargo build --release --bin glc-admin"
    say "  or point GLC_ADMIN at the current one. Nothing was run."
    return 1
}

# -------------------------------------------------------- running things --

# Runs glc-admin, showing the exact command first and reporting PASS/FAIL
# after. Nothing here is hidden from the operator: the command line IS the
# audit trail, and it is echoed BEFORE it runs, not after.
run_admin() {
    local label="$1"; shift
    printf '\n%s$ %s' "$DIM" "$GLC_ADMIN_BIN"
    printf ' %q' "$@"
    printf '%s\n\n' "$RESET"
    local status=0
    "$GLC_ADMIN_BIN" "$@" || status=$?
    log_action "$label" "$status" "$@"
    echo
    if [ "$status" -eq 0 ]; then
        pass "$label"
    else
        fail "$label (glc-admin exited $status)"
    fi
    return "$status"
}

# As `run_admin`, but the output is also captured for a verification that
# has to inspect it. Same echo, same PASS/FAIL, same log line.
CAPTURED=""
run_admin_capture() {
    local label="$1"; shift
    CAPTURED="$TMPROOT/captured.txt"
    printf '\n%s$ %s' "$DIM" "$GLC_ADMIN_BIN"
    printf ' %q' "$@"
    printf '%s\n\n' "$RESET"
    local status=0
    "$GLC_ADMIN_BIN" "$@" >"$CAPTURED" 2>&1 || status=$?
    cat "$CAPTURED"
    log_action "$label" "$status" "$@"
    echo
    if [ "$status" -eq 0 ]; then
        pass "$label"
    else
        fail "$label (glc-admin exited $status)"
    fi
    return "$status"
}

# Exact `key = value` test over the last captured output.
#
# Deliberately NOT a regex: one of the keys this checks is literally
# `paused (global)`, whose parentheses are regex metacharacters — a
# `grep -E` for it matches `paused global = true`, which is a verification
# that passes without proving anything. awk compares the key as a STRING,
# and takes the first whitespace-separated token after the `=` so a
# trailing explanatory comment on the value line cannot break it.
captured_field_equals() {
    awk -F'=' -v f="$1" -v w="$2" '
        NF < 2 { next }
        {
            key = $1
            sub(/^[[:space:]]+/, "", key); sub(/[[:space:]]+$/, "", key)
            rest = $2
            sub(/^[[:space:]]+/, "", rest)
            split(rest, parts, /[[:space:]]/)
            if (key == f && parts[1] == w) found = 1
        }
        END { exit !found }
    ' "$CAPTURED"
}

# Quiet variant for machine-readable reads (--porcelain). Prints nothing
# itself; the caller consumes stdout. Never used for a mutation.
admin_read() {
    "$GLC_ADMIN_BIN" "$@" 2>/dev/null
}

# ------------------------------------------------------------ validation --

# Refuses anything that is not a plain readable file. A route gate, a policy
# or a ledger must never be reached through a directory, a device, a fifo or
# a dangling symlink because someone typed a path with a typo in it.
require_regular_file() {
    local path="$1" what="$2"
    if [ ! -e "$path" ]; then
        err "$what: no such file: $path"
        return 1
    fi
    if [ -d "$path" ]; then
        err "$what: that is a directory, not a file: $path"
        return 1
    fi
    if [ ! -f "$path" ]; then
        err "$what: not a regular file (device, socket or fifo): $path"
        return 1
    fi
    if [ ! -r "$path" ]; then
        err "$what: not readable by $(id -un): $path"
        return 1
    fi
    return 0
}

# Operator notes travel to glc-admin as ONE argv element and are never
# interpolated into a shell command, so quoting alone already defeats shell
# injection — there is no `eval` and no unquoted expansion anywhere here.
# What is rejected below is the OTHER injection: control characters and
# terminal escape sequences, which would otherwise be replayed out of the
# audit log and out of this console's own log into somebody's terminal.
validate_note() {
    local note="$1"
    if [ -z "${note//[[:space:]]/}" ]; then
        err "a note is required and must not be blank (mandatory audit trail)."
        return 1
    fi
    if [ "${#note}" -gt 200 ]; then
        err "note is ${#note} characters; keep it under 200."
        return 1
    fi
    if printf '%s' "$note" | LC_ALL=C grep -q '[^[:print:]]'; then
        err "notes must be printable text — no tabs, newlines or escape sequences."
        return 1
    fi
    return 0
}

ask_note() {
    local __note_var="$1" __note_line
    while true; do
        ask $'\nShort note for the audit trail (why, and a ticket if you have one): ' __note_line \
            || return 1
        if validate_note "$__note_line"; then
            printf -v "$__note_var" '%s' "$__note_line"
            return 0
        fi
    done
}

# Strips the thousands separators an operator naturally types: `20,000` and
# `20_000` both mean twenty thousand, and neither is a valid number to hand
# onward.
normalize_number() {
    local raw="$1"
    raw="${raw//,/}"
    raw="${raw//_/}"
    raw="${raw#"${raw%%[![:space:]]*}"}"
    raw="${raw%"${raw##*[![:space:]]}"}"
    printf '%s' "$raw"
}

# A percentage: unsigned decimal, at most TWO fractional digits, strictly
# below 100. A leading `-` fails the pattern, which is how negatives are
# refused — there is no separate sign check to forget.
#
# Two decimal places because a basis point IS one hundredth of a percent:
# `1.37%` is 137 bps exactly, and `1.375%` is not a number of basis points
# at all. Matching glc-admin's own precision here means an operator is told
# so at the prompt rather than after typing a note.
#
# ZERO IS ACCEPTED. A free route is a real commercial choice, `fee = 0` and
# `net = gross` settles end to end, and the reason it used to be refused —
# "a rate the protocol has never charged" — went away with the allowlist.
# It is confirmed separately below, because it is worth being sure about.
#
# 100% and above are refused: at exactly 100% every transfer on that route
# would deliver nothing, and above it the net entitlement would be
# negative.
validate_percent() {
    local value="$1" int frac
    if [[ ! "$value" =~ ^[0-9]{1,3}(\.[0-9]{1,2})?$ ]]; then
        err "not a valid percentage: '${value}'. Type a plain number with at most two decimal places, such as 6, 3, 1.5 or 1.37 — no %, no sign, no exponent."
        return 1
    fi
    int="${value%%.*}"
    frac=""
    [[ "$value" == *.* ]] && frac="${value#*.}"
    if [ "$((10#$int))" -ge 100 ]; then
        err "a fee of ${value}% would leave the user nothing (100%) or less than nothing. The maximum is 99.99%."
        return 1
    fi
    return 0
}

# A GLC amount in human units: unsigned decimal, at most 8 fractional digits
# (the canonical precision), strictly positive. The conversion to canonical
# 8dp atomic units is NOT done here — it is done by glc-admin's own
# --fee-percent/--per-transfer-glc/--rolling-glc flags, which are the one
# implementation of that arithmetic in this repository.
validate_glc() {
    local value="$1" int frac
    if [[ ! "$value" =~ ^[0-9]{1,15}(\.[0-9]{1,8})?$ ]]; then
        err "not a valid GLC amount: '${value}'. Type a plain number such as 20000 or 10000000.5 — at most 8 decimal places, no sign, no exponent."
        return 1
    fi
    int="${value%%.*}"
    frac=""
    [[ "$value" == *.* ]] && frac="${value#*.}"
    if [ "$((10#$int))" -eq 0 ] && [ -z "${frac//0/}" ]; then
        err "amount must be greater than zero."
        return 1
    fi
    return 0
}

# A whole number of atomic units, for the two places glc-admin's own
# interface is atomic: the Solana on-chain `set-limit --value`, and the
# optional 18dp minimum overrides on `robinhood-governance-set-limits`.
validate_u64() {
    local value="$1"
    if [[ ! "$value" =~ ^[0-9]{1,26}$ ]]; then
        err "not a whole number: '${value}'. Atomic units are integers — no sign, no decimal point."
        return 1
    fi
    return 0
}

ask_validated() {
    local prompt="$1" validator="$2" __out_var="$3" raw normalized
    while true; do
        ask "$prompt" raw || return 1
        normalized="$(normalize_number "$raw")"
        if "$validator" "$normalized"; then
            printf -v "$__out_var" '%s' "$normalized"
            return 0
        fi
    done
}

# ------------------------------------------------------------ path state --

CONFIG="${GLC_BRIDGE_CONFIG:-}"
DB="${GLC_BRIDGE_DB:-}"
RPC_URL="${GLC_SOLANA_RPC_URL:-}"
CONFIG_OK=0
DB_OK=0

# Establishes a --config that the REAL parser accepts, before any action
# uses it.
#
# The check is `glc-admin chain-policy-check-config`, not a test written
# here: deciding whether a file is a bridge config means loading it the way
# the daemon loads it, and a shell approximation — grepping for "[solana]",
# say — would be a second set of rules that disagrees with the first the
# moment a section is renamed. It also distinguishes a POLICY FRAGMENT from
# a config, in those words, instead of leaving the parser to say "missing
# field solana" about a file that was never a config.
need_config() {
    [ "$CONFIG_OK" -eq 1 ] && return 0
    require_cmd chain-policy-check-config || return 1
    while true; do
        if [ -z "$CONFIG" ]; then
            echo
            say "The FULL bridge config the daemon loads is needed here"
            note "(typically /etc/glc-bridge/config.toml — NOT a policy fragment)."
            ask "Path to config.toml (blank to cancel): " CONFIG || return 1
            [ -n "$CONFIG" ] || return 1
            continue
        fi
        if ! require_regular_file "$CONFIG" "--config"; then
            CONFIG=""
            continue
        fi
        if run_admin "config check" chain-policy-check-config --config "$CONFIG"; then
            CONFIG="$(cd -- "$(dirname -- "$CONFIG")" && pwd)/$(basename -- "$CONFIG")"
            CONFIG_OK=1
            return 0
        fi
        echo
        say "That file cannot be used — see above. Nothing was read from it beyond"
        say "the check, and nothing anywhere was written."
        CONFIG=""
    done
}

# Establishes a --db that opens as a real bridge ledger.
#
# Probed with `glc-admin status --db PATH`, which is READ-ONLY and is the
# ledger's own opener — so "is this a bridge ledger?" is answered by the
# code that would have to open it anyway, not by sniffing a file header.
need_db() {
    [ "$DB_OK" -eq 1 ] && return 0
    require_cmd status || return 1
    local probe="$TMPROOT/db-probe.txt"
    while true; do
        if [ -z "$DB" ]; then
            echo
            say "The bridge ledger database is needed here"
            note "(typically /var/lib/glc-bridge/ledger.db)."
            ask "Path to ledger.db (blank to cancel): " DB || return 1
            [ -n "$DB" ] || return 1
            continue
        fi
        if ! require_regular_file "$DB" "--db"; then
            DB=""
            continue
        fi
        printf '\n%s$ %s status --db %q  (read-only probe)%s\n' \
            "$DIM" "$GLC_ADMIN_BIN" "$DB" "$RESET"
        if "$GLC_ADMIN_BIN" status --db "$DB" >"$probe" 2>&1; then
            DB="$(cd -- "$(dirname -- "$DB")" && pwd)/$(basename -- "$DB")"
            DB_OK=1
            pass "ledger opens: $DB"
            return 0
        fi
        fail "that path does not open as a bridge ledger:"
        sed 's/^/    /' "$probe"
        DB=""
    done
}

# The Solana RPC endpoint, for the on-chain program reads and the
# admin-gated on-chain commands. Never persisted anywhere by this console.
need_rpc() {
    while true; do
        if [ -n "$RPC_URL" ]; then
            case "$RPC_URL" in
                http://*|https://*) return 0 ;;
                *) err "--rpc-url must be an http(s) URL: '$RPC_URL'"; RPC_URL="" ;;
            esac
            continue
        fi
        echo
        say "A Solana RPC URL is needed for on-chain reads."
        note "It is the same endpoint [solana].rpc_url in the config names."
        ask "Solana RPC URL (blank to cancel): " RPC_URL || return 1
        [ -n "$RPC_URL" ] || return 1
    done
}

# --------------------------------------------------------------- secrets --
#
# The only secret handling this console does, and it does as little as
# possible. Nothing is printed, echoed, logged, or passed as an argument;
# the console never learns a single value.

SECRET_ENV_LOADED=0
signer_secret_notice() {
    say "SIGNER CREDENTIALS"
    say "  A 2-of-3 quorum means contacting three independent custody domains."
    say "  glc-admin authenticates to each with a bearer token it reads from the"
    say "  ENVIRONMENT VARIABLE that config's own auth_token_env field names."
    say "  This console never reads, prints, prompts for, logs or passes any of"
    say "  them. If one is missing, the command below says so and stops."
    if [ -n "${GLC_BRIDGE_ENV_FILE:-}" ] && [ "$SECRET_ENV_LOADED" -eq 0 ]; then
        echo
        say "  GLC_BRIDGE_ENV_FILE is set: $GLC_BRIDGE_ENV_FILE"
        say "  It can be sourced into this shell so the command below inherits those"
        say "  variables. It is NEVER sourced automatically, tracing stays off, and"
        say "  nothing from it is printed — only a count of variables defined."
        local answer
        ask "  Source it now? [y/N]: " answer || return 0
        if [ "$answer" = "y" ] || [ "$answer" = "Y" ]; then
            source_signer_env_file || true
        else
            say "  Not sourced. The command will use whatever is already in the environment."
        fi
    fi
}

# Sources the operator's existing root-owned env file and reports a COUNT.
# `set +x` is asserted immediately beforehand: this console never enables
# xtrace, and this makes that true even if an outer shell exported it.
source_signer_env_file() {
    local file="${GLC_BRIDGE_ENV_FILE:-}"
    [ -n "$file" ] || return 0
    if ! require_regular_file "$file" "GLC_BRIDGE_ENV_FILE"; then
        return 1
    fi
    local before after
    before="$(compgen -e | wc -l)"
    set +x
    set -a
    # shellcheck disable=SC1090
    . "$file"
    set +a
    after="$(compgen -e | wc -l)"
    SECRET_ENV_LOADED=1
    say "  Sourced. $((after - before)) new environment variable(s) defined."
    note "  No name and no value from that file has been printed or logged."
    return 0
}

# ----------------------------------------------------------- route names --

# The ONLY two routes any part of this console may name in a route-enable
# position. One list, not two: the guard below is a WHITELIST, so there is
# no second "never enableable" list that could drift out of step with it.
#
# SolToRhn and RhnToSol are structurally non-executable in this build: no
# Direction exists for either, so an enabled flag would advertise a path
# that cannot move value. They are never offered by a menu, and this guard
# refuses them even if one somehow reached a command builder — ahead of
# glc-admin's own refusal, so the message names the reason rather than an
# argument error.
OPERATOR_ROUTES=(GlcToRhn RhnToGlc)

assert_operator_route() {
    local route="$1" candidate
    for candidate in "${OPERATOR_ROUTES[@]}"; do
        if [ "$route" = "$candidate" ]; then
            return 0
        fi
    done
    err "refusing to build a route command for '${route}'."
    case "$route" in
        SolToRhn|RhnToSol)
            say "  ${route} is structurally NON-EXECUTABLE in this deployment: no settlement"
            say "  machinery exists for it, so enabling it anywhere would advertise a path"
            say "  that cannot move value. This console never offers it and never will."
            ;;
        GlcToSol|SolToGlc)
            say "  ${route}'s controls are the local pause and admission control, not a"
            say "  route flag. A second, divergent switch is exactly what must not exist."
            ;;
        *)
            say "  Only ${OPERATOR_ROUTES[*]} are operator-settable."
            ;;
    esac
    return 1
}

# Menu that offers ONLY the operator-settable routes. There is no code path
# from any menu to SolToRhn or RhnToSol.
choose_operator_route() {
    local __route_var="$1" choice
    echo
    say "Which route?"
    say "  1) GlcToRhn   (Goldcoin -> Robinhood)"
    say "  2) RhnToGlc   (Robinhood -> Goldcoin)"
    say "  3) Cancel"
    note "  SolToRhn and RhnToSol are not listed: they are structurally non-executable"
    note "  in this build and cannot be enabled from here, or by glc-admin, at all."
    ask "Choice: " choice || return 1
    case "$choice" in
        1) printf -v "$__route_var" '%s' "GlcToRhn" ;;
        2) printf -v "$__route_var" '%s' "RhnToGlc" ;;
        *) return 1 ;;
    esac
    assert_operator_route "${!__route_var}"
}

# ----------------------------------------------------------- route state --

# One route's recorded LEDGER-gate value, from `glc-admin robinhood-routes
# --porcelain`. Returns the literal `enabled`, `disabled` or `no-row` —
# never a resolved default, because that command deliberately resolves none.
ledger_route_flag() {
    local route="$1"
    admin_read robinhood-routes --db "$DB" --porcelain \
        | awk -F'\t' -v r="$route" '$1 == "route" && $2 == r { print $3; exit }'
}

# The whole service-side gate picture, from glc-admin — never from sqlite.
# `--config` adds the config gate and the adapter gate's static verdict;
# without it the command says so rather than guessing.
show_route_gates() {
    require_cmd robinhood-routes || return 1
    local -a cmd=(robinhood-routes)
    if [ "$CONFIG_OK" -eq 1 ]; then
        cmd+=(--config "$CONFIG")
    fi
    if [ "$DB_OK" -eq 1 ]; then
        cmd+=(--db "$DB")
    fi
    run_admin "route gates (config + ledger + adapter)" "${cmd[@]}" || true
}

show_contract_route_gate() {
    need_config || return 1
    require_cmd robinhood-preflight || return 1
    echo
    say "CONTRACT GATE — GlcRobinhoodBridge.routeEnabled, read live."
    note "Every check is reported PASS / FAIL / UNVERIFIED. UNVERIFIED is not PASS."
    run_admin "robinhood preflight (contract routes, pauses, signer quorum)" \
        robinhood-preflight --config "$CONFIG" || true
}

robinhood_route_state_screen() {
    hdr "Robinhood route state — FOUR INDEPENDENT SWITCHES"
    cat <<'ROUTEDOC'
A Robinhood route moves value only when ALL of these agree. They are
different switches, changed different ways, and none substitutes for
another. On top of all of them, the contract's pause flags, the 2-of-3
signer quorum, reserve availability and the local ledger pause are still
evaluated on every single request.

  1. CONFIG    [routes] in the bridge config file    (config edit + restart)
  2. LEDGER    bridge_routes                         (robinhood-route-enable)
  3. ADAPTER   chain-adapter capability              (daemon startup preflight)
  4. CONTRACT  GlcRobinhoodBridge.routeEnabled       (2-of-3 governance)
ROUTEDOC
    need_db || return 0
    need_config || true
    show_route_gates
    show_contract_route_gate || true
    echo
    note "Enabling one switch opens nothing on its own."
}

solana_route_state_screen() {
    hdr "Goldcoin <-> Solana route state"
    cat <<'SOLROUTEDOC'
GlcToSol and SolToGlc are NOT controlled by a route flag, on any switch.
Their controls are:

  LOCAL LEDGER PAUSE   per direction, this service's own gate
  ADMISSION CONTROL    Solana -> Goldcoin only: whether a NEWLY observed
                       obligation is admitted or parked to ManualReview
  ON-CHAIN PAUSE       BridgeConfig.paused / release_paused / deposit_paused

They do have a seeded `bridge_routes` row (both enabled) because the v24
migration records every route's state — but that row is not their control
and never becomes one: `robinhood-route-enable` refuses them outright.
SOLROUTEDOC
    need_db || return 0
    show_route_gates
    echo
    run_admin "local pause / admission / reserve state" status --db "$DB" || true
    if [ -n "$RPC_URL" ] && admin_has show-config; then
        run_admin "on-chain pause flags" show-config --rpc-url "$RPC_URL" || true
    else
        echo
        note "On-chain pause flags not read — no --rpc-url selected."
    fi
}

# -------------------------------------------------------- policy helpers --

# The current value of one policy field, in EXACT machine units, straight
# out of glc-admin's porcelain. Used to carry a field across unchanged when
# only one of the three is being edited — never re-derived, never rounded
# through a human representation.
policy_field() {
    local network="$1" key="$2"
    admin_read chain-policy-show --config "$CONFIG" --network "$network" --porcelain \
        | awk -F'\t' -v k="$key" '$1 == k { print $2; exit }'
}

network_is_configurable() {
    admin_read chain-policy-networks --porcelain \
        | awk -F'\t' -v n="$1" '$1 == n && $2 == "configurable" { found = 1 } END { exit !found }'
}

FEE_ARGS=() PER_ARGS=() ROLL_ARGS=()
collect_policy_values() {
    local network="$1" which="$2"
    FEE_ARGS=() PER_ARGS=() ROLL_ARGS=()
    local cur_fee cur_per cur_roll answer
    cur_fee="$(policy_field "$network" fee_bps)"
    cur_per="$(policy_field "$network" per_transfer_limit)"
    cur_roll="$(policy_field "$network" rolling_daily_limit)"

    # The CURRENT values, from glc-admin, before any prompt — human units
    # beside their exact machine units, so an operator never types a
    # replacement for a number they have not just read. Nothing here is a
    # literal: every figure comes out of the config file through the binary.
    echo
    run_admin "current policy ($network)" \
        chain-policy-show --config "$CONFIG" --network "$network" --no-onchain || true

    # The fee is NEVER collected here, and this flow never changes one.
    #
    # Fees are per ROUTE (`[fees]`); this function edits a per-CHAIN policy
    # (`[<chain>.policy]`), whose `fee_bps` no longer prices anything once
    # `[fees]` exists. A fee prompt in this flow would ask one question and
    # change something else. `change_route_fee` is the fee path.
    #
    # The chain policy's existing fee_bps is carried across byte-identical,
    # because `chain-policy-apply` requires all three values and this flow
    # must not move the one it was not asked about.
    if [ "$which" = fee ]; then
        err "this flow changes LIMITS, not fees — use \"Change one route's fee %\"."
        return 1
    fi
    if [ -z "$cur_fee" ]; then
        err "no fee_bps is stated for ${network} yet — use 'Apply backend policy' instead."
        return 1
    fi
    FEE_ARGS=(--fee-bps "$cur_fee")

    if [ "$which" = per ] || [ "$which" = all ]; then
        echo
        say "Changing: PER-TRANSFER LIMIT"
        note "Typed in GLC. glc-admin converts it to canonical 8dp atomic units."
        ask_validated "New per-transfer limit [GLC]: " validate_glc answer || return 1
        PER_ARGS=(--per-transfer-glc "$answer")
    else
        if [ -z "$cur_per" ]; then
            err "no per-transfer limit is configured for ${network} yet — use 'Apply backend policy'."
            return 1
        fi
        PER_ARGS=(--per-transfer-limit "$cur_per")
    fi

    if [ "$which" = roll ] || [ "$which" = all ]; then
        echo
        say "Changing: STRICT 24h ROLLING LIMIT"
        if [ "$network" = robinhood ]; then
            say "  This is the STRICT 24h policy figure — the backend number."
            say "  The GlcRobinhoodBridge contract's window is a FIXED bucket whose"
            say "  reachable worst case is 2x, so the number that belongs ON CHAIN is"
            say "  exactly HALF of this. glc-admin derives that halving itself and"
            say "  prints both figures below."
            say "  Enter the STRICT policy figure. Do NOT halve it yourself."
        fi
        ask_validated "New strict 24h limit [GLC]: " validate_glc answer || return 1
        ROLL_ARGS=(--rolling-glc "$answer")
    else
        if [ -z "$cur_roll" ]; then
            err "no rolling limit is configured for ${network} yet — use 'Apply backend policy'."
            return 1
        fi
        ROLL_ARGS=(--rolling-daily-limit "$cur_roll")
    fi
    return 0
}

show_policy() {
    local network="$1"
    need_config || return 0
    require_cmd chain-policy-show || return 0
    run_admin "chain policy ($network)" \
        chain-policy-show --config "$CONFIG" --network "$network" || true
    echo
    note "FEES are not shown above as a single per-network number any more: they are"
    note "per ROUTE, in [fees]. Use 'Show per-route fees' for them."
    if [ "$network" = solana ]; then
        note "Solana's transfer ceilings live in the on-chain program's config account"
        note "and are changed with set-limit; its FEES are ordinary config values like"
        note "every other route's."
    fi
}

validate_policy_only() {
    local network="$1"
    need_config || return 0
    require_cmd chain-policy-validate || return 0
    if ! network_is_configurable "$network"; then
        echo
        say "${network}'s policy is not configurable in this config file. glc-admin's"
        say "own answer, including where its fee and limits DO live:"
        show_policy "$network"
        return 0
    fi
    collect_policy_values "$network" all || return 0
    hdr "Validate only — this writes NOTHING, ever"
    run_admin "validate proposed ${network} policy" \
        chain-policy-validate --config "$CONFIG" --network "$network" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}" || true
}

# Reads the backend policy against the deployed contract and turns
# glc-admin's own verdict into a PASS/FAIL.
#
# `chain-policy-show` prints `MATCH —` when the contract enforces exactly
# the configured policy, `MISMATCH —` with each disagreement named, or
# `UNAVAILABLE` when it could not read. UNVERIFIED IS NOT A PASS: this
# repository's own preflight doctrine, applied here.
verify_policy_vs_contract() {
    local label="backend policy and deployed contract agree"
    if ! run_admin_capture "read backend policy vs contract" \
        chain-policy-show --config "$CONFIG" --network robinhood; then
        record_verify "$label" 1 "the comparison read itself failed" || true
        return 1
    fi
    # MISMATCH is tested FIRST: it contains "MATCH" as a substring, and a
    # test that looked for MATCH first would report a mismatch as a pass.
    if grep -qE '^[[:space:]]+MISMATCH' "$CAPTURED"; then
        record_verify "$label" 1 "the contract does not enforce the configured policy" || true
        return 1
    fi
    if grep -qE '^[[:space:]]+MATCH — ' "$CAPTURED"; then
        record_verify "$label" 0
        return 0
    fi
    record_verify "$label" 1 \
        "UNVERIFIED — the contract's limits could not be compared; UNVERIFIED is not PASS" || true
    return 1
}

# Re-reads the ledger gate and proves it holds what was just written.
verify_ledger_route() {
    local route="$1" expected="$2" actual
    actual="$(ledger_route_flag "$route")"
    if [ "$actual" = "$expected" ]; then
        record_verify "ledger gate for ${route} is ${expected}" 0
        return 0
    fi
    record_verify "ledger gate for ${route} is ${expected}" 1 \
        "the ledger reports '${actual:-<no reading>}'" || true
    return 1
}

# ---------------------------------------------------------- route fees --
#
# Fees are per ROUTE, not per network: `[fees]` in the config states one
# rate for each executable route, and every quote, request and fold prices
# from that route's own entry. So the fee flow asks WHICH ROUTE first, and
# changes exactly that one — `glc-admin fees-set` proves the other three
# did not move by reloading the edited file and comparing, and refuses the
# edit if any of them did.

# The routes that can be priced, read from glc-admin rather than listed
# here, so a future executable route appears in this menu with no edit.
fee_routes() {
    admin_read fees-show --config "$CONFIG" --porcelain \
        | awk -F'\t' '$1 == "fee" { print $2 }'
}

fee_bps_for_route() {
    local route="$1"
    admin_read fees-show --config "$CONFIG" --route "$route" --porcelain \
        | awk -F'\t' -v r="$route" '$1 == "fee" && $2 == r { print $3; exit }'
}

# Route-first: an operator picks the route, THEN sees that route's current
# fee, THEN types a replacement.
choose_fee_route() {
    local __route_var="$1" choice i=0
    local -a routes=()
    while IFS= read -r line; do
        [ -n "$line" ] && routes+=("$line")
    done < <(fee_routes)
    if [ "${#routes[@]}" -eq 0 ]; then
        err "glc-admin fees-show reported no priceable routes."
        return 1
    fi
    echo
    say "Which ROUTE's fee?"
    note "Fees are per route. Changing one never changes another."
    for i in "${!routes[@]}"; do
        printf '  %d) %-9s  currently %s\n' \
            "$((i + 1))" "${routes[$i]}" "$(fee_percent_for_route "${routes[$i]}")"
    done
    printf '  %d) Cancel\n' "$(( ${#routes[@]} + 1 ))"
    note "  SolToRhn and RhnToSol are absent because neither can be priced:"
    note "  neither can move value, so a fee for either would be a price on a"
    note "  path that cannot carry one. glc-admin refuses them."
    ask "Choice: " choice || return 1
    case "$choice" in
        ''|*[!0-9]*) return 1 ;;
    esac
    if [ "$choice" -ge 1 ] && [ "$choice" -le "${#routes[@]}" ]; then
        printf -v "$__route_var" '%s' "${routes[$((choice - 1))]}"
        return 0
    fi
    return 1
}

fee_percent_for_route() {
    admin_read fees-show --config "$CONFIG" --route "$1" --porcelain \
        | awk -F'\t' '$1 == "fee" { print $4; exit }'
}

show_route_fees() {
    need_config || return 0
    require_cmd fees-show || return 0
    run_admin "per-route fees" fees-show --config "$CONFIG" || true
}

# Route -> current -> new -> validate -> dry run -> exact command -> typed
# confirmation -> apply -> re-read -> PASS/FAIL.
change_route_fee() {
    local route note_text answer before after
    need_config || return 0
    require_cmd fees-show || return 0
    require_cmd fees-set || return 0

    hdr "Step A — current state"
    run_admin "per-route fees" fees-show --config "$CONFIG" || true

    # Snapshot every route BEFORE the edit, so "nothing else moved" is
    # verified against a reading taken here rather than trusted.
    declare -gA FEE_SNAPSHOT=()
    local snap
    while IFS= read -r snap; do
        [ -n "$snap" ] || continue
        FEE_SNAPSHOT["$snap"]="$(fee_bps_for_route "$snap")"
    done < <(fee_routes)

    choose_fee_route route || return 0
    before="$(fee_bps_for_route "$route")"
    if [ -z "$before" ]; then
        err "could not read ${route}'s current fee."
        return 0
    fi

    echo
    say "Route:       ${route}"
    say "Current fee: $(fee_percent_for_route "$route")  (${before} bps)"
    note "Any rate from 0% to 99.99% is valid — a fee is configuration, and"
    note "changing one never rebuilds a binary. Two decimal places at most:"
    note "a basis point is one hundredth of a percent, so 1.37% is 137 bps."
    note "glc-admin does the conversion; this console does no fee arithmetic."
    ask_validated "New fee [%]: " validate_percent answer || return 0

    # 0% is valid and deliberate, and it is also the single easiest thing
    # to type by accident. Confirmed separately, in its own words.
    case "$answer" in
        0|0.0|0.00)
            echo
            warn "A fee of 0% makes ${route} FREE: every transfer on it will deliver the"
            warn "full gross amount and the bridge will collect nothing on that route."
            confirm_typed FREE "The fee is unchanged." || return 0
            ;;
    esac

    ask_note note_text || return 0

    hdr "Step B/C — dry run (before -> after, and every other route re-read)"
    if ! run_admin "dry-run ${route} fee change" \
        fees-set --config "$CONFIG" --route "$route" --fee-percent "$answer" \
        --note "$note_text"; then
        say "Rejected. Nothing was changed."
        return 0
    fi

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s fees-set --config %q --route %q --fee-percent %q --note %q --execute\n' \
        "$GLC_ADMIN_BIN" "$CONFIG" "$route" "$answer" "$note_text"
    cat <<FEEDOC

This changes ONE key in one file: [fees].${route}.

glc-admin reloads the edited file with the real config parser and compares
EVERY other route's resolved rate against what it was — if any of them
moved, the edit is refused rather than installed. A timestamped backup is
taken first, and the file is replaced with one atomic rename, so every
comment and unrelated section survives byte-for-byte.

It will NOT restart the daemon, NOT change any other route's fee, and NOT
send any on-chain transaction: the GlcRobinhoodBridge contract stores no
fee, so there is nothing on chain to reconcile.

In-flight requests are unaffected — each one snapshotted its rate when it
was created and settles at that rate.
FEEDOC
    confirm_typed APPLY "The config file is untouched." || return 0

    hdr "Step F — apply"
    if ! run_admin_capture "apply ${route} fee change" \
        fees-set --config "$CONFIG" --route "$route" --fee-percent "$answer" \
        --note "$note_text" --execute; then
        record_verify "${route} fee change" 1 "the write itself failed" || true
        return 0
    fi
    local backup
    backup="$(awk -F': +' '/^  Backup:/ { print $2; exit }' "$CAPTURED")"
    if [ -n "$backup" ]; then
        record_verify "a timestamped config backup was taken" 0
        say "  Backup: $backup"
    else
        record_verify "a timestamped config backup was taken" 1 \
            "glc-admin reported no backup path" || true
    fi

    hdr "Step G/H — post-change verification"
    run_admin "re-read per-route fees" fees-show --config "$CONFIG" || true
    after="$(fee_bps_for_route "$route")"
    if [ -n "$after" ] && [ "$after" != "$before" ]; then
        record_verify "${route} now prices at ${after} bps" 0
    elif [ "$after" = "$before" ]; then
        record_verify "${route}'s fee changed" 1 \
            "the file still reports ${before} bps" || true
    else
        record_verify "${route}'s fee changed" 1 "the re-read produced no value" || true
    fi

    # The promise this whole flow rests on, checked rather than asserted.
    local other other_before other_after unchanged=0
    while IFS= read -r other; do
        [ -n "$other" ] || continue
        [ "$other" = "$route" ] && continue
        other_after="$(fee_bps_for_route "$other")"
        other_before="${FEE_SNAPSHOT[$other]:-}"
        if [ -n "$other_before" ] && [ "$other_before" != "$other_after" ]; then
            record_verify "${other}'s fee was left alone" 1 \
                "it moved from ${other_before} to ${other_after}" || true
            unchanged=1
        fi
    done < <(fee_routes)
    if [ "$unchanged" -eq 0 ]; then
        record_verify "every other route's fee was left alone" 0
    fi

    restart_notice
}

# Validate -> dry run -> exact command -> typed confirmation -> apply ->
# re-read -> PASS/FAIL. Any failure stops there, and nothing is written
# unless the operator types APPLY after seeing the exact before/after diff.
change_policy() {
    local network="$1" which="$2" note_text
    need_config || return 0
    require_cmd chain-policy-apply || return 0

    if ! network_is_configurable "$network"; then
        echo
        say "${network}'s backend policy cannot be changed in this config file."
        say "glc-admin's own answer, including where its fee and limits DO live:"
        show_policy "$network"
        if [ "$network" = solana ]; then
            echo
            say "For the Solana transfer ceilings use this menu's 'On-chain limits'"
            say "action (glc-admin set-limit), which is the mechanism glc-admin names."
        fi
        return 0
    fi

    collect_policy_values "$network" "$which" || return 0

    hdr "Step A/B — current and proposed state, validated (writes nothing)"
    if ! run_admin "validate ${network} policy" \
        chain-policy-validate --config "$CONFIG" --network "$network" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}"; then
        say "Rejected. Nothing was changed."
        return 0
    fi

    ask_note note_text || return 0

    hdr "Step C — dry run (exact before/after diff; writes nothing)"
    if ! run_admin "dry-run ${network} policy apply" \
        chain-policy-apply --config "$CONFIG" --network "$network" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}" \
        --note "$note_text" --dry-run; then
        say "Dry run failed. Nothing was changed."
        return 0
    fi

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s chain-policy-apply --config %q --network %q' \
        "$GLC_ADMIN_BIN" "$CONFIG" "$network"
    printf ' %q' "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}"
    printf ' --note %q --execute\n' "$note_text"
    cat <<POLICYDOC

This rewrites exactly one file:  $CONFIG

glc-admin edits it as a DOCUMENT, with a comment- and layout-preserving
TOML editor, setting three keys — fee_bps, per_transfer_limit,
rolling_daily_limit — and nothing else. It is not sed, and it does not
re-serialise a parsed struct, so every comment and every unrelated section
survives byte-for-byte. The diff shown above is the whole change, and it
contains no secret: these three fields are the only ones touched.

Before writing it takes a TIMESTAMPED BACKUP, validates a candidate file
with the real config parser, and installs it with one atomic rename — the
config is never partially written, and its permissions are preserved.

It will NOT restart the daemon, NOT enable any route, NOT read any secret,
and NOT send any on-chain transaction.
POLICYDOC
    confirm_typed APPLY "The config file is untouched." || return 0

    hdr "Step F — apply"
    if ! run_admin_capture "apply ${network} policy" \
        chain-policy-apply --config "$CONFIG" --network "$network" \
        "${FEE_ARGS[@]}" "${PER_ARGS[@]}" "${ROLL_ARGS[@]}" \
        --note "$note_text" --execute; then
        record_verify "${network} policy apply" 1 "the write itself failed" || true
        return 0
    fi
    local backup
    backup="$(awk -F': +' '/^  Backup:/ { print $2; exit }' "$CAPTURED")"
    if [ -n "$backup" ]; then
        record_verify "a timestamped config backup was taken" 0
        say "  Backup: $backup"
    else
        record_verify "a timestamped config backup was taken" 1 \
            "glc-admin reported no backup path" || true
    fi

    hdr "Step G/H — post-change verification"
    if run_admin "re-read ${network} policy from the config" \
        chain-policy-show --config "$CONFIG" --network "$network" --no-onchain; then
        record_verify "the config file now states the new ${network} policy" 0
    else
        record_verify "the config file now states the new ${network} policy" 1 \
            "the re-read failed" || true
    fi

    restart_notice

    if [ "$network" = robinhood ]; then
        robinhood_post_policy_reconcile
    fi
}

# After a Robinhood policy change the CONTRACT still holds its old limits.
# Reconciling is a separate 2-of-3 governance action, offered here and never
# performed as a side effect.
robinhood_post_policy_reconcile() {
    hdr "The CONTRACT still holds its previous limits"
    cat <<'RECONDOC'
The backend config now states the new policy. The deployed contract does
not yet enforce it — that is a separate setLimits(...) governance action
under a 2-of-3 quorum, and nothing has proposed, signed or sent one.

Until it is done the backend and the chain disagree, which the read below
reports as a MISMATCH. That is expected at this exact point, and it is
also why this console counts it: an operator who stops here has not
finished the change.
RECONDOC
    echo
    verify_policy_vs_contract || true
    local answer
    ask $'\nReconcile the contract to this policy now? [y/N]: ' answer || return 0
    if [ "$answer" = "y" ] || [ "$answer" = "Y" ]; then
        governance_set_limits
    else
        say "Not reconciled. Run 'Reconcile contract limits to the backend policy'"
        say "from the Robinhood menu when you are ready."
    fi
}

restart_notice() {
    hdr "A DAEMON RESTART IS REQUIRED — this console will not do it"
    cat <<'RESTARTDOC'
The running glc-bridge-daemon has NOT been restarted and has NOT reloaded
anything. It still holds the PREVIOUS policy until an operator restarts it
deliberately, using this deployment's own procedure.

This console never restarts, reloads or signals the daemon, and does not
offer to: this repository documents no service unit name, so inventing one
here would be a guess executed with privilege. Schedule the restart
yourself, at a moment you choose.

Ledger route changes are the exception and need NO restart — nothing caches
them, and the gate re-reads on every request.
RESTARTDOC
}

# ------------------------------------------------------------ governance --

# The shared spine of every state-changing Robinhood governance action.
#
# Steps A and B (current state, proposed state) are the caller's read plus
# the dry run's own BEFORE/AFTER block. C-H are here:
#   C. dry run, always, first
#   D. the exact command, shown
#   E. typed EXECUTE confirmation
#   F. the same command with --execute
#   G. the resulting transaction hash / status (printed by glc-admin)
#   H. a post-change verification read, through a DIFFERENT command, so the
#      two cannot agree by construction
governance_flow() {
    local label="$1" postcheck="$2"; shift 2
    local -a cmd=("$@")

    hdr "Step C — DRY RUN (no custody domain contacted, no nonce consumed)"
    if ! run_admin "$label (dry run)" "${cmd[@]}"; then
        echo
        say "Dry run failed. No signature was gathered, no transaction was built or"
        say "sent, and the governance nonce was not consumed."
        return 1
    fi

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run, with --execute appended:"
    printf '  %s' "$GLC_ADMIN_BIN"
    printf ' %q' "${cmd[@]}"
    printf ' --execute\n'
    cat <<'GOVDOC'

Read the BEFORE / AFTER block above. If it is not exactly what you intend,
abort now.

Proceeding asks a 2-of-3 quorum of the PRODUCTION custody domains to sign
this exact digest, then simulates and broadcasts the transaction.

  - This tool holds no authorization key and cannot manufacture one.
  - A dev signer set is refused outright.
  - The 2-of-3 threshold is never reduced.
  - The payload is one of the three governance actions glc-admin
    implements; there is no arbitrary-calldata and no blind-signing path.
  - No route is enabled as a side effect of this action.
  - No config file is edited and no daemon is restarted.
GOVDOC
    echo
    signer_secret_notice
    confirm_typed EXECUTE "No quorum was contacted." || return 1

    hdr "Step F — execute"
    if ! run_admin "$label (execute)" "${cmd[@]}" --execute; then
        echo
        say "The governance action did not complete. Read the error above before"
        say "retrying: an operation that broadcast but did not confirm must be"
        say "resolved, not re-proposed blindly."
        record_verify "$label" 1 "the governance action did not complete" || true
        return 1
    fi

    hdr "Step G — transaction and status"
    say "Printed by glc-admin above: the transaction hash, gas used, and the"
    say "signers that formed the quorum. glc-admin re-read the contract after the"
    say "receipt and proved it holds exactly what the proposal said."

    hdr "Step H — post-change verification (independent read)"
    case "$postcheck" in
        policy)          verify_policy_vs_contract || true ;;
        preflight)
            if run_admin "post-change preflight" robinhood-preflight --config "$CONFIG"; then
                record_verify "contract preflight passes after the change" 0
            else
                record_verify "contract preflight passes after the change" 1 \
                    "one or more preflight checks FAILED" || true
            fi
            ;;
        preflight-routes) verify_contract_routes ;;
        *)               note "No independent read applies to this action." ;;
    esac
    return 0
}

# Verifies the CONTRACT's route flags against what the operator states they
# should now be. `--expect-route-enabled` makes preflight FAIL on any
# disagreement, so this is a real check rather than a printout.
verify_contract_routes() {
    local choice expect=""
    echo
    say "Which Robinhood routes should be OPEN on the contract now?"
    say "  1) GlcToRhn only"
    say "  2) RhnToGlc only"
    say "  3) Both"
    say "  4) Neither"
    say "  5) Skip this check"
    note "  preflight FAILS on any disagreement with your answer, so an unexpectedly"
    note "  open route is caught rather than merely printed."
    ask "Choice: " choice || return 0
    case "$choice" in
        1) expect="GlcToRhn" ;;
        2) expect="RhnToGlc" ;;
        3) expect="GlcToRhn,RhnToGlc" ;;
        4) expect="" ;;
        *) note "Skipped. The contract's flags were not verified against an expectation."
           return 0 ;;
    esac
    local -a cmd=(robinhood-preflight --config "$CONFIG")
    if [ -n "$expect" ]; then
        cmd+=(--expect-route-enabled "$expect")
    fi
    if run_admin "verify contract route flags" "${cmd[@]}"; then
        record_verify "contract route flags match the stated expectation" 0
    else
        record_verify "contract route flags match the stated expectation" 1 \
            "preflight FAILED" || true
    fi
}

governance_set_limits() {
    need_config || return 0
    require_cmd robinhood-governance-set-limits || return 0

    hdr "Step A — current state"
    say "The backend policy, the contract's live limits, and every disagreement"
    say "between them. The contract's fixed rolling bucket is derived by glc-admin"
    say "as exactly HALF the strict 24h policy — you are never asked to compute it."
    run_admin "current Robinhood policy vs. contract" \
        chain-policy-show --config "$CONFIG" --network robinhood || true

    hdr "Step B — proposed state"
    cat <<'SETLIMITSDOC'
This action has NO figures of its own. It reconciles the contract to
whatever [robinhood.policy] in the config file currently says:

    inboundMax / outboundMax                   = per_transfer_limit
    inboundRollingLimit / outboundRollingLimit = rolling_daily_limit / 2

To propose DIFFERENT numbers, change the policy first (this menu's
'Change ...' actions), then come back here. The minimums and
protectedMinReserve are PRESERVED from current on-chain state unless you
override one below.
SETLIMITSDOC
    local -a extra=()
    local answer value flagname
    ask $'\nOverride any 18dp minimum (inbound / outbound / protected)? [y/N]: ' answer || return 0
    if [ "$answer" = "y" ] || [ "$answer" = "Y" ]; then
        note "These three are the one place glc-admin's own interface is 18dp atomic."
        for flagname in --inbound-min --outbound-min --protected-min; do
            ask "  ${flagname} (18dp atomic units, blank to leave unchanged): " value || return 0
            value="$(normalize_number "$value")"
            if [ -n "$value" ]; then
                validate_u64 "$value" || return 0
                extra+=("$flagname" "$value")
            fi
        done
    fi

    local note_text
    ask_note note_text || return 0

    governance_flow "Robinhood governance: setLimits" policy \
        robinhood-governance-set-limits --config "$CONFIG" --note "$note_text" "${extra[@]}"
}

governance_pause() {
    need_config || return 0
    require_cmd robinhood-governance-pause || return 0

    local scope paused choice
    echo
    say "Which direction's CONTRACT pause flag?"
    note "This is the contract's own flag — NOT the local ledger pause and NOT"
    note "admission control. All three are separate and none implies another."
    say "  1) deposits   (inbound to the contract)"
    say "  2) payouts    (outbound from the contract)"
    say "  3) Cancel"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) scope=deposits ;;
        2) scope=payouts ;;
        *) return 0 ;;
    esac
    echo
    say "Set ${scope}Paused to:"
    say "  1) true    (PAUSE ${scope})"
    say "  2) false   (RESUME ${scope})"
    say "  3) Cancel"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) paused=true ;;
        2) paused=false ;;
        *) return 0 ;;
    esac

    hdr "Step A — current state"
    say "Both pause flags, live from the contract, plus every other preflight check."
    run_admin "current contract state" robinhood-preflight --config "$CONFIG" || true

    hdr "Step B — proposed state"
    say "  ${scope}Paused -> ${paused}"
    say "  The other direction's flag is carried across UNCHANGED from the chain's"
    say "  current value — glc-admin reads it rather than defaulting it."
    if [ "$paused" = false ]; then
        echo
        say "  Clearing a pause ENABLES NO ROUTE. A route governance never enabled"
        say "  stays closed with both directions unpaused."
    fi

    local note_text
    ask_note note_text || return 0

    governance_flow "Robinhood governance: setPaused(${scope}=${paused})" preflight \
        robinhood-governance-pause --config "$CONFIG" \
        --scope "$scope" --paused "$paused" --note "$note_text"
}

governance_route() {
    need_config || return 0
    require_cmd robinhood-governance-route || return 0

    local route enabled choice
    choose_operator_route route || return 0
    assert_operator_route "$route" || return 0

    echo
    say "Set the CONTRACT's routeEnabled flag for ${route} to:"
    say "  1) true    (open the route on chain)"
    say "  2) false   (close the route on chain)"
    say "  3) Cancel"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) enabled=true ;;
        2) enabled=false ;;
        *) return 0 ;;
    esac

    hdr "Step A — current state (every switch)"
    if [ "$DB_OK" -eq 1 ] || [ -n "$DB" ]; then
        need_db && show_route_gates || true
    else
        note "Ledger gate not read — no --db selected."
    fi
    show_contract_route_gate || true

    hdr "Step B — proposed state"
    say "  CONTRACT routeEnabled[${route}] -> ${enabled}"
    echo
    say "  This changes the CONTRACT switch ONLY. The config gate, the ledger gate"
    say "  and the adapter gate are untouched, and the route stays closed unless"
    say "  every one of them agrees."
    if [ "$enabled" = true ]; then
        say "  Enabling a route here does NOT unpause anything."
    fi

    local note_text
    ask_note note_text || return 0

    governance_flow "Robinhood governance: setRouteEnabled(${route}=${enabled})" preflight-routes \
        robinhood-governance-route --config "$CONFIG" \
        --route "$route" --enabled "$enabled" --note "$note_text"
}

# --------------------------------------------------- ledger route state --

ledger_route_change() {
    local enable="$1" verb cmd route note_text expected
    if [ "$enable" = true ]; then
        verb="ENABLE"; cmd="robinhood-route-enable"; expected="enabled"
    else
        verb="DISABLE"; cmd="robinhood-route-disable"; expected="disabled"
    fi
    # Capability first, paths second: a binary that cannot do this should
    # say so before it asks an operator for a ledger path.
    require_cmd "$cmd" || return 0
    require_cmd robinhood-routes || return 0
    need_db || return 0

    choose_operator_route route || return 0
    # Second, independent refusal before the command is built. A menu can be
    # edited; this cannot be reached past.
    assert_operator_route "$route" || return 0

    hdr "Step A — current state"
    show_route_gates

    hdr "Step B — proposed state"
    say "  LEDGER bridge_routes[${route}].enabled -> ${enable}"
    echo
    say "  This changes the LEDGER switch ONLY — this service's own flag. It"
    say "  contacts no chain, reads no config, loads no keypair and touches no"
    say "  secret. Enabling here is NECESSARY and NOWHERE NEAR SUFFICIENT: the"
    say "  config gate, the adapter gate, the contract's routeEnabled and pause"
    say "  flags, preflight, the signer quorum, reserve availability and the local"
    say "  pause each still decide every transfer independently."
    echo
    say "  It takes effect on the NEXT REQUEST. Nothing caches it, so no daemon"
    say "  restart is needed in either direction."

    hdr "Step C — no dry run exists for this command"
    say "glc-admin's route-ledger commands have no --dry-run mode: the write is a"
    say "single audited boolean, and its refusals are audited too. That is stated"
    say "here rather than faked with a pretend preview."
    if [ "$enable" = true ]; then
        echo
        say "Before opening a route in ledger state, consider a fresh ledger backup:"
        say "    scripts/backup-ledger.sh <db path> <backup dir>"
        say "This console will not run it for you."
    fi

    ask_note note_text || return 0

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s %s --db %q --route %q --note %q\n' \
        "$GLC_ADMIN_BIN" "$cmd" "$DB" "$route" "$note_text"
    confirm_typed EXECUTE "The ledger is untouched." || return 0

    hdr "Step F — execute"
    if ! run_admin "ledger route ${verb} ${route}" \
        "$cmd" --db "$DB" --route "$route" --note "$note_text"; then
        record_verify "ledger route ${verb} ${route}" 1 "the write failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    show_route_gates
    verify_ledger_route "$route" "$expected" || true
    echo
    say "The other switches are UNCHANGED. Check them before announcing the route"
    say "as open: Robinhood menu -> 'Route & gate state'."
}

# ------------------------------------------------------- rolling windows --

# Goldcoin<->Solana: an official, implemented reset EXISTS.
solana_reset_rolling_window() {
    need_rpc || return 0
    require_cmd reset-rolling-window || return 0
    local choice direction keypair note_text
    echo
    say "Reset a 24h rolling-volume window (Solana program)."
    say "An ADMINISTRATIVE OVERRIDE of the anti-drain protection. This is an"
    say "official, implemented glc-admin command — not something invented here."
    echo
    say "What it resets, and nothing else:"
    say "  used volume     -> 0"
    say "  remaining       -> the full configured rolling_volume_limit"
    say "  quota_exhausted -> false"
    say "It touches NO reserve balance, NO obligation, NO limit, and NOT the other"
    say "direction's window."
    echo
    say "  1) glc-to-sol   (resets the RELEASE window)"
    say "  2) sol-to-glc   (resets the DEPOSIT window)"
    say "  3) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) direction=glc-to-sol ;;
        2) direction=sol-to-glc ;;
        *) return 0 ;;
    esac

    hdr "Step A — current usage / window state"
    say "Both directions' windows, live, with remaining quota and the global pause"
    say "flag this command requires."
    require_cmd show-config && run_admin "on-chain bridge config + rolling windows" \
        show-config --rpc-url "$RPC_URL" || true

    hdr "Step B — proposed state"
    say "  reset-rolling-window --direction ${direction}"
    echo
    warn "glc-admin refuses this ON CHAIN unless BridgeConfig.paused is ALREADY true."
    say "Global pause first, then this — docs/09-runbook.md's maintenance sequence."
    say "This console will not pause for you: that is a separate, deliberate action"
    say "in this same menu."

    hdr "Step C — no dry run exists for this command"
    say "It is one admin-gated instruction. Stated here rather than faked."

    ask_keypair_path keypair || return 0
    ask_note note_text || return 0

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s reset-rolling-window --rpc-url %q --keypair %q --direction %q --note %q\n' \
        "$GLC_ADMIN_BIN" "$RPC_URL" "$keypair" "$direction" "$note_text"
    note "  The keypair is a PATH. This console never reads its contents."
    confirm_typed EXECUTE "Nothing was signed or sent." || return 0

    hdr "Step F — execute"
    if ! run_admin "reset-rolling-window ${direction}" reset-rolling-window \
        --rpc-url "$RPC_URL" --keypair "$keypair" --direction "$direction" \
        --note "$note_text"; then
        record_verify "reset-rolling-window ${direction}" 1 "the command failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    if run_admin "on-chain rolling windows after reset" show-config --rpc-url "$RPC_URL"; then
        record_verify "the on-chain window state was re-read after the reset" 0
        note "Compare the ${direction} window's remaining quota above against the"
        note "configured rolling_volume_limit: a successful reset restores it in full."
    else
        record_verify "the on-chain window state was re-read after the reset" 1 \
            "the re-read failed" || true
    fi
}

# Robinhood: NO supported reset exists. This says so, with the evidence, and
# offers nothing.
robinhood_rolling_window_reset() {
    hdr "Reset the Robinhood contract rolling bucket — NOT SUPPORTED"
    cat <<'NORESETDOC'
NOT SUPPORTED BY THE CURRENT IMPLEMENTATION.

There is no reset mechanism for the GlcRobinhoodBridge rolling buckets, and
this console will not manufacture one.

The evidence, from the deployed contract's own source:

  - The accumulators are `_inboundWindow` / `_outboundWindow`, declared
    PRIVATE. The only external surface is the two VIEW functions
    `inboundWindow()` / `outboundWindow()`. There is no setter.
  - The contract's complete governance action set is ACTION_SET_PAUSE
    (0x04), ACTION_ROTATE_SIGNERS (0x05), ACTION_ROTATE_GUARDIANS (0x06),
    ACTION_SET_LIMITS (0x07), ACTION_COMMIT_MIGRATION (0x08),
    ACTION_FINALIZE_MIGRATION (0x09), ACTION_ABANDON (0x0A) and
    ACTION_SET_ROUTE_ENABLED (0x0B). None of them resets a window.
    glc-admin implements three of them — set-limits, set-pause,
    set-route-enabled — and no other.
  - `_consumeWindow` advances `windowStart` only when
    `now - windowStart >= ROLLING_WINDOW_SECONDS` (24 hours). Time is the
    only thing that empties a bucket.

WHAT THIS CONSOLE WILL NOT DO INSTEAD

  - It will NOT write contract storage directly. It cannot, and would not.
  - It will NOT fake a reset by raising inboundRollingLimit /
    outboundRollingLimit and lowering them again. That is a real governance
    change to the enforced limit, under a real 2-of-3 quorum, leaving a
    window of genuinely weakened protection and an audit trail that says
    the limit was raised. It is not a reset and must not be used as one.
  - It will NOT touch the ledger to compensate.

WHAT YOU CAN DO

  - Wait. The bucket empties wholesale at its own boundary; the reserve
    read below prints the exact `bucket resets at` timestamp per direction.
  - If the LIMIT itself is wrong, change the policy and reconcile the
    contract to it, deliberately, through the governance action that exists
    for exactly that: 'Change strict 24h rolling limit', then 'Reconcile
    contract limits to the backend policy'.
NORESETDOC
    need_config || return 0
    require_cmd robinhood-reserve || return 0
    echo
    say "Current bucket state, read-only:"
    run_admin "Robinhood reserve + rolling buckets" \
        robinhood-reserve --config "$CONFIG" || true
}

# ------------------------------------------------------ Solana on-chain --

# glc-admin takes a keypair PATH. This console validates that the path is a
# plain readable file and NEVER reads, prints, copies or transports its
# contents. No prompt anywhere here accepts key material or a token.
ask_keypair_path() {
    local __kp_var="$1" path
    while true; do
        echo
        say "The on-chain admin KEYPAIR PATH is required by glc-admin for this command."
        note "This console never reads, prints or copies the file. Paste a PATH, never a key."
        ask "Keypair path (blank to cancel): " path || return 1
        [ -n "$path" ] || return 1
        case "$path" in
            '['*|'{'*|*,*)
                err "that looks like key MATERIAL, not a path. Refused, and not logged."
                path=""
                continue
                ;;
        esac
        if require_regular_file "$path" "--keypair"; then
            printf -v "$__kp_var" '%s' "$path"
            return 0
        fi
    done
}

solana_onchain_reads() {
    need_rpc || return 0
    local choice
    echo
    say "  1) Bridge config account (pauses, limits, rolling windows)"
    say "  2) Authorities (admin, pending handover, upgrade authority, timelock)"
    say "  3) Rebalance policy + governance timelock"
    say "  4) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) require_cmd show-config && run_admin "on-chain bridge config" \
               show-config --rpc-url "$RPC_URL" || true ;;
        2) require_cmd show-authorities && run_admin "on-chain authorities" \
               show-authorities --rpc-url "$RPC_URL" || true ;;
        3) require_cmd rebalance-policy-show && run_admin "on-chain rebalance policy" \
               rebalance-policy-show --rpc-url "$RPC_URL" || true ;;
        *) return 0 ;;
    esac
}

solana_local_pause() {
    need_db || return 0
    local choice direction cmd label note_text want expect_dir
    echo
    say "LOCAL LEDGER PAUSE — this service's OWN directional pause."
    note "Independent of admission control and of the on-chain pause. None of the"
    note "three implies another, and this console never conflates them."
    say "  1) Pause   GoldcoinReserve direction"
    say "  2) Resume  GoldcoinReserve direction"
    say "  3) Pause   SolanaReserve direction"
    say "  4) Resume  SolanaReserve direction"
    say "  5) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) cmd=pause;   direction=goldcoin; label="PAUSE goldcoin" ;;
        2) cmd=unpause; direction=goldcoin; label="RESUME goldcoin" ;;
        3) cmd=pause;   direction=solana;   label="PAUSE solana" ;;
        4) cmd=unpause; direction=solana;   label="RESUME solana" ;;
        *) return 0 ;;
    esac
    require_cmd "$cmd" || return 0

    hdr "Step A — current state"
    run_admin "ledger status (pause + admission + reserves)" status --db "$DB" || true

    hdr "Step B — proposed state"
    say "  ${label}  (local ledger pause only; no chain contact, no restart)"

    hdr "Step C — no dry run exists for this command"
    say "It is one audited ledger flag. Stated here rather than faked."

    ask_note note_text || return 0

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s %s --db %q --direction %q --note %q\n' \
        "$GLC_ADMIN_BIN" "$cmd" "$DB" "$direction" "$note_text"
    confirm_typed EXECUTE "The ledger is untouched." || return 0

    hdr "Step F — execute"
    if ! run_admin "local ${label}" "$cmd" --db "$DB" --direction "$direction" \
        --note "$note_text"; then
        record_verify "local ${label}" 1 "the write failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    if [ "$cmd" = pause ]; then want="paused=true"; else want="paused=false"; fi
    case "$direction" in
        goldcoin) expect_dir="GoldcoinReserve" ;;
        *)        expect_dir="SolanaReserve" ;;
    esac
    if run_admin_capture "ledger status after change" status --db "$DB"; then
        if grep -q "^${expect_dir}:.*${want}" "$CAPTURED"; then
            record_verify "${expect_dir} now reports ${want}" 0
        else
            record_verify "${expect_dir} now reports ${want}" 1 \
                "the ledger status does not show it" || true
        fi
    else
        record_verify "ledger status re-read after change" 1 "the re-read failed" || true
    fi
}

solana_admission() {
    need_db || return 0
    local choice cmd note_text want
    echo
    say "ADMISSION CONTROL (Solana -> Goldcoin only)."
    note "Whether a NEWLY observed SolToGlc obligation is admitted into normal"
    note "processing, or parked to ManualReview. Already-accepted obligations are"
    note "NEVER affected. This is NOT the local pause and NOT the on-chain pause."
    say "  1) close-admission  (new SolToGlc deposits park in ManualReview)"
    say "  2) open-admission   (refuses unless the reserve invariant AND the"
    say "                       confirmed-liquidity gate both allow it)"
    say "  3) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) cmd=close-admission; want="admission_closed=true" ;;
        2) cmd=open-admission;  want="admission_closed=false" ;;
        *) return 0 ;;
    esac
    require_cmd "$cmd" || return 0

    hdr "Step A — current state"
    run_admin "ledger status (pause + admission + liquidity gate)" status --db "$DB" || true

    hdr "Step B — proposed state"
    say "  ${cmd} --direction goldcoin"
    if [ "$cmd" = open-admission ]; then
        echo
        say "  glc-admin refuses this outright, with no override, unless the"
        say "  GoldcoinReserve hard invariant holds AND the automatic"
        say "  confirmed-liquidity gate has already reopened."
    fi

    hdr "Step C — no dry run exists for this command"
    say "It is one audited ledger flag, guarded by its own checks. Stated here"
    say "rather than faked."

    ask_note note_text || return 0

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s %s --db %q --direction goldcoin --note %q\n' \
        "$GLC_ADMIN_BIN" "$cmd" "$DB" "$note_text"
    confirm_typed EXECUTE "The ledger is untouched." || return 0

    hdr "Step F — execute"
    if ! run_admin "$cmd goldcoin" "$cmd" --db "$DB" --direction goldcoin \
        --note "$note_text"; then
        record_verify "$cmd goldcoin" 1 "the command refused or failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    if run_admin_capture "ledger status after change" status --db "$DB"; then
        if grep -q "^GoldcoinReserve:.*${want}" "$CAPTURED"; then
            record_verify "GoldcoinReserve now reports ${want}" 0
        else
            record_verify "GoldcoinReserve now reports ${want}" 1 \
                "the ledger status does not show it" || true
        fi
    else
        record_verify "ledger status re-read after change" 1 "the re-read failed" || true
    fi
}

solana_onchain_pause() {
    need_rpc || return 0
    local choice cmd scope keypair note_text field want
    echo
    say "ON-CHAIN PAUSE (admin-gated-immediate, Solana program)."
    note "The program's own flags. NOT the local ledger pause and NOT admission"
    note "control — all three are separate."
    say "  1) onchain-pause   --scope global"
    say "  2) onchain-pause   --scope release"
    say "  3) onchain-pause   --scope deposit"
    say "  4) onchain-unpause --scope global"
    say "  5) onchain-unpause --scope release"
    say "  6) onchain-unpause --scope deposit"
    say "  7) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) cmd=onchain-pause;   scope=global ;;
        2) cmd=onchain-pause;   scope=release ;;
        3) cmd=onchain-pause;   scope=deposit ;;
        4) cmd=onchain-unpause; scope=global ;;
        5) cmd=onchain-unpause; scope=release ;;
        6) cmd=onchain-unpause; scope=deposit ;;
        *) return 0 ;;
    esac
    require_cmd "$cmd" || return 0

    hdr "Step A — current state (read-only)"
    require_cmd show-config && run_admin "on-chain bridge config" \
        show-config --rpc-url "$RPC_URL" || true

    hdr "Step B — proposed state"
    say "  ${cmd} --scope ${scope}"

    hdr "Step C — no dry run exists for this command"
    say "It is a single admin-gated instruction. Stated here rather than faked."

    ask_keypair_path keypair || return 0
    ask_note note_text || return 0

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s %s --rpc-url %q --keypair %q --scope %q --note %q\n' \
        "$GLC_ADMIN_BIN" "$cmd" "$RPC_URL" "$keypair" "$scope" "$note_text"
    note "  The keypair is a PATH. This console never reads its contents."
    confirm_typed EXECUTE "Nothing was signed or sent." || return 0

    hdr "Step F — execute"
    if ! run_admin "${cmd} ${scope}" "$cmd" --rpc-url "$RPC_URL" --keypair "$keypair" \
        --scope "$scope" --note "$note_text"; then
        record_verify "${cmd} ${scope}" 1 "the transaction failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    case "$scope" in
        global)  field="paused (global)" ;;
        release) field="release_paused" ;;
        *)       field="deposit_paused" ;;
    esac
    if [ "$cmd" = onchain-pause ]; then want=true; else want=false; fi
    if run_admin_capture "on-chain bridge config after change" \
        show-config --rpc-url "$RPC_URL"; then
        if captured_field_equals "$field" "$want"; then
            record_verify "on-chain ${field} is now ${want}" 0
        else
            record_verify "on-chain ${field} is now ${want}" 1 \
                "the re-read does not show it" || true
        fi
    else
        record_verify "on-chain state re-read after change" 1 "the re-read failed" || true
    fi
}

solana_set_limit() {
    local field="$1" label="$2"
    need_rpc || return 0
    require_cmd set-limit || return 0
    local value keypair note_text key

    hdr "Step A — current state (read-only)"
    say "The Solana-side limits live in the on-chain program's config account."
    say "glc-admin reads them; this service never mirrors them."
    require_cmd show-config && run_admin "on-chain bridge config" \
        show-config --rpc-url "$RPC_URL" || true

    hdr "Step B — proposed state"
    cat <<UNITSDOC
  set-limit --field ${field}   (${label})

  UNITS — the one place this console cannot take a human GLC figure.
  glc-admin documents --value as ATOMIC UNITS OF THE SOLANA-SIDE MINT.
  That mint's decimals are read LIVE from the chain and are NOT the
  canonical 8dp precision the backend policy uses, so converting a human
  GLC figure here would put a second set of rules about decimals inside a
  shell script. The numbers printed by show-config above are already in
  exactly the units wanted — compare yours against them before typing it.
UNITSDOC
    ask_validated $'\nNew value [atomic units of the Solana-side mint]: ' validate_u64 value || return 0

    hdr "Step C — no dry run exists for this command"
    say "It is a single admin-gated instruction. Stated here rather than faked."

    ask_keypair_path keypair || return 0
    ask_note note_text || return 0

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run:"
    printf '  %s set-limit --rpc-url %q --keypair %q --field %q --value %q --note %q\n' \
        "$GLC_ADMIN_BIN" "$RPC_URL" "$keypair" "$field" "$value" "$note_text"
    note "  The keypair is a PATH. This console never reads its contents."
    confirm_typed EXECUTE "Nothing was signed or sent." || return 0

    hdr "Step F — execute"
    if ! run_admin "set-limit ${field}" set-limit --rpc-url "$RPC_URL" --keypair "$keypair" \
        --field "$field" --value "$value" --note "$note_text"; then
        record_verify "set-limit ${field}" 1 "the transaction failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    case "$field" in
        per-transfer)      key="per_transfer_limit" ;;
        rolling-volume)    key="rolling_volume_limit" ;;
        min-transfer)      key="min_transfer_amount" ;;
        protected-minimum) key="protected_minimum" ;;
        *)                 key="$field" ;;
    esac
    if run_admin_capture "on-chain bridge config after change" \
        show-config --rpc-url "$RPC_URL"; then
        if captured_field_equals "$key" "$value"; then
            record_verify "on-chain ${key} is now ${value}" 0
        else
            record_verify "on-chain ${key} is now ${value}" 1 \
                "the re-read does not show it" || true
        fi
    else
        record_verify "on-chain state re-read after change" 1 "the re-read failed" || true
    fi
}

solana_limit_menu() {
    local choice
    echo
    say "Which on-chain Solana limit?"
    note "These are the Solana program's own ceilings, changed with glc-admin"
    note "set-limit — the mechanism glc-admin itself names for them."
    say "  1) per-transfer        (per_transfer_limit)"
    say "  2) rolling-volume      (rolling_volume_limit, the 24h ceiling)"
    say "  3) min-transfer        (min_transfer_amount)"
    say "  4) protected-minimum   (protected_minimum)"
    say "  5) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) solana_set_limit per-transfer "the per-transfer ceiling" ;;
        2) solana_set_limit rolling-volume "the 24h rolling-volume ceiling" ;;
        3) solana_set_limit min-transfer "the minimum transfer" ;;
        4) solana_set_limit protected-minimum "the protected reserve minimum" ;;
        *) return 0 ;;
    esac
}

solana_reserve_status() {
    need_db || return 0
    require_cmd status && run_admin "reserve + ledger status" status --db "$DB" || true
    if admin_has rebalance-status; then
        run_admin "reserve imbalance assessment" rebalance-status --db "$DB" || true
    fi
}

solana_pause_state() {
    need_db || return 0
    hdr "Pause and admission state — three separate controls"
    cat <<'PAUSEDOC'
  LOCAL LEDGER PAUSE   per direction, this service's own gate
  ADMISSION CONTROL    Solana -> Goldcoin only; parks NEW obligations
  ON-CHAIN PAUSE       BridgeConfig.paused / release_paused / deposit_paused

None implies another. Clearing one clears nothing else.
PAUSEDOC
    run_admin "local pause + admission (ledger)" status --db "$DB" || true
    if [ -n "$RPC_URL" ] && admin_has show-config; then
        run_admin "on-chain pause flags" show-config --rpc-url "$RPC_URL" || true
    else
        echo
        note "On-chain pause flags not read — no --rpc-url selected."
    fi
}

solana_menu() {
    local choice
    while true; do
        hdr "Goldcoin Bridge Admin — Goldcoin <-> Solana"
        say "  Config: ${CONFIG:-<not selected>}"
        say "  Ledger: ${DB:-<not selected>}"
        say "  RPC:    ${RPC_URL:-<not selected>}"
        echo
        say "  READ-ONLY"
        say "   1) Route & gate state"
        say "   2) Pause & admission state"
        say "   3) Reserve status"
        say "   4) Fee & limits (chain policy)"
        say "   5) On-chain program state"
        echo
        say "  FEES  (per ROUTE)"
        say "   6) Show per-route fees"
        say "   7) Change one route's fee %"
        echo
        say "  LIMITS"
        say "   8) Change per-transfer limit"
        say "   9) Change 24h rolling limit"
        say "  10) Validate a proposed policy (writes nothing)"
        say "  11) Apply backend policy (limits)"
        say "  12) On-chain limits (set-limit)"
        echo
        say "  PAUSE / ADMISSION"
        say "  13) Local ledger pause / resume"
        say "  14) Admission control (Solana -> Goldcoin)"
        say "  15) On-chain pause / resume"
        echo
        say "  ROLLING WINDOW"
        say "  16) Reset a 24h rolling window"
        echo
        say "  17) Back"
        echo
        ask "Choice: " choice || return 0
        case "$choice" in
            1)  solana_route_state_screen ;;
            2)  solana_pause_state ;;
            3)  solana_reserve_status ;;
            4)  show_policy solana ;;
            5)  solana_onchain_reads ;;
            6)  show_route_fees ;;
            7)  change_route_fee ;;
            8)  change_policy solana per ;;
            9)  change_policy solana roll ;;
            10) validate_policy_only solana ;;
            11) change_policy solana all ;;
            12) solana_limit_menu ;;
            13) solana_local_pause ;;
            14) solana_admission ;;
            15) solana_onchain_pause ;;
            16) solana_reset_rolling_window ;;
            17) return 0 ;;
            *)  err "not a valid choice: '$choice'" ;;
        esac
    done
}

# -------------------------------------------------------- Robinhood menu --

robinhood_diagnostics() {
    local choice
    echo
    say "  1) Indexer, operations, ManualReview queue, reserve"
    say "  2) Operations detail (digests, signatures, nonces, receipts)"
    say "  3) Stalled operations only"
    say "  4) Submitter nonce picture (read-only)"
    say "  5) RhnToGlc ManualReview queue"
    say "  6) Signer quorum + full preflight"
    say "  7) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        7|"") return 0 ;;
    esac
    need_config || return 0
    case "$choice" in
        1) require_cmd robinhood-status && run_admin "Robinhood status" \
               robinhood-status --config "$CONFIG" || true ;;
        2) require_cmd robinhood-tx-show && run_admin "Robinhood operations" \
               robinhood-tx-show --config "$CONFIG" || true ;;
        3) require_cmd robinhood-tx-show && run_admin "stalled Robinhood operations" \
               robinhood-tx-show --config "$CONFIG" --stalled || true ;;
        4) require_cmd robinhood-nonce-status && run_admin "Robinhood nonce status" \
               robinhood-nonce-status --config "$CONFIG" || true ;;
        5) require_cmd robinhood-manual-review-list && run_admin "Robinhood ManualReview queue" \
               robinhood-manual-review-list --config "$CONFIG" || true ;;
        6) require_cmd robinhood-preflight && run_admin "signer quorum + preflight" \
               robinhood-preflight --config "$CONFIG" || true ;;
        *) return 0 ;;
    esac
}

robinhood_pause_state() {
    hdr "Robinhood pause state — separate controls, never conflated"
    cat <<'RHPAUSEDOC'
  LOCAL LEDGER PAUSE       the Robinhood reserve's own paused flag
  CONTRACT depositsPaused  inbound to GlcRobinhoodBridge
  CONTRACT payoutsPaused   outbound from GlcRobinhoodBridge

Admission control does NOT apply here: it is implemented for the
Solana -> Goldcoin direction only.
RHPAUSEDOC
    need_config || return 0
    require_cmd robinhood-reserve && run_admin "Robinhood reserve (ledger paused flag)" \
        robinhood-reserve --config "$CONFIG" || true
    require_cmd robinhood-preflight && run_admin "contract pause flags" \
        robinhood-preflight --config "$CONFIG" || true
}

robinhood_clear_halt() {
    need_config || return 0
    require_cmd robinhood-clear-halt || return 0
    local reason note_text answer choice
    echo
    say "Clearing a halted Robinhood indexer. --expect-reason is REQUIRED and must"
    say "equal the STORED halt reason: naming a different one is a refusal, because"
    say "a halt whose cause has not been diagnosed must not be cleared."
    say "  1) observation_conflict"
    say "  2) post_finality_reorg"
    say "  3) reorg_beyond_retained_anchors"
    say "  4) chain_id_mismatch"
    say "  5) unexpected_contract_route"
    say "  6) Back"
    ask "Choice: " choice || return 0
    case "$choice" in
        1) reason=observation_conflict ;;
        2) reason=post_finality_reorg ;;
        3) reason=reorg_beyond_retained_anchors ;;
        4) reason=chain_id_mismatch ;;
        5) reason=unexpected_contract_route ;;
        *) return 0 ;;
    esac

    local -a extra=()
    if [ "$reason" = post_finality_reorg ] || [ "$reason" = reorg_beyond_retained_anchors ]; then
        echo
        say "A reorg halt additionally requires acknowledging that finalized"
        say "observations may have been invalidated. Review the count in the dry run."
        ask "Add --acknowledge-orphaned-finality? [y/N]: " answer || return 0
        if [ "$answer" = "y" ] || [ "$answer" = "Y" ]; then
            extra+=(--acknowledge-orphaned-finality)
        fi
    fi

    hdr "Step A — current state"
    require_cmd robinhood-status && run_admin "Robinhood status" \
        robinhood-status --config "$CONFIG" || true

    ask_note note_text || return 0

    hdr "Step B/C — DRY RUN (every clearance check as PASS/FAIL; changes nothing)"
    if ! run_admin "clear halt (dry run)" robinhood-clear-halt --config "$CONFIG" \
        --note "$note_text" --expect-reason "$reason" "${extra[@]}"; then
        say "Nothing was changed."
        return 0
    fi

    hdr "Step D/E — the exact action, and confirmation"
    say "About to run the same command with --execute appended."
    confirm_typed EXECUTE "The halt is untouched." || return 0

    hdr "Step F — execute"
    if ! run_admin "clear halt" robinhood-clear-halt --config "$CONFIG" \
        --note "$note_text" --expect-reason "$reason" "${extra[@]}" --execute; then
        record_verify "clear indexer halt" 1 "the command refused or failed" || true
        return 0
    fi

    hdr "Step G/H — post-change verification"
    if run_admin_capture "Robinhood status after change" \
        robinhood-status --config "$CONFIG"; then
        if grep -qE '^  halted +no' "$CAPTURED"; then
            record_verify "the indexer is no longer halted" 0
        else
            record_verify "the indexer is no longer halted" 1 \
                "robinhood-status still reports a halt" || true
        fi
    else
        record_verify "Robinhood status re-read after change" 1 "the re-read failed" || true
    fi
}

robinhood_menu() {
    local choice
    while true; do
        hdr "Goldcoin Bridge Admin — Goldcoin <-> Robinhood"
        say "  Config: ${CONFIG:-<not selected>}"
        say "  Ledger: ${DB:-<not selected>}"
        echo
        say "  READ-ONLY"
        say "   1) Route & gate state (all four switches)"
        say "   2) Pause state (ledger + contract)"
        say "   3) Reserve status (+ rolling buckets)"
        say "   4) Fee & limits (backend + contract, with the half-bucket rule)"
        say "   5) Indexer / operations / ManualReview / signer quorum"
        echo
        say "  FEES  (per ROUTE; a restart is required afterwards)"
        say "   6) Show per-route fees"
        say "   7) Change one route's fee %"
        echo
        say "  LIMITS  (backend config; a restart is required afterwards)"
        say "   8) Change per-transfer limit"
        say "   9) Change strict 24h rolling limit"
        say "  10) Validate a proposed policy (writes nothing)"
        say "  11) Apply backend policy (limits)"
        echo
        say "  LEDGER ROUTE GATE  (no restart needed)"
        say "  12) Enable a route in ledger state"
        say "  13) Disable a route in ledger state"
        echo
        say "  CONTRACT GOVERNANCE  (2-of-3 quorum; dry run always first)"
        say "  14) Reconcile contract limits to the backend policy"
        say "  15) Contract pause / resume (deposits | payouts)"
        say "  16) Contract route enable / disable"
        echo
        say "  ROLLING WINDOW"
        say "  17) Reset the contract rolling bucket"
        echo
        say "  RECOVERY"
        say "  18) Clear a halted indexer"
        echo
        say "  19) Back"
        echo
        ask "Choice: " choice || return 0
        case "$choice" in
            1)  robinhood_route_state_screen ;;
            2)  robinhood_pause_state ;;
            3)  need_config && require_cmd robinhood-reserve && \
                    run_admin "Robinhood reserve" robinhood-reserve --config "$CONFIG" || true ;;
            4)  show_policy robinhood ;;
            5)  robinhood_diagnostics ;;
            6)  show_route_fees ;;
            7)  change_route_fee ;;
            8)  change_policy robinhood per ;;
            9)  change_policy robinhood roll ;;
            10) validate_policy_only robinhood ;;
            11) change_policy robinhood all ;;
            12) ledger_route_change true ;;
            13) ledger_route_change false ;;
            14) governance_set_limits ;;
            15) governance_pause ;;
            16) governance_route ;;
            17) robinhood_rolling_window_reset ;;
            18) robinhood_clear_halt ;;
            19) return 0 ;;
            *)  err "not a valid choice: '$choice'" ;;
        esac
    done
}

# --------------------------------------------------------- overall status --

overall_status() {
    hdr "Overall bridge status (read-only)"
    if need_db; then
        require_cmd status && run_admin "ledger status (Goldcoin + Solana reserves)" \
            status --db "$DB" || true
        if admin_has robinhood-status; then
            run_admin "Robinhood status" robinhood-status --db "$DB" || true
        fi
        if admin_has robinhood-routes; then
            local -a cmd=(robinhood-routes --db "$DB")
            [ "$CONFIG_OK" -eq 1 ] && cmd+=(--config "$CONFIG")
            run_admin "route gates" "${cmd[@]}" || true
        fi
    fi
    if [ "$CONFIG_OK" -eq 1 ] || [ -n "$CONFIG" ]; then
        if need_config; then
            if admin_has chain-policy-networks; then
                run_admin "policy-governed networks" chain-policy-networks || true
            fi
            local network
            while IFS=$'\t' read -r network _; do
                [ -n "$network" ] || continue
                run_admin "chain policy ($network)" \
                    chain-policy-show --config "$CONFIG" --network "$network" || true
            done < <(admin_read chain-policy-networks --porcelain || true)
            if admin_has robinhood-reserve; then
                run_admin "Robinhood reserve + rolling buckets" \
                    robinhood-reserve --config "$CONFIG" || true
            fi
        fi
    else
        echo
        note "No config selected — the chain policies and the Robinhood contract view"
        note "are not shown. Re-run with --config PATH for the full picture."
    fi
    echo
    note "Nothing above changed anything: every command in this screen is read-only."
}

# ------------------------------------------------------------------- main --

while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)  CONFIG="${2:-}";  shift 2 || true ;;
        --db)      DB="${2:-}";      shift 2 || true ;;
        --rpc-url) RPC_URL="${2:-}"; shift 2 || true ;;
        -h|--help) print_header; exit 0 ;;   # handled above; kept as a backstop
        *) err "unknown argument: $1"; exit 2 ;;
    esac
done

if ! GLC_ADMIN_BIN="$(find_glc_admin)"; then
    cat >&2 <<'NOBINDOC'
error: could not find the glc-admin binary.

Build it, or point GLC_ADMIN at it:

    cd service && cargo build --release --bin glc-admin
    GLC_ADMIN=/path/to/glc-admin scripts/bridge-admin.sh
NOBINDOC
    exit 2
fi

probe_admin
init_log

hdr "Goldcoin Bridge Admin"
say "  glc-admin: $GLC_ADMIN_BIN"
say "  log:       $([ "$LOG_ENABLED" -eq 1 ] && printf '%s' "$LOG" || printf 'disabled')"
echo
note "Every command this console runs is printed before it runs. Every change"
note "shows current state, proposed state, a dry run where one exists, the exact"
note "command, and needs a typed confirmation — then it is verified."
note "It never reads a secret, never asks for a private key, never restarts the"
note "daemon, and never enables a route as a side effect of another action."

while true; do
    hdr "Goldcoin Bridge Admin"
    echo
    say "Select network:"
    say "1) Goldcoin <-> Solana"
    say "2) Goldcoin <-> Robinhood"
    say "3) Overall bridge status"
    say "4) Exit"
    echo
    ask "Choice: " MAIN_CHOICE || finish
    case "$MAIN_CHOICE" in
        1) solana_menu ;;
        2) robinhood_menu ;;
        3) overall_status ;;
        4) finish ;;
        *) err "not a valid choice: '$MAIN_CHOICE'" ;;
    esac
done
