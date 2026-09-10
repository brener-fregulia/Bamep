# shellcheck shell=bash
#
# Pure log-parsing helpers for the Issue #71 WinPE UEFI-PXE host proof
# (scripts/bve-winpe-pxe-proof.sh). No side effects — safe to source and unit
# test (scripts/bve-winpe-pxe-proof-parser-test.sh).
#
# All evidence is read from the real fixture log only (dnsmasq --log-dhcp
# --log-queries + `python3 -m http.server`), never from run-bve's stdout.

# _rx_escape <literal>  ->  the literal with ERE metacharacters backslash-escaped
_rx_escape() { printf '%s' "$1" | sed 's/[.[\*^$()+?{|]/\\&/g'; }

# http_200 <fixture-log> <literal url path>
#   0 iff `python3 -m http.server` logged a 200 for exactly that GET.
#   The '.' in a path like /boot.sdi is matched literally, and HTTP/1.0 or
#   HTTP/1.1 both count. python always appends " <code> -", so the trailing
#   space disambiguates 200 from 2001/2000.
http_200() {
    local log="$1" path
    path="$(_rx_escape "$2")"
    grep -qE "\"GET ${path} HTTP/1\.[01]\" 200 " -- "$log"
}

# http_transfer_started <fixture-log> <literal url path>
#   0 on a 200 OR a 206 (partial) — a full 340 MB delivery is not proven by the
#   status line alone; reaching WinPE-ready is the downstream evidence.
http_transfer_started() {
    local log="$1" path
    path="$(_rx_escape "$2")"
    grep -qE "\"GET ${path} HTTP/1\.[01]\" 20[06] " -- "$log"
}

# winpe_dhcp_ack_in_range <fixture-log> <start> <end> <bve-mac>
#   0 iff, within the 1-indexed inclusive line range [start,end], the fixture
#   log shows a Windows-client DHCP transaction that reached DHCPACK for the
#   BVE MAC: a WinPE identity line (vendor class "MSFT 5.0", or a "minint"
#   client name) at some line L, followed at line >= L by a DHCPACK line that
#   carries the BVE MAC. dnsmasq logs a transaction's lines contiguously in
#   order (identity/DISCOVER .. REQUEST .. ACK), and the PXE/iPXE ACKs for the
#   same MAC come *before* any WinPE identity line, so this rules out crediting
#   a WinPE boot for a bare PXE lease. Not a general DHCP parser.
winpe_dhcp_ack_in_range() {
    local log="$1" start="$2" end="$3" mac="$4"
    [[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ ]] || return 1
    (( end >= start )) || return 1
    sed -n "${start},${end}p" -- "$log" | awk -v mac="$(printf '%s' "$mac" | tr 'A-Z' 'a-z')" '
        { l = tolower($0) }
        (l ~ /msft 5\.0/) || (l ~ /minint/)      { if (idl == 0) idl = NR }
        (l ~ /dhcpack/) && (index(l, mac) > 0)   { ackl = NR }
        END { exit (idl > 0 && ackl >= idl) ? 0 : 1 }
    '
}
