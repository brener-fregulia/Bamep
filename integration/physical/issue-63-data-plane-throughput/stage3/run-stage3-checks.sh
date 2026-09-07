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
#   5. generated bootstrap -> runner argv contract (the exact @TOKEN@-substituted
#                         bootstrap runner invocation MUST parse; it must reach
#                         the network boundary, never BAD_ARGS)
#   6. stage3-harness   — release build + `issue-credential` arg guard (NO DB touch)
#   7. bash -n on the Stage-3 shell scripts + launcher banner/preflight exits
#
# WinPE cross-build + PE-import inspection is a separate manual step (needs
# cargo-xwin); see README.md.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)

step() { printf '\n=== %s ===\n' "$1"; }
fail() { printf '\nSTAGE3_CHECKS_FAIL: %s\n' "$1"; exit 1; }

step "1/7 stage2-engine"
( cd stage2-engine && cargo test --quiet && cargo build --release --quiet ) || fail "stage2-engine"

step "2/7 coordinator (Stage-1 + Stage-2 + Stage-3 matrix_net)"
( cd coordinator && cargo test --quiet ) || fail "coordinator tests"
( cd coordinator && cargo run --quiet -- --matrix-selftest | tail -1 | grep -q MATRIX_SELFTEST_PASS ) || fail "matrix-selftest"
( cd coordinator && cargo run --quiet -- --matrix | grep -q 'PHYSICAL MATRIX NOT ARMED' ) || fail "--matrix not-armed banner"

step "3/7 stage2-probe (host)"
( cd stage2-probe && cargo test --quiet ) || fail "probe tests"
( cd stage2-probe && cargo run --quiet -- --self-check | grep -q PROBE_SELF_CHECK_PASS ) || fail "probe self-check"

step "4/7 winpe-runner (Stage-1 regression + --matrix modes)"
( cd winpe-runner && cargo test --quiet ) || fail "winpe-runner tests"
( cd winpe-runner && cargo run --quiet -- --matrix 2>&1 || true ) | grep -q STAGE2_MATRIX_RUNNER_NOT_ARMED || fail "runner --matrix not armed"
( cd winpe-runner && cargo run --quiet -- --matrix --arm 2>&1 || true ) | grep -q 'FATAL: --pin' || fail "runner --matrix --arm must fail closed without a valid --pin"

step "5/7 generated bootstrap -> runner argv contract"
# Render the Stage-3 WinPE bootstrap EXACTLY as derive-stage3-runtime.sh does
# (same @TOKEN@ set), extract the `bamep-i63-runner.exe ...` invocation, and run
# that exact argv against the host runner. Deliberately unreachable network
# boundary (127.0.0.1:1, --net-wait-secs 1): the runner MUST get past argument
# parsing and fail at the network wait, NEVER exit BAD_ARGS.
render_bootstrap() {  # $1 = template path -> stdout
  local pin; pin=$(printf 'a%.0s' $(seq 1 64))   # 64 hex chars
  sed -e 's|@RUN_ID@|i63s3-argvtest|g' \
      -e 's|@LAB_IP@|127.0.0.1|g' \
      -e 's|@MATRIX_PORT@|1|g' \
      -e 's|@COORD_PORT@|19206|g' \
      -e 's|@WSS_PORT@|18443|g' \
      -e 's|@SINK_PORT@|19299|g' \
      -e "s|@PIN@|${pin}|g" \
      -e 's|@MODEL_SUBSTR@|256GB|g' \
      -e 's|@SKEW_FLOOR_MS@|-2000|g' \
      -e 's|@SKEW_CEIL_MS@|2000|g' \
      -e 's|@NET_WAIT_SECS@|1|g' \
      -e 's|@SEAL_TIMEOUT_SECS@|300|g' \
      "$1"
}
GEN="$(mktemp)"
render_bootstrap stage3/bamep-i63-stage3-bootstrap.cmd.template > "$GEN"
grep -qE '@[A-Z_]+@' "$GEN" && fail "unsubstituted @TOKEN@ in the rendered bootstrap"
RUNLINE="$(grep -F 'bamep-i63-runner.exe --matrix --arm' "$GEN")"
[ -n "${RUNLINE}" ] || fail "no runner invocation line in the rendered bootstrap"
printf '  generated runner invocation:\n    %s\n' "${RUNLINE}"
# strip the `X:\...\bamep-i63-runner.exe ` prefix, tokenise (respects the one quoted arg)
ARGS="${RUNLINE#*bamep-i63-runner.exe }"
eval "set -- ${ARGS}"
printf '  tokenised argv (%d args): %s\n' "$#" "$*"
set +e
OUT="$( cd winpe-runner && cargo run --quiet -- "$@" 2>&1 )"
RC=$?
set -e
printf '%s\n' "${OUT}" | sed 's/^/    /' | tail -8
echo "  runner exit=${RC}"
printf '%s\n' "${OUT}" | grep -q 'STAGE3_MATRIX_RUNNER_ARMED' || fail "generated argv: STAGE3_MATRIX_RUNNER_ARMED not printed"
printf '%s\n' "${OUT}" | grep -q 'FATAL: unknown --matrix argument' && fail "generated argv REJECTED by parse_matrix_args (BAD_ARGS)"
[ "${RC}" -ne 2 ] || fail "generated argv: runner exited BAD_ARGS=2"
printf '%s\n' "${OUT}" | grep -qE 'stage3\.network_unreachable|no TCP path to' || fail "generated argv: runner did not reach the network boundary"
echo "  OK: generated argv parses and progresses to the (unreachable) network boundary"
rm -f "${GEN}"

step "6/7 stage3-harness (release build + arg guard; NO database touched)"
( cd stage3-harness && cargo build --release --quiet ) || fail "stage3-harness build"
( cd stage3-harness && cargo run --quiet -- issue-credential 2>&1 || true ) | grep -q 'usage: stage3-harness issue-credential' || fail "issue-credential arg guard"

step "7/7 shell scripts"
for s in stage3/derive-stage3-runtime.sh stage3/run-stage3-lab.sh stage3/run-stage3-checks.sh; do
  bash -n "$s" || fail "bash -n $s"
done
( stage3/run-stage3-lab.sh | grep -q 'PHYSICAL MATRIX NOT ARMED' ) || fail "launcher (no args) must print PHYSICAL MATRIX NOT ARMED"
command -v shellcheck >/dev/null 2>&1 && { shellcheck -S warning stage3/*.sh || fail "shellcheck"; } || printf 'shellcheck not installed — skipped\n'

printf '\nSTAGE3_CHECKS_PASS  (root=%s)\n' "$root"
printf 'PHYSICAL MATRIX NOT ARMED — no boot, no transfer, nothing armed.\n'
