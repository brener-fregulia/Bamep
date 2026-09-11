#!/usr/bin/env bash
#
# Pure unit tests for scripts/lib/bare-pxe-evidence.sh — the fixture-log
# (host-side provisioning) parsing helpers of the Issue #73 BARE UEFI-PXE
# host proof. Also exercises scripts/lib/bare-serial-evidence.sh (Issue #72,
# reused unchanged) for the guest-side readiness authority, and a small
# LOCAL composite ("full boot proven") that ANDs both authorities together —
# the composite lives only in this test file, never duplicated into either
# library. No sudo, no QEMU, no network.
#
#   ./scripts/bve-bare-pxe-proof-parser-test.sh

set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=scripts/lib/bare-pxe-evidence.sh
source lib/bare-pxe-evidence.sh
# shellcheck source=scripts/lib/bare-serial-evidence.sh
source lib/bare-serial-evidence.sh

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
MAC="52:54:00:c4:3c:74"

# --- fixture: a full, successful two-boot HOST-SIDE provisioning log -------
# Boot #1: PXE DORA (efi-x64) -> TFTP snponly.efi -> iPXE DORA -> GET
# boot.ipxe/bzImage/rootfs.cpio.gz, all 200. Boot #2: same again, new
# transaction ids.
cat >"$TMP/two-boots.log" <<'EOF'
dnsmasq: started, version 2.90 DNS disabled
dnsmasq-dhcp: 2895839652 available DHCP range: 192.0.2.50 -- 192.0.2.100
dnsmasq-dhcp: 2895839652 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 2895839652 DHCPDISCOVER(bvpc43c7454) 52:54:00:c4:3c:74
dnsmasq-dhcp: 2895839652 tags: efi-x64, known, bvpc43c7454
dnsmasq-dhcp: 2895839652 DHCPOFFER(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
dnsmasq-dhcp: 2895839652 next server: 192.0.2.1
dnsmasq-dhcp: 2895839652 sent size:  9 option: 67 bootfile-name  snponly.efi
dnsmasq-dhcp: 2895839652 DHCPREQUEST(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
dnsmasq-dhcp: 2895839652 DHCPACK(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
dnsmasq-tftp: sent /tmp/bamep-bve-fixture-c43c7454/tftp/snponly.efi to 192.0.2.59
dnsmasq-dhcp: 3902841012 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 3902841012 user class: iPXE
dnsmasq-dhcp: 3902841012 DHCPDISCOVER(bvpc43c7454) 52:54:00:c4:3c:74
dnsmasq-dhcp: 3902841012 tags: efi-x64, ipxe, known, bvpc43c7454
dnsmasq-dhcp: 3902841012 DHCPACK(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
192.0.2.59 - - [11/Sep/2026 10:00:01] "GET /boot.ipxe HTTP/1.1" 200 -
192.0.2.59 - - [11/Sep/2026 10:00:02] "GET /bzImage HTTP/1.1" 200 -
192.0.2.59 - - [11/Sep/2026 10:00:03] "GET /rootfs.cpio.gz HTTP/1.1" 200 -
dnsmasq-dhcp: 2001112222 available DHCP range: 192.0.2.50 -- 192.0.2.100
dnsmasq-dhcp: 2001112222 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 2001112222 DHCPDISCOVER(bvpc43c7454) 52:54:00:c4:3c:74
dnsmasq-dhcp: 2001112222 tags: efi-x64, known, bvpc43c7454
dnsmasq-dhcp: 2001112222 DHCPOFFER(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
dnsmasq-dhcp: 2001112222 sent size:  9 option: 67 bootfile-name  snponly.efi
dnsmasq-dhcp: 2001112222 DHCPREQUEST(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
dnsmasq-dhcp: 2001112222 DHCPACK(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
dnsmasq-tftp: sent /tmp/bamep-bve-fixture-c43c7454/tftp/snponly.efi to 192.0.2.59
dnsmasq-dhcp: 3339992222 vendor class: PXEClient:Arch:00007:UNDI:003016
dnsmasq-dhcp: 3339992222 user class: iPXE
dnsmasq-dhcp: 3339992222 DHCPDISCOVER(bvpc43c7454) 52:54:00:c4:3c:74
dnsmasq-dhcp: 3339992222 tags: efi-x64, ipxe, known, bvpc43c7454
dnsmasq-dhcp: 3339992222 DHCPACK(bvpc43c7454) 192.0.2.59 52:54:00:c4:3c:74
192.0.2.59 - - [11/Sep/2026 10:05:01] "GET /boot.ipxe HTTP/1.1" 200 -
192.0.2.59 - - [11/Sep/2026 10:05:02] "GET /bzImage HTTP/1.1" 200 -
192.0.2.59 - - [11/Sep/2026 10:05:03] "GET /rootfs.cpio.gz HTTP/1.1" 200 -
EOF
# Line ranges (1-indexed inclusive) as run-bve's BVE_BOOT{1,2}_FIXTURE_RANGE
# would report them:
BOOT1_START=2;  BOOT1_END=19
BOOT2_START=20; BOOT2_END=36

# --- fixture: boot #1 never requested rootfs.cpio.gz (truncated chain) -----
sed -n '1,18p' "$TMP/two-boots.log" >"$TMP/missing-rootfs.log"
MISS_ROOTFS_START=2; MISS_ROOTFS_END=18

# --- fixture: bzImage 404'd instead of 200 ----------------------------------
sed '18s#200 -#404 -#' "$TMP/two-boots.log" >"$TMP/bzimage-404.log"

# --- fixture: a look-alike path — '.' must be literal, and a longer/short
# path must not substring-match ------------------------------------------
cat >"$TMP/lookalikes.log" <<'EOF'
192.0.2.59 - - [x] "GET /bzImage2 HTTP/1.1" 200 -
192.0.2.59 - - [x] "GET /XbzImage HTTP/1.1" 200 -
192.0.2.59 - - [x] "GET /rootfsXcpioXgz HTTP/1.1" 200 -
192.0.2.59 - - [x] "GET /bootXipxe HTTP/1.1" 200 -
EOF

echo "== http_get_200_in_range =="
check "GET /boot.ipxe 200 in boot #1's range"   0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" /boot.ipxe
check "GET /bzImage 200 in boot #1's range"     0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" /bzImage
check "GET /rootfs.cpio.gz 200 in boot #1's range" 0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" /rootfs.cpio.gz
check "GET /boot.ipxe 200 in boot #2's range"   0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" /boot.ipxe
check "GET /bzImage 200 in boot #2's range"     0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" /bzImage
check "GET /rootfs.cpio.gz 200 in boot #2's range" 0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" /rootfs.cpio.gz

check "GET /rootfs.cpio.gz absent (whole-boot truncated log) fails" 1 http_get_200_in_range "$TMP/missing-rootfs.log" "$MISS_ROOTFS_START" "$MISS_ROOTFS_END" /rootfs.cpio.gz
check "absent GET (never requested) fails"      1 http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" 10 /bzImage

check "404 does not count as success"           1 http_get_200_in_range "$TMP/bzimage-404.log" "$BOOT1_START" "$BOOT1_END" /bzImage
# The same log's boot #2 range is untouched by the boot #1 404 substitution.
check "boot #2 GET /bzImage is unaffected by boot #1's 404" 0 http_get_200_in_range "$TMP/bzimage-404.log" "$BOOT2_START" "$BOOT2_END" /bzImage

echo "== a request logged for one boot never satisfies another boot's range =="
# Boot #1's three GETs are lines 17-19; boot #2's range is [20,36] and has its
# OWN (later, 10:05:0x-timestamped) GETs — so querying boot #2's range must
# still pass (its own requests are there)...
check "boot #2's range still proves its own GET /bzImage" 0 \
    http_get_200_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" /bzImage
# ...but a range that stops BEFORE boot #1's GETs ever happened must fail:
# a truncated slice containing only the PXE/TFTP/iPXE DORA lines, none of the
# three HTTP GET lines.
check "GET /boot.ipxe not yet requested (range ends before line 17) fails" 1 \
    http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" 16 /boot.ipxe
check "GET /bzImage not yet requested (range ends before line 18) fails"   1 \
    http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" 17 /bzImage
check "GET /rootfs.cpio.gz not yet requested (range ends before line 19) fails" 1 \
    http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" 18 /rootfs.cpio.gz

echo "== '.' in a path is matched literally, and paths do not substring-match =="
check "boot.ipxe literal dot"   0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" /boot.ipxe
check "rootfs.cpio.gz literal dots (real line)" 0 http_get_200_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" /rootfs.cpio.gz
check "/bzImage2 does not satisfy /bzImage"          1 http_get_200_in_range "$TMP/lookalikes.log" 1 4 /bzImage
check "/XbzImage does not satisfy /bzImage"          1 http_get_200_in_range "$TMP/lookalikes.log" 1 4 /bzImage
check "rootfsXcpioXgz does not satisfy rootfs.cpio.gz (dot is literal)" 1 \
    http_get_200_in_range "$TMP/lookalikes.log" 1 4 /rootfs.cpio.gz
check "bootXipxe does not satisfy /boot.ipxe (dot is literal)" 1 \
    http_get_200_in_range "$TMP/lookalikes.log" 1 4 /boot.ipxe

echo "== range validation fails closed =="
check "reversed/empty range fails"      1 http_get_200_in_range "$TMP/two-boots.log" 30 20 /bzImage
check "non-numeric range fails"         1 http_get_200_in_range "$TMP/two-boots.log" x y /bzImage
check "uefi_pxe_attempt: reversed range fails" 1 uefi_pxe_attempt_in_range "$TMP/two-boots.log" 30 20 "$MAC"
check "dhcp_ack: reversed range fails"         1 dhcp_ack_in_range "$TMP/two-boots.log" 30 20 "$MAC"

echo "== uefi_pxe_attempt_in_range / dhcp_ack_in_range / snponly_tftp_in_range / ipxe_boot_script_selected_in_range =="
check "UEFI PXE attempt (efi-x64 DHCPDISCOVER) in boot #1"  0 uefi_pxe_attempt_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "$MAC"
check "UEFI PXE attempt in boot #2"                          0 uefi_pxe_attempt_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" "$MAC"
check "UEFI PXE attempt for a different MAC fails"           1 uefi_pxe_attempt_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "52:54:00:de:ad:00"
check "DHCP bootstrap closes (DHCPACK) in boot #1"           0 dhcp_ack_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "$MAC"
check "DHCP bootstrap closes in boot #2"                     0 dhcp_ack_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" "$MAC"
check "snponly.efi TFTP transfer in boot #1"                 0 snponly_tftp_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END"
check "snponly.efi TFTP transfer in boot #2"                 0 snponly_tftp_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END"
check "no snponly.efi TFTP outside either boot's TFTP line"  1 snponly_tftp_in_range "$TMP/two-boots.log" 17 19
check "iPXE selected (user-class / boot.ipxe GET) in boot #1" 0 ipxe_boot_script_selected_in_range "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END"
check "iPXE selected in boot #2"                              0 ipxe_boot_script_selected_in_range "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END"
check "iPXE not selected before it appears (PXE-only slice)"  1 ipxe_boot_script_selected_in_range "$TMP/two-boots.log" 2 11

echo "== scripts/lib/bare-serial-evidence.sh reused unchanged for guest readiness =="
cat >"$TMP/serial-two-boots.log" <<'EOF'
Starting BARE startup hook
BARE READY nic=eth0 block=vda block_sectors=131072
udhcpc: lease of 10.0.2.15 obtained
BARE NET_READY nic=eth0 addr=10.0.2.15
Starting BARE startup hook
BARE READY nic=eth0 block=vda block_sectors=131072
udhcpc: lease of 10.0.2.15 obtained
BARE NET_READY nic=eth0 addr=10.0.2.15
EOF
SBOOT1_START=1; SBOOT1_END=4
SBOOT2_START=5; SBOOT2_END=8
check "BARE_READY in boot #1's serial range"     0 bare_ready_in_range "$TMP/serial-two-boots.log" "$SBOOT1_START" "$SBOOT1_END"
check "BARE_NET_READY in boot #1's serial range" 0 bare_net_ready_in_range "$TMP/serial-two-boots.log" "$SBOOT1_START" "$SBOOT1_END"
check "BARE_READY in boot #2's serial range"     0 bare_ready_in_range "$TMP/serial-two-boots.log" "$SBOOT2_START" "$SBOOT2_END"

# A serial log where boot #2 never actually reached readiness (it hung after
# init) — boot #1's real READY line must NOT be creditable to boot #2's range.
cat >"$TMP/serial-boot2-never-ready.log" <<'EOF'
Starting BARE startup hook
BARE READY nic=eth0 block=vda block_sectors=131072
udhcpc: lease of 10.0.2.15 obtained
BARE NET_READY nic=eth0 addr=10.0.2.15
Starting BARE startup hook
udhcpc: sending discover
EOF
check "boot #1's READY does not satisfy boot #2's own (empty) range" 1 \
    bare_ready_in_range "$TMP/serial-boot2-never-ready.log" "$SBOOT2_START" 6

cat >"$TMP/serial-net-before-ready.log" <<'EOF'
BARE NET_READY nic=eth0 addr=10.0.2.15
BARE READY nic=eth0 block=vda block_sectors=131072
EOF
check "NET_READY logged before READY does not pass" 1 bare_net_ready_in_range "$TMP/serial-net-before-ready.log" 1 2

# --- LOCAL composite: "full boot proven" ANDs both authorities together ----
# (this combinator is test-only scaffolding, never shipped into either lib —
# scripts/bve-bare-pxe-proof.sh prints each stage individually instead).
full_boot_proven() { # full_boot_proven <fixture-log> <fs> <fe> <serial-log> <ss> <se> <mac>
    local flog="$1" fs="$2" fe="$3" slog="$4" ss="$5" se="$6" mac="$7"
    uefi_pxe_attempt_in_range "$flog" "$fs" "$fe" "$mac" &&
        dhcp_ack_in_range "$flog" "$fs" "$fe" "$mac" &&
        snponly_tftp_in_range "$flog" "$fs" "$fe" &&
        ipxe_boot_script_selected_in_range "$flog" "$fs" "$fe" &&
        http_get_200_in_range "$flog" "$fs" "$fe" /boot.ipxe &&
        http_get_200_in_range "$flog" "$fs" "$fe" /bzImage &&
        http_get_200_in_range "$flog" "$fs" "$fe" /rootfs.cpio.gz &&
        bare_ready_in_range "$slog" "$ss" "$se" &&
        bare_net_ready_in_range "$slog" "$ss" "$se"
}

echo "== composite: a full boot is provable end to end, independently per boot =="
check "boot #1 complete (fixture + serial) passes" 0 \
    full_boot_proven "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "$TMP/serial-two-boots.log" "$SBOOT1_START" "$SBOOT1_END" "$MAC"
check "boot #2 complete (fixture + serial) passes" 0 \
    full_boot_proven "$TMP/two-boots.log" "$BOOT2_START" "$BOOT2_END" "$TMP/serial-two-boots.log" "$SBOOT2_START" "$SBOOT2_END" "$MAC"
check "a boot missing rootfs.cpio.gz fails the composite (fixture half)" 1 \
    full_boot_proven "$TMP/missing-rootfs.log" "$MISS_ROOTFS_START" "$MISS_ROOTFS_END" "$TMP/serial-two-boots.log" "$SBOOT1_START" "$SBOOT1_END" "$MAC"
check "a boot that never reached BARE_READY fails the composite (serial half), even with a complete fixture chain" 1 \
    full_boot_proven "$TMP/two-boots.log" "$BOOT1_START" "$BOOT1_END" "$TMP/serial-boot2-never-ready.log" "$SBOOT2_START" 6 "$MAC"

echo
if (( FAIL == 0 )); then echo "parser tests: $PASS passed"; exit 0
else echo "parser tests: $PASS passed, $FAIL FAILED" >&2; exit 1; fi
