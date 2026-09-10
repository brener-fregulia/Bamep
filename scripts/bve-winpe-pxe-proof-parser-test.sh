#!/usr/bin/env bash
#
# Pure unit tests for scripts/lib/winpe-pxe-evidence.sh — the log-parsing
# helpers of the Issue #71 WinPE UEFI-PXE host proof. No sudo, no QEMU, no
# network. Fixtures are canonical `python3 -m http.server` and `dnsmasq
# --log-dhcp --log-queries` lines.
#
#   ./scripts/bve-winpe-pxe-proof-parser-test.sh

set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=scripts/lib/winpe-pxe-evidence.sh
source lib/winpe-pxe-evidence.sh

PASS=0
FAIL=0
ok()   { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1" >&2; }
check() { # check <desc> <expected 0|1> <cmd...>
    local desc="$1" want="$2"; shift 2
    if "$@"; then local got=0; else local got=1; fi
    if [[ "$got" == "$want" ]]; then ok "$desc"; else bad "$desc (want rc=$want, got rc=$got)"; fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
MAC="52:54:00:e4:a4:d4"

# --- fixture: a full, successful two-boot fixture log ----------------------
# Boot #1: PXE DORA -> TFTP -> iPXE DORA -> HTTP chain -> WinPE DORA (MSFT 5.0).
# Boot #2: same again, new transaction ids, WinPE DORA reaches DHCPACK.
cat >"$TMP/two-boots.log" <<EOF
dnsmasq: started, version 2.90 DNS disabled
dnsmasq-dhcp: 2895839652 available DHCP range: 192.0.2.50 -- 192.0.2.100
dnsmasq-dhcp: 2895839652 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 2895839652 DHCPDISCOVER(bvp54e4a4d4) 52:54:00:e4:a4:d4
dnsmasq-dhcp: 2895839652 tags: efi-x64, known, bvp54e4a4d4
dnsmasq-dhcp: 2895839652 DHCPOFFER(bvp54e4a4d4) 192.0.2.59 52:54:00:e4:a4:d4
dnsmasq-dhcp: 2895839652 next server: 192.0.2.1
dnsmasq-dhcp: 2895839652 sent size:  9 option: 67 bootfile-name  snponly.efi
dnsmasq-dhcp: 2895839652 DHCPREQUEST(bvp54e4a4d4) 192.0.2.59 52:54:00:e4:a4:d4
dnsmasq-dhcp: 2895839652 DHCPACK(bvp54e4a4d4) 192.0.2.59 52:54:00:e4:a4:d4
dnsmasq-tftp: sent /tmp/bamep-bve-fixture-54e4a4d4/tftp/snponly.efi to 192.0.2.59
dnsmasq-dhcp: 3902841012 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 3902841012 user class: iPXE
dnsmasq-dhcp: 3902841012 DHCPDISCOVER(bvp54e4a4d4) 52:54:00:e4:a4:d4
dnsmasq-dhcp: 3902841012 tags: efi-x64, ipxe, known, bvp54e4a4d4
dnsmasq-dhcp: 3902841012 DHCPACK(bvp54e4a4d4) 192.0.2.59 52:54:00:e4:a4:d4
192.0.2.59 - - [10/Sep/2026 12:34:55] "GET /boot.ipxe HTTP/1.1" 200 -
192.0.2.59 - - [10/Sep/2026 12:34:56] "GET /wimboot HTTP/1.0" 200 -
192.0.2.59 - - [10/Sep/2026 12:34:57] "GET /BCD HTTP/1.1" 200 -
192.0.2.59 - - [10/Sep/2026 12:34:58] "GET /boot.sdi HTTP/1.1" 200 -
192.0.2.59 - - [10/Sep/2026 12:35:20] "GET /boot.wim HTTP/1.1" 200 -
dnsmasq-dhcp: 1049283746 vendor class: MSFT 5.0
dnsmasq-dhcp: 1049283746 DHCPDISCOVER(bvp54e4a4d4) 52:54:00:e4:a4:d4
dnsmasq-dhcp: 1049283746 client provides name: minint-abc123
dnsmasq-dhcp: 1049283746 DHCPOFFER(bvp54e4a4d4) 192.0.2.60 52:54:00:e4:a4:d4
dnsmasq-dhcp: 1049283746 DHCPREQUEST(bvp54e4a4d4) 192.0.2.60 52:54:00:e4:a4:d4
dnsmasq-dhcp: 1049283746 DHCPACK(bvp54e4a4d4) 192.0.2.60 52:54:00:e4:a4:d4 minint-abc123
dnsmasq-dhcp: 2001112222 available DHCP range: 192.0.2.50 -- 192.0.2.100
dnsmasq-dhcp: 2001112222 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 2001112222 DHCPDISCOVER(bvp54e4a4d4) 52:54:00:e4:a4:d4
dnsmasq-dhcp: 2001112222 DHCPACK(bvp54e4a4d4) 192.0.2.59 52:54:00:e4:a4:d4
dnsmasq-tftp: sent /tmp/bamep-bve-fixture-54e4a4d4/tftp/snponly.efi to 192.0.2.59
192.0.2.59 - - [10/Sep/2026 12:40:56] "GET /wimboot HTTP/1.1" 200 -
192.0.2.59 - - [10/Sep/2026 12:40:57] "GET /BCD HTTP/1.1" 200 -
192.0.2.59 - - [10/Sep/2026 12:40:58] "GET /boot.sdi HTTP/1.1" 200 -
192.0.2.59 - - [10/Sep/2026 12:41:20] "GET /boot.wim HTTP/1.1" 200 -
dnsmasq-dhcp: 3339992222 vendor class: MSFT 5.0
dnsmasq-dhcp: 3339992222 DHCPDISCOVER(bvp54e4a4d4) 52:54:00:e4:a4:d4
dnsmasq-dhcp: 3339992222 client provides name: minint-abc123
dnsmasq-dhcp: 3339992222 DHCPACK(bvp54e4a4d4) 192.0.2.60 52:54:00:e4:a4:d4 minint-abc123
EOF
# Line ranges (1-indexed inclusive) as run-bve would emit them:
BOOT1_START=2 ; BOOT1_END=27   # PXE DORA .. first WinPE DHCPACK
BOOT2_START=28; BOOT2_END=42   # second PXE DORA .. second WinPE DHCPACK

# fixture: only boot #1 ever produced a WinPE DORA (boot #2 hung after iPXE)
sed -n "1,27p" "$TMP/two-boots.log" >"$TMP/one-winpe-dora.log"
cat >>"$TMP/one-winpe-dora.log" <<EOF
dnsmasq-dhcp: 2001112222 DHCPDISCOVER(bvp54e4a4d4) 52:54:00:e4:a4:d4
dnsmasq-dhcp: 2001112222 DHCPACK(bvp54e4a4d4) 192.0.2.59 52:54:00:e4:a4:d4
192.0.2.59 - - [10/Sep/2026 12:41:00] "GET /wimboot HTTP/1.1" 200 -
EOF
ONE_BOOT2_START=28; ONE_BOOT2_END=30

# fixture: python 404 for boot.wim
printf '192.0.2.59 - - [x] "GET /boot.wim HTTP/1.1" 404 -\n' >"$TMP/wim-404.log"
# fixture: a look-alike path — the '.' in the pattern must be literal
printf '192.0.2.59 - - [x] "GET /bootXsdi HTTP/1.1" 200 -\n' >"$TMP/lookalike.log"

echo "== http_200 / http_transfer_started =="
check "http_200 matches HTTP/1.0 200"          0 http_200 "$TMP/two-boots.log" /wimboot
check "http_200 matches HTTP/1.1 200 (BCD)"    0 http_200 "$TMP/two-boots.log" /BCD
check "http_200 escapes the dot in boot.sdi"   0 http_200 "$TMP/two-boots.log" /boot.sdi
check "http_200 dot is literal (not any-char)" 1 http_200 "$TMP/lookalike.log" /boot.sdi
check "http_200 is false on a 404"             1 http_200 "$TMP/wim-404.log" /boot.wim
check "http_transfer_started true on 200"      0 http_transfer_started "$TMP/two-boots.log" /boot.wim
check "http_transfer_started false on 404"     1 http_transfer_started "$TMP/wim-404.log" /boot.wim

echo "== winpe_dhcp_ack_in_range =="
check "boot #1 range proves a WinPE DHCPACK"   0 winpe_dhcp_ack_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "$MAC"
check "boot #2 range proves a WinPE DHCPACK"   0 winpe_dhcp_ack_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" "$MAC"
check "boot #2 range (only 1 real DORA) fails" 1 winpe_dhcp_ack_in_range "$TMP/one-winpe-dora.log" "$ONE_BOOT2_START" "$ONE_BOOT2_END" "$MAC"
check "empty range (end<start) fails"          1 winpe_dhcp_ack_in_range "$TMP/two-boots.log" 30 20 "$MAC"
check "wrong MAC in range fails"               1 winpe_dhcp_ack_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "52:54:00:de:ad:00"
# A PXE-only range (identity absent) must not pass just because a DHCPACK for
# the MAC is present.
check "PXE-only lines (no WinPE identity) fail" 1 winpe_dhcp_ack_in_range "$TMP/two-boots.log" 2 12 "$MAC"

echo "== 'BVE reboot' must not be a whole-log text count =="
# The OLD approach: one WinPE DORA yields >=2 matching lines
#   (vendor class: MSFT 5.0 / provides name: minint / DHCPACK ... minint-*)
# so a whole-log `grep -c ... >= 2` would WRONGLY report two boots.
old_reboot_would_pass() { (( "$(grep -cE 'MSFT 5\.0|minint' "$1")" >= 2 )); }
check "old whole-log count FALSE-passes 1 DORA" 0 old_reboot_would_pass "$TMP/one-winpe-dora.log"
# The NEW approach requires a WinPE DHCPACK in EACH boot's own range:
new_reboot_proven() {
    winpe_dhcp_ack_in_range "$1" "$2" "$3" "$MAC" && winpe_dhcp_ack_in_range "$1" "$4" "$5" "$MAC"
}
check "new per-range reboot: two-boots PASSES"  0 new_reboot_proven "$TMP/two-boots.log"  "$BOOT1_START" "$BOOT1_END" "$BOOT2_START" "$BOOT2_END"
check "new per-range reboot: one-DORA FAILS"    1 new_reboot_proven "$TMP/one-winpe-dora.log" "$BOOT1_START" 27 "$ONE_BOOT2_START" "$ONE_BOOT2_END"

echo
if (( FAIL == 0 )); then echo "parser tests: $PASS passed"; exit 0
else echo "parser tests: $PASS passed, $FAIL FAILED" >&2; exit 1; fi
