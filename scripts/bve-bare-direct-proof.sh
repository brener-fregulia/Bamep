#!/usr/bin/env bash
#
# Bamep Issue #72 - boot BARE (the Bamep Agent Runtime Environment) DIRECTLY in
# one BVE: QEMU/KVM loads the BARE kernel + initramfs (-kernel/-initrd), with no
# firmware boot device, no bootloader, no ISO, no PXE. Capture the serial
# console; require BARE_READY and BARE_NET_READY in each of two boots of the
# same BVE; dispose.
#
# Versioned DEVELOPMENT HARNESS - not product runtime. Normal user, no sudo, no
# TAP/bridge, no #70/#71 network. This proof NEVER builds BARE - run
# scripts/build-bare.sh first.
#
# Usage:
#   scripts/bve-bare-direct-proof.sh [--verbose] [--boot-hold <secs>] [bve-id]
#   BAMEP_BARE_OUTPUT=<dir>   or   --kernel <bzImage> --initrd <rootfs.cpio.gz>

set -euo pipefail

VERBOSE=0
ID="bve-bare-direct-proof"
BOOT_HOLD=25
KERNEL=""
INITRD=""
while (($#)); do
	case "$1" in
		-v | --verbose) VERBOSE=1 ;;
		--boot-hold)
			BOOT_HOLD="$2"
			shift
			;;
		--kernel)
			KERNEL="$2"
			shift
			;;
		--initrd)
			INITRD="$2"
			shift
			;;
		-h | --help)
			sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
			exit 0
			;;
		--*)
			echo "unknown flag: $1" >&2
			exit 2
			;;
		*) ID="$1" ;;
	esac
	shift
done

if [[ "$(id -u)" -eq 0 ]]; then
	echo "run this as your normal user" >&2
	exit 2
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
# shellcheck source=scripts/lib/bare-serial-evidence.sh
source "$HERE/lib/bare-serial-evidence.sh"
cd "$REPO"

# ---- resolve BARE artifacts -------------------------------------------
if [[ -z "$KERNEL" || -z "$INITRD" ]]; then
	CACHE_ROOT="${BAMEP_BARE_CACHE_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare}"
	OUT_DIR="${BAMEP_BARE_OUTPUT:-$CACHE_ROOT/output/bamep_bare_x86_64}"
	KERNEL="${KERNEL:-$OUT_DIR/images/bzImage}"
	INITRD="${INITRD:-$OUT_DIR/images/rootfs.cpio.gz}"
fi
for f in "$KERNEL" "$INITRD"; do
	[[ -s "$f" ]] || {
		echo "BARE artifact missing: $f" >&2
		echo "build it first:  scripts/build-bare.sh" >&2
		exit 3
	}
done

BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug/examples/bve_bare"
SERIAL_OUT="$(mktemp -t bamep-bare-serial.XXXXXX)"
RUN_LOG="$(mktemp -t bamep-bare-runbve.XXXXXX)"
trap 'rm -f "$SERIAL_OUT" "$RUN_LOG"' EXIT

line() {
	local label="$1" width=22 pad
	pad=$((width - ${#label} - 1))
	((pad < 1)) && pad=1
	printf '%s ' "$label"
	printf '%*s' "$pad" '' | tr ' ' '.'
	printf ' '
}
die() {
	echo "FAIL"
	[[ -n "${1:-}" ]] && echo "  $1" >&2
	if [[ -s "$SERIAL_OUT" ]]; then
		echo "  --- last serial lines ---" >&2
		tail -n 30 "$SERIAL_OUT" >&2 || true
	fi
	exit 1
}

# ---- build the example (NOT BARE) ------------------------------------
if ((VERBOSE)); then
	cargo build -p bamep-ve --example bve_bare
else
	cargo build -q -p bamep-ve --example bve_bare >/dev/null
fi

line "host + artifacts"
if "$BIN" check "$ID" --kernel "$KERNEL" --initrd "$INITRD" >/dev/null 2>&1; then
	echo "ok"
else
	echo "FAIL"
	"$BIN" check "$ID" --kernel "$KERNEL" --initrd "$INITRD" || true
	exit 1
fi

line "BVE x2 (direct boot)"
set +e
"$BIN" run-bve "$ID" \
	--kernel "$KERNEL" --initrd "$INITRD" \
	--serial-out "$SERIAL_OUT" --boot-hold "$BOOT_HOLD" >"$RUN_LOG" 2>&1
RC=$?
set -e
((VERBOSE)) && sed 's/^/    /' "$RUN_LOG"
((RC == 0)) && echo "ok" || {
	sed 's/^/    /' "$RUN_LOG" >&2
	die "run-bve rc=$RC"
}

range_of() { grep -oE "^BVE_BOOT${1}_LOG_RANGE=[0-9]+:[0-9]+\$" "$RUN_LOG" | tail -1 | cut -d= -f2 || true; }
B1="$(range_of 1)"
B2="$(range_of 2)"
[[ "$B1" =~ ^[0-9]+:[0-9]+$ && "$B2" =~ ^[0-9]+:[0-9]+$ ]] || die "run-bve did not emit BVE_BOOT{1,2}_LOG_RANGE"
B1S="${B1%%:*}" B1E="${B1##*:}" B2S="${B2%%:*}" B2E="${B2##*:}"
((VERBOSE)) && echo "  boot#1 serial lines $B1   boot#2 serial lines $B2"

echo
FAILED=0
stage() { # stage <label> <cmd...>
	line "$1"
	shift
	if "$@"; then echo "PASS"; else
		echo "NOT PROVEN"
		FAILED=1
	fi
}

stage "BARE_READY #1"     bare_ready_in_range     "$SERIAL_OUT" "$B1S" "$B1E"
stage "BARE_NET_READY #1" bare_net_ready_in_range "$SERIAL_OUT" "$B1S" "$B1E"
stage "BARE_READY #2"     bare_ready_in_range     "$SERIAL_OUT" "$B2S" "$B2E"
stage "BARE_NET_READY #2" bare_net_ready_in_range "$SERIAL_OUT" "$B2S" "$B2E"

line "BVE reboot"
if bare_ready_in_range "$SERIAL_OUT" "$B1S" "$B1E" && bare_ready_in_range "$SERIAL_OUT" "$B2S" "$B2E"; then
	echo "PASS"
else
	echo "NOT PROVEN"
	FAILED=1
fi

if ((FAILED)); then
	for r in "$B1S:$B1E" "$B2S:$B2E"; do
		bare_not_ready_in_range "$SERIAL_OUT" "${r%%:*}" "${r##*:}" &&
			echo "  BARE reported NOT_READY - a virtio device was not seen" >&2
	done
	echo
	echo "one or more boot stages were not proven (see above)"
	exit 1
fi
echo
echo "Issue #72 BARE direct-boot proof: BARE_READY + BARE_NET_READY in two boots, BVE disposed."
