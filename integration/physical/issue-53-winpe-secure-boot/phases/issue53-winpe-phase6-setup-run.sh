#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 6 - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review of every step below, and do not execute it automatically from
# any agent - it must be run interactively by the owner.
#
# Baseline: this returns to the Phase 3 harness (NOT Phase 5). Phase 5 was
# a diagnostic negative result - bootmgfw.efi (from boot.wim) was proven
# non-identical to the retained-media bootx64.efi, but substituting it did
# not materially change the physical boundary (same 0xc0000225, confirmed
# by both wire evidence and the owner-visible screen). Phase 6 goes back
# to the original, more natural Phase 3 combination (stock media
# bootx64.efi + stock BCD) and adds exactly one further asset.
#
# Phase 3 established, on the diskless physical Endpoint (MAC
# e8:ff:1e:d6:2e:f5, Secure Boot Enabled/Active/Standard, factory keys):
#   bootx64.efi -> \Boot\BCD -> repeated Secure-Boot/Code-Integrity
#   policy/font RRQs (SbcpFlightToken.p7b, SecureBootPolicy.p7b,
#   SiPolicy.p7b x3, SkuSiPolicy.p7b x3, WinSiPolicy.p7b x3,
#   ATPSiPolicy.p7b x3, VbsSiPolicy.p7b x2, wgl4_boot.ttf x2), all not
#   found -> no boot.sdi -> no boot.wim -> no winload.efi -> network
#   silence -> Windows Boot Manager Status 0xc0000225 "An unexpected
#   error has occurred."
#
# Post-Phase-5 read-only inspection of the SAME retained WinPE lineage
# found a stock WinSiPolicy.p7b inside boot.wim index 1 at
# \Windows\Boot\EFI\winsipolicy.p7b (a byte-identical WinSxS copy also
# exists). WinSiPolicy.p7b is a filename Windows Boot Manager actually
# requested repeatedly in both Phase 3 and Phase 5 - it is therefore an
# evidence-driven candidate, unlike guessing.
#
# Explicitly NOT chosen and why: SecureBootPolicy.p7b was also requested
# in Phase 3/5, but no stock copy of it was found anywhere in the
# retained media or boot.wim inspection - there is no artifact to stage
# for it yet. boot.sdi was never requested on the wire in any phase, so
# adding it now would not be evidence-driven.
#
# Question: does satisfying ONLY the repeatedly observed
# \EFI\Microsoft\Boot\Policies\WinSiPolicy.p7b request change the
# physical Windows Boot Manager boundary?
#
# bootx64.efi artifact provenance (ORIGINAL retained-media bytes,
# UNCHANGED from Phase 3, re-verified below before any network service
# starts - this is explicitly NOT the Phase 5 bootmgfw.efi substitute):
#   Windows source: C:\BamepSpike\winpe_media\amd64\media\EFI\Boot\bootx64.efi
#   Fedora copy:    /tmp/bamep-issue53-bootx64.efi
#   Size:           2772912 bytes
#   SHA-256:        b2355c3e8a5fa140afb147d16646a7aa497a87d41937254b4925811614ba78a6
#
# BCD artifact provenance (UNCHANGED from Phase 3, re-verified below):
#   Windows source: C:\BamepSpike\winpe_media\amd64\media\Boot\BCD
#   Fedora copy:    /tmp/bamep-issue53-BCD
#   Size:           262144 bytes
#   SHA-256:        21bf8054adfe0614baba6f21a4bad0b7bfe71dbe9169d2422de42a79258beba0
# Used as an OPAQUE artifact - not authored or modified by this script,
# no bcdedit/bcdboot anywhere in this harness.
#
# WinSiPolicy.p7b artifact provenance (NEW this run, owner-supplied and
# independently re-verified on this Fedora host before this script was
# written):
#   Source:      boot.wim index 1, \Windows\Boot\EFI\winsipolicy.p7b
#                (from the same retained WinPE media tree; a
#                byte-identical WinSxS copy also exists)
#   Fedora copy: /tmp/bamep-issue53-WinSiPolicy.p7b
#   Size:        10341 bytes
#   SHA-256:     186d4262d6301d235ce38680b16dc3f3236a8415135087e781133ee7598974e7
# `file` identifies it as "DER Encoded PKCS#7 Signed Data", consistent
# with a genuine .p7b signed-policy artifact. Served at the EXACT
# capitalization Windows Boot Manager requested on the wire in Phase 3/5:
# EFI/Microsoft/Boot/Policies/WinSiPolicy.p7b.
#
# The ONLY intended boot-relevant change from Phase 3 is this one
# additional served asset. DHCP subnet/range, the efi-x64 architecture
# match (option 93 = 7), the DHCP-offered boot file path
# (EFI/Boot/bootx64.efi, with the ORIGINAL media bytes), the BCD
# asset/path/contents, NetworkManager/firewall isolation on enp8s0, the
# full-packet capture instrumentation, and the 10-minute dnsmasq ceiling
# are all otherwise UNCHANGED from Phase 3.
#
# Deliberately NOT pre-staged, by design, to observe the real next
# boundary empirically rather than assume it: Boot/boot.sdi,
# sources/boot.wim, winload.efi, SbcpFlightToken.p7b,
# SecureBootPolicy.p7b, SiPolicy.p7b, SkuSiPolicy.p7b, ATPSiPolicy.p7b,
# VbsSiPolicy.p7b, wgl4_boot.ttf, a bootmgfw.efi substitution,
# wdsmgfw.efi, a WDS server, shim, GRUB, iPXE, wimboot, Windows Setup.
#
# Safety note: this script stages three opaque, already-provenanced
# artifacts (a Microsoft-signed EFI binary, a stock Windows registry
# hive, and a stock PKCS#7 policy blob). It authors no configuration
# script of its own, so there is nothing for this script to grep for
# disk-touching directives. This script does not configure or execute
# any disk-writing, partition, format, install, or destructive storage
# action. It does not prove the absence of disk reads/writes performed
# by the Windows Boot Manager itself past this harness's TFTP root -
# that remains unproven Endpoint-side behavior, not something this
# harness controls.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself.

set -euo pipefail

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-winpe-phase6"
TFTP_ROOT="${SPIKE_DIR}/tftp"
WINBOOT_DIR="${TFTP_ROOT}/EFI/Boot"
BOOT_DIR="${TFTP_ROOT}/Boot"
POLICIES_DIR="${TFTP_ROOT}/EFI/Microsoft/Boot/Policies"
BOOTX64_SOURCE="/tmp/bamep-issue53-bootx64.efi"
BCD_SOURCE="/tmp/bamep-issue53-BCD"
WINSIPOLICY_SOURCE="/tmp/bamep-issue53-WinSiPolicy.p7b"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-dora-tftp.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"

EXPECTED_BOOTX64_SHA256="b2355c3e8a5fa140afb147d16646a7aa497a87d41937254b4925811614ba78a6"
EXPECTED_BOOTX64_SIZE="2772912"
EXPECTED_BCD_SHA256="21bf8054adfe0614baba6f21a4bad0b7bfe71dbe9169d2422de42a79258beba0"
EXPECTED_BCD_SIZE="262144"
EXPECTED_WINSIPOLICY_SHA256="186d4262d6301d235ce38680b16dc3f3236a8415135087e781133ee7598974e7"
EXPECTED_WINSIPOLICY_SIZE="10341"

echo "== Bamep Issue #53 Phase 6 - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Baseline:  Phase 3 (original media bootx64.efi + stock BCD) - NOT Phase 5"
echo "   Assets:    EFI/Boot/bootx64.efi + Boot/BCD (both unchanged from Phase 3)"
echo "              + EFI/Microsoft/Boot/Policies/WinSiPolicy.p7b (NEW this run)"
echo

echo "-- Pre-check 0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-winpe-phase6-cleanup.sh --purge"
    exit 1
fi
echo "OK: no stale ${SPIKE_DIR} present."
echo

echo "-- Pre-check 0b: bootx64.efi source artifact present and matches recorded provenance (ORIGINAL media bytes) --"
if [ ! -f "${BOOTX64_SOURCE}" ]; then
    echo "ABORT: ${BOOTX64_SOURCE} not found."
    exit 1
fi
ACTUAL_BOOTX64_SIZE="$(stat -c '%s' "${BOOTX64_SOURCE}")"
ACTUAL_BOOTX64_SHA256="$(sha256sum "${BOOTX64_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_BOOTX64_SIZE}" != "${EXPECTED_BOOTX64_SIZE}" ]; then
    echo "ABORT: ${BOOTX64_SOURCE} size ${ACTUAL_BOOTX64_SIZE} != expected ${EXPECTED_BOOTX64_SIZE}."
    exit 1
fi
if [ "${ACTUAL_BOOTX64_SHA256}" != "${EXPECTED_BOOTX64_SHA256}" ]; then
    echo "ABORT: ${BOOTX64_SOURCE} SHA-256 ${ACTUAL_BOOTX64_SHA256} != expected ${EXPECTED_BOOTX64_SHA256}."
    exit 1
fi
echo "OK: ${BOOTX64_SOURCE} matches recorded size (${ACTUAL_BOOTX64_SIZE}) and SHA-256 (${ACTUAL_BOOTX64_SHA256})."
echo

echo "-- Pre-check 0c: BCD source artifact present and matches recorded provenance --"
if [ ! -f "${BCD_SOURCE}" ]; then
    echo "ABORT: ${BCD_SOURCE} not found."
    exit 1
fi
ACTUAL_BCD_SIZE="$(stat -c '%s' "${BCD_SOURCE}")"
ACTUAL_BCD_SHA256="$(sha256sum "${BCD_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_BCD_SIZE}" != "${EXPECTED_BCD_SIZE}" ]; then
    echo "ABORT: ${BCD_SOURCE} size ${ACTUAL_BCD_SIZE} != expected ${EXPECTED_BCD_SIZE}."
    exit 1
fi
if [ "${ACTUAL_BCD_SHA256}" != "${EXPECTED_BCD_SHA256}" ]; then
    echo "ABORT: ${BCD_SOURCE} SHA-256 ${ACTUAL_BCD_SHA256} != expected ${EXPECTED_BCD_SHA256}."
    exit 1
fi
echo "OK: ${BCD_SOURCE} matches recorded size (${ACTUAL_BCD_SIZE}) and SHA-256 (${ACTUAL_BCD_SHA256})."
echo

echo "-- Pre-check 0d: WinSiPolicy.p7b source artifact present and matches recorded provenance --"
if [ ! -f "${WINSIPOLICY_SOURCE}" ]; then
    echo "ABORT: ${WINSIPOLICY_SOURCE} not found."
    exit 1
fi
ACTUAL_WINSIPOLICY_SIZE="$(stat -c '%s' "${WINSIPOLICY_SOURCE}")"
ACTUAL_WINSIPOLICY_SHA256="$(sha256sum "${WINSIPOLICY_SOURCE}" | awk '{print $1}')"
if [ "${ACTUAL_WINSIPOLICY_SIZE}" != "${EXPECTED_WINSIPOLICY_SIZE}" ]; then
    echo "ABORT: ${WINSIPOLICY_SOURCE} size ${ACTUAL_WINSIPOLICY_SIZE} != expected ${EXPECTED_WINSIPOLICY_SIZE}."
    exit 1
fi
if [ "${ACTUAL_WINSIPOLICY_SHA256}" != "${EXPECTED_WINSIPOLICY_SHA256}" ]; then
    echo "ABORT: ${WINSIPOLICY_SOURCE} SHA-256 ${ACTUAL_WINSIPOLICY_SHA256} != expected ${EXPECTED_WINSIPOLICY_SHA256}."
    exit 1
fi
echo "OK: ${WINSIPOLICY_SOURCE} matches recorded size (${ACTUAL_WINSIPOLICY_SIZE}) and SHA-256 (${ACTUAL_WINSIPOLICY_SHA256})."
echo

echo "-- Pre-check 1: no existing DHCP/TFTP/PXE listener --"
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: a DHCP/TFTP/PXE listener already exists. Investigate before proceeding."
    ss -lunp | grep -E ':67|:68|:69|:4011'
    exit 1
fi
echo "OK: no DHCP/TFTP/PXE listener present."
echo

echo "-- Pre-check 2: no local IPv4 address already in ${SUBNET} --"
if ip -4 addr show 2>/dev/null | grep -qF '192.168.99.'; then
    echo "ABORT: an address in ${SUBNET} already exists on this host."
    ip -4 addr show | grep -F '192.168.99.'
    exit 1
fi
echo "OK: no local address in ${SUBNET}."
echo

echo "-- Pre-check 3: no existing route for ${SUBNET} --"
if ip route show 2>/dev/null | grep -qF "${SUBNET}"; then
    echo "ABORT: a route for ${SUBNET} already exists."
    ip route show | grep -F "${SUBNET}"
    exit 1
fi
echo "OK: no existing route for ${SUBNET}."
echo

echo "-- Pre-check 4: current ${IFACE} state --"
ip -4 addr show "${IFACE}"
echo

echo "== Step 0: take ${IFACE} out of NetworkManager's automatic management (runtime only) =="
sudo nmcli device set "${IFACE}" managed no
echo

echo "== Step 1: add temporary address (exact add, no flush) =="
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

echo "== Step 2: runtime-only firewalld scope for this throwaway isolated Spike =="
echo "   THROWAWAY SPIKE CONFIGURATION ONLY."
echo "   This is NOT the future Bamep appliance firewall design."
sudo firewall-cmd --zone=trusted --change-interface="${IFACE}"
echo

echo "== Step 3: create the Spike directory tree (owned by brener, no sudo) =="
mkdir -p "${WINBOOT_DIR}" "${BOOT_DIR}" "${POLICIES_DIR}"
echo "Created ${WINBOOT_DIR}"
echo "Created ${BOOT_DIR}"
echo "Created ${POLICIES_DIR}"
echo

echo "== Step 4: copy the owner-supplied, already-verified artifacts (no sudo needed - already brener-readable) =="
install -m 0644 "${BOOTX64_SOURCE}" "${WINBOOT_DIR}/bootx64.efi"
echo "Copied ${BOOTX64_SOURCE} -> ${WINBOOT_DIR}/bootx64.efi (unchanged from Phase 3, ORIGINAL media bytes)"
install -m 0644 "${BCD_SOURCE}" "${BOOT_DIR}/BCD"
echo "Copied ${BCD_SOURCE} -> ${BOOT_DIR}/BCD (unchanged from Phase 3)"
install -m 0644 "${WINSIPOLICY_SOURCE}" "${POLICIES_DIR}/WinSiPolicy.p7b"
echo "Copied ${WINSIPOLICY_SOURCE} -> ${POLICIES_DIR}/WinSiPolicy.p7b (NEW this run - exact capitalization preserved)"
echo

echo "== Step 5: hash all three copies =="
sha256sum "${WINBOOT_DIR}/bootx64.efi" "${BOOT_DIR}/BCD" "${POLICIES_DIR}/WinSiPolicy.p7b" | tee "${SPIKE_DIR}/sha256sums.txt"
echo

echo "== Step 5b: pin all three copies against the recorded provenance (abort on any mismatch) =="
COPY_BOOTX64_SHA256="$(sha256sum "${WINBOOT_DIR}/bootx64.efi" | awk '{print $1}')"
if [ "${COPY_BOOTX64_SHA256}" != "${EXPECTED_BOOTX64_SHA256}" ]; then
    echo "ABORT: staged bootx64.efi SHA-256 does not match recorded provenance."
    echo "  expected: ${EXPECTED_BOOTX64_SHA256}"
    echo "  actual:   ${COPY_BOOTX64_SHA256}"
    exit 1
fi
echo "OK: staged bootx64.efi SHA-256 matches recorded provenance (${COPY_BOOTX64_SHA256})."
COPY_BCD_SHA256="$(sha256sum "${BOOT_DIR}/BCD" | awk '{print $1}')"
if [ "${COPY_BCD_SHA256}" != "${EXPECTED_BCD_SHA256}" ]; then
    echo "ABORT: staged BCD SHA-256 does not match recorded provenance."
    echo "  expected: ${EXPECTED_BCD_SHA256}"
    echo "  actual:   ${COPY_BCD_SHA256}"
    exit 1
fi
echo "OK: staged BCD SHA-256 matches recorded provenance (${COPY_BCD_SHA256})."
COPY_WINSIPOLICY_SHA256="$(sha256sum "${POLICIES_DIR}/WinSiPolicy.p7b" | awk '{print $1}')"
if [ "${COPY_WINSIPOLICY_SHA256}" != "${EXPECTED_WINSIPOLICY_SHA256}" ]; then
    echo "ABORT: staged WinSiPolicy.p7b SHA-256 does not match recorded provenance."
    echo "  expected: ${EXPECTED_WINSIPOLICY_SHA256}"
    echo "  actual:   ${COPY_WINSIPOLICY_SHA256}"
    exit 1
fi
echo "OK: staged WinSiPolicy.p7b SHA-256 matches recorded provenance (${COPY_WINSIPOLICY_SHA256})."
echo

echo "== Step 6: author dnsmasq.conf (THROWAWAY SPIKE CONFIG ONLY) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 6 - throwaway harness. NOT production configuration.
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
dhcp-boot=tag:efi-x64,EFI/Boot/bootx64.efi
EOF
echo "Wrote ${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 7: pre-create log/lease files, owner dnsmasq, group brener, mode 0640 =="
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.log"
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.leases"
echo

echo "== Step 8: validate readability/traversal for the dnsmasq runtime user =="
sudo -u dnsmasq test -x "${SPIKE_DIR}" && echo "OK: ${SPIKE_DIR} traversable by dnsmasq" || { echo "ABORT: ${SPIKE_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}" && echo "OK: ${TFTP_ROOT} traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/EFI" && echo "OK: ${TFTP_ROOT}/EFI traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT}/EFI not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${WINBOOT_DIR}" && echo "OK: ${WINBOOT_DIR} traversable by dnsmasq" || { echo "ABORT: ${WINBOOT_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${WINBOOT_DIR}/bootx64.efi" && echo "OK: bootx64.efi readable by dnsmasq" || { echo "ABORT: bootx64.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${BOOT_DIR}" && echo "OK: ${BOOT_DIR} traversable by dnsmasq" || { echo "ABORT: ${BOOT_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${BOOT_DIR}/BCD" && echo "OK: BCD readable by dnsmasq" || { echo "ABORT: BCD not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/EFI/Microsoft" && echo "OK: ${TFTP_ROOT}/EFI/Microsoft traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT}/EFI/Microsoft not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/EFI/Microsoft/Boot" && echo "OK: ${TFTP_ROOT}/EFI/Microsoft/Boot traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT}/EFI/Microsoft/Boot not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${POLICIES_DIR}" && echo "OK: ${POLICIES_DIR} traversable by dnsmasq" || { echo "ABORT: ${POLICIES_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${POLICIES_DIR}/WinSiPolicy.p7b" && echo "OK: WinSiPolicy.p7b readable by dnsmasq" || { echo "ABORT: WinSiPolicy.p7b not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.log" && echo "OK: dnsmasq.log writable by dnsmasq" || { echo "ABORT: dnsmasq.log not writable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.leases" && echo "OK: dnsmasq.leases writable by dnsmasq" || { echo "ABORT: dnsmasq.leases not writable by dnsmasq"; exit 1; }
echo

echo "== Step 9: validate dnsmasq config syntax without binding any socket =="
sudo dnsmasq --test --conf-file="${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 10: final pre-flight - still no listener before actually starting =="
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: unexpected listener present just before start."
    exit 1
fi
echo "OK: still no DHCP/TFTP/PXE listener."
echo

echo "== Step 11: start dnsmasq in the background (10-minute ceiling) =="
sudo timeout 600 dnsmasq -d --conf-file="${SPIKE_DIR}/dnsmasq.conf" \
    > "${SPIKE_DIR}/dnsmasq-stdout.log" 2>&1 &
DNSMASQ_PID=$!

echo "== Step 12: start packet capture in the background - ALL traffic on ${IFACE}, no protocol filter =="
echo "   (instrumentation-only change from Phase 2: enp8s0 is a direct point-to-point"
echo "    cable to the one physical Endpoint, so capturing everything here still does"
echo "    not expose/alter any other network. This lets TFTP DATA/ACK on ephemeral"
echo "    ports be measured, which the Phase 2 udp:67/68/69-only filter could not.)"
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
    echo
    echo "Next: run issue53-winpe-phase6-cleanup.sh to revert IP/firewall/NetworkManager state."
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
echo "    udp/67 and udp/69 listening."
echo

echo "HARNESS READY - trigger UEFI PXE IPv4 now"
echo

echo "Waiting for dnsmasq to exit (Ctrl-C to stop early, 10-minute ceiling otherwise)..."
wait "${DNSMASQ_PID}"
