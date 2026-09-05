#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 9a - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review of every step below, and do not execute it automatically from
# any agent - it must be run interactively by the owner.
#
# This is still a Technical Spike. This script does NOT select Bamep's
# production network-delivery mechanism.
#
# Question: does the official iPXE v2.0.0 Secure Boot chain
#   snponly-shim.efi -> snponly.efi -> automatic autoexec.ipxe -> iPXE shell
# progress on this physical, diskless UEFI x86-64 Endpoint while firmware
# Secure Boot remains Enabled/Active/Standard (System Mode: User)?
#
# This phase deliberately stops BEFORE wimboot, BEFORE WinPE, BEFORE any
# explicit iPXE "dhcp"/"chain"/"kernel"/"initrd" command, and BEFORE any
# HTTP service. Only TFTP is served. The autoexec.ipxe script issues no
# network command of its own - it only reads a UEFI variable and drops to
# an interactive iPXE shell.
#
# Known upstream risk (read independently from ipxe/ipxe issue #1684,
# "snponly-shimx64.efi loading ipxe.efi (instead of snponly.efi)", opened
# 2026-04-13, still OPEN): on some UEFI firmware, the shim cannot read its
# own LoadedImage FilePath and therefore cannot derive the sibling
# second-stage filename. When that happens the shim instead requests
# revocations_sku.efi, revocations_sbat.efi, shim_certificate_0.efi, and
# finally /ipxe.efi - all at the TFTP ROOT, not the shim's own directory.
# Every physical/VM device reported in that upstream issue as of this
# writing hit this fallback; none reported success. This run deliberately
# does NOT stage /ipxe.efi or any fallback copy, so the wire evidence can
# show unambiguously whether this Endpoint's firmware has the same
# limitation. If /ipxe.efi is requested and "file not found" is logged,
# that is a genuine firmware/shim interoperability result to preserve, not
# a harness mistake - do not add the fallback file to "fix" it in this run.
#
# Artifact provenance (already downloaded and hash-inspected in a prior
# read-only session; this script re-verifies every gate again before
# staging anything):
#
#   Containing release: iPXE v2.0.0 (github.com/ipxe/ipxe, tag v2.0.0,
#   commit 12798ec, published 2026-03-06T16:16:13Z)
#
#   Official release archive (already downloaded, re-verified below):
#     ipxeboot.tar.gz
#     size:   12002760 bytes
#     sha256: 01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1
#     (matches the GitHub Release API "digest" field for this asset exactly)
#
#   Phase 9a shim (source inside the official bundle:
#   ipxeboot/x86_64-sb/snponly-shim.efi, a symlink to shimx64.efi in the
#   official archive; staged here as a regular file named snponly-shim.efi,
#   content byte-identical):
#     size:   1038920 bytes
#     sha256: 83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885
#     Authenticode signer chain (extracted and inspected in the prior
#     read-only session): Microsoft Windows UEFI Driver Publisher <-
#     Microsoft Corporation UEFI CA 2011 <- Microsoft Corporation Third
#     Party Marketplace Root.
#
#   Phase 9a second stage (source inside the official bundle:
#   ipxeboot/x86_64-sb/snponly.efi):
#     size:   295784 bytes
#     sha256: b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
#     Authenticode signer chain: iPXE Secure Boot Automatic Code Signing
#     G1A <- iPXE Secure Boot Intermediate G1A <- iPXE Secure Boot CA
#     (verified by the shim itself via its embedded vendor certificate,
#     NOT by firmware db).
#
# autoexec.ipxe (authored by this script, not from upstream - content is
# the official ipxe.org/secboot example, network-free). Pinned exactly,
# same discipline as the two binary artifacts above - LF only, one final
# newline, written with printf and explicit \n escapes (no embedded
# literal newlines in the script source) to remove any ambiguity, then
# asserted against a fixed size/hash gate before anything else runs:
#     #!ipxe
#     echo Bamep Issue #53 Phase 9a probe from ${cwuri}
#     show efi/SecureBoot
#     shell
#     size:   83 bytes
#     sha256: 62bcdab34966b09f8a03da167b39ce77e2ff8173e6a24978b96cab9d805a0b69
#
# Deliberately NOT staged, by design - this phase must not pre-empt the
# very upstream-known fallback behavior it is trying to observe, and must
# not leak any asset from a different candidate mechanism: ipxe.efi,
# ipxe-shim.efi, shimx64.efi under its own name, wimboot, BCD, boot.sdi,
# boot.wim, any WinPE asset, GRUB, the Fedora shim. If a request for one of
# these appears on the wire, record the exact boundary and STOP - do not
# supply the requested asset in this same run.
#
# Interpretation boundary: this probe does not select Bamep's final
# production network-delivery mechanism. Overall Issue #53 remains
# classified B regardless of this phase's outcome, because no WinPE shell
# is attempted here. Use only "progressed to a new meaningful boundary" /
# "same effective boundary" / "failed earlier" / "harness prevented
# evaluation" for this individual phase - do not use A/B/C/D here.
#
# Observability correction versus earlier phases: a complete TFTP transfer
# of snponly-shim.efi does NOT by itself prove the firmware accepted or
# executed the shim under Secure Boot - UEFI firmware may download a
# complete EFI image over TFTP and only then perform Secure Boot
# verification and reject it. Do not report "firmware rejection" from an
# incomplete/absent transfer alone, and do not report "accepted" from a
# complete transfer alone. Evidence that the shim actually executed is
# POST-transfer, shim-originated network activity (revocations_sku.efi,
# revocations_sbat.efi, shim_certificate_0.efi, snponly.efi, or /ipxe.efi
# requests) and/or owner-visible screen output (iPXE banner, "show
# efi/SecureBoot" output, MokManager UI, or an explicit firmware rejection
# message). Report only what the pcap/log/screen actually show.
#
# Safety note: this script stages only two opaque, hash-pinned, official,
# already-provenanced EFI binaries and one plaintext script of our own
# authorship. It authors no Windows/BCD/WIM content of any kind. It does
# not configure or start any HTTP service - only DHCP+TFTP, same as every
# prior Issue #53 phase. It does not configure or execute any
# disk-writing, partition, format, install, or destructive storage action.
# The Endpoint remains physically diskless (M.2 SATA SSD and HDD removed,
# per #53's fixture).
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself.

set -euo pipefail

# Force a fixed, locale-independent collation/formatting for every command
# this script runs (sort, comparisons, etc). Without this, `sort` picks up
# the interactive operator's shell locale (e.g. LC_COLLATE=pt_BR.UTF-8),
# which can order punctuation differently from plain byte/ASCII order and
# make two textually-different-but-set-equal file listings compare unequal.
# This was the exact root cause of a prior pre-execution abort at Gate B7.
export LC_ALL=C

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-phase9a-ipxe-shim"
TFTP_ROOT="${SPIKE_DIR}/tftp"
SHIM_DIR="${TFTP_ROOT}/ipxeboot/x86_64-sb"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-shim.pcap"
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
#
# Phase 9a performs no HTTP of its own; the only safety property this check
# protects is: no existing listener could ALSO accept traffic on this
# Spike's physical/lab path (${IFACE}, and ${ADDR_HOST} once assigned).
#
# A listener bound EXCLUSIVELY to some other specific address (e.g. a
# Tailscale/management interface's own address, or loopback) cannot receive
# traffic arriving on ${IFACE}/${ADDR_HOST} and is therefore not a conflict -
# it must be left alone. Management/Tailscale services must never be
# disabled by this script.
#
# A listener bound to a wildcard address (0.0.0.0 / bare '*' / ::) - or to
# ${ADDR_HOST} itself - WOULD also accept traffic on ${IFACE} once ${ADDR}
# is assigned, and is therefore treated as a real conflict.
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

echo "== Bamep Issue #53 Phase 9a - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Question:  does snponly-shim.efi -> snponly.efi -> autoexec.ipxe execute"
echo "              under physical Secure Boot Enabled/Active/Standard?"
echo "   No wimboot. No WinPE. No HTTP. No explicit iPXE network command."
echo

# --------------------------------------------------------------------
# Gate group A: local, read-only artifact/hash/absence checks.
# Per owner instruction, ALL of these must complete BEFORE this script
# touches network state (NetworkManager, ip addr, firewalld) or starts
# any DHCP/TFTP service.
# --------------------------------------------------------------------

echo "-- Gate A0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-phase9a-cleanup.sh --purge"
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
echo "    (matches the GitHub Release API digest for ipxeboot.tar.gz v2.0.0 exactly)"
echo

echo "-- Gate A2: shim source (snponly-shim.efi / shimx64.efi content) matches pinned provenance --"
echo "   NOTE: in the official extracted bundle, snponly-shim.efi is a symlink to"
echo "   shimx64.efi. This gate must measure the DEREFERENCED target content, not"
echo "   the symlink itself - a bare 'stat -c %s' on a symlink reports the length"
echo "   of the link payload string (11 bytes for the literal text 'shimx64.efi'),"
echo "   not the size of the file it points to. sha256sum always opens and hashes"
echo "   the dereferenced target, so it was never affected by this class of bug."
if [ ! -f "${SHIM_SOURCE}" ]; then
    echo "ABORT: ${SHIM_SOURCE} not found (or is a symlink to a missing/non-regular target)."
    exit 1
fi
echo "-- provenance evidence: is it a symlink, and what does it resolve to? --"
if [ -L "${SHIM_SOURCE}" ]; then
    echo "confirmed: ${SHIM_SOURCE} is a symlink."
    echo "readlink:    $(readlink "${SHIM_SOURCE}")"
    echo "readlink -f: $(readlink -f "${SHIM_SOURCE}")"
else
    echo "note: ${SHIM_SOURCE} is not a symlink in this copy (already a regular file)."
fi
ACTUAL_SHIM_SIZE="$(stat -Lc '%s' "${SHIM_SOURCE}")"
ACTUAL_SHIM_SHA256="$(sha256sum "${SHIM_SOURCE}" | awk '{print $1}')"
echo "dereferenced size:   ${ACTUAL_SHIM_SIZE}"
echo "dereferenced SHA-256: ${ACTUAL_SHIM_SHA256}"
if [ "${ACTUAL_SHIM_SIZE}" != "${EXPECTED_SHIM_SIZE}" ]; then
    echo "ABORT: ${SHIM_SOURCE} dereferenced size ${ACTUAL_SHIM_SIZE} != expected ${EXPECTED_SHIM_SIZE}."
    exit 1
fi
if [ "${ACTUAL_SHIM_SHA256}" != "${EXPECTED_SHIM_SHA256}" ]; then
    echo "ABORT: ${SHIM_SOURCE} SHA-256 ${ACTUAL_SHIM_SHA256} != expected ${EXPECTED_SHIM_SHA256}."
    exit 1
fi
echo "OK: ${SHIM_SOURCE} dereferences to content matching pinned size (${ACTUAL_SHIM_SIZE})"
echo "    and SHA-256 (${ACTUAL_SHIM_SHA256}) - i.e. shimx64.efi's actual bytes."
echo

echo "-- Gate A3: snponly.efi source matches pinned provenance --"
if [ ! -f "${SNPONLY_SOURCE}" ]; then
    echo "ABORT: ${SNPONLY_SOURCE} not found."
    exit 1
fi
ACTUAL_SNPONLY_SIZE="$(stat -c '%s' "${SNPONLY_SOURCE}")"
ACTUAL_SNPONLY_SHA256="$(sha256sum "${SNPONLY_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_SNPONLY_SIZE}" != "${EXPECTED_SNPONLY_SIZE}" ]; then
    echo "ABORT: ${SNPONLY_SOURCE} size ${ACTUAL_SNPONLY_SIZE} != expected ${EXPECTED_SNPONLY_SIZE}."
    exit 1
fi
if [ "${ACTUAL_SNPONLY_SHA256}" != "${EXPECTED_SNPONLY_SHA256}" ]; then
    echo "ABORT: ${SNPONLY_SOURCE} SHA-256 ${ACTUAL_SNPONLY_SHA256} != expected ${EXPECTED_SNPONLY_SHA256}."
    exit 1
fi
echo "OK: ${SNPONLY_SOURCE} matches pinned size (${ACTUAL_SNPONLY_SIZE}) and SHA-256 (${ACTUAL_SNPONLY_SHA256})."
echo

echo "== Gate group B: stage the TFTP tree (local filesystem only, no network mutation yet) =="
echo

echo "-- Step B1: create the Spike directory tree (owned by brener, no sudo) --"
mkdir -p "${SHIM_DIR}"
echo "Created ${SHIM_DIR}"
echo

echo "-- Step B2: copy the two pinned, already-verified official binaries --"
echo "   install(1) dereferences a symlink source and writes a regular file at the"
echo "   destination; this is verified explicitly right below, not just assumed."
install -m 0644 "${SHIM_SOURCE}" "${SHIM_DIR}/snponly-shim.efi"
echo "Copied ${SHIM_SOURCE} -> ${SHIM_DIR}/snponly-shim.efi"

echo "-- Gate B2a: staged snponly-shim.efi is a REGULAR FILE containing the dereferenced bytes --"
if [ -L "${SHIM_DIR}/snponly-shim.efi" ]; then
    echo "ABORT: staged ${SHIM_DIR}/snponly-shim.efi is a symlink, not a regular file."
    echo "  A TFTP server must serve real content at this path, not a link payload."
    exit 1
fi
if [ ! -f "${SHIM_DIR}/snponly-shim.efi" ]; then
    echo "ABORT: staged ${SHIM_DIR}/snponly-shim.efi is missing or not a regular file."
    exit 1
fi
STAGED_SHIM_REGULAR_SIZE="$(stat -c '%s' "${SHIM_DIR}/snponly-shim.efi")"
STAGED_SHIM_REGULAR_SHA256="$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')"
if [ "${STAGED_SHIM_REGULAR_SIZE}" != "${EXPECTED_SHIM_SIZE}" ]; then
    echo "ABORT: staged snponly-shim.efi regular-file size ${STAGED_SHIM_REGULAR_SIZE} != expected ${EXPECTED_SHIM_SIZE}."
    exit 1
fi
if [ "${STAGED_SHIM_REGULAR_SHA256}" != "${EXPECTED_SHIM_SHA256}" ]; then
    echo "ABORT: staged snponly-shim.efi regular-file SHA-256 ${STAGED_SHIM_REGULAR_SHA256} != expected ${EXPECTED_SHIM_SHA256}."
    exit 1
fi
echo "OK: ${SHIM_DIR}/snponly-shim.efi is a regular file (test ! -L passed), size ${STAGED_SHIM_REGULAR_SIZE},"
echo "    SHA-256 ${STAGED_SHIM_REGULAR_SHA256} - the dereferenced shimx64.efi content, servable as-is by TFTP."
install -m 0644 "${SNPONLY_SOURCE}" "${SHIM_DIR}/snponly.efi"
echo "Copied ${SNPONLY_SOURCE} -> ${SHIM_DIR}/snponly.efi"
echo

echo "-- Step B3: author autoexec.ipxe deterministically (our own content, network-free) --"
echo "   Written with printf and explicit \\n escapes only - no embedded literal"
echo "   newlines in this script's source - to remove any ambiguity about the"
echo "   exact bytes produced (LF only, one final newline, no CRLF, no trailing"
echo "   blank line, no BOM)."
printf '#!ipxe\necho Bamep Issue #53 Phase 9a probe from ${cwuri}\nshow efi/SecureBoot\nshell\n' \
    > "${SHIM_DIR}/autoexec.ipxe"
echo "Wrote ${SHIM_DIR}/autoexec.ipxe"
echo "-- exact staged content (cat -A: \$ marks end of line, no other control chars expected) --"
cat -A "${SHIM_DIR}/autoexec.ipxe"
echo

echo "-- Gate B3a: autoexec.ipxe matches the pinned deterministic size/hash exactly --"
echo "   This gate runs immediately after the file is written, still before any"
echo "   NetworkManager/firewall/IP/dnsmasq mutation."
ACTUAL_AUTOEXEC_SIZE="$(stat -c '%s' "${SHIM_DIR}/autoexec.ipxe")"
ACTUAL_AUTOEXEC_SHA256="$(sha256sum "${SHIM_DIR}/autoexec.ipxe" | awk '{print $1}')"
if [ "${ACTUAL_AUTOEXEC_SIZE}" != "${EXPECTED_AUTOEXEC_SIZE}" ]; then
    echo "ABORT: autoexec.ipxe size ${ACTUAL_AUTOEXEC_SIZE} != expected ${EXPECTED_AUTOEXEC_SIZE}."
    exit 1
fi
if [ "${ACTUAL_AUTOEXEC_SHA256}" != "${EXPECTED_AUTOEXEC_SHA256}" ]; then
    echo "ABORT: autoexec.ipxe SHA-256 ${ACTUAL_AUTOEXEC_SHA256} != expected ${EXPECTED_AUTOEXEC_SHA256}."
    exit 1
fi
echo "OK: autoexec.ipxe matches pinned size (${ACTUAL_AUTOEXEC_SIZE}) and SHA-256 (${ACTUAL_AUTOEXEC_SHA256})."
echo

echo "-- Gate B4: hash every staged file --"
sha256sum "${SHIM_DIR}/snponly-shim.efi" "${SHIM_DIR}/snponly.efi" "${SHIM_DIR}/autoexec.ipxe" \
    | tee "${SPIKE_DIR}/sha256sums.txt"
echo

echo "-- Gate B5: pin staged copies against recorded provenance (abort on any mismatch) --"
STAGED_SHIM_SHA256="$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')"
if [ "${STAGED_SHIM_SHA256}" != "${EXPECTED_SHIM_SHA256}" ]; then
    echo "ABORT: staged snponly-shim.efi SHA-256 does not match recorded provenance."
    exit 1
fi
echo "OK: staged snponly-shim.efi SHA-256 matches recorded provenance (${STAGED_SHIM_SHA256})."
STAGED_SNPONLY_SHA256="$(sha256sum "${SHIM_DIR}/snponly.efi" | awk '{print $1}')"
if [ "${STAGED_SNPONLY_SHA256}" != "${EXPECTED_SNPONLY_SHA256}" ]; then
    echo "ABORT: staged snponly.efi SHA-256 does not match recorded provenance."
    exit 1
fi
echo "OK: staged snponly.efi SHA-256 matches recorded provenance (${STAGED_SNPONLY_SHA256})."
STAGED_AUTOEXEC_SHA256="$(sha256sum "${SHIM_DIR}/autoexec.ipxe" | awk '{print $1}')"
if [ "${STAGED_AUTOEXEC_SHA256}" != "${EXPECTED_AUTOEXEC_SHA256}" ]; then
    echo "ABORT: staged autoexec.ipxe SHA-256 does not match the pinned deterministic value."
    exit 1
fi
echo "OK: staged autoexec.ipxe SHA-256 matches the pinned deterministic value (${STAGED_AUTOEXEC_SHA256})."
echo

echo "-- Gate B6: forbidden-name sweep (defense in depth) --"
FORBIDDEN_HIT=0
while IFS= read -r -d '' f; do
    lower="$(basename "$f" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        ipxe.efi|ipxe-shim.efi|shimx64.efi|shimaa64.efi|wimboot*|bcd|boot.sdi|boot.wim|*grub*|*fedora*|*winpe*|*bootmgfw*|*bootmgr*|*winload*|*.p7b|*.ttf)
            echo "ABORT: forbidden/unexpected file present: ${f}"
            FORBIDDEN_HIT=1
            ;;
    esac
done < <(find "${TFTP_ROOT}" -type f -print0)
if [ "${FORBIDDEN_HIT}" != "0" ]; then
    exit 1
fi
echo "OK: no forbidden filename (ipxe.efi, ipxe-shim.efi, shimx64.efi, wimboot, BCD,"
echo "    boot.sdi, boot.wim, GRUB/Fedora/WinPE/bootmgfw/bootmgr/winload/.p7b/.ttf)"
echo "    found anywhere under ${TFTP_ROOT}."
echo

echo "-- Gate B7: exact-listing gate - the staged tree contains ONLY the three expected files --"
echo "   Both sides are generated, then normalized with LC_ALL=C sort (not the"
echo "   operator's interactive shell locale), before comparison. Order is"
echo "   therefore irrelevant. Neither side is deduplicated (no 'sort -u'), so a"
echo "   duplicate entry on either side changes the line count and still fails -"
echo "   it cannot pass silently. Any missing or extra path still fails."
EXPECTED_TREE_LIST="$(printf '%s\n' \
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
echo "OK: staged TFTP tree contains exactly the three expected files, nothing else, no duplicates:"
echo "${ACTUAL_TREE_LIST}" | sed 's/^/    /'
echo

echo "-- Gate B8: explicit confirmation /ipxe.efi is absent from the staged tree --"
if [ -e "${TFTP_ROOT}/ipxe.efi" ]; then
    echo "ABORT: ${TFTP_ROOT}/ipxe.efi exists. This run must not supply the upstream fallback."
    exit 1
fi
if find "${TFTP_ROOT}" -iname 'ipxe.efi' | grep -q .; then
    echo "ABORT: an ipxe.efi was found somewhere under ${TFTP_ROOT}."
    exit 1
fi
echo "OK: no ipxe.efi (fallback or otherwise) exists anywhere under ${TFTP_ROOT}."
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
echo "   Phase 9a performs no HTTP itself. This gate does not object to an HTTP-like"
echo "   listener in general - only to one bound to 0.0.0.0, '*', ::, or ${ADDR_HOST}"
echo "   itself, i.e. one that would also accept traffic on ${IFACE} once ${ADDR} is"
echo "   assigned. A listener bound exclusively to another specific address (e.g. a"
echo "   Tailscale/management interface address, or loopback) is left alone - this"
echo "   script must never disable Tailscale or any other management service."
if http_like_listener_conflicts_with_lab_path; then
    echo "ABORT: an HTTP-like listener is bound to an address that would also accept"
    echo "  traffic on ${IFACE}/${ADDR_HOST}. Phase 9a must not run alongside one."
    exit 1
fi
echo "OK: no HTTP-like listener bound to a wildcard address or to ${ADDR_HOST}."
echo "    This script starts DHCP+TFTP only (dnsmasq); it does not start, configure,"
echo "    or reference any HTTP server. HTTP is reserved for a later Phase 9b."
echo

echo "-- Gate C3: no local IPv4 address already in ${SUBNET} --"
if ip -4 addr show 2>/dev/null | grep -qF '192.168.99.'; then
    echo "ABORT: an address in ${SUBNET} already exists on this host."
    ip -4 addr show | grep -F '192.168.99.'
    exit 1
fi
echo "OK: no local address in ${SUBNET}."
echo

echo "-- Gate C4: no existing route for ${SUBNET} --"
if ip route show 2>/dev/null | grep -qF "${SUBNET}"; then
    echo "ABORT: a route for ${SUBNET} already exists."
    ip route show | grep -F "${SUBNET}"
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

echo "-- Confirm the address landed only on ${IFACE} --"
ip -4 addr show "${IFACE}"
if ip -4 addr show 2>/dev/null | grep -F "${ADDR_HOST}" | grep -qv "${IFACE}"; then
    echo "ABORT: ${ADDR_HOST} leaked to another interface. Reverting."
    sudo ip addr del "${ADDR}" dev "${IFACE}"
    exit 1
fi
echo "OK: ${ADDR_HOST} only on ${IFACE}."
echo

echo "== Step 3: runtime-only firewalld scope for this throwaway isolated Spike =="
echo "   THROWAWAY SPIKE CONFIGURATION ONLY."
echo "   This is NOT the future Bamep appliance firewall design."
sudo firewall-cmd --zone=trusted --change-interface="${IFACE}"
echo

echo "== Step 4: author dnsmasq.conf (THROWAWAY SPIKE CONFIG ONLY; DHCP+TFTP only, no HTTP) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 9a - throwaway harness. NOT production configuration.
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

echo "== Step 5: pre-create log/lease files, owner dnsmasq, group brener, mode 0640 =="
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.log"
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.leases"
echo

echo "== Step 6: validate readability/traversal for the dnsmasq runtime user =="
sudo -u dnsmasq test -x "${SPIKE_DIR}" && echo "OK: ${SPIKE_DIR} traversable by dnsmasq" || { echo "ABORT: ${SPIKE_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}" && echo "OK: ${TFTP_ROOT} traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/ipxeboot" && echo "OK: ${TFTP_ROOT}/ipxeboot traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT}/ipxeboot not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${SHIM_DIR}" && echo "OK: ${SHIM_DIR} traversable by dnsmasq" || { echo "ABORT: ${SHIM_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/snponly-shim.efi" && echo "OK: snponly-shim.efi readable by dnsmasq" || { echo "ABORT: snponly-shim.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/snponly.efi" && echo "OK: snponly.efi readable by dnsmasq" || { echo "ABORT: snponly.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${SHIM_DIR}/autoexec.ipxe" && echo "OK: autoexec.ipxe readable by dnsmasq" || { echo "ABORT: autoexec.ipxe not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.log" && echo "OK: dnsmasq.log writable by dnsmasq" || { echo "ABORT: dnsmasq.log not writable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.leases" && echo "OK: dnsmasq.leases writable by dnsmasq" || { echo "ABORT: dnsmasq.leases not writable by dnsmasq"; exit 1; }
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
    echo "    wildcard address or ${ADDR_HOST} (interface-specific/management listeners,"
    echo "    e.g. Tailscale, are not a conflict and were left untouched)."
fi
echo

echo "== Step 9: start dnsmasq in the background (10-minute ceiling) =="
sudo timeout 600 dnsmasq -d --conf-file="${SPIKE_DIR}/dnsmasq.conf" \
    > "${SPIKE_DIR}/dnsmasq-stdout.log" 2>&1 &
DNSMASQ_PID=$!

echo "== Step 10: start packet capture in the background - ALL traffic on ${IFACE}, no protocol filter =="
echo "   (enp8s0 is a direct point-to-point cable to the one physical Endpoint, so"
echo "    capturing everything here does not expose/alter any other network.)"
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
    echo "Reconstruct from the pcap + dnsmasq.log, in order: DORA, first-stage shim"
    echo "RRQ/transfer, every subsequent RRQ in exact order, whether snponly.efi was"
    echo "requested, whether /ipxe.efi was requested instead, whether snponly.efi"
    echo "transferred completely, whether autoexec.ipxe was requested/transferred,"
    echo "and the owner-visible screen result."
    echo
    echo "Next: run issue53-phase9a-cleanup.sh to revert IP/firewall/NetworkManager state."
}
trap cleanup_harness EXIT
trap 'exit 130' INT TERM

sleep 2
if ! kill -0 "${DNSMASQ_PID}" 2>/dev/null; then
    echo "ABORT: dnsmasq failed to start or exited immediately."
    echo "  Check ${SPIKE_DIR}/dnsmasq-stdout.log and ${SPIKE_DIR}/dnsmasq.log."
    exit 1
fi
if ! kill -0 "${TCPDUMP_PID}" 2>/dev/null; then
    echo "ABORT: tcpdump failed to start. Check ${CAPTURE_LOG}."
    exit 1
fi

echo "-- Confirm DHCP/TFTP listeners are actually present on the harness --"
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
echo

echo "Waiting for dnsmasq to exit (Ctrl-C to stop early, 10-minute ceiling otherwise)..."
wait "${DNSMASQ_PID}"
