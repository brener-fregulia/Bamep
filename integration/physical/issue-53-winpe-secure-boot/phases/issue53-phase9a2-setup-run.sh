#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 9a2 - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review of every step below, and do not execute it automatically from
# any agent - it must be run interactively by the owner.
#
# This is still a Technical Spike. This script does NOT select Bamep's
# production network-delivery mechanism. This is NOT Phase 9b.
#
# ---------------------------------------------------------------------
# Baseline (Phase 9a, physically executed and evidence-preserved at
# /var/tmp/bamep-issue53-phase9a-ipxe-shim/):
# ---------------------------------------------------------------------
#
# UEFI Secure Boot Enabled/Active/Standard
#   -> official snponly-shim.efi transferred completely (twice, byte-exact,
#      708 TFTP blocks / 1,038,920 bytes each time)
#   -> shim executed (proven by post-transfer shim-originated activity, not
#      merely by the transfer itself)
#   -> shim requested, at the TFTP ROOT (not its own directory):
#        revocations_sku.efi      (not found)
#        revocations_sbat.efi     (not found)
#        shim_certificate_0.efi   (not found)
#        ipxe.efi                 (not found)
#        ipxe.efi                 (not found, retry ~2.003s later, matching
#                                   shim.c's own usleep(2000000) before its
#                                   DEFAULT_LOADER fallback attempt)
#   -> the documented sibling ipxeboot/x86_64-sb/snponly.efi was NEVER
#      requested; ipxeboot/x86_64-sb/autoexec.ipxe was NEVER requested.
#
# This reproduces the interoperability class documented in upstream
# ipxe/ipxe#1684 ("snponly-shimx64.efi loading ipxe.efi (instead of
# snponly.efi)", opened 2026-04-13, still OPEN at time of writing): on
# firmware that does not expose the shim's own LoadedImage FilePath, the
# shim cannot derive the sibling second-stage filename and falls back to
# its compiled DEFAULT_LOADER ("ipxe.efi") at the TFTP root. We did NOT
# directly instrument or prove that LoadedImage.FilePath is empty on this
# exact Endpoint - we reproduced the same externally observable fallback
# behavior reported upstream on different hardware.
#
# Phase 9a classification: progressed to a new meaningful boundary.
# Overall Issue #53: B - unchanged.
#
# ---------------------------------------------------------------------
# Phase 9a2 question (single variable change from the Phase 9a baseline):
# ---------------------------------------------------------------------
#
# If the exact /ipxe.efi path requested by the physically executing shim is
# satisfied with the already-pinned bytes of official snponly.efi, does the
# iPXE second stage execute, and what exact next boundary does it reach?
#
# No HTTP. No wimboot. No WinPE. No explicit iPXE dhcp/chain/kernel/initrd
# command. This phase does not manually type anything into an iPXE shell if
# one appears - it only observes.
#
# The ONLY functional addition versus the successful Phase 9a harness is a
# new regular file at the TFTP root:
#
#     <tftp-root>/ipxe.efi
#
# CRITICAL DISTINCTION, preserved explicitly in this script's own gates and
# echoed at staging time: the FILENAME is "ipxe.efi" only because that is
# the literal path the shim requests on this physical firmware (see the
# Phase 9a wire evidence above). The BYTES placed under that filename are
# NOT iPXE's own ipxe.efi build. They are the exact, already-pinned,
# official snponly.efi bytes:
#     size:   295784 bytes
#     sha256: b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
#
# This is a minimal physical reproduction of the workaround pattern
# upstream ipxe/ipxe#1684 itself documents (serve the expected snponly.efi
# content at the path the shim actually requests), not a new/independent
# hypothesis.
#
# Everything else from the successful Phase 9a baseline is kept identical:
# same DHCP option 67 (still points at snponly-shim.efi, NOT directly at
# /ipxe.efi - the shim is not bypassed), same
# ipxeboot/x86_64-sb/autoexec.ipxe content/hash, same network harness
# conventions, same artifact provenance. No /autoexec.ipxe is added at the
# TFTP root - if the newly-executing second stage requests autoexec.ipxe at
# the root, let that fail and observe; if it requests the existing sibling
# ipxeboot/x86_64-sb/autoexec.ipxe, let that succeed and observe. Do not
# intervene manually in either case.
#
# Artifact provenance (identical sources/hashes to Phase 9a; re-verified
# again below before staging anything):
#
#   Containing release: iPXE v2.0.0 (github.com/ipxe/ipxe, tag v2.0.0,
#   commit 12798ec, published 2026-03-06T16:16:13Z)
#     ipxeboot.tar.gz: size 12002760, sha256
#     01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1
#     (matches the GitHub Release API "digest" field for this asset exactly)
#
#   Shim (source: ipxeboot/x86_64-sb/snponly-shim.efi, a symlink to
#   shimx64.efi in the official archive; staged as a regular file):
#     size 1038920, sha256
#     83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885
#
#   snponly.efi (source: ipxeboot/x86_64-sb/snponly.efi in the official
#   archive; staged TWICE in this phase - once under its documented sibling
#   name, once under the root ipxe.efi fallback name - from the SAME
#   source, both re-verified independently below):
#     size 295784, sha256
#     b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
#
#   autoexec.ipxe (our own authorship, unchanged from Phase 9a, only ever
#   staged at ipxeboot/x86_64-sb/autoexec.ipxe - never aliased to the root):
#     size 83, sha256
#     62bcdab34966b09f8a03da167b39ce77e2ff8173e6a24978b96cab9d805a0b69
#
# Deliberately NOT staged: /autoexec.ipxe (root), ipxe-shim.efi, shimx64.efi
# under its own name, wimboot, BCD, boot.sdi, boot.wim, any WinPE asset,
# GRUB, the Fedora shim, and no second/duplicate ipxe.efi anywhere other
# than the one intentional root copy.
#
# Interpretation boundary: this probe does not select Bamep's final
# production network-delivery mechanism. Overall Issue #53 remains B
# regardless of this phase's outcome, since no functional WinPE shell is
# attempted here. Use only "progressed to a new meaningful boundary" /
# "same effective boundary" / "failed earlier" / "harness prevented
# evaluation" for this individual phase - do not use A/B/C/D here.
#
# Observability discipline (unchanged from Phase 9a): do not infer
# successful iPXE second-stage execution merely because /ipxe.efi
# transferred completely. Proof of execution requires downstream behavior
# attributable to iPXE itself: an iPXE banner, an autoexec.ipxe request
# (root or sibling), iPXE-specific console output, an iPXE shell prompt, or
# iPXE-originated network activity (e.g. "No more network devices" is
# itself iPXE-originated output and counts as execution evidence, distinct
# from a NIC/SNP driver failure inside a running iPXE).
#
# Safety note: this script stages only three opaque, hash-pinned, official,
# already-provenanced EFI-content copies (two of them byte-identical
# snponly.efi copies under two different filenames) and one plaintext
# script of our own authorship, unchanged from Phase 9a. It authors no
# Windows/BCD/WIM content. It does not configure or start any HTTP service
# - only DHCP+TFTP. It does not configure or execute any disk-writing,
# partition, format, install, or destructive storage action. The Endpoint
# remains physically diskless.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself.

set -euo pipefail

# Force a fixed, locale-independent collation/formatting for every command
# this script runs (sort, comparisons, etc). See Phase 9a's Gate B7 postmortem.
export LC_ALL=C

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-phase9a2-ipxe-fallback"
TFTP_ROOT="${SPIKE_DIR}/tftp"
SHIM_DIR="${TFTP_ROOT}/ipxeboot/x86_64-sb"
ROOT_IPXE_EFI="${TFTP_ROOT}/ipxe.efi"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-shim-fallback.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"

PROVENANCE_DIR="/home/brener/.claude/jobs/07ec8ea7/tmp/issue53-phase9-provenance"
ARCHIVE_SOURCE="${PROVENANCE_DIR}/ipxeboot.tar.gz"
SHIM_SOURCE="${PROVENANCE_DIR}/ipxeboot/x86_64-sb/snponly-shim.efi"
SNPONLY_SOURCE="${PROVENANCE_DIR}/ipxeboot/x86_64-sb/snponly.efi"

EXPECTED_ARCHIVE_SHA256="01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1"
EXPECTED_ARCHIVE_SIZE="12002760"
EXPECTED_SHIM_SHA256="83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885"
EXPECTED_SHIM_SIZE="1038920"
EXPECTED_SNPONLY_SHA256="b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a"
EXPECTED_SNPONLY_SIZE="295784"
EXPECTED_AUTOEXEC_SHA256="62bcdab34966b09f8a03da167b39ce77e2ff8173e6a24978b96cab9d805a0b69"
EXPECTED_AUTOEXEC_SIZE="83"

# HTTP-like-listener check, shared by Gate C2 and the final pre-flight check.
# Identical logic to the corrected Phase 9a version (also mirrored into
# issue53-phase9a2-cleanup.sh so setup and cleanup agree).
#
# Return-code convention (matches the function's name directly, no double
# negative at call sites): exit 0 ("true") means a conflict WAS found;
# exit 1 ("false") means no conflict. Call as `if http_like_listener_...; then`.
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
                echo "CONFLICT: HTTP-like listener bound to ${addr}:${port} -"
                echo "  this bind would also accept traffic on ${IFACE}/${ADDR_HOST}."
                conflict_found=0
                ;;
            *)
                echo "benign, interface-specific (does not reach ${IFACE}/${ADDR_HOST}): ${addr}:${port}"
                ;;
        esac
    done < <(ss -Hltn 2>/dev/null | awk '{print $4}')
    return "${conflict_found}"
}

echo "== Bamep Issue #53 Phase 9a2 - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Question:  with the shim's exact requested fallback path /ipxe.efi"
echo "              satisfied by the pinned official snponly.efi bytes, does"
echo "              the iPXE second stage execute, and what boundary next?"
echo "   No wimboot. No WinPE. No HTTP. No explicit iPXE network command."
echo "   No manual typing into any iPXE shell that may appear."
echo

# --------------------------------------------------------------------
# Gate group A: local, read-only artifact/hash/absence checks.
# ALL of these must complete BEFORE this script touches network state
# (NetworkManager, ip addr, firewalld) or starts any DHCP/TFTP service.
# --------------------------------------------------------------------

echo "-- Gate A0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-phase9a2-cleanup.sh --purge"
    exit 1
fi
echo "OK: no stale ${SPIKE_DIR} present."
echo

echo "-- Gate A1: official release archive present and matches pinned provenance --"
if [ ! -f "${ARCHIVE_SOURCE}" ]; then
    echo "ABORT: ${ARCHIVE_SOURCE} not found. Re-fetch and re-verify before proceeding."
    exit 1
fi
ACTUAL_ARCHIVE_SIZE="$(stat -c '%s' "${ARCHIVE_SOURCE}")"
ACTUAL_ARCHIVE_SHA256="$(sha256sum "${ARCHIVE_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_ARCHIVE_SIZE}" != "${EXPECTED_ARCHIVE_SIZE}" ]; then
    echo "ABORT: ${ARCHIVE_SOURCE} size ${ACTUAL_ARCHIVE_SIZE} != expected ${EXPECTED_ARCHIVE_SIZE}."
    exit 1
fi
if [ "${ACTUAL_ARCHIVE_SHA256}" != "${EXPECTED_ARCHIVE_SHA256}" ]; then
    echo "ABORT: ${ARCHIVE_SOURCE} SHA-256 ${ACTUAL_ARCHIVE_SHA256} != expected ${EXPECTED_ARCHIVE_SHA256}."
    exit 1
fi
echo "OK: ${ARCHIVE_SOURCE} matches pinned size (${ACTUAL_ARCHIVE_SIZE}) and SHA-256 (${ACTUAL_ARCHIVE_SHA256})."
echo

echo "-- Gate A2: shim source (snponly-shim.efi / shimx64.efi content) matches pinned provenance --"
echo "   snponly-shim.efi is a symlink to shimx64.efi in the official bundle; this"
echo "   gate measures the DEREFERENCED target content, not the symlink itself."
if [ ! -f "${SHIM_SOURCE}" ]; then
    echo "ABORT: ${SHIM_SOURCE} not found (or is a symlink to a missing/non-regular target)."
    exit 1
fi
if [ -L "${SHIM_SOURCE}" ]; then
    echo "confirmed symlink: $(readlink "${SHIM_SOURCE}") (resolves to $(readlink -f "${SHIM_SOURCE}"))"
fi
ACTUAL_SHIM_SIZE="$(stat -Lc '%s' "${SHIM_SOURCE}")"
ACTUAL_SHIM_SHA256="$(sha256sum "${SHIM_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_SHIM_SIZE}" != "${EXPECTED_SHIM_SIZE}" ] || [ "${ACTUAL_SHIM_SHA256}" != "${EXPECTED_SHIM_SHA256}" ]; then
    echo "ABORT: ${SHIM_SOURCE} dereferenced size/SHA-256 does not match pinned provenance."
    echo "  size:   ${ACTUAL_SHIM_SIZE} (expected ${EXPECTED_SHIM_SIZE})"
    echo "  sha256: ${ACTUAL_SHIM_SHA256} (expected ${EXPECTED_SHIM_SHA256})"
    exit 1
fi
echo "OK: ${SHIM_SOURCE} dereferences to pinned shimx64.efi content (${ACTUAL_SHIM_SIZE} bytes, ${ACTUAL_SHIM_SHA256})."
echo

echo "-- Gate A3: snponly.efi source matches pinned provenance --"
echo "   This SAME source will be staged TWICE below: once as the documented"
echo "   sibling ipxeboot/x86_64-sb/snponly.efi, once as the root ipxe.efi"
echo "   fallback-path content. Both destinations are re-verified independently"
echo "   after staging (Gate B2c)."
if [ ! -f "${SNPONLY_SOURCE}" ]; then
    echo "ABORT: ${SNPONLY_SOURCE} not found."
    exit 1
fi
ACTUAL_SNPONLY_SIZE="$(stat -c '%s' "${SNPONLY_SOURCE}")"
ACTUAL_SNPONLY_SHA256="$(sha256sum "${SNPONLY_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_SNPONLY_SIZE}" != "${EXPECTED_SNPONLY_SIZE}" ] || [ "${ACTUAL_SNPONLY_SHA256}" != "${EXPECTED_SNPONLY_SHA256}" ]; then
    echo "ABORT: ${SNPONLY_SOURCE} size/SHA-256 does not match pinned provenance."
    exit 1
fi
echo "OK: ${SNPONLY_SOURCE} matches pinned size (${ACTUAL_SNPONLY_SIZE}) and SHA-256 (${ACTUAL_SNPONLY_SHA256})."
echo

echo "== Gate group B: stage the TFTP tree (local filesystem only, no network mutation yet) =="
echo

echo "-- Step B1: create the Spike directory tree (owned by brener, no sudo) --"
mkdir -p "${SHIM_DIR}"
echo "Created ${SHIM_DIR} (and ${TFTP_ROOT} as its parent)"
echo

echo "-- Step B2: copy the shim (unchanged from Phase 9a) --"
install -m 0644 "${SHIM_SOURCE}" "${SHIM_DIR}/snponly-shim.efi"
echo "Copied ${SHIM_SOURCE} -> ${SHIM_DIR}/snponly-shim.efi"

echo "-- Gate B2a: staged snponly-shim.efi is a REGULAR FILE containing the dereferenced bytes --"
if [ -L "${SHIM_DIR}/snponly-shim.efi" ] || [ ! -f "${SHIM_DIR}/snponly-shim.efi" ]; then
    echo "ABORT: staged ${SHIM_DIR}/snponly-shim.efi is not a plain regular file."
    exit 1
fi
STAGED_SHIM_SIZE="$(stat -c '%s' "${SHIM_DIR}/snponly-shim.efi")"
STAGED_SHIM_SHA256="$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')"
if [ "${STAGED_SHIM_SIZE}" != "${EXPECTED_SHIM_SIZE}" ] || [ "${STAGED_SHIM_SHA256}" != "${EXPECTED_SHIM_SHA256}" ]; then
    echo "ABORT: staged snponly-shim.efi size/SHA-256 does not match pinned provenance."
    exit 1
fi
echo "OK: ${SHIM_DIR}/snponly-shim.efi is a regular file, ${STAGED_SHIM_SIZE} bytes, ${STAGED_SHIM_SHA256}."
echo

echo "-- Step B2b: stage snponly.efi content TWICE, from the SAME source, at TWO paths --"
echo "   1) ipxeboot/x86_64-sb/snponly.efi - the documented sibling path (kept from Phase 9a)"
echo "   2) ${ROOT_IPXE_EFI} - the ONE intentional new file this phase adds."
echo "   IMPORTANT: the filename 'ipxe.efi' at the TFTP root is used ONLY because"
echo "   that is the literal path the shim requested on this physical firmware in"
echo "   Phase 9a. The bytes served under that filename are NOT iPXE's ipxe.efi"
echo "   build - they are the exact official snponly.efi bytes. This is a"
echo "   deliberate, explicit workaround reproduction (see ipxe/ipxe#1684), not a"
echo "   mislabeled artifact."
echo "   Both copies are independent regular-file copies (install(1) semantics -"
echo "   not a symlink, not a hardlink); verified explicitly below (Gate B2c)."
install -m 0644 "${SNPONLY_SOURCE}" "${SHIM_DIR}/snponly.efi"
echo "Copied ${SNPONLY_SOURCE} -> ${SHIM_DIR}/snponly.efi (documented sibling path)"
install -m 0644 "${SNPONLY_SOURCE}" "${ROOT_IPXE_EFI}"
echo "Copied ${SNPONLY_SOURCE} -> ${ROOT_IPXE_EFI} (root fallback path; SAME bytes, DIFFERENT filename)"
echo

echo "-- Gate B2c: both snponly.efi copies are independent regular files with identical,"
echo "   pinned size/hash - and are NOT the same inode (proving an actual copy, not a"
echo "   hardlink or symlink) --"
for f in "${SHIM_DIR}/snponly.efi" "${ROOT_IPXE_EFI}"; do
    if [ -L "${f}" ] || [ ! -f "${f}" ]; then
        echo "ABORT: ${f} is not a plain regular file."
        exit 1
    fi
done
SIBLING_SIZE="$(stat -c '%s' "${SHIM_DIR}/snponly.efi")"
SIBLING_SHA256="$(sha256sum "${SHIM_DIR}/snponly.efi" | awk '{print $1}')"
SIBLING_INODE="$(stat -c '%i' "${SHIM_DIR}/snponly.efi")"
ROOT_SIZE="$(stat -c '%s' "${ROOT_IPXE_EFI}")"
ROOT_SHA256="$(sha256sum "${ROOT_IPXE_EFI}" | awk '{print $1}')"
ROOT_INODE="$(stat -c '%i' "${ROOT_IPXE_EFI}")"
echo "sibling ipxeboot/x86_64-sb/snponly.efi: size=${SIBLING_SIZE} sha256=${SIBLING_SHA256} inode=${SIBLING_INODE}"
echo "root    ipxe.efi:                       size=${ROOT_SIZE} sha256=${ROOT_SHA256} inode=${ROOT_INODE}"
if [ "${SIBLING_SIZE}" != "${EXPECTED_SNPONLY_SIZE}" ] || [ "${SIBLING_SHA256}" != "${EXPECTED_SNPONLY_SHA256}" ]; then
    echo "ABORT: staged sibling snponly.efi does not match pinned provenance."
    exit 1
fi
if [ "${ROOT_SIZE}" != "${EXPECTED_SNPONLY_SIZE}" ] || [ "${ROOT_SHA256}" != "${EXPECTED_SNPONLY_SHA256}" ]; then
    echo "ABORT: staged root ipxe.efi does not match pinned snponly.efi provenance."
    exit 1
fi
if [ "${SIBLING_SHA256}" != "${ROOT_SHA256}" ]; then
    echo "ABORT: the two staged copies do not have identical content."
    exit 1
fi
if [ "${SIBLING_INODE}" = "${ROOT_INODE}" ]; then
    echo "ABORT: the two staged copies share the same inode (${SIBLING_INODE}) - this"
    echo "  would mean a hardlink was created instead of an independent copy."
    exit 1
fi
echo "OK: both copies are independent regular files (different inodes: ${SIBLING_INODE} vs"
echo "    ${ROOT_INODE}), both ${ROOT_SIZE} bytes, both SHA-256 ${ROOT_SHA256} -"
echo "    i.e. official snponly.efi bytes, self-contained and duplicated on disk,"
echo "    one served under its documented name and one under the shim's requested"
echo "    fallback name."
echo

echo "-- Step B3: author autoexec.ipxe deterministically, ONLY at the documented sibling path --"
echo "   Unchanged from Phase 9a. No /autoexec.ipxe is added at the TFTP root - we"
echo "   want to observe, unmodified, which path the newly-executing second stage"
echo "   actually requests."
printf '#!ipxe\necho Bamep Issue #53 Phase 9a probe from ${cwuri}\nshow efi/SecureBoot\nshell\n' \
    > "${SHIM_DIR}/autoexec.ipxe"
echo "Wrote ${SHIM_DIR}/autoexec.ipxe"
cat -A "${SHIM_DIR}/autoexec.ipxe"
echo

echo "-- Gate B3a: autoexec.ipxe matches the pinned deterministic size/hash exactly --"
ACTUAL_AUTOEXEC_SIZE="$(stat -c '%s' "${SHIM_DIR}/autoexec.ipxe")"
ACTUAL_AUTOEXEC_SHA256="$(sha256sum "${SHIM_DIR}/autoexec.ipxe" | awk '{print $1}')"
if [ "${ACTUAL_AUTOEXEC_SIZE}" != "${EXPECTED_AUTOEXEC_SIZE}" ] || [ "${ACTUAL_AUTOEXEC_SHA256}" != "${EXPECTED_AUTOEXEC_SHA256}" ]; then
    echo "ABORT: autoexec.ipxe size/SHA-256 does not match the pinned deterministic value."
    exit 1
fi
echo "OK: autoexec.ipxe matches pinned size (${ACTUAL_AUTOEXEC_SIZE}) and SHA-256 (${ACTUAL_AUTOEXEC_SHA256})."
echo

echo "-- Gate B3b: confirm /autoexec.ipxe does NOT exist at the TFTP root --"
if [ -e "${TFTP_ROOT}/autoexec.ipxe" ]; then
    echo "ABORT: ${TFTP_ROOT}/autoexec.ipxe exists. This run must only serve autoexec.ipxe"
    echo "  from the documented sibling path, so the requested path itself is observable."
    exit 1
fi
echo "OK: no ${TFTP_ROOT}/autoexec.ipxe exists."
echo

echo "-- Gate B4: hash every staged file --"
sha256sum "${SHIM_DIR}/snponly-shim.efi" "${SHIM_DIR}/snponly.efi" "${SHIM_DIR}/autoexec.ipxe" "${ROOT_IPXE_EFI}" \
    | tee "${SPIKE_DIR}/sha256sums.txt"
echo

echo "-- Gate B5: re-pin all staged copies against recorded provenance (redundant, post-B4) --"
[ "$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')" = "${EXPECTED_SHIM_SHA256}" ] || { echo "ABORT: snponly-shim.efi re-pin failed."; exit 1; }
[ "$(sha256sum "${SHIM_DIR}/snponly.efi" | awk '{print $1}')" = "${EXPECTED_SNPONLY_SHA256}" ] || { echo "ABORT: sibling snponly.efi re-pin failed."; exit 1; }
[ "$(sha256sum "${ROOT_IPXE_EFI}" | awk '{print $1}')" = "${EXPECTED_SNPONLY_SHA256}" ] || { echo "ABORT: root ipxe.efi re-pin failed."; exit 1; }
[ "$(sha256sum "${SHIM_DIR}/autoexec.ipxe" | awk '{print $1}')" = "${EXPECTED_AUTOEXEC_SHA256}" ] || { echo "ABORT: autoexec.ipxe re-pin failed."; exit 1; }
echo "OK: all four staged files re-pinned successfully."
echo

echo "-- Gate B6: forbidden-name sweep (defense in depth) --"
echo "   'ipxe.efi' is intentionally EXCLUDED from this list in this phase only -"
echo "   see Gate B8 below for its dedicated, path-specific check."
FORBIDDEN_HIT=0
while IFS= read -r -d '' f; do
    lower="$(basename "$f" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        ipxe-shim.efi|shimx64.efi|shimaa64.efi|wimboot*|bcd|boot.sdi|boot.wim|*grub*|*fedora*|*winpe*|*bootmgfw*|*bootmgr*|*winload*|*.p7b|*.ttf)
            echo "ABORT: forbidden/unexpected file present: ${f}"
            FORBIDDEN_HIT=1
            ;;
    esac
done < <(find "${TFTP_ROOT}" -type f -print0)
if [ "${FORBIDDEN_HIT}" != "0" ]; then
    exit 1
fi
echo "OK: no forbidden filename (other than the one intentional root ipxe.efi) found"
echo "    anywhere under ${TFTP_ROOT}."
echo

echo "-- Gate B7: exact-listing gate - the staged tree contains EXACTLY the four expected files --"
echo "   Both sides generated, then normalized with LC_ALL=C sort. Order is irrelevant."
echo "   Neither side is deduplicated (no 'sort -u'), so a duplicate entry on either"
echo "   side changes the line count and still fails. Any missing or extra path fails."
EXPECTED_TREE_LIST="$(printf '%s\n' \
    "ipxe.efi" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly-shim.efi" \
    "ipxeboot/x86_64-sb/snponly.efi" \
    | LC_ALL=C sort)"
ACTUAL_TREE_LIST="$(cd "${TFTP_ROOT}" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)"
if [ "${ACTUAL_TREE_LIST}" != "${EXPECTED_TREE_LIST}" ]; then
    echo "ABORT: staged TFTP tree does not match the exact expected file list."
    echo "--- expected (LC_ALL=C sort) ---"
    echo "${EXPECTED_TREE_LIST}"
    echo "--- actual (LC_ALL=C sort) ---"
    echo "${ACTUAL_TREE_LIST}"
    echo "--- diff ---"
    diff <(printf '%s\n' "${EXPECTED_TREE_LIST}") <(printf '%s\n' "${ACTUAL_TREE_LIST}") || true
    exit 1
fi
echo "OK: staged TFTP tree contains exactly the four expected files, nothing else, no duplicates:"
echo "${ACTUAL_TREE_LIST}" | sed 's/^/    /'
echo

echo "-- Gate B8: exactly ONE ipxe.efi exists anywhere under the tree, and it is the intentional root one --"
IPXE_EFI_HITS="$(find "${TFTP_ROOT}" -iname 'ipxe.efi' | LC_ALL=C sort)"
IPXE_EFI_HIT_COUNT="$(printf '%s\n' "${IPXE_EFI_HITS}" | grep -c .)"
if [ "${IPXE_EFI_HIT_COUNT}" != "1" ]; then
    echo "ABORT: expected exactly one ipxe.efi under ${TFTP_ROOT}, found ${IPXE_EFI_HIT_COUNT}:"
    echo "${IPXE_EFI_HITS}"
    exit 1
fi
if [ "${IPXE_EFI_HITS}" != "${ROOT_IPXE_EFI}" ]; then
    echo "ABORT: the one ipxe.efi found is not at the intended root path."
    echo "  found:    ${IPXE_EFI_HITS}"
    echo "  expected: ${ROOT_IPXE_EFI}"
    exit 1
fi
echo "OK: exactly one ipxe.efi exists under ${TFTP_ROOT}, at the intended root path"
echo "    ${ROOT_IPXE_EFI}, containing the pinned snponly.efi bytes (see Gate B2c)."
echo

echo "== Gate group C: network pre-state checks (read-only) =="
echo

echo "-- Gate C1: no existing DHCP/TFTP/PXE listener --"
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: a DHCP/TFTP/PXE listener already exists. Investigate before proceeding."
    ss -lunp | grep -E ':67|:68|:69|:4011'
    exit 1
fi
echo "OK: no DHCP/TFTP/PXE listener present."
echo

echo "-- Gate C2: no HTTP-like listener bound to a path that reaches ${IFACE}/${ADDR_HOST} --"
echo "   Only a listener bound to 0.0.0.0, '*', ::, or ${ADDR_HOST} itself is a conflict."
echo "   A listener bound exclusively to another specific address (e.g. Tailscale/"
echo "   management) is left alone - this script must never disable such a service."
if http_like_listener_conflicts_with_lab_path; then
    echo "ABORT: an HTTP-like listener is bound to an address that would also accept"
    echo "  traffic on ${IFACE}/${ADDR_HOST}. Phase 9a2 must not run alongside one."
    exit 1
fi
echo "OK: no HTTP-like listener bound to a wildcard address or to ${ADDR_HOST}."
echo "    This script starts DHCP+TFTP only (dnsmasq); no HTTP server is configured."
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

echo "== All artifact/hash/absence/network-pre-state gates passed. =="
echo "== Only now does this script begin mutating network/runtime state. =="
echo

echo "== Step 1: take ${IFACE} out of NetworkManager's automatic management (runtime only) =="
sudo nmcli device set "${IFACE}" managed no
echo

echo "== Step 2: add temporary address (exact add, no flush) =="
sudo ip addr add "${ADDR}" dev "${IFACE}"
echo
ip -4 addr show "${IFACE}"
if ip -4 addr show 2>/dev/null | grep -F "${ADDR_HOST}" | grep -qv "${IFACE}"; then
    echo "ABORT: ${ADDR_HOST} leaked to another interface. Reverting."
    sudo ip addr del "${ADDR}" dev "${IFACE}"
    exit 1
fi
echo "OK: ${ADDR_HOST} only on ${IFACE}."
echo

echo "== Step 3: runtime-only firewalld scope for this throwaway isolated Spike =="
echo "   THROWAWAY SPIKE CONFIGURATION ONLY. Not the future Bamep appliance firewall design."
sudo firewall-cmd --zone=trusted --change-interface="${IFACE}"
echo

echo "== Step 4: author dnsmasq.conf (THROWAWAY SPIKE CONFIG ONLY; DHCP+TFTP only, no HTTP) =="
echo "   DHCP option 67 is IDENTICAL to Phase 9a - still points at the shim, not"
echo "   directly at /ipxe.efi. The shim is not bypassed."
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 9a2 - throwaway harness. NOT production configuration.
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
echo

echo "-- Gate D0: confirm dnsmasq.conf carries only DHCP/TFTP directives, no HTTP reference --"
if grep -iqE 'http|proxy' "${SPIKE_DIR}/dnsmasq.conf"; then
    echo "ABORT: dnsmasq.conf unexpectedly references HTTP/proxy."
    exit 1
fi
echo "OK: dnsmasq.conf contains only DHCP/TFTP directives."
echo

echo "-- Gate D1: confirm dhcp-boot is unchanged from Phase 9a (still targets the shim) --"
if ! grep -qF 'dhcp-boot=tag:efi-x64,ipxeboot/x86_64-sb/snponly-shim.efi' "${SPIKE_DIR}/dnsmasq.conf"; then
    echo "ABORT: dhcp-boot does not match the Phase 9a baseline value."
    exit 1
fi
echo "OK: dhcp-boot is byte-identical to the Phase 9a baseline (still the shim, not /ipxe.efi)."
echo

echo "== Step 5: pre-create log/lease files, owner dnsmasq, group brener, mode 0640 =="
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.log"
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.leases"
echo

echo "== Step 6: validate readability/traversal for the dnsmasq runtime user =="
sudo -u dnsmasq test -x "${SPIKE_DIR}" && echo "OK: ${SPIKE_DIR} traversable" || { echo "ABORT: ${SPIKE_DIR} not traversable"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}" && echo "OK: ${TFTP_ROOT} traversable" || { echo "ABORT: ${TFTP_ROOT} not traversable"; exit 1; }
sudo -u dnsmasq test -r "${ROOT_IPXE_EFI}" && echo "OK: root ipxe.efi readable" || { echo "ABORT: root ipxe.efi not readable"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/ipxeboot" && echo "OK: ipxeboot/ traversable" || { echo "ABORT: ipxeboot/ not traversable"; exit 1; }
sudo -u dnsmasq test -x "${SHIM_DIR}" && echo "OK: ${SHIM_DIR} traversable" || { echo "ABORT: ${SHIM_DIR} not traversable"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/snponly-shim.efi" && echo "OK: snponly-shim.efi readable" || { echo "ABORT: snponly-shim.efi not readable"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/snponly.efi" && echo "OK: sibling snponly.efi readable" || { echo "ABORT: sibling snponly.efi not readable"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/autoexec.ipxe" && echo "OK: autoexec.ipxe readable" || { echo "ABORT: autoexec.ipxe not readable"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.log" && echo "OK: dnsmasq.log writable" || { echo "ABORT: dnsmasq.log not writable"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.leases" && echo "OK: dnsmasq.leases writable" || { echo "ABORT: dnsmasq.leases not writable"; exit 1; }
echo

echo "== Step 7: validate dnsmasq config syntax without binding any socket =="
sudo dnsmasq --test --conf-file="${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 8: final pre-flight - still no listener before actually starting =="
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: unexpected listener present just before start."
    exit 1
fi
if http_like_listener_conflicts_with_lab_path >/dev/null; then
    echo "ABORT: unexpected HTTP-like listener bound to a lab-reachable address just before start."
    exit 1
else
    echo "OK: still no DHCP/TFTP/PXE listener, still no HTTP-like listener bound to a"
    echo "    wildcard address or ${ADDR_HOST}."
fi
echo

echo "== Step 9: start dnsmasq in the background (10-minute ceiling) =="
sudo timeout 600 dnsmasq -d --conf-file="${SPIKE_DIR}/dnsmasq.conf" \
    > "${SPIKE_DIR}/dnsmasq-stdout.log" 2>&1 &
DNSMASQ_PID=$!

echo "== Step 10: start packet capture in the background - ALL traffic on ${IFACE}, no protocol filter =="
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
    else
        echo "tcpdump (pid ${TCPDUMP_PID}) already stopped."
    fi
    if kill -0 "${DNSMASQ_PID}" 2>/dev/null; then
        sudo kill -TERM "${DNSMASQ_PID}" 2>/dev/null
        wait "${DNSMASQ_PID}" 2>/dev/null || true
        echo "Stopped dnsmasq (pid ${DNSMASQ_PID})."
    else
        echo "dnsmasq (pid ${DNSMASQ_PID}) already stopped."
    fi
    echo
    echo "== Evidence written to: =="
    echo "   ${CAPTURE_PCAP}"
    echo "   ${SPIKE_DIR}/dnsmasq.log"
    echo "   ${SPIKE_DIR}/dnsmasq.leases"
    echo "   ${SPIKE_DIR}/sha256sums.txt"
    echo "   ${SPIKE_DIR}/dnsmasq.conf"
    echo
    echo "Reconstruct from the pcap + dnsmasq.log, in order: DORA, snponly-shim.efi"
    echo "probe/transfer, shim-originated revocation/certificate RRQs, the /ipxe.efi"
    echo "RRQ and whether it transfers completely, whether an iPXE banner/autoexec"
    echo "request/shell/network-device message appears, exact path of any autoexec"
    echo "request (root vs sibling), and the owner-visible screen result. Do not infer"
    echo "second-stage execution from the /ipxe.efi transfer alone."
    echo
    echo "Next: run issue53-phase9a2-cleanup.sh to revert IP/firewall/NetworkManager state."
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

if ! ss -lunp 2>/dev/null | grep -q ':67 '; then
    echo "ABORT: no listener on udp/67 after starting dnsmasq."
    exit 1
fi
if ! ss -lunp 2>/dev/null | grep -q ':69 '; then
    echo "ABORT: no listener on udp/69 after starting dnsmasq."
    exit 1
fi
echo "OK: dnsmasq (pid ${DNSMASQ_PID}) and tcpdump (pid ${TCPDUMP_PID}) both alive;"
echo "    udp/67 and udp/69 listening. No HTTP listener started."
echo

echo "HARNESS READY - trigger UEFI PXE IPv4 now"
echo "Expected boot file (DHCP option 67): ipxeboot/x86_64-sb/snponly-shim.efi"
echo "Do NOT manually type commands into any iPXE shell that may appear - observe only."
echo

echo "Waiting for dnsmasq to exit (Ctrl-C to stop early, 10-minute ceiling otherwise)..."
wait "${DNSMASQ_PID}"
