# shellcheck shell=bash
#
# Pure fixture-log parsing helpers for the Issue #73 BARE UEFI-PXE host proof
# (scripts/bve-bare-pxe-proof.sh). No side effects — safe to source and unit
# test (scripts/bve-bare-pxe-proof-parser-test.sh).
#
# This is the HOST-SIDE provisioning authority only: dnsmasq (--log-dhcp
# --log-queries) + `python3 -m http.server`, both captured into ONE fixture
# log by `start-fixture` (crates/ve/examples/bve_bare_pxe.rs) — never the BVE
# guest serial console. BARE readiness (BARE_READY / BARE_NET_READY) is a
# SEPARATE authority, read from the guest serial capture, and already owned
# by scripts/lib/bare-serial-evidence.sh (Issue #72) — reused here unchanged,
# never re-implemented.
#
# The Issue #73 spike found that trusting the guest serial console alone is
# not sufficient evidence that the HOST actually transferred bzImage /
# rootfs.cpio.gz over HTTP — the two authorities must both be proven, and
# proven independently per boot (a boot #1 line must never satisfy boot #2).

# _in_range <fixture-log> <start> <end> -> the 1-indexed inclusive line slice.
# Fails closed on a non-numeric or reversed/empty range.
_bare_pxe_in_range() {
	local log="$1" start="$2" end="$3"
	[[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ ]] || return 1
	((end >= start)) || return 1
	sed -n "${start},${end}p" -- "$log"
}

# _rx_escape <literal> -> the literal with ERE metacharacters backslash-escaped.
_rx_escape() { printf '%s' "$1" | sed 's/[.[\*^$()+?{|]/\\&/g'; }

# uefi_pxe_attempt_in_range <fixture-log> <start> <end> <bve-mac>
#   0 iff, within the range, dnsmasq logged a DHCPDISCOVER for <bve-mac> that
#   also carries the EFI x86-64 architecture tag (option:client-arch,7) —
#   i.e. the firmware actually attempted a UEFI network boot, not just any
#   DHCP client on the segment.
uefi_pxe_attempt_in_range() {
	local log="$1" start="$2" end="$3" mac
	mac="$(printf '%s' "$4" | tr 'A-Z' 'a-z')"
	_bare_pxe_in_range "$log" "$start" "$end" | awk -v mac="$mac" '
        { l = tolower($0) }
        (l ~ /dhcpdiscover/) && (index(l, mac) > 0) { d = NR }
        (l ~ /tags:.*efi-x64/) || (l ~ /client-arch/)  { a = NR }
        END { exit (d > 0 && a > 0) ? 0 : 1 }
    '
}

# dhcp_ack_in_range <fixture-log> <start> <end> <bve-mac>
#   0 iff a DHCPACK for <bve-mac> appears in the range — proves at least one
#   complete DHCP bootstrap (PXE-stage or iPXE-stage) closed inside this boot.
dhcp_ack_in_range() {
	local log="$1" start="$2" end="$3" mac
	mac="$(printf '%s' "$4" | tr 'A-Z' 'a-z')"
	_bare_pxe_in_range "$log" "$start" "$end" | grep -qiE "dhcpack.*${mac}"
}

# snponly_tftp_in_range <fixture-log> <start> <end>
#   0 iff dnsmasq logged sending snponly.efi over TFTP in the range.
snponly_tftp_in_range() {
	_bare_pxe_in_range "$@" | grep -qE 'sent .*snponly\.efi'
}

# ipxe_boot_script_selected_in_range <fixture-log> <start> <end>
#   0 iff iPXE's second-stage DHCP (user-class iPXE) or its GET /boot.ipxe
#   appears in the range — iPXE is running and requested the BARE chain
#   script.
ipxe_boot_script_selected_in_range() {
	_bare_pxe_in_range "$@" | grep -qE 'user class: iPXE|GET /boot\.ipxe'
}

# http_get_200_in_range <fixture-log> <start> <end> <literal url path>
#   0 iff `python3 -m http.server`'s access log shows a 200 or 206 (partial)
#   GET for EXACTLY that path within the range. The path's `.` is matched
#   literally (not "any character"); a 404 never counts; a request outside
#   the given range never counts (the range slice is taken before matching).
http_get_200_in_range() {
	local log="$1" start="$2" end="$3" path
	path="$(_rx_escape "$4")"
	_bare_pxe_in_range "$log" "$start" "$end" | grep -qE "\"GET ${path} HTTP/1\.[01]\" 20[06] "
}
