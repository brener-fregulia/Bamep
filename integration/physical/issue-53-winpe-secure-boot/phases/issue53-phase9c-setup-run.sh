#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 9c - THROWAWAY Spike operator script.
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
# Baseline (Phase 9b, physically executed and evidence-preserved at
# /var/tmp/bamep-issue53-phase9b-http-probe/):
# ---------------------------------------------------------------------
#
# UEFI Secure Boot Enabled/Active/Standard
#   -> official iPXE v2.0.0 shim -> shim fallback /ipxe.efi
#   -> exact official snponly.efi bytes served under /ipxe.efi
#   -> iPXE v2.0.0 (g12798) -> automatic ipxeboot/x86_64-sb/autoexec.ipxe
#   -> efi/SecureBoot:hex = 01
#   -> explicit HTTP GET from iPXE, byte-verified delivery (SHA-256
#      reassembled from raw pcap matched the pinned asset exactly)
#   -> iPXE shell
#
# Phase 9b classification: progressed to a new meaningful boundary.
# Overall Issue #53: B - unchanged.
#
# ---------------------------------------------------------------------
# Phase 9c question (single new variable: wimboot + stock boot.wim,
# stopping at wimboot's OWN pause, before Windows Boot Manager handoff):
# ---------------------------------------------------------------------
#
# Can the proven physical Secure Boot+iPXE+HTTP chain fetch official
# wimboot v2.9.0, fetch the exact pinned stock WinPE 10.0.26100 boot.wim,
# execute wimboot, let it process the WIM, and reach wimboot's OWN
# documented `pause` boundary - WITHOUT handing control to Windows Boot
# Manager?
#
# This phase does NOT attempt Windows Boot Manager execution, WinPE
# startup, wpeinit, or cmd.exe. The owner MUST NOT press a key at
# wimboot's own pause prompt - see the TWO-GATE explanation below.
#
# Artifact provenance (re-verified again below before staging anything):
#
#   Boot chain (IDENTICAL to Phase 9b): iPXE v2.0.0 (tag v2.0.0, commit
#   12798ec, published 2026-03-06), snponly-shim.efi (1038920 bytes,
#   sha256 83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885),
#   snponly.efi (295784 bytes, sha256
#   b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a,
#   staged twice: sibling path and root /ipxe.efi fallback).
#
#   wimboot v2.9.0 (github.com/ipxe/ipxe/wimboot tag v2.9.0, published
#   2025-11-17T23:58:15Z - 2025, not 2026): official asset "wimboot"
#   (x86-64 BIOS/UEFI hybrid, NOT wimboot.i386/wimboot.arm64):
#     size 76064 bytes, sha256
#     5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3
#     (matches the GitHub Release API digest exactly, re-verified in the
#     prior read-only provenance session). Inspected directly (not
#     assumed from docs): the binary is a genuine hybrid Linux
#     bzImage/PE32+ x86-64 artifact, dual Authenticode-signed by
#     Microsoft - one signature chains through "Microsoft Corporation
#     UEFI CA 2011" (the same chain already proven accepted by this
#     physical firmware via the Fedora shim and shimx64.efi), the other
#     through the newer "Microsoft UEFI CA 2023" (matching the v2.9.0
#     changelog's bootmgfw_EX.efi selection feature). Whether iPXE's
#     `kernel` command actually invokes any signature check when loading
#     this hybrid artifact via the bzImage code path (as opposed to a
#     native EFI LoadImage() call) was NOT conclusively established from
#     iPXE source inspection alone - this phase's physical execution is
#     the empirical test. Do not pre-classify expected failure wording;
#     capture whatever the firmware/iPXE/wimboot actually show.
#
#   boot.wim (retained stock WinPE, Windows source provenance
#   C:\BamepSpike\winpe_media\amd64\media\sources\boot.wim; DISM metadata:
#   index 1, "Microsoft Windows PE (amd64)", x64, version 10.0.26100,
#   edition WindowsPE, 3652 directories, 17276 files, expanded size
#   2009251937 bytes, en-US):
#     size 340134390 bytes, sha256
#     fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197
#   This script only COPIES this file (install -m 0644); it is never
#   mounted, rebuilt, exported, recompressed, or otherwise mutated.
#
# Upstream semantics independently re-verified (not taken on faith):
#   - https://ipxe.org/cmd/kernel: "kernel [--name <name>]
#     [--timeout <timeout>] <uri|image> [<arguments>...] ... Any
#     remaining arguments will be passed directly to the image." This
#     confirms `kernel http://.../wimboot pause` passes the literal
#     argument "pause" to wimboot, matching wimboot's own documented
#     `pause[=quiet]: Show info and wait for keypress` option.
#   - https://ipxe.org/wimboot: the official quickstart is exactly
#     `kernel wimboot; initrd sources/boot.wim boot.wim; boot` - i.e.
#     wimboot + boot.wim ALONE (no BCD/boot.sdi/bootmgfw.efi supplied
#     explicitly) is the documented, supported path, relying entirely on
#     wimboot's automatic extraction/selection from the WIM. This phase
#     deliberately withholds all such pre-extracted Windows files so a
#     failure cannot be masked.
#   - https://ipxe.org/cmd/prompt: "If no timeout is explicitly
#     specified, or if a zero timeout is specified, then iPXE will wait
#     indefinitely," and succeeds once any key is pressed - a bare
#     `prompt <text>` (no --timeout) therefore blocks forever and does
#     NOT need a trailing `||` guard (unlike a timeout-bounded prompt).
#   - https://ipxe.org/scripting: a bare '#' anywhere on a line begins a
#     trailing comment (confirmed root cause of the Phase 9a2 truncation
#     bug). This script's autoexec uses "Issue 53" (no '#') and contains
#     '#' only on the mandatory "#!ipxe" magic first line.
#
# TWO OPERATOR GATES - how to tell them apart (do not rely on timing):
#   Gate 1 - iPXE's OWN `prompt` command, BEFORE `boot` runs anything.
#     Text printed is OUR text, verbatim: "Bamep Phase 9c ready. Press
#     ONE key to start wimboot. Do NOT press again at wimboot pause."
#     This is iPXE's normal shell-prompt-style text rendering - nothing
#     Windows/wimboot-related has executed yet. Pressing a key HERE is
#     safe and expected exactly once.
#   Gate 2 - wimboot's OWN pause, which only exists AFTER `boot` has
#     handed control to wimboot. The owner-visible transition is
#     unmistakable: wimboot prints ITS OWN banner line "wimboot v2.9.0 --
#     Windows Imaging Format bootloader -- https://ipxe.org/wimboot",
#     "Command line: "pause"", then a sequence of "Using BCD via ...",
#     "Using boot.sdi via ...", "Using boot.wim via ...", "...found WIM
#     file boot.wim", and a boot-manager selection line (e.g. "...found
#     file "\Windows\Boot\EFI\bootmgfw.efi""). NONE of this text is ours
#     and none of it can appear before Gate 1's prompt has already been
#     answered and `boot` has run. The exact literal wording of wimboot's
#     keypress-wait line itself was not confirmed from available
#     documentation - treat ANY point after this file-listing block, and
#     before any line beginning "Entering...", as the boundary. DO NOT
#     PRESS A KEY once wimboot's own banner has appeared.
#
# Deliberately NOT staged in the HTTP root: BCD, boot.sdi, bootmgfw.efi,
# bootmgfw_EX.efi, bootx64.efi, boot.stl, fonts, policy files, WinPE
# scripts, or any other EFI binary - this phase tests wimboot's own
# automatic extraction/selection from the unmodified stock WIM, and a
# pre-supplied file would mask a real failure.
#
# Interpretation boundary: this probe does not select Bamep's final
# production network-delivery mechanism. Overall Issue #53 remains B
# regardless of this phase's outcome, since no functional WinPE shell is
# attempted. Use only "progressed to a new meaningful boundary" / "same
# effective boundary" / "failed earlier" / "harness prevented evaluation"
# for this individual phase - do not use A/B/C/D here, and do not label
# this phase simply SUCCESS/FAILURE.
#
# RAM assessment (Endpoint has 16 GB, boot.wim is 340134390 bytes
# compressed / 2009251937 bytes expanded per DISM): wimboot loads the WIM
# into RAM as a virtual disk backing (per the architecture doc, "allows
# Windows to reuse the memory that was used to hold the RAM disk image").
# Upstream does not document an exact peak-memory figure, so none is
# invented here; the COMPRESSED 340134390-byte transfer, plus iPXE's own
# modest runtime footprint, is a small fraction of 16 GB regardless of
# how wimboot represents the WIM (compressed staging + on-demand
# expansion, per general WIM/WOF design, not a wholesale eager expansion
# to 2 GB before Windows itself runs). 16 GB is not expected to be a
# constraint for reaching wimboot's own pause; remaining uncertainty is
# noted, not resolved here.
#
# HTTP/transfer estimate for boot.wim (340134390 bytes):
#   theoretical ideal 1GbE (125,000,000 bytes/s):      ~2.72 seconds
#   realistic sustained ~940 Mbit/s (~117.5 MB/s):     ~2.9 seconds
#   This Issue's own physical evidence (Phases 9a/9a2/9b) has shown
#   small-file transfers completing in well under 100ms with no
#   retransmissions, suggesting the link itself is not a bottleneck; the
#   above are calculated bounds, not measurements of this exact transfer.
#
# Evidence strategy for the large boot.wim: full unfiltered tcpdump is
# still used (disk space gate below confirms this is affordable), but
# full pcap body reassembly/hashing is NOT the primary proof for
# boot.wim (unlike the small wimboot binary, which IS fully
# reassembled/hashed from the pcap exactly as done for Phase 9b's
# probe.txt). For boot.wim, the primary evidence is: (1) a LOCAL
# pre-trigger HTTP fetch+hash from this Fedora host itself, proving the
# server serves the exact pinned bytes independent of any capture; (2)
# the physical Endpoint's own HTTP response Content-Length matching
# 340134390 exactly; (3) TCP sequence/ACK arithmetic across the full
# capture proving the complete byte count was transferred with no
# resets/retransmissions; (4) imgstat showing the expected image name
# and byte count. Full body reassembly from the (still-complete, not
# snaplen-truncated) pcap remains POSSIBLE as a fallback escalation if
# any of the above raise a discrepancy - it is just not required by
# default given the file's size. Do not claim byte-exact WIM delivery
# from Content-Length alone; use the layered evidence above together.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself. This script NEVER triggers the physical PXE boot itself - it
# stops after confirming the harness is ready and waits for the owner to
# power on/reboot the Endpoint by hand.

set -euo pipefail
export LC_ALL=C

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-phase9c-wimboot-pause"
TFTP_ROOT="${SPIKE_DIR}/tftp"
HTTP_ROOT="${SPIKE_DIR}/http"
SHIM_DIR="${TFTP_ROOT}/ipxeboot/x86_64-sb"
ROOT_IPXE_EFI="${TFTP_ROOT}/ipxe.efi"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
HTTP_PORT="8080"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-wimboot-pause.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"
HTTP_LOG="${SPIKE_DIR}/http-server.log"

PROVENANCE_DIR="/home/brener/.claude/jobs/07ec8ea7/tmp/issue53-phase9-provenance"
PHASE9C_PROVENANCE_DIR="/home/brener/.claude/jobs/07ec8ea7/tmp/issue53-phase9c-provenance"
ARCHIVE_SOURCE="${PROVENANCE_DIR}/ipxeboot.tar.gz"
SHIM_SOURCE="${PROVENANCE_DIR}/ipxeboot/x86_64-sb/snponly-shim.efi"
SNPONLY_SOURCE="${PROVENANCE_DIR}/ipxeboot/x86_64-sb/snponly.efi"
WIMBOOT_SOURCE="${PHASE9C_PROVENANCE_DIR}/wimboot"
BOOTWIM_SOURCE="${PHASE9C_PROVENANCE_DIR}/boot.wim"

EXPECTED_ARCHIVE_SHA256="01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1"
EXPECTED_ARCHIVE_SIZE="12002760"
EXPECTED_SHIM_SHA256="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
EXPECTED_SHIM_SIZE="1038920"
EXPECTED_SNPONLY_SHA256="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
EXPECTED_SNPONLY_SIZE="295784"
EXPECTED_AUTOEXEC_SHA256="afa7df7fa7cede84829231934e019e8ccfdaef831be9441df6d7154ff1ae5769"
EXPECTED_AUTOEXEC_SIZE="263"
EXPECTED_WIMBOOT_SHA256="5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3"
EXPECTED_WIMBOOT_SIZE="76064"
EXPECTED_BOOTWIM_SHA256="fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197"
EXPECTED_BOOTWIM_SIZE="340134390"

# Disk-space gate: require at least 2,000,000,000 bytes (2 GiB-ish) free,
# OR 5x the boot.wim size, whichever is larger. Rationale: this run adds
# roughly 1x boot.wim as a staged HTTP copy, plus up to ~1x boot.wim again
# in the full pcap (payload plus protocol overhead, with generous margin
# for any retransmission), plus negligible logs/metadata - so ~2-3x the
# WIM size covers the realistic worst case; 5x leaves a large additional
# safety margin. For our exact 340134390-byte WIM, 5x = 1700671950 bytes,
# which is BELOW the 2,000,000,000-byte floor, so the floor governs here;
# the floor exists so this gate stays meaningful even for a much smaller
# WIM in some future reuse of this script.
REQUIRED_FREE_BYTES=$(( 5 * EXPECTED_BOOTWIM_SIZE ))
MIN_FREE_FLOOR=2000000000
if [ "${REQUIRED_FREE_BYTES}" -lt "${MIN_FREE_FLOOR}" ]; then
    REQUIRED_FREE_BYTES="${MIN_FREE_FLOOR}"
fi

# HTTP-like-listener check, identical logic to the corrected Phase
# 9a/9a2/9b version (also mirrored into issue53-phase9c-cleanup.sh).
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

echo "== Bamep Issue #53 Phase 9c - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Question:  can wimboot v2.9.0 fetch/process the pinned stock boot.wim"
echo "              and reach its OWN pause boundary, before Windows Boot"
echo "              Manager handoff?"
echo "   This script NEVER triggers the physical PXE boot itself."
echo

# --------------------------------------------------------------------
# Gate group A: local, read-only artifact/hash/absence checks. ALL of
# these complete BEFORE this script touches network state or starts any
# DHCP/TFTP/HTTP service.
# --------------------------------------------------------------------

echo "-- Gate A0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-phase9c-cleanup.sh --purge"
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

echo "-- Gate A4: wimboot v2.9.0 source matches pinned provenance --"
[ -f "${WIMBOOT_SOURCE}" ] || { echo "ABORT: ${WIMBOOT_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${WIMBOOT_SOURCE}")" = "${EXPECTED_WIMBOOT_SIZE}" ] || { echo "ABORT: wimboot size mismatch."; exit 1; }
[ "$(sha256sum "${WIMBOOT_SOURCE}" | awk '{print $1}')" = "${EXPECTED_WIMBOOT_SHA256}" ] || { echo "ABORT: wimboot SHA-256 mismatch."; exit 1; }
echo "OK: ${WIMBOOT_SOURCE} matches pinned size (${EXPECTED_WIMBOOT_SIZE}) and SHA-256."
echo

echo "-- Gate A5: retained stock boot.wim matches pinned provenance (NOT mounted/rebuilt) --"
[ -f "${BOOTWIM_SOURCE}" ] || { echo "ABORT: ${BOOTWIM_SOURCE} not found."; exit 1; }
[ "$(stat -c '%s' "${BOOTWIM_SOURCE}")" = "${EXPECTED_BOOTWIM_SIZE}" ] || { echo "ABORT: boot.wim size mismatch."; exit 1; }
echo "Hashing ${EXPECTED_BOOTWIM_SIZE} bytes (this can take a few seconds)..."
[ "$(sha256sum "${BOOTWIM_SOURCE}" | awk '{print $1}')" = "${EXPECTED_BOOTWIM_SHA256}" ] || { echo "ABORT: boot.wim SHA-256 mismatch."; exit 1; }
echo "OK: ${BOOTWIM_SOURCE} matches pinned size (${EXPECTED_BOOTWIM_SIZE}) and SHA-256."
echo

echo "-- Gate A6: disk-space preflight (need >= ${REQUIRED_FREE_BYTES} bytes free on ${SPIKE_DIR%/*}'s filesystem) --"
AVAIL_BYTES="$(df --output=avail -B1 "$(dirname "${SPIKE_DIR}")" | tail -1 | tr -d ' ')"
echo "Available: ${AVAIL_BYTES} bytes; required: ${REQUIRED_FREE_BYTES} bytes."
if [ "${AVAIL_BYTES}" -lt "${REQUIRED_FREE_BYTES}" ]; then
    echo "ABORT: insufficient free disk space for a staged boot.wim copy + full pcap + logs."
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

echo "-- Step B2: copy the shim (unchanged from Phase 9a2/9b) --"
install -m 0644 "${SHIM_SOURCE}" "${SHIM_DIR}/snponly-shim.efi"
[ -L "${SHIM_DIR}/snponly-shim.efi" ] && { echo "ABORT: staged shim is a symlink."; exit 1; }
[ "$(stat -c '%s' "${SHIM_DIR}/snponly-shim.efi")" = "${EXPECTED_SHIM_SIZE}" ] || { echo "ABORT: staged shim size mismatch."; exit 1; }
[ "$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')" = "${EXPECTED_SHIM_SHA256}" ] || { echo "ABORT: staged shim SHA-256 mismatch."; exit 1; }
echo "OK: ${SHIM_DIR}/snponly-shim.efi staged and re-verified."
echo

echo "-- Step B2b: stage snponly.efi content TWICE, from the SAME source (unchanged from Phase 9a2/9b) --"
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

echo "-- Step B3: author the NEW Phase 9c autoexec.ipxe deterministically --"
echo "   Written with printf and explicit \\n escapes only. Uses 'Issue 53' (no"
echo "   '#'). Gate 1 (iPXE's own prompt) is textually distinct from wimboot's"
echo "   own pause banner - see header comment for the full explanation."
printf '#!ipxe\necho Bamep Issue 53 Phase 9c\nshow efi/SecureBoot\nkernel http://%s:%s/wimboot pause\ninitrd http://%s:%s/boot.wim boot.wim\nimgstat\nprompt Bamep Phase 9c ready. Press ONE key to start wimboot. Do NOT press again at wimboot pause.\nboot\n' \
    "${ADDR_HOST}" "${HTTP_PORT}" "${ADDR_HOST}" "${HTTP_PORT}" > "${SHIM_DIR}/autoexec.ipxe"
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

echo "-- Gate B3c: confirm /autoexec.ipxe does NOT exist at the TFTP root --"
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
echo "OK: no forbidden filename found under ${TFTP_ROOT} (wimboot/boot.wim/BCD/etc. belong"
echo "    only in the HTTP root, not TFTP)."
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

echo "-- Step H2: copy wimboot and boot.wim into the HTTP root (opaque copies, boot.wim never mounted/modified) --"
install -m 0644 "${WIMBOOT_SOURCE}" "${HTTP_ROOT}/wimboot"
echo "Copying boot.wim (${EXPECTED_BOOTWIM_SIZE} bytes)... this may take a moment."
install -m 0644 "${BOOTWIM_SOURCE}" "${HTTP_ROOT}/boot.wim"
echo "Copied both assets into ${HTTP_ROOT}"
echo

echo "-- Gate H2a: staged wimboot matches pinned size/hash, is a regular file --"
[ -L "${HTTP_ROOT}/wimboot" ] && { echo "ABORT: staged wimboot is a symlink."; exit 1; }
[ "$(stat -c '%s' "${HTTP_ROOT}/wimboot")" = "${EXPECTED_WIMBOOT_SIZE}" ] || { echo "ABORT: staged wimboot size mismatch."; exit 1; }
[ "$(sha256sum "${HTTP_ROOT}/wimboot" | awk '{print $1}')" = "${EXPECTED_WIMBOOT_SHA256}" ] || { echo "ABORT: staged wimboot SHA-256 mismatch."; exit 1; }
echo "OK: staged wimboot matches pinned size/SHA-256."
echo

echo "-- Gate H2b: staged boot.wim matches pinned size/hash, is a regular file --"
[ -L "${HTTP_ROOT}/boot.wim" ] && { echo "ABORT: staged boot.wim is a symlink."; exit 1; }
[ "$(stat -c '%s' "${HTTP_ROOT}/boot.wim")" = "${EXPECTED_BOOTWIM_SIZE}" ] || { echo "ABORT: staged boot.wim size mismatch."; exit 1; }
echo "Hashing staged boot.wim (${EXPECTED_BOOTWIM_SIZE} bytes)..."
[ "$(sha256sum "${HTTP_ROOT}/boot.wim" | awk '{print $1}')" = "${EXPECTED_BOOTWIM_SHA256}" ] || { echo "ABORT: staged boot.wim SHA-256 mismatch."; exit 1; }
echo "OK: staged boot.wim matches pinned size/SHA-256."
echo

echo "-- Gate H3: forbidden-content sweep over the HTTP root --"
echo "   Must contain ONLY wimboot and boot.wim - no BCD/boot.sdi/bootmgfw.efi/"
echo "   bootx64.efi/boot.stl/fonts/policy files/other EFI binaries."
FORBIDDEN_HTTP_HIT=0
while IFS= read -r -d '' f; do
    base="$(basename "$f")"
    lower="$(echo "$base" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        wimboot|boot.wim) : ;;
        *)
            echo "ABORT: unexpected file present in HTTP root: ${f}"
            FORBIDDEN_HTTP_HIT=1
            ;;
    esac
done < <(find "${HTTP_ROOT}" -type f -print0)
[ "${FORBIDDEN_HTTP_HIT}" = "0" ] || exit 1
echo "OK: HTTP root contains no unexpected file."
echo

echo "-- Gate H4: exact-listing gate - HTTP root contains EXACTLY wimboot and boot.wim --"
EXPECTED_HTTP_LIST="$(printf '%s\n' "boot.wim" "wimboot" | LC_ALL=C sort)"
ACTUAL_HTTP_LIST="$(cd "${HTTP_ROOT}" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)"
if [ "${ACTUAL_HTTP_LIST}" != "${EXPECTED_HTTP_LIST}" ]; then
    echo "ABORT: HTTP root does not contain exactly {wimboot, boot.wim}."
    diff <(printf '%s\n' "${EXPECTED_HTTP_LIST}") <(printf '%s\n' "${ACTUAL_HTTP_LIST}") || true
    exit 1
fi
echo "OK: HTTP root contains exactly: wimboot, boot.wim."
echo

echo "-- Gate H5: hash the staged HTTP assets --"
sha256sum "${HTTP_ROOT}/wimboot" "${HTTP_ROOT}/boot.wim" | tee "${SPIKE_DIR}/sha256sums-http.txt"
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

echo "-- Gate C2b: nothing already listens on ${ADDR_HOST}:${HTTP_PORT} (cannot exist before Step 4 anyway) --"
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

echo "== Step 8 / Gate H7: LOCAL pre-trigger HTTP verification of both staged assets =="
echo "   Fetches each asset from this Fedora host itself, over the just-assigned"
echo "   ${ADDR_HOST}, and hashes the response body - proving the HTTP server"
echo "   serves the exact pinned bytes BEFORE the physical Endpoint ever asks."
LOCAL_WIMBOOT_TMP="$(mktemp)"
LOCAL_BOOTWIM_TMP="$(mktemp)"
curl -sf -o "${LOCAL_WIMBOOT_TMP}" "http://${ADDR_HOST}:${HTTP_PORT}/wimboot"
curl -sf -o "${LOCAL_BOOTWIM_TMP}" "http://${ADDR_HOST}:${HTTP_PORT}/boot.wim"
LOCAL_WIMBOOT_SHA256="$(sha256sum "${LOCAL_WIMBOOT_TMP}" | awk '{print $1}')"
LOCAL_BOOTWIM_SHA256="$(sha256sum "${LOCAL_BOOTWIM_TMP}" | awk '{print $1}')"
rm -f "${LOCAL_WIMBOOT_TMP}" "${LOCAL_BOOTWIM_TMP}"
if [ "${LOCAL_WIMBOOT_SHA256}" != "${EXPECTED_WIMBOOT_SHA256}" ]; then
    echo "ABORT: local HTTP fetch of /wimboot does not match pinned SHA-256."
    sudo kill -TERM "${HTTP_PID}" 2>/dev/null || true
    exit 1
fi
if [ "${LOCAL_BOOTWIM_SHA256}" != "${EXPECTED_BOOTWIM_SHA256}" ]; then
    echo "ABORT: local HTTP fetch of /boot.wim does not match pinned SHA-256."
    sudo kill -TERM "${HTTP_PID}" 2>/dev/null || true
    exit 1
fi
echo "OK: local HTTP fetch of both /wimboot and /boot.wim matches pinned SHA-256 exactly."
echo

echo "== Step 9a: author dnsmasq.conf (DHCP+TFTP only; HTTP server above is separate) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 9c - throwaway harness. NOT production configuration.
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
    echo "ABORT: dhcp-boot does not match the proven Phase 9a2/9b baseline value."
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
    echo "autoexec.ipxe, GET /wimboot (reassemble+hash the small body from pcap),"
    echo "GET /boot.wim (Content-Length + TCP sequence arithmetic + imgstat; full"
    echo "reassembly only as a fallback if a discrepancy appears), wimboot's own"
    echo "banner/pause boundary, and the owner-visible screen result. Do NOT infer"
    echo "wimboot execution merely from a completed boot.wim transfer."
    echo
    echo "Next: run issue53-phase9c-cleanup.sh to revert IP/firewall/NetworkManager/HTTP state."
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
echo "Expected HTTP GETs: http://${ADDR_HOST}:${HTTP_PORT}/wimboot then /boot.wim"
echo
echo "REMINDER: press ONE key at the iPXE 'prompt' line. DO NOT press any key"
echo "once wimboot's own banner ('wimboot v2.9.0 -- Windows Imaging Format"
echo "bootloader...') appears - stop there."
echo
echo "== Step 13: this script does NOT trigger PXE itself. Waiting for dnsmasq to"
echo "   exit (Ctrl-C to stop early once you have observed the wimboot pause,"
echo "   10-minute ceiling otherwise)... =="
wait "${DNSMASQ_PID}"
