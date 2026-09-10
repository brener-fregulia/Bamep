# BARE — Bamep Agent Runtime Environment
#
# No BARE-owned Buildroot packages yet (Issue #72). Intentionally minimal:
# the runtime substrate is BusyBox plus the rootfs overlay under
# board/bamep/bare/rootfs-overlay/. A future Bamep Agent package would be
# included from here.

include $(sort $(wildcard $(BR2_EXTERNAL_BAMEP_BARE_PATH)/package/*/*.mk))
