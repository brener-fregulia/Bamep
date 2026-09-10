# shellcheck shell=bash
#
# Controlled build environment for BARE (Issue #72 / ADR-0026). Pure helpers -
# safe to source and unit test (scripts/build-bare-env-test.sh).
#
# Buildroot aborts before compiling if PATH contains a space, TAB, or newline.
# On WSL the inherited PATH carries Windows entries such as
# "/mnt/c/Program Files/...". BARE is built with an explicit, whitespace-free
# Linux PATH instead of touching the user's global environment - the fix
# belongs to the build script so the build is reproducible.

# The Linux directories BARE hands to Buildroot. Override with
# BAMEP_BARE_BUILD_PATH only if a mandatory tool genuinely lives elsewhere;
# the value is still validated (no whitespace) before use.
BARE_DEFAULT_BUILD_PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

# Buildroot mandatory host tools BARE checks for (Buildroot manual, "System
# requirements / Mandatory packages").
BARE_REQUIRED_TOOLS=(
	make gcc g++ ld perl python3 tar cpio unzip rsync file bc gzip bzip2 xz
	patch sed gawk wget find which
)

# bare_build_path -> the effective, whitespace-free PATH to hand to Buildroot
# (BAMEP_BARE_BUILD_PATH override, else BARE_DEFAULT_BUILD_PATH).
bare_build_path() {
	printf '%s' "${BAMEP_BARE_BUILD_PATH:-$BARE_DEFAULT_BUILD_PATH}"
}

# bare_path_is_clean <path>
#   0 iff <path> contains no whitespace at all (space, TAB, newline, CR, ...).
bare_path_is_clean() {
	[[ "$1" =~ [[:space:]] ]] && return 1
	return 0
}

# bare_missing_tools <path> <tool>...
#   prints, one per line, each <tool> that does NOT resolve under PATH=<path>.
#   No output (and rc 0) means every tool was found.
bare_missing_tools() {
	local path="$1" t
	shift
	for t in "$@"; do
		PATH="$path" command -v "$t" >/dev/null 2>&1 || printf '%s\n' "$t"
	done
}
