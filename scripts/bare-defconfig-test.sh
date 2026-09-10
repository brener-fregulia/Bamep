#!/usr/bin/env bash
#
# Pure static validation of the BARE Buildroot defconfig + kernel fragment
# (Issue #72 / ADR-0026). No Buildroot, no download, no build - just asserts
# invariants a build must never silently lose.
#
# Regression anchor: the x86_64 arch default kernel config enables
# CONFIG_UNWINDER_ORC, which builds the `objtool` host tool, which needs
# <gelf.h> (libelf). Buildroot provides that via `host-elfutils` ONLY when
# BR2_LINUX_KERNEL_NEEDS_HOST_LIBELF=y. Without it the build fails at objtool
# ("fatal error: gelf.h: No such file or directory"). The fix is that Buildroot
# option, NOT a host libelf-dev package and NOT disabling ORC/objtool.
#
#   ./scripts/bare-defconfig-test.sh

set -uo pipefail
cd "$(dirname "$0")/.."

DEFCONFIG="bare/configs/bamep_bare_x86_64_defconfig"
FRAGMENT="bare/board/bamep/bare/linux.fragment"

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

[[ -f "$DEFCONFIG" ]] || { echo "missing $DEFCONFIG" >&2; exit 1; }
[[ -f "$FRAGMENT" ]] || { echo "missing $FRAGMENT" >&2; exit 1; }

# --- helpers (operate on the two files above) --------------------------
d_y()      { grep -qxE "$1=y" "$DEFCONFIG"; }                 # KEY=y present
d_unset()  { ! grep -qE "^$1=y\$" "$DEFCONFIG"; }             # KEY not =y (absent or "# ... is not set")
d_has()    { grep -qF -- "$1" "$DEFCONFIG"; }                 # substring present
f_y()      { grep -qxE "$1=y" "$FRAGMENT"; }                  # CONFIG=y in the fragment
fragment_no_orc_disable() {
	! grep -qxE 'CONFIG_UNWINDER_ORC=n|# CONFIG_UNWINDER_ORC is not set' "$FRAGMENT"
}
fragment_no_objtool_disable() {
	! grep -qxE 'CONFIG_STACK_VALIDATION=n|# CONFIG_STACK_VALIDATION is not set|CONFIG_OBJTOOL=n|# CONFIG_OBJTOOL is not set' "$FRAGMENT"
}

# --- target / kernel selection --------------------------------------
check "target arch is x86_64"                          0 d_y BR2_x86_64
check "Linux kernel is built"                          0 d_y BR2_LINUX_KERNEL
check "kernel uses the arch default config"            0 d_y BR2_LINUX_KERNEL_USE_ARCH_DEFAULT_CONFIG
check "kernel config fragment is referenced"           0 d_has 'BR2_LINUX_KERNEL_CONFIG_FRAGMENT_FILES='
check "fragment path points at board/bamep/bare/linux.fragment" 0 \
	d_has 'board/bamep/bare/linux.fragment'

# --- objtool / ORC needs host libelf via Buildroot -----------------
# This is the assertion that fails on the pre-fix defconfig.
check "x86 arch-default kernel declares host libelf (objtool/ORC needs gelf.h)" 0 \
	d_y BR2_LINUX_KERNEL_NEEDS_HOST_LIBELF
# ...and the fix must NOT be to defeat objtool instead:
check "ORC unwinder is not force-disabled in the fragment"  0 fragment_no_orc_disable
check "STACK_VALIDATION / objtool not force-disabled in the fragment" 0 fragment_no_objtool_disable

# --- rootfs shape: standalone gzip cpio, not embedded --------------
check "rootfs is a cpio image"                         0 d_y BR2_TARGET_ROOTFS_CPIO
check "rootfs cpio is gzip-compressed"                 0 d_y BR2_TARGET_ROOTFS_CPIO_GZIP
check "initramfs is NOT embedded in the kernel"        1 d_y BR2_TARGET_ROOTFS_INITRAMFS

# --- BARE is not a general-purpose OS -----------------------------
check "no getty (BARE is not interactive)"             1 d_y BR2_TARGET_GENERIC_GETTY
check "BusyBox init, not systemd"                      0 d_y BR2_INIT_BUSYBOX
check "systemd init is not selected"                   1 d_y BR2_INIT_SYSTEMD
check "rootfs overlay is the BARE overlay"             0 d_has 'board/bamep/bare/rootfs-overlay'
for pkg in BR2_PACKAGE_OPENSSH BR2_PACKAGE_DROPBEAR BR2_PACKAGE_PYTHON3 BR2_PACKAGE_SYSTEMD; do
	check "no package creep: $pkg"                     0 d_unset "$pkg"
done

# --- fragment forces the virtual devices built-in ----------------
for sym in CONFIG_VIRTIO_PCI CONFIG_VIRTIO_NET CONFIG_VIRTIO_BLK \
	CONFIG_SERIAL_8250_CONSOLE CONFIG_DEVTMPFS_MOUNT CONFIG_BLK_DEV_INITRD CONFIG_RD_GZIP; do
	check "fragment forces $sym=y"                     0 f_y "$sym"
done

echo
if ((FAIL == 0)); then
	echo "bare-defconfig: all $PASS checks passed."
else
	echo "bare-defconfig: $FAIL FAILED, $PASS passed." >&2
	exit 1
fi
