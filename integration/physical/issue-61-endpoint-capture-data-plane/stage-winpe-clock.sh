#!/usr/bin/env bash
#
# Bamep Issue #61 CP7A — (re)stage the WinPE clock-alignment command file.
#
# LAB-ONLY helper for run-cp7a-lab.sh. A baked clock value goes stale within
# minutes, and the CP7A probe runs an asymmetric clock pre-flight gate
# (agent_now - server_utc within [-60s, +10s]) BEFORE it touches any device.
# So this file must be regenerated on the Fedora side IMMEDIATELY before the
# operator boots WinPE, not once at launcher startup.
#
# It writes integration/physical/issue-60-winpe-agent-slice/smb-share/set-clock.cmd
# with the CURRENT Fedora local wall clock. Run it again as many times as you
# like; it just overwrites the file.
#
# Trustworthy time in the bare-metal maintenance environment is a known future
# Discovery / architecture concern (CP6 clock-skew finding) and is NOT solved
# here. If the probe still exits 69 (CP7A_CLOCK_PREFLIGHT) after running
# P:\set-clock.cmd, the WinPE time-zone bias is the likely cause — realign by
# hand and re-run.
#
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHARE="$(cd "${SCRIPT_DIR}/../issue-60-winpe-agent-slice/smb-share" 2>/dev/null && pwd || true)"
[ -n "${SHARE}" ] && [ -d "${SHARE}" ] || { echo "stage-winpe-clock: SMB share not found under ${SCRIPT_DIR}/../issue-60-winpe-agent-slice/smb-share" >&2; exit 1; }
OUT="${SHARE}/set-clock.cmd"

D="$(date '+%m-%d-%Y')"
T="$(date '+%H:%M:%S')"

cat > "${OUT}" <<EOF
@echo off
rem  Bamep Issue #61 CP7A — WinPE clock alignment (regenerated $(date -Is))
rem  LAB-ONLY. Matches Fedora local wall clock at generation time.
rem  Regenerate on the Fedora side (stage-winpe-clock.sh) right before booting.
date ${D}
time ${T}
echo CP7A_CLOCK_SET ${D} ${T}
EOF
chmod 600 "${OUT}"

printf '%s\n' "stage-winpe-clock: wrote ${OUT}"
printf '%s\n' "  date ${D}   time ${T}   (Fedora local)"
printf '%s\n' '  inside WinPE, after  net use P: \\192.168.99.1\PROBE  run:  P:\set-clock.cmd'
