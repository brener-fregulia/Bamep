#!/usr/bin/env bash
#
# Bamep Issue #65 — derive a THROWAWAY Issue-65 PXE/WinPE runtime from the
# preserved Issue #53 Phase-9d assets WITHOUT modifying them.
#
# =====================================================================
# NOT APPLIANCE / PRODUCTION BOOT CONFIGURATION.
# It composes ALREADY-PROVEN Phase-9d assets (identical lineage to the Issue-63
# Stage-1 derive) and adds only:
#   * a derived autoexec.ipxe with three extra `initrd` lines that make wimboot
#     overlay winpeshl.ini + a bootstrap .cmd + the Issue-65 capture probe .exe
#     into X:\Windows\System32 of the booted WinPE (documented wimboot
#     behaviour; the Phase-9d boot.wim itself is never touched);
#   * a derived dnsmasq.conf pointing at the Issue-65 TFTP root.
# =====================================================================
#
# Issue #65 carries NO credential, key, token or transfer authorization. This
# script fails closed if any secret-shaped material appears in the derived tree.
#
#   bash -n derive-issue65-runtime.sh
#   ./derive-issue65-runtime.sh --out DIR --probe-exe PATH [options]
#
set -euo pipefail
export LC_ALL=C

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"

# Pinned Phase-9d asset hashes (identical to the Issue-63 Stage-1 derive; the
# lineage is unchanged).
declare -A PIN=(
  [http/wimboot]=5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3
  [http/BCD]=c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41
  [http/boot.sdi]=cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd
  [http/boot.wim]=fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197
  [tftp/ipxeboot/x86_64-sb/snponly-shim.efi]=83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885
  [tftp/ipxeboot/x86_64-sb/snponly.efi]=b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
  [tftp/ipxe.efi]=b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
)

OUT=""
PROBE_EXE=""
LAB_IP="192.168.99.1"
HTTP_PORT="8080"
SINK_PORT="9265"
EXTENT_BYTES="2147483648"
CONNECT_WAIT_SECS="60"
TOTAL_TRANSFERS="3"
PROBE_EXTRA_ARGS=""
CONTROL="capture"          # capture (default) | http (WinPE plain-HTTP throughput control)
RUN_ID="issue65"
IFACE="enp8s0"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "derive-issue65: FATAL: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --probe-exe) PROBE_EXE="$2"; shift 2 ;;
    --lab-ip) LAB_IP="$2"; shift 2 ;;
    --http-port) HTTP_PORT="$2"; shift 2 ;;
    --sink-port) SINK_PORT="$2"; shift 2 ;;
    --extent-bytes) EXTENT_BYTES="$2"; shift 2 ;;
    --connect-wait-secs) CONNECT_WAIT_SECS="$2"; shift 2 ;;
    --total-transfers) TOTAL_TRANSFERS="$2"; shift 2 ;;
    --probe-extra-args) PROBE_EXTRA_ARGS="$2"; shift 2 ;;
    --control) CONTROL="$2"; shift 2 ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --iface) IFACE="$2"; shift 2 ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "${OUT}" ] || die "--out DIR is required"
[ -n "${PROBE_EXE}" ] || die "--probe-exe PATH is required"
[ -f "${PROBE_EXE}" ] || die "probe exe not found: ${PROBE_EXE}"
[ ! -L "${PROBE_EXE}" ] || die "probe exe must not be a symlink: ${PROBE_EXE}"
[ "$(head -c2 "${PROBE_EXE}")" = "MZ" ] || die "probe exe is not a PE/COFF image (no MZ magic): ${PROBE_EXE}"
[ -d "${PHASE9D_DIR}" ] || die "Phase-9d runtime not found at ${PHASE9D_DIR}"
[ -e "${OUT}" ] && die "refusing to overwrite existing ${OUT} — pass a fresh --out path"
case "${TOTAL_TRANSFERS}" in ''|*[!0-9]*) die "--total-transfers must be a positive integer" ;; esac
[ "${TOTAL_TRANSFERS}" -ge 1 ] || die "--total-transfers must be >= 1"
# The extra-args string is injected verbatim into the WinPE bootstrap probe
# invocation. Allowlist every space-separated token (flags only, no values, no
# shell metacharacters) so a combination like "--no-digest --synthetic-source"
# is accepted while anything else fails closed.
for _tok in ${PROBE_EXTRA_ARGS}; do
  case "${_tok}" in
    --no-digest|--digest|--synthetic-source) : ;;
    *) die "--probe-extra-args token not allowed: '${_tok}' (allowed: --no-digest --digest --synthetic-source)" ;;
  esac
done

# The control selects which throwaway WinPE probe + bootstrap gets injected. The
# bootstrap output filename stays bamep-i65-bootstrap.cmd for both (winpeshl.ini
# is reused verbatim); only the probe .exe basename and the template differ.
case "${CONTROL}" in
  capture)
    PROBE_BASENAME="bamep-i65-capture-probe.exe"
    BOOTSTRAP_TEMPLATE="${SCRIPT_DIR}/bamep-i65-bootstrap.cmd.template" ;;
  http)
    PROBE_BASENAME="bamep-i65-http-probe.exe"
    BOOTSTRAP_TEMPLATE="${SCRIPT_DIR}/bamep-i65-http-bootstrap.cmd.template" ;;
  *) die "--control must be 'capture' or 'http' (got: ${CONTROL})" ;;
esac
[ -f "${BOOTSTRAP_TEMPLATE}" ] || die "bootstrap template not found: ${BOOTSTRAP_TEMPLATE}"

hash_of() { sha256sum "$1" | awk '{print $1}'; }

# 1. verify every Phase-9d source asset against the pinned hash (BEFORE)
BEFORE="$(mktemp)"
for rel in "${!PIN[@]}"; do
  src="${PHASE9D_DIR}/${rel}"
  [ -f "${src}" ] || die "pinned Phase-9d asset missing: ${src}"
  [ ! -L "${src}" ] || die "Phase-9d asset is unexpectedly a symlink: ${src}"
  got="$(hash_of "${src}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "Phase-9d asset ${rel} hash ${got} != pinned ${PIN[$rel]}"
  printf '%s  %s\n' "${got}" "${rel}"
done | sort > "${BEFORE}"
echo "derive-issue65: Phase-9d source assets verified against pinned hashes (7 files)"

# 2. build the derived tree
mkdir -p "${OUT}/tftp/ipxeboot/x86_64-sb" "${OUT}/http"

install -m 0644 "${PHASE9D_DIR}/tftp/ipxe.efi"                            "${OUT}/tftp/ipxe.efi"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi" "${OUT}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly.efi"      "${OUT}/tftp/ipxeboot/x86_64-sb/snponly.efi"

ln -s "${PHASE9D_DIR}/http/wimboot"  "${OUT}/http/wimboot"
ln -s "${PHASE9D_DIR}/http/BCD"      "${OUT}/http/BCD"
ln -s "${PHASE9D_DIR}/http/boot.sdi" "${OUT}/http/boot.sdi"
ln -s "${PHASE9D_DIR}/http/boot.wim" "${OUT}/http/boot.wim"

cat > "${OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe" <<EOF
#!ipxe
echo Bamep Issue 65 throughput-first capture (derived from Phase 9d - boot.wim/BCD/boot.sdi unmodified)
show efi/SecureBoot
kernel http://${LAB_IP}:${HTTP_PORT}/wimboot pause
initrd http://${LAB_IP}:${HTTP_PORT}/BCD BCD
initrd http://${LAB_IP}:${HTTP_PORT}/boot.sdi boot.sdi
initrd http://${LAB_IP}:${HTTP_PORT}/winpeshl.ini winpeshl.ini
initrd http://${LAB_IP}:${HTTP_PORT}/bamep-i65-bootstrap.cmd bamep-i65-bootstrap.cmd
initrd http://${LAB_IP}:${HTTP_PORT}/${PROBE_BASENAME} ${PROBE_BASENAME}
initrd http://${LAB_IP}:${HTTP_PORT}/boot.wim boot.wim
imgstat
boot
EOF

install -m 0644 "${SCRIPT_DIR}/winpeshl.ini" "${OUT}/http/winpeshl.ini"
sed -e "s|@LAB_IP@|${LAB_IP}|g" \
    -e "s|@SINK_PORT@|${SINK_PORT}|g" \
    -e "s|@EXTENT_BYTES@|${EXTENT_BYTES}|g" \
    -e "s|@CONNECT_WAIT_SECS@|${CONNECT_WAIT_SECS}|g" \
    -e "s|@TOTAL_TRANSFERS@|${TOTAL_TRANSFERS}|g" \
    -e "s| \{0,1\}@PROBE_EXTRA_ARGS@|${PROBE_EXTRA_ARGS:+ ${PROBE_EXTRA_ARGS}}|g" \
    "${BOOTSTRAP_TEMPLATE}" > "${OUT}/http/bamep-i65-bootstrap.cmd"
chmod 0644 "${OUT}/http/bamep-i65-bootstrap.cmd"
grep -qE '@[A-Z_]+@' "${OUT}/http/bamep-i65-bootstrap.cmd" && die "unsubstituted @TOKEN@ left in bamep-i65-bootstrap.cmd"
grep -qF "for /L %%i in (1,1,${TOTAL_TRANSFERS}) do" "${OUT}/http/bamep-i65-bootstrap.cmd" || die "bootstrap.cmd missing the ${TOTAL_TRANSFERS}-transfer loop"
case "${CONTROL}" in
  capture) grep -qF -- '--sink %SINK% --label t%%i --extent-bytes' "${OUT}/http/bamep-i65-bootstrap.cmd" || die "bootstrap.cmd missing the capture probe invocation" ;;
  http)    grep -qF -- '--host %HOST% --port %PORT% --path /i65-http-control --label t%%i' "${OUT}/http/bamep-i65-bootstrap.cmd" || die "bootstrap.cmd missing the http probe invocation" ;;
esac
grep -qF 'wpeinit' "${OUT}/http/bamep-i65-bootstrap.cmd" || die "bootstrap.cmd does not run wpeinit"
install -m 0644 "${PROBE_EXE}" "${OUT}/http/${PROBE_BASENAME}"

cat > "${OUT}/dnsmasq.conf" <<EOF
# Bamep Issue #65 - THROWAWAY lab harness. NOT production configuration.
# Derived from the Phase 9d dnsmasq.conf; only the tftp-root / log paths differ.
interface=${IFACE}
bind-interfaces
port=0
dhcp-authoritative
dhcp-range=192.168.99.50,192.168.99.100,255.255.255.0,1h
dhcp-option=option:router
log-dhcp
log-queries
log-facility=${OUT}/dnsmasq.log
dhcp-leasefile=${OUT}/dnsmasq.leases

enable-tftp=${IFACE}
tftp-root=${OUT}/tftp
tftp-no-fail

dhcp-match=set:efi-x64,option:client-arch,7
dhcp-boot=tag:efi-x64,ipxeboot/x86_64-sb/snponly-shim.efi
EOF

# 3. re-verify Phase-9d originals (AFTER)
AFTER="$(mktemp)"
for rel in "${!PIN[@]}"; do
  got="$(hash_of "${PHASE9D_DIR}/${rel}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "Phase-9d asset ${rel} CHANGED during derive (${got} != pinned)"
  printf '%s  %s\n' "${got}" "${rel}"
done | sort > "${AFTER}"
if ! diff -q "${BEFORE}" "${AFTER}" >/dev/null; then
  diff "${BEFORE}" "${AFTER}" >&2 || true
  die "Phase-9d before/after hashes differ"
fi
cp "${BEFORE}" "${OUT}/phase9d-hashes-before.txt"
cp "${AFTER}"  "${OUT}/phase9d-hashes-after.txt"
rm -f "${BEFORE}" "${AFTER}"
echo "derive-issue65: Phase-9d originals byte-identical before and after (7/7)"

# 4. verify the SERVED bytes == pinned
for rel in "${!PIN[@]}"; do
  base="${rel##*/}"
  case "${rel}" in
    http/*) served="${OUT}/http/${base}" ;;
    tftp/ipxe.efi) served="${OUT}/tftp/ipxe.efi" ;;
    tftp/*) served="${OUT}/tftp/ipxeboot/x86_64-sb/${base}" ;;
  esac
  got="$(hash_of "${served}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "derived served copy ${served} hash ${got} != pinned"
done
echo "derive-issue65: derived tree serves the exact pinned Phase-9d bytes (7/7)"

# 5. secret + forbidden-file sweep over the derived tree
if grep -RIlE -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' \
              -e '(password|passwd|secret|bearer|api[_-]?key|credential)[[:space:]]*[:=]' \
              -e 'Authorization:[[:space:]]*Bearer' \
              "${OUT}/http" "${OUT}/tftp" 2>/dev/null | grep -q .; then
  die "secret-shaped content found in the derived boot tree"
fi
if find "${OUT}" -type f \( -name '*.cred' -o -name '*.pem' -o -name '*.key' \
     -o -name '*.pkcs8' -o -name '*.der' -o -name '*.p12' -o -name '*.pfx' \) | grep -q .; then
  die "credential/key file present in the derived boot tree"
fi
echo "derive-issue65: no secret / key / credential material in the derived tree"

# 6. structural checks on the derived autoexec
AX="${OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
grep -qxF 'initrd http://'"${LAB_IP}:${HTTP_PORT}"'/winpeshl.ini winpeshl.ini' "${AX}" || die "autoexec missing winpeshl.ini injection"
grep -qxF 'initrd http://'"${LAB_IP}:${HTTP_PORT}"'/bamep-i65-bootstrap.cmd bamep-i65-bootstrap.cmd' "${AX}" || die "autoexec missing bootstrap.cmd injection"
grep -qxF 'initrd http://'"${LAB_IP}:${HTTP_PORT}"'/'"${PROBE_BASENAME}"' '"${PROBE_BASENAME}" "${AX}" || die "autoexec missing probe.exe injection"
[ "$(grep -c '^initrd .*/boot.wim boot.wim$' "${AX}")" = "1" ] || die "autoexec must contain exactly one boot.wim initrd"
[ "$(grep '^initrd ' "${AX}" | tail -1)" = "initrd http://${LAB_IP}:${HTTP_PORT}/boot.wim boot.wim" ] || die "boot.wim initrd must be the LAST initrd line"
STRAY="$(grep -n '#' "${AX}" | grep -v '^1:#!ipxe$' || true)"
[ -z "${STRAY}" ] || die "stray '#' in autoexec: ${STRAY}"
grep -qi '^prompt' "${AX}" && die "autoexec must contain no iPXE 'prompt' command"
echo "derive-issue65: derived autoexec.ipxe structurally OK (3 injections, boot.wim last, one keypress = wimboot pause)"

# 7. manifest
{
  echo "# Bamep Issue #65 derived-runtime manifest ($(date -Is))"
  echo "# run_id=${RUN_ID} lab_ip=${LAB_IP} http=${HTTP_PORT} sink=${SINK_PORT}"
  echo "# control=${CONTROL} probe=${PROBE_BASENAME}"
  echo "# extent_bytes=${EXTENT_BYTES} connect_wait_secs=${CONNECT_WAIT_SECS} total_transfers=${TOTAL_TRANSFERS} iface=${IFACE}"
  echo "# probe_extra_args=${PROBE_EXTRA_ARGS:-<none>}"
  echo
  echo "## authored / substituted / copied (independent bytes):"
  sha256sum "${AX}" \
            "${OUT}/http/winpeshl.ini" \
            "${OUT}/http/bamep-i65-bootstrap.cmd" \
            "${OUT}/http/${PROBE_BASENAME}" \
            "${OUT}/tftp/ipxe.efi" \
            "${OUT}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi" \
            "${OUT}/tftp/ipxeboot/x86_64-sb/snponly.efi" \
            "${OUT}/dnsmasq.conf" | sed "s|${OUT}/||"
  echo
  echo "## HTTP boot assets served via symlink -> Phase-9d original:"
  for f in wimboot BCD boot.sdi boot.wim; do
    printf '%s -> %s\n' "http/${f}" "$(readlink "${OUT}/http/${f}")"
  done
} > "${OUT}/derived-manifest.txt"

echo "DERIVE_OK out=${OUT}"
