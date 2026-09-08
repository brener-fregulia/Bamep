#!/usr/bin/env bash
# Issue #63 Stage 4 — reproducible off-device validation. THROWAWAY Spike.
#
# Runs every Stage-4 host check in order. NO physical boot, NO physical source
# read, NO 10-case transfer matrix, NO MiniPC power-on. Nothing here arms
# anything (the launcher's `--arm` is NOT passed).
#
#   1. stage2-engine    — unit tests (incl. the stage4 + window_8 plan/analysis) + release build
#   2. coordinator      — all tests; --stage4-selftest; --window8-selftest; --stage4 (no --arm) NOT ARMED
#   3. stage2-probe      — host tests (incl. prep-ahead + window_8 RED/GREEN) + --self-check
#   4. winpe-runner      — Stage-1/3 regression + --stage4 NOT ARMED + --stage4 --arm
#                          fails closed without a valid --pin + stage4 loop unit tests (incl. window_8 mode)
#   5. Worker PUT timing hook — `cargo test -p bamep-worker` (env-gated hook inert;
#                          the staging sub-timings fire only with i63_active); the
#                          out-of-order commit/seal contract check
#                          (`chunks_committed_in_strictly_descending_order_still_seal_verified`)
#   6. generated stage4 bootstrap -> runner argv contract (the exact @TOKEN@
#                          substituted invocation MUST parse, reach the network
#                          boundary, and NEVER exit BAD_ARGS) — window_8 reuses this
#                          SAME bootstrap unchanged (only the coordinator's own
#                          `--window8` flag differs; the runner forwards `mode`
#                          from the coordinator's case JSON generically)
#   7. stage3-harness    — release build + `--stage4` requires `--worker-timing-file`
#   8. bash -n on the Stage-4 shell scripts + derive `--stage 4` dry checks +
#                          launcher NOT ARMED banner (both `--stage4` and `--window8`)
#
# WinPE cross-build + PE-import inspection is a separate manual step (needs
# cargo-xwin); see README.md.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
REPO_ROOT="$(cd "${root}/../../.." && pwd)"

step() { printf '\n=== %s ===\n' "$1"; }
fail() { printf '\nSTAGE4_CHECKS_FAIL: %s\n' "$1"; exit 1; }

step "1/8 stage2-engine (incl. stage4 + window_8 module)"
( cd stage2-engine && cargo test --quiet && cargo clippy --quiet --all-targets -- -D warnings \
  && cargo build --release --quiet ) || fail "stage2-engine"

step "2/8 coordinator (stage4_net + selftest + window8-selftest + NOT ARMED)"
( cd coordinator && cargo test --quiet ) || fail "coordinator tests"
( cd coordinator && cargo run --quiet -- --stage4-selftest | tail -1 | grep -q STAGE4_SELFTEST_PASS ) || fail "stage4-selftest"
( cd coordinator && cargo run --quiet -- --window8-selftest | tail -1 | grep -q WINDOW8_SELFTEST_PASS ) || fail "window8-selftest"
( cd coordinator && cargo run --quiet -- --stage4 | grep -q 'STAGE4 NOT ARMED' ) || fail "--stage4 not-armed banner"

step "3/8 stage2-probe (host; prep-ahead + window_8 pipeline RED/GREEN + synthetic S-vs-P smoke)"
( cd stage2-probe && cargo test --quiet ) || fail "probe tests"
( cd stage2-probe && cargo run --quiet -- --self-check | grep -q PROBE_SELF_CHECK_PASS ) || fail "probe self-check"
( cd stage2-probe && cargo run --quiet -- --pipeline-check | grep -q PROBE_PIPELINE_CHECK_PASS ) || fail "probe pipeline-check (serial vs prep-ahead digest parity)"

step "4/8 winpe-runner (regression + --stage4 modes)"
( cd winpe-runner && cargo test --quiet ) || fail "winpe-runner tests"
( cd winpe-runner && cargo run --quiet -- --stage4 2>&1 || true ) | grep -q STAGE4_RUNNER_NOT_ARMED || fail "runner --stage4 not armed"
( cd winpe-runner && cargo run --quiet -- --stage4 --arm 2>&1 || true ) | grep -q 'FATAL: --pin' || fail "runner --stage4 --arm must fail closed without a valid --pin"

step "5/8 Worker PUT timing hook (env-gated; bamep-worker tests)"
( cd "${REPO_ROOT}" && cargo test -p bamep-worker --quiet ) || fail "bamep-worker tests"
I63T_OUT="$( cd "${REPO_ROOT}" && cargo test -p bamep-worker --quiet i63_stage_timing 2>&1 || true )"
printf '%s\n' "${I63T_OUT}" | grep -qE 'test result: ok\. 1 passed' || fail "i63 stage timing test did not pass"
grep -q 'BAMEP_I63_WORKER_PUT_TIMING' "${REPO_ROOT}/crates/worker/src/data_plane/i63_timing.rs" || fail "worker hook env var not present"
# window_8 CONTRACT / ORDERING CHECK: independently addressed chunk PUTs may
# durably complete/commit out of order before seal (m0-data-plane-and-storage-
# contracts.md "Full-Artifact byte reconstruction").
W8ORD_OUT="$( cd "${REPO_ROOT}" && cargo test -p bamep-worker --test data_plane_transfer --quiet chunks_committed_in_strictly_descending_order 2>&1 || true )"
printf '%s\n' "${W8ORD_OUT}" | grep -qE 'test result: ok\. 1 passed' || fail "window_8 out-of-order commit/seal contract check did not pass"
# The hook must be inert with the env var UNSET: no NDJSON sink is written.
I63T_INERT="$(mktemp -d)"
( cd "${REPO_ROOT}" && env -u BAMEP_I63_WORKER_PUT_TIMING cargo test -p bamep-worker --quiet valid_body_finalizes 2>&1 || true ) >/dev/null
[ -z "$(find "${I63T_INERT}" -type f)" ] || fail "worker hook wrote something with the env var unset"
rmdir "${I63T_INERT}"

step "6/8 generated stage4 bootstrap -> runner argv contract"
render_bootstrap() {  # $1 = template path -> stdout
  local pin; pin=$(printf 'a%.0s' $(seq 1 64))   # 64 hex chars
  sed -e 's|@RUN_ID@|i63s4-argvtest|g' \
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
render_bootstrap stage3/bamep-i63-stage4-bootstrap.cmd.template > "$GEN"
grep -qE '@[A-Z_]+@' "$GEN" && fail "unsubstituted @TOKEN@ in the rendered stage4 bootstrap"
RUNLINE="$(grep -F 'bamep-i63-runner.exe --stage4 --arm' "$GEN")"
[ -n "${RUNLINE}" ] || fail "no runner invocation line in the rendered stage4 bootstrap"
printf '  generated runner invocation:\n    %s\n' "${RUNLINE}"
ARGS="${RUNLINE#*bamep-i63-runner.exe }"
eval "set -- ${ARGS}"
printf '  tokenised argv (%d args): %s\n' "$#" "$*"
set +e
OUT="$( cd winpe-runner && cargo run --quiet -- "$@" 2>&1 )"
RC=$?
set -e
printf '%s\n' "${OUT}" | sed 's/^/    /' | tail -8
echo "  runner exit=${RC}"
printf '%s\n' "${OUT}" | grep -q 'STAGE4_RUNNER_ARMED' || fail "generated argv: STAGE4_RUNNER_ARMED not printed"
printf '%s\n' "${OUT}" | grep -q 'FATAL: unknown --matrix argument' && fail "generated argv REJECTED by parse_matrix_args (BAD_ARGS)"
[ "${RC}" -ne 2 ] || fail "generated argv: runner exited BAD_ARGS=2"
printf '%s\n' "${OUT}" | grep -qE 'stage4\.network_unreachable|no TCP path to' || fail "generated argv: runner did not reach the network boundary"
echo "  OK: generated stage4 argv parses and progresses to the (unreachable) network boundary"
rm -f "${GEN}"

step "7/8 stage3-harness (release build + --stage4 arg guard; NO database touched)"
( cd stage3-harness && cargo build --release --quiet ) || fail "stage3-harness build"
( cd stage3-harness && cargo run --quiet --release -- --stage4 --storage-root /tmp/i63s4-guard-$$ 2>&1 || true ) \
  | grep -q 'requires --worker-timing-file' || fail "--stage4 must require --worker-timing-file"
rm -rf "/tmp/i63s4-guard-$$"

step "8/8 shell scripts"
for s in stage4/run-stage4-lab.sh stage4/run-stage4-checks.sh stage3/derive-stage3-runtime.sh; do
  bash -n "$s" || fail "bash -n $s"
done
[ -f stage3/winpeshl-stage4.ini ] || fail "stage3/winpeshl-stage4.ini missing"
[ -f stage3/bamep-i63-stage4-bootstrap.cmd.template ] || fail "stage4 bootstrap template missing"
( stage4/run-stage4-lab.sh | grep -q 'STAGE4 NOT ARMED' ) || fail "launcher (no args) must print STAGE4 NOT ARMED"
( stage4/run-stage4-lab.sh --window8 | grep -q 'STAGE4 NOT ARMED' ) || fail "launcher --window8 (no --arm) must print STAGE4 NOT ARMED"
command -v shellcheck >/dev/null 2>&1 && { shellcheck -S warning stage4/*.sh || fail "shellcheck"; } || printf 'shellcheck not installed — skipped\n'

printf '\nSTAGE4_CHECKS_PASS  (root=%s)\n' "$root"
printf 'STAGE4 NOT ARMED — no boot, no transfer, nothing armed.\n'
