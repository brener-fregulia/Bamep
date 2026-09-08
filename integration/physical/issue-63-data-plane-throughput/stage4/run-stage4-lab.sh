#!/usr/bin/env bash
#
# Bamep Issue #63 Stage 4 — physical MiniPC micro-matrix lab supervisor.
# THROWAWAY Spike scaffolding. NOT appliance architecture, NOT production service
# management, NOT the Bamep firewall/network/boot design.
#
# =====================================================================
# THE PHYSICAL 10-CASE MICRO-MATRIX IS ARMED ONLY BEHIND  --arm.
# Without --arm this prints "STAGE4 NOT ARMED" and exits 0.
# Even WITH --arm the supervisor first runs every host-side preflight and only
# then prints READY_FOR_STAGE4_MINIPC_POWER_ON. It NEVER powers the MiniPC and
# NEVER starts a physical source read or a transfer.
# =====================================================================
#
# One foreground supervisor that owns every background service the Stage-4
# micro-matrix needs, composing ALREADY-PROVEN pieces:
#   * Stage 1  — derived Secure-Boot/iPXE/wimboot runtime + winpeshl auto-start
#                + SetSystemTime UTC alignment (the Stage-1 runner, --stage4 --arm);
#   * Stage 4  — the deterministic 10-case S/P plan + coordinator (--stage4 --arm)
#                + the transfer probe (--mode serial | --mode prep_ahead_2);
#   * Issue #61 CP7 shape — real PostgreSQL + AgentControlGateway/WSS +
#                first-contact credential + Worker HTTPS + Transfer/Artifact
#                lifecycle + terminal Artifact::Verified evidence (the same
#                stage3-harness binary in --stage4 mode; #61 is not modified).
#
# The experiment: 64 MiB chunk size ONLY, 2048 MiB extent per case, 2 warm-ups
# (S,P) + 4 paired cycles (S,P / P,S / S,P / P,S) = 10 transfers, ONE WinPE boot,
# NO reboot between cases, NO deliberate fault injection. PREP-AHEAD DEPTH 2 =
# PREP-AHEAD ONLY (one PUT in flight, ascending, no multi-PUT concurrency).
#
# Verification (no physical boot triggered by any of these):
#   bash -n run-stage4-lab.sh
#   ./run-stage4-lab.sh --preflight            (read-only gate; no services)
#   ./run-stage4-lab.sh --arm --preflight      (armed interface; still no boot)
#
# Owner action, AFTER READY_FOR_STAGE4_MINIPC_POWER_ON (ONLY this):
#   1. run this launcher / enter the sudo password;
#   2. power ON the MiniPC;
#   3. press a key ONCE at wimboot's "Press any key to continue booting..." prompt;
#   4. type NOTHING else in WinPE — winpeshl.ini auto-runs the whole 10-case matrix.
#
# Env overrides: I63_LAB_IFACE (enp8s0), I63_LAB_IP (192.168.99.1),
#   I63_FW_ZONE (trusted), I63_STORAGE_ROOT, I63_SKEW_FLOOR_MS (-2000),
#   I63_SKEW_CEIL_MS (2000), I63_NET_WAIT_SECS (180), I63_SEAL_TIMEOUT_SECS (300),
#   I63_MODEL_SUBSTR (256GB), BAMEP_PHYSINT_DB_URL.
#
set -euo pipefail
export LC_ALL=C

SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
STAGE4_DIR="$(dirname "${SCRIPT_PATH}")"
I63_DIR="$(dirname "${STAGE4_DIR}")"
REPO_ROOT="$(cd "${I63_DIR}/../../.." && pwd)"

LAB_IFACE="${I63_LAB_IFACE:-enp8s0}"
LAB_IP="${I63_LAB_IP:-192.168.99.1}"
LAB_CIDR="${LAB_IP}/24"
FW_ZONE="${I63_FW_ZONE:-trusted}"

PORT_HTTP=8080
PORT_WSS=8443
PORT_COORD=9206
PORT_DP=9207
PORT_MATRIX=9210
PORT_SINK=9299

SKEW_FLOOR_MS="${I63_SKEW_FLOOR_MS:--2000}"
SKEW_CEIL_MS="${I63_SKEW_CEIL_MS:-2000}"
NET_WAIT_SECS="${I63_NET_WAIT_SECS:-180}"
SEAL_TIMEOUT_SECS="${I63_SEAL_TIMEOUT_SECS:-300}"
MODEL_SUBSTR="${I63_MODEL_SUBSTR:-256GB}"

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"
COORD_BIN="${I63_DIR}/coordinator/target/release/bamep-i63-stage1-coordinator"
HARNESS_BIN="${I63_DIR}/stage3-harness/target/release/bamep-i63-stage3-harness"
RUNNER_EXE="${I63_DIR}/winpe-runner/target/x86_64-pc-windows-msvc/release/bamep-i63-runner.exe"
PROBE_EXE="${I63_DIR}/stage2-probe/target/x86_64-pc-windows-msvc/release/bamep-i63-stage2-probe.exe"
DERIVE="${I63_DIR}/stage3/derive-stage3-runtime.sh"

STORAGE_ROOT="${I63_STORAGE_ROOT:-${I63_DIR}/stage3-harness/runtime-stage4/chunkstore}"
DB_NAME="bamep_physint_spike"

# 10 x 2 GiB preserved payload + margin.
MATRIX_PAYLOAD_BYTES=$((10 * 2147483648))     # 21474836480 = 20 GiB
MIN_FREE_BYTES=$((30 * 1024 * 1024 * 1024))   # 32212254720 = 30 GiB

PINNED_HTTP_WIMBOOT="5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3"
PINNED_HTTP_BCD="c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41"
PINNED_HTTP_BOOTSDI="cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd"
PINNED_HTTP_BOOTWIM="fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197"
PINNED_TFTP_SHIM="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
PINNED_TFTP_SNPONLY="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
PINNED_TFTP_IPXE="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"

PROVEN_DLLS="api-ms-win-core-synch-l1-2-0.dll kernel32.dll ntdll.dll ws2_32.dll advapi32.dll bcrypt.dll bcryptprimitives.dll"

RUN_ID="i63s4-$(date +%Y%m%dT%H%M%S)"
EVID="${I63_DIR}/evidence/${RUN_ID}"
MATRIX_EVID="${EVID}/matrix"
WORKER_TIMING_FILE="${MATRIX_EVID}/worker-put-timing.ndjson"
SENTINEL=""
MATRIX_VERDICT=""
WATCHDOG_ABORT=""
ARMED=0
MAIN_PID=$$

CHILD_DESC=(); CHILD_PID=(); CHILD_SUDO=()
OWNED_IP=0; OWNED_FW=""; OWNED_NM=0
CLEANING=0
WATCHDOG_PID=""
LAUNCHER_LOG="/dev/null"
FINGERPRINT=""
DERIVE_OUT=""
CRED_TMP=""

log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "${LAUNCHER_LOG}" >&2; }
die() { log "FATAL: $*"; exit 1; }
hr()  { log "------------------------------------------------------------------"; }

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
  local ok=1 p f want got
  local -a pairs=(
    "${PHASE9D_DIR}/http/wimboot|${PINNED_HTTP_WIMBOOT}"
    "${PHASE9D_DIR}/http/BCD|${PINNED_HTTP_BCD}"
    "${PHASE9D_DIR}/http/boot.sdi|${PINNED_HTTP_BOOTSDI}"
    "${PHASE9D_DIR}/http/boot.wim|${PINNED_HTTP_BOOTWIM}"
    "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi|${PINNED_TFTP_SHIM}"
    "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly.efi|${PINNED_TFTP_SNPONLY}"
    "${PHASE9D_DIR}/tftp/ipxe.efi|${PINNED_TFTP_IPXE}"
  )
  for p in "${pairs[@]}"; do
    f="${p%%|*}"; want="${p##*|}"
    [ -f "${f}" ] || { log "  MISSING ${f}"; ok=0; continue; }
    got="$(hash_of "${f}")"
    [ "${got}" = "${want}" ] || { log "  HASH MISMATCH ${f}"; ok=0; }
  done
  [ "${ok}" = "1" ]
}

PE_OBJDUMP="$(command -v llvm-objdump || true)"
[ -z "${PE_OBJDUMP}" ] && for c in \
  "$(rustc --print sysroot 2>/dev/null)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objdump"; do
  [ -x "${c}" ] && PE_OBJDUMP="${c}" && break
done
[ -z "${PE_OBJDUMP}" ] && PE_OBJDUMP="$(command -v objdump || true)"
pe_dlls() { [ -n "${PE_OBJDUMP}" ] && LC_ALL=C "${PE_OBJDUMP}" -p "$1" 2>/dev/null | sed -n 's/.*DLL Name: //p' | tr 'A-Z' 'a-z' | sort -u; }
pe_dlls_ok() {
  local d found=0
  while read -r d; do
    [ -z "${d}" ] && continue
    found=1
    case " ${PROVEN_DLLS} " in *" ${d} "*) : ;; *) log "  UNPROVEN DLL import in $2: ${d}"; return 1 ;; esac
  done < <(pe_dlls "$1")
  [ "${found}" -eq 1 ] || log "  warn: could not read PE imports of $2 (no working objdump)"
  return 0
}

db_reachable() {
  if [ -n "${BAMEP_PHYSINT_DB_URL:-}" ]; then
    command -v psql >/dev/null 2>&1 && psql "${BAMEP_PHYSINT_DB_URL}" -tAc 'select 1' >/dev/null 2>&1
    return
  fi
  command -v pg_isready >/dev/null 2>&1 || return 2
  pg_isready -q 2>/dev/null || return 1
  if command -v psql >/dev/null 2>&1; then
    psql -d "${DB_NAME}" -tAc 'select 1' >/dev/null 2>&1 || return 3
  fi
  return 0
}

usage() { sed -n '3,50p' "${SCRIPT_PATH}" | sed 's/^#\{1,\} \{0,1\}//;s/^#$//'; }

resolve_terminal_exit() {
  if [ -f "${MATRIX_VERDICT}" ]; then
    case "$(cat "${MATRIX_VERDICT}" 2>/dev/null)" in
      stage4_pass) echo 0 ;;
      stage4_fail) echo 10 ;;
      stage4_invalid) echo 12 ;;
      *) echo 11 ;;
    esac
  elif [ -f "${WATCHDOG_ABORT}" ]; then
    echo 20
  else
    echo 130
  fi
}

# ---------------------------------------------------------------------
PREFLIGHT_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --arm) ARMED=1; shift ;;
    --preflight|--dry-run) PREFLIGHT_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

if [ "${ARMED}" -eq 0 ] && [ "${PREFLIGHT_ONLY}" -eq 0 ]; then
  echo "STAGE4 NOT ARMED"
  echo
  echo "run-stage4-lab.sh needs an explicit --arm to start the physical services."
  echo "  ./run-stage4-lab.sh --preflight        # read-only host checks, no services"
  echo "  ./run-stage4-lab.sh --arm --preflight  # armed interface, host checks only, still no boot"
  echo "  ./run-stage4-lab.sh --arm              # bring the lab up; prints READY_FOR_STAGE4_MINIPC_POWER_ON"
  exit 0
fi

# ---------------------------------------------------------------------
# 1. PREFLIGHT — read-only.
# ---------------------------------------------------------------------
hr
log "Bamep Issue #63 Stage 4 micro-matrix lab supervisor — ${RUN_ID}"
log "armed=${ARMED}  preflight_only=${PREFLIGHT_ONLY}   (LAB-ONLY scaffolding; NOT the appliance design)"
hr

log "[preflight] repository + build artifacts"
require_file "${REPO_ROOT}/AGENTS.md" "repo root sanity"
require_file "${DERIVE}" "derive-stage3-runtime.sh (--stage 4)"
require_file "${COORD_BIN}" "build: ( cd ${I63_DIR}/coordinator && cargo build --release )"
require_file "${HARNESS_BIN}" "build: ( cd ${I63_DIR}/stage3-harness && cargo build --release )"
require_file "${RUNNER_EXE}" "cross-build: ( cd ${I63_DIR}/winpe-runner && RUSTFLAGS='-C target-feature=+crt-static' cargo xwin build --release --target x86_64-pc-windows-msvc )"
require_file "${PROBE_EXE}" "cross-build: ( cd ${I63_DIR}/stage2-probe && RUSTFLAGS='-C target-feature=+crt-static' cargo xwin build --release --target x86_64-pc-windows-msvc )"
[ "$(head -c2 "${RUNNER_EXE}")" = "MZ" ] || die "runner exe is not a PE image"
[ "$(head -c2 "${PROBE_EXE}")" = "MZ" ] || die "probe exe is not a PE image"
log "  ok: coordinator + harness + runner.exe + probe.exe present"

log "[preflight] WinPE PE imports within the #60/#61-proven stock-WinPE set"
if command -v objdump >/dev/null 2>&1; then
  pe_dlls_ok "${RUNNER_EXE}" "runner" || die "runner imports a DLL outside the proven set — STOP and report"
  pe_dlls_ok "${PROBE_EXE}" "probe" || die "probe imports a DLL outside the proven set — STOP and report"
  log "  ok: runner=[$(pe_dlls "${RUNNER_EXE}" | tr '\n' ' ')] probe=[$(pe_dlls "${PROBE_EXE}" | tr '\n' ' ')]"
else
  log "  warn: objdump not installed — cannot check PE imports here"
fi

log "[preflight] Issue #53 Phase 9d assets present and byte-identical to pinned"
[ -d "${PHASE9D_DIR}" ] || die "Phase 9d runtime not found at ${PHASE9D_DIR}"
check_pinned || die "Phase 9d assets do not match pinned hashes — refusing to derive"
log "  ok: 7/7 Phase 9d assets match pinned hashes"

log "[preflight] deterministic Stage-4 10-case plan + S/P analysis shape"
( cd "${I63_DIR}/coordinator" && "${COORD_BIN}" --stage4-selftest 2>&1 | tail -2 ) | tee -a "${LAUNCHER_LOG}" >&2
"${COORD_BIN}" --stage4-selftest 2>&1 | grep -q '^STAGE4_SELFTEST_PASS' || die "coordinator --stage4-selftest FAILED"
"${COORD_BIN}" --stage4 2>&1 | grep -q 'STAGE4 NOT ARMED' || die "coordinator --stage4 (no --arm) must print STAGE4 NOT ARMED"
log "  ok: 10-case plan (2 warm-up + 8 measured, 64 MiB, 32 chunks/2048 MiB); n=4 per mode"

log "[preflight] Worker storage root + free space (>= 30 GiB for 10 preserved 2 GiB Artifacts)"
mkdir -p "${STORAGE_ROOT}"
case "${STORAGE_ROOT}" in
  *runtime-cp6*|*runtime-cp7a*) die "storage root must not resolve under an Issue #61 runtime-cp* tree" ;;
esac
git -C "${REPO_ROOT}" check-ignore -q "${STORAGE_ROOT}" || log "  warn: ${STORAGE_ROOT} is NOT git-ignored — inspect .gitignore"
touch "${STORAGE_ROOT}/.write-probe-$$" && rm -f "${STORAGE_ROOT}/.write-probe-$$" || die "storage root not writable: ${STORAGE_ROOT}"
FREE_BYTES="$(df -B1 --output=avail "${STORAGE_ROOT}" | tail -1 | tr -d ' ')"
log "  ${STORAGE_ROOT}: free=${FREE_BYTES} bytes  (gate ${MIN_FREE_BYTES})"
[ "${FREE_BYTES}" -ge "${MIN_FREE_BYTES}" ] || die "need >= ${MIN_FREE_BYTES} bytes free under the Worker storage root"

log "[preflight] runtime / evidence directories writable + git-ignored"
for d in "${I63_DIR}/stage3-harness/runtime-stage4" "${I63_DIR}/evidence"; do
  mkdir -p "${d}"
  touch "${d}/.write-probe-$$" && rm -f "${d}/.write-probe-$$" || die "not writable: ${d}"
  git -C "${REPO_ROOT}" check-ignore -q "${d}" || log "  warn: ${d} is NOT git-ignored"
done
log "  ok"

log "[preflight] Worker PUT timing sink target (evidence dir; OUTSIDE the chunk-store tree)"
case "$(readlink -f "${MATRIX_EVID}" 2>/dev/null || echo "${MATRIX_EVID}")" in
  "$(readlink -f "${STORAGE_ROOT}" 2>/dev/null || echo "${STORAGE_ROOT}")"*) die "worker timing sink must be OUTSIDE the storage root" ;;
esac
log "  ok: ${WORKER_TIMING_FILE}"

log "[preflight] PostgreSQL reachable / ${DB_NAME} present (read-only check; never created/migrated here)"
if db_reachable; then
  log "  ok: ${DB_NAME} reachable"
else
  rc=$?
  case "${rc}" in
    2) log "  warn: neither psql nor pg_isready available — the harness will fail closed if PG is down" ;;
    1) die "PostgreSQL is not accepting connections (pg_isready). Start it or set BAMEP_PHYSINT_DB_URL." ;;
    3) die "database ${DB_NAME} not reachable via psql — create + migrate it before arming (the harness never creates it)" ;;
    *) die "PostgreSQL reachability check failed (rc=${rc})" ;;
  esac
fi

log "[preflight] lab interface ${LAB_IFACE}"
ip link show "${LAB_IFACE}" >/dev/null 2>&1 || die "interface ${LAB_IFACE} does not exist"
HAVE_IP=0; ip -4 addr show "${LAB_IFACE}" | grep -qF "inet ${LAB_CIDR}" && HAVE_IP=1
FW_OK=0; CUR_FW_ZONE=""
if fw_available; then FW_OK=1; CUR_FW_ZONE="$(fw_zone_of "${LAB_IFACE}")"; fi
log "  ${LAB_IFACE}: ${LAB_CIDR} present=${HAVE_IP}   firewalld=${FW_OK} zone='${CUR_FW_ZONE:-<none>}' (want '${FW_ZONE}')"

log "[preflight] no conflicting listeners / stale Issue-63 lab processes on the lab ports"
CONFLICT=0
for p in "${PORT_HTTP}" "${PORT_WSS}" "${PORT_COORD}" "${PORT_DP}" "${PORT_MATRIX}" "${PORT_SINK}"; do
  tcp_up "${p}" && { log "  CONFLICT: something already listens on tcp/${p}"; CONFLICT=1; }
done
udp_up 67 && { log "  CONFLICT: udp/67 (DHCP) already in use"; CONFLICT=1; }
udp_up 69 && { log "  CONFLICT: udp/69 (TFTP) already in use"; CONFLICT=1; }
if pgrep -af 'bamep-i63-stage3-harness|bamep-i63-stage1-coordinator --(matrix|stage4)' >/dev/null 2>&1; then
  log "  CONFLICT: a stale Issue-63 process is still running"; CONFLICT=1
fi
[ "${CONFLICT}" -eq 0 ] || die "resolve the listed conflicts (another lab session running?)"
log "  ok: all lab ports free; these ports are LAB-ONLY (no production Bamep service is being replaced)"

log "[preflight] scratch space for the derived runtime + preserved evidence"
mkdir -p "${I63_DIR}/evidence"
SCRATCH_FREE="$(df -B1 --output=avail "${I63_DIR}/evidence" | tail -1 | tr -d ' ')"
[ "${SCRATCH_FREE}" -ge 536870912 ] || die "want >= 512 MiB free under ${I63_DIR}/evidence (have ${SCRATCH_FREE})"
log "  ok: ${SCRATCH_FREE} bytes free"

if [ "${PREFLIGHT_ONLY}" -eq 1 ]; then
  hr
  if [ "${ARMED}" -eq 1 ]; then
    log "ARMED PREFLIGHT_OK — every host-side check passed. No network state changed, no services"
    log "started, NO derive, NO MiniPC boot. Re-run './run-stage4-lab.sh --arm' to bring the lab up."
  else
    log "PREFLIGHT_OK — no network state changed, no services started. Add --arm to run the micro-matrix."
  fi
  hr
  exit 0
fi

# ---------------------------------------------------------------------
# 2. evidence directory + trap
# ---------------------------------------------------------------------
mkdir -p "${EVID}" "${MATRIX_EVID}"
LAUNCHER_LOG="${EVID}/launcher.log"; : > "${LAUNCHER_LOG}"
SENTINEL="${EVID}/.cleaning"
MATRIX_VERDICT="${EVID}/matrix.verdict"
WATCHDOG_ABORT="${EVID}/.watchdog-abort"
CRED_TMP="${EVID}/.enroll.cred"
DERIVE_OUT="${EVID}/derived-runtime"
: > "${WORKER_TIMING_FILE}"
log "evidence directory: ${EVID}"
git -C "${REPO_ROOT}" rev-parse HEAD > "${EVID}/repo-head.txt" 2>/dev/null || echo unknown > "${EVID}/repo-head.txt"
{
  echo "run_id        ${RUN_ID}"
  echo "started_at    $(date -Is)"
  echo "repo_head     $(cat "${EVID}/repo-head.txt")"
  echo "lab_iface     ${LAB_IFACE}"
  echo "lab_ip        ${LAB_IP}"
  echo "ports         http=${PORT_HTTP} wss=${PORT_WSS} coord=${PORT_COORD} dp=${PORT_DP} matrix=${PORT_MATRIX} sink=${PORT_SINK}"
  echo "storage_root  ${STORAGE_ROOT}"
  echo "worker_timing ${WORKER_TIMING_FILE}"
  echo "free_bytes    ${FREE_BYTES}"
  echo "payload_bytes ${MATRIX_PAYLOAD_BYTES}"
  echo "skew_window   [${SKEW_FLOOR_MS}, ${SKEW_CEIL_MS}] ms"
} > "${EVID}/resolved.txt"

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
    while proc_alive "${pid}" && [ "${w}" -lt 24 ]; do sleep 0.25; w=$((w + 1)); done
    if proc_alive "${pid}"; then
      if [ "${sudo}" -eq 1 ]; then sudo pkill -KILL -P "${pid}" 2>/dev/null; sudo kill -KILL "${pid}" 2>/dev/null
      else kill -KILL "${pid}" 2>/dev/null; fi
    fi
    wait "${pid}" 2>/dev/null || true
    log "  stopped ${desc} (pid ${pid})"
  done
  [ -n "${CRED_TMP}" ] && { shred -u "${CRED_TMP}" 2>/dev/null || rm -f "${CRED_TMP}"; }
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
  [ "${OWNED_NM}" -eq 1 ] && sudo nmcli device set "${LAB_IFACE}" managed yes >/dev/null 2>&1 && log "  reverted: returned ${LAB_IFACE} to NetworkManager"
  if [ "${OWNED_IP}" -eq 0 ] && [ -z "${OWNED_FW}" ] && [ "${OWNED_NM}" -eq 0 ]; then
    log "  network: this launcher changed NOTHING — pre-existing lab state left as found"
  fi
  log "  Artifacts + evidence preserved. Evidence: ${EVID}"
  log "Stage-4 lab down."
  hr
}
# shellcheck disable=SC2329
on_signal() {
  trap - INT TERM
  echo
  local rc; rc="$(resolve_terminal_exit)"
  if [ -f "${MATRIX_VERDICT}" ]; then
    log "Stage-4 micro-matrix terminal: $(cat "${MATRIX_VERDICT}" 2>/dev/null) — lab shutting down (launcher exit ${rc})"
  elif [ -f "${WATCHDOG_ABORT}" ]; then
    log "a lab service died before the matrix terminal — lab shutting down (launcher exit ${rc})"
  else
    log "signal received before the matrix terminal — lab shutting down (launcher exit ${rc})"
  fi
  exit "${rc}"
}
trap on_signal INT TERM
trap cleanup EXIT

# ---------------------------------------------------------------------
# 3. lab network runtime (check first, mutate only what is missing)
# ---------------------------------------------------------------------
hr
log "LAB NETWORK RUNTIME (runtime-only; NOT the Bamep appliance network design)"
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
# 4. #61-shaped harness in --stage4 mode (fingerprint source + Worker timing sink)
# ---------------------------------------------------------------------
hr
log "STARTING #61-SHAPED HARNESS (--stage4: real Postgres + WSS + Worker HTTPS + PUT timing sink)"
HARNESS_LOG="${EVID}/harness.log"
env I63_LAB_IP="${LAB_IP}" I63_WSS_PORT="${PORT_WSS}" I63_COORD_PORT="${PORT_COORD}" I63_DP_PORT="${PORT_DP}" \
    "${HARNESS_BIN}" --stage4 --storage-root "${STORAGE_ROOT}" --worker-timing-file "${WORKER_TIMING_FILE}" \
    > "${HARNESS_LOG}" 2>&1 &
register_child "stage4-harness" "$!" 0
await "harness worker.https_listening" 120 grep -q '"event":"worker.https_listening"' "${HARNESS_LOG}" \
  || die "stage4-harness never reported worker.https_listening — see ${HARNESS_LOG}"
await "harness WSS tcp/${PORT_WSS}"   40 tcp_up "${PORT_WSS}"  || die "no WSS listener — see ${HARNESS_LOG}"
await "harness coord tcp/${PORT_COORD}" 40 tcp_up "${PORT_COORD}" || die "no coord listener — see ${HARNESS_LOG}"
await "harness data-plane tcp/${PORT_DP}" 40 tcp_up "${PORT_DP}" || die "no data-plane listener — see ${HARNESS_LOG}"
grep -q '"event":"worker.put_timing_sink"' "${HARNESS_LOG}" || die "harness did not report the Worker PUT timing sink"

FINGERPRINT="$(grep -oE '"server_leaf_sha256":"[0-9a-f]{64}"' "${HARNESS_LOG}" | head -1 | grep -oE '[0-9a-f]{64}' || true)"
[ "${#FINGERPRINT}" -eq 64 ] || die "could not obtain a 64-hex harness leaf fingerprint — see ${HARNESS_LOG}"
printf '%s\n' "${FINGERPRINT}" > "${EVID}/fingerprint.txt"
log "  harness leaf-cert SHA-256 fingerprint captured; Worker PUT timing sink armed"

# ---------------------------------------------------------------------
# 5. ONE fresh first-contact enrollment credential (never printed)
# ---------------------------------------------------------------------
log "[credential] minting exactly one fresh first-contact enrollment credential"
( umask 077; env I63_LAB_IP="${LAB_IP}" "${HARNESS_BIN}" issue-credential "${RUN_ID}" > "${CRED_TMP}" 2> "${EVID}/credential-issue.stderr.log" ) \
  || { cat "${EVID}/credential-issue.stderr.log" >&2; die "issue-credential failed"; }
[ -s "${CRED_TMP}" ] || die "issue-credential produced an empty credential"
chmod 600 "${CRED_TMP}"
log "  fresh credential written (mode 600, value NOT logged)"

# ---------------------------------------------------------------------
# 6. derive the Issue-63 Stage-4 runtime (pin baked into the WinPE bootstrap)
# ---------------------------------------------------------------------
hr
log "DERIVING the Issue-63 Stage-4 PXE/WinPE runtime (Phase-9d consumed READ-ONLY)"
if ! "${DERIVE}" --stage 4 --out "${DERIVE_OUT}" --runner-exe "${RUNNER_EXE}" --probe-exe "${PROBE_EXE}" \
      --enroll-cred "${CRED_TMP}" --pin "${FINGERPRINT}" \
      --lab-ip "${LAB_IP}" --http-port "${PORT_HTTP}" --matrix-port "${PORT_MATRIX}" \
      --coord-port "${PORT_COORD}" --wss-port "${PORT_WSS}" --sink-port "${PORT_SINK}" \
      --skew-floor-ms "${SKEW_FLOOR_MS}" --skew-ceil-ms "${SKEW_CEIL_MS}" \
      --net-wait-secs "${NET_WAIT_SECS}" --seal-timeout-secs "${SEAL_TIMEOUT_SECS}" \
      --model-substr "${MODEL_SUBSTR}" --run-id "${RUN_ID}" --iface "${LAB_IFACE}" \
      > "${EVID}/derive.log" 2>&1; then
  cat "${EVID}/derive.log" >&2
  die "derive-stage3-runtime.sh --stage 4 failed — see ${EVID}/derive.log"
fi
grep -q '^DERIVE_OK ' "${EVID}/derive.log" || die "derive did not report DERIVE_OK"
diff -q "${DERIVE_OUT}/phase9d-hashes-before.txt" "${DERIVE_OUT}/phase9d-hashes-after.txt" >/dev/null \
  || die "Phase-9d assets changed during derive"
log "  derived runtime at ${DERIVE_OUT}; Phase-9d before/after IDENTICAL"

# ---------------------------------------------------------------------
# 7. WinPE HTTP + dnsmasq + Stage-4 coordinator
# ---------------------------------------------------------------------
hr
log "STARTING BACKGROUND LAB SERVICES"

log "[http] python3 http.server on ${LAB_IP}:${PORT_HTTP} serving ${DERIVE_OUT}/http"
python3 -m http.server --bind "${LAB_IP}" --directory "${DERIVE_OUT}/http" "${PORT_HTTP}" > "${EVID}/http.log" 2>&1 &
register_child "winpe-http" "$!" 0
await "http tcp/${PORT_HTTP}" 30 tcp_up "${PORT_HTTP}" || die "HTTP server did not start"

log "[dnsmasq] DHCP + TFTP on ${LAB_IFACE} (derived conf)"
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

log "[coordinator] bamep-i63-stage1-coordinator --stage4 --arm (typed 10-case authority + probe sink)"
rm -f "${MATRIX_VERDICT}"
"${COORD_BIN}" --stage4 --arm --matrix-addr "${LAB_IP}:${PORT_MATRIX}" --sink-addr "${LAB_IP}:${PORT_SINK}" \
  --evidence-dir "${MATRIX_EVID}" --run-id "${RUN_ID}" --verdict-file "${MATRIX_VERDICT}" \
  --worker-timing-file "${WORKER_TIMING_FILE}" > "${EVID}/coordinator.log" 2>&1 &
register_child "stage4-coordinator" "$!" 0
await "coordinator STAGE4_LISTENING" 30 grep -q '^STAGE4_LISTENING ' "${EVID}/coordinator.log" \
  || { cat "${EVID}/coordinator.log" >&2; die "coordinator never reported listening"; }
await "coordinator matrix tcp/${PORT_MATRIX}" 20 tcp_up "${PORT_MATRIX}" || die "no matrix listener"
await "coordinator sink tcp/${PORT_SINK}"   20 tcp_up "${PORT_SINK}"   || die "no sink listener"

# ---------------------------------------------------------------------
# 8. readiness gate
# ---------------------------------------------------------------------
hr
log "READINESS GATE"
gate_fail=0
gate() { local d="$1"; shift; if "$@"; then log "  PASS  ${d}"; else log "  FAIL  ${d}"; gate_fail=1; fi; }

gate "${LAB_IFACE} has ${LAB_CIDR}"          bash -c "ip -4 addr show '${LAB_IFACE}' | grep -qF 'inet ${LAB_CIDR}'"
gate "udp/67 (DHCP) listening"               udp_up 67
gate "udp/69 (TFTP) listening"               udp_up 69
gate "tcp/${PORT_HTTP} (WinPE HTTP)"         tcp_up "${PORT_HTTP}"
gate "tcp/${PORT_WSS} (Agent WSS)"           tcp_up "${PORT_WSS}"
gate "tcp/${PORT_COORD} (harness coord)"     tcp_up "${PORT_COORD}"
gate "tcp/${PORT_DP} (Worker HTTPS)"         tcp_up "${PORT_DP}"
gate "tcp/${PORT_MATRIX} (stage4 coord)"     tcp_up "${PORT_MATRIX}"
gate "tcp/${PORT_SINK} (probe sink)"         tcp_up "${PORT_SINK}"
gate "harness worker.https_listening"        grep -q '"event":"worker.https_listening"' "${HARNESS_LOG}"
gate "harness Worker PUT timing sink armed"  grep -q '"event":"worker.put_timing_sink"' "${HARNESS_LOG}"
gate "worker timing sink file exists + empty" bash -c "[ -f '${WORKER_TIMING_FILE}' ] && [ ! -s '${WORKER_TIMING_FILE}' ]"
gate "stage4 coordinator ARMED"              grep -q '^STAGE4_ARMED ' "${EVID}/coordinator.log"
gate "no STAGE4_START_FAIL"                  bash -c "! grep -q 'STAGE4_START_FAIL' '${EVID}/coordinator.log'"
gate "Phase-9d assets still match pinned"    check_pinned
if command -v objdump >/dev/null 2>&1; then
  gate "runner PE imports within proven set"  pe_dlls_ok "${RUNNER_EXE}" runner
  gate "probe PE imports within proven set"   pe_dlls_ok "${PROBE_EXE}" probe
fi
for aw in "wimboot|${PINNED_HTTP_WIMBOOT}" "BCD|${PINNED_HTTP_BCD}" "boot.sdi|${PINNED_HTTP_BOOTSDI}" "boot.wim|${PINNED_HTTP_BOOTWIM}"; do
  a="${aw%%|*}"; w="${aw##*|}"
  got="$(curl -sf "http://${LAB_IP}:${PORT_HTTP}/${a}" | sha256sum | awk '{print $1}')" || got="ERR"
  gate "HTTP /${a} serves pinned bytes" bash -c "[ '${got}' = '${w}' ]"
done
for f in winpeshl.ini bamep-i63-stage4-bootstrap.cmd bamep-i63-runner.exe bamep-i63-stage2-probe.exe bamep-i63-enroll.cred; do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://${LAB_IP}:${PORT_HTTP}/${f}")"
  gate "HTTP /${f} -> 200" bash -c "[ '${code}' = '200' ]"
done
AX="${DERIVE_OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
gate "autoexec injects winpeshl.ini"        grep -qF "/winpeshl.ini winpeshl.ini" "${AX}"
gate "autoexec injects stage4 bootstrap"    grep -qF "/bamep-i63-stage4-bootstrap.cmd bamep-i63-stage4-bootstrap.cmd" "${AX}"
gate "autoexec injects runner.exe"          grep -qF "/bamep-i63-runner.exe bamep-i63-runner.exe" "${AX}"
gate "autoexec injects probe.exe"           grep -qF "/bamep-i63-stage2-probe.exe bamep-i63-stage2-probe.exe" "${AX}"
gate "autoexec injects enroll.cred"         grep -qF "/bamep-i63-enroll.cred bamep-i63-enroll.cred" "${AX}"
gate "autoexec: boot.wim initrd is last"    bash -c "[ \"\$(grep '^initrd ' '${AX}' | tail -1)\" = \"initrd http://${LAB_IP}:${PORT_HTTP}/boot.wim boot.wim\" ]"
gate "bootstrap.cmd launches runner --stage4 --arm" grep -qF -- '--stage4 --arm' "${DERIVE_OUT}/http/bamep-i63-stage4-bootstrap.cmd"
gate "bootstrap.cmd carries this fingerprint"       grep -qF "${FINGERPRINT}" "${DERIVE_OUT}/http/bamep-i63-stage4-bootstrap.cmd"
gate "matrix-plan.json written (10 cases)"   bash -c "[ -f '${MATRIX_EVID}/matrix-plan.json' ] && [ \"\$(grep -c '\"case_id\"' '${MATRIX_EVID}/matrix-plan.json')\" -ge 10 ]"

for i in "${!CHILD_PID[@]}"; do
  gate "child alive: ${CHILD_DESC[$i]} (pid ${CHILD_PID[$i]})" proc_alive "${CHILD_PID[$i]}"
done

{
  echo "run_id     ${RUN_ID}"
  echo "ready_at   $(date -Is)"
  echo "gate_fail  ${gate_fail}"
  echo "fingerprint ${FINGERPRINT}"
  echo "worker_timing_file ${WORKER_TIMING_FILE}"
  for i in "${!CHILD_PID[@]}"; do printf 'pid %-8s sudo=%s  %s\n' "${CHILD_PID[$i]}" "${CHILD_SUDO[$i]}" "${CHILD_DESC[$i]}"; done
} > "${EVID}/readiness-summary.txt"

[ "${gate_fail}" -eq 0 ] || die "readiness gate FAILED — see the FAIL lines above. Cleaning up."

# ---------------------------------------------------------------------
# 9. health watchdog
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
        if [ "${names[$k]}" = "stage4-coordinator" ] && [ -f "${MATRIX_VERDICT}" ]; then
          log ""
          log ">>> stage4 coordinator reached its terminal verdict ($(cat "${MATRIX_VERDICT}" 2>/dev/null)) and exited (expected) — tearing the lab down"
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
# 10. READY
# ---------------------------------------------------------------------
cat <<EOF | tee -a "${LAUNCHER_LOG}"

==================================================
READY_FOR_STAGE4_MINIPC_POWER_ON   (${RUN_ID})
==================================================
DHCP/TFTP      : READY   (dnsmasq, derived Issue-63 conf, ${LAB_IFACE})
WinPE HTTP     : READY   (${LAB_IP}:${PORT_HTTP}  ${DERIVE_OUT}/http)
Stage-4 harness: READY   real Postgres(${DB_NAME}) + WSS ${LAB_IP}:${PORT_WSS} + Worker HTTPS ${LAB_IP}:${PORT_DP} + coord ${LAB_IP}:${PORT_COORD}
                 Worker PUT timing sink -> ${WORKER_TIMING_FILE}
Stage-4 coord  : READY   ARMED   (typed 10-case S/P authority ${LAB_IP}:${PORT_MATRIX}  +  probe sink ${LAB_IP}:${PORT_SINK})
Phase-9d       : byte-identical before/after derive (7/7 pinned)
Storage root   : ${STORAGE_ROOT}   free=${FREE_BYTES} bytes  (>= 30 GiB gate; 20 GiB payload preserved)
Clock method   : SetSystemTime (UTC); strict skew window [${SKEW_FLOOR_MS}, ${SKEW_CEIL_MS}] ms; aligned OUTSIDE every measured wall

Micro-matrix (fixed):
  chunk size  64 MiB ONLY   ->   32 chunks over 2048 MiB (no partial final chunk)
  extent      2,147,483,648 bytes (2048 MiB) per case
  warm-ups (excluded): S, P    measured cycles: (S,P) (P,S) (S,P) (P,S)  => 2 + 8 = 10 transfers
  ONE WinPE boot, NO reboot between cases, NO deliberate fault injection
  PREP-AHEAD DEPTH 2 = PREP-AHEAD ONLY: one PUT in flight, ascending PUT order, no multi-PUT concurrency

Failure policy: any source-safety failure / unexpected retry|resume|auth suspension / transport|read|
  digest|proof|Worker|seal failure / Artifact != Verified / malformed result / per-case process death
  -> the case is FAILED or CONTAMINATED, the coordinator stops handing out cases, all completed
  evidence is preserved, and the matrix terminates NON-ZERO. NO "retry until green".
  A 10/10 run with MISSING Worker PUT timing evidence terminates stage4_invalid (Q2 unanswerable).

Evidence: ${EVID}
  launcher.log  harness.log  http.log  dnsmasq.log  coordinator.log  derive.log  fingerprint.txt
  matrix/{matrix-plan.json, case-results.ndjson, coordinator-events.ndjson, probe-evidence.ndjson,
          worker-put-timing.ndjson, analysis.json}
  derived-runtime/{phase9d-hashes-*.txt, derived-manifest.txt}   matrix.verdict

Owner action (ONLY this):
  1. (done) launcher running / sudo password entered.
  2. Power ON the MiniPC. It DHCP-leases in 192.168.99.50-100 and PXE-boots.
  3. Press a key ONCE at wimboot's "Press any key to continue booting..." prompt.
  4. Type NOTHING in WinPE. winpeshl.ini auto-runs bootstrap -> wpeinit -> runner --stage4 --arm,
     which drives all 10 cases (serial + prep-ahead) and reports each to the Stage-4 coordinator.
Watch this terminal for  STAGE4_MATRIX_TERMINAL marker=stage4_pass  (or stage4_fail / stage4_invalid). Ctrl-C to tear down.
==================================================
EOF

# ---------------------------------------------------------------------
# 11. foreground supervise — stream the coordinator log
# ---------------------------------------------------------------------
(
  tail -n +1 -F "${EVID}/coordinator.log" 2>/dev/null | while IFS= read -r line; do
    printf '%s\n' "${line}"
    case "${line}" in
      STAGE4_MATRIX_TERMINAL*marker=stage4_pass*)
        printf '\n############################################\n# STAGE 4 PASS — 10/10 verified + Worker PUT timing captured — review analysis.json\n# %s\n############################################\n' "${EVID}" ;;
      STAGE4_MATRIX_TERMINAL*marker=stage4_invalid*)
        printf '\n????????????????????????????????????????????\n? STAGE 4 INVALID — 10/10 completed but Worker PUT timing evidence missing/short (Q2 unanswerable)\n? %s\n????????????????????????????????????????????\n' "${EVID}" ;;
      STAGE4_MATRIX_TERMINAL*marker=stage4_fail*)
        printf '\n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n! STAGE 4 FAILED — stopped, evidence preserved\n! %s\n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n' "${EVID}" ;;
    esac
  done
) &
TAIL_PID=$!
CHILD_DESC+=("coordinator-tail"); CHILD_PID+=("${TAIL_PID}"); CHILD_SUDO+=("0")
wait "${TAIL_PID}" 2>/dev/null || true

RC="$(resolve_terminal_exit)"
log "Stage-4 lab supervise ended — launcher exit ${RC} (verdict: $(cat "${MATRIX_VERDICT}" 2>/dev/null || echo none))"
exit "${RC}"
