#!/usr/bin/env bash
# Issue #63 Stage 2 — reproducible off-device validation. THROWAWAY Spike.
#
# Runs every Stage-2 host check in order. NO physical boot, NO physical source
# read, NO coordinator TCP listener, NO 36-case matrix. Nothing here arms
# anything.
#
#   1. stage2-engine   — unit tests + clippy + release build
#   2. coordinator     — Stage-1 + Stage-2 matrix tests, --matrix-selftest
#   3. stage2-probe     — host tests + --self-check (Accept/Reject, zero bulk read)
#   4. stage2-harness   — the 8/16/32/64 MiB host synthetic vertical (--smoke)
#   5. winpe-runner     — Stage-1 regression + --matrix NOT ARMED
#
# WinPE cross-build + PE imports are a separate manual step (needs cargo-xwin);
# see README.md.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)

step() { printf '\n=== %s ===\n' "$1"; }
fail() { printf '\nSTAGE2_CHECKS_FAIL: %s\n' "$1"; exit 1; }

step "1/5 stage2-engine"
( cd stage2-engine && cargo test --quiet && cargo clippy --quiet --all-targets -- -D warnings \
  && cargo build --release --quiet ) || fail "stage2-engine"

step "2/5 coordinator (Stage-1 + Stage-2 matrix)"
( cd coordinator && cargo test --quiet ) || fail "coordinator tests"
( cd coordinator && cargo run --quiet -- --matrix-selftest | tail -1 | grep -q MATRIX_SELFTEST_PASS ) \
  || fail "matrix-selftest"
( cd coordinator && cargo run --quiet -- --matrix | head -1 | grep -q 'PHYSICAL MATRIX NOT ARMED' ) \
  || fail "--matrix not-armed banner"

step "3/5 stage2-probe (host)"
( cd stage2-probe && cargo test --quiet ) || fail "probe tests"
( cd stage2-probe && cargo run --quiet -- --self-check | grep -q PROBE_SELF_CHECK_PASS ) \
  || fail "probe self-check"

step "4/5 stage2-harness host synthetic vertical (8/16/32/64 MiB @ 128 MiB)"
( cd stage2-harness && cargo run --quiet --release -- --smoke 2>&1 | tee /dev/stderr \
  | grep -q 'HOST_SMOKE_PASS' ) || fail "host smoke"

step "5/5 winpe-runner (Stage-1 regression + --matrix)"
( cd winpe-runner && cargo test --quiet ) || fail "winpe-runner tests"
( cd winpe-runner && cargo run --quiet -- --matrix | grep -q STAGE2_MATRIX_RUNNER_NOT_ARMED ) \
  || fail "runner --matrix"

printf '\nSTAGE2_CHECKS_PASS  (root=%s)\n' "$root"
printf 'PHYSICAL MATRIX NOT ARMED\n'
