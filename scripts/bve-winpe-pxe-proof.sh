#!/usr/bin/env bash
#
# Bamep Issue #71 - PXE-boot the existing iPXE + wimboot WinPE path in one BVE
# over virtual UEFI (OVMF non-Secure-Boot), on the isolated #70 network.
#
# Versioned DEVELOPMENT HARNESS - not product runtime, not production config.
# One command drives the whole cycle and is safe to repeat:
#
#   preflight -> artifact hashes -> UEFI firmware -> setup (privileged) ->
#   start fixture (dnsmasq DHCP/TFTP + python HTTP, privileged) ->
#   run BVE UEFI-PXE-first x2 (NORMAL USER) -> parse boot-stage evidence ->
#   stop the real fixture processes deterministically -> teardown (privileged) ->
#   verify-clean -> assert zero residual BVE iptables rules.
#
# This is #71-specific and depends on external Microsoft/iPXE artifacts. The
# generic isolated-PXE-network proof stays at scripts/bve-network-proof.sh and
# is NOT changed by this harness.
#
# Prereq: export BAMEP_BVE_WINPE_FIXTURE_ROOT=<dir with snponly.efi wimboot BCD
# boot.sdi boot.wim>  (hashes: scripts/winpe-pxe-fixture.sha256). Nothing is
# ever downloaded; a missing/mismatched artifact is a hard error.
#
# Usage:
#   ./scripts/bve-winpe-pxe-proof.sh [--verbose] [bve-id]

set -euo pipefail

VERBOSE=0
ID="bve-winpe-pxe-proof"
for a in "$@"; do
    case "$a" in
        -v | --verbose) VERBOSE=1 ;;
        -h | --help)
            sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        --*) echo "unknown flag: $a" >&2; exit 2 ;;
        *) ID="$a" ;;
    esac
done

if [[ "$(id -u)" -eq 0 ]]; then
    echo "run this as your normal user - the harness calls sudo itself" >&2
    exit 2
fi
: "${BAMEP_BVE_WINPE_FIXTURE_ROOT:?set it to the dir holding the retained WinPE PXE artifacts (see scripts/winpe-pxe-fixture.provenance.md)}"

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib/winpe-pxe-evidence.sh
source "$HERE/lib/winpe-pxe-evidence.sh"
cd "$HERE/.."
REPO="$PWD"
BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug/examples/bve_winpe_pxe"
FIXTURE_LOG="$(mktemp -t bamep-winpe-proof.XXXXXX)"
RUN_LOG="$(mktemp -t bamep-winpe-runbve.XXXXXX)"

FIXTURE_BGPID=""
CLEANED=0

line() {
    local label="$1" width=20 pad
    pad=$((width - ${#label} - 1)); ((pad < 1)) && pad=1
    printf '%s ' "$label"; printf '%*s' "$pad" '' | tr ' ' '.'; printf ' '
}
die() {
    echo "FAIL"
    [[ -n "${1:-}" ]] && echo "  $1" >&2
    if [[ -s "$FIXTURE_LOG" ]]; then
        echo "  --- last fixture lines ---" >&2
        tail -n 40 "$FIXTURE_LOG" >&2 || true
    fi
    exit 1
}
vrun() {
    if ((VERBOSE)); then echo "  + sudo $BIN $*" >&2; sudo "$BIN" "$@"
    else sudo "$BIN" "$@" >/dev/null 2>&1; fi
}

# ---- fixture process control -------------------------------------------------
start_fixture() {
    # sudo resets the environment, so the artifact paths go as CLI flags (which
    # always survive). CWD is preserved by sudo, so the relative default
    # manifest path still resolves against $REPO.
    sudo "$BIN" start-fixture "$ID" \
        --fixture-root "$BAMEP_BVE_WINPE_FIXTURE_ROOT" \
        ${BAMEP_BVE_WINPE_MANIFEST:+--manifest "$BAMEP_BVE_WINPE_MANIFEST"} \
        >"$FIXTURE_LOG" 2>&1 &
    FIXTURE_BGPID=$!
    local i dpid hpid pids
    for i in $(seq 1 100); do  # up to ~20s (staging boot.wim copy included)
        kill -0 "$FIXTURE_BGPID" 2>/dev/null || return 1
        dpid="$(sudo cat "$BVE_FIXTURE_PIDFILE" 2>/dev/null | tr -dc '0-9')"
        hpid="$(sudo cat "$BVE_HTTP_PIDFILE" 2>/dev/null | tr -dc '0-9')"
        if [[ -n "$dpid" && -n "$hpid" ]]; then
            pids="$(sudo ip netns pids "$BVE_NETNS" 2>/dev/null || true)"
            if grep -qx "$dpid" <<<"$pids" && grep -qx "$hpid" <<<"$pids" \
                && grep -qE 'started, version' "$FIXTURE_LOG"; then
                return 0
            fi
        fi
        sleep 0.2
    done
    return 1
}
netns_empty() { [[ -z "$(sudo ip netns pids "$BVE_NETNS" 2>/dev/null || true)" ]]; }
stop_fixture() {
    local i p
    for p in "$(sudo cat "$BVE_FIXTURE_PIDFILE" 2>/dev/null | tr -dc '0-9')" \
             "$(sudo cat "$BVE_HTTP_PIDFILE" 2>/dev/null | tr -dc '0-9')"; do
        [[ -n "$p" ]] && sudo kill -TERM "$p" 2>/dev/null || true
    done
    for i in $(seq 1 50); do netns_empty && break; sleep 0.2; done
    if ! netns_empty; then
        while read -r p; do [[ -n "$p" ]] && sudo kill -KILL "$p" 2>/dev/null || true
        done < <(sudo ip netns pids "$BVE_NETNS" 2>/dev/null || true)
        for i in $(seq 1 25); do netns_empty && break; sleep 0.2; done
    fi
    [[ -n "$FIXTURE_BGPID" ]] && { kill "$FIXTURE_BGPID" 2>/dev/null || true; wait "$FIXTURE_BGPID" 2>/dev/null || true; }
    FIXTURE_BGPID=""
    netns_empty
}

cleanup() {
    local rc=$?
    ((CLEANED)) && exit "$rc"
    CLEANED=1
    set +e
    trap - EXIT INT TERM
    stop_fixture >/dev/null 2>&1
    [[ -n "${BVE_NETNS:-}" ]] && sudo "$BIN" teardown "$ID" >/dev/null 2>&1
    rm -f "$FIXTURE_LOG" "$RUN_LOG"
    ((rc != 0)) && echo "  (host restore attempted after rc=$rc)" >&2
    exit "$rc"
}
trap cleanup EXIT INT TERM

# ---- stage evidence ---------------------------------------------------------
# Whole-log "did this ever happen" checks for the pre-WinPE chain; per-boot
# range checks (winpe_dhcp_ack_in_range) are used for WinPE readiness below.
have() { grep -qE "$1" "$FIXTURE_LOG"; }
stage() {  # stage() <label> <pass-regex> [<n/a-regex> <n/a-note>]
    local label="$1" pat="$2" naPat="${3:-}" naNote="${4:-}"
    line "$label"
    if have "$pat"; then echo "PASS"; return 0; fi
    if [[ -n "$naPat" ]] && have "$naPat"; then echo "N/A ($naNote)"; return 0; fi
    echo "NOT REACHED"; STAGE_FAIL=1
}

# ---- proof ------------------------------------------------------------------
if ((VERBOSE)); then cargo build -p bamep-ve --example bve_winpe_pxe
else cargo build -q -p bamep-ve --example bve_winpe_pxe >/dev/null; fi

eval "$("$BIN" env "$ID")"
((VERBOSE)) && echo "  id=$BVE_ID mac=$BVE_MAC netns=$BVE_NETNS tap=$BVE_TAP"

line "artifacts"
if ARTOUT="$("$BIN" check-artifacts 2>&1)"; then echo "ok"
else echo "FAIL"; printf '%s\n' "$ARTOUT" >&2; exit 1; fi

line "UEFI firmware"
if "$BIN" check >/dev/null 2>&1; then echo "ok"
else echo "FAIL"; "$BIN" check || true; exit 1; fi

line "host clean"
"$BIN" verify-clean "$ID" >/dev/null 2>&1 && echo "ok" || {
    echo "DIRTY"; die "a previous run left resources - run: sudo $BIN teardown $ID"; }

echo "priming sudo (you may be prompted once)..."
sudo -v || { echo "sudo is required" >&2; exit 2; }

line "network setup"
vrun setup "$ID" && echo "ok" || die "setup failed"

line "PXE fixture"
start_fixture && echo "ok" || die "dnsmasq + http fixture did not become ready"

line "BVE x2"
set +e
# run-bve controls both boots inside the SAME BveRuntime/definition/storage/OVMF
# VARS; --evidence-log makes it emit the fixture-log line ranges that bound each
# boot (it does NOT interpret DHCP — the harness does).
"$BIN" run-bve "$ID" --evidence-log "$FIXTURE_LOG" >"$RUN_LOG" 2>&1
BVE_RC=$?
set -e
((VERBOSE)) && sed 's/^/    /' "$RUN_LOG"
((BVE_RC == 0)) && echo "ok" || { sed 's/^/    /' "$RUN_LOG" >&2; die "run-bve rc=$BVE_RC"; }

# Boot boundaries in the real fixture log (1-indexed inclusive), from run-bve.
range_of() { grep -oE "^BVE_BOOT${1}_LOG_RANGE=[0-9]+:[0-9]+\$" "$RUN_LOG" | tail -1 | cut -d= -f2 || true; }
B1="$(range_of 1)"
B2="$(range_of 2)"
[[ "$B1" =~ ^[0-9]+:[0-9]+$ && "$B2" =~ ^[0-9]+:[0-9]+$ ]] \
    || die "run-bve did not emit BVE_BOOT{1,2}_LOG_RANGE"
B1S="${B1%%:*}"; B1E="${B1##*:}"; B2S="${B2%%:*}"; B2E="${B2##*:}"
((VERBOSE)) && echo "  boot#1 lines $B1   boot#2 lines $B2"

echo
STAGE_FAIL=0
http() {  # http() <label> <path> [--partial]
    line "$1"
    if [[ "${3:-}" == --partial ]]; then
        http_transfer_started "$FIXTURE_LOG" "$2" && { echo "transfer started"; return; }
    else
        http_200 "$FIXTURE_LOG" "$2" && { echo "PASS"; return; }
    fi
    echo "NOT REACHED"; STAGE_FAIL=1
}

stage "UEFI PXE"      "DHCPDISCOVER.*${BVE_MAC}"
stage "  arch EFIx64" "tags:.*efi-x64|client-arch"
stage "TFTP bootstrap" "sent .*snponly\\.efi" "user class: iPXE|GET /boot\\.ipxe" "firmware NIC option ROM already entered iPXE"
stage "iPXE"          "user class: iPXE|GET /boot\\.ipxe"
http  "wimboot"   /wimboot
http  "BCD"       /BCD
http  "boot.sdi"  /boot.sdi
http  "boot.wim"  /boot.wim --partial

# WinPE readiness must be proven independently INSIDE each boot's own line
# range: a Windows-client DHCP transaction (MSFT 5.0 / minint) reaching DHCPACK
# for the BVE MAC. A single boot yields several matching text lines, so a
# whole-log count would be wrong.
line "WinPE ready #1"
if winpe_dhcp_ack_in_range "$FIXTURE_LOG" "$B1S" "$B1E" "$BVE_MAC"; then echo "PASS"; else echo "NOT PROVEN"; STAGE_FAIL=1; fi
line "WinPE ready #2"
if winpe_dhcp_ack_in_range "$FIXTURE_LOG" "$B2S" "$B2E" "$BVE_MAC"; then echo "PASS"; else echo "NOT PROVEN"; STAGE_FAIL=1; fi
line "BVE reboot"
if winpe_dhcp_ack_in_range "$FIXTURE_LOG" "$B1S" "$B1E" "$BVE_MAC" \
   && winpe_dhcp_ack_in_range "$FIXTURE_LOG" "$B2S" "$B2E" "$BVE_MAC"; then echo "PASS"
else echo "NOT PROVEN"; STAGE_FAIL=1; fi

line "fixture stop"
stop_fixture && echo "ok" || die "fixture did not exit / netns not process-free"

line "teardown"
vrun teardown "$ID" && echo "ok" || { sudo "$BIN" teardown "$ID" || true; die "teardown failed"; }

line "host clean"
RESID="$(sudo iptables -S FORWARD 2>/dev/null | grep -E "${BVE_TAP}|${BVE_VETH_HOST}" || true)"
if "$BIN" verify-clean "$ID" >/dev/null 2>&1 && [[ -z "$RESID" ]]; then echo "yes"
else echo "NO"; [[ -n "$RESID" ]] && printf '  residual rule: %s\n' "$RESID" >&2; die "residual host state"; fi

CLEANED=1
trap - EXIT INT TERM
rm -f "$FIXTURE_LOG" "$RUN_LOG"
(( STAGE_FAIL == 0 )) || { echo; echo "one or more boot stages were not proven (see above)"; exit 1; }
echo; echo "Issue #71 WinPE UEFI-PXE proof: all stages PASS, two accepted boots, host clean."
