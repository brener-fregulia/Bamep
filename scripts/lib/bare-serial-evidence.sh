# shellcheck shell=bash
#
# Pure serial-log parsing helpers for the Issue #72 BARE direct-boot host proof
# (scripts/bve-bare-direct-proof.sh). No side effects - safe to source and unit
# test (scripts/bve-bare-direct-proof-parser-test.sh).
#
# Evidence is read only from the BVE-owned serial capture (QEMU file chardev,
# append mode). BARE emits, on the serial console:
#
#   BARE READY nic=<iface> block=<dev> block_sectors=<n>
#   BARE NET_READY nic=<iface> addr=<ipv4>
#
# (the leading token is "BARE"; the marker keyword follows). Absence of
# NET_READY never counts as networking ready.

# _in_range <serial-log> <start> <end>  -> the 1-indexed inclusive line slice
_in_range() {
	local log="$1" start="$2" end="$3"
	[[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ ]] || return 1
	((end >= start)) || return 1
	sed -n "${start},${end}p" -- "$log"
}

# bare_ready_in_range <serial-log> <start> <end>
#   0 iff a well-formed "BARE READY nic=<iface> block=<dev> ..." line appears in
#   the range. nic= and block= must both name a non-empty device.
bare_ready_in_range() {
	_in_range "$@" | grep -qE '(^|[[:space:]])BARE READY nic=[A-Za-z0-9._-]+ block=[A-Za-z0-9._-]+'
}

# bare_net_ready_in_range <serial-log> <start> <end>
#   0 iff a well-formed "BARE NET_READY nic=<iface> addr=<ipv4>" line appears in
#   the range with a dotted-quad address (not "unknown", not empty). The
#   NET_READY line must come at or after the READY line - networking readiness
#   never precedes base readiness.
bare_net_ready_in_range() {
	local log="$1" start="$2" end="$3"
	_in_range "$log" "$start" "$end" | awk '
		/(^|[[:space:]])BARE READY nic=/                                        { if (rl == 0) rl = NR }
		/(^|[[:space:]])BARE NET_READY nic=[A-Za-z0-9._-]+ addr=[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+/ { nl = NR }
		END { exit (rl > 0 && nl >= rl) ? 0 : 1 }
	'
}

# bare_not_ready_in_range <serial-log> <start> <end>
#   0 iff BARE explicitly reported it could not reach readiness (missing virtio
#   device) - used only to give an actionable failure message, never to pass.
bare_not_ready_in_range() {
	_in_range "$@" | grep -qE '(^|[[:space:]])BARE NOT_READY '
}
