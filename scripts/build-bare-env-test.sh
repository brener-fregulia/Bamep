#!/usr/bin/env bash
#
# Pure unit tests for scripts/lib/bare-build-env.sh + the source-provenance
# discipline of scripts/build-bare.sh - the controlled build environment of the
# Issue #72 BARE build. No Buildroot, no download, no build.
#
# Regressions guarded here:
#  1. `build-bare.sh --preflight` reported "ok" while the environment it would
#     hand to Buildroot was invalid (Windows PATH entries with spaces ->
#     Buildroot aborts: "Your PATH contains spaces, TABs, and/or newline").
#  2. A `BAMEP_BARE_BUILDROOT_SRC` override let the compiled Buildroot come from
#     an arbitrary pre-extracted tree while SHA-256 verification still ran
#     against the (possibly absent, possibly unrelated) cached archive - so the
#     verification did not establish the identity of the source actually built.
#
#   ./scripts/build-bare-env-test.sh

set -uo pipefail
cd "$(dirname "$0")"
# shellcheck source=scripts/lib/bare-build-env.sh
source lib/bare-build-env.sh

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

# --- bare_path_is_clean ------------------------------------------------
check "default build PATH is clean"          0 bare_path_is_clean "$BARE_DEFAULT_BUILD_PATH"
check "bare_build_path output is clean"      0 bare_path_is_clean "$(bare_build_path)"
check "PATH with a Windows 'Program Files' entry is rejected" 1 \
	bare_path_is_clean "/usr/bin:/mnt/c/Program Files/Git/bin:/bin"
check "PATH with a literal TAB is rejected"  1 bare_path_is_clean "/usr/bin:$(printf '\t'):/bin"
check "PATH with an embedded newline is rejected" 1 \
	bare_path_is_clean "$(printf '/usr/bin:\n/bin')"
check "a single plain dir is clean"          0 bare_path_is_clean "/usr/bin"

# --- BAMEP_BARE_BUILD_PATH override ----------------------------------
(
	export BAMEP_BARE_BUILD_PATH="/opt/tools:/usr/bin"
	[[ "$(bare_build_path)" == "/opt/tools:/usr/bin" ]]
) && ok "BAMEP_BARE_BUILD_PATH override is honoured" || bad "override not honoured"

# --- bare_missing_tools ---------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
: >"$TMP/make" && chmod +x "$TMP/make"
: >"$TMP/gcc" && chmod +x "$TMP/gcc"

got="$(bare_missing_tools "$TMP" make gcc definitely-not-a-real-tool)"
[[ "$got" == "definitely-not-a-real-tool" ]] &&
	ok "bare_missing_tools reports only the absent tool" ||
	bad "bare_missing_tools wrong output: [$got]"

got="$(bare_missing_tools "/nonexistent-bindir" make gcc)"
[[ "$(printf '%s' "$got" | grep -c .)" == 2 ]] &&
	ok "bare_missing_tools reports all tools missing on an empty PATH" ||
	bad "bare_missing_tools should have reported 2 missing, got: [$got]"

got="$(bare_missing_tools "$TMP" make gcc)"
[[ -z "$got" ]] &&
	ok "bare_missing_tools is silent when every tool resolves" ||
	bad "bare_missing_tools should be silent, got: [$got]"

# --- source provenance: the Buildroot BARE compiles always originates from the
#     archive identified and verified by bare/buildroot.lock ---------------
BUILD_BARE="./build-bare.sh"
[[ -f "$BUILD_BARE" ]] || bad "cannot find $BUILD_BARE"

check "build-bare.sh carries no BAMEP_BARE_BUILDROOT_SRC source-tree override" 1 \
	grep -q 'BAMEP_BARE_BUILDROOT_SRC' "$BUILD_BARE"
check "SRC_DIR is derived only from the pinned version in the controlled cache" 0 \
	grep -qF 'SRC_DIR="$CACHE_ROOT/buildroot-$BR_VERSION"' "$BUILD_BARE"
check "extract_source extracts from the verified archive (\$ARCHIVE_PATH)" 0 \
	grep -qF 'tar -xf "$ARCHIVE_PATH"' "$BUILD_BARE"

echo
if ((FAIL == 0)); then
	echo "bare-build-env: all $PASS checks passed."
else
	echo "bare-build-env: $FAIL FAILED, $PASS passed." >&2
	exit 1
fi
