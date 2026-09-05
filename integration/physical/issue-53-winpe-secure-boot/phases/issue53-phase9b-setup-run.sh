#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 9b - THROWAWAY Spike operator script.
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
# Baseline (Phase 9a2, physically executed and evidence-preserved at
# /var/tmp/bamep-issue53-phase9a2-ipxe-fallback/):
# ---------------------------------------------------------------------
#
# firmware UEFI PXE
#   -> official iPXE v2.0.0 snponly-shim.efi (accepted, executed)
#   -> shim fallback request /ipxe.efi
#   -> exact official snponly.efi bytes served under /ipxe.efi
#   -> iPXE v2.0.0 (g12798) executes
#   -> automatic fetch of ipxeboot/x86_64-sb/autoexec.ipxe (sibling path,
#      no root alias needed, resolved on the first try)
#   -> show efi/SecureBoot -> efi/SecureBoot:hex = 01
#   -> iPXE shell
#
# Phase 9a2 classification: progressed to a new meaningful boundary.
# Overall Issue #53: B - unchanged.
#
# Empirical Phase 9a2 observations carried forward unchanged into this
# phase's design: no /autoexec.ipxe root alias needed; no explicit iPXE
# `dhcp` command was executed by us or by iPXE itself; no second DHCP DORA
# occurred after second-stage startup; no "No more network devices" or
# similar SNP/NIC error appeared; Secure Boot remained enabled from iPXE's
# own reported perspective.
#
# ---------------------------------------------------------------------
# Phase 9b question (HTTP TRANSPORT ONLY - one conceptual change):
# ---------------------------------------------------------------------
#
# Can the already-validated physical snponly/iPXE chain perform an
# explicit HTTP GET over the current direct Fedora<->Endpoint link while
# Secure Boot remains Enabled/Active/Standard?
#
# No wimboot. No WinPE. No kernel. No initrd. No chain. No boot. No
# executable HTTP payload. No explicit iPXE "dhcp" command (iPXE's HTTP
# stack uses the network settings already configured by the firmware PXE
# stage / shim handoff, exactly as observed working for TFTP in Phase 9a2 -
# we are not asserting this is guaranteed to work for HTTP too; that is
# exactly what this phase tests).
#
# Upstream command semantics, independently re-verified against current
# https://ipxe.org/cmd/imgfetch and https://ipxe.org/cmd/imgstat (not
# taken on faith from any handoff):
#   imgfetch [--name <name>] [--timeout <timeout>] <uri> [<arguments>...]
#     "Download an image from the specified URI." Command status:
#     Success = "The image was successfully downloaded"; Failure = "The
#     image was not successfully downloaded." imgfetch does NOT execute
#     the downloaded image - see also: imgstat, chain, kernel, imgfree.
#   imgstat [<name>...]
#     "Display information about the specified images. If no images are
#     explicitly specified, iPXE will display information about all
#     images," e.g. "boot.php : 111 bytes [script] [SELECTED]". Does not
#     execute anything; purely inert inspection.
# This confirms imgfetch+imgstat is the correct minimal INERT mechanism:
# download into memory, inspect, never boot/chain/execute.
#
# Also independently re-verified against current https://ipxe.org/scripting:
#   - "You can start a comment using the # symbol" - a bare '#' ANYWHERE
#     on a line (not only at line start) begins a trailing comment that
#     strips the remainder of that line. This is the exact, now-confirmed
#     root cause of the Phase 9a2 screen showing only "Bamep Issue"
#     instead of the full echo text: the literal '#53' in that script's
#     echo line was parsed as a comment marker, not literal text. Per
#     owner instruction, this script uses "Issue 53" (no '#') everywhere,
#     and contains a '#' ONLY on the mandatory "#!ipxe" magic first line,
#     which is a special-cased magic marker, not a stripped comment.
#   - "iPXE will terminate a script immediately if any line of the script
#     fails," overridable with the "||" operator, where the empty/`echo`
#     right-hand side is "treated as 'do nothing, successfully'". This
#     script therefore guards imgfetch with "|| echo HTTP fetch failed" so
#     that a failed fetch prints a visible diagnostic AND still allows
#     script execution to continue to imgstat/shell, rather than
#     terminating the script silently before reaching a diagnostic shell.
#     imgstat is called with NO name argument (lists all present images,
#     which is itself informative and does not fail merely because a
#     named image is absent) to avoid a second possible fail-fast point.
#
# Artifact provenance for the boot chain itself (IDENTICAL to Phase 9a2;
# re-verified again below before staging anything - only autoexec.ipxe
# content changes in this phase):
#
#   Containing release: iPXE v2.0.0 (github.com/ipxe/ipxe, tag v2.0.0,
#   commit 12798ec, published 2026-03-06T16:16:13Z)
#     ipxeboot.tar.gz: size 12002760, sha256
#     01a526d4cc791fc30362259c609d6c506cc64a7bdff51b9a5eb788354e17eee1
#
#   Shim (source: ipxeboot/x86_64-sb/snponly-shim.efi, symlink to
#   shimx64.efi in the official archive; staged as a regular file):
#     size 1038920, sha256
#     83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885
#
#   snponly.efi (staged TWICE from the same source, as in Phase 9a2: once
#   under its documented sibling name, once under the root ipxe.efi
#   fallback name the shim actually requests on this physical firmware):
#     size 295784, sha256
#     b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a
#
# New artifacts frozen for this phase (both authored with printf and
# explicit \n escapes only - no embedded literal newlines in this script's
# source - LF only, one final newline, no CRLF, no BOM):
#
#   ipxeboot/x86_64-sb/autoexec.ipxe (REPLACES the Phase 9a2 83-byte file -
#   this is a NEW, differently-pinned file for this phase, not a reuse):
#     #!ipxe
#     echo Bamep Issue 53 Phase 9b
#     show efi/SecureBoot
#     imgfetch --name bamep-http-probe http://192.168.99.1:8080/probe.txt || echo HTTP fetch failed
#     imgstat
#     shell
#     size:   164 bytes
#     sha256: 5f73b2555a7a1419a3e0bf97d010ac237010fda10e6618cc97f89930350cf92c
#
#   probe.txt (the ONLY file in the dedicated HTTP root; plain ASCII text,
#   never executed, never chained, never booted):
#     Bamep Issue 53 Phase 9b HTTP transport probe
#     Secure Boot chain already established
#     HTTP payload only - never executed
#     size:   118 bytes
#     sha256: 5c41127e3564745afd6bb8082c000dfdda5cc3b53dcb28ec4f9da02a335d7133
#
# Deliberately NOT staged: /autoexec.ipxe (root), ipxe-shim.efi, shimx64.efi
# under its own name, wimboot, BCD, boot.sdi, boot.wim, any WinPE asset,
# GRUB, the Fedora shim, any second/duplicate ipxe.efi, and no executable
# format of any kind in the HTTP root.
#
# HTTP server: Python's standard library `http.server` (no package
# installation - already part of the Fedora base Python 3 install used
# throughout this Issue). It supports an explicit `--bind <address>` and
# `--directory <path>`, which is exactly the minimal, already-available,
# deterministic mechanism needed to bind to ONE specific address and serve
# ONE specific directory tree - no alternative already-installed server on
# this host is smaller or more deterministic for this single-file probe.
#
# Interpretation boundary: this probe does not select Bamep's final
# production network-delivery mechanism. Overall Issue #53 remains B
# regardless of this phase's outcome, since no functional WinPE shell is
# attempted here. Use only "progressed to a new meaningful boundary" /
# "same effective boundary" / "failed earlier" / "harness prevented
# evaluation" for this individual phase - do not use A/B/C/D here.
#
# Observability discipline: do not claim byte-exact HTTP delivery merely
# from a 200 status or a Content-Length header. The pcap captures HTTP in
# plaintext (no TLS); the response body must be reassembled from the
# capture and its SHA-256 compared against the pinned probe.txt hash
# before claiming byte-exact delivery.
#
# Safety note: this script stages only two opaque, hash-pinned, official,
# already-provenanced EFI-content copies (as in Phase 9a2), one plaintext
# iPXE script of our own authorship, and one plaintext HTTP payload of our
# own authorship - never an executable format. It does not configure or
# execute any disk-writing, partition, format, install, or destructive
# storage action. The Endpoint remains physically diskless.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself.

set -euo pipefail
export LC_ALL=C

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-phase9b-http-probe"
TFTP_ROOT="${SPIKE_DIR}/tftp"
HTTP_ROOT="${SPIKE_DIR}/http"
SHIM_DIR="${TFTP_ROOT}/ipxeboot/x86_64-sb"
ROOT_IPXE_EFI="${TFTP_ROOT}/ipxe.efi"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
HTTP_PORT="8080"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-http-probe.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"
HTTP_LOG="${SPIKE_DIR}/http-server.log"

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
EXPECTED_AUTOEXEC_SHA256="5f73b2555a7a1419a3e0bf97d010ac237010fda10e6618cc97f89930350cf92c"
EXPECTED_AUTOEXEC_SIZE="164"
EXPECTED_PROBE_SHA256="5c41127e3564745afd6bb8082c000dfdda5cc3b53dcb28ec4f9da02a335d7133"
EXPECTED_PROBE_SIZE="118"

# HTTP-like-listener check, identical logic to the corrected Phase 9a/9a2
# version (also mirrored into issue53-phase9b-cleanup.sh so setup and
# cleanup agree). Return-code convention: exit 0 ("true") = conflict
# found; exit 1 ("false") = no conflict. Call as `if http_like_...; then`.
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

echo "== Bamep Issue #53 Phase 9b - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Question:  can the validated snponly/iPXE chain perform an explicit"
echo "              HTTP GET to ${ADDR_HOST}:${HTTP_PORT}/probe.txt while"
echo "              Secure Boot remains Enabled/Active/Standard?"
echo "   No wimboot. No WinPE. No kernel/initrd/chain/boot. No executable"
echo "   HTTP payload. No explicit iPXE dhcp command."
echo

# --------------------------------------------------------------------
# Gate group A: local, read-only artifact/hash/absence checks. ALL of
# these complete BEFORE this script touches network state or starts any
# DHCP/TFTP/HTTP service.
# --------------------------------------------------------------------

echo "-- Gate A0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-phase9b-cleanup.sh --purge"
    exit 1
fi
echo "OK: no stale ${SPIKE_DIR} present."
echo

echo "-- Gate A1: official release archive present and matches pinned provenance --"
if [ ! -f "${ARCHIVE_SOURCE}" ]; then
    echo "ABORT: ${ARCHIVE_SOURCE} not found."
    exit 1
fi
[ "$(stat -c '%s' "${ARCHIVE_SOURCE}")" = "${EXPECTED_ARCHIVE_SIZE}" ] || { echo "ABORT: archive size mismatch."; exit 1; }
[ "$(sha256sum "${ARCHIVE_SOURCE}" | awk '{print $1}')" = "${EXPECTED_ARCHIVE_SHA256}" ] || { echo "ABORT: archive SHA-256 mismatch."; exit 1; }
echo "OK: ${ARCHIVE_SOURCE} matches pinned provenance."
echo

echo "-- Gate A2: shim source (dereferenced) matches pinned provenance --"
if [ ! -f "${SHIM_SOURCE}" ]; then
    echo "ABORT: ${SHIM_SOURCE} not found (or symlink to missing/non-regular target)."
    exit 1
fi
if [ -L "${SHIM_SOURCE}" ]; then
    echo "confirmed symlink: $(readlink "${SHIM_SOURCE}") (resolves to $(readlink -f "${SHIM_SOURCE}"))"
fi
[ "$(stat -Lc '%s' "${SHIM_SOURCE}")" = "${EXPECTED_SHIM_SIZE}" ] || { echo "ABORT: shim dereferenced size mismatch."; exit 1; }
[ "$(sha256sum "${SHIM_SOURCE}" | awk '{print $1}')" = "${EXPECTED_SHIM_SHA256}" ] || { echo "ABORT: shim dereferenced SHA-256 mismatch."; exit 1; }
echo "OK: ${SHIM_SOURCE} dereferences to pinned shimx64.efi content."
echo

echo "-- Gate A3: snponly.efi source matches pinned provenance (staged twice below) --"
if [ ! -f "${SNPONLY_SOURCE}" ]; then
    echo "ABORT: ${SNPONLY_SOURCE} not found."
    exit 1
fi
[ "$(stat -c '%s' "${SNPONLY_SOURCE}")" = "${EXPECTED_SNPONLY_SIZE}" ] || { echo "ABORT: snponly.efi size mismatch."; exit 1; }
[ "$(sha256sum "${SNPONLY_SOURCE}" | awk '{print $1}')" = "${EXPECTED_SNPONLY_SHA256}" ] || { echo "ABORT: snponly.efi SHA-256 mismatch."; exit 1; }
echo "OK: ${SNPONLY_SOURCE} matches pinned size/SHA-256."
echo

echo "== Gate group B: stage the TFTP tree (local filesystem only, no network mutation yet) =="
echo

echo "-- Step B1: create the Spike TFTP directory tree (owned by brener, no sudo) --"
mkdir -p "${SHIM_DIR}"
echo "Created ${SHIM_DIR} (and ${TFTP_ROOT} as its parent)"
echo

echo "-- Step B2: copy the shim (unchanged from Phase 9a2) --"
install -m 0644 "${SHIM_SOURCE}" "${SHIM_DIR}/snponly-shim.efi"
if [ -L "${SHIM_DIR}/snponly-shim.efi" ] || [ ! -f "${SHIM_DIR}/snponly-shim.efi" ]; then
    echo "ABORT: staged snponly-shim.efi is not a plain regular file."
    exit 1
fi
[ "$(stat -c '%s' "${SHIM_DIR}/snponly-shim.efi")" = "${EXPECTED_SHIM_SIZE}" ] || { echo "ABORT: staged shim size mismatch."; exit 1; }
[ "$(sha256sum "${SHIM_DIR}/snponly-shim.efi" | awk '{print $1}')" = "${EXPECTED_SHIM_SHA256}" ] || { echo "ABORT: staged shim SHA-256 mismatch."; exit 1; }
echo "OK: ${SHIM_DIR}/snponly-shim.efi staged and re-verified."
echo

echo "-- Step B2b: stage snponly.efi content TWICE, from the SAME source (unchanged from Phase 9a2) --"
echo "   1) ipxeboot/x86_64-sb/snponly.efi - documented sibling path"
echo "   2) ${ROOT_IPXE_EFI} - the shim's actual requested fallback path on this"
echo "      physical firmware. Same bytes, different filename - not iPXE's own"
echo "      ipxe.efi build."
install -m 0644 "${SNPONLY_SOURCE}" "${SHIM_DIR}/snponly.efi"
install -m 0644 "${SNPONLY_SOURCE}" "${ROOT_IPXE_EFI}"
for f in "${SHIM_DIR}/snponly.efi" "${ROOT_IPXE_EFI}"; do
    if [ -L "${f}" ] || [ ! -f "${f}" ]; then
        echo "ABORT: ${f} is not a plain regular file."
        exit 1
    fi
done
SIBLING_INODE="$(stat -c '%i' "${SHIM_DIR}/snponly.efi")"
ROOT_INODE="$(stat -c '%i' "${ROOT_IPXE_EFI}")"
if [ "${SIBLING_INODE}" = "${ROOT_INODE}" ]; then
    echo "ABORT: the two snponly.efi copies share an inode (hardlink, not an independent copy)."
    exit 1
fi
for f in "${SHIM_DIR}/snponly.efi" "${ROOT_IPXE_EFI}"; do
    [ "$(stat -c '%s' "${f}")" = "${EXPECTED_SNPONLY_SIZE}" ] || { echo "ABORT: ${f} size mismatch."; exit 1; }
    [ "$(sha256sum "${f}" | awk '{print $1}')" = "${EXPECTED_SNPONLY_SHA256}" ] || { echo "ABORT: ${f} SHA-256 mismatch."; exit 1; }
done
echo "OK: both snponly.efi copies staged, independent inodes (${SIBLING_INODE} vs ${ROOT_INODE}),"
echo "    both re-verified against pinned size/SHA-256."
echo

echo "-- Step B3: author the NEW Phase 9b autoexec.ipxe deterministically --"
echo "   Written with printf and explicit \\n escapes only. Uses 'Issue 53' (no"
echo "   '#') per the Phase 9a2 finding that a bare '#' anywhere on a line begins"
echo "   an iPXE script comment and strips the rest of that line (confirmed"
echo "   against https://ipxe.org/scripting). imgfetch is guarded with"
echo "   '|| echo HTTP fetch failed' so a failure is visible but the script"
echo "   still reaches imgstat/shell rather than terminating silently, per"
echo "   documented iPXE error-handling semantics (fail-fast by default,"
echo "   overridable with ||)."
printf '#!ipxe\necho Bamep Issue 53 Phase 9b\nshow efi/SecureBoot\nimgfetch --name bamep-http-probe http://%s:%s/probe.txt || echo HTTP fetch failed\nimgstat\nshell\n' \
    "${ADDR_HOST}" "${HTTP_PORT}" > "${SHIM_DIR}/autoexec.ipxe"
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
if [ -n "${STRAY_HASH_LINES}" ]; then
    echo "ABORT: unexpected '#' found outside the magic first line:"
    echo "${STRAY_HASH_LINES}"
    exit 1
fi
echo "OK: '#' appears only on the mandatory magic first line (#!ipxe)."
echo

echo "-- Gate B3c: confirm /autoexec.ipxe does NOT exist at the TFTP root --"
if [ -e "${TFTP_ROOT}/autoexec.ipxe" ]; then
    echo "ABORT: ${TFTP_ROOT}/autoexec.ipxe exists. Only the sibling path is served."
    exit 1
fi
echo "OK: no ${TFTP_ROOT}/autoexec.ipxe exists."
echo

echo "-- Gate B4: hash every staged TFTP file --"
sha256sum "${SHIM_DIR}/snponly-shim.efi" "${SHIM_DIR}/snponly.efi" "${SHIM_DIR}/autoexec.ipxe" "${ROOT_IPXE_EFI}" \
    | tee "${SPIKE_DIR}/sha256sums-tftp.txt"
echo

echo "-- Gate B6: forbidden-name sweep over the TFTP tree (defense in depth) --"
echo "   'ipxe.efi' is intentionally excluded - it is the one known fallback path."
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
[ "${FORBIDDEN_HIT}" = "0" ] || exit 1
echo "OK: no forbidden filename found under ${TFTP_ROOT}."
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
echo "OK: TFTP tree contains exactly the four expected files:"
echo "${ACTUAL_TFTP_LIST}" | sed 's/^/    /'
echo

echo "-- Gate B8: exactly ONE ipxe.efi exists anywhere under the TFTP tree, at the root --"
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

echo "-- Step H2: author probe.txt deterministically (plain ASCII, never executed) --"
printf 'Bamep Issue 53 Phase 9b HTTP transport probe\nSecure Boot chain already established\nHTTP payload only - never executed\n' \
    > "${HTTP_ROOT}/probe.txt"
echo "Wrote ${HTTP_ROOT}/probe.txt"
cat -A "${HTTP_ROOT}/probe.txt"
echo

echo "-- Gate H2a: probe.txt matches the pinned deterministic size/hash exactly --"
[ "$(stat -c '%s' "${HTTP_ROOT}/probe.txt")" = "${EXPECTED_PROBE_SIZE}" ] || { echo "ABORT: probe.txt size mismatch."; exit 1; }
[ "$(sha256sum "${HTTP_ROOT}/probe.txt" | awk '{print $1}')" = "${EXPECTED_PROBE_SHA256}" ] || { echo "ABORT: probe.txt SHA-256 mismatch."; exit 1; }
echo "OK: probe.txt matches pinned size (${EXPECTED_PROBE_SIZE}) and SHA-256 (${EXPECTED_PROBE_SHA256})."
FILE_TYPE="$(file -b "${HTTP_ROOT}/probe.txt")"
echo "file(1) type: ${FILE_TYPE}"
case "${FILE_TYPE}" in
    *text*) : ;;
    *) echo "ABORT: probe.txt is not recognized as plain text."; exit 1 ;;
esac
echo

echo "-- Gate H3: forbidden-content sweep over the HTTP root --"
echo "   The HTTP root must never contain EFI binaries, wimboot, BCD, boot.sdi,"
echo "   boot.wim, WinPE assets, or anything intended for execution/chaining."
FORBIDDEN_HTTP_HIT=0
while IFS= read -r -d '' f; do
    lower="$(basename "$f" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        *.efi|wimboot*|bcd|boot.sdi|boot.wim|*.wim|*grub*|*winpe*|*.ipxe|*.sh|*.exe|*.dll|*.sys)
            echo "ABORT: forbidden/executable-looking file present in HTTP root: ${f}"
            FORBIDDEN_HTTP_HIT=1
            ;;
    esac
done < <(find "${HTTP_ROOT}" -type f -print0)
[ "${FORBIDDEN_HTTP_HIT}" = "0" ] || exit 1
echo "OK: no forbidden/executable-looking file found under ${HTTP_ROOT}."
echo

echo "-- Gate H4: exact-listing gate - HTTP root contains EXACTLY one file, probe.txt --"
EXPECTED_HTTP_LIST="probe.txt"
ACTUAL_HTTP_LIST="$(cd "${HTTP_ROOT}" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)"
if [ "${ACTUAL_HTTP_LIST}" != "${EXPECTED_HTTP_LIST}" ]; then
    echo "ABORT: HTTP root does not contain exactly one file named probe.txt."
    echo "actual: ${ACTUAL_HTTP_LIST}"
    exit 1
fi
echo "OK: HTTP root contains exactly one file: probe.txt."
echo

echo "-- Gate H5: hash the staged HTTP asset --"
sha256sum "${HTTP_ROOT}/probe.txt" | tee "${SPIKE_DIR}/sha256sums-http.txt"
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
echo "   Only a listener bound to 0.0.0.0, '*', ::, or ${ADDR_HOST} itself is a"
echo "   conflict (this also covers port ${HTTP_PORT}). A listener bound"
echo "   exclusively to another specific address (e.g. Tailscale/management) is"
echo "   left alone - this script must never disable such a service."
if http_like_listener_conflicts_with_lab_path; then
    echo "ABORT: an HTTP-like listener is bound to an address that would also"
    echo "  accept traffic on ${IFACE}/${ADDR_HOST}."
    exit 1
fi
echo "OK: no HTTP-like listener bound to a wildcard address or to ${ADDR_HOST}."
echo

echo "-- Gate C2b: no listener specifically on ${ADDR_HOST}:${HTTP_PORT} yet (cannot exist before Step 2 anyway) --"
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

echo "== All artifact/hash/absence/network-pre-state gates passed. =="
echo "== Only now does this script begin mutating network/runtime state. =="
echo

echo "== Step 1: take ${IFACE} out of NetworkManager's automatic management (runtime only) =="
sudo nmcli device set "${IFACE}" managed no
echo

echo "== Step 2: add temporary address (exact add, no flush) =="
sudo ip addr add "${ADDR}" dev "${IFACE}"
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

echo "== Step 4: start the HTTP server, bound ONLY to ${ADDR_HOST}:${HTTP_PORT}, serving ONLY ${HTTP_ROOT} =="
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

echo "-- Gate H6: prove the HTTP server owns/listens on EXACTLY ${ADDR_HOST}:${HTTP_PORT} - nothing else --"
HTTP_LISTEN_LINES="$(ss -Hltnp 2>/dev/null | awk -v p="${HTTP_PID}" '$0 ~ "pid="p"," {print $4}')"
echo "Listening address(es) owned by pid ${HTTP_PID}:"
echo "${HTTP_LISTEN_LINES}"
if [ "$(printf '%s\n' "${HTTP_LISTEN_LINES}" | grep -c .)" != "1" ]; then
    echo "ABORT: expected exactly one listening address for the HTTP server pid."
    sudo kill -TERM "${HTTP_PID}" 2>/dev/null || true
    exit 1
fi
if [ "${HTTP_LISTEN_LINES}" != "${ADDR_HOST}:${HTTP_PORT}" ]; then
    echo "ABORT: HTTP server is not bound to exactly ${ADDR_HOST}:${HTTP_PORT}."
    echo "  actual: ${HTTP_LISTEN_LINES}"
    sudo kill -TERM "${HTTP_PID}" 2>/dev/null || true
    exit 1
fi
echo "OK: HTTP server (pid ${HTTP_PID}) listens on exactly ${ADDR_HOST}:${HTTP_PORT} -"
echo "    not 0.0.0.0, not ::, not any Tailscale address, not any other port."
echo

echo "== Step 5: author dnsmasq.conf (DHCP+TFTP only; the HTTP server above is separate) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 9b - throwaway harness. NOT production configuration.
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
    echo "ABORT: dhcp-boot does not match the proven Phase 9a2 baseline value."
    exit 1
fi
echo "OK: dhcp-boot is byte-identical to the Phase 9a2 baseline."
echo

echo "== Step 6: pre-create log/lease files, owner dnsmasq, group brener, mode 0640 =="
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.log"
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.leases"
echo

echo "== Step 7: validate readability/traversal for the dnsmasq runtime user =="
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

echo "== Step 8: validate dnsmasq config syntax without binding any socket =="
sudo dnsmasq --test --conf-file="${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 9: final pre-flight before starting dnsmasq =="
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

echo "== Step 10: start dnsmasq in the background (10-minute ceiling) =="
sudo timeout 600 dnsmasq -d --conf-file="${SPIKE_DIR}/dnsmasq.conf" \
    > "${SPIKE_DIR}/dnsmasq-stdout.log" 2>&1 &
DNSMASQ_PID=$!

echo "== Step 11: start packet capture in the background - ALL traffic on ${IFACE}, no protocol filter =="
echo "   Started AFTER the HTTP server and dnsmasq so the capture window fully"
echo "   covers the entire physical boot attempt once triggered, including the"
echo "   HTTP transaction itself."
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
    echo "   ${SPIKE_DIR}/dnsmasq.leases"
    echo "   ${SPIKE_DIR}/sha256sums-tftp.txt"
    echo "   ${SPIKE_DIR}/sha256sums-http.txt"
    echo "   ${HTTP_LOG}"
    echo
    echo "Reconstruct from the pcap + logs, in order: DORA, shim probe/transfer,"
    echo "shim-originated revocation/certificate RRQs, /ipxe.efi RRQ and complete"
    echo "transfer, autoexec.ipxe request/transfer, TCP handshake to"
    echo "${ADDR_HOST}:${HTTP_PORT}, the literal HTTP request line/headers, the HTTP"
    echo "response status/headers/body, and reassemble+hash the plaintext response"
    echo "body against the pinned probe.txt SHA-256 before claiming byte-exact"
    echo "delivery. Do not infer imgfetch success merely from a 200 status."
    echo
    echo "Next: run issue53-phase9b-cleanup.sh to revert IP/firewall/NetworkManager/HTTP state."
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
echo "OK: dnsmasq (pid ${DNSMASQ_PID}), tcpdump (pid ${TCPDUMP_PID}), and the HTTP"
echo "    server (pid ${HTTP_PID}) are all alive; udp/67, udp/69, and"
echo "    ${ADDR_HOST}:${HTTP_PORT} are listening."
echo

echo "HARNESS READY - trigger UEFI PXE IPv4 now"
echo "Expected boot file (DHCP option 67): ipxeboot/x86_64-sb/snponly-shim.efi"
echo "Expected HTTP GET target: http://${ADDR_HOST}:${HTTP_PORT}/probe.txt"
echo "Do NOT manually type commands into any iPXE shell that may appear - observe only."
echo

echo "Waiting for dnsmasq to exit (Ctrl-C to stop early, 10-minute ceiling otherwise)..."
wait "${DNSMASQ_PID}"
