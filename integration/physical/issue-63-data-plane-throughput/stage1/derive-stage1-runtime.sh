#!/usr/bin/env bash
#
# Bamep Issue #63 Stage 1 — derive a THROWAWAY Issue-63 PXE/WinPE runtime from
# the preserved Issue #53 Phase-9d assets WITHOUT modifying them.
#
# =====================================================================
# THIS IS NOT APPLIANCE / PRODUCTION BOOT CONFIGURATION.
# It composes ALREADY-PROVEN Phase-9d assets and adds only:
#   * a derived autoexec.ipxe with three extra `initrd` lines that make wimboot
#     overlay winpeshl.ini + a bootstrap .cmd + the Stage-1 runner .exe into
#     X:\Windows\System32 of the booted WinPE (documented wimboot behaviour;
#     the Phase-9d boot.wim itself is never touched);
#   * a derived dnsmasq.conf pointing at the Issue-63 TFTP root.
# =====================================================================
#
# The Phase-9d wimboot / BCD / boot.sdi / boot.wim are HTTP-served through
# symlinks (a symlink cannot mutate its target); the small TFTP files are
# copied. Every served byte is hash-checked against the pinned Phase-9d values,
# and the Phase-9d originals are re-hashed before AND after this script runs.
#
# Stage 1 has NO credential, key, token or transfer. This script fails closed if
# any secret-shaped material appears in the derived tree.
#
#   bash -n derive-stage1-runtime.sh          # syntax
#   ./derive-stage1-runtime.sh --out DIR --runner-exe PATH [options]
#
set -euo pipefail
export LC_ALL=C

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"

# Pinned Phase-9d asset hashes (from ${PHASE9D_DIR}/sha256sums-{http,tftp}.txt,
# themselves pinned by issue53-phase9d-setup-run.sh).
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
RUNNER_EXE=""
LAB_IP="192.168.99.1"
HTTP_PORT="8080"
COORD_PORT="9206"
SINK_PORT="9299"
SKEW_FLOOR_MS="-2000"
SKEW_CEIL_MS="2000"
NET_WAIT_SECS="120"
RUN_ID="stage1"
IFACE="enp8s0"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "derive-stage1: FATAL: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --runner-exe) RUNNER_EXE="$2"; shift 2 ;;
    --lab-ip) LAB_IP="$2"; shift 2 ;;
    --http-port) HTTP_PORT="$2"; shift 2 ;;
    --coord-port) COORD_PORT="$2"; shift 2 ;;
    --sink-port) SINK_PORT="$2"; shift 2 ;;
    --skew-floor-ms) SKEW_FLOOR_MS="$2"; shift 2 ;;
    --skew-ceil-ms) SKEW_CEIL_MS="$2"; shift 2 ;;
    --net-wait-secs) NET_WAIT_SECS="$2"; shift 2 ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --iface) IFACE="$2"; shift 2 ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "${OUT}" ] || die "--out DIR is required"
[ -n "${RUNNER_EXE}" ] || die "--runner-exe PATH is required"
[ -f "${RUNNER_EXE}" ] || die "runner exe not found: ${RUNNER_EXE}"
[ ! -L "${RUNNER_EXE}" ] || die "runner exe must not be a symlink: ${RUNNER_EXE}"
[ "$(head -c2 "${RUNNER_EXE}")" = "MZ" ] || die "runner exe is not a PE/COFF image (no MZ magic): ${RUNNER_EXE}"
[ -d "${PHASE9D_DIR}" ] || die "Phase-9d runtime not found at ${PHASE9D_DIR} (run issue53-phase9d-setup first)"
[ -e "${OUT}" ] && die "refusing to overwrite existing ${OUT} — pass a fresh --out path"

hash_of() { sha256sum "$1" | awk '{print $1}'; }

# ---------------------------------------------------------------------
# 1. verify every Phase-9d source asset against the pinned hash (BEFORE)
# ---------------------------------------------------------------------
BEFORE="$(mktemp)"
for rel in "${!PIN[@]}"; do
  src="${PHASE9D_DIR}/${rel}"
  [ -f "${src}" ] || die "pinned Phase-9d asset missing: ${src}"
  [ ! -L "${src}" ] || die "Phase-9d asset is unexpectedly a symlink: ${src}"
  got="$(hash_of "${src}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "Phase-9d asset ${rel} hash ${got} != pinned ${PIN[$rel]} — refusing to derive"
  printf '%s  %s\n' "${got}" "${rel}"
done | sort > "${BEFORE}"
echo "derive-stage1: Phase-9d source assets verified against pinned hashes (7 files)"

# ---------------------------------------------------------------------
# 2. build the derived tree
# ---------------------------------------------------------------------
mkdir -p "${OUT}/tftp/ipxeboot/x86_64-sb" "${OUT}/http"

# 2a. TFTP — copy the small files (removes any dnsmasq symlink question)
install -m 0644 "${PHASE9D_DIR}/tftp/ipxe.efi"                          "${OUT}/tftp/ipxe.efi"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi" "${OUT}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly.efi"      "${OUT}/tftp/ipxeboot/x86_64-sb/snponly.efi"

# 2b. HTTP — symlink the four large boot assets (a symlink cannot mutate its
#     target; python http.server follows symlinks; bytes are re-checked below)
ln -s "${PHASE9D_DIR}/http/wimboot"  "${OUT}/http/wimboot"
ln -s "${PHASE9D_DIR}/http/BCD"      "${OUT}/http/BCD"
ln -s "${PHASE9D_DIR}/http/boot.sdi" "${OUT}/http/boot.sdi"
ln -s "${PHASE9D_DIR}/http/boot.wim" "${OUT}/http/boot.wim"

# 2c. derived autoexec.ipxe — Phase-9d chain + three injected-file initrd lines.
#     wimboot pause is KEPT (that IS the one intentional operator keypress).
#     No '#' beyond the mandatory magic; no iPXE 'prompt'.
cat > "${OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe" <<EOF
#!ipxe
echo Bamep Issue 63 Stage 1 (derived from Phase 9d - boot.wim/BCD/boot.sdi unmodified)
show efi/SecureBoot
kernel http://${LAB_IP}:${HTTP_PORT}/wimboot pause
initrd http://${LAB_IP}:${HTTP_PORT}/BCD BCD
initrd http://${LAB_IP}:${HTTP_PORT}/boot.sdi boot.sdi
initrd http://${LAB_IP}:${HTTP_PORT}/winpeshl.ini winpeshl.ini
initrd http://${LAB_IP}:${HTTP_PORT}/bamep-i63-bootstrap.cmd bamep-i63-bootstrap.cmd
initrd http://${LAB_IP}:${HTTP_PORT}/bamep-i63-runner.exe bamep-i63-runner.exe
initrd http://${LAB_IP}:${HTTP_PORT}/boot.wim boot.wim
imgstat
boot
EOF

# 2d. injected System32 payload
install -m 0644 "${SCRIPT_DIR}/winpeshl.ini" "${OUT}/http/winpeshl.ini"
sed -e "s|@RUN_ID@|${RUN_ID}|g" \
    -e "s|@LAB_IP@|${LAB_IP}|g" \
    -e "s|@COORD_PORT@|${COORD_PORT}|g" \
    -e "s|@SINK_PORT@|${SINK_PORT}|g" \
    -e "s|@SKEW_FLOOR_MS@|${SKEW_FLOOR_MS}|g" \
    -e "s|@SKEW_CEIL_MS@|${SKEW_CEIL_MS}|g" \
    -e "s|@NET_WAIT_SECS@|${NET_WAIT_SECS}|g" \
    "${SCRIPT_DIR}/bamep-i63-bootstrap.cmd.template" > "${OUT}/http/bamep-i63-bootstrap.cmd"
chmod 0644 "${OUT}/http/bamep-i63-bootstrap.cmd"
grep -qE '@[A-Z_]+@' "${OUT}/http/bamep-i63-bootstrap.cmd" && die "unsubstituted @TOKEN@ left in bamep-i63-bootstrap.cmd"
grep -qF -- '--mode physical' "${OUT}/http/bamep-i63-bootstrap.cmd" || die "bootstrap.cmd does not launch the runner with --mode physical"
grep -qF 'bootstrap_local_ts' "${OUT}/http/bamep-i63-bootstrap.cmd" || die "bootstrap.cmd does not record bootstrap_local_ts on its milestones"
grep -qF '"event":"winpe.booted"' "${OUT}/http/bamep-i63-bootstrap.cmd" || die "bootstrap.cmd does not write winpe.booted before wpeinit"
grep -qF '"event":"winpe.wpeinit_complete"' "${OUT}/http/bamep-i63-bootstrap.cmd" || die "bootstrap.cmd does not write winpe.wpeinit_complete after wpeinit"
install -m 0644 "${RUNNER_EXE}" "${OUT}/http/bamep-i63-runner.exe"

# 2e. derived dnsmasq.conf
cat > "${OUT}/dnsmasq.conf" <<EOF
# Bamep Issue #63 Stage 1 - THROWAWAY lab harness. NOT production configuration.
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

# ---------------------------------------------------------------------
# 3. re-verify Phase-9d originals (AFTER) — must be byte-identical
# ---------------------------------------------------------------------
AFTER="$(mktemp)"
for rel in "${!PIN[@]}"; do
  got="$(hash_of "${PHASE9D_DIR}/${rel}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "Phase-9d asset ${rel} CHANGED during derive (${got} != pinned) — abort"
  printf '%s  %s\n' "${got}" "${rel}"
done | sort > "${AFTER}"
if ! diff -q "${BEFORE}" "${AFTER}" >/dev/null; then
  diff "${BEFORE}" "${AFTER}" >&2 || true
  die "Phase-9d before/after hashes differ"
fi
cp "${BEFORE}" "${OUT}/phase9d-hashes-before.txt"
cp "${AFTER}"  "${OUT}/phase9d-hashes-after.txt"
rm -f "${BEFORE}" "${AFTER}"
echo "derive-stage1: Phase-9d originals byte-identical before and after (7/7)"

# ---------------------------------------------------------------------
# 4. verify the SERVED bytes (through the symlinks/copies) == pinned
# ---------------------------------------------------------------------
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
echo "derive-stage1: derived tree serves the exact pinned Phase-9d bytes (7/7)"

# ---------------------------------------------------------------------
# 5. secret sweep + forbidden-file sweep over the derived tree
# ---------------------------------------------------------------------
if grep -RIlE -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' \
              -e '(password|passwd|secret|bearer|api[_-]?key|credential)[[:space:]]*[:=]' \
              -e 'Authorization:[[:space:]]*Bearer' \
              "${OUT}/http" "${OUT}/tftp" 2>/dev/null | grep -q .; then
  grep -RIlE -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' -e '(password|secret|bearer|credential)[[:space:]]*[:=]' "${OUT}/http" "${OUT}/tftp" >&2 || true
  die "secret-shaped content found in the derived boot tree"
fi
if find "${OUT}" -type f \( -name '*.cred' -o -name '*.pem' -o -name '*.key' \
     -o -name '*.pkcs8' -o -name '*.der' -o -name '*.p12' -o -name '*.pfx' \) | grep -q .; then
  die "credential/key file present in the derived boot tree"
fi
echo "derive-stage1: no secret / key / credential material in the derived tree"

# ---------------------------------------------------------------------
# 6. structural checks on the derived autoexec
# ---------------------------------------------------------------------
AX="${OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
grep -qxF 'initrd http://'"${LAB_IP}:${HTTP_PORT}"'/winpeshl.ini winpeshl.ini' "${AX}" || die "autoexec missing winpeshl.ini injection"
grep -qxF 'initrd http://'"${LAB_IP}:${HTTP_PORT}"'/bamep-i63-bootstrap.cmd bamep-i63-bootstrap.cmd' "${AX}" || die "autoexec missing bootstrap.cmd injection"
grep -qxF 'initrd http://'"${LAB_IP}:${HTTP_PORT}"'/bamep-i63-runner.exe bamep-i63-runner.exe' "${AX}" || die "autoexec missing runner.exe injection"
[ "$(grep -c '^initrd .*/boot.wim boot.wim$' "${AX}")" = "1" ] || die "autoexec must contain exactly one boot.wim initrd"
[ "$(tail -3 "${AX}" | head -1)" = "initrd http://${LAB_IP}:${HTTP_PORT}/boot.wim boot.wim" ] || die "boot.wim initrd must be the LAST initrd line (before imgstat/boot)"
STRAY="$(grep -n '#' "${AX}" | grep -v '^1:#!ipxe$' || true)"
[ -z "${STRAY}" ] || die "stray '#' in autoexec: ${STRAY}"
grep -qi '^prompt' "${AX}" && die "autoexec must contain no iPXE 'prompt' command"
echo "derive-stage1: derived autoexec.ipxe structurally OK (3 injections, boot.wim last, one keypress = wimboot pause)"

# ---------------------------------------------------------------------
# 7. manifest of what THIS script authored / copied
# ---------------------------------------------------------------------
{
  echo "# Bamep Issue #63 Stage 1 derived-runtime manifest ($(date -Is))"
  echo "# run_id=${RUN_ID} lab_ip=${LAB_IP} http=${HTTP_PORT} coord=${COORD_PORT} sink=${SINK_PORT}"
  echo "# skew_window_ms=[${SKEW_FLOOR_MS}, ${SKEW_CEIL_MS}] net_wait_secs=${NET_WAIT_SECS} iface=${IFACE}"
  echo
  echo "## authored / substituted / copied (independent bytes):"
  sha256sum "${AX}" \
            "${OUT}/http/winpeshl.ini" \
            "${OUT}/http/bamep-i63-bootstrap.cmd" \
            "${OUT}/http/bamep-i63-runner.exe" \
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
