#!/usr/bin/env bash
#
# Pure unit tests for scripts/lib/bare-serial-evidence.sh - the serial-log
# parsing helpers of the Issue #72 BARE direct-boot host proof. No QEMU, no
# build, no network. Fixtures are canonical BARE serial-console output.
#
#   ./scripts/bve-bare-direct-proof-parser-test.sh

set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=scripts/lib/bare-serial-evidence.sh
source lib/bare-serial-evidence.sh

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1" >&2; }
check() { # check <desc> <expected 0|1> <cmd...>
	local desc="$1" want="$2"
	shift 2
	if "$@"; then local got=0; else local got=1; fi
	if [[ "$got" == "$want" ]]; then ok "$desc"; else bad "$desc (want rc=$want, got rc=$got)"; fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- fixture: two clean boots, accumulated in one append-mode serial log ----
# Kernel banner noise is deliberately interleaved; the markers are the signal.
cat >"$TMP/two-boots.log" <<'EOF'
[    0.000000] Linux version 6.x.y (bare@buildroot)
[    1.234567] virtio_net virtio0 eth0: renamed from eth0
[    1.400000] virtio_blk virtio1: [vda] 131072 512-byte logical blocks
Starting BARE startup hook
BARE READY nic=eth0 block=vda block_sectors=131072
udhcpc: sending discover
udhcpc: lease of 10.0.2.15 obtained
BARE NET_READY nic=eth0 addr=10.0.2.15
[   25.0] reboot: Restarting system
[    0.000000] Linux version 6.x.y (bare@buildroot)
[    1.5] virtio_blk virtio1: [vda] 131072 512-byte logical blocks
BARE READY nic=eth0 block=vda block_sectors=131072
udhcpc: lease of 10.0.2.15 obtained
BARE NET_READY nic=eth0 addr=10.0.2.15
EOF

B1S=1
B1E=9
B2S=10
B2E=15

check "boot #1 BARE_READY in range"          0 bare_ready_in_range     "$TMP/two-boots.log" "$B1S" "$B1E"
check "boot #1 BARE_NET_READY in range"      0 bare_net_ready_in_range "$TMP/two-boots.log" "$B1S" "$B1E"
check "boot #2 BARE_READY in range"          0 bare_ready_in_range     "$TMP/two-boots.log" "$B2S" "$B2E"
check "boot #2 BARE_NET_READY in range"      0 bare_net_ready_in_range "$TMP/two-boots.log" "$B2S" "$B2E"

# --- a boot that reached READY but never got a DHCP lease -----------------
cat >"$TMP/no-net.log" <<'EOF'
BARE READY nic=eth0 block=vda block_sectors=131072
udhcpc: sending discover
udhcpc: sending discover
BARE NET_NOT_READY nic=eth0 reason=dhcp-failed
EOF
check "no-lease: BARE_READY holds"           0 bare_ready_in_range     "$TMP/no-net.log" 1 4
check "no-lease: BARE_NET_READY is NOT set"  1 bare_net_ready_in_range "$TMP/no-net.log" 1 4

# --- a boot where virtio never appeared ----------------------------------
cat >"$TMP/no-virtio.log" <<'EOF'
Starting BARE startup hook
BARE NOT_READY nic=none block=none
EOF
check "no-virtio: BARE_READY is NOT set"     1 bare_ready_in_range     "$TMP/no-virtio.log" 1 2
check "no-virtio: NOT_READY is detectable"   0 bare_not_ready_in_range "$TMP/no-virtio.log" 1 2

# --- NET_READY must not be credited from outside the boot's range --------
# lines 1..7 hold boot #1's READY (line 5) but its NET_READY is at line 8.
check "net marker beyond range not credited" 1 bare_net_ready_in_range "$TMP/two-boots.log" "$B1S" 7

# --- malformed markers are rejected -------------------------------------
cat >"$TMP/malformed.log" <<'EOF'
BARE READY nic= block=vda
BARE NET_READY nic=eth0 addr=unknown
BARE NET_READY nic=eth0 addr=999
EOF
check "empty nic= rejected"                  1 bare_ready_in_range     "$TMP/malformed.log" 1 1
check "addr=unknown / non-dotted rejected"   1 bare_net_ready_in_range "$TMP/malformed.log" 1 3

# --- bad ranges fail closed -------------------------------------------
check "reversed range fails closed"          1 bare_ready_in_range     "$TMP/two-boots.log" 9 1
check "non-numeric range fails closed"       1 bare_ready_in_range     "$TMP/two-boots.log" x y

echo
if ((FAIL == 0)); then
	echo "bare-serial-evidence: all $PASS checks passed."
else
	echo "bare-serial-evidence: $FAIL FAILED, $PASS passed." >&2
	exit 1
fi
