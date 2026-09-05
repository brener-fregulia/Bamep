#!/usr/bin/env bash
#
# Bamep Issue #53 Phase 9a - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review, and do not execute it automatically from any agent - it must be
# run interactively by the owner.
#
# Reverts every runtime change made by issue53-phase9a-setup-run.sh and
# reports residual state. By default this script does NOT delete the
# evidence directory - pass --purge as the only argument to remove it,
# after you have copied out whatever you need from it.
#
# Issue #53 Phase 9a: official iPXE v2.0.0 Secure Boot chain
# (snponly-shim.efi -> snponly.efi -> autoexec.ipxe -> iPXE shell) under
# physical Secure Boot Enabled/Active/Standard, diskless Endpoint. See
# issue53-phase9a-setup-run.sh for full context and artifact provenance.

set -u

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-phase9a-ipxe-shim"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
PURGE="no"

if [ "${1:-}" = "--purge" ]; then
    PURGE="yes"
fi

# Consistency fix (post-hoc, after the physical Phase 9a run showed cleanup
# emitting a spurious warning): same discrimination logic as setup's Gate
# C2. A listener bound exclusively to another specific address (Tailscale/
# management/loopback) is NOT a conflict; only a wildcard bind or a bind to
# ${ADDR_HOST} itself is. This does not alter anything about the already-
# recorded physical boot result.
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
                echo "benign, interface-specific (not a conflict): ${addr}:${port}"
                ;;
        esac
    done < <(ss -Hltn 2>/dev/null | awk '{print $4}')
    return "${conflict_found}"
}

echo "== Bamep Issue #53 Phase 9a - CLEANUP =="
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
if sudo pkill -INT -f "tcpdump .*${SPIKE_DIR}/pxe-shim.pcap" 2>/dev/null; then
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
if http_like_listener_conflicts_with_lab_path; then
    echo "WARNING: an HTTP-like listener bound to a lab-reachable address is present."
else
    echo "OK: no HTTP-like listener bound to a wildcard address or ${ADDR_HOST}."
    echo "    (interface-specific/management listeners, e.g. Tailscale, are not flagged)"
fi
echo

echo "-- Evidence at ${SPIKE_DIR} --"
if [ -d "${SPIKE_DIR}" ]; then
    ls -la "${SPIKE_DIR}"
    echo
    ls -la "${SPIKE_DIR}/tftp/ipxeboot/x86_64-sb" 2>/dev/null
    echo
    if [ -f "${SPIKE_DIR}/sha256sums.txt" ]; then
        cat "${SPIKE_DIR}/sha256sums.txt"
    fi
else
    echo "(${SPIKE_DIR} does not exist)"
fi
echo

echo "-- Preservation check: prior Issue #53 evidence directories are untouched --"
ls -la /var/tmp/ | grep -E 'bamep-issue53-' || echo "(none found under /var/tmp - unexpected, investigate)"
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
