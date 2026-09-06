#!/usr/bin/env bash
#
# Bamep Issue #61 CP7A — (re)stage the WinPE clock-alignment command file.
#
# LAB-ONLY helper for run-cp7a-lab.sh.
#
# Run this ONLY AFTER the WinPE command prompt is actually reached, then run
#   P:\set-clock.cmd
# inside WinPE immediately afterwards. The CP7A probe's asymmetric clock
# pre-flight gate is
#   agent_now - server_utc  in  [-60s, +10s]
# (checked before any device access; the window is NOT widened here), so a clock
# file generated before a PXE/WinPE boot that can itself take more than 60 s
# would be needlessly fragile.
#
# The staged date/time are NOT Fedora local wall clock. They are the LAB-ONLY
# workaround for the currently proven stock-WinPE lineage: physical CP6/CP7
# evidence showed this WinPE's timezone Bias makes its interpreted UTC land
# roughly +5 h ahead when the Brazil local wall clock is typed in directly.
# Entering the wall clock of Etc/GMT+8 (UTC-8, matching that Bias) instead makes
# WinPE's Bias-adjusted SystemTime line up with the Server. Trustworthy time in
# the bare-metal maintenance environment remains a future Discovery /
# architecture concern; this helper is not a fix for it.
#
# It writes integration/physical/issue-60-winpe-agent-slice/smb-share/set-clock.cmd
# and just overwrites it on each run.
#
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHARE="$(cd "${SCRIPT_DIR}/../issue-60-winpe-agent-slice/smb-share" 2>/dev/null && pwd || true)"
[ -n "${SHARE}" ] && [ -d "${SHARE}" ] || { echo "stage-winpe-clock: SMB share not found under ${SCRIPT_DIR}/../issue-60-winpe-agent-slice/smb-share" >&2; exit 1; }
OUT="${SHARE}/set-clock.cmd"

# LAB-ONLY stock-WinPE Bias workaround: the operator types the Etc/GMT+8 (UTC-8)
# wall clock into WinPE, NOT Fedora local. See the header comment.
CLOCK_TZ="Etc/GMT+8"
D="$(TZ="${CLOCK_TZ}" date '+%m-%d-%Y')"
T="$(TZ="${CLOCK_TZ}" date '+%H:%M:%S')"

cat > "${OUT}" <<EOF
@echo off
rem  Bamep Issue #61 CP7A — WinPE clock alignment (generated $(date -Is))
rem  LAB-ONLY workaround for the currently proven stock-WinPE timezone Bias:
rem  the values below are the ${CLOCK_TZ} (UTC-8) wall clock, NOT Fedora local.
rem  Run this the moment the WinPE CMD prompt is reached; regenerate on the
rem  Fedora side (stage-winpe-clock.sh) and re-run if the probe exits 69.
date ${D}
time ${T}
echo CP7A_CLOCK_SET ${D} ${T} (${CLOCK_TZ} workaround)
EOF
chmod 600 "${OUT}"

printf '%s\n' "stage-winpe-clock: wrote ${OUT}"
printf '%s\n' "  date ${D}   time ${T}   (${CLOCK_TZ} / UTC-8 stock-WinPE Bias workaround — NOT Fedora local)"
printf '%s\n' '  now, inside the WinPE CMD prompt, run immediately:  P:\set-clock.cmd'
