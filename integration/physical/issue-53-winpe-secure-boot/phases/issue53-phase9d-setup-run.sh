#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 9d - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review of every step below, and do not execute it automatically from
# any agent - it must be run interactively by the owner.
#
# This is still a Technical Spike. This script does NOT select Bamep's
# production network-delivery mechanism.
#
# ---------------------------------------------------------------------
# Baseline (Phase 9c, physically executed and evidence-preserved at
# /var/tmp/bamep-issue53-phase9c-wimboot-pause/):
# ---------------------------------------------------------------------
#
# Secure Boot Enabled/Active/Standard -> official iPXE Secure Boot shim ->
# official snponly.efi second stage -> iPXE v2.0.0 (g12798) -> HTTP ->
# official wimboot v2.9.0 -> exact stock WinPE boot.wim -> wimboot
# execution -> WIM parsing -> Secure Boot-aware boot-manager selection ->
# bootmgfw_EX.efi rejected by the FIRMWARE itself with "Verification
# failed: Security Policy Violation" / EFI_STATUS 0x800000000000001a ->
# fallback bootmgfw.efi loaded successfully -> wimboot's own pause reached.
#
# The owner then accidentally pressed a second key, crossing the Phase 9c
# boundary. Windows Boot Manager started and failed immediately:
#   File: \EFI\Microsoft\Boot\BCD
#   Status: 0xc000000f
#   Info: The Boot Configuration Data for your PC is missing or contains
#         errors.
# No Endpoint network traffic occurred after the boot.wim HTTP transfer -
# everything from wimboot execution through the BCD failure was local.
#
# Phase 9c classification: progressed to a new meaningful boundary.
# Overall Issue #53: B - unchanged.
#
# Read-only root-cause investigation (wimboot's current source,
# src/efifile.c / src/main.c): wimboot's automatic BCD/boot.sdi
# extraction-from-WIM searches only DVD/installation-media-convention
# paths (\Windows\Boot\DVD\EFI\BCD, \Windows\Boot\DVD\EFI\boot.sdi,
# \sms\boot\boot.sdi), and silently skips any path not found - no error is
# surfaced unless neither bootmgfw nor bootmgfw_EX is found at all. Cross-
# referenced against this WIM's DISM metadata (Edition/Installation:
# WindowsPE - a standalone deployment image, not full Setup/installation
# media) and against this repository's own prior evidence that ADK WinPE
# media conventionally ships BCD/boot.sdi as separate sibling files
# alongside boot.wim (not embedded within it), the best-supported
# hypothesis was that this stock boot.wim simply does not contain a BCD/
# boot.sdi at the paths wimboot's automatic extraction searches.
#
# ---------------------------------------------------------------------
# Phase 9d question (single new variable: explicitly supply the SAME ADK
# media's external BCD and boot.sdi alongside the same wimboot+boot.wim):
# ---------------------------------------------------------------------
#
# If the exact external BCD and boot.sdi belonging to this same ADK WinPE
# media are explicitly supplied to wimboot, can the physical Endpoint
# continue through wimboot -> Windows Boot Manager -> WinPE -> wpeinit ->
# X:\Windows\System32\cmd.exe, with Secure Boot remaining enabled? This is
# the decisive functional WinPE probe for overall Issue #53 outcome A.
#
# Artifact provenance (re-verified again below before staging anything):
#
#   Boot chain (IDENTICAL to Phase 9c): iPXE v2.0.0, snponly-shim.efi,
#   snponly.efi - same hashes as every prior Phase 9 sub-phase.
#
#   wimboot v2.9.0 (reused from Phase 9c's provenance copy, same official
#   asset, unchanged): size 76064 bytes, sha256
#   5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3
#
#   boot.wim (reused from Phase 9c's provenance copy, same retained stock
#   WinPE, unchanged, never mounted/rebuilt/exported): size 340134390
#   bytes, sha256
#   fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197
#
#   BCD (NEW - the exact external BCD belonging to the SAME ADK WinPE
#   media, Windows source
#   C:\BamepSpike\winpe_media\amd64\media\Boot\BCD; enumerated read-only
#   via a disposable copy on the Windows side, never via bcdedit against
#   the pristine original; Fedora and Windows hashes matched exactly):
#     size 262144 bytes, sha256
#     c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41
#   This script only COPIES this file (install -m 0644) and hashes the
#   copy. It NEVER runs bcdedit, hivex, reg tools, mount, repair, or any
#   mutable inspection against it, at any point, in either the source or
#   the staged copy.
#
#   boot.sdi (NEW - the exact external boot.sdi from the SAME ADK WinPE
#   media, Windows source
#   C:\BamepSpike\winpe_media\amd64\media\Boot\boot.sdi):
#     size 3170304 bytes, sha256
#     cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd
#
# Upstream semantics independently re-verified for THIS phase (not taken
# on faith, re-fetched from https://ipxe.org/wimboot and
# https://ipxe.org/appnote/wimboot_architecture in this session):
#   - "Custom boot manager": "wimboot will attempt to extract an
#     appropriate boot manager ... along with the boot configuration data
#     (BCD). You can disable this behaviour by explicitly providing an
#     appropriate set of boot manager files," with the exact example
#     `initrd bcd bcd` (case-insensitive - wimboot's own source does
#     `wcscasecmp(wname, L"BCD")`). Supplying BCD/boot.sdi explicitly, as
#     this phase does, is the documented, correct way to bypass the
#     automatic from-WIM extraction that Phase 9c showed does not find
#     anything for this WIM lineage.
#   - Architecture guide: files fetched via `initrd <uri> <name>` are
#     placed in a CPIO archive under the literal flat virtual filename
#     <name> (no subdirectory components), and are then exposed by
#     wimboot's virtual filesystem at ALL of: \, \Boot, \Boot\Fonts,
#     \Boot\Resources, \Sources, \EFI, \EFI\Boot, \EFI\Microsoft,
#     \EFI\Microsoft\Boot simultaneously - including the exact
#     \EFI\Microsoft\Boot\BCD path that Phase 9c's Windows Boot Manager
#     reported as "missing". "The BCD file name must be simply BCD" and
#     "boot.sdi"/"boot.wim" are exactly the virtual names required - this
#     script uses exactly those three names via `initrd ... BCD`,
#     `initrd ... boot.sdi`, `initrd ... boot.wim`.
#   - "Disabling automatic BCD modifications": "wimboot will automatically
#     patch standard BIOS-compatible boot configuration data (BCD) files
#     to allow them to be used on UEFI systems, by changing all
#     occurrences of the string '.exe' to '.efi'. You can disable this
#     behaviour by using the rawbcd command-line option." This phase
#     deliberately does NOT pass `rawbcd`, so normal automatic patching
#     applies - exactly the owner's intended design; there is no upstream
#     evidence suggesting rawbcd is needed or beneficial here.
#   - https://ipxe.org/cmd/kernel / https://ipxe.org/cmd/prompt: same
#     semantics already verified in Phase 9c (kernel passes trailing
#     arguments to the image; a bare `prompt` with no timeout blocks
#     forever). This phase's script contains NO `prompt` command at all -
#     see the ONE-KEYPRESS CONTRACT below.
#   - https://ipxe.org/scripting: '#' anywhere on a line begins a trailing
#     comment. This script uses "Issue 53" (no '#') and contains '#' only
#     on the mandatory "#!ipxe" magic first line.
#
# ONE-KEYPRESS CONTRACT (different from Phase 9c, which had TWO gates):
#   Phase 9c's iPXE-owned `prompt` command is DELIBERATELY REMOVED in this
#   script. iPXE execution, HTTP, imgstat, and wimboot execution are
#   already validated by Phases 9a-9c; no further iPXE-side confirmation
#   gate is needed. The autoexec below goes straight from `imgstat` to
#   `boot`, with NO operator prompt of our own. The ONLY intentional
#   keypress in this phase is the one wimboot itself requests at its own
#   "Press any key to continue booting..." line (identical wording/timing
#   to the boundary already reached and correctly recognized in Phase
#   9c). After that ONE keypress, the owner must NOT press anything else
#   unless Windows/WinPE itself presents an unexpected interactive prompt
#   that must be captured for evidence. If any error appears, STOP,
#   photograph/transcribe it, and do not retry or manually repair.
#
# Deliberately NOT staged: explicit bootmgfw.efi, explicit bootmgfw_EX.efi,
# bootx64.efi, boot.stl, fonts, policy files, any custom/modified BCD, any
# startup script. This phase changes exactly one thing versus Phase 9c:
# the addition of the two external, unmodified, pristine-media files BCD
# and boot.sdi. wimboot's automatic from-WIM extraction attempt (and the
# "directory entry \"DVD\"/\"sms\" not found"-style silent misses it may
# still produce internally, per Phase 9c's root-cause finding) is EXPECTED
# to still occur as a side activity and is NOT a blocker by itself, since
# our explicitly-supplied BCD/boot.sdi are independent of that WIM-search
# path and take effect regardless. bootmgfw_EX.efi may again be rejected
# by the firmware exactly as in Phase 9c - that is not "fixed" here and is
# not itself a Phase 9d failure, provided wimboot again falls back to the
# classic bootmgfw.efi as it did physically last time.
#
# Interpretation boundary: overall Issue #53 reaches outcome A ONLY if a
# functional WinPE shell (X:\Windows\System32\cmd.exe after wpeinit) is
# actually reached - not merely Windows Boot Manager starting. Use only
# "progressed to a new meaningful boundary" / "same effective boundary" /
# "failed earlier" / "harness prevented evaluation" for this individual
# phase.
#
# Evidence strategy (unchanged discipline from Phase 9c, extended to the
# two new small files): wimboot and BCD and boot.sdi are all small enough
# to be fully reassembled and hashed from the pcap, exactly as done for
# Phase 9b's probe.txt and Phase 9c's wimboot. boot.wim remains the one
# exception: Content-Length + TCP sequence/ACK arithmetic is the primary
# transfer-completion proof, with full reassembly available as an
# optional fallback escalation, not claimed as byte-exact from
# Content-Length alone.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself. This script NEVER triggers the physical PXE boot itself - it
# stops after confirming the harness is ready and waits for the owner to
# power on/reboot the Endpoint by hand.

set -euo pipefail
export LC_ALL=C

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-phase9d-winpe-completion"
TFTP_ROOT="${SPIKE_DIR}/tftp"
HTTP_ROOT="${SPIKE_DIR}/http"
SHIM_DIR="${TFTP_ROOT}/ipxeboot/x86_64-sb"
ROOT_IPXE_EFI="${TFTP_ROOT}/ipxe.efi"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
HTTP_PORT="8080"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-winpe-completion.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"
HTTP_LOG="${SPIKE_DIR}/http-server.log"

PROVENANCE_DIR="/home/brener/.claude/jobs/07ec8ea7/tmp/issue53-phase9-provenance"
PHASE9C_PROVENANCE_DIR="/home/brener/.claude/jobs/07ec8ea7/tmp/issue53-phase9c-provenance"
PHASE9D_PROVENANCE_DIR="/home/brener/.claude/jobs/07ec8ea7/tmp/issue53-phase9d-provenance"
ARCHIVE_SOURCE="${PROVENANCE_DIR}/ipxeboot.tar.gz"
SHIM_SOURCE="${PROVENANCE_DIR}/ipxeboot/x86_64-sb/snponly-shim.efi"
SNPONLY_SOURCE="${PROVENANCE_DIR}/ipxeboot/x86_64-sb/snponly.efi"
WIMBOOT_SOURCE="${PHASE9C_PROVENANCE_DIR}/wimboot"
BOOTWIM_SOURCE="${PHASE9C_PROVENANCE_DIR}/boot.wim"
BCD_SOURCE="${PHASE9D_PROVENANCE_DIR}/BCD"
BOOTSDI_SOURCE="${PHASE9D_PROVENANCE_DIR}/boot.sdi"

EXPECTED_ARCHIVE_SHA256="01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1"
EXPECTED_ARCHIVE_SIZE="12002760"
EXPECTED_SHIM_SHA256="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
EXPECTED_SHIM_SIZE="1038920"
EXPECTED_SNPONLY_SHA256="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
EXPECTED_SNPONLY_SIZE="295784"
EXPECTED_AUTOEXEC_SHA256="bd325b3838585e15536d74f256c6a1017b4576e579f8de80e8dd237e55c30005"
EXPECTED_AUTOEXEC_SIZE="255"
EXPECTED_WIMBOOT_SHA256="5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3"
EXPECTED_WIMBOOT_SIZE="76064"
EXPECTED_BOOTWIM_SHA256="fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197"
EXPECTED_BOOTWIM_SIZE="340134390"
EXPECTED_BCD_SHA256="c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41"
EXPECTED_BCD_SIZE="262144"
EXPECTED_BOOTSDI_SHA256="cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd"
EXPECTED_BOOTSDI_SIZE="3170304"

# Disk-space gate: same rationale as Phase 9c (boot.wim dominates total
# size by ~100x over BCD+boot.sdi+wimboot combined, so the formula is
# unchanged: require at least 2,000,000,000 bytes free, or 5x the
# boot.wim size, whichever is larger).
REQUIRED_FREE_BYTES=$(( 5 * EXPECTED_BOOTWIM_SIZE ))
MIN_FREE_FLOOR=2000000000
if [ "${REQUIRED_FREE_BYTES}" -lt "${MIN_FREE_FLOOR}" ]; then
    REQUIRED_FREE_BYTES="${MIN_FREE_FLOOR}"
fi

# HTTP-like-listener check, identical logic to the corrected Phase
# 9a/9a2/9b/9c version (also mirrored into issue53-phase9d-cleanup.sh).
http_like_listener_conflicts_with_lab_path() {
    local conflict_found=1
    local local_addr addr port
    while read -r local_addr; do
        [ -z "${local_addr}" ] && continue
        if [[ "${local_addr}" =~ ^\[(.*)\]:([0-9]+)$ ]]; then
            addr="${BASH_REMATCH[1]}"
            port="${BASH_REMATCH[2]}"
        elif [[ "${local_addr}" =~ ^([^:]+):([0-9]+)$ ]]; then
            addr="${BASH_REMATCH[1]}"
            port="${BASH_REMATCH[2]}"
        else
            continue
        fi
        case "${port}" in
            80|8080|8000|443) : ;;
            *) continue ;;
        esac
        case "${addr}" in
            0.0.0.0|'*'|::|"${ADDR_HOST}")
                echo "CONFLICT: HTTP-like listener bound to ${addr}:${port}"
                conflict_found=0
                ;;
            *)
                echo "benign, interface-specific (does not reach ${IFACE}/${ADDR_HOST}): ${addr}:${port}"
                ;;
        esac
    done < <(ss -Hltn 2>/dev/null | awk '{print $4}')
    return "${conflict_found}"
}

echo "== Bamep Issue #53 Phase 9d - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Question:  with the exact external BCD+boot.sdi from the same ADK"
echo "              media supplied alongside wimboot+boot.wim, does the"
echo "              physical Endpoint reach a functional WinPE shell?"
echo "   This script NEVER triggers the physical PXE boot itself."
echo "   ONE intentional keypress only: wimboot's OWN pause prompt."
echo

# --------------------------------------------------------------------
# Gate group A: local, read-only artifact/hash/absence checks. ALL of
# these complete BEFORE this script touches network state or starts any
# DHCP/TFTP/HTTP service.
# --------------------------------------------------------------------

echo "-- Gate A0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-phase9d-cleanup.sh --purge"
    exit 1
fi
echo "OK: no stale ${SPIKE_DIR} present."
echo

echo "-- Gate A1: official iPXE release archive matches pinned provenance --"
[ -f "${ARCHIVE_SOURCE}" ] || { echo "ABORT: ${ARCHIVE_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${ARCHIVE_SOURCE}")" = "${EXPECTED_ARCHIVE_SIZE}" ] || { echo "ABORT: archive size mismatch."; exit 1; }
[ "$(sha256sum "${ARCHIVE_SOURCE}" | awk '{print $1}')" = "${EXPECTED_ARCHIVE_SHA256}" ] || { echo "ABORT: archive SHA-256 mismatch."; exit 1; }
echo "OK: ${ARCHIVE_SOURCE} matches pinned provenance."
echo

echo "-- Gate A2: shim source (dereferenced) matches pinned provenance --"
[ -f "${SHIM_SOURCE}" ] || { echo "ABORT: ${SHIM_SOURCE} not found."; exit 1; }
if [ -L "${SHIM_SOURCE}" ]; then
    echo "confirmed symlink: $(readlink "${SHIM_SOURCE}") (resolves to $(readlink -f "${SHIM_SOURCE}"))"
fi
[ "$(stat -Lc '%s' "${SHIM_SOURCE}")" = "${EXPECTED_SHIM_SIZE}" ] || { echo "ABORT: shim dereferenced size mismatch."; exit 1; }
[ "$(sha256sum "${SHIM_SOURCE}" | awk '{print $1}')" = "${EXPECTED_SHIM_SHA256}" ] || { echo "ABORT: shim dereferenced SHA-256 mismatch."; exit 1; }
echo "OK: ${SHIM_SOURCE} dereferences to pinned shimx64.efi content."
echo

echo "-- Gate A3: snponly.efi source matches pinned provenance (staged twice below) --"
[ -f "${SNPONLY_SOURCE}" ] || { echo "ABORT: ${SNPONLY_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${SNPONLY_SOURCE}")" = "${EXPECTED_SNPONLY_SIZE}" ] || { echo "ABORT: snponly.efi size mismatch."; exit 1; }
[ "$(sha256sum "${SNPONLY_SOURCE}" | awk '{print $1}')" = "${EXPECTED_SNPONLY_SHA256}" ] || { echo "ABORT: snponly.efi SHA-256 mismatch."; exit 1; }
echo "OK: ${SNPONLY_SOURCE} matches pinned size/SHA-256."
echo

echo "-- Gate A4: wimboot v2.9.0 source matches pinned provenance (reused from Phase 9c) --"
[ -f "${WIMBOOT_SOURCE}" ] || { echo "ABORT: ${WIMBOOT_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${WIMBOOT_SOURCE}")" = "${EXPECTED_WIMBOOT_SIZE}" ] || { echo "ABORT: wimboot size mismatch."; exit 1; }
[ "$(sha256sum "${WIMBOOT_SOURCE}" | awk '{print $1}')" = "${EXPECTED_WIMBOOT_SHA256}" ] || { echo "ABORT: wimboot SHA-256 mismatch."; exit 1; }
echo "OK: ${WIMBOOT_SOURCE} matches pinned size (${EXPECTED_WIMBOOT_SIZE}) and SHA-256."
echo

echo "-- Gate A5: retained stock boot.wim matches pinned provenance (reused from Phase 9c, NOT mounted/rebuilt) --"
[ -f "${BOOTWIM_SOURCE}" ] || { echo "ABORT: ${BOOTWIM_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${BOOTWIM_SOURCE}")" = "${EXPECTED_BOOTWIM_SIZE}" ] || { echo "ABORT: boot.wim size mismatch."; exit 1; }
echo "Hashing ${EXPECTED_BOOTWIM_SIZE} bytes (this can take a few seconds)..."
[ "$(sha256sum "${BOOTWIM_SOURCE}" | awk '{print $1}')" = "${EXPECTED_BOOTWIM_SHA256}" ] || { echo "ABORT: boot.wim SHA-256 mismatch."; exit 1; }
echo "OK: ${BOOTWIM_SOURCE} matches pinned size (${EXPECTED_BOOTWIM_SIZE}) and SHA-256."
echo

echo "-- Gate A6: external BCD source matches pinned provenance (NEW this phase) --"
echo "   This gate ONLY stats and hashes the file. It never invokes bcdedit,"
echo "   hivex, reg tools, mount, repair, or any mutable inspection - here or"
echo "   anywhere else in this script."
[ -f "${BCD_SOURCE}" ] || { echo "ABORT: ${BCD_SOURCE} not found."; exit 1; }
[ -L "${BCD_SOURCE}" ] && { echo "ABORT: BCD source is a symlink, refusing to treat as pristine."; exit 1; }
[ "$(stat -c '%s' "${BCD_SOURCE}")" = "${EXPECTED_BCD_SIZE}" ] || { echo "ABORT: BCD size mismatch."; exit 1; }
[ "$(sha256sum "${BCD_SOURCE}" | awk '{print $1}')" = "${EXPECTED_BCD_SHA256}" ] || { echo "ABORT: BCD SHA-256 mismatch."; exit 1; }
echo "OK: ${BCD_SOURCE} matches pinned size (${EXPECTED_BCD_SIZE}) and SHA-256 (${EXPECTED_BCD_SHA256})."
echo

echo "-- Gate A7: external boot.sdi source matches pinned provenance (NEW this phase) --"
[ -f "${BOOTSDI_SOURCE}" ] || { echo "ABORT: ${BOOTSDI_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${BOOTSDI_SOURCE}")" = "${EXPECTED_BOOTSDI_SIZE}" ] || { echo "ABORT: boot.sdi size mismatch."; exit 1; }
[ "$(sha256sum "${BOOTSDI_SOURCE}" | awk '{print $1}')" = "${EXPECTED_BOOTSDI_SHA256}" ] || { echo "ABORT: boot.sdi SHA-256 mismatch."; exit 1; }
echo "OK: ${BOOTSDI_SOURCE} matches pinned size (${EXPECTED_BOOTSDI_SIZE}) and SHA-256 (${EXPECTED_BOOTSDI_SHA256})."
echo

echo "-- Gate A8: disk-space preflight (need >= ${REQUIRED_FREE_BYTES} bytes free) --"
AVAIL_BYTES="$(df --output=avail -B1 "$(dirname "${SPIKE_DIR}")" | tail -1 | tr -d ' ')"
echo "Available: ${AVAIL_BYTES} bytes; required: ${REQUIRED_FREE_BYTES} bytes."
if [ "${AVAIL_BYTES}" -lt "${REQUIRED_FREE_BYTES}" ]; then
    echo "ABORT: insufficient free disk space for staged assets + full pcap + logs."
    exit 1
fi
echo "OK: sufficient free disk space."
echo

echo "== Gate group B: stage the TFTP tree (local filesystem only, no network mutation yet) =="
echo

echo "-- Step B1: create the Spike TFTP directory tree (owned by brener, no sudo) --"
mkdir -p "${SHIM_DIR}"
echo "Created ${SHIM_DIR} (and ${TFTP_ROOT} as its parent)"
echo

echo "-- Step B2: copy the shim (unchanged from every prior Phase 9 sub-phase) --"
install -m 0644 "${SHIM_SOURCE}" "${SHIM_DIR}/snponly-shim.efi"
[ -L "${SHIM_DIR}/snponly-shim.efi" ] && { echo "ABORT: staged shim is a symlink."; exit 1; }
[ "$(stat -c '%s' "${SHIM_DIR}/snponly-shim.efi")" = "${EXPECTED_SHIM_SIZE}" ] || { echo "ABORT: staged shim size mismatch."; exit 1; }
[ "$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')" = "${EXPECTED_SHIM_SHA256}" ] || { echo "ABORT: staged shim SHA-256 mismatch."; exit 1; }
echo "OK: ${SHIM_DIR}/snponly-shim.efi staged and re-verified."
echo

echo "-- Step B2b: stage snponly.efi content TWICE, from the SAME source (unchanged) --"
install -m 0644 "${SNPONLY_SOURCE}" "${SHIM_DIR}/snponly.efi"
install -m 0644 "${SNPONLY_SOURCE}" "${ROOT_IPXE_EFI}"
for f in "${SHIM_DIR}/snponly.efi" "${ROOT_IPXE_EFI}"; do
    [ -L "${f}" ] && { echo "ABORT: ${f} is a symlink."; exit 1; }
done
SIBLING_INODE="$(stat -c '%i' "${SHIM_DIR}/snponly.efi")"
ROOT_INODE="$(stat -c '%i' "${ROOT_IPXE_EFI}")"
[ "${SIBLING_INODE}" != "${ROOT_INODE}" ] || { echo "ABORT: hardlink detected, not an independent copy."; exit 1; }
for f in "${SHIM_DIR}/snponly.efi" "${ROOT_IPXE_EFI}"; do
    [ "$(stat -c '%s' "${f}")" = "${EXPECTED_SNPONLY_SIZE}" ] || { echo "ABORT: ${f} size mismatch."; exit 1; }
    [ "$(sha256sum "${f}" | awk '{print $1}')" = "${EXPECTED_SNPONLY_SHA256}" ] || { echo "ABORT: ${f} SHA-256 mismatch."; exit 1; }
done
echo "OK: both snponly.efi copies staged, independent inodes (${SIBLING_INODE} vs ${ROOT_INODE}), re-verified."
echo

echo "-- Step B3: author the NEW Phase 9d autoexec.ipxe deterministically --"
echo "   Written with printf and explicit \\n escapes only. Uses 'Issue 53' (no"
echo "   '#'). Contains NO iPXE-owned prompt this time - see the ONE-KEYPRESS"
echo "   CONTRACT in the header comment."
printf '#!ipxe\necho Bamep Issue 53 Phase 9d\nshow efi/SecureBoot\nkernel http://%s:%s/wimboot pause\ninitrd http://%s:%s/BCD BCD\ninitrd http://%s:%s/boot.sdi boot.sdi\ninitrd http://%s:%s/boot.wim boot.wim\nimgstat\nboot\n' \
    "${ADDR_HOST}" "${HTTP_PORT}" "${ADDR_HOST}" "${HTTP_PORT}" "${ADDR_HOST}" "${HTTP_PORT}" "${ADDR_HOST}" "${HTTP_PORT}" > "${SHIM_DIR}/autoexec.ipxe"
echo "Wrote ${SHIM_DIR}/autoexec.ipxe"
cat -A "${SHIM_DIR}/autoexec.ipxe"
echo

echo "-- Gate B3a: autoexec.ipxe matches the pinned deterministic size/hash exactly --"
[ "$(stat -c '%s' "${SHIM_DIR}/autoexec.ipxe")" = "${EXPECTED_AUTOEXEC_SIZE}" ] || { echo "ABORT: autoexec.ipxe size mismatch."; exit 1; }
[ "$(sha256sum "${SHIM_DIR}/autoexec.ipxe" | awk '{print $1}')" = "${EXPECTED_AUTOEXEC_SHA256}" ] || { echo "ABORT: autoexec.ipxe SHA-256 mismatch."; exit 1; }
echo "OK: autoexec.ipxe matches pinned size (${EXPECTED_AUTOEXEC_SIZE}) and SHA-256 (${EXPECTED_AUTOEXEC_SHA256})."
echo

echo "-- Gate B3b: confirm no stray '#' beyond the mandatory magic first line --"
STRAY_HASH_LINES="$(grep -n '#' "${SHIM_DIR}/autoexec.ipxe" | grep -v '^1:#!ipxe$' || true)"
[ -z "${STRAY_HASH_LINES}" ] || { echo "ABORT: unexpected '#' found: ${STRAY_HASH_LINES}"; exit 1; }
echo "OK: '#' appears only on the mandatory magic first line."
echo

echo "-- Gate B3c: confirm autoexec.ipxe contains NO 'prompt' command (one-keypress contract) --"
if grep -qi '^prompt' "${SHIM_DIR}/autoexec.ipxe"; then
    echo "ABORT: autoexec.ipxe contains an iPXE 'prompt' command - Phase 9d must have"
    echo "  exactly one intentional keypress, at wimboot's own pause only."
    exit 1
fi
echo "OK: no iPXE 'prompt' command present - the only intentional keypress is wimboot's own."
echo

echo "-- Gate B3d: confirm /autoexec.ipxe does NOT exist at the TFTP root --"
[ ! -e "${TFTP_ROOT}/autoexec.ipxe" ] || { echo "ABORT: ${TFTP_ROOT}/autoexec.ipxe exists."; exit 1; }
echo "OK: no ${TFTP_ROOT}/autoexec.ipxe exists."
echo

echo "-- Gate B4: hash every staged TFTP file --"
sha256sum "${SHIM_DIR}/snponly-shim.efi" "${SHIM_DIR}/snponly.efi" "${SHIM_DIR}/autoexec.ipxe" "${ROOT_IPXE_EFI}" \
    | tee "${SPIKE_DIR}/sha256sums-tftp.txt"
echo

echo "-- Gate B6: forbidden-name sweep over the TFTP tree (defense in depth) --"
FORBIDDEN_HIT=0
while IFS= read -r -d '' f; do
    lower="$(basename "$f" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        ipxe-shim.efi|shimx64.efi|shimaa64.efi|wimboot*|bcd|boot.sdi|boot.wim|*grub*|*fedora*|*winpe*|*bootmgfw*|*bootmgr*|*winload*|*.p7b|*.ttf|*.stl)
            echo "ABORT: forbidden/unexpected file present in TFTP tree: ${f}"
            FORBIDDEN_HIT=1
            ;;
    esac
done < <(find "${TFTP_ROOT}" -type f -print0)
[ "${FORBIDDEN_HIT}" = "0" ] || exit 1
echo "OK: no forbidden filename found under ${TFTP_ROOT} (wimboot/BCD/boot.sdi/boot.wim"
echo "    belong only in the HTTP root, not TFTP)."
echo

echo "-- Gate B7: exact-listing gate - TFTP tree contains EXACTLY the four expected files --"
EXPECTED_TFTP_LIST="$(printf '%s\n' \
    "ipxe.efi" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly-shim.efi" \
    "ipxeboot/x86_64-sb/snponly.efi" \
    | LC_ALL=C sort)"
ACTUAL_TFTP_LIST="$(cd "${TFTP_ROOT}" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)"
if [ "${ACTUAL_TFTP_LIST}" != "${EXPECTED_TFTP_LIST}" ]; then
    echo "ABORT: staged TFTP tree does not match the exact expected file list."
    diff <(printf '%s\n' "${EXPECTED_TFTP_LIST}") <(printf '%s\n' "${ACTUAL_TFTP_LIST}") || true
    exit 1
fi
echo "OK: TFTP tree contains exactly the four expected files."
echo

echo "-- Gate B8: exactly ONE ipxe.efi anywhere under the TFTP tree, at the root --"
IPXE_EFI_HITS="$(find "${TFTP_ROOT}" -iname 'ipxe.efi' | LC_ALL=C sort)"
[ "$(printf '%s\n' "${IPXE_EFI_HITS}" | grep -c .)" = "1" ] || { echo "ABORT: expected exactly one ipxe.efi."; exit 1; }
[ "${IPXE_EFI_HITS}" = "${ROOT_IPXE_EFI}" ] || { echo "ABORT: the one ipxe.efi is not at the intended root path."; exit 1; }
echo "OK: exactly one ipxe.efi, at ${ROOT_IPXE_EFI}."
echo

echo "== Gate group H: stage and gate the dedicated HTTP root (local filesystem only) =="
echo

echo "-- Step H1: create the dedicated HTTP root --"
mkdir -p "${HTTP_ROOT}"
echo "Created ${HTTP_ROOT}"
echo

echo "-- Step H2: copy wimboot, BCD, boot.sdi, and boot.wim into the HTTP root --"
echo "   All four are opaque copies (install -m 0644). BCD/boot.sdi/boot.wim are"
echo "   never mounted, rebuilt, patched, or otherwise mutated by this script -"
echo "   only stat/sha256sum are ever run against them."
install -m 0644 "${WIMBOOT_SOURCE}" "${HTTP_ROOT}/wimboot"
install -m 0644 "${BCD_SOURCE}" "${HTTP_ROOT}/BCD"
install -m 0644 "${BOOTSDI_SOURCE}" "${HTTP_ROOT}/boot.sdi"
echo "Copying boot.wim (${EXPECTED_BOOTWIM_SIZE} bytes)... this may take a moment."
install -m 0644 "${BOOTWIM_SOURCE}" "${HTTP_ROOT}/boot.wim"
echo "Copied all four assets into ${HTTP_ROOT}"
echo

echo "-- Gate H2a: staged wimboot matches pinned size/hash, is a regular file --"
[ -L "${HTTP_ROOT}/wimboot" ] && { echo "ABORT: staged wimboot is a symlink."; exit 1; }
[ "$(stat -c '%s' "${HTTP_ROOT}/wimboot")" = "${EXPECTED_WIMBOOT_SIZE}" ] || { echo "ABORT: staged wimboot size mismatch."; exit 1; }
[ "$(sha256sum "${HTTP_ROOT}/wimboot" | awk '{print $1}')" = "${EXPECTED_WIMBOOT_SHA256}" ] || { echo "ABORT: staged wimboot SHA-256 mismatch."; exit 1; }
echo "OK: staged wimboot matches pinned size/SHA-256."
echo

echo "-- Gate H2b: staged BCD is byte-identical to the pristine source, is a regular file --"
echo "   (re-hash of the STAGED COPY only - the pristine source was already"
echo "   verified in Gate A6 and is never touched again by this script)"
[ -L "${HTTP_ROOT}/BCD" ] && { echo "ABORT: staged BCD is a symlink."; exit 1; }
[ "$(stat -c '%s' "${HTTP_ROOT}/BCD")" = "${EXPECTED_BCD_SIZE}" ] || { echo "ABORT: staged BCD size mismatch."; exit 1; }
[ "$(sha256sum "${HTTP_ROOT}/BCD" | awk '{print $1}')" = "${EXPECTED_BCD_SHA256}" ] || { echo "ABORT: staged BCD SHA-256 mismatch."; exit 1; }
echo "OK: staged BCD matches pinned size (${EXPECTED_BCD_SIZE}) and SHA-256 (${EXPECTED_BCD_SHA256}) -"
echo "    byte-identical to the pristine external media BCD, never opened with bcdedit."
echo

echo "-- Gate H2c: staged boot.sdi matches pinned size/hash, is a regular file --"
[ -L "${HTTP_ROOT}/boot.sdi" ] && { echo "ABORT: staged boot.sdi is a symlink."; exit 1; }
[ "$(stat -c '%s' "${HTTP_ROOT}/boot.sdi")" = "${EXPECTED_BOOTSDI_SIZE}" ] || { echo "ABORT: staged boot.sdi size mismatch."; exit 1; }
[ "$(sha256sum "${HTTP_ROOT}/boot.sdi" | awk '{print $1}')" = "${EXPECTED_BOOTSDI_SHA256}" ] || { echo "ABORT: staged boot.sdi SHA-256 mismatch."; exit 1; }
echo "OK: staged boot.sdi matches pinned size/SHA-256."
echo

echo "-- Gate H2d: staged boot.wim matches pinned size/hash, is a regular file --"
[ -L "${HTTP_ROOT}/boot.wim" ] && { echo "ABORT: staged boot.wim is a symlink."; exit 1; }
[ "$(stat -c '%s' "${HTTP_ROOT}/boot.wim")" = "${EXPECTED_BOOTWIM_SIZE}" ] || { echo "ABORT: staged boot.wim size mismatch."; exit 1; }
echo "Hashing staged boot.wim (${EXPECTED_BOOTWIM_SIZE} bytes)..."
[ "$(sha256sum "${HTTP_ROOT}/boot.wim" | awk '{print $1}')" = "${EXPECTED_BOOTWIM_SHA256}" ] || { echo "ABORT: staged boot.wim SHA-256 mismatch."; exit 1; }
echo "OK: staged boot.wim matches pinned size/SHA-256."
echo

echo "-- Gate H3: forbidden-content sweep over the HTTP root --"
echo "   Must contain ONLY wimboot, BCD, boot.sdi, boot.wim - no bootmgfw.efi/"
echo "   bootmgfw_EX.efi/bootx64.efi/boot.stl/fonts/policy files/custom BCD/"
echo "   startup scripts."
FORBIDDEN_HTTP_HIT=0
while IFS= read -r -d '' f; do
    base="$(basename "$f")"
    lower="$(echo "$base" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        wimboot|bcd|boot.sdi|boot.wim) : ;;
        *)
            echo "ABORT: unexpected file present in HTTP root: ${f}"
            FORBIDDEN_HTTP_HIT=1
            ;;
    esac
done < <(find "${HTTP_ROOT}" -type f -print0)
[ "${FORBIDDEN_HTTP_HIT}" = "0" ] || exit 1
echo "OK: HTTP root contains no unexpected file."
echo

echo "-- Gate H4: exact-listing gate - HTTP root contains EXACTLY the four expected files --"
EXPECTED_HTTP_LIST="$(printf '%s\n' "BCD" "boot.sdi" "boot.wim" "wimboot" | LC_ALL=C sort)"
ACTUAL_HTTP_LIST="$(cd "${HTTP_ROOT}" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)"
if [ "${ACTUAL_HTTP_LIST}" != "${EXPECTED_HTTP_LIST}" ]; then
    echo "ABORT: HTTP root does not contain exactly {wimboot, BCD, boot.sdi, boot.wim}."
    diff <(printf '%s\n' "${EXPECTED_HTTP_LIST}") <(printf '%s\n' "${ACTUAL_HTTP_LIST}") || true
    exit 1
fi
echo "OK: HTTP root contains exactly: BCD, boot.sdi, boot.wim, wimboot."
echo

echo "-- Gate H5: hash the staged HTTP assets --"
sha256sum "${HTTP_ROOT}/wimboot" "${HTTP_ROOT}/BCD" "${HTTP_ROOT}/boot.sdi" "${HTTP_ROOT}/boot.wim" \
    | tee "${SPIKE_DIR}/sha256sums-http.txt"
echo

echo "== Gate group C: network pre-state checks (read-only) =="
echo

echo "-- Gate C1: no existing DHCP/TFTP/PXE listener --"
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: a DHCP/TFTP/PXE listener already exists."
    exit 1
fi
echo "OK: no DHCP/TFTP/PXE listener present."
echo

echo "-- Gate C2: no HTTP-like listener bound to a path that reaches ${IFACE}/${ADDR_HOST} --"
if http_like_listener_conflicts_with_lab_path; then
    echo "ABORT: an HTTP-like listener is bound to an address that would also"
    echo "  accept traffic on ${IFACE}/${ADDR_HOST}."
    exit 1
fi
echo "OK: no HTTP-like listener bound to a wildcard address or to ${ADDR_HOST}."
echo

echo "-- Gate C2b: nothing already listens on ${ADDR_HOST}:${HTTP_PORT} (cannot exist before Step 6 anyway) --"
if ss -Hltn 2>/dev/null | awk '{print $4}' | grep -qF "${ADDR_HOST}:${HTTP_PORT}"; then
    echo "ABORT: something is already listening on ${ADDR_HOST}:${HTTP_PORT}."
    exit 1
fi
echo "OK: nothing currently listens on ${ADDR_HOST}:${HTTP_PORT}."
echo

echo "-- Gate C3: no local IPv4 address already in ${SUBNET} --"
if ip -4 addr show 2>/dev/null | grep -qF '192.168.99.'; then
    echo "ABORT: an address in ${SUBNET} already exists on this host."
    exit 1
fi
echo "OK: no local address in ${SUBNET}."
echo

echo "-- Gate C4: no existing route for ${SUBNET} --"
if ip route show 2>/dev/null | grep -qF "${SUBNET}"; then
    echo "ABORT: a route for ${SUBNET} already exists."
    exit 1
fi
echo "OK: no existing route for ${SUBNET}."
echo

echo "-- Gate C5: current ${IFACE} state --"
ip -4 addr show "${IFACE}"
echo

echo "== All artifact/hash/absence/disk-space/network-pre-state gates passed. =="
echo "== Only now does this script begin mutating network/runtime state. =="
echo

echo "== Step 3: take ${IFACE} out of NetworkManager's automatic management (runtime only) =="
sudo nmcli device set "${IFACE}" managed no
echo

echo "== Step 4: add temporary address (exact add, no flush) =="
sudo ip addr add "${ADDR}" dev "${IFACE}"
ip -4 addr show "${IFACE}"
if ip -4 addr show 2>/dev/null | grep -F "${ADDR_HOST}" | grep -qv "${IFACE}"; then
    echo "ABORT: ${ADDR_HOST} leaked to another interface. Reverting."
    sudo ip addr del "${ADDR}" dev "${IFACE}"
    exit 1
fi
echo "OK: ${ADDR_HOST} only on ${IFACE}."
echo

echo "== Step 5: runtime-only firewalld scope for this throwaway isolated Spike =="
sudo firewall-cmd --zone=trusted --change-interface="${IFACE}"
echo

echo "== Step 6: start the HTTP server, bound ONLY to ${ADDR_HOST}:${HTTP_PORT}, serving ONLY ${HTTP_ROOT} =="
python3 -m http.server --bind "${ADDR_HOST}" --directory "${HTTP_ROOT}" "${HTTP_PORT}" \
    > "${HTTP_LOG}" 2>&1 &
HTTP_PID=$!
sleep 1
if ! kill -0 "${HTTP_PID}" 2>/dev/null; then
    echo "ABORT: HTTP server failed to start. Check ${HTTP_LOG}."
    exit 1
fi
echo "Started python3 http.server (pid ${HTTP_PID})."
echo

echo "-- Step 7 / Gate H6: prove the HTTP server owns/listens on EXACTLY ${ADDR_HOST}:${HTTP_PORT} - nothing else --"
HTTP_LISTEN_LINES="$(ss -Hltnp 2>/dev/null | awk -v p="${HTTP_PID}" '$0 ~ "pid="p"," {print $4}')"
echo "Listening address(es) owned by pid ${HTTP_PID}: ${HTTP_LISTEN_LINES}"
if [ "$(printf '%s\n' "${HTTP_LISTEN_LINES}" | grep -c .)" != "1" ] || [ "${HTTP_LISTEN_LINES}" != "${ADDR_HOST}:${HTTP_PORT}" ]; then
    echo "ABORT: HTTP server is not bound to exactly ${ADDR_HOST}:${HTTP_PORT}."
    sudo kill -TERM "${HTTP_PID}" 2>/dev/null || true
    exit 1
fi
echo "OK: HTTP server (pid ${HTTP_PID}) listens on exactly ${ADDR_HOST}:${HTTP_PORT}."
echo

echo "== Step 8 / Gate H7: LOCAL pre-trigger HTTP verification of ALL FOUR staged assets =="
echo "   Fetches each asset from this Fedora host itself, over the just-assigned"
echo "   ${ADDR_HOST}, and hashes the response body - proving the HTTP server"
echo "   serves the exact pinned bytes BEFORE the physical Endpoint ever asks."
echo "   This verification only READS via HTTP; it never modifies any staged file."
declare -A LOCAL_EXPECTED=(
    [wimboot]="${EXPECTED_WIMBOOT_SHA256}"
    [BCD]="${EXPECTED_BCD_SHA256}"
    [boot.sdi]="${EXPECTED_BOOTSDI_SHA256}"
    [boot.wim]="${EXPECTED_BOOTWIM_SHA256}"
)
for asset in wimboot BCD boot.sdi boot.wim; do
    LOCAL_TMP="$(mktemp)"
    curl -sf -o "${LOCAL_TMP}" "http://${ADDR_HOST}:${HTTP_PORT}/${asset}"
    LOCAL_SHA256="$(sha256sum "${LOCAL_TMP}" | awk '{print $1}')"
    rm -f "${LOCAL_TMP}"
    if [ "${LOCAL_SHA256}" != "${LOCAL_EXPECTED[${asset}]}" ]; then
        echo "ABORT: local HTTP fetch of /${asset} does not match pinned SHA-256."
        sudo kill -TERM "${HTTP_PID}" 2>/dev/null || true
        exit 1
    fi
    echo "OK: local HTTP fetch of /${asset} matches pinned SHA-256 exactly."
done
echo

echo "== Step 9a: author dnsmasq.conf (DHCP+TFTP only; HTTP server above is separate) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 9d - throwaway harness. NOT production configuration.
interface=${IFACE}
bind-interfaces
port=0
dhcp-authoritative
dhcp-range=192.168.99.50,192.168.99.100,255.255.255.0,1h
dhcp-option=option:router
log-dhcp
log-queries
log-facility=${SPIKE_DIR}/dnsmasq.log
dhcp-leasefile=${SPIKE_DIR}/dnsmasq.leases

enable-tftp=${IFACE}
tftp-root=${TFTP_ROOT}
tftp-no-fail

dhcp-match=set:efi-x64,option:client-arch,7
dhcp-boot=tag:efi-x64,ipxeboot/x86_64-sb/snponly-shim.efi
EOF
echo "Wrote ${SPIKE_DIR}/dnsmasq.conf"
if ! grep -qF 'dhcp-boot=tag:efi-x64,ipxeboot/x86_64-sb/snponly-shim.efi' "${SPIKE_DIR}/dnsmasq.conf"; then
    echo "ABORT: dhcp-boot does not match the proven baseline value."
    exit 1
fi
echo "OK: dhcp-boot is byte-identical to the proven baseline."
echo

echo "== Step 9b: pre-create log/lease files, owner dnsmasq, group brener, mode 0640 =="
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.log"
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.leases"
echo

echo "== Step 9c: validate readability/traversal for the dnsmasq runtime user =="
sudo -u dnsmasq test -x "${SPIKE_DIR}" && echo "OK: ${SPIKE_DIR} traversable" || { echo "ABORT: not traversable"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}" && echo "OK: ${TFTP_ROOT} traversable" || { echo "ABORT: not traversable"; exit 1; }
sudo -u dnsmasq test -r "${ROOT_IPXE_EFI}" && echo "OK: root ipxe.efi readable" || { echo "ABORT: not readable"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/ipxeboot" && echo "OK: ipxeboot/ traversable" || { echo "ABORT: not traversable"; exit 1; }
sudo -u dnsmasq test -x "${SHIM_DIR}" && echo "OK: ${SHIM_DIR} traversable" || { echo "ABORT: not traversable"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/snponly-shim.efi" && echo "OK: snponly-shim.efi readable" || { echo "ABORT: not readable"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/snponly.efi" && echo "OK: sibling snponly.efi readable" || { echo "ABORT: not readable"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/autoexec.ipxe" && echo "OK: autoexec.ipxe readable" || { echo "ABORT: not readable"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.log" && echo "OK: dnsmasq.log writable" || { echo "ABORT: not writable"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.leases" && echo "OK: dnsmasq.leases writable" || { echo "ABORT: not writable"; exit 1; }
echo

echo "== Step 9d: validate dnsmasq config syntax without binding any socket =="
sudo dnsmasq --test --conf-file="${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 9e: final pre-flight before starting dnsmasq =="
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: unexpected DHCP/TFTP listener present just before start."
    exit 1
fi
if ! kill -0 "${HTTP_PID}" 2>/dev/null; then
    echo "ABORT: HTTP server (pid ${HTTP_PID}) is no longer running."
    exit 1
fi
echo "OK: still no DHCP/TFTP/PXE listener; HTTP server (pid ${HTTP_PID}) still alive."
echo

echo "== Step 9f: start dnsmasq in the background (10-minute ceiling) =="
sudo timeout 600 dnsmasq -d --conf-file="${SPIKE_DIR}/dnsmasq.conf" \
    > "${SPIKE_DIR}/dnsmasq-stdout.log" 2>&1 &
DNSMASQ_PID=$!

echo "== Step 10: start packet capture in the background - ALL traffic on ${IFACE}, no protocol filter, no snaplen reduction =="
sudo tcpdump -ni "${IFACE}" -e -vvv -Z brener \
    -w "${CAPTURE_PCAP}" \
    > "${CAPTURE_LOG}" 2>&1 &
TCPDUMP_PID=$!

cleanup_harness() {
    echo
    echo "== Stopping throwaway harness processes =="
    if kill -0 "${TCPDUMP_PID}" 2>/dev/null; then
        sudo kill -INT "${TCPDUMP_PID}" 2>/dev/null
        wait "${TCPDUMP_PID}" 2>/dev/null || true
        echo "Stopped tcpdump (pid ${TCPDUMP_PID})."
    fi
    if kill -0 "${DNSMASQ_PID}" 2>/dev/null; then
        sudo kill -TERM "${DNSMASQ_PID}" 2>/dev/null
        wait "${DNSMASQ_PID}" 2>/dev/null || true
        echo "Stopped dnsmasq (pid ${DNSMASQ_PID})."
    fi
    if kill -0 "${HTTP_PID}" 2>/dev/null; then
        kill -TERM "${HTTP_PID}" 2>/dev/null
        wait "${HTTP_PID}" 2>/dev/null || true
        echo "Stopped HTTP server (pid ${HTTP_PID})."
    fi
    echo
    echo "== Evidence written to: =="
    echo "   ${CAPTURE_PCAP}"
    echo "   ${SPIKE_DIR}/dnsmasq.log"
    echo "   ${SPIKE_DIR}/sha256sums-tftp.txt"
    echo "   ${SPIKE_DIR}/sha256sums-http.txt"
    echo "   ${HTTP_LOG}"
    echo
    echo "Reconstruct: DORA, shim probe/transfer, revocation/cert RRQs, /ipxe.efi,"
    echo "autoexec.ipxe, GET /wimboot + /BCD + /boot.sdi (reassemble+hash all three"
    echo "small bodies from pcap), GET /boot.wim (Content-Length + TCP sequence"
    echo "arithmetic + imgstat; full reassembly only as a fallback escalation),"
    echo "wimboot's own banner/WIM-processing/BCD-boot.sdi-usage lines, Secure Boot"
    echo "handling of bootmgfw_EX.efi vs bootmgfw.efi, wimboot's own pause, the"
    echo "single owner keypress, and whatever Windows Boot Manager/WinPE/wpeinit/"
    echo "cmd.exe evidence follows. Do NOT infer wimboot or WinPE success merely"
    echo "from completed HTTP transfers."
    echo
    echo "Next: run issue53-phase9d-cleanup.sh to revert IP/firewall/NetworkManager/HTTP state."
}
trap cleanup_harness EXIT
trap 'exit 130' INT TERM

sleep 2
if ! kill -0 "${DNSMASQ_PID}" 2>/dev/null; then
    echo "ABORT: dnsmasq failed to start or exited immediately."
    exit 1
fi
if ! kill -0 "${TCPDUMP_PID}" 2>/dev/null; then
    echo "ABORT: tcpdump failed to start. Check ${CAPTURE_LOG}."
    exit 1
fi
if ! kill -0 "${HTTP_PID}" 2>/dev/null; then
    echo "ABORT: HTTP server is no longer running."
    exit 1
fi
if ! ss -lunp 2>/dev/null | grep -q ':67 '; then
    echo "ABORT: no listener on udp/67 after starting dnsmasq."
    exit 1
fi
if ! ss -lunp 2>/dev/null | grep -q ':69 '; then
    echo "ABORT: no listener on udp/69 after starting dnsmasq."
    exit 1
fi
if ! ss -Hltn 2>/dev/null | awk '{print $4}' | grep -qF "${ADDR_HOST}:${HTTP_PORT}"; then
    echo "ABORT: HTTP server no longer listening on ${ADDR_HOST}:${HTTP_PORT}."
    exit 1
fi
echo "== Step 11: all harness processes confirmed alive =="
echo "OK: dnsmasq (pid ${DNSMASQ_PID}), tcpdump (pid ${TCPDUMP_PID}), and the HTTP"
echo "    server (pid ${HTTP_PID}) are all alive; udp/67, udp/69, and"
echo "    ${ADDR_HOST}:${HTTP_PORT} are listening."
echo

echo "== Step 12: HARNESS READY =="
echo "HARNESS READY - trigger UEFI PXE IPv4 on the physical Endpoint now (MAC"
echo "e8:ff:1e:d6:2e:f5, expected to lease 192.168.99.66)."
echo "Expected boot file (DHCP option 67): ipxeboot/x86_64-sb/snponly-shim.efi"
echo "Expected HTTP GETs, in order: /wimboot, /BCD, /boot.sdi, /boot.wim"
echo
echo "REMINDER: this autoexec has NO iPXE-owned prompt. iPXE will download all"
echo "four assets and run 'boot' automatically. The ONLY intentional keypress is"
echo "at wimboot's OWN 'Press any key to continue booting...' line. Do not press"
echo "anything before that. After that one keypress, observe without further"
echo "intervention unless an unexpected interactive prompt must be captured."
echo "If any error appears: STOP, photograph/transcribe it, do not retry or repair."
echo
echo "== Step 13: this script does NOT trigger PXE itself. Waiting for dnsmasq to"
echo "   exit (Ctrl-C to stop early once the outcome is recorded, 10-minute"
echo "   ceiling otherwise)... =="
wait "${DNSMASQ_PID}"
