#!/usr/bin/env bash
#
# Bamep Issue #61 — CP7A physical-lab launcher (LAB-ONLY operational scaffolding).
#
# =====================================================================
# THIS IS NOT APPLIANCE ARCHITECTURE.
# THIS IS NOT PRODUCTION SERVICE MANAGEMENT.
# THIS IS NOT THE Bamep FIREWALL / NETWORK / BOOT DESIGN.
# =====================================================================
#
# It is a single throwaway foreground supervisor that owns every background
# service the reusable CP7A physical attempt needs, so the operator runs
# ONE launcher in ONE terminal instead of juggling dnsmasq / HTTP / SMB /
# probe-sink / cp7-harness by hand (which previously caused avoidable
# operator errors). A separate future Discovery will define real persistent
# appliance boot / network / service behaviour once the first physical
# appliance exists — this script must NOT be mistaken for it.
#
# Intended layout:
#   Terminal 1: Claude
#   Terminal 2: this launcher
#   Terminal 3: occasional manual commands (regenerate the WinPE clock file,
#               inspect evidence, etc.)
#
# It composes ALREADY-PROVEN assets:
#   * Issue #53 Phase 9d PXE/WinPE runtime  /var/tmp/bamep-issue53-phase9d-winpe-completion
#   * Issue #60 read-only SMB PROBE share    integration/physical/issue-60-winpe-agent-slice/smb-share
#   * Issue #60 lab evidence sink            integration/physical/issue-60-winpe-agent-slice/sink
#   * Issue #61 CP7A harness + probe7        integration/physical/issue-61-endpoint-capture-data-plane
#
# CP7A SAFETY (unchanged, preserved verbatim):
#   * bounded prefix only: 2,148,532,224 bytes -> 257 chunks (final 1 MiB)
#   * PhysicalDrive0 selected by the EXISTING probe logic; GENERIC_READ only
#   * no writes to MiniPC disks; no CP7B; no full-device capture
#   * no Server raw disks; CP6 frozen runtime / lineage is never touched
#   * RF-2 / RF-6 / RF-7 remain unimplemented; no production Agent / appliance
#   * this launcher NEVER triggers the PXE boot, NEVER reboots the MiniPC and
#     NEVER starts the physical CP7A transfer — the operator does that by hand.
#
# WinPE "Press any key to continue booting..." is a KNOWN follow-up and is
# explicitly OUT OF SCOPE here.
#
# Verification (no physical capture is triggered by any of these):
#   bash -n  run-cp7a-lab.sh
#   run ShellCheck on it if available (koalaman/shellcheck)
#   ./run-cp7a-lab.sh --preflight       (read-only gate, no mutation, no services)
#
# Usage:
#   ./run-cp7a-lab.sh                 bring the CP7A lab up and supervise it
#   ./run-cp7a-lab.sh --preflight     read-only gate only (alias: --dry-run)
#   ./run-cp7a-lab.sh --help
#
# Env overrides: CP7A_LAB_IFACE (enp8s0), CP7A_LAB_IP (192.168.99.1),
#   CP7A_FW_ZONE (trusted), CP7_INTERRUPT_AFTER_HELD (8).
#
set -euo pipefail
export LC_ALL=C

# ---------------------------------------------------------------------
# 0. constants + resolved paths
# ---------------------------------------------------------------------
SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
ISSUE61_DIR="$(dirname "${SCRIPT_PATH}")"
REPO_ROOT="$(cd "${ISSUE61_DIR}/../../.." && pwd)"
ISSUE60_DIR="${REPO_ROOT}/integration/physical/issue-60-winpe-agent-slice"

LAB_IFACE="${CP7A_LAB_IFACE:-enp8s0}"
LAB_IP="${CP7A_LAB_IP:-192.168.99.1}"
LAB_CIDR="${LAB_IP}/24"
FW_ZONE="${CP7A_FW_ZONE:-trusted}"

# Ports — MUST match the cp7-harness defaults (see cp7-harness.rs) + the
# proven Phase 9d / Issue #60 services.
PORT_HTTP=8080          # Phase 9d WinPE HTTP root
PORT_SMB=445            # Issue #60 PROBE share
PORT_SINK=9099          # Issue #60 evidence sink
PORT_WSS=8443           # cp7-harness Agent WSS         (CP7_WSS_PORT)
PORT_COORD=9106         # cp7-harness lab coord + Server-UTC ACK (CP7_COORD_PORT)
PORT_DP=9107            # cp7-harness Worker HTTPS data plane   (CP7_DP_PORT)

# CP7A bounded-prefix pressure parameters (DO NOT widen — see CP7A SAFETY).
CP7A_PREFIX_BYTES=2148532224
CP7A_SEAL_TIMEOUT_SECS=120
CP7_INTERRUPT_AFTER_HELD="${CP7_INTERRUPT_AFTER_HELD:-8}"

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"
PHASE9D_HTTP="${PHASE9D_DIR}/http"
PHASE9D_DNSMASQ_CONF="${PHASE9D_DIR}/dnsmasq.conf"

SMB_SHARE="${ISSUE60_DIR}/smb-share"
SMB_SHARE_NAME="PROBE"
SINK_BIN="${ISSUE60_DIR}/sink/target/release/bamep-probe-sink"

HARNESS_BIN="${ISSUE61_DIR}/harness/target/release/cp7-harness"
STORAGE_ROOT="${ISSUE61_DIR}/harness/runtime-cp7a/chunkstore"
FINGERPRINT_FILE="${ISSUE61_DIR}/harness/runtime-cp7a/cp7-fingerprint.txt"
PROBE7_EXE="${ISSUE61_DIR}/probe7/target/x86_64-pc-windows-msvc/release/bamep-issue61-cp7a-probe.exe"

STAGED_PROBE_EXE="${SMB_SHARE}/cp7a-probe.exe"
CRED_DEST="${SMB_SHARE}/cp7a.cred"
RUN_CMD="${SMB_SHARE}/run-cp7a.cmd"
PREP_CMD="${SMB_SHARE}/prep-cp7a.cmd"
CLOCK_CMD="${SMB_SHARE}/set-clock.cmd"
CLOCK_HELPER="${ISSUE61_DIR}/stage-winpe-clock.sh"

RUN_ID="cp7a-lab-$(date +%Y%m%dT%H%M%S)"
EVID="${ISSUE61_DIR}/evidence/${RUN_ID}"
SENTINEL="${EVID}/.cleaning"

CP6_RUNTIME="${ISSUE61_DIR}/harness/runtime-cp6"

MODE="run"
MAIN_PID=$$

# ---------------------------------------------------------------------
# child-process registry (only things WE started; cleaned up together)
# ---------------------------------------------------------------------
CHILD_DESC=()
CHILD_PID=()
CHILD_SUDO=()

# runtime-only lab network state THIS launcher created (for conservative revert)
OWNED_IP=0
OWNED_FW=""          # "" = untouched; "__none__" = we added it with no prior zone; else prior zone
OWNED_NM=0

CLEANING=0

# ---------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------
LAUNCHER_LOG="/dev/null"
log()  { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "${LAUNCHER_LOG}" >&2; }
die()  { log "FATAL: $*"; exit 1; }
hr()   { log "------------------------------------------------------------------"; }

tcp_up() { ss -Hltn "sport = :$1" 2>/dev/null | grep -q .; }
udp_up() { ss -Hlun "sport = :$1" 2>/dev/null | grep -q .; }
# liveness that works for root-owned children too (kill -0 would EPERM on those)
proc_alive() { [ -n "${1:-}" ] && [ -d "/proc/$1" ]; }

# firewalld: unprivileged queries only (--state needs auth; these do not)
fw_available() { command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --get-default-zone >/dev/null 2>&1; }
fw_zone_of() {
    local z
    z="$(firewall-cmd --get-zone-of-interface="$1" 2>/dev/null)" || z=""
    [ "${z}" = "no zone" ] && z=""
    printf '%s' "${z}"
}

# await DESC RETRIES CMD...   (script-side sleep is fine; operator runs this)
await() {
    local desc="$1" tries="$2"; shift 2
    local i=0
    while ! "$@"; do
        i=$((i + 1))
        if [ "${i}" -ge "${tries}" ]; then
            log "  TIMEOUT waiting for: ${desc}"
            return 1
        fi
        sleep 0.5
    done
    log "  ready: ${desc}"
    return 0
}

register_child() { # DESC PID SUDO(0|1)
    CHILD_DESC+=("$1")
    CHILD_PID+=("$2")
    CHILD_SUDO+=("$3")
    log "  started ${1} (pid $2)"
}

require_file() { [ -f "$1" ] || die "required file missing: $1${2:+  ($2)}"; }
require_dir()  { [ -d "$1" ] || die "required directory missing: $1${2:+  ($2)}"; }

resolve_smbserver() {
    local bin="${HOME}/.local/bin/smbserver.py"
    [ -f "${bin}" ] || bin="$(command -v smbserver.py || true)"
    [ -n "${bin}" ] && [ -f "${bin}" ] || die "smbserver.py not found (impacket); expected ${HOME}/.local/bin/smbserver.py"
    SMBSERVER_PY="${bin}"
    SMB_ENV=()
    local sp
    for sp in "${HOME}"/.local/lib/python3.*/site-packages; do
        [ -d "${sp}" ] && SMB_ENV=("PYTHONPATH=${sp}")
    done
}

usage() {
    sed -n '3,54p' "${SCRIPT_PATH}" | sed 's/^#\{1,\} \{0,1\}//;s/^#$//'
}

# ---------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------
for arg in "$@"; do
    case "${arg}" in
        --preflight|--dry-run) MODE="preflight" ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: ${arg} (see --help)" ;;
    esac
done

# ---------------------------------------------------------------------
# 1. PREFLIGHT — read-only. No network mutation, no services started.
# ---------------------------------------------------------------------
hr
log "Bamep Issue #61 CP7A physical-lab launcher — ${RUN_ID}"
log "MODE=${MODE}   (LAB-ONLY scaffolding; NOT the appliance design)"
hr

log "[preflight] repository + build artifacts"
require_dir  "${ISSUE60_DIR}"                 "Issue #60 dir"
require_dir  "${SMB_SHARE}"                   "Issue #60 SMB PROBE share"
require_file "${SINK_BIN}"                    "build: cd ${ISSUE60_DIR}/sink && cargo build --release"
require_file "${HARNESS_BIN}"                 "build: cd ${ISSUE61_DIR}/harness && cargo build --release --bin cp7-harness"
require_file "${STAGED_PROBE_EXE}"            "stage the CP7A WinPE probe into the SMB share"
[ -f "${PROBE7_EXE}" ] || log "  note: freshly-built probe7 exe not found at ${PROBE7_EXE} (staged copy will be used as-is)"
require_file "${REPO_ROOT}/AGENTS.md"         "repo root sanity"
log "  ok: sink, harness, staged probe present"

log "[preflight] Issue #53 Phase 9d PXE/WinPE runtime assets"
require_dir  "${PHASE9D_DIR}"                 "run the Issue #53 Phase 9d setup first"
require_file "${PHASE9D_DNSMASQ_CONF}"        "Phase 9d dnsmasq.conf"
require_dir  "${PHASE9D_HTTP}"                "Phase 9d HTTP root"
for asset in wimboot BCD boot.sdi boot.wim; do
    require_file "${PHASE9D_HTTP}/${asset}"   "pristine Phase 9d boot asset"
done
require_dir  "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb" "Phase 9d TFTP tree"
require_file "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi" "Phase 9d Secure Boot shim"
grep -qF "interface=${LAB_IFACE}" "${PHASE9D_DNSMASQ_CONF}" \
    || die "Phase 9d dnsmasq.conf is not bound to ${LAB_IFACE}; refusing to guess"
grep -qE "^enable-tftp=${LAB_IFACE}" "${PHASE9D_DNSMASQ_CONF}" \
    || die "Phase 9d dnsmasq.conf does not enable TFTP on ${LAB_IFACE}"
log "  ok: dnsmasq.conf bound to ${LAB_IFACE}, TFTP enabled, boot assets present (NOT restaged, NOT mutated)"

log "[preflight] impacket smbserver"
resolve_smbserver
log "  ok: ${SMBSERVER_PY}  (${#SMB_ENV[@]} env override)"

log "[preflight] lab interface ${LAB_IFACE}"
ip link show "${LAB_IFACE}" >/dev/null 2>&1 || die "interface ${LAB_IFACE} does not exist"
HAVE_IP=0; ip -4 addr show "${LAB_IFACE}" | grep -qF "inet ${LAB_CIDR}" && HAVE_IP=1
CUR_FW_ZONE=""
FW_OK=0
if fw_available; then FW_OK=1; CUR_FW_ZONE="$(fw_zone_of "${LAB_IFACE}")"; fi
log "  ${LAB_IFACE}: ${LAB_CIDR} present=${HAVE_IP}   firewalld=${FW_OK} zone='${CUR_FW_ZONE:-<none>}' (want '${FW_ZONE}')"

log "[preflight] no conflicting listeners on the lab ports"
CONFLICT=0
for p in "${PORT_HTTP}" "${PORT_SMB}" "${PORT_SINK}" "${PORT_WSS}" "${PORT_COORD}" "${PORT_DP}"; do
    if tcp_up "${p}"; then log "  CONFLICT: something already listens on tcp/${p}"; CONFLICT=1; fi
done
if udp_up 67; then log "  CONFLICT: something already listens on udp/67 (DHCP)"; CONFLICT=1; fi
if udp_up 69; then log "  CONFLICT: something already listens on udp/69 (TFTP)"; CONFLICT=1; fi
[ "${CONFLICT}" -eq 0 ] || die "resolve the listed port conflicts before launching (another lab session still running?)"
log "  ok: all lab ports free"

log "[preflight] storage root free space (bounded ~2.15 GB Artifact)"
require_dir "$(dirname "${STORAGE_ROOT}")" "harness runtime-cp7a parent"
STORAGE_DF_PATH="${STORAGE_ROOT}"
[ -d "${STORAGE_DF_PATH}" ] || STORAGE_DF_PATH="$(dirname "${STORAGE_ROOT}")"
FREE_BYTES="$(df -B1 --output=avail "${STORAGE_DF_PATH}" | tail -1 | tr -d ' ')"
log "  ${STORAGE_ROOT}: free=${FREE_BYTES} bytes"
[ "${FREE_BYTES}" -ge 3000000000 ] || die "need >= 3,000,000,000 bytes free under the storage root"
if [ -d "${STORAGE_ROOT}/transfers" ] && [ -n "$(ls -A "${STORAGE_ROOT}/transfers" 2>/dev/null)" ]; then
    log "  note: ${STORAGE_ROOT}/transfers is non-empty (leftover chunk dirs from a prior run;"
    log "        harmless — each CP7A run mints a fresh transfer_id — but inspect if disk is tight)"
fi

log "[preflight] CP6 frozen runtime is present and will NOT be touched"
[ -d "${CP6_RUNTIME}" ] && log "  ${CP6_RUNTIME} exists (frozen; launcher never reads/writes it)"

if [ "${MODE}" = "preflight" ]; then
    hr
    log "PREFLIGHT OK — no network state changed, no services started."
    log "Run without --preflight to bring the CP7A lab up."
    hr
    exit 0
fi

# ---------------------------------------------------------------------
# 2. evidence directory + launcher log
# ---------------------------------------------------------------------
mkdir -p "${EVID}"
LAUNCHER_LOG="${EVID}/launcher.log"
: > "${LAUNCHER_LOG}"
log "evidence directory: ${EVID}"
{
    echo "run_id            ${RUN_ID}"
    echo "started_at        $(date -Is)"
    echo "repo_root         ${REPO_ROOT}"
    echo "lab_iface         ${LAB_IFACE}"
    echo "lab_ip            ${LAB_IP}"
    echo "phase9d_dir       ${PHASE9D_DIR}"
    echo "phase9d_http      ${PHASE9D_HTTP}"
    echo "smb_share         ${SMB_SHARE}"
    echo "sink_bin          ${SINK_BIN}"
    echo "harness_bin       ${HARNESS_BIN}"
    echo "storage_root      ${STORAGE_ROOT}"
    echo "staged_probe_exe  ${STAGED_PROBE_EXE}"
    echo "ports             http=${PORT_HTTP} smb=${PORT_SMB} sink=${PORT_SINK} wss=${PORT_WSS} coord=${PORT_COORD} dp=${PORT_DP}"
    echo "cp7a_prefix_bytes ${CP7A_PREFIX_BYTES}"
    echo "cp7_interrupt_after_held ${CP7_INTERRUPT_AFTER_HELD}"
} > "${EVID}/resolved-paths.txt"

# ---------------------------------------------------------------------
# 3. cleanup / supervision trap  (installed BEFORE any mutation)
# ---------------------------------------------------------------------
# shellcheck disable=SC2329  # invoked via `trap ... EXIT`
cleanup() {
    [ "${CLEANING}" -eq 1 ] && return
    CLEANING=1
    touch "${SENTINEL}" 2>/dev/null || true
    set +e
    [ -n "${WATCHDOG_PID:-}" ] && kill -TERM "${WATCHDOG_PID}" 2>/dev/null
    echo
    hr
    log "CLEANUP — stopping only the children THIS launcher started"

    # stop children in reverse order
    local idx
    for (( idx=${#CHILD_PID[@]}-1 ; idx>=0 ; idx-- )); do
        local pid="${CHILD_PID[$idx]}" desc="${CHILD_DESC[$idx]}" sudo="${CHILD_SUDO[$idx]}"
        if ! proc_alive "${pid}"; then
            log "  ${desc} (pid ${pid}) already gone"
            continue
        fi
        if [ "${sudo}" -eq 1 ]; then
            # sudo forwards TERM to its single child; also TERM that child directly
            # by parent-pid (NOT by name) so nothing is orphaned as root.
            sudo pkill -TERM -P "${pid}" 2>/dev/null
            sudo kill  -TERM    "${pid}" 2>/dev/null
        else
            kill -TERM "${pid}" 2>/dev/null
        fi
        local w=0
        while proc_alive "${pid}" && [ "${w}" -lt 20 ]; do sleep 0.25; w=$((w + 1)); done
        if proc_alive "${pid}"; then
            if [ "${sudo}" -eq 1 ]; then
                sudo pkill -KILL -P "${pid}" 2>/dev/null
                sudo kill  -KILL    "${pid}" 2>/dev/null
            else
                kill -KILL "${pid}" 2>/dev/null
            fi
        fi
        wait "${pid}" 2>/dev/null || true
        log "  stopped ${desc} (pid ${pid})"
    done

    # revert ONLY runtime-only lab network state we created ourselves
    if [ -n "${OWNED_FW}" ]; then
        if [ "${OWNED_FW}" = "__none__" ]; then
            sudo firewall-cmd --zone="${FW_ZONE}" --remove-interface="${LAB_IFACE}" >/dev/null 2>&1 \
                && log "  reverted: removed ${LAB_IFACE} from firewalld zone '${FW_ZONE}'"
        else
            sudo firewall-cmd --zone="${OWNED_FW}" --change-interface="${LAB_IFACE}" >/dev/null 2>&1 \
                && log "  reverted: returned ${LAB_IFACE} to firewalld zone '${OWNED_FW}'"
        fi
    fi
    if [ "${OWNED_IP}" -eq 1 ]; then
        if ip -4 addr show "${LAB_IFACE}" | grep -qF "inet ${LAB_CIDR}"; then
            sudo ip addr del "${LAB_CIDR}" dev "${LAB_IFACE}" 2>/dev/null \
                && log "  reverted: removed ${LAB_CIDR} from ${LAB_IFACE}"
        fi
    fi
    if [ "${OWNED_NM}" -eq 1 ]; then
        sudo nmcli device set "${LAB_IFACE}" managed yes >/dev/null 2>&1 \
            && log "  reverted: returned ${LAB_IFACE} to NetworkManager management"
    fi

    if [ "${OWNED_IP}" -eq 0 ] && [ -z "${OWNED_FW}" ] && [ "${OWNED_NM}" -eq 0 ]; then
        log "  network: this launcher changed NOTHING — the pre-existing bounded lab"
        log "           runtime state (${LAB_CIDR} on ${LAB_IFACE}, firewalld zone"
        log "           '${CUR_FW_ZONE:-<none>}') is left exactly as found."
    fi

    log "  credential ${CRED_DEST} left in place (git-ignored, mode 600; overwritten next run)."
    log "  evidence preserved at: ${EVID}"
    log "CP7A lab down."
    hr
}
# shellcheck disable=SC2329  # invoked via `trap ... INT TERM`
on_signal() { trap - INT TERM; echo; log "signal received — shutting the CP7A lab down"; exit 130; }
trap on_signal INT TERM
trap cleanup EXIT

# ---------------------------------------------------------------------
# 4. LAB NETWORK RUNTIME  (check first, mutate only what is missing)
# ---------------------------------------------------------------------
hr
log "LAB NETWORK RUNTIME  (runtime-only; NOT the Bamep appliance firewall/network design)"

if [ "${HAVE_IP}" -eq 1 ]; then
    log "  ${LAB_CIDR} already on ${LAB_IFACE} — leaving it (not ours to remove)"
else
    if nmcli -t -f DEVICE,STATE device status 2>/dev/null | grep -qE "^${LAB_IFACE}:connected$"; then
        sudo nmcli device set "${LAB_IFACE}" managed no
        OWNED_NM=1
        log "  set ${LAB_IFACE} unmanaged in NetworkManager (RUNTIME ONLY — no persistent profile created)"
    fi
    sudo ip addr add "${LAB_CIDR}" dev "${LAB_IFACE}"
    sudo ip link set "${LAB_IFACE}" up
    OWNED_IP=1
    if ip -4 addr show | grep -F "inet ${LAB_IP}/" | grep -qv "${LAB_IFACE}"; then
        die "${LAB_IP} leaked onto another interface — aborting"
    fi
    log "  added ${LAB_CIDR} to ${LAB_IFACE} (runtime only)"
fi

if [ "${FW_OK}" -eq 1 ]; then
    if [ "${CUR_FW_ZONE}" = "${FW_ZONE}" ]; then
        log "  ${LAB_IFACE} already in firewalld zone '${FW_ZONE}' — leaving it (not ours to change)"
    else
        OWNED_FW="${CUR_FW_ZONE:-__none__}"
        sudo firewall-cmd --zone="${FW_ZONE}" --change-interface="${LAB_IFACE}" >/dev/null
        log "  moved ${LAB_IFACE} into firewalld RUNTIME zone '${FW_ZONE}' (was '${OWNED_FW}')"
        log "  >>> THIS IS A TEMPORARY LAB SHORTCUT, NOT THE APPLIANCE FIREWALL DESIGN <<<"
        log "  >>> no --permanent change is made; it is reverted on launcher exit      <<<"
    fi
else
    log "  firewalld not queryable — nothing to scope (assuming an already-isolated lab link)"
fi

# ---------------------------------------------------------------------
# 5. BACKGROUND LAB SERVICES  (each: start -> register -> readiness gate)
# ---------------------------------------------------------------------
hr
log "STARTING BACKGROUND LAB SERVICES"

# 5a. dnsmasq — DHCP + TFTP, from the pristine Phase 9d conf (unchanged)
log "[dnsmasq] DHCP + TFTP on ${LAB_IFACE} (Phase 9d conf, verbatim)"
# SC2024: the log file is deliberately opened by THIS user in the evidence dir;
# root dnsmasq just inherits the fd. We do not want a root-owned log.
# shellcheck disable=SC2024
sudo dnsmasq -d --conf-file="${PHASE9D_DNSMASQ_CONF}" > "${EVID}/dnsmasq.log" 2>&1 &
register_child "dnsmasq" "$!" 1
await "dnsmasq udp/67 (DHCP)" 40 udp_up 67 || die "dnsmasq did not bind udp/67 — see ${EVID}/dnsmasq.log"
await "dnsmasq udp/69 (TFTP)" 20 udp_up 69 || die "dnsmasq did not bind udp/69 — see ${EVID}/dnsmasq.log"

# 5b. WinPE HTTP root (Phase 9d), bound only to the lab IP
log "[http] python3 http.server on ${LAB_IP}:${PORT_HTTP} serving ${PHASE9D_HTTP}"
python3 -m http.server --bind "${LAB_IP}" --directory "${PHASE9D_HTTP}" "${PORT_HTTP}" \
    > "${EVID}/http.log" 2>&1 &
register_child "winpe-http" "$!" 0
await "http tcp/${PORT_HTTP}" 30 tcp_up "${PORT_HTTP}" || die "HTTP server did not start — see ${EVID}/http.log"

# 5c. read-only SMB PROBE share (Issue #60 lineage)
log "[smb] impacket smbserver -readonly share ${SMB_SHARE_NAME} -> ${SMB_SHARE}"
# shellcheck disable=SC2024  # log fd opened by this user on purpose (see dnsmasq note)
sudo env "${SMB_ENV[@]}" \
    python3 "${SMBSERVER_PY}" -readonly -smb2support -ip "${LAB_IP}" \
    "${SMB_SHARE_NAME}" "${SMB_SHARE}" > "${EVID}/smb.log" 2>&1 &
register_child "smb (PROBE)" "$!" 1
await "smb tcp/${PORT_SMB}" 30 tcp_up "${PORT_SMB}" || die "smbserver did not bind tcp/445 — see ${EVID}/smb.log"

# 5d. probe evidence sink — fresh per-run evidence filename
SINK_NDJSON="${EVID}/probe-sink-${RUN_ID}.ndjson"
log "[sink] bamep-probe-sink on ${LAB_IP}:${PORT_SINK} -> ${SINK_NDJSON}"
"${SINK_BIN}" "${LAB_IP}:${PORT_SINK}" "${SINK_NDJSON}" > "${EVID}/sink.log" 2>&1 &
register_child "probe-sink" "$!" 0
await "sink tcp/${PORT_SINK}" 30 tcp_up "${PORT_SINK}" || die "probe-sink did not bind tcp/${PORT_SINK} — see ${EVID}/sink.log"

# 5e. FRESH cp7-harness — explicit storage root + explicit interruption threshold
HARNESS_LOG="${EVID}/cp7-harness.log"
log "[cp7-harness] fresh process; CP7_INTERRUPT_AFTER_HELD=${CP7_INTERRUPT_AFTER_HELD}; --storage-root ${STORAGE_ROOT}"
env CP7_INTERRUPT_AFTER_HELD="${CP7_INTERRUPT_AFTER_HELD}" \
    CP7_LAB_IP="${LAB_IP}" CP7_WSS_PORT="${PORT_WSS}" \
    CP7_COORD_PORT="${PORT_COORD}" CP7_DP_PORT="${PORT_DP}" \
    "${HARNESS_BIN}" --storage-root "${STORAGE_ROOT}" > "${HARNESS_LOG}" 2>&1 &
HARNESS_PID=$!
register_child "cp7-harness" "${HARNESS_PID}" 0

await "cp7-harness Worker HTTPS (worker.https_listening)" 120 \
    grep -q '"event":"worker.https_listening"' "${HARNESS_LOG}" \
    || die "cp7-harness never reported worker.https_listening — see ${HARNESS_LOG}"
await "cp7-harness WSS tcp/${PORT_WSS}"   40 tcp_up "${PORT_WSS}"  || die "no WSS listener — see ${HARNESS_LOG}"
await "cp7-harness coord tcp/${PORT_COORD}" 40 tcp_up "${PORT_COORD}" || die "no coord listener — see ${HARNESS_LOG}"
await "cp7-harness data-plane tcp/${PORT_DP}" 40 tcp_up "${PORT_DP}" || die "no data-plane listener — see ${HARNESS_LOG}"

# ---------------------------------------------------------------------
# 6. harness fingerprint
# ---------------------------------------------------------------------
FINGERPRINT="$(grep -oE '"server_leaf_sha256":"[0-9a-f]{64}"' "${HARNESS_LOG}" | head -1 | grep -oE '[0-9a-f]{64}' || true)"
if [ -z "${FINGERPRINT}" ] && [ -f "${FINGERPRINT_FILE}" ]; then
    FINGERPRINT="$(tr -d '[:space:]' < "${FINGERPRINT_FILE}")"
fi
[ "${#FINGERPRINT}" -eq 64 ] || die "could not obtain a 64-hex harness leaf fingerprint — see ${HARNESS_LOG}"
printf '%s\n' "${FINGERPRINT}" > "${EVID}/fingerprint.txt"
log "harness leaf-cert SHA-256 fingerprint captured (${EVID}/fingerprint.txt)"

# ---------------------------------------------------------------------
# 7. FRESH CP7A enrollment credential  (exactly one; never printed)
# ---------------------------------------------------------------------
log "[credential] minting exactly one fresh CP7A enrollment credential"
CRED_TMP="$(mktemp "${EVID}/.cred.XXXXXX")"
chmod 600 "${CRED_TMP}"
if ! ( umask 077; "${HARNESS_BIN}" issue-credential "${RUN_ID}" \
        > "${CRED_TMP}" 2> "${EVID}/credential-issue.stderr.log" ); then
    rm -f "${CRED_TMP}"
    die "cp7-harness issue-credential failed — see ${EVID}/credential-issue.stderr.log"
fi
[ -s "${CRED_TMP}" ] || { rm -f "${CRED_TMP}"; die "issue-credential produced an empty credential"; }
( umask 077; install -m 600 "${CRED_TMP}" "${CRED_DEST}" )
shred -u "${CRED_TMP}" 2>/dev/null || rm -f "${CRED_TMP}"
CRED_MODE="$(stat -c '%a' "${CRED_DEST}")"
[ "${CRED_MODE}" = "600" ] || die "staged credential ${CRED_DEST} has mode ${CRED_MODE}, expected 600"
log "  fresh credential written to ${CRED_DEST} (mode 600, value NOT logged)"

# ---------------------------------------------------------------------
# 8. WinPE run-command staging  (fingerprint substituted; NO stale clock)
# ---------------------------------------------------------------------
log "[stage] regenerating ${RUN_CMD} with the current harness fingerprint"
cat > "${RUN_CMD}" <<EOF
@echo off
rem  Bamep Issue #61 CP7A — staged by run-cp7a-lab.sh (${RUN_ID})
rem  LAB-ONLY. Bounded prefix ${CP7A_PREFIX_BYTES} bytes / 257 chunks. GENERIC_READ only.
rem  Does NOT start automatically — the operator runs this by hand inside WinPE.
X:\\cp7a-probe.exe ^
  --wss ${LAB_IP}:${PORT_WSS} ^
  --coord ${LAB_IP}:${PORT_COORD} ^
  --sink ${LAB_IP}:${PORT_SINK} ^
  --pin ${FINGERPRINT} ^
  --auth-credential-file X:\\cp7a.cred ^
  --prefix-bytes ${CP7A_PREFIX_BYTES} ^
  --seal-timeout-secs ${CP7A_SEAL_TIMEOUT_SECS}
set "CP7A_EXITCODE=%ERRORLEVEL%"
echo.
echo CP7A_EXITCODE=%CP7A_EXITCODE%
exit /b %CP7A_EXITCODE%
EOF
chmod 600 "${RUN_CMD}"

log "[stage] regenerating ${PREP_CMD}  (copies probe + credential + run-cmd to X:\\)"
cat > "${PREP_CMD}" <<'EOF'
@echo off
rem  Bamep Issue #61 CP7A prep — copy the probe + this run's credential + the run
rem  wrapper to X:\ so the transfer no longer depends on SMB after prep.
rem  Map the share first, e.g.:  net use P: \\192.168.99.1\PROBE
copy /Y P:\cp7a-probe.exe X:\cp7a-probe.exe
copy /Y P:\cp7a.cred X:\cp7a.cred
copy /Y P:\run-cp7a.cmd X:\run-cp7a.cmd
echo CP7A_PREP_DONE
echo NEXT: run stage-winpe-clock.sh in Terminal 3, then  P:\set-clock.cmd  here, then  X:\run-cp7a.cmd
EOF
chmod 600 "${PREP_CMD}"

# The WinPE clock file is deliberately NOT generated now: it must be created only
# once the WinPE CMD prompt is actually reached (a PXE/WinPE boot can exceed the
# probe's 60 s clock-freshness floor). Clear any stale leftover from a prior run.
rm -f "${CLOCK_CMD}"
if [ -x "${CLOCK_HELPER}" ]; then
    log "  clock: NOT staged yet — run ${CLOCK_HELPER} in Terminal 3 AFTER the WinPE CMD prompt is reached"
else
    log "  warn: ${CLOCK_HELPER} not executable — the operator cannot stage the WinPE clock file"
fi

# ---------------------------------------------------------------------
# 9. READINESS GATE — verify every boundary, then and only then print READY
# ---------------------------------------------------------------------
hr
log "READINESS GATE"
gate_fail=0
gate() { # DESC CMD...
    local d="$1"; shift
    if "$@"; then log "  PASS  ${d}"; else log "  FAIL  ${d}"; gate_fail=1; fi
}
gate "${LAB_IFACE} has ${LAB_CIDR}"            bash -c "ip -4 addr show '${LAB_IFACE}' | grep -qF 'inet ${LAB_CIDR}'"
if [ "${FW_OK}" -eq 1 ]; then
    gate "${LAB_IFACE} in firewalld zone '${FW_ZONE}'" \
        bash -c "[ \"\$(firewall-cmd --get-zone-of-interface='${LAB_IFACE}' 2>/dev/null)\" = '${FW_ZONE}' ]"
fi
gate "udp/67 (DHCP) listening"                 udp_up 67
gate "udp/69 (TFTP) listening"                 udp_up 69
gate "tcp/${PORT_HTTP} (WinPE HTTP) listening" tcp_up "${PORT_HTTP}"
gate "tcp/${PORT_SMB} (SMB PROBE) listening"   tcp_up "${PORT_SMB}"
gate "tcp/${PORT_SINK} (probe sink) listening" tcp_up "${PORT_SINK}"
gate "tcp/${PORT_WSS} (Agent WSS) listening"   tcp_up "${PORT_WSS}"
gate "tcp/${PORT_COORD} (coord) listening"     tcp_up "${PORT_COORD}"
gate "tcp/${PORT_DP} (Worker HTTPS) listening" tcp_up "${PORT_DP}"
gate "cp7-harness worker.https_listening"      grep -q '"event":"worker.https_listening"' "${HARNESS_LOG}"
gate "harness fingerprint obtained"            bash -c "[ '${#FINGERPRINT}' -eq 64 ]"
gate "cp7a.cred present and mode 600"          bash -c "[ \"\$(stat -c '%a' '${CRED_DEST}' 2>/dev/null)\" = '600' ]"
gate "Phase 9d boot assets present"            bash -c "[ -f '${PHASE9D_HTTP}/wimboot' ] && [ -f '${PHASE9D_HTTP}/boot.wim' ] && [ -f '${PHASE9D_HTTP}/BCD' ] && [ -f '${PHASE9D_HTTP}/boot.sdi' ]"
gate "staged CP7A probe present"               bash -c "[ -f '${STAGED_PROBE_EXE}' ]"
gate "staged run-cp7a.cmd carries this fingerprint" grep -qF "${FINGERPRINT}" "${RUN_CMD}"

# per-child liveness
for i in "${!CHILD_PID[@]}"; do
    gate "child alive: ${CHILD_DESC[$i]} (pid ${CHILD_PID[$i]})" proc_alive "${CHILD_PID[$i]}"
done

{
    echo "run_id       ${RUN_ID}"
    echo "ready_at     $(date -Is)"
    echo "fingerprint  ${FINGERPRINT}"
    echo "gate_fail    ${gate_fail}"
    echo
    for i in "${!CHILD_PID[@]}"; do
        printf 'pid %-8s sudo=%s  %s\n' "${CHILD_PID[$i]}" "${CHILD_SUDO[$i]}" "${CHILD_DESC[$i]}"
    done
} > "${EVID}/readiness-summary.txt"
cp "${EVID}/readiness-summary.txt" "${EVID}/pids.txt"

if [ "${gate_fail}" -ne 0 ]; then
    die "readiness gate FAILED — see the FAIL lines above; the lab is NOT ready. Cleaning up."
fi

# ---------------------------------------------------------------------
# 10. health watchdog for the non-foreground children
# ---------------------------------------------------------------------
watchdog() {
    set +e
    local names=("${CHILD_DESC[@]}") pids=("${CHILD_PID[@]}")
    while :; do
        sleep 3
        if [ -e "${SENTINEL}" ]; then return 0; fi
        local k
        for k in "${!pids[@]}"; do
            if ! proc_alive "${pids[$k]}"; then
                log ""
                log "!!! ${names[$k]} (pid ${pids[$k]}) EXITED UNEXPECTEDLY — lab is NOT READY"
                log "!!! tearing the rest of the CP7A lab down coherently"
                touch "${SENTINEL}" 2>/dev/null
                kill -TERM "${MAIN_PID}" 2>/dev/null
                return 0
            fi
        done
    done
}
watchdog &
WATCHDOG_PID=$!
log "health watchdog running (pid ${WATCHDOG_PID})"

# ---------------------------------------------------------------------
# 11. READY — operator block
# ---------------------------------------------------------------------
cat <<EOF | tee -a "${LAUNCHER_LOG}"

==================================================
BAMEP CP7A LAB READY   (${RUN_ID})
==================================================
PXE/DHCP/TFTP : READY   (dnsmasq, Phase 9d conf, ${LAB_IFACE})
WinPE HTTP    : READY   (${LAB_IP}:${PORT_HTTP}  ${PHASE9D_HTTP})
SMB           : READY   (\\\\${LAB_IP}\\${SMB_SHARE_NAME}  read-only  ${SMB_SHARE})
Probe sink    : READY   (${LAB_IP}:${PORT_SINK})
CP7 harness   : READY   (fresh process, CP7_INTERRUPT_AFTER_HELD=${CP7_INTERRUPT_AFTER_HELD})
Worker HTTPS  : READY   (${LAB_IP}:${PORT_DP})
WSS           : READY   (${LAB_IP}:${PORT_WSS})
Coord         : READY   (${LAB_IP}:${PORT_COORD})

Fingerprint:
  ${FINGERPRINT}

Bounded CP7A extent (UNCHANGED SAFETY):
  ${CP7A_PREFIX_BYTES} bytes = 257 chunks (final 1 MiB) | GENERIC_READ only | no CP7B | no full-device | CP6 untouched

Evidence:
  ${EVID}
    launcher.log  dnsmasq.log  http.log  smb.log  sink.log  cp7-harness.log
    probe-sink-${RUN_ID}.ndjson  fingerprint.txt  readiness-summary.txt

Staged into the SMB share (git-ignored):
  ${CRED_DEST}    (fresh, mode 600 — one per launcher run)
  ${RUN_CMD}
  ${PREP_CMD}
  set-clock.cmd is NOT staged yet — generated on demand once WinPE CMD is up

Next (ALL MANUAL — this launcher does none of it):
  1. PXE boot the MiniPC (192.168.99.66) by hand.
     (WinPE "Press any key to continue booting..." is a KNOWN follow-up, not solved here.)
  2. Wait for the WinPE command prompt to actually appear.
  3. Inside WinPE — map the share and prep (copies probe + cred + run-cmd to X:\\):
       net use P: \\\\${LAB_IP}\\${SMB_SHARE_NAME}
       P:\\prep-cp7a.cmd
  4. NOW that WinPE CMD is up, align the clock (stock-WinPE Bias workaround):
       Terminal 3 :  ${CLOCK_HELPER}
       then in WinPE, immediately:  P:\\set-clock.cmd
  5. Run the CP7A command from X:\\ (no SMB dependency after prep):
       X:\\run-cp7a.cmd

DO NOT start the physical transfer automatically.

Launcher is now the lab session supervisor. Ctrl-C to stop everything cleanly.
==================================================
EOF

# ---------------------------------------------------------------------
# 12. foreground supervise — block on the harness; watchdog covers the rest
# ---------------------------------------------------------------------
# The harness is meant to run until the operator stops the launcher (Ctrl-C),
# which on_signal handles and which never reaches the code below. Reaching here
# means cp7-harness exited on its own: an unexpected required-child exit. Quiesce
# the watchdog, then exit NON-ZERO after cleanup — never report success here.
harness_rc=0
wait "${HARNESS_PID}" 2>/dev/null || harness_rc=$?
touch "${SENTINEL}" 2>/dev/null || true
trap '' INT TERM   # ignore a racing watchdog TERM; the EXIT trap still runs cleanup
[ "${harness_rc}" -ne 0 ] || harness_rc=1
log "!!! cp7-harness (pid ${HARNESS_PID}) EXITED UNEXPECTEDLY (rc=${harness_rc}) while supervising — tearing the CP7A lab down"
exit "${harness_rc}"
