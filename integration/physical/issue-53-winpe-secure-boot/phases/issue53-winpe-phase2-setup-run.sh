#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 2 - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review of every step below, and do not execute it automatically from
# any agent - it must be run interactively by the owner.
#
# Purpose: narrow one-variable probe on top of the known-good #53 Phase 1
# harness (classified A: physical Secure Boot Enabled/Active/Standard
# accepted the Fedora shim->GRUB chain and the inert GRUB menu was visibly
# reached, on the diskless physical Endpoint, MAC e8:ff:1e:d6:2e:f5).
#
# Question: can the same diskless physical UEFI x86-64 Endpoint, with
# Secure Boot still Enabled/Active/Standard, PXE-download and execute the
# exact stock Microsoft-signed EFI\Boot\bootx64.efi Windows Boot Manager
# artifact?
#
# Artifact provenance (owner-supplied and independently re-verified on this
# Fedora host before this script was written):
#   Windows source: C:\BamepSpike\winpe_media\amd64\media\EFI\Boot\bootx64.efi
#   Fedora copy:    /tmp/bamep-issue53-bootx64.efi
#   Size:           2772912 bytes
#   SHA-256:        b2355c3e8a5fa140afb147d16646a7aa497a87d41937254b4925811614ba78a6
#   Authenticode:   Valid; Subject CN=Microsoft Windows, O=Microsoft
#                   Corporation, L=Redmond, S=Washington, C=US; Issuer
#                   CN=Microsoft Windows Production PCA 2011; signer
#                   thumbprint 71F53A26BB1625E466727183409A30D03D7923DF
# The retained WinPE media tree contains only this file and
# EFI\Microsoft\Boot\memtest.efi - no bootmgfw.efi, no wdsmgfw.efi are
# present in that tree, so neither is used here.
#
# Deliberately UNCHANGED from #53 Phase 1: DHCP subnet/range, the efi-x64
# architecture match (option 93 = 7), NetworkManager/firewall isolation on
# enp8s0, the DHCP/TFTP/ARP-only packet capture, the 10-minute dnsmasq
# ceiling. The ONLY intended boot-relevant change is the DHCP-offered/
# TFTP-served boot file: EFI/fedora/shimx64.efi (Phase 1) -> EFI/Boot/
# bootx64.efi (this Phase 2 run). No shim, no GRUB, no grub.cfg, no
# revocation/certificate files are staged this run - this artifact does
# not need shim (it is natively Microsoft-signed, not chained through a
# Fedora-trusted shim).
#
# Deliberately NOT pre-staged, by design, to observe the real next
# boundary: BCD, boot.sdi, boot.wim, Windows Setup, a WDS server,
# bootmgfw.efi, wdsmgfw.efi, GRUB, shim, iPXE, wimboot. If bootx64.efi
# requests another asset, or executes but cannot locate BCD/device/media,
# or is rejected by Secure Boot, record the exact boundary and STOP - do
# not add the missing piece in this same run.
#
# Safety note: this script stages one opaque, already Microsoft-signed
# binary. It authors no configuration script of its own (there is no
# grub.cfg-equivalent here), so there is nothing for this script to grep
# for disk-touching directives. This script does not configure or execute
# any disk-writing, partition, format, install, or destructive storage
# action. It does not prove the absence of disk reads/writes performed by
# the Windows Boot Manager itself if it runs past this harness's TFTP
# root and finds no further assets - that remains unproven Endpoint-side
# behavior, not something this harness controls.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself.

set -euo pipefail

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-winpe-phase2"
TFTP_ROOT="${SPIKE_DIR}/tftp"
WINBOOT_DIR="${TFTP_ROOT}/EFI/Boot"
SOURCE_ARTIFACT="/tmp/bamep-issue53-bootx64.efi"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-dora-tftp.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"

EXPECTED_BOOTX64_SHA256="b2355c3e8a5fa140afb147d16646a7aa497a87d41937254b4925811614ba78a6"
EXPECTED_BOOTX64_SIZE="2772912"

echo "== Bamep Issue #53 Phase 2 - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Candidate: EFI/Boot/bootx64.efi (stock Microsoft-signed Windows Boot Manager)"
echo

echo "-- Pre-check 0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-winpe-phase2-cleanup.sh --purge"
    exit 1
fi
echo "OK: no stale ${SPIKE_DIR} present."
echo

echo "-- Pre-check 0b: source artifact present and matches the recorded provenance --"
if [ ! -f "${SOURCE_ARTIFACT}" ]; then
    echo "ABORT: ${SOURCE_ARTIFACT} not found."
    exit 1
fi
ACTUAL_SIZE="$(stat -c '%s' "${SOURCE_ARTIFACT}")"
ACTUAL_SHA256="$(sha256sum "${SOURCE_ARTIFACT}" | awk '{print $1}')"
if [ "${ACTUAL_SIZE}" != "${EXPECTED_BOOTX64_SIZE}" ]; then
    echo "ABORT: ${SOURCE_ARTIFACT} size ${ACTUAL_SIZE} != expected ${EXPECTED_BOOTX64_SIZE}."
    exit 1
fi
if [ "${ACTUAL_SHA256}" != "${EXPECTED_BOOTX64_SHA256}" ]; then
    echo "ABORT: ${SOURCE_ARTIFACT} SHA-256 ${ACTUAL_SHA256} != expected ${EXPECTED_BOOTX64_SHA256}."
    exit 1
fi
echo "OK: ${SOURCE_ARTIFACT} matches recorded size (${ACTUAL_SIZE}) and SHA-256 (${ACTUAL_SHA256})."
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
mkdir -p "${WINBOOT_DIR}"
echo "Created ${WINBOOT_DIR}"
echo

echo "== Step 4: copy the owner-supplied, already-verified bootx64.efi (no sudo needed - already brener-readable) =="
install -m 0644 "${SOURCE_ARTIFACT}" "${WINBOOT_DIR}/bootx64.efi"
echo "Copied ${SOURCE_ARTIFACT} -> ${WINBOOT_DIR}/bootx64.efi"
echo

echo "== Step 5: hash the copy =="
sha256sum "${WINBOOT_DIR}/bootx64.efi" | tee "${SPIKE_DIR}/sha256sums.txt"
echo

echo "== Step 5b: pin the copy against the recorded provenance (abort on any mismatch) =="
COPY_SHA256="$(sha256sum "${WINBOOT_DIR}/bootx64.efi" | awk '{print $1}')"
if [ "${COPY_SHA256}" != "${EXPECTED_BOOTX64_SHA256}" ]; then
    echo "ABORT: staged bootx64.efi SHA-256 does not match recorded provenance."
    echo "  expected: ${EXPECTED_BOOTX64_SHA256}"
    echo "  actual:   ${COPY_SHA256}"
    exit 1
fi
echo "OK: staged bootx64.efi SHA-256 matches recorded provenance (${COPY_SHA256})."
echo

echo "== Step 6: author dnsmasq.conf (THROWAWAY SPIKE CONFIG ONLY) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #53 Phase 2 - throwaway harness. NOT production configuration.
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

echo "== Step 12: start packet capture in the background (DHCP + TFTP + ARP only, on ${IFACE}) =="
sudo tcpdump -ni "${IFACE}" -e -vvv -Z brener \
    -w "${CAPTURE_PCAP}" \
    '(udp port 67 or udp port 68 or udp port 69) or arp' \
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
    echo "Next: run issue53-winpe-phase2-cleanup.sh to revert IP/firewall/NetworkManager state."
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
