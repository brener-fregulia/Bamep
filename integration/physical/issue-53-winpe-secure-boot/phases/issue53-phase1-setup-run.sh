#!/usr/bin/env bash
#
# Bamep Issue #52 - THROWAWAY Spike operator script.
#
# NOT part of the Bamep repository. NOT production configuration.
# Do NOT stage/commit this file. Do NOT execute it without explicit owner
# review of every step below, and do not execute it automatically from
# any agent - it must be run interactively by the owner.
#
# Purpose: minimal, reversible, isolated harness to observe whether the
# physical mini PC (UEFI PXE IPv4, MAC e8:ff:1e:d6:2e:f5) completes a full
# DHCP DORA and transfers/executes an INERT UEFI x86-64 boot file over the
# direct point-to-point link "Fedora Server enp8s0 <-> mini PC".
#
# Safety note on the inert payload: this script only asserts that no
# disk-writing, install, or destructive command is configured or
# intentionally executed by the grub.cfg it authors (verified below by
# grep). It does not claim to have empirically proven the absence of any
# disk read by firmware/shim/grub outside that authored config.
#
# ISSUE #53 - PHASE 1: rerun the known-good #52 Run 2 chain under a new
# physical firmware state. Physical fixture is now confirmed by the owner
# as: fully diskless (M.2 SATA SSD and HDD removed), System Mode: User,
# Secure Boot: Enabled, Secure Boot status: Active, Secure Boot Mode:
# Standard, factory/default keys restored immediately before enabling
# Secure Boot. Same physical Endpoint as #50/#52 (MAC e8:ff:1e:d6:2e:f5).
#
# This script is otherwise byte-identical in boot-relevant behavior to the
# known-good #52 Run 2 harness (classified A: DORA -> shim -> root-level
# grubx64.efi -> EFI/fedora/grub.cfg -> visible inert GRUB menu
# "Bamep Spike #52 - inert PXE test payload"). Only the evidence directory
# changed, plus a new Step 5b that pins the freshly copied EFI artifacts to
# the exact SHA-256 hashes recorded as #52 evidence and aborts before any
# DHCP/TFTP service starts if they do not match bit-for-bit.
#
# Deliberately UNCHANGED from #52 Run 2: DHCP subnet/range, the efi-x64
# architecture match (option 93 = 7), the offered shim path
# (EFI/fedora/shimx64.efi), the root-level grubx64.efi copy, the inert
# grub.cfg content/location/message, NetworkManager/firewall isolation on
# enp8s0. Deliberately NOT pre-supplied (unchanged from #52): revocations_
# sku.efi, revocations_sbat.efi, shim_certificate_0.efi, and no grub.cfg
# copy at the TFTP root. Do not add these unless new evidence under Secure
# Boot proves them relevant.
#
# The variable under test this time is entirely external to this script:
# physical Secure Boot enforcement in the mini-PC firmware, not the harness.
#
# Nothing here runs automatically on your behalf. Read it, then run it
# yourself.

set -euo pipefail

IFACE="enp8s0"
SPIKE_DIR="/var/tmp/bamep-issue53-secureboot-phase1"
TFTP_ROOT="${SPIKE_DIR}/tftp"
EFI_DIR="${TFTP_ROOT}/EFI/fedora"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"
SUBNET="192.168.99.0/24"
CAPTURE_PCAP="${SPIKE_DIR}/pxe-dora-tftp.pcap"
CAPTURE_LOG="${SPIKE_DIR}/tcpdump-stderr.log"

echo "== Bamep Issue #53 Phase 1 - SETUP + RUN (throwaway, reversible) =="
echo "   Interface: ${IFACE}"
echo "   Spike dir: ${SPIKE_DIR}"
echo

echo "-- Pre-check 0: no stale evidence directory from a previous run --"
if [ -e "${SPIKE_DIR}" ]; then
    echo "ABORT: ${SPIKE_DIR} already exists. Preserve/purge the previous evidence first."
    echo "  Inspect it, or run: issue53-phase1-cleanup.sh --purge"
    exit 1
fi
echo "OK: no stale ${SPIKE_DIR} present."
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
mkdir -p "${EFI_DIR}"
echo "Created ${EFI_DIR}"
echo

echo "== Step 4: copy local Fedora shim/GRUB assets, chown to brener at copy time =="
sudo install -o brener -g brener -m 0644 \
    /usr/lib/efi/shim/16.1-5/EFI/fedora/shimx64.efi \
    "${EFI_DIR}/shimx64.efi"
sudo install -o brener -g brener -m 0644 \
    /usr/lib/efi/shim/16.1-5/EFI/fedora/mmx64.efi \
    "${EFI_DIR}/mmx64.efi"
sudo install -o brener -g brener -m 0644 \
    "/usr/lib/efi/grub2/1:2.12-64.fc44/EFI/fedora/grubx64.efi" \
    "${EFI_DIR}/grubx64.efi"
echo "== Step 4b: RUN 2 variable - additionally expose the same grubx64.efi at the TFTP root =="
sudo install -o brener -g brener -m 0644 \
    "/usr/lib/efi/grub2/1:2.12-64.fc44/EFI/fedora/grubx64.efi" \
    "${TFTP_ROOT}/grubx64.efi"
echo

echo "== Step 5: hash the copies (no sudo needed, already brener-readable) =="
sha256sum "${EFI_DIR}"/*.efi "${TFTP_ROOT}/grubx64.efi" | tee "${SPIKE_DIR}/sha256sums.txt"
echo

echo "== Step 5b: pin against the exact #52 evidence hashes (abort on any mismatch) =="
EXPECTED_SHIM_SHA256="571ea56b855dcf73bec6acb63c5ded44c2a191138bca0d8cfa5aa93f60f46fff"
EXPECTED_MM_SHA256="f8af592759c8ab33b69c4b0e772da5a8e2aa6d09c7dbd5e24c62c89fa5fdbd05"
EXPECTED_GRUB_SHA256="db283a408682e92dabec2c2098576c2a6e374e714320124a0161136c5b326095"

check_hash() {
    local file="$1" expected="$2" label="$3"
    local actual
    actual="$(sha256sum "${file}" | awk '{print $1}')"
    if [ "${actual}" != "${expected}" ]; then
        echo "ABORT: ${label} SHA-256 does not match #52 evidence."
        echo "  file:     ${file}"
        echo "  expected: ${expected}"
        echo "  actual:   ${actual}"
        echo "  This EFI artifact changed since #52 (e.g. a package update)."
        echo "  Re-baseline the #52 evidence/comparison before proceeding."
        exit 1
    fi
    echo "OK: ${label} SHA-256 matches #52 evidence (${actual})."
}
check_hash "${EFI_DIR}/shimx64.efi" "${EXPECTED_SHIM_SHA256}" "shimx64.efi"
check_hash "${EFI_DIR}/mmx64.efi" "${EXPECTED_MM_SHA256}" "mmx64.efi"
check_hash "${EFI_DIR}/grubx64.efi" "${EXPECTED_GRUB_SHA256}" "EFI/fedora/grubx64.efi"
check_hash "${TFTP_ROOT}/grubx64.efi" "${EXPECTED_GRUB_SHA256}" "root-level grubx64.efi"
echo

echo "== Step 6: author the inert grub.cfg =="
cat > "${EFI_DIR}/grub.cfg" <<'EOF'
set timeout=-1
set timeout_style=menu

menuentry "Bamep Spike #52 - inert PXE test payload" {
    echo ""
    echo "=============================================="
    echo " Bamep Spike #52 - UEFI PXE payload executed"
    echo " This is a throwaway inert test payload."
    echo " No disk-writing or installer action is configured."
    echo "=============================================="
}
EOF
echo "Wrote ${EFI_DIR}/grub.cfg"
echo

echo "-- Verify grub.cfg contains no disk-touching directives --"
if grep -qiE '(^|[^a-z])(search|ls|probe|chainloader|linux|initrd|configfile|insmod[[:space:]]+(part_|fat|ext2|ntfs|hfs|iso9660|udf|zfs|lvm|mdraid|diskfilter|ahci|ata|scsi|usb))' "${EFI_DIR}/grub.cfg"; then
    echo "ABORT: grub.cfg contains a disallowed directive. Refusing to continue."
    exit 1
fi
echo "OK: grub.cfg contains no disk/search/chainload/insmod-storage directives."
echo

echo "== Step 7: author dnsmasq.conf (THROWAWAY SPIKE CONFIG ONLY) =="
cat > "${SPIKE_DIR}/dnsmasq.conf" <<EOF
# Bamep Spike #52 - throwaway harness. NOT production configuration.
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
dhcp-boot=tag:efi-x64,EFI/fedora/shimx64.efi
EOF
echo "Wrote ${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 8: pre-create log/lease files, owner dnsmasq, group brener, mode 0640 =="
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.log"
sudo install -o dnsmasq -g brener -m 0640 /dev/null "${SPIKE_DIR}/dnsmasq.leases"
echo

echo "== Step 9: validate readability/traversal for the dnsmasq runtime user =="
sudo -u dnsmasq test -x "${SPIKE_DIR}" && echo "OK: ${SPIKE_DIR} traversable by dnsmasq" || { echo "ABORT: ${SPIKE_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}" && echo "OK: ${TFTP_ROOT} traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${TFTP_ROOT}/EFI" && echo "OK: ${TFTP_ROOT}/EFI traversable by dnsmasq" || { echo "ABORT: ${TFTP_ROOT}/EFI not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -x "${EFI_DIR}" && echo "OK: ${EFI_DIR} traversable by dnsmasq" || { echo "ABORT: ${EFI_DIR} not traversable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${EFI_DIR}/shimx64.efi" && echo "OK: shimx64.efi readable by dnsmasq" || { echo "ABORT: shimx64.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${EFI_DIR}/mmx64.efi" && echo "OK: mmx64.efi readable by dnsmasq" || { echo "ABORT: mmx64.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${EFI_DIR}/grubx64.efi" && echo "OK: grubx64.efi readable by dnsmasq" || { echo "ABORT: grubx64.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${TFTP_ROOT}/grubx64.efi" && echo "OK: root grubx64.efi (RUN 2 variable) readable by dnsmasq" || { echo "ABORT: root grubx64.efi not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -r "${EFI_DIR}/grub.cfg" && echo "OK: grub.cfg readable by dnsmasq" || { echo "ABORT: grub.cfg not readable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.log" && echo "OK: dnsmasq.log writable by dnsmasq" || { echo "ABORT: dnsmasq.log not writable by dnsmasq"; exit 1; }
sudo -u dnsmasq test -w "${SPIKE_DIR}/dnsmasq.leases" && echo "OK: dnsmasq.leases writable by dnsmasq" || { echo "ABORT: dnsmasq.leases not writable by dnsmasq"; exit 1; }
echo

echo "== Step 10: validate dnsmasq config syntax without binding any socket =="
sudo dnsmasq --test --conf-file="${SPIKE_DIR}/dnsmasq.conf"
echo

echo "== Step 11: final pre-flight - still no listener before actually starting =="
if ss -lunp 2>/dev/null | grep -qE ':67|:68|:69|:4011'; then
    echo "ABORT: unexpected listener present just before start."
    exit 1
fi
echo "OK: still no DHCP/TFTP/PXE listener."
echo

echo "== Step 12: start dnsmasq in the background (10-minute ceiling) =="
sudo timeout 600 dnsmasq -d --conf-file="${SPIKE_DIR}/dnsmasq.conf" \
    > "${SPIKE_DIR}/dnsmasq-stdout.log" 2>&1 &
DNSMASQ_PID=$!

echo "== Step 13: start packet capture in the background (DHCP + TFTP + ARP only, on ${IFACE}) =="
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
    echo "Next: run issue53-phase1-cleanup.sh to revert IP/firewall/NetworkManager state."
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
