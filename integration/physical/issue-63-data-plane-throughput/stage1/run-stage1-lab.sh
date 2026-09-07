#!/usr/bin/env bash
#
# Bamep Issue #63 Stage 1 — physical-lab launcher (LAB-ONLY operational scaffolding).
#
# =====================================================================
# THIS IS NOT APPLIANCE ARCHITECTURE. NOT PRODUCTION SERVICE MANAGEMENT.
# NOT THE Bamep FIREWALL / NETWORK / BOOT DESIGN.
# =====================================================================
#
# A single throwaway foreground supervisor that owns every background service
# Stage 1 needs, so the operator runs ONE launcher in ONE terminal. It composes
# ALREADY-PROVEN assets:
#   * Issue #53 Phase 9d PXE/WinPE runtime   /var/tmp/bamep-issue53-phase9d-winpe-completion
#       (consumed READ-ONLY; a derived Issue-63 runtime is built beside it, the
#        originals are re-hashed before and after and never modified)
#   * Issue #63 Stage-1 coordinator + WinPE runner (this directory)
#
# Stage 1 proves ONLY the risky automation primitives: derived PXE/WinPE runtime
# -> WinPE auto-starts the Issue-63 runner after wpeinit -> network ready ->
# runner reaches the Fedora coordinator -> Server UTC obtained -> WinPE system
# clock aligned automatically (SetSystemTime, UTC) -> skew re-checked -> READY.
#
# NO bulk disk read. NO Transfer. NO Artifact. NO matrix. The launcher NEVER
# triggers the PXE boot and NEVER powers the MiniPC.
#
# Verification (no physical boot triggered by any of these):
#   bash -n run-stage1-lab.sh
#   ./run-stage1-lab.sh --preflight        (read-only gate; no mutation, no services)
#   ./run-stage1-lab.sh --selftest-exit <state>   (exit-code propagation only)
#
# Launcher exit status:
#   physical_pass / host_smoke_pass verdict -> 0
#   physical_fail / host_smoke_fail verdict -> 10  (expected terminal, experiment FAILED)
#   a lab service died before any verdict    -> 20
#   Ctrl-C before any verdict                -> 130
#   setup/readiness failure                  -> 1
#
# Usage:
#   ./run-stage1-lab.sh                 bring the Stage-1 lab up and supervise it
#   ./run-stage1-lab.sh --preflight     read-only gate only (alias: --dry-run)
#   ./run-stage1-lab.sh --selftest-exit physical_pass|physical_fail|host_smoke_pass|host_smoke_fail|watchdog-abort|owner-ctrlc
#   ./run-stage1-lab.sh --help
#
# Env overrides: I63_LAB_IFACE (enp8s0), I63_LAB_IP (192.168.99.1),
#   I63_FW_ZONE (trusted), I63_SKEW_FLOOR_MS (-2000), I63_SKEW_CEIL_MS (2000),
#   I63_NET_WAIT_SECS (120).
#
set -euo pipefail
export LC_ALL=C

SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
STAGE1_DIR="$(dirname "${SCRIPT_PATH}")"
I63_DIR="$(dirname "${STAGE1_DIR}")"
REPO_ROOT="$(cd "${I63_DIR}/../../.." && pwd)"

LAB_IFACE="${I63_LAB_IFACE:-enp8s0}"
LAB_IP="${I63_LAB_IP:-192.168.99.1}"
LAB_CIDR="${LAB_IP}/24"
FW_ZONE="${I63_FW_ZONE:-trusted}"

PORT_HTTP=8080
PORT_COORD=9206
PORT_SINK=9299

SKEW_FLOOR_MS="${I63_SKEW_FLOOR_MS:--2000}"
SKEW_CEIL_MS="${I63_SKEW_CEIL_MS:-2000}"
NET_WAIT_SECS="${I63_NET_WAIT_SECS:-120}"

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"
COORD_BIN="${I63_DIR}/coordinator/target/release/bamep-i63-stage1-coordinator"
RUNNER_EXE="${I63_DIR}/winpe-runner/target/x86_64-pc-windows-msvc/release/bamep-i63-runner.exe"
DERIVE="${STAGE1_DIR}/derive-stage1-runtime.sh"

# Pinned Phase-9d asset hashes (mirror of derive-stage1-runtime.sh; the launcher
# re-checks independently so a preflight catches drift before any service start).
PINNED_HTTP_WIMBOOT="5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3"
PINNED_HTTP_BCD="c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41"
PINNED_HTTP_BOOTSDI="cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd"
PINNED_HTTP_BOOTWIM="fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197"
PINNED_TFTP_SHIM="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
PINNED_TFTP_SNPONLY="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
PINNED_TFTP_IPXE="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"

# Stock-WinPE-proven DLL set (#60/#61 evidence). The Stage-1 runner must import a
# SUBSET of this — a new name is a STOP-and-report condition.
PROVEN_DLLS="api-ms-win-core-synch-l1-2-0.dll kernel32.dll ntdll.dll ws2_32.dll advapi32.dll bcrypt.dll bcryptprimitives.dll"

RUN_ID="stage1-$(date +%Y%m%dT%H%M%S)"
EVID="${I63_DIR}/evidence/${RUN_ID}"
SENTINEL=""
# Written by the coordinator when it reaches a terminal verdict, just before it
# exits. Its existence tells the watchdog an "expected terminal exit" from a
# crash; its content is the verdict (physical_pass / physical_fail / ...).
COORD_VERDICT=""
# Touched by the watchdog ONLY on its unexpected-death branch (a lab service died
# before any terminal verdict) so the exit-code resolver can tell that from an
# owner Ctrl-C.
WATCHDOG_ABORT=""
MODE="run"
MAIN_PID=$$

CHILD_DESC=(); CHILD_PID=(); CHILD_SUDO=()
OWNED_IP=0; OWNED_FW=""; OWNED_NM=0
CLEANING=0
WATCHDOG_PID=""
LAUNCHER_LOG="/dev/null"

log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "${LAUNCHER_LOG}" >&2; }
die() { log "FATAL: $*"; exit 1; }
hr()  { log "------------------------------------------------------------------"; }

# Launcher exit code for the current terminal state. A *_fail verdict is an
# EXPECTED terminal completion of the lifecycle, but it is still an experiment
# FAILURE and must NOT be shell success.
#   physical_pass  | host_smoke_pass  -> 0
#   physical_fail  | host_smoke_fail  -> 10
#   unknown verdict marker            -> 11
#   no verdict + watchdog abort (a service died before any verdict) -> 20
#   no verdict + no abort  (owner Ctrl-C before any verdict)        -> 130
resolve_terminal_exit() {
  if [ -f "${COORD_VERDICT}" ]; then
    case "$(cat "${COORD_VERDICT}" 2>/dev/null)" in
      physical_pass | host_smoke_pass) echo 0 ;;
      physical_fail | host_smoke_fail) echo 10 ;;
      *) echo 11 ;;
    esac
  elif [ -f "${WATCHDOG_ABORT}" ]; then
    echo 20
  else
    echo 130
  fi
}

tcp_up() { ss -Hltn "sport = :$1" 2>/dev/null | grep -q .; }
udp_up() { ss -Hlun "sport = :$1" 2>/dev/null | grep -q .; }
proc_alive() { [ -n "${1:-}" ] && [ -d "/proc/$1" ]; }
fw_available() { command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --get-default-zone >/dev/null 2>&1; }
fw_zone_of() { local z; z="$(firewall-cmd --get-zone-of-interface="$1" 2>/dev/null)" || z=""; [ "${z}" = "no zone" ] && z=""; printf '%s' "${z}"; }
hash_of() { sha256sum "$1" | awk '{print $1}'; }
require_file() { [ -f "$1" ] || die "required file missing: $1${2:+  ($2)}"; }

await() {
  local desc="$1" tries="$2"; shift 2
  local i=0
  while ! "$@"; do
    i=$((i + 1))
    [ "${i}" -ge "${tries}" ] && { log "  TIMEOUT waiting for: ${desc}"; return 1; }
    sleep 0.5
  done
  log "  ready: ${desc}"
}

register_child() { CHILD_DESC+=("$1"); CHILD_PID+=("$2"); CHILD_SUDO+=("$3"); log "  started ${1} (pid $2)"; }

check_pinned() {
  local ok=1
  local -a pairs=(
    "${PHASE9D_DIR}/http/wimboot|${PINNED_HTTP_WIMBOOT}"
    "${PHASE9D_DIR}/http/BCD|${PINNED_HTTP_BCD}"
    "${PHASE9D_DIR}/http/boot.sdi|${PINNED_HTTP_BOOTSDI}"
    "${PHASE9D_DIR}/http/boot.wim|${PINNED_HTTP_BOOTWIM}"
    "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi|${PINNED_TFTP_SHIM}"
    "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly.efi|${PINNED_TFTP_SNPONLY}"
    "${PHASE9D_DIR}/tftp/ipxe.efi|${PINNED_TFTP_IPXE}"
  )
  local p f want got
  for p in "${pairs[@]}"; do
    f="${p%%|*}"; want="${p##*|}"
    [ -f "${f}" ] || { log "  MISSING ${f}"; ok=0; continue; }
    got="$(hash_of "${f}")"
    if [ "${got}" != "${want}" ]; then log "  HASH MISMATCH ${f}"; ok=0; fi
  done
  [ "${ok}" = "1" ]
}

runner_dlls() { LC_ALL=C objdump -p "$1" 2>/dev/null | sed -n 's/.*DLL Name: //p' | tr 'A-Z' 'a-z' | sort -u; }
runner_dlls_ok() {
  local d
  while read -r d; do
    [ -z "${d}" ] && continue
    case " ${PROVEN_DLLS} " in *" ${d} "*) : ;; *) log "  UNPROVEN DLL import: ${d}"; return 1 ;; esac
  done < <(runner_dlls "$1")
  return 0
}

usage() { sed -n '3,46p' "${SCRIPT_PATH}" | sed 's/^#\{1,\} \{0,1\}//;s/^#$//'; }

SELFTEST_MARKER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --preflight|--dry-run) MODE="preflight"; shift ;;
    # Hardware-free check of the terminal exit-status propagation ONLY. Writes a
    # synthetic terminal state into a throwaway EVID and reports the launcher's
    # resolved exit code. No services, no network, no derive.
    --selftest-exit) MODE="selftest"; SELFTEST_MARKER="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

if [ "${MODE}" = "selftest" ]; then
  EVID="$(mktemp -d "${TMPDIR:-/tmp}/i63-stage1-selftest.XXXXXX")"
  SENTINEL="${EVID}/.cleaning"
  COORD_VERDICT="${EVID}/coordinator.verdict"
  WATCHDOG_ABORT="${EVID}/.watchdog-abort"
  case "${SELFTEST_MARKER}" in
    physical_pass|physical_fail|host_smoke_pass|host_smoke_fail)
      printf '%s' "${SELFTEST_MARKER}" > "${COORD_VERDICT}"
      log "selftest: coordinator reached terminal verdict '${SELFTEST_MARKER}' (verdict file written)"
      ;;
    watchdog-abort)
      touch "${WATCHDOG_ABORT}"
      log "selftest: a lab service died before any terminal verdict (watchdog abort marker set, no verdict file)"
      ;;
    owner-ctrlc|"")
      log "selftest: owner Ctrl-C before any terminal verdict (no verdict file, no watchdog abort)"
      ;;
    *)
      rm -rf "${EVID}"; die "selftest: unknown marker '${SELFTEST_MARKER}' (physical_pass|physical_fail|host_smoke_pass|host_smoke_fail|watchdog-abort|owner-ctrlc)" ;;
  esac
  RC="$(resolve_terminal_exit)"
  log "selftest: no services were started — nothing to tear down (the real run paths all fire 'trap cleanup EXIT')"
  log "SELFTEST_EXIT_CODE=${RC}"
  rm -rf "${EVID}"
  exit "${RC}"
fi

# ---------------------------------------------------------------------
# 1. PREFLIGHT — read-only. No network mutation, no services started.
# ---------------------------------------------------------------------
hr
log "Bamep Issue #63 Stage 1 lab launcher — ${RUN_ID}"
log "MODE=${MODE}   (LAB-ONLY scaffolding; NOT the appliance design)"
hr

log "[preflight] repository + build artifacts"
require_file "${REPO_ROOT}/AGENTS.md" "repo root sanity"
require_file "${DERIVE}" "derive-stage1-runtime.sh"
require_file "${COORD_BIN}" "build: cd ${I63_DIR}/coordinator && cargo build --release"
require_file "${RUNNER_EXE}" "cross-build: cd ${I63_DIR}/winpe-runner && RUSTFLAGS='-C target-feature=+crt-static' cargo xwin build --release --target x86_64-pc-windows-msvc"
[ "$(head -c2 "${RUNNER_EXE}")" = "MZ" ] || die "runner exe is not a PE image: ${RUNNER_EXE}"
log "  ok: coordinator + runner exe present"

log "[preflight] Stage-1 runner PE imports are a subset of the stock-WinPE-proven set"
if command -v objdump >/dev/null 2>&1; then
  runner_dlls_ok "${RUNNER_EXE}" || die "runner imports a DLL outside the #60/#61-proven set — STOP and report"
  log "  ok: $(runner_dlls "${RUNNER_EXE}" | tr '\n' ' ')"
else
  log "  warn: objdump not installed — cannot check PE imports here"
fi

log "[preflight] Issue #53 Phase 9d assets present and byte-identical to pinned"
[ -d "${PHASE9D_DIR}" ] || die "Phase 9d runtime not found at ${PHASE9D_DIR} (run issue53-phase9d-setup first)"
check_pinned || die "Phase 9d assets do not match pinned hashes — refusing to derive"
log "  ok: 7/7 Phase 9d assets match pinned hashes"

log "[preflight] lab interface ${LAB_IFACE}"
ip link show "${LAB_IFACE}" >/dev/null 2>&1 || die "interface ${LAB_IFACE} does not exist"
HAVE_IP=0; ip -4 addr show "${LAB_IFACE}" | grep -qF "inet ${LAB_CIDR}" && HAVE_IP=1
FW_OK=0; CUR_FW_ZONE=""
if fw_available; then FW_OK=1; CUR_FW_ZONE="$(fw_zone_of "${LAB_IFACE}")"; fi
log "  ${LAB_IFACE}: ${LAB_CIDR} present=${HAVE_IP}   firewalld=${FW_OK} zone='${CUR_FW_ZONE:-<none>}' (want '${FW_ZONE}')"

log "[preflight] no conflicting listeners on the lab ports"
CONFLICT=0
for p in "${PORT_HTTP}" "${PORT_COORD}" "${PORT_SINK}"; do
  tcp_up "${p}" && { log "  CONFLICT: something already listens on tcp/${p}"; CONFLICT=1; }
done
udp_up 67 && { log "  CONFLICT: udp/67 (DHCP) already in use"; CONFLICT=1; }
udp_up 69 && { log "  CONFLICT: udp/69 (TFTP) already in use"; CONFLICT=1; }
[ "${CONFLICT}" -eq 0 ] || die "resolve the listed port conflicts (another lab session running?)"
log "  ok: all lab ports free"

log "[preflight] scratch space for the derived runtime (~2 MiB copied + symlinks)"
mkdir -p "${I63_DIR}/evidence"
FREE_BYTES="$(df -B1 --output=avail "${I63_DIR}/evidence" | tail -1 | tr -d ' ')"
[ "${FREE_BYTES}" -ge 536870912 ] || die "want >= 512 MiB free under ${I63_DIR}/evidence (have ${FREE_BYTES})"
log "  ok: ${FREE_BYTES} bytes free"

if [ "${MODE}" = "preflight" ]; then
  hr
  log "PREFLIGHT_OK — no network state changed, no services started."
  log "Run without --preflight to bring the Stage-1 lab up."
  hr
  exit 0
fi

# ---------------------------------------------------------------------
# 2. evidence directory + launcher log
# ---------------------------------------------------------------------
mkdir -p "${EVID}"
LAUNCHER_LOG="${EVID}/launcher.log"
: > "${LAUNCHER_LOG}"
SENTINEL="${EVID}/.cleaning"
COORD_VERDICT="${EVID}/coordinator.verdict"
WATCHDOG_ABORT="${EVID}/.watchdog-abort"
log "evidence directory: ${EVID}"
git -C "${REPO_ROOT}" rev-parse HEAD > "${EVID}/repo-head.txt" 2>/dev/null || echo "unknown" > "${EVID}/repo-head.txt"
{
  echo "run_id        ${RUN_ID}"
  echo "started_at    $(date -Is)"
  echo "repo_head     $(cat "${EVID}/repo-head.txt")"
  echo "lab_iface     ${LAB_IFACE}"
  echo "lab_ip        ${LAB_IP}"
  echo "ports         http=${PORT_HTTP} coord=${PORT_COORD} sink=${PORT_SINK}"
  echo "skew_window   [${SKEW_FLOOR_MS}, ${SKEW_CEIL_MS}] ms"
  echo "net_wait_secs ${NET_WAIT_SECS}"
} > "${EVID}/resolved.txt"

# ---------------------------------------------------------------------
# 3. cleanup / supervision trap (installed BEFORE any mutation)
# ---------------------------------------------------------------------
# shellcheck disable=SC2329
cleanup() {
  [ "${CLEANING}" -eq 1 ] && return
  CLEANING=1
  touch "${SENTINEL}" 2>/dev/null || true
  set +e
  [ -n "${WATCHDOG_PID}" ] && kill -TERM "${WATCHDOG_PID}" 2>/dev/null
  echo; hr
  log "CLEANUP — stopping only the children THIS launcher started"
  local idx
  for (( idx=${#CHILD_PID[@]}-1 ; idx>=0 ; idx-- )); do
    local pid="${CHILD_PID[$idx]}" desc="${CHILD_DESC[$idx]}" sudo="${CHILD_SUDO[$idx]}"
    proc_alive "${pid}" || { log "  ${desc} (pid ${pid}) already gone"; continue; }
    if [ "${sudo}" -eq 1 ]; then sudo pkill -TERM -P "${pid}" 2>/dev/null; sudo kill -TERM "${pid}" 2>/dev/null
    else kill -TERM "${pid}" 2>/dev/null; fi
    local w=0
    while proc_alive "${pid}" && [ "${w}" -lt 20 ]; do sleep 0.25; w=$((w + 1)); done
    if proc_alive "${pid}"; then
      if [ "${sudo}" -eq 1 ]; then sudo pkill -KILL -P "${pid}" 2>/dev/null; sudo kill -KILL "${pid}" 2>/dev/null
      else kill -KILL "${pid}" 2>/dev/null; fi
    fi
    wait "${pid}" 2>/dev/null || true
    log "  stopped ${desc} (pid ${pid})"
  done
  if [ -n "${OWNED_FW}" ]; then
    if [ "${OWNED_FW}" = "__none__" ]; then
      sudo firewall-cmd --zone="${FW_ZONE}" --remove-interface="${LAB_IFACE}" >/dev/null 2>&1 && log "  reverted: removed ${LAB_IFACE} from zone '${FW_ZONE}'"
    else
      sudo firewall-cmd --zone="${OWNED_FW}" --change-interface="${LAB_IFACE}" >/dev/null 2>&1 && log "  reverted: returned ${LAB_IFACE} to zone '${OWNED_FW}'"
    fi
  fi
  if [ "${OWNED_IP}" -eq 1 ] && ip -4 addr show "${LAB_IFACE}" | grep -qF "inet ${LAB_CIDR}"; then
    sudo ip addr del "${LAB_CIDR}" dev "${LAB_IFACE}" 2>/dev/null && log "  reverted: removed ${LAB_CIDR} from ${LAB_IFACE}"
  fi
  if [ "${OWNED_NM}" -eq 1 ]; then
    sudo nmcli device set "${LAB_IFACE}" managed yes >/dev/null 2>&1 && log "  reverted: returned ${LAB_IFACE} to NetworkManager"
  fi
  if [ "${OWNED_IP}" -eq 0 ] && [ -z "${OWNED_FW}" ] && [ "${OWNED_NM}" -eq 0 ]; then
    log "  network: this launcher changed NOTHING — pre-existing lab state left as found"
  fi
  log "  evidence preserved at: ${EVID}"
  log "Stage-1 lab down."
  hr
}
# shellcheck disable=SC2329
on_signal() {
  trap - INT TERM
  echo
  local rc
  rc="$(resolve_terminal_exit)"
  if [ -f "${COORD_VERDICT}" ]; then
    log "Stage-1 terminal verdict: $(cat "${COORD_VERDICT}" 2>/dev/null) — lab shutting down (launcher exit ${rc})"
  elif [ -f "${WATCHDOG_ABORT}" ]; then
    log "a lab service died before any terminal verdict — lab shutting down (launcher exit ${rc})"
  else
    log "signal received before any terminal verdict — lab shutting down (launcher exit ${rc})"
  fi
  exit "${rc}"
}
trap on_signal INT TERM
trap cleanup EXIT

# ---------------------------------------------------------------------
# 4. derive the Issue-63 runtime (Phase-9d re-hashed before/after inside)
# ---------------------------------------------------------------------
hr
log "DERIVING the Issue-63 Stage-1 PXE/WinPE runtime (Phase-9d assets consumed READ-ONLY)"
DERIVE_OUT="${EVID}/derived-runtime"
if ! "${DERIVE}" --out "${DERIVE_OUT}" --runner-exe "${RUNNER_EXE}" \
      --lab-ip "${LAB_IP}" --http-port "${PORT_HTTP}" \
      --coord-port "${PORT_COORD}" --sink-port "${PORT_SINK}" \
      --skew-floor-ms "${SKEW_FLOOR_MS}" --skew-ceil-ms "${SKEW_CEIL_MS}" \
      --net-wait-secs "${NET_WAIT_SECS}" --run-id "${RUN_ID}" --iface "${LAB_IFACE}" \
      > "${EVID}/derive.log" 2>&1; then
  cat "${EVID}/derive.log" >&2
  die "derive-stage1-runtime.sh failed — see ${EVID}/derive.log"
fi
grep -q '^DERIVE_OK ' "${EVID}/derive.log" || die "derive did not report DERIVE_OK"
log "  derived runtime at ${DERIVE_OUT}"
log "  Phase-9d before/after: $(diff -q "${DERIVE_OUT}/phase9d-hashes-before.txt" "${DERIVE_OUT}/phase9d-hashes-after.txt" >/dev/null && echo IDENTICAL || echo DIFFERENT)"
diff -q "${DERIVE_OUT}/phase9d-hashes-before.txt" "${DERIVE_OUT}/phase9d-hashes-after.txt" >/dev/null || die "Phase-9d assets changed during derive"

# ---------------------------------------------------------------------
# 5. lab network runtime (check first, mutate only what is missing)
# ---------------------------------------------------------------------
hr
log "LAB NETWORK RUNTIME  (runtime-only; NOT the Bamep appliance network design)"
if [ "${HAVE_IP}" -eq 1 ]; then
  log "  ${LAB_CIDR} already on ${LAB_IFACE} — leaving it (not ours to remove)"
else
  if nmcli -t -f DEVICE,STATE device status 2>/dev/null | grep -qE "^${LAB_IFACE}:connected$"; then
    sudo nmcli device set "${LAB_IFACE}" managed no; OWNED_NM=1
    log "  set ${LAB_IFACE} unmanaged in NetworkManager (RUNTIME ONLY)"
  fi
  sudo ip addr add "${LAB_CIDR}" dev "${LAB_IFACE}"
  sudo ip link set "${LAB_IFACE}" up
  OWNED_IP=1
  ip -4 addr show | grep -F "inet ${LAB_IP}/" | grep -qv "${LAB_IFACE}" && die "${LAB_IP} leaked onto another interface"
  log "  added ${LAB_CIDR} to ${LAB_IFACE} (runtime only)"
fi
if [ "${FW_OK}" -eq 1 ]; then
  if [ "${CUR_FW_ZONE}" = "${FW_ZONE}" ]; then
    log "  ${LAB_IFACE} already in firewalld zone '${FW_ZONE}' — leaving it"
  else
    OWNED_FW="${CUR_FW_ZONE:-__none__}"
    sudo firewall-cmd --zone="${FW_ZONE}" --change-interface="${LAB_IFACE}" >/dev/null
    log "  moved ${LAB_IFACE} into firewalld RUNTIME zone '${FW_ZONE}' (was '${OWNED_FW}'); reverted on exit"
  fi
else
  log "  firewalld not queryable — assuming an already-isolated lab link"
fi

# ---------------------------------------------------------------------
# 6. background lab services
# ---------------------------------------------------------------------
hr
log "STARTING BACKGROUND LAB SERVICES"

log "[dnsmasq] DHCP + TFTP on ${LAB_IFACE} (derived conf)"
# dnsmasq drops to the 'dnsmasq' user after binding, so pre-create its
# log-facility + leasefile owned by that user (same idiom as the #53/#61 labs).
DM_GROUP="$(id -gn)"
if id dnsmasq >/dev/null 2>&1; then
  sudo install -o dnsmasq -g "${DM_GROUP}" -m 0640 /dev/null "${DERIVE_OUT}/dnsmasq.log"
  sudo install -o dnsmasq -g "${DM_GROUP}" -m 0640 /dev/null "${DERIVE_OUT}/dnsmasq.leases"
fi
# shellcheck disable=SC2024
sudo dnsmasq -d --conf-file="${DERIVE_OUT}/dnsmasq.conf" > "${EVID}/dnsmasq.log" 2>&1 &
register_child "dnsmasq" "$!" 1
await "dnsmasq udp/67 (DHCP)" 40 udp_up 67 || die "dnsmasq did not bind udp/67 — see ${EVID}/dnsmasq.log"
await "dnsmasq udp/69 (TFTP)" 20 udp_up 69 || die "dnsmasq did not bind udp/69 — see ${EVID}/dnsmasq.log"

log "[http] python3 http.server on ${LAB_IP}:${PORT_HTTP} serving ${DERIVE_OUT}/http"
python3 -m http.server --bind "${LAB_IP}" --directory "${DERIVE_OUT}/http" "${PORT_HTTP}" > "${EVID}/http.log" 2>&1 &
register_child "winpe-http" "$!" 0
await "http tcp/${PORT_HTTP}" 30 tcp_up "${PORT_HTTP}" || die "HTTP server did not start — see ${EVID}/http.log"

log "[coordinator] bamep-i63-stage1-coordinator --mode physical (coord + sink + Stage-1 state machine)"
rm -f "${COORD_VERDICT}"
"${COORD_BIN}" --mode physical --coord-addr "${LAB_IP}:${PORT_COORD}" --sink-addr "${LAB_IP}:${PORT_SINK}" \
  --evidence "${EVID}/stage1-events.ndjson" --run-id "${RUN_ID}" --verdict-file "${COORD_VERDICT}" \
  > "${EVID}/coordinator.log" 2>&1 &
register_child "coordinator" "$!" 0
await "coordinator STAGE1_COORDINATOR_LISTENING" 30 grep -q '^STAGE1_COORDINATOR_LISTENING ' "${EVID}/coordinator.log" \
  || die "coordinator never reported listening — see ${EVID}/coordinator.log"
await "coordinator coord tcp/${PORT_COORD}" 20 tcp_up "${PORT_COORD}" || die "no coord listener"
await "coordinator sink tcp/${PORT_SINK}"  20 tcp_up "${PORT_SINK}"  || die "no sink listener"

# ---------------------------------------------------------------------
# 7. readiness gate
# ---------------------------------------------------------------------
hr
log "READINESS GATE"
gate_fail=0
gate() { local d="$1"; shift; if "$@"; then log "  PASS  ${d}"; else log "  FAIL  ${d}"; gate_fail=1; fi; }

gate "${LAB_IFACE} has ${LAB_CIDR}"          bash -c "ip -4 addr show '${LAB_IFACE}' | grep -qF 'inet ${LAB_CIDR}'"
gate "udp/67 (DHCP) listening"               udp_up 67
gate "udp/69 (TFTP) listening"               udp_up 69
gate "tcp/${PORT_HTTP} (WinPE HTTP)"         tcp_up "${PORT_HTTP}"
gate "tcp/${PORT_COORD} (coord)"             tcp_up "${PORT_COORD}"
gate "tcp/${PORT_SINK} (sink)"               tcp_up "${PORT_SINK}"
gate "Phase-9d assets still match pinned"    check_pinned
gate "runner PE imports within proven set"   runner_dlls_ok "${RUNNER_EXE}"

for asset_want in "wimboot|${PINNED_HTTP_WIMBOOT}" "BCD|${PINNED_HTTP_BCD}" "boot.sdi|${PINNED_HTTP_BOOTSDI}" "boot.wim|${PINNED_HTTP_BOOTWIM}"; do
  a="${asset_want%%|*}"; w="${asset_want##*|}"
  got="$(curl -sf "http://${LAB_IP}:${PORT_HTTP}/${a}" | sha256sum | awk '{print $1}')" || got="ERR"
  gate "HTTP /${a} serves pinned bytes" bash -c "[ '${got}' = '${w}' ]"
done
for f in winpeshl.ini bamep-i63-bootstrap.cmd bamep-i63-runner.exe; do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://${LAB_IP}:${PORT_HTTP}/${f}")"
  gate "HTTP /${f} -> 200" bash -c "[ '${code}' = '200' ]"
done
AX="${DERIVE_OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
gate "autoexec injects winpeshl.ini"        grep -qF "/winpeshl.ini winpeshl.ini" "${AX}"
gate "autoexec injects bootstrap.cmd"       grep -qF "/bamep-i63-bootstrap.cmd bamep-i63-bootstrap.cmd" "${AX}"
gate "autoexec injects runner.exe"          grep -qF "/bamep-i63-runner.exe bamep-i63-runner.exe" "${AX}"
gate "bootstrap.cmd launches runner --mode physical" grep -qF -- '--mode physical' "${DERIVE_OUT}/http/bamep-i63-bootstrap.cmd"
gate "bootstrap.cmd records bootstrap_local_ts"      grep -qF 'bootstrap_local_ts' "${DERIVE_OUT}/http/bamep-i63-bootstrap.cmd"
gate "coordinator started in physical mode"          grep -q '^STAGE1_COORDINATOR_LISTENING mode=physical ' "${EVID}/coordinator.log"
gate "autoexec: boot.wim initrd is last"    bash -c "[ \"\$(grep '^initrd ' '${AX}' | tail -1)\" = \"\$(grep '/boot.wim boot.wim$' '${AX}')\" ]"

for i in "${!CHILD_PID[@]}"; do
  gate "child alive: ${CHILD_DESC[$i]} (pid ${CHILD_PID[$i]})" proc_alive "${CHILD_PID[$i]}"
done

{
  echo "run_id     ${RUN_ID}"
  echo "ready_at   $(date -Is)"
  echo "gate_fail  ${gate_fail}"
  echo "runner_dlls $(runner_dlls "${RUNNER_EXE}" | tr '\n' ' ')"
  for i in "${!CHILD_PID[@]}"; do printf 'pid %-8s sudo=%s  %s\n' "${CHILD_PID[$i]}" "${CHILD_SUDO[$i]}" "${CHILD_DESC[$i]}"; done
} > "${EVID}/readiness-summary.txt"

[ "${gate_fail}" -eq 0 ] || die "readiness gate FAILED — see the FAIL lines above. Cleaning up."

# ---------------------------------------------------------------------
# 8. health watchdog
# ---------------------------------------------------------------------
# shellcheck disable=SC2329
watchdog() {
  set +e
  local names=("${CHILD_DESC[@]}") pids=("${CHILD_PID[@]}") k
  while :; do
    sleep 3
    [ -e "${SENTINEL}" ] && return 0
    for k in "${!pids[@]}"; do
      if ! proc_alive "${pids[$k]}"; then
        # The coordinator is EXPECTED to exit after it reaches a terminal
        # verdict — it writes ${COORD_VERDICT} just before exiting. That is a
        # completed run, NOT "service unexpectedly died". Any other child, or
        # the coordinator with no verdict file, is a real failure -> fail closed.
        if [ "${names[$k]}" = "coordinator" ] && [ -f "${COORD_VERDICT}" ]; then
          log ""
          log ">>> coordinator reached its terminal verdict ($(cat "${COORD_VERDICT}" 2>/dev/null)) and exited (expected) — tearing the lab down"
        else
          log ""
          log "!!! ${names[$k]} (pid ${pids[$k]}) EXITED UNEXPECTEDLY — lab is NOT READY"
          touch "${WATCHDOG_ABORT}" 2>/dev/null
        fi
        touch "${SENTINEL}" 2>/dev/null; kill -TERM "${MAIN_PID}" 2>/dev/null; return 0
      fi
    done
  done
}
watchdog & WATCHDOG_PID=$!
log "health watchdog running (pid ${WATCHDOG_PID})"

# ---------------------------------------------------------------------
# 9. READY
# ---------------------------------------------------------------------
cat <<EOF | tee -a "${LAUNCHER_LOG}"

==================================================
READY_FOR_MINIPC_POWER_ON   (${RUN_ID})
==================================================
DHCP/TFTP     : READY   (dnsmasq, derived Issue-63 conf, ${LAB_IFACE})
WinPE HTTP    : READY   (${LAB_IP}:${PORT_HTTP}  ${DERIVE_OUT}/http)
Coordinator   : READY   mode=PHYSICAL   (coord ${LAB_IP}:${PORT_COORD}  sink ${LAB_IP}:${PORT_SINK})
Phase-9d      : byte-identical before/after derive (7/7 pinned)
Runner imports: $(runner_dlls "${RUNNER_EXE}" | tr '\n' ' ') (subset of stock-WinPE-proven set)
Clock method  : SetSystemTime (UTC); strict skew window [${SKEW_FLOOR_MS}, ${SKEW_CEIL_MS}] ms
Evidence gate : bootstrap milestones must be FORWARDED from X:\\ (never synthesised);
                clock backend must be the real Win32 backend; every event mode=physical
                — else STAGE1_PHYSICAL_FAIL, never a pass

Stage-1 chain the coordinator is waiting on:
  winpe.booted -> winpe.wpeinit_complete -> winpe.network_ready -> winpe.runner_ready
  -> winpe.server_utc_received -> winpe.clock_alignment_attempted -> winpe.clock_aligned
  -> stage1.ready

Evidence: ${EVID}
  launcher.log  derive.log  dnsmasq.log  http.log  coordinator.log
  stage1-events.ndjson  derived-runtime/{phase9d-hashes-*.txt,derived-manifest.txt}

Owner action (ONLY this):
  1. Power ON the MiniPC (NIC e8:ff:1e:d6:2e:f5; it will DHCP-lease in 192.168.99.50-100).
  2. Press a key ONCE at wimboot's "Press any key to continue booting..." prompt.
  3. Do NOT type any WinPE command. The injected winpeshl.ini auto-starts the
     bootstrap -> wpeinit -> runner. Watch this terminal for STAGE1_PHYSICAL_PASS.

Stage 1 opens NO disk handle, performs NO transfer. After PASS/FAIL, Ctrl-C here
to tear the lab down (only what this launcher started is reverted).
==================================================
EOF

# ---------------------------------------------------------------------
# 10. foreground supervise — stream coordinator output; flag the verdict
#
# The coordinator OWNS the state machine and the run lifecycle: on a terminal
# verdict it drains trailing flushes for a few seconds, writes ${COORD_VERDICT},
# and exits. The watchdog then teardown-signals us (expected). This tail is only
# a live view; it does not decide anything.
# ---------------------------------------------------------------------
(
  # shellcheck disable=SC2016
  tail -n +1 -F "${EVID}/coordinator.log" 2>/dev/null | while IFS= read -r line; do
    printf '%s\n' "${line}"
    case "${line}" in
      STAGE1_PHYSICAL_PASS*)
        printf '\n############################################\n# STAGE 1 PHYSICAL PASS — review the evidence\n# %s\n############################################\n' "${EVID}" ;;
      STAGE1_PHYSICAL_FAIL*|STAGE1_HOST_SMOKE_FAIL*)
        printf '\n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n! STAGE 1 FAIL — evidence preserved\n! %s\n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n' "${EVID}" ;;
    esac
  done
) &
TAIL_PID=$!
CHILD_DESC+=("coordinator-tail"); CHILD_PID+=("${TAIL_PID}"); CHILD_SUDO+=("0")

wait "${TAIL_PID}" 2>/dev/null || true

# Reached only if the tail ended without a signal (e.g. coordinator.log removed).
# The signal path is handled by on_signal, which already `exit`ed. Use the same
# resolver so a *_fail verdict still exits non-zero.
RC="$(resolve_terminal_exit)"
log "Stage-1 lab supervise ended — launcher exit ${RC} (verdict: $(cat "${COORD_VERDICT}" 2>/dev/null || echo none))"
exit "${RC}"
