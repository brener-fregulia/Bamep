#!/usr/bin/env bash
#
# Bamep Issue #63 Stage 3 — derive a THROWAWAY Issue-63 PXE/WinPE runtime from
# the preserved Issue #53 Phase-9d assets WITHOUT modifying them.
#
# =====================================================================
# THIS IS NOT APPLIANCE / PRODUCTION BOOT CONFIGURATION.
# It composes ALREADY-PROVEN Phase-9d assets (identical lineage to Stage 1) and
# adds only:
#   * a derived autoexec.ipxe with FIVE extra `initrd` lines that make wimboot
#     overlay winpeshl.ini + the Stage-3 bootstrap .cmd + the Stage-1 runner .exe
#     + the Stage-2 transfer probe .exe + the single first-contact enrollment
#     credential into X:\Windows\System32 of the booted WinPE (documented wimboot
#     behaviour; the Phase-9d boot.wim itself is never touched);
#   * a derived dnsmasq.conf pointing at the Issue-63 Stage-3 TFTP root.
# =====================================================================
#
# The Phase-9d wimboot / BCD / boot.sdi / boot.wim are HTTP-served through
# symlinks; small TFTP files are copied. Every served byte is hash-checked
# against the pinned Phase-9d values, and the Phase-9d originals are re-hashed
# before AND after this script runs.
#
# Unlike Stage 1, Stage 3 DOES carry ONE credential: the single first-contact
# enrollment credential minted by the Stage-3 harness for this run. It is passed
# in with --enroll-cred, injected as `bamep-i63-enroll.cred` (mode preserved),
# and is the ONLY secret-shaped file permitted in the derived tree. It is
# single-use, short-TTL, for a throwaway spike endpoint, delivered over the
# isolated 192.168.99.0/24 lab link only. Everything else fails the secret sweep.
#
#   bash -n derive-stage3-runtime.sh
#   ./derive-stage3-runtime.sh --out DIR --runner-exe P --probe-exe P --enroll-cred P --pin HEX [options]
#
set -euo pipefail
export LC_ALL=C

PHASE9D_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"

declare -A PIN=(
  [http/wimboot]=5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3
  [http/BCD]=c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41
  [http/boot.sdi]=cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd
  [http/boot.wim]=fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197
  [tftp/ipxeboot/x86_64-sb/snponly-shim.efi]=83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885
  [tftp/ipxeboot/x86_64-sb/snponly.efi]=b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
  [tftp/ipxe.efi]=b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
)

OUT=""; RUNNER_EXE=""; PROBE_EXE=""; ENROLL_CRED=""; PIN_HEX=""
LAB_IP="192.168.99.1"
HTTP_PORT="8080"; MATRIX_PORT="9210"; COORD_PORT="9206"; WSS_PORT="8443"; SINK_PORT="9299"
SKEW_FLOOR_MS="-2000"; SKEW_CEIL_MS="2000"; NET_WAIT_SECS="180"; SEAL_TIMEOUT_SECS="300"
MODEL_SUBSTR="256GB"
RUN_ID="i63s3"; IFACE="enp8s0"
# --stage 3 => the 36-case chunk-size matrix (runner `--matrix --arm`).
# --stage 4 => the Issue #63 Stage-4 64 MiB serial-vs-prep-ahead micro-matrix
#              (runner `--stage4 --arm`). ONLY the injected bootstrap .cmd and
#              the runner arm token differ; the Phase-9d lineage is identical.
STAGE="3"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
die() { echo "derive-stage3: FATAL: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --runner-exe) RUNNER_EXE="$2"; shift 2 ;;
    --probe-exe) PROBE_EXE="$2"; shift 2 ;;
    --enroll-cred) ENROLL_CRED="$2"; shift 2 ;;
    --pin) PIN_HEX="$2"; shift 2 ;;
    --lab-ip) LAB_IP="$2"; shift 2 ;;
    --http-port) HTTP_PORT="$2"; shift 2 ;;
    --matrix-port) MATRIX_PORT="$2"; shift 2 ;;
    --coord-port) COORD_PORT="$2"; shift 2 ;;
    --wss-port) WSS_PORT="$2"; shift 2 ;;
    --sink-port) SINK_PORT="$2"; shift 2 ;;
    --skew-floor-ms) SKEW_FLOOR_MS="$2"; shift 2 ;;
    --skew-ceil-ms) SKEW_CEIL_MS="$2"; shift 2 ;;
    --net-wait-secs) NET_WAIT_SECS="$2"; shift 2 ;;
    --seal-timeout-secs) SEAL_TIMEOUT_SECS="$2"; shift 2 ;;
    --model-substr) MODEL_SUBSTR="$2"; shift 2 ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --stage) STAGE="$2"; shift 2 ;;
    --iface) IFACE="$2"; shift 2 ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "${OUT}" ] || die "--out DIR is required"
[ -n "${RUNNER_EXE}" ] && [ -f "${RUNNER_EXE}" ] || die "--runner-exe PATH missing/not a file"
[ -n "${PROBE_EXE}" ] && [ -f "${PROBE_EXE}" ] || die "--probe-exe PATH missing/not a file"
[ -n "${ENROLL_CRED}" ] && [ -s "${ENROLL_CRED}" ] || die "--enroll-cred PATH missing/empty"
[ "$(printf '%s' "${PIN_HEX}" | tr -d '[:space:]' | wc -c)" -eq 64 ] || die "--pin must be 64 hex chars"
for exe in "${RUNNER_EXE}" "${PROBE_EXE}"; do
  [ ! -L "${exe}" ] || die "exe must not be a symlink: ${exe}"
  [ "$(head -c2 "${exe}")" = "MZ" ] || die "not a PE/COFF image (no MZ magic): ${exe}"
done
[ -d "${PHASE9D_DIR}" ] || die "Phase-9d runtime not found at ${PHASE9D_DIR}"
[ -e "${OUT}" ] && die "refusing to overwrite existing ${OUT} — pass a fresh --out path"

case "${STAGE}" in
  3) BOOT_CMD_NAME="bamep-i63-stage3-bootstrap.cmd"; RUNNER_ARM_TOKEN="--matrix --arm"
     WINPESHL_SRC="${SCRIPT_DIR}/winpeshl.ini" ;;
  4) BOOT_CMD_NAME="bamep-i63-stage4-bootstrap.cmd"; RUNNER_ARM_TOKEN="--stage4 --arm"
     WINPESHL_SRC="${SCRIPT_DIR}/winpeshl-stage4.ini" ;;
  *) die "--stage must be 3 or 4 (got ${STAGE})" ;;
esac
BOOT_TPL="${SCRIPT_DIR}/${BOOT_CMD_NAME}.template"
[ -f "${BOOT_TPL}" ] || die "bootstrap template not found: ${BOOT_TPL}"
[ -f "${WINPESHL_SRC}" ] || die "winpeshl source not found: ${WINPESHL_SRC}"

hash_of() { sha256sum "$1" | awk '{print $1}'; }

# 1. verify every Phase-9d source asset against the pinned hash (BEFORE)
BEFORE="$(mktemp)"
for rel in "${!PIN[@]}"; do
  src="${PHASE9D_DIR}/${rel}"
  [ -f "${src}" ] || die "pinned Phase-9d asset missing: ${src}"
  [ ! -L "${src}" ] || die "Phase-9d asset is unexpectedly a symlink: ${src}"
  got="$(hash_of "${src}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "Phase-9d asset ${rel} hash ${got} != pinned — refusing to derive"
  printf '%s  %s\n' "${got}" "${rel}"
done | sort > "${BEFORE}"
echo "derive-stage${STAGE}: Phase-9d source assets verified against pinned hashes (7 files)"

# 2. build the derived tree
mkdir -p "${OUT}/tftp/ipxeboot/x86_64-sb" "${OUT}/http"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxe.efi"                            "${OUT}/tftp/ipxe.efi"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi" "${OUT}/tftp/ipxeboot/x86_64-sb/snponly-shim.efi"
install -m 0644 "${PHASE9D_DIR}/tftp/ipxeboot/x86_64-sb/snponly.efi"      "${OUT}/tftp/ipxeboot/x86_64-sb/snponly.efi"
ln -s "${PHASE9D_DIR}/http/wimboot"  "${OUT}/http/wimboot"
ln -s "${PHASE9D_DIR}/http/BCD"      "${OUT}/http/BCD"
ln -s "${PHASE9D_DIR}/http/boot.sdi" "${OUT}/http/boot.sdi"
ln -s "${PHASE9D_DIR}/http/boot.wim" "${OUT}/http/boot.wim"

# 2c. derived autoexec.ipxe — Phase-9d chain + FIVE injected-file initrd lines,
#     boot.wim LAST. wimboot pause kept (the one intentional operator keypress).
cat > "${OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe" <<EOF
#!ipxe
echo Bamep Issue 63 Stage ${STAGE} (derived from Phase 9d - boot.wim/BCD/boot.sdi unmodified)
show efi/SecureBoot
kernel http://${LAB_IP}:${HTTP_PORT}/wimboot pause
initrd http://${LAB_IP}:${HTTP_PORT}/BCD BCD
initrd http://${LAB_IP}:${HTTP_PORT}/boot.sdi boot.sdi
initrd http://${LAB_IP}:${HTTP_PORT}/winpeshl.ini winpeshl.ini
initrd http://${LAB_IP}:${HTTP_PORT}/${BOOT_CMD_NAME} ${BOOT_CMD_NAME}
initrd http://${LAB_IP}:${HTTP_PORT}/bamep-i63-runner.exe bamep-i63-runner.exe
initrd http://${LAB_IP}:${HTTP_PORT}/bamep-i63-stage2-probe.exe bamep-i63-stage2-probe.exe
initrd http://${LAB_IP}:${HTTP_PORT}/bamep-i63-enroll.cred bamep-i63-enroll.cred
initrd http://${LAB_IP}:${HTTP_PORT}/boot.wim boot.wim
imgstat
boot
EOF

# 2d. injected System32 payload
install -m 0644 "${WINPESHL_SRC}" "${OUT}/http/winpeshl.ini"
sed -e "s|@RUN_ID@|${RUN_ID}|g" \
    -e "s|@LAB_IP@|${LAB_IP}|g" \
    -e "s|@MATRIX_PORT@|${MATRIX_PORT}|g" \
    -e "s|@COORD_PORT@|${COORD_PORT}|g" \
    -e "s|@WSS_PORT@|${WSS_PORT}|g" \
    -e "s|@SINK_PORT@|${SINK_PORT}|g" \
    -e "s|@PIN@|${PIN_HEX}|g" \
    -e "s|@MODEL_SUBSTR@|${MODEL_SUBSTR}|g" \
    -e "s|@SKEW_FLOOR_MS@|${SKEW_FLOOR_MS}|g" \
    -e "s|@SKEW_CEIL_MS@|${SKEW_CEIL_MS}|g" \
    -e "s|@NET_WAIT_SECS@|${NET_WAIT_SECS}|g" \
    -e "s|@SEAL_TIMEOUT_SECS@|${SEAL_TIMEOUT_SECS}|g" \
    "${BOOT_TPL}" > "${OUT}/http/${BOOT_CMD_NAME}"
chmod 0644 "${OUT}/http/${BOOT_CMD_NAME}"
grep -qE '@[A-Z_]+@' "${OUT}/http/${BOOT_CMD_NAME}" && die "unsubstituted @TOKEN@ left in bootstrap.cmd"
grep -qF -- "${RUNNER_ARM_TOKEN}" "${OUT}/http/${BOOT_CMD_NAME}" || die "bootstrap.cmd does not launch the runner with ${RUNNER_ARM_TOKEN}"
grep -qF -- "${PIN_HEX}" "${OUT}/http/${BOOT_CMD_NAME}" || die "bootstrap.cmd does not carry the pin"
install -m 0644 "${RUNNER_EXE}" "${OUT}/http/bamep-i63-runner.exe"
install -m 0644 "${PROBE_EXE}"  "${OUT}/http/bamep-i63-stage2-probe.exe"
install -m 0600 "${ENROLL_CRED}" "${OUT}/http/bamep-i63-enroll.cred"

# 2e. derived dnsmasq.conf
cat > "${OUT}/dnsmasq.conf" <<EOF
# Bamep Issue #63 Stage 3 - THROWAWAY lab harness. NOT production configuration.
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

# 3. re-verify Phase-9d originals (AFTER) — must be byte-identical
AFTER="$(mktemp)"
for rel in "${!PIN[@]}"; do
  got="$(hash_of "${PHASE9D_DIR}/${rel}")"
  [ "${got}" = "${PIN[$rel]}" ] || die "Phase-9d asset ${rel} CHANGED during derive (${got}) — abort"
  printf '%s  %s\n' "${got}" "${rel}"
done | sort > "${AFTER}"
diff -q "${BEFORE}" "${AFTER}" >/dev/null || { diff "${BEFORE}" "${AFTER}" >&2 || true; die "Phase-9d before/after hashes differ"; }
cp "${BEFORE}" "${OUT}/phase9d-hashes-before.txt"
cp "${AFTER}"  "${OUT}/phase9d-hashes-after.txt"
rm -f "${BEFORE}" "${AFTER}"
echo "derive-stage3: Phase-9d originals byte-identical before and after (7/7)"

# 4. verify the SERVED bytes (through the symlinks/copies) == pinned
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
echo "derive-stage3: derived tree serves the exact pinned Phase-9d bytes (7/7)"

# 5. secret sweep + forbidden-file sweep — EVERYTHING except the one injected
#    first-contact enrollment credential (bamep-i63-enroll.cred).
SWEEP_FILES="$(find "${OUT}/http" "${OUT}/tftp" -type f ! -name 'bamep-i63-enroll.cred')"
if printf '%s\n' "${SWEEP_FILES}" | xargs -r grep -IlE \
      -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' \
      -e '(password|passwd|secret|bearer|api[_-]?key|credential)[[:space:]]*[:=]' \
      -e 'Authorization:[[:space:]]*Bearer' 2>/dev/null | grep -q .; then
  die "secret-shaped content found in the derived boot tree (outside the permitted enrollment credential)"
fi
if printf '%s\n' "${SWEEP_FILES}" | grep -E '\.(cred|pem|key|pkcs8|der|p12|pfx)$' | grep -q .; then
  die "unexpected credential/key file present in the derived boot tree"
fi
[ -f "${OUT}/http/bamep-i63-enroll.cred" ] || die "the injected enrollment credential is missing"
[ "$(stat -c '%a' "${OUT}/http/bamep-i63-enroll.cred")" = "600" ] || die "enrollment credential must be mode 600"
echo "derive-stage3: secret sweep OK (only the single mode-600 first-contact credential is present)"

# 6. structural checks on the derived autoexec
AX="${OUT}/tftp/ipxeboot/x86_64-sb/autoexec.ipxe"
for inj in "winpeshl.ini winpeshl.ini" \
           "${BOOT_CMD_NAME} ${BOOT_CMD_NAME}" \
           "bamep-i63-runner.exe bamep-i63-runner.exe" \
           "bamep-i63-stage2-probe.exe bamep-i63-stage2-probe.exe" \
           "bamep-i63-enroll.cred bamep-i63-enroll.cred"; do
  grep -qxF "initrd http://${LAB_IP}:${HTTP_PORT}/${inj}" "${AX}" || die "autoexec missing injection: ${inj}"
done
[ "$(grep -c '^initrd .*/boot.wim boot.wim$' "${AX}")" = "1" ] || die "autoexec must contain exactly one boot.wim initrd"
[ "$(grep '^initrd ' "${AX}" | tail -1)" = "initrd http://${LAB_IP}:${HTTP_PORT}/boot.wim boot.wim" ] || die "boot.wim initrd must be the LAST initrd line"
STRAY="$(grep -n '#' "${AX}" | grep -v '^1:#!ipxe$' || true)"
[ -z "${STRAY}" ] || die "stray '#' in autoexec: ${STRAY}"
grep -qi '^prompt' "${AX}" && die "autoexec must contain no iPXE 'prompt' command"
echo "derive-stage3: derived autoexec.ipxe structurally OK (5 injections, boot.wim last, one keypress = wimboot pause)"

# 7. manifest
{
  echo "# Bamep Issue #63 Stage 3 derived-runtime manifest ($(date -Is))"
  echo "# run_id=${RUN_ID} lab_ip=${LAB_IP} http=${HTTP_PORT} matrix=${MATRIX_PORT} coord=${COORD_PORT} wss=${WSS_PORT} sink=${SINK_PORT}"
  echo "# skew_window_ms=[${SKEW_FLOOR_MS}, ${SKEW_CEIL_MS}] net_wait_secs=${NET_WAIT_SECS} seal_timeout_secs=${SEAL_TIMEOUT_SECS} iface=${IFACE}"
  echo
  echo "## authored / substituted / copied (independent bytes; credential hash NOT listed):"
  sha256sum "${AX}" \
            "${OUT}/http/winpeshl.ini" \
            "${OUT}/http/${BOOT_CMD_NAME}" \
            "${OUT}/http/bamep-i63-runner.exe" \
            "${OUT}/http/bamep-i63-stage2-probe.exe" \
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
