#!/usr/bin/env bash
#
# Bamep Issue #52 - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review, and do not execute it automatically from any agent - it must be
# run interactively by the owner.
#
# Reverts every runtime change made by issue53-winpe-phase6-setup-run.sh
# and reports residual state. By default this script does NOT delete the
# evidence directory - pass --purge as the only argument to remove it,
# after you have copied out whatever you need from it.
#
# Issue #53 Phase 6: returns to the Phase 3 baseline (original media
# bootx64.efi + stock BCD, NOT the Phase 5 bootmgfw.efi substitute) and
# adds exactly one further asset - the stock WinSiPolicy.p7b repeatedly
# requested in Phase 3/5 - under the same physical Secure Boot state
# (diskless, Secure Boot Enabled/Active/Standard, factory keys restored).
# See issue53-winpe-phase6-setup-run.sh for the full context and
# artifact provenance.

set -u

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-winpe-phase6"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
PURGE="no"

if [ "${1:-}" = "--purge" ]; then
    PURGE="yes"
fi

echo "== Bamep Issue #53 Phase 6 - CLEANUP =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo "   Purge evidence: ${PURGE}"
echo

echo "-- Stop any leftover dnsmasq/tcpdump tied to this Spike --"
if sudo pkill -INT -f "dnsmasq .*${SPIKE_DIR}/dnsmasq.conf" 2>/dev/null; then
    echo "Stopped a running spike dnsmasq."
else
    echo "No spike dnsmasq running."
fi
if sudo pkill -INT -f "tcpdump .*${SPIKE_DIR}/pxe-dora-tftp.pcap" 2>/dev/null; then
    echo "Stopped a running spike tcpdump."
else
    echo "No spike tcpdump running."
fi
sleep 1
echo

echo "-- Remove the temporary address (exact del, only if present) --"
if ip -4 addr show "${IFACE}" | grep -qF "${ADDR_HOST}"; then
    sudo ip addr del "${ADDR}" dev "${IFACE}"
    echo "Removed ${ADDR} from ${IFACE}."
else
    echo "No ${ADDR} present on ${IFACE}; nothing to remove."
fi
echo

echo "-- Revert firewalld runtime scope (deterministic, privileged query) --"
if sudo firewall-cmd --zone=trusted --query-interface="${IFACE}" >/dev/null 2>&1; then
    sudo firewall-cmd --zone=trusted --remove-interface="${IFACE}"
    echo "Removed ${IFACE} from the trusted zone."
else
    echo "${IFACE} is not in the trusted zone."
fi
echo

echo "-- Verify firewalld (privileged query) --"
if sudo firewall-cmd --zone=trusted --query-interface="${IFACE}" >/dev/null 2>&1; then
    echo "WARNING: ${IFACE} is still in the trusted zone."
else
    echo "OK: ${IFACE} is not in the trusted zone."
fi
echo

echo "-- Return ${IFACE} to NetworkManager control --"
sudo nmcli device set "${IFACE}" managed yes
echo

echo "-- Verify reverted state --"
ip -4 addr show "${IFACE}"
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "WARNING: a DHCP/TFTP/PXE listener is still present:"
    ss -lunp | grep -E ':67|:68|:69|:4011'
else
    echo "OK: no DHCP/TFTP/PXE listener remains."
fi
echo

echo "-- Evidence at ${SPIKE_DIR} --"
if [ -d "${SPIKE_DIR}" ]; then
    ls -la "${SPIKE_DIR}"
    echo
    ls -la "${SPIKE_DIR}/tftp/EFI/Boot" 2>/dev/null
    echo
    ls -la "${SPIKE_DIR}/tftp/Boot" 2>/dev/null
    echo
    ls -la "${SPIKE_DIR}/tftp/EFI/Microsoft/Boot/Policies" 2>/dev/null
    echo
    ls -la "${SPIKE_DIR}/tftp/Boot" 2>/dev/null
    echo
    if [ -f "${SPIKE_DIR}/sha256sums.txt" ]; then
        cat "${SPIKE_DIR}/sha256sums.txt"
    fi
else
    echo "(${SPIKE_DIR} does not exist)"
fi
echo

if [ "${PURGE}" = "yes" ]; then
    echo "-- --purge given: removing ${SPIKE_DIR} now --"
    sudo rm -rf "${SPIKE_DIR}"
    echo "Removed."
else
    echo "-- Evidence directory kept. --"
    echo "   Copy out whatever you need from ${SPIKE_DIR}, then re-run:"
    echo "     $0 --purge"
    echo "   to delete it."
fi
