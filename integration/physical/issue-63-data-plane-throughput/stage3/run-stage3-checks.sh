#!/usr/bin/env bash
# Issue #63 Stage 3 — reproducible off-device validation. THROWAWAY Spike.
#
# Runs every Stage-3 host check in order. NO physical boot, NO physical source
# read, NO 36-case transfer matrix, NO MiniPC power-on. Nothing here arms
# anything (the launcher's `--arm` is NOT passed).
#
#   1. stage2-engine   — unit tests + release build (Analysis is now Serialize)
#   2. coordinator     — Stage-1 + Stage-2 + Stage-3 matrix_net tests;
#                        --matrix-selftest; --matrix (no --arm) NOT ARMED banner
#   3. stage2-probe     — host tests + --self-check
#   4. winpe-runner     — Stage-1 regression + --matrix NOT ARMED + --matrix --arm
#                         fails closed without a valid --pin + matrix-loop unit tests
#   5. stage3-harness   — release build + `issue-credential` arg guard (NO DB touch)
#   6. bash -n on the Stage-3 shell scripts + launcher banner/preflight exits
#
# WinPE cross-build + PE-import inspection is a separate manual step (needs
# cargo-xwin); see README.md.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)

step() { printf '\n=== %s ===\n' "$1"; }
fail() { printf '\nSTAGE3_CHECKS_FAIL: %s\n' "$1"; exit 1; }

step "1/6 stage2-engine"
( cd stage2-engine && cargo test --quiet && cargo build --release --quiet ) || fail "stage2-engine"

step "2/6 coordinator (Stage-1 + Stage-2 + Stage-3 matrix_net)"
( cd coordinator && cargo test --quiet ) || fail "coordinator tests"
( cd coordinator && cargo run --quiet -- --matrix-selftest | tail -1 | grep -q MATRIX_SELFTEST_PASS ) || fail "matrix-selftest"
( cd coordinator && cargo run --quiet -- --matrix | grep -q 'PHYSICAL MATRIX NOT ARMED' ) || fail "--matrix not-armed banner"

step "3/6 stage2-probe (host)"
( cd stage2-probe && cargo test --quiet ) || fail "probe tests"
( cd stage2-probe && cargo run --quiet -- --self-check | grep -q PROBE_SELF_CHECK_PASS ) || fail "probe self-check"

step "4/6 winpe-runner (Stage-1 regression + --matrix modes)"
( cd winpe-runner && cargo test --quiet ) || fail "winpe-runner tests"
( cd winpe-runner && cargo run --quiet -- --matrix 2>&1 || true ) | grep -q STAGE2_MATRIX_RUNNER_NOT_ARMED || fail "runner --matrix not armed"
( cd winpe-runner && cargo run --quiet -- --matrix --arm 2>&1 || true ) | grep -q 'FATAL: --pin' || fail "runner --matrix --arm must fail closed without a valid --pin"

step "5/6 stage3-harness (release build + arg guard; NO database touched)"
( cd stage3-harness && cargo build --release --quiet ) || fail "stage3-harness build"
( cd stage3-harness && cargo run --quiet -- issue-credential 2>&1 || true ) | grep -q 'usage: stage3-harness issue-credential' || fail "issue-credential arg guard"

step "6/6 shell scripts"
for s in stage3/derive-stage3-runtime.sh stage3/run-stage3-lab.sh stage3/run-stage3-checks.sh; do
  bash -n "$s" || fail "bash -n $s"
done
( stage3/run-stage3-lab.sh | grep -q 'PHYSICAL MATRIX NOT ARMED' ) || fail "launcher (no args) must print PHYSICAL MATRIX NOT ARMED"
command -v shellcheck >/dev/null 2>&1 && { shellcheck -S warning stage3/*.sh || fail "shellcheck"; } || printf 'shellcheck not installed — skipped\n'

printf '\nSTAGE3_CHECKS_PASS  (root=%s)\n' "$root"
printf 'PHYSICAL MATRIX NOT ARMED — no boot, no transfer, nothing armed.\n'
