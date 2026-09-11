#!/usr/bin/env bash
#
# Bamep Issue #73 - PXE-boot the existing BARE bzImage + rootfs.cpio.gz
# (Issue #72 / ADR-0026) in one BVE over virtual UEFI (OVMF non-Secure-Boot),
# on the isolated #70 network, reusing the exact Issue #71 iPXE + snponly.efi
# 2.0.0 bootstrap unchanged (Issue #73 spike:
# docs/reference/bve-bare-uefi-pxe-host-proof.md — no BARE kernel change was
# needed or made).
#
# Versioned DEVELOPMENT HARNESS - not product runtime, not production config.
# One command drives the whole cycle and is safe to repeat:
#
#   preflight -> artifact hashes -> UEFI firmware -> setup (privileged) ->
#   start fixture (dnsmasq DHCP/TFTP + python HTTP, privileged) ->
#   run BVE UEFI-PXE-first x2 (NORMAL USER, serial capture on) ->
#   parse boot-stage evidence from TWO SEPARATE authorities (fixture log,
#   serial log), independently per boot ->
#   stop the real fixture processes deterministically -> teardown (privileged) ->
#   verify-clean -> assert zero residual BVE iptables rules.
#
# This proof NEVER builds/rebuilds BARE - run scripts/build-bare.sh first.
# It NEVER downloads snponly.efi - it is the exact Issue #71 artifact
# (scripts/winpe-pxe-fixture.sha256 is the one authoritative source for its
# hash; reuse an already-staged $BAMEP_BVE_WINPE_FIXTURE_ROOT or pass
# --snponly / $BAMEP_BVE_BARE_PXE_SNPONLY explicitly).
#
# Usage:
#   ./scripts/bve-bare-pxe-proof.sh [--verbose] [--boot-hold <secs>] \
#       [--kernel <bzImage>] [--initrd <rootfs.cpio.gz>] [--snponly <path>] \
#       [bve-id]

set -euo pipefail

VERBOSE=0
ID="bve-bare-pxe-proof"
BOOT_HOLD=45
KERNEL=""
INITRD=""
SNPONLY=""
while (($#)); do
    case "$1" in
        -v | --verbose) VERBOSE=1 ;;
        --boot-hold)
            BOOT_HOLD="$2"
            shift
            ;;
        --kernel)
            KERNEL="$2"
            shift
            ;;
        --initrd)
            INITRD="$2"
            shift
            ;;
        --snponly)
            SNPONLY="$2"
            shift
            ;;
        -h | --help)
            sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        --*)
            echo "unknown flag: $1" >&2
            exit 2
            ;;
        *) ID="$1" ;;
    esac
    shift
done

if [[ "$(id -u)" -eq 0 ]]; then
    echo "run this as your normal user - the harness calls sudo itself" >&2
    exit 2
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib/bare-pxe-evidence.sh
source "$HERE/lib/bare-pxe-evidence.sh"
# shellcheck source=scripts/lib/bare-serial-evidence.sh
source "$HERE/lib/bare-serial-evidence.sh"
cd "$HERE/.."
REPO="$PWD"
BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug/examples/bve_bare_pxe"

# ---- resolve BARE artifacts (never rebuilt) --------------------------------
if [[ -z "$KERNEL" || -z "$INITRD" ]]; then
    CACHE_ROOT="${BAMEP_BARE_CACHE_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare}"
    OUT_DIR="${BAMEP_BARE_OUTPUT:-$CACHE_ROOT/output/bamep_bare_x86_64}"
    KERNEL="${KERNEL:-$OUT_DIR/images/bzImage}"
    INITRD="${INITRD:-$OUT_DIR/images/rootfs.cpio.gz}"
fi
for f in "$KERNEL" "$INITRD"; do
    [[ -s "$f" ]] || {
        echo "BARE artifact missing: $f" >&2
        echo "build it first:  scripts/build-bare.sh" >&2
        exit 3
    }
done

# ---- resolve snponly.efi (the exact Issue #71 artifact, reused) -----------
if [[ -z "$SNPONLY" ]]; then
    SNPONLY="${BAMEP_BVE_BARE_PXE_SNPONLY:-}"
fi
if [[ -z "$SNPONLY" && -n "${BAMEP_BVE_WINPE_FIXTURE_ROOT:-}" ]]; then
    SNPONLY="$BAMEP_BVE_WINPE_FIXTURE_ROOT/snponly.efi"
fi
if [[ -z "$SNPONLY" ]]; then
    echo "no snponly.efi - pass --snponly <path>, set \$BAMEP_BVE_BARE_PXE_SNPONLY, or reuse" >&2
    echo "an already-staged \$BAMEP_BVE_WINPE_FIXTURE_ROOT from Issue #71 (same qualified" >&2
    echo "artifact; see scripts/winpe-pxe-fixture.provenance.md). Never downloaded automatically." >&2
    exit 3
fi

FIXTURE_LOG="$(mktemp -t bamep-bare-pxe-proof.XXXXXX)"
RUN_LOG="$(mktemp -t bamep-bare-pxe-runbve.XXXXXX)"
# External, controlled path for the guest serial evidence — OUTSIDE the BVE
# instance control directory, so it survives runtime.destroy() (which
# correctly, deliberately removes the internal <instance-dir>/serial.log as
# transitory control state; persistence is this proof's job, same as #72's
# bve_bare.rs --serial-out). Passed explicitly to `run-bve`; never read from
# the internal instance path after destroy().
SERIAL_LOG="$(mktemp -t bamep-bare-pxe-serial.XXXXXX)"

FIXTURE_BGPID=""
CLEANED=0

line() {
    local label="$1" width=22 pad
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
    if [[ -n "${SERIAL_LOG:-}" && -s "${SERIAL_LOG:-}" ]]; then
        echo "  --- last serial lines ---" >&2
        tail -n 30 "$SERIAL_LOG" >&2 || true
    fi
    exit 1
}
vrun() {
    if ((VERBOSE)); then echo "  + sudo $BIN $*" >&2; sudo "$BIN" "$@"
    else sudo "$BIN" "$@" >/dev/null 2>&1; fi
}

# ---- fixture process control (same pattern as #71) -------------------------
start_fixture() {
    sudo "$BIN" start-fixture "$ID" \
        --kernel "$KERNEL" --initrd "$INITRD" --snponly "$SNPONLY" \
        >"$FIXTURE_LOG" 2>&1 &
    FIXTURE_BGPID=$!
    local i dpid hpid pids
    for i in $(seq 1 100); do  # up to ~20s (staging + dnsmasq/http startup)
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
    rm -f "$FIXTURE_LOG" "$RUN_LOG" "$SERIAL_LOG"
    ((rc != 0)) && echo "  (host restore attempted after rc=$rc)" >&2
    exit "$rc"
}
trap cleanup EXIT INT TERM

# ---- proof ------------------------------------------------------------------
if ((VERBOSE)); then cargo build -p bamep-ve --example bve_bare_pxe
else cargo build -q -p bamep-ve --example bve_bare_pxe >/dev/null; fi

eval "$("$BIN" env "$ID")"
((VERBOSE)) && echo "  id=$BVE_ID mac=$BVE_MAC netns=$BVE_NETNS tap=$BVE_TAP"

line "host + artifacts"
if ARTOUT="$("$BIN" check-artifacts --kernel "$KERNEL" --initrd "$INITRD" --snponly "$SNPONLY" 2>&1)" \
    && "$BIN" check >/dev/null 2>&1; then
    echo "ok"
else
    echo "FAIL"
    printf '%s\n' "${ARTOUT:-}" >&2
    "$BIN" check || true
    exit 1
fi

line "host clean"
"$BIN" verify-clean "$ID" >/dev/null 2>&1 && echo "ok" || {
    echo "DIRTY"; die "a previous run left resources - run: sudo $BIN teardown $ID"; }

echo "priming sudo (you may be prompted once)..."
sudo -v || { echo "sudo is required" >&2; exit 2; }

line "isolated network"
vrun setup "$ID" && echo "ok" || die "setup failed"

line "PXE fixture"
start_fixture && echo "ok" || die "dnsmasq + http fixture did not become ready"

line "BVE x2 (UEFI PXE)"
set +e
"$BIN" run-bve "$ID" --evidence-log "$FIXTURE_LOG" --serial-out "$SERIAL_LOG" --boot-hold "$BOOT_HOLD" >"$RUN_LOG" 2>&1
BVE_RC=$?
set -e
((VERBOSE)) && sed 's/^/    /' "$RUN_LOG"
((BVE_RC == 0)) && echo "ok" || { sed 's/^/    /' "$RUN_LOG" >&2; die "run-bve rc=$BVE_RC"; }

range_of() { grep -oE "^$1=[0-9]+:[0-9]+\$" "$RUN_LOG" | tail -1 | cut -d= -f2 || true; }
FB1="$(range_of BVE_BOOT1_FIXTURE_RANGE)"; FB2="$(range_of BVE_BOOT2_FIXTURE_RANGE)"
SB1="$(range_of BVE_BOOT1_SERIAL_RANGE)";  SB2="$(range_of BVE_BOOT2_SERIAL_RANGE)"
[[ "$FB1" =~ ^[0-9]+:[0-9]+$ && "$FB2" =~ ^[0-9]+:[0-9]+$ ]] || die "run-bve did not emit BVE_BOOT{1,2}_FIXTURE_RANGE"
[[ "$SB1" =~ ^[0-9]+:[0-9]+$ && "$SB2" =~ ^[0-9]+:[0-9]+$ ]] || die "run-bve did not emit BVE_BOOT{1,2}_SERIAL_RANGE"
# run-bve was given --serial-out "$SERIAL_LOG" above, so this is the
# persisted copy (survives runtime.destroy(), which already ran inside
# run-bve) — never the internal instance path.
[[ -s "$SERIAL_LOG" ]] || die "run-bve did not leave a readable persisted serial log at $SERIAL_LOG"
FB1S="${FB1%%:*}"; FB1E="${FB1##*:}"; FB2S="${FB2%%:*}"; FB2E="${FB2##*:}"
SB1S="${SB1%%:*}"; SB1E="${SB1##*:}"; SB2S="${SB2%%:*}"; SB2E="${SB2##*:}"
((VERBOSE)) && echo "  boot#1 fixture $FB1 serial $SB1   boot#2 fixture $FB2 serial $SB2"

echo
STAGE_FAIL=0

# fixture_stage() <label> <fn> <fs> <fe> [args...]  - prints PASS/NOT REACHED
fixture_stage() {
    local label="$1" fn="$2"; shift 2
    line "$label"
    if "$fn" "$FIXTURE_LOG" "$@"; then echo "PASS"; return 0; fi
    echo "NOT REACHED"; STAGE_FAIL=1
}
# http_stage() <label> <fs> <fe> <path>
http_stage() {
    local label="$1" fs="$2" fe="$3" path="$4"
    line "$label"
    if http_get_200_in_range "$FIXTURE_LOG" "$fs" "$fe" "$path"; then echo "PASS"; return 0; fi
    echo "NOT REACHED"; STAGE_FAIL=1
}
# serial_stage() <label> <fn> <ss> <se>
serial_stage() {
    local label="$1" fn="$2" ss="$3" se="$4"
    line "$label"
    if "$fn" "$SERIAL_LOG" "$ss" "$se"; then echo "PASS"; return 0; fi
    echo "NOT REACHED"; STAGE_FAIL=1
}

for boot in 1 2; do
    if [[ "$boot" == 1 ]]; then FS="$FB1S"; FE="$FB1E"; SS="$SB1S"; SE="$SB1E"
    else FS="$FB2S"; FE="$FB2E"; SS="$SB2S"; SE="$SB2E"; fi
    echo "-- boot #$boot --"
    line "UEFI PXE attempt"
    if uefi_pxe_attempt_in_range "$FIXTURE_LOG" "$FS" "$FE" "$BVE_MAC"; then echo "PASS"
    else echo "NOT REACHED"; STAGE_FAIL=1; fi
    line "DHCP/bootstrap"
    if dhcp_ack_in_range "$FIXTURE_LOG" "$FS" "$FE" "$BVE_MAC"; then echo "PASS"
    else echo "NOT REACHED"; STAGE_FAIL=1; fi
    line "snponly.efi (TFTP)"
    if snponly_tftp_in_range "$FIXTURE_LOG" "$FS" "$FE"; then echo "PASS"
    elif ipxe_boot_script_selected_in_range "$FIXTURE_LOG" "$FS" "$FE"; then
        echo "N/A (firmware NIC option ROM already entered iPXE)"
    else echo "NOT REACHED"; STAGE_FAIL=1; fi
    line "iPXE / boot.ipxe select"
    if ipxe_boot_script_selected_in_range "$FIXTURE_LOG" "$FS" "$FE"; then echo "PASS"
    else echo "NOT REACHED"; STAGE_FAIL=1; fi
    http_stage "boot.ipxe (HTTP 200)" "$FS" "$FE" /boot.ipxe
    http_stage "bzImage (HTTP 200)" "$FS" "$FE" /bzImage
    http_stage "rootfs.cpio.gz (HTTP 200)" "$FS" "$FE" /rootfs.cpio.gz
    serial_stage "BARE_READY" bare_ready_in_range "$SS" "$SE"
    serial_stage "BARE_NET_READY" bare_net_ready_in_range "$SS" "$SE"
done

echo
line "repeat PXE path (BVE reboot)"
if uefi_pxe_attempt_in_range "$FIXTURE_LOG" "$FB1S" "$FB1E" "$BVE_MAC" \
    && http_get_200_in_range "$FIXTURE_LOG" "$FB1S" "$FB1E" /rootfs.cpio.gz \
    && bare_net_ready_in_range "$SERIAL_LOG" "$SB1S" "$SB1E" \
    && uefi_pxe_attempt_in_range "$FIXTURE_LOG" "$FB2S" "$FB2E" "$BVE_MAC" \
    && http_get_200_in_range "$FIXTURE_LOG" "$FB2S" "$FB2E" /rootfs.cpio.gz \
    && bare_net_ready_in_range "$SERIAL_LOG" "$SB2S" "$SB2E"; then
    echo "PASS"
else
    echo "NOT PROVEN"; STAGE_FAIL=1
fi

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
rm -f "$FIXTURE_LOG" "$RUN_LOG" "$SERIAL_LOG"
(( STAGE_FAIL == 0 )) || { echo; echo "one or more boot stages were not proven (see above)"; exit 1; }
echo; echo "Issue #73 BARE UEFI-PXE proof: all stages PASS, two accepted boots, host clean."
