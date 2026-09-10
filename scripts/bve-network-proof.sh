#!/usr/bin/env bash
#
# Bamep Issue #70 - isolated PXE-capable BVE network host proof.
#
# Versioned DEVELOPMENT HARNESS - not product runtime, not production
# configuration. It drives the whole proof cycle end to end and is safe to
# repeat:
#
#   verify-clean -> setup (privileged) -> start fixture (privileged) ->
#   run BVE PXE-first (NORMAL USER) -> assert DHCP DORA for the deterministic
#   MAC -> stop the real dnsmasq deterministically -> wait for it to exit ->
#   remove the netfilter accommodation + teardown (privileged) ->
#   verify-clean -> assert zero residual BVE iptables rules.
#
# This harness uses `sudo` explicitly because it is a human host-validation
# tool. The `bamep-ve` crate itself never calls `sudo`; QEMU / `run-bve`
# always run as the normal user. The real fixture process is located via the
# dnsmasq pid-file (bamep_ve::fixture_pid_file) validated against
# `ip netns pids <netns>` - never the `sudo`/`ip` wrapper PID.
#
# All cleanup is scoped to exactly the proof's BVE id. No wildcard.
#
# Usage:
#   ./scripts/bve-network-proof.sh [--verbose] [--no-netfilter] [bve-id]
#
#   --verbose       echo privileged commands, run-bve output, full dnsmasq log
#   --no-netfilter  pass --no-netfilter-accommodation to setup
#   bve-id          default: bve-net-proof

set -euo pipefail

VERBOSE=0
NO_NF=0
ID="bve-net-proof"
for a in "$@"; do
    case "$a" in
        -v | --verbose) VERBOSE=1 ;;
        --no-netfilter | --no-netfilter-accommodation) NO_NF=1 ;;
        -h | --help)
            sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        --*)
            echo "unknown flag: $a" >&2
            exit 2
            ;;
        *) ID="$a" ;;
    esac
done

if [[ "$(id -u)" -eq 0 ]]; then
    echo "run this as your normal user - the harness calls sudo itself so the BVE never runs as root" >&2
    exit 2
fi

cd "$(dirname "$0")/.."
REPO="$PWD"
BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug/examples/bve_isolated_net"
FIXTURE_LOG="$(mktemp -t bamep-bve-proof-fixture.XXXXXX)"

FIXTURE_STARTED=0
FIXTURE_BGPID=""
FIXTURE_REAL_PID=""
CLEANED=0

# ---- output helpers -------------------------------------------------------
line() {
    local label="$1" width=20 pad
    pad=$((width - ${#label} - 1))
    ((pad < 1)) && pad=1
    printf '%s ' "$label"
    printf '%*s' "$pad" '' | tr ' ' '.'
    printf ' '
}
vrun() { # run a privileged bve_isolated_net subcommand
    if ((VERBOSE)); then
        echo "  + sudo $BIN $*" >&2
        sudo "$BIN" "$@"
    else
        sudo "$BIN" "$@" >/dev/null 2>&1
    fi
}
die() {
    # completes the pending dotted line, then prints the reason + fixture tail
    echo "FAIL"
    [[ -n "${1:-}" ]] && echo "  $1" >&2
    if [[ -s "$FIXTURE_LOG" ]]; then
        echo "  --- last dnsmasq/fixture lines ---" >&2
        tail -n 40 "$FIXTURE_LOG" >&2 || true
    fi
    exit 1
}

# ---- fixture process control -------------------------------------------------
start_fixture() {
    sudo "$BIN" start-fixture "$ID" >"$FIXTURE_LOG" 2>&1 &
    FIXTURE_BGPID=$!
    FIXTURE_STARTED=1
    local i pid pids
    for i in $(seq 1 60); do # up to ~12s
        if ! kill -0 "$FIXTURE_BGPID" 2>/dev/null; then
            return 1 # the sudo wrapper died before the fixture came up
        fi
        pid="$(sudo cat "$BVE_FIXTURE_PIDFILE" 2>/dev/null | tr -dc '0-9')"
        if [[ -n "$pid" ]]; then
            pids="$(sudo ip netns pids "$BVE_NETNS" 2>/dev/null || true)"
            if grep -qx "$pid" <<<"$pids" \
                && grep -qE 'DHCP, IP range|started, version' "$FIXTURE_LOG"; then
                FIXTURE_REAL_PID="$pid"
                return 0
            fi
        fi
        sleep 0.2
    done
    return 1
}

wait_bg() {
    [[ -n "$FIXTURE_BGPID" ]] || return 0
    # the real dnsmasq is already handled via the netns; this only reaps our
    # own `sudo` wrapper process.
    kill "$FIXTURE_BGPID" 2>/dev/null || true
    wait "$FIXTURE_BGPID" 2>/dev/null || true
    FIXTURE_BGPID=""
}

netns_empty() {
    [[ -z "$(sudo ip netns pids "$BVE_NETNS" 2>/dev/null || true)" ]]
}

stop_fixture() {
    ((FIXTURE_STARTED)) || return 0
    local pid="$FIXTURE_REAL_PID" i p
    [[ -z "$pid" ]] && pid="$(sudo cat "$BVE_FIXTURE_PIDFILE" 2>/dev/null | tr -dc '0-9')"
    [[ -n "$pid" ]] && sudo kill -TERM "$pid" 2>/dev/null || true

    for i in $(seq 1 50); do # ~10s for a clean SIGTERM exit
        if netns_empty; then
            FIXTURE_STARTED=0
            wait_bg
            return 0
        fi
        sleep 0.2
    done

    # Escalate - still strictly scoped to this proof's netns.
    while read -r p; do
        [[ -n "$p" ]] && sudo kill -KILL "$p" 2>/dev/null || true
    done < <(sudo ip netns pids "$BVE_NETNS" 2>/dev/null || true)
    for i in $(seq 1 25); do
        if netns_empty; then
            FIXTURE_STARTED=0
            wait_bg
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# ---- trap: best-effort host restore on Ctrl-C / mid-failure ----------------
cleanup() {
    local rc=$?
    ((CLEANED)) && exit "$rc"
    CLEANED=1
    set +e
    trap - EXIT INT TERM
    stop_fixture >/dev/null 2>&1
    # teardown is idempotent: removes the accommodation + any residual
    # bridge/TAP/veth/netns for THIS id only, or is a no-op if already clean.
    if [[ -n "${BVE_NETNS:-}" ]]; then
        sudo "$BIN" teardown "$ID" >/dev/null 2>&1
    fi
    rm -f "$FIXTURE_LOG"
    ((rc != 0)) && echo "  (host restore attempted after rc=$rc)" >&2
    exit "$rc"
}
trap cleanup EXIT INT TERM

# ---- proof ----------------------------------------------------------------
if ((VERBOSE)); then
    cargo build -p bamep-ve --example bve_isolated_net
else
    cargo build -q -p bamep-ve --example bve_isolated_net >/dev/null
fi

echo "priming sudo (you may be prompted once)..."
sudo -v || {
    echo "sudo is required for the privileged steps" >&2
    exit 2
}

eval "$("$BIN" env "$ID")"
((VERBOSE)) && {
    echo "  id=$BVE_ID tap=$BVE_TAP veth=$BVE_VETH_HOST netns=$BVE_NETNS mac=$BVE_MAC"
}

line "check clean"
if "$BIN" verify-clean "$ID" >/dev/null 2>&1; then
    echo "ok"
else
    echo "DIRTY"
    "$BIN" verify-clean "$ID" || true
    die "a previous run left resources - inspect, then: sudo $BIN teardown $ID"
fi

line "setup"
if ((NO_NF)); then
    vrun setup "$ID" --no-netfilter-accommodation && echo "ok" || die "setup failed"
else
    vrun setup "$ID" && echo "ok" || die "setup failed"
fi

line "fixture"
start_fixture && echo "ok" || die "fixture did not become ready"

line "BVE PXE"
set +e
BVE_OUT="$("$BIN" run-bve "$ID" 2>&1)"
BVE_RC=$?
set -e
((VERBOSE)) && printf '%s\n' "$BVE_OUT" | sed 's/^/    /'

FOUND=()
MISSING=()
for kind in DHCPDISCOVER DHCPOFFER DHCPREQUEST DHCPACK; do
    if grep -qE "${kind}\b.*${BVE_MAC}" "$FIXTURE_LOG"; then
        FOUND+=("$kind")
    else
        MISSING+=("$kind")
    fi
done

if ((BVE_RC == 0)) && ((${#MISSING[@]} == 0)); then
    echo "PASS"
    for k in "${FOUND[@]}"; do echo "  $k"; done
    echo "  MAC $BVE_MAC"
else
    ((BVE_RC != 0)) && printf '%s\n' "$BVE_OUT" | sed 's/^/    /' >&2
    ((${#MISSING[@]} > 0)) && echo "  missing: ${MISSING[*]}" >&2
    die "DHCP evidence incomplete (run-bve rc=$BVE_RC)"
fi

line "fixture stop"
stop_fixture && echo "ok" || die "dnsmasq did not exit"

line "teardown"
vrun teardown "$ID" && echo "ok" || {
    sudo "$BIN" teardown "$ID" || true
    die "teardown failed"
}

line "host clean"
RESID_RULES="$(sudo iptables -S FORWARD 2>/dev/null | grep -E "${BVE_TAP}|${BVE_VETH_HOST}" || true)"
if "$BIN" verify-clean "$ID" >/dev/null 2>&1 && [[ -z "$RESID_RULES" ]]; then
    echo "yes"
else
    echo "NO"
    "$BIN" verify-clean "$ID" || true
    [[ -n "$RESID_RULES" ]] && printf '  residual rule: %s\n' "$RESID_RULES" >&2
    die "residual host state after teardown"
fi

CLEANED=1
trap - EXIT INT TERM
rm -f "$FIXTURE_LOG"
