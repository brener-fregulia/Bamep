#!/usr/bin/env bash
#
# Bamep Issue #65 — throughput-first physical capture lab launcher (LAB-ONLY).
#
# =====================================================================
# NOT APPLIANCE ARCHITECTURE. NOT PRODUCTION SERVICE MANAGEMENT.
# NOT THE Bamep FIREWALL / NETWORK / BOOT / DATA-PLANE DESIGN.
# =====================================================================
#
# A single throwaway foreground supervisor that owns every background service
# the Issue-65 first-candidate needs, so the operator runs ONE launcher in ONE
# terminal. It composes ALREADY-PROVEN assets:
#   * Issue #53 Phase 9d PXE/WinPE runtime  /var/tmp/bamep-issue53-phase9d-winpe-completion
#       (consumed READ-ONLY; re-hashed before and after; never modified)
#   * the Issue-65 capture probe (WinPE) + capture sink (Fedora), this directory
#
# It brings up: dnsmasq (DHCP+TFTP), a python HTTP server for the WinPE boot
# assets, and the plain-TCP capture sink. It NEVER powers the MiniPC and NEVER
# triggers the PXE boot.
#
#   bash -n run-issue65-lab.sh
#   ./run-issue65-lab.sh --preflight     (read-only gate; no services)
#   ./run-issue65-lab.sh                  (bring the lab up and supervise it)
#
# Env overrides: I65_LAB_IFACE (enp8s0), I65_LAB_IP (192.168.99.1),
#   I65_FW_ZONE (trusted), I65_EXTENT_BYTES (2147483648), I65_TRANSFERS (3, per
#   destination), I65_SINK_DEST (<evidence>/capture.bin), I65_DISCARD (0),
#   I65_DEST_DIRS (comma-separated writable dirs — per-disk comparison, one boot).
#
set -euo pipefail
export LC_ALL=C

SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
I65_DIR="$(dirname "${SCRIPT_PATH}")"
REPO_ROOT="$(cd "${I65_DIR}/../../.." && pwd)"

LAB_IFACE="${I65_LAB_IFACE:-enp8s0}"
LAB_IP="${I65_LAB_IP:-192.168.99.1}"
LAB_CIDR="${LAB_IP}/24"
FW_ZONE="${I65_FW_ZONE:-trusted}"

PORT_HTTP=8080
PORT_SINK=9265

EXTENT_BYTES="${I65_EXTENT_BYTES:-2147483648}"
TRANSFERS="${I65_TRANSFERS:-3}"        # per destination: 1 warmup + 2 measured
CONNECT_WAIT_SECS="${I65_CONNECT_WAIT_SECS:-60}"

# Issue #65 control-run switches, both routed through the probe's existing flags
# via derive's --probe-extra-args (throwaway Spike isolation knobs, not product):
#   I65_NO_DIGEST=1        -> --no-digest        (disable endpoint SHA-256 / hashing)
#   I65_SYNTHETIC_SOURCE=1 -> --synthetic-source (deterministic in-memory source;
#                             NO enumeration / device open / disk read — isolates
#                             whether the raw source-read path is the limiter)
# Neither run carries any payload-correctness meaning.
PROBE_EXTRA_ARGS=""
if [ "${I65_NO_DIGEST:-0}" = "1" ]; then PROBE_EXTRA_ARGS="${PROBE_EXTRA_ARGS:+${PROBE_EXTRA_ARGS} }--no-digest"; fi
if [ "${I65_SYNTHETIC_SOURCE:-0}" = "1" ]; then PROBE_EXTRA_ARGS="${PROBE_EXTRA_ARGS:+${PROBE_EXTRA_ARGS} }--synthetic-source"; fi

# Optional per-disk destination comparison (Issue #65 disk test). Comma-separated
# list of writable directories, each on the filesystem under test. When set, the
# sink runs ${TRANSFERS} connections against each directory IN ORDER
# (total = TRANSFERS * ndirs) in ONE MiniPC boot; unset => the single evidence
# directory is used exactly as before.
DEST_DIRS=()
if [ -n "${I65_DEST_DIRS:-}" ]; then
  IFS=',' read -r -a DEST_DIRS <<< "${I65_DEST_DIRS}"
fi
NDIRS="${#DEST_DIRS[@]}"
if [ "${NDIRS}" -gt 0 ]; then TOTAL_TRANSFERS=$((TRANSFERS * NDIRS)); else TOTAL_TRANSFERS="${TRANSFERS}"; fi

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"
SINK_BIN="${I65_DIR}/sink/target/release/bamep-i65-capture-sink"
PROBE_EXE="${I65_DIR}/capture-probe/target/x86_64-pc-windows-msvc/release/bamep-i65-capture-probe.exe"
DERIVE="${I65_DIR}/derive-issue65-runtime.sh"

PINNED_HTTP_WIMBOOT="5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3"
PINNED_HTTP_BCD="c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41"
PINNED_HTTP_BOOTSDI="cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd"
PINNED_HTTP_BOOTWIM="fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197"
PINNED_TFTP_SHIM="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
PINNED_TFTP_SNPONLY="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
PINNED_TFTP_IPXE="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"

# Stock-WinPE-proven DLL set (#60/#61 evidence). The capture probe must import a
# SUBSET of this — a new name is a STOP-and-report condition.
PROVEN_DLLS="api-ms-win-core-synch-l1-2-0.dll kernel32.dll ntdll.dll ws2_32.dll advapi32.dll bcrypt.dll bcryptprimitives.dll"

RUN_ID="i65-$(date +%Y%m%dT%H%M%S)"
EVID="${I65_DIR}/evidence/${RUN_ID}"
SINK_DEST="${I65_SINK_DEST:-${EVID}/capture.bin}"
SENTINEL=""
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
    [ "${got}" = "${want}" ] || { log "  HASH MISMATCH ${f}"; ok=0; }
  done
  [ "${ok}" = "1" ]
}

probe_dlls() { LC_ALL=C objdump -p "$1" 2>/dev/null | sed -n 's/.*DLL Name: //p' | tr 'A-Z' 'a-z' | sort -u; }
probe_dlls_ok() {
  local d
  while read -r d; do
    [ -z "${d}" ] && continue
    case " ${PROVEN_DLLS} " in *" ${d} "*) : ;; *) log "  UNPROVEN DLL import: ${d}"; return 1 ;; esac
  done < <(probe_dlls "$1")
  return 0
}

usage() { sed -n '3,32p' "${SCRIPT_PATH}" | sed 's/^#\{1,\} \{0,1\}//;s/^#$//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --preflight|--dry-run) MODE="preflight"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

# ---------------------------------------------------------------------
# 1. PREFLIGHT — read-only.
# ---------------------------------------------------------------------
hr
log "Bamep Issue #65 throughput-first capture lab launcher — ${RUN_ID}"
log "MODE=${MODE}   (LAB-ONLY scaffolding; NOT the appliance design)"
hr

log "[preflight] repository + build artifacts"
require_file "${REPO_ROOT}/AGENTS.md" "repo root sanity"
require_file "${DERIVE}" "derive-issue65-runtime.sh"
require_file "${SINK_BIN}" "build: cd ${I65_DIR}/sink && cargo build --release"
require_file "${PROBE_EXE}" "cross-build: cd ${I65_DIR}/capture-probe && RUSTFLAGS='-C target-feature=+crt-static' cargo xwin build --release --target x86_64-pc-windows-msvc"
[ "$(head -c2 "${PROBE_EXE}")" = "MZ" ] || die "probe exe is not a PE image: ${PROBE_EXE}"
log "  ok: sink + probe exe present"

log "[preflight] capture probe PE imports are a subset of the stock-WinPE-proven set"
if command -v objdump >/dev/null 2>&1; then
  probe_dlls_ok "${PROBE_EXE}" || die "probe imports a DLL outside the #60/#61-proven set — STOP and report"
  log "  ok: $(probe_dlls "${PROBE_EXE}" | tr '\n' ' ')"
else
  log "  warn: objdump not installed — cannot check PE imports here"
fi

log "[preflight] Issue #53 Phase 9d assets present and byte-identical to pinned"
[ -d "${PHASE9D_DIR}" ] || die "Phase 9d runtime not found at ${PHASE9D_DIR}"
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
for p in "${PORT_HTTP}" "${PORT_SINK}"; do
  tcp_up "${p}" && { log "  CONFLICT: something already listens on tcp/${p}"; CONFLICT=1; }
done
udp_up 67 && { log "  CONFLICT: udp/67 (DHCP) already in use"; CONFLICT=1; }
udp_up 69 && { log "  CONFLICT: udp/69 (TFTP) already in use"; CONFLICT=1; }
[ "${CONFLICT}" -eq 0 ] || die "resolve the listed port conflicts"
log "  ok: all lab ports free"

log "[preflight] scratch for the derived runtime + one throwaway destination file"
mkdir -p "${I65_DIR}/evidence"
FREE_BYTES="$(df -B1 --output=avail "${I65_DIR}/evidence" | tail -1 | tr -d ' ')"
# The sink truncates + deletes the destination between transfers, so one extent
# plus headroom is enough; gate at 8 GiB to be safe.
[ "${FREE_BYTES}" -ge 8589934592 ] || die "want >= 8 GiB free under ${I65_DIR}/evidence (have ${FREE_BYTES})"
log "  ok: ${FREE_BYTES} bytes free"

ROOT_SRC="$(findmnt -n -o SOURCE -T / 2>/dev/null || true)"
if [ "${NDIRS}" -gt 0 ]; then
  log "[preflight] per-disk destination directories (I65_DEST_DIRS, ${NDIRS} targets, ${TRANSFERS} transfers each)"
  for d in "${DEST_DIRS[@]}"; do
    [ -d "${d}" ] || die "destination dir does not exist: ${d} (create + mount it first)"
    mnt="$(findmnt -n -o TARGET -T "${d}" 2>/dev/null || true)"
    src="$(findmnt -n -o SOURCE -T "${d}" 2>/dev/null || true)"
    fstype="$(findmnt -n -o FSTYPE -T "${d}" 2>/dev/null || true)"
    [ -n "${mnt}" ] || die "${d} is not on any mounted filesystem"
    [ "${mnt}" != "/" ] || die "${d} resolves to the ROOT filesystem (${src}) — the disk test must target a dedicated mount, not the NVMe"
    [ "${src}" != "${ROOT_SRC}" ] || die "${d} is backed by the root device ${src} — refusing (must be a separate disk)"
    probe="${d}/.i65-write-probe-$$"
    ( : > "${probe}" ) 2>/dev/null && rm -f "${probe}" || die "destination dir not writable by $(id -un): ${d}"
    davail="$(df -B1 --output=avail "${d}" | tail -1 | tr -d ' ')"
    [ "${davail}" -ge 4294967296 ] || die "want >= 4 GiB free on ${d} (have ${davail})"
    log "  ok: ${d}  mount=${mnt}  dev=${src}  fstype=${fstype}  free=${davail} B"
  done
fi

if [ "${MODE}" = "preflight" ]; then
  hr
  log "PREFLIGHT_OK — no network state changed, no services started."
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
log "evidence directory: ${EVID}"
git -C "${REPO_ROOT}" rev-parse HEAD > "${EVID}/repo-head.txt" 2>/dev/null || echo "unknown" > "${EVID}/repo-head.txt"
{
  echo "run_id        ${RUN_ID}"
  echo "started_at    $(date -Is)"
  echo "repo_head     $(cat "${EVID}/repo-head.txt")"
  echo "lab_iface     ${LAB_IFACE}"
  echo "lab_ip        ${LAB_IP}"
  echo "ports         http=${PORT_HTTP} sink=${PORT_SINK}"
  echo "extent_bytes  ${EXTENT_BYTES}"
  echo "probe_extra   ${PROBE_EXTRA_ARGS:-<none> (real source, digest enabled)}"
  echo "transfers     ${TRANSFERS}/dest (1 warmup + $((TRANSFERS - 1)) measured); total ${TOTAL_TRANSFERS}"
  if [ "${NDIRS}" -gt 0 ]; then
    echo "dest_dirs     ${DEST_DIRS[*]}"
  else
    echo "sink_dest     ${SINK_DEST}"
  fi
} > "${EVID}/resolved.txt"

# ---------------------------------------------------------------------
# 3. cleanup / supervision trap
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
    proc_alive "${pid}" && { if [ "${sudo}" -eq 1 ]; then sudo kill -KILL "${pid}" 2>/dev/null; else kill -KILL "${pid}" 2>/dev/null; fi; }
    wait "${pid}" 2>/dev/null || true
    log "  stopped ${desc} (pid ${pid})"
  done
  rm -f "${SINK_DEST}" 2>/dev/null || true
  # Sweep the exact throwaway capture file from every per-disk destination
  # (covers a mid-run Ctrl-C where the sink did not get to clean up).
  if [ "${NDIRS:-0}" -gt 0 ]; then
    for d in "${DEST_DIRS[@]}"; do rm -f "${d}/capture.bin" 2>/dev/null || true; done
  fi
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
  log "Issue-65 lab down."
  hr
}
# shellcheck disable=SC2329
on_signal() {
  trap - INT TERM
  echo
  if [ -e "${EVID}/.unexpected" ]; then
    log "a lab service died unexpectedly — lab shutting down"
    exit 20
  fi
  log "signal received — lab shutting down"
  exit 130
}
trap on_signal INT TERM
trap cleanup EXIT

# ---------------------------------------------------------------------
# 4. derive the Issue-65 runtime
# ---------------------------------------------------------------------
hr
log "DERIVING the Issue-65 PXE/WinPE runtime (Phase-9d assets consumed READ-ONLY)"
DERIVE_OUT="${EVID}/derived-runtime"
DERIVE_EXTRA=()
[ -n "${PROBE_EXTRA_ARGS}" ] && DERIVE_EXTRA=(--probe-extra-args "${PROBE_EXTRA_ARGS}")
if ! "${DERIVE}" --out "${DERIVE_OUT}" --probe-exe "${PROBE_EXE}" \
      --lab-ip "${LAB_IP}" --http-port "${PORT_HTTP}" --sink-port "${PORT_SINK}" \
      --extent-bytes "${EXTENT_BYTES}" --connect-wait-secs "${CONNECT_WAIT_SECS}" \
      --total-transfers "${TOTAL_TRANSFERS}" \
      ${DERIVE_EXTRA[@]+"${DERIVE_EXTRA[@]}"} \
      --run-id "${RUN_ID}" --iface "${LAB_IFACE}" \
      > "${EVID}/derive.log" 2>&1; then
  cat "${EVID}/derive.log" >&2
  die "derive-issue65-runtime.sh failed — see ${EVID}/derive.log"
fi
grep -q '^DERIVE_OK ' "${EVID}/derive.log" || die "derive did not report DERIVE_OK"
log "  derived runtime at ${DERIVE_OUT}"
diff -q "${DERIVE_OUT}/phase9d-hashes-before.txt" "${DERIVE_OUT}/phase9d-hashes-after.txt" >/dev/null || die "Phase-9d assets changed during derive"
log "  Phase-9d before/after: IDENTICAL (7/7 pinned)"

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

SINK_EXTRA=()
[ "${I65_DISCARD:-0}" = "1" ] && SINK_EXTRA+=(--discard) && log "  NOTE: I65_DISCARD=1 — sink will NOT write the destination file (destination-storage isolation diagnostic)"
if [ "${NDIRS}" -gt 0 ]; then
  for d in "${DEST_DIRS[@]}"; do SINK_EXTRA+=(--dest-dir "${d}"); done
  log "[sink] bamep-i65-capture-sink on ${LAB_IP}:${PORT_SINK} (${TRANSFERS}/dest x ${NDIRS} dests = ${TOTAL_TRANSFERS} transfers)"
  for d in "${DEST_DIRS[@]}"; do log "        dest: ${d}"; done
else
  SINK_EXTRA+=(--dest "${SINK_DEST}")
  log "[sink] bamep-i65-capture-sink on ${LAB_IP}:${PORT_SINK} (count=${TRANSFERS}, dest=${SINK_DEST})"
fi
"${SINK_BIN}" --listen "${LAB_IP}:${PORT_SINK}" \
  --count "${TRANSFERS}" --extent-bytes "${EXTENT_BYTES}" ${SINK_EXTRA[@]+"${SINK_EXTRA[@]}"} > "${EVID}/sink.log" 2>&1 &
register_child "capture-sink" "$!" 0
await "sink I65_SINK_LISTENING" 30 grep -q '^I65_SINK_LISTENING ' "${EVID}/sink.log" || die "sink never reported listening — see ${EVID}/sink.log"
await "sink tcp/${PORT_SINK}" 20 tcp_up "${PORT_SINK}" || die "no sink listener"

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
gate "tcp/${PORT_SINK} (capture sink)"       tcp_up "${PORT_SINK}"
gate "Phase-9d assets still match pinned"    check_pinned
gate "probe PE imports within proven set"    probe_dlls_ok "${PROBE_EXE}"

for asset_want in "wimboot|${PINNED_HTTP_WIMBOOT}" "BCD|${PINNED_HTTP_BCD}" "boot.sdi|${PINNED_HTTP_BOOTSDI}" "boot.wim|${PINNED_HTTP_BOOTWIM}"; do
  a="${asset_want%%|*}"; w="${asset_want##*|}"
  got="$(curl -sf "http://${LAB_IP}:${PORT_HTTP}/${a}" | sha256sum | awk '{print $1}')" || got="ERR"
  gate "HTTP /${a} serves pinned bytes" bash -c "[ '${got}' = '${w}' ]"
done
for f in winpeshl.ini bamep-i65-bootstrap.cmd bamep-i65-capture-probe.exe; do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://${LAB_IP}:${PORT_HTTP}/${f}")"
  gate "HTTP /${f} -> 200" bash -c "[ '${code}' = '200' ]"
done
AX="${DERIVE_OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
gate "autoexec injects winpeshl.ini"        grep -qF "/winpeshl.ini winpeshl.ini" "${AX}"
gate "autoexec injects bootstrap.cmd"       grep -qF "/bamep-i65-bootstrap.cmd bamep-i65-bootstrap.cmd" "${AX}"
gate "autoexec injects probe.exe"           grep -qF "/bamep-i65-capture-probe.exe bamep-i65-capture-probe.exe" "${AX}"
gate "autoexec: boot.wim initrd is last"    bash -c "[ \"\$(grep '^initrd ' '${AX}' | tail -1)\" = \"initrd http://${LAB_IP}:${PORT_HTTP}/boot.wim boot.wim\" ]"
gate "bootstrap.cmd runs the ${TOTAL_TRANSFERS}-transfer loop" bash -c "grep -qF 'for /L %%i in (1,1,${TOTAL_TRANSFERS}) do' '${DERIVE_OUT}/http/bamep-i65-bootstrap.cmd' && grep -qF -- '--sink %SINK% --label t%%i --extent-bytes' '${DERIVE_OUT}/http/bamep-i65-bootstrap.cmd'"
if [ "${NDIRS}" -gt 0 ]; then
  gate "sink schedule = ${TOTAL_TRANSFERS} total connections" bash -c "grep -q 'total_connections=${TOTAL_TRANSFERS} ' '${EVID}/sink.log'"
fi
if [ -n "${PROBE_EXTRA_ARGS}" ]; then
  gate "bootstrap.cmd passes ${PROBE_EXTRA_ARGS} to the probe" bash -c "grep -qF -- '--connect-wait-secs ${CONNECT_WAIT_SECS} ${PROBE_EXTRA_ARGS}' '${DERIVE_OUT}/http/bamep-i65-bootstrap.cmd'"
fi

for i in "${!CHILD_PID[@]}"; do
  gate "child alive: ${CHILD_DESC[$i]} (pid ${CHILD_PID[$i]})" proc_alive "${CHILD_PID[$i]}"
done

[ "${gate_fail}" -eq 0 ] || die "readiness gate FAILED — see the FAIL lines above. Cleaning up."

# ---------------------------------------------------------------------
# 8. health watchdog (the sink exiting after ${TRANSFERS} connections is EXPECTED)
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
        if [ "${names[$k]}" = "capture-sink" ] && grep -q '^I65_SINK_DONE ' "${EVID}/sink.log" 2>/dev/null; then
          log ""
          log ">>> capture-sink received all ${TOTAL_TRANSFERS} transfers and exited (expected)"
          # Expected completion: let the foreground finish its result report and
          # exit on its own — do NOT signal main.
          touch "${SENTINEL}" 2>/dev/null; return 0
        fi
        log ""
        log "!!! ${names[$k]} (pid ${pids[$k]}) EXITED UNEXPECTEDLY — lab is NOT READY"
        touch "${EVID}/.unexpected" 2>/dev/null
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
DHCP/TFTP     : READY   (dnsmasq, derived Issue-65 conf, ${LAB_IFACE})
WinPE HTTP    : READY   (${LAB_IP}:${PORT_HTTP}  ${DERIVE_OUT}/http)
Capture sink  : READY   (${LAB_IP}:${PORT_SINK}  ${TRANSFERS}/dest  extent=${EXTENT_BYTES} B  total=${TOTAL_TRANSFERS})
$([ "${NDIRS}" -gt 0 ] && printf 'Destinations  : %s\n' "$(printf '%s ' "${DEST_DIRS[@]}")" || printf 'Destination   : %s\n' "${SINK_DEST}")
Phase-9d      : byte-identical before/after derive (7/7 pinned)
Probe imports : $(probe_dlls "${PROBE_EXE}" | tr '\n' ' ') (subset of stock-WinPE-proven set)

Source safety : opaque same-boot source epoch -> resolver -> Issue-63 predicate
                (model 'NGFF 2280 256GB SSD', exact length 256,060,514,304,
                 extent <= length, no PhysicalDrive0 authority); GENERIC_READ
                only; fixed ${EXTENT_BYTES}-byte extent; a Reject reads ZERO
                bulk bytes and sends nothing.

Owner action (ONLY this):
  1. Power ON the disposable MiniPC (it will DHCP-lease in 192.168.99.50-100).
  2. Press a key ONCE at wimboot's "Press any key to continue booting..." prompt.
  3. Type NOTHING in WinPE. The injected winpeshl.ini auto-runs:
       wpeinit -> ${TOTAL_TRANSFERS} back-to-back 2 GiB captures (t1..t${TOTAL_TRANSFERS}).
       Per destination: transfer 1 = warmup, transfers 2-${TRANSFERS} = measured.
  Watch THIS terminal for the ${TOTAL_TRANSFERS} 'i65_sink_result' lines
  (each carries "dest_dir" so you can tell which disk).

Evidence: ${EVID}
  launcher.log  derive.log  dnsmasq.log  http.log  sink.log
  derived-runtime/{phase9d-hashes-*.txt,derived-manifest.txt}
==================================================
EOF

# ---------------------------------------------------------------------
# 10. foreground supervise — stream the sink output
# ---------------------------------------------------------------------
(
  tail -n +1 -F "${EVID}/sink.log" 2>/dev/null | while IFS= read -r line; do
    printf '%s\n' "${line}"
    case "${line}" in
      *'"i65_sink_result":true'*)
        printf '\n########## Issue-65 capture result ##########\n%s\n#############################################\n' "${line}" ;;
      I65_SINK_DONE*)
        printf '\n===== all %s transfers received — see the i65_sink_result lines above =====\n' "${TOTAL_TRANSFERS}" ;;
    esac
  done
) &
TAIL_PID=$!
CHILD_DESC+=("sink-tail"); CHILD_PID+=("${TAIL_PID}"); CHILD_SUDO+=("0")

# Wait for the sink to report I65_SINK_DONE (expected) or for the watchdog to
# flag a problem (SENTINEL). The watchdog does NOT signal us on the expected
# path, so we can finish the result report below cleanly.
while :; do
  sleep 2
  grep -q '^I65_SINK_DONE ' "${EVID}/sink.log" 2>/dev/null && break
  [ -e "${SENTINEL}" ] && break
done
sleep 1
kill "${TAIL_PID}" 2>/dev/null || true

hr
if grep -q '^I65_SINK_DONE ' "${EVID}/sink.log" 2>/dev/null; then
  log "Issue-65 lab: all transfers received. Result lines:"
  grep '"i65_sink_result":true' "${EVID}/sink.log" | tee -a "${LAUNCHER_LOG}" >&2 || true
  if [ "${NDIRS}" -gt 0 ]; then
    hr
    log "PER-DESTINATION CLEANUP + FREE SPACE (sanity check; filesystems left mounted for you to unmount)"
    leftover=0
    for d in "${DEST_DIRS[@]}"; do
      found="$(find "${d}" -maxdepth 1 -type f -name 'capture*.bin' 2>/dev/null || true)"
      if [ -n "${found}" ]; then leftover=1; log "  LEFTOVER in ${d}:"; printf '%s\n' "${found}" | sed 's/^/    /' >&2; fi
      davail="$(df -B1 --output=avail "${d}" | tail -1 | tr -d ' ')"
      log "  ${d}: capture payloads present=$([ -n "${found}" ] && echo YES || echo none)  free=${davail} B"
    done
    if [ "${leftover}" -eq 0 ]; then
      log "  OK: no leftover Issue-65 capture payloads on any destination"
    else
      log "  WARNING: leftover capture payloads found — remove them manually"
    fi
  fi
  exit 0
fi
log "Issue-65 lab supervise ended without I65_SINK_DONE — inspect ${EVID}/sink.log"
exit 20
