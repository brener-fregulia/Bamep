#!/usr/bin/env bash
#
# Bamep Issue #65 — WinPE plain-HTTP throughput CONTROL lab launcher (LAB-ONLY).
#
# =====================================================================
# NOT APPLIANCE ARCHITECTURE. NOT PRODUCTION SERVICE MANAGEMENT.
# NOT THE Bamep FIREWALL / NETWORK / BOOT / DATA-PLANE DESIGN.
# THROWAWAY control inside the Issue #65 Spike. The Bamep capture path is
# NOT touched by this run.
# =====================================================================
#
# Question: can stock WinPE + its NIC/driver + the physical 1 GbE link sustain
# >= 110 MB/s with a minimal native WinHTTP POST (one 32 MiB buffer, sent 64
# times = 2 GiB, discarded on the Fedora side)? No Bamep source resolver, no
# disk on either side, no producer/consumer queue, no SHA, no TLS, no auth, no
# DB, no Bamep protocol framing.
#
# It reuses the EXISTING Issue-65 PXE/WinPE boot plumbing:
#   * Issue #53 Phase 9d PXE/WinPE runtime  /var/tmp/bamep-issue53-phase9d-winpe-completion
#       (consumed READ-ONLY; re-hashed before and after; never modified)
#   * derive-issue65-runtime.sh --control http  (injects the HTTP probe +
#       bootstrap via wimboot's boot-time overlay; boot.wim is never touched)
#   * winpeshl.ini                              (reused verbatim)
#
# It brings up: dnsmasq (DHCP+TFTP), a python HTTP server for the WinPE boot
# assets, and the plain-HTTP throughput sink. It NEVER powers the MiniPC and
# NEVER triggers the PXE boot.
#
#   bash -n run-issue65-http-control.sh
#   ./run-issue65-http-control.sh --preflight     (read-only gate; no services)
#   ./run-issue65-http-control.sh                  (bring the lab up and supervise it)
#
# Env overrides: I65_LAB_IFACE (enp8s0), I65_LAB_IP (192.168.99.1),
#   I65_FW_ZONE (trusted), I65_CONTENT_LENGTH (2147483648), I65_TRANSFERS (3 =
#   1 warmup + 2 measured), I65_CONNECT_WAIT_SECS (60).
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

CONTENT_LENGTH="${I65_CONTENT_LENGTH:-2147483648}"
TRANSFERS="${I65_TRANSFERS:-3}"          # 1 warmup + 2 measured
CONNECT_WAIT_SECS="${I65_CONNECT_WAIT_SECS:-60}"
TOTAL_TRANSFERS="${TRANSFERS}"

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"
SINK_BIN="${I65_DIR}/http-sink/target/release/bamep-i65-http-sink"
PROBE_EXE="${I65_DIR}/http-probe/target/x86_64-pc-windows-msvc/release/bamep-i65-http-probe.exe"
DERIVE="${I65_DIR}/derive-issue65-runtime.sh"

PINNED_HTTP_WIMBOOT="5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3"
PINNED_HTTP_BCD="c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41"
PINNED_HTTP_BOOTSDI="cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd"
PINNED_HTTP_BOOTWIM="fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197"
PINNED_TFTP_SHIM="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
PINNED_TFTP_SNPONLY="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
PINNED_TFTP_IPXE="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"

# Stock-WinPE-proven DLL set. Base: #60/#61 evidence. winhttp.dll + iphlpapi.dll
# were additionally verified present in this exact boot.wim (index 1, Windows PE
# amd64 build 26100): both /Windows/System32/winhttp.dll and
# /Windows/System32/iphlpapi.dll exist, and the full WinHTTP dependency closure
# (webio, winnsi, nsi, mswsock, ws2_32, dnsapi, rpcrt4, kernelbase) is present.
PROVEN_DLLS="api-ms-win-core-synch-l1-2-0.dll kernel32.dll ntdll.dll ws2_32.dll advapi32.dll bcrypt.dll bcryptprimitives.dll winhttp.dll iphlpapi.dll"

RUN_ID="i65http-$(date +%Y%m%dT%H%M%S)"
EVID="${I65_DIR}/evidence/${RUN_ID}"
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

usage() { sed -n '3,44p' "${SCRIPT_PATH}" | sed 's/^#\{1,\} \{0,1\}//;s/^#$//'; }

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
log "Bamep Issue #65 WinPE plain-HTTP throughput CONTROL launcher — ${RUN_ID}"
log "MODE=${MODE}   (LAB-ONLY control; the Bamep capture path is NOT touched)"
hr

log "[preflight] repository + build artifacts"
require_file "${REPO_ROOT}/AGENTS.md" "repo root sanity"
require_file "${DERIVE}" "derive-issue65-runtime.sh"
require_file "${SINK_BIN}" "build: cd ${I65_DIR}/http-sink && cargo build --release"
require_file "${PROBE_EXE}" "cross-build: cd ${I65_DIR}/http-probe && RUSTFLAGS='-C target-feature=+crt-static' cargo xwin build --release --target x86_64-pc-windows-msvc"
[ "$(head -c2 "${PROBE_EXE}")" = "MZ" ] || die "probe exe is not a PE image: ${PROBE_EXE}"
log "  ok: http sink + http probe exe present"

log "[preflight] http probe PE imports are a subset of the stock-WinPE-proven set (incl. winhttp.dll, iphlpapi.dll)"
if command -v objdump >/dev/null 2>&1; then
  probe_dlls_ok "${PROBE_EXE}" || die "http probe imports a DLL outside the proven set — STOP and report"
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

log "[preflight] scratch for the derived runtime (no destination file — the sink discards)"
mkdir -p "${I65_DIR}/evidence"
FREE_BYTES="$(df -B1 --output=avail "${I65_DIR}/evidence" | tail -1 | tr -d ' ')"
[ "${FREE_BYTES}" -ge 1073741824 ] || die "want >= 1 GiB free under ${I65_DIR}/evidence (have ${FREE_BYTES})"
log "  ok: ${FREE_BYTES} bytes free"

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
  echo "run_id          ${RUN_ID}"
  echo "started_at      $(date -Is)"
  echo "repo_head       $(cat "${EVID}/repo-head.txt")"
  echo "control         http  (WinPE WinHTTP POST -> Fedora HTTP sink -> discard)"
  echo "lab_iface       ${LAB_IFACE}"
  echo "lab_ip          ${LAB_IP}"
  echo "ports           winpe_http=${PORT_HTTP} http_sink=${PORT_SINK}"
  echo "content_length  ${CONTENT_LENGTH}  (32 MiB buffer x 64 writes)"
  echo "transfers       ${TRANSFERS} (1 warmup + $((TRANSFERS - 1)) measured)"
  echo "destination     none — POST body is drained and DISCARDED (no disk)"
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
  log "Issue-65 HTTP-control lab down."
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
# 4. derive the Issue-65 runtime (control=http)
# ---------------------------------------------------------------------
hr
log "DERIVING the Issue-65 PXE/WinPE runtime — control=http (Phase-9d assets consumed READ-ONLY)"
DERIVE_OUT="${EVID}/derived-runtime"
if ! "${DERIVE}" --out "${DERIVE_OUT}" --probe-exe "${PROBE_EXE}" --control http \
      --lab-ip "${LAB_IP}" --http-port "${PORT_HTTP}" --sink-port "${PORT_SINK}" \
      --extent-bytes "${CONTENT_LENGTH}" --connect-wait-secs "${CONNECT_WAIT_SECS}" \
      --total-transfers "${TOTAL_TRANSFERS}" \
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

log "[http] python3 http.server on ${LAB_IP}:${PORT_HTTP} serving ${DERIVE_OUT}/http (WinPE boot assets)"
python3 -m http.server --bind "${LAB_IP}" --directory "${DERIVE_OUT}/http" "${PORT_HTTP}" > "${EVID}/http.log" 2>&1 &
register_child "winpe-http" "$!" 0
await "http tcp/${PORT_HTTP}" 30 tcp_up "${PORT_HTTP}" || die "HTTP server did not start — see ${EVID}/http.log"

log "[sink] bamep-i65-http-sink on ${LAB_IP}:${PORT_SINK} (count=${TRANSFERS}, content-length=${CONTENT_LENGTH}, body DISCARDED)"
"${SINK_BIN}" --listen "${LAB_IP}:${PORT_SINK}" --count "${TRANSFERS}" --content-length "${CONTENT_LENGTH}" > "${EVID}/sink.log" 2>&1 &
register_child "http-sink" "$!" 0
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
gate "tcp/${PORT_SINK} (HTTP sink)"          tcp_up "${PORT_SINK}"
gate "Phase-9d assets still match pinned"    check_pinned
gate "probe PE imports within proven set"    probe_dlls_ok "${PROBE_EXE}"

for asset_want in "wimboot|${PINNED_HTTP_WIMBOOT}" "BCD|${PINNED_HTTP_BCD}" "boot.sdi|${PINNED_HTTP_BOOTSDI}" "boot.wim|${PINNED_HTTP_BOOTWIM}"; do
  a="${asset_want%%|*}"; w="${asset_want##*|}"
  got="$(curl -sf "http://${LAB_IP}:${PORT_HTTP}/${a}" | sha256sum | awk '{print $1}')" || got="ERR"
  gate "HTTP /${a} serves pinned bytes" bash -c "[ '${got}' = '${w}' ]"
done
for f in winpeshl.ini bamep-i65-bootstrap.cmd bamep-i65-http-probe.exe; do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://${LAB_IP}:${PORT_HTTP}/${f}")"
  gate "HTTP /${f} -> 200" bash -c "[ '${code}' = '200' ]"
done
AX="${DERIVE_OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
gate "autoexec injects winpeshl.ini"        grep -qF "/winpeshl.ini winpeshl.ini" "${AX}"
gate "autoexec injects bootstrap.cmd"       grep -qF "/bamep-i65-bootstrap.cmd bamep-i65-bootstrap.cmd" "${AX}"
gate "autoexec injects http probe.exe"      grep -qF "/bamep-i65-http-probe.exe bamep-i65-http-probe.exe" "${AX}"
gate "autoexec: boot.wim initrd is last"    bash -c "[ \"\$(grep '^initrd ' '${AX}' | tail -1)\" = \"initrd http://${LAB_IP}:${PORT_HTTP}/boot.wim boot.wim\" ]"
gate "bootstrap.cmd runs the ${TOTAL_TRANSFERS}-POST loop" bash -c "grep -qF 'for /L %%i in (1,1,${TOTAL_TRANSFERS}) do' '${DERIVE_OUT}/http/bamep-i65-bootstrap.cmd' && grep -qF -- '--host %HOST% --port %PORT% --path /i65-http-control --label t%%i' '${DERIVE_OUT}/http/bamep-i65-bootstrap.cmd'"
gate "sink schedule = ${TOTAL_TRANSFERS} connections" bash -c "grep -q 'total_connections=${TOTAL_TRANSFERS}$' '${EVID}/sink.log'"

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
        if [ "${names[$k]}" = "http-sink" ] && grep -q '^I65_SINK_DONE ' "${EVID}/sink.log" 2>/dev/null; then
          log ""
          log ">>> http-sink received all ${TOTAL_TRANSFERS} POSTs and exited (expected)"
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
Control      : WinPE plain-HTTP throughput (Bamep capture path NOT touched)
DHCP/TFTP    : READY   (dnsmasq, derived Issue-65 conf, ${LAB_IFACE})
WinPE HTTP   : READY   (${LAB_IP}:${PORT_HTTP}  ${DERIVE_OUT}/http)
HTTP sink    : READY   (${LAB_IP}:${PORT_SINK}  count=${TRANSFERS}  content-length=${CONTENT_LENGTH} B  body DISCARDED)
Phase-9d     : byte-identical before/after derive (7/7 pinned)
Probe imports: $(probe_dlls "${PROBE_EXE}" | tr '\n' ' ') (subset of stock-WinPE-proven set; winhttp.dll + iphlpapi.dll verified present in boot.wim)

Transfer     : WinHTTP POST, Content-Length ${CONTENT_LENGTH}, ONE 32 MiB buffer
               (allocated + filled once) sent 64 times over ONE WinHTTP session.
               NO disk on either side. NO SHA, NO TLS, NO auth, NO DB.

Owner action (ONLY this):
  1. Power ON the disposable MiniPC (it will DHCP-lease in 192.168.99.50-100).
  2. Press a key ONCE at wimboot's "Press any key to continue booting..." prompt.
  3. Type NOTHING in WinPE. The injected winpeshl.ini auto-runs:
       wpeinit -> ${TOTAL_TRANSFERS} back-to-back WinHTTP POSTs (t1..t${TOTAL_TRANSFERS}).
       transfer 1 = warmup, transfers 2-${TRANSFERS} = measured.
  Watch THIS terminal for the ${TOTAL_TRANSFERS} 'i65_sink_result' lines.
  The WinPE console also prints 'I65_HTTP_CLIENT_RESULT' per POST (link speed,
  TCP retransmit delta, HTTP status) — those are visible on the MiniPC screen
  only and are not captured here.

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
        printf '\n########## Issue-65 HTTP-control result ##########\n%s\n#################################################\n' "${line}" ;;
      I65_SINK_DONE*)
        printf '\n===== all %s POSTs received — see the i65_sink_result lines above =====\n' "${TOTAL_TRANSFERS}" ;;
    esac
  done
) &
TAIL_PID=$!
CHILD_DESC+=("sink-tail"); CHILD_PID+=("${TAIL_PID}"); CHILD_SUDO+=("0")

while :; do
  sleep 2
  grep -q '^I65_SINK_DONE ' "${EVID}/sink.log" 2>/dev/null && break
  [ -e "${SENTINEL}" ] && break
done
sleep 1
kill "${TAIL_PID}" 2>/dev/null || true

hr
if grep -q '^I65_SINK_DONE ' "${EVID}/sink.log" 2>/dev/null; then
  log "Issue-65 HTTP control: all POSTs received. Result lines:"
  grep '"i65_sink_result":true' "${EVID}/sink.log" | tee -a "${LAUNCHER_LOG}" >&2 || true
  exit 0
fi
log "Issue-65 HTTP control supervise ended without I65_SINK_DONE — inspect ${EVID}/sink.log"
exit 20
