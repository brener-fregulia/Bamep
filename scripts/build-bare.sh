#!/usr/bin/env bash
#
# Build BARE - the Bamep Agent Runtime Environment (Issue #72 / ADR-0026).
#
# BARE is the minimal bootable runtime environment that hosts the Bamep Agent.
# It is NOT a general-purpose OS. This script:
#   - runs Buildroot under a controlled, whitespace-free Linux PATH (Buildroot
#     aborts on a PATH with spaces/TABs/newlines; on WSL the inherited PATH
#     carries Windows entries such as "/mnt/c/Program Files/..."). The user's
#     global environment is never modified;
#   - pins Buildroot to bare/buildroot.lock, authenticated on the first pin by
#     the official Buildroot release PGP signature;
#   - keeps every heavy tree OFF the repo / off /mnt, and produces exactly:
#       <cache>/output/bamep_bare_x86_64/images/bzImage
#       <cache>/output/bamep_bare_x86_64/images/rootfs.cpio.gz
#
# Cache layout (BAMEP_BARE_CACHE_ROOT, default ${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare):
#   <cache>/buildroot-<version>/      Buildroot source, extracted once from the verified archive
#   <cache>/dl/                       BR2_DL_DIR - downloaded package sources (reusable offline)
#   <cache>/output/bamep_bare_x86_64/ Buildroot O= tree ($BAMEP_BARE_OUTPUT overrides)
#
# Usage:
#   scripts/build-bare.sh              build BARE
#   scripts/build-bare.sh --pin        download the archive + its .sign, verify
#                                      the PGP signature against the pinned
#                                      Buildroot signing key, then record the
#                                      signed SHA-256 into bare/buildroot.lock
#   scripts/build-bare.sh clean        remove ONLY the generated output tree
#   scripts/build-bare.sh --preflight  check the controlled build environment only
#
# Nothing is installed automatically. A missing prerequisite is reported with
# the exact command for the owner to run.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
LOCK="$REPO/bare/buildroot.lock"
DEFCONFIG="bamep_bare_x86_64_defconfig"
OUTPUT_NAME="bamep_bare_x86_64"
# shellcheck source=scripts/lib/bare-build-env.sh
source "$HERE/lib/bare-build-env.sh"

# The single controlled PATH handed to Buildroot AND to every tool this script
# runs on Buildroot's behalf (preflight probing, downloads, gpg, tar, make).
BUILD_PATH="$(bare_build_path)"

MODE="build"
for a in "$@"; do
	case "$a" in
		--pin) MODE="pin" ;;
		--preflight) MODE="preflight" ;;
		clean) MODE="clean" ;;
		-h | --help)
			sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
			exit 0
			;;
		*)
			echo "unknown argument: $a" >&2
			exit 2
			;;
	esac
done

if [[ "$(id -u)" -eq 0 ]]; then
	echo "run this as your normal user - Buildroot must not build as root" >&2
	exit 2
fi

# ---- lock file ------------------------------------------------------------
lock_get() { sed -n "s/^$1[[:space:]]*=[[:space:]]*//p" "$LOCK" | head -n1 | tr -d '[:space:]'; }

BR_VERSION="$(lock_get buildroot_version)"
BR_ARCHIVE="$(lock_get buildroot_archive)"
BR_URL="$(lock_get buildroot_url)"
BR_SHA256="$(lock_get buildroot_sha256)"
BR_SIGN_FPR="$(lock_get buildroot_signing_fpr)"
BR_KEY_URL="$(lock_get buildroot_signing_key_url)"
[[ -n "$BR_VERSION" && -n "$BR_ARCHIVE" && -n "$BR_URL" ]] || {
	echo "bare/buildroot.lock is incomplete" >&2
	exit 1
}

CACHE_ROOT="${BAMEP_BARE_CACHE_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare}"
DL_DIR="$CACHE_ROOT/dl"
# Buildroot source is ALWAYS the pinned version extracted from the verified
# archive into the controlled cache - there is no source-tree override, so the
# compiled Buildroot's identity is exactly what bare/buildroot.lock verifies.
SRC_DIR="$CACHE_ROOT/buildroot-$BR_VERSION"
OUT_DIR="${BAMEP_BARE_OUTPUT:-$CACHE_ROOT/output/$OUTPUT_NAME}"
ARCHIVE_PATH="$CACHE_ROOT/$BR_ARCHIVE"
SIGN_PATH="$CACHE_ROOT/$BR_ARCHIVE.sign"

# run_build <cmd...>  - run a command under the controlled PATH only.
run_build() { PATH="$BUILD_PATH" "$@"; }

PIN_TMP=""
trap '[[ -n "$PIN_TMP" ]] && rm -rf "$PIN_TMP"' EXIT

# ---- preflight: validate the environment handed to Buildroot ------------
preflight() {
	echo "effective build PATH: $BUILD_PATH"
	if ! bare_path_is_clean "$BUILD_PATH"; then
		echo >&2
		echo "the build PATH contains whitespace - Buildroot aborts on this." >&2
		echo "it is derived from BARE_DEFAULT_BUILD_PATH (or BAMEP_BARE_BUILD_PATH);" >&2
		echo "the user's global PATH is not used. Fix the override, do not touch ~/.bashrc." >&2
		return 1
	fi
	local missing
	missing="$(bare_missing_tools "$BUILD_PATH" "${BARE_REQUIRED_TOOLS[@]}")"
	if [[ -n "$missing" ]]; then
		echo >&2
		echo "missing host prerequisites under the build PATH:" >&2
		printf '  %s\n' $missing >&2
		# shellcheck disable=SC2086
		echo "install them, for example:  sudo apt-get install -y build-essential $missing" >&2
		return 1
	fi
	echo "tools found under the build PATH: ${BARE_REQUIRED_TOOLS[*]}"
	echo "host prerequisites: ok"
}

# ---- archive acquisition ----------------------------------------------
sha256_of() { run_build sha256sum -- "$1" | awk '{print $1}'; }

acquire_archive() {
	mkdir -p "$CACHE_ROOT"
	if [[ ! -s "$ARCHIVE_PATH" ]]; then
		echo "downloading $BR_URL"
		run_build wget -q -O "$ARCHIVE_PATH.part" "$BR_URL"
		mv "$ARCHIVE_PATH.part" "$ARCHIVE_PATH"
	fi
}

# ---- rebuild-time check: the archive must match the pinned SHA-256 ------
verify_archive() {
	local actual
	actual="$(sha256_of "$ARCHIVE_PATH")"
	if [[ "$BR_SHA256" == "PENDING" || -z "$BR_SHA256" ]]; then
		echo >&2
		echo "bare/buildroot.lock has no pinned SHA-256 for $BR_ARCHIVE." >&2
		echo "establish the first authenticated pin:  scripts/build-bare.sh --pin" >&2
		return 1
	fi
	if [[ "$actual" != "$BR_SHA256" ]]; then
		echo "SHA-256 mismatch for $BR_ARCHIVE" >&2
		echo "  expected (lock): $BR_SHA256" >&2
		echo "  actual         : $actual" >&2
		echo "refusing to build against a non-pinned Buildroot (ADR-0026)" >&2
		return 1
	fi
	echo "buildroot archive matches the pinned SHA-256: $actual"
}

# ---- first pin: authenticate via the official Buildroot PGP signature ---
pin() {
	preflight || exit 3
	if ! run_build command -v gpg >/dev/null 2>&1; then
		echo "gpg is required to authenticate the first Buildroot pin." >&2
		echo "install it, then re-run:  sudo apt-get install -y gnupg" >&2
		exit 3
	fi
	[[ -n "$BR_SIGN_FPR" && -n "$BR_KEY_URL" ]] || {
		echo "bare/buildroot.lock is missing buildroot_signing_fpr / buildroot_signing_key_url" >&2
		exit 1
	}

	acquire_archive
	echo "downloading signature $BR_URL.sign"
	run_build wget -q -O "$SIGN_PATH" "$BR_URL.sign"

	local gpghome
	gpghome="$(mktemp -d)"
	PIN_TMP="$gpghome"
	chmod 700 "$gpghome"

	# Obtain the Buildroot signing key and pin its fingerprint. A key served
	# under a different fingerprint than bare/buildroot.lock records is refused
	# - "valid signature by some key" is not acceptance.
	local keyfile="$gpghome/buildroot-signing-key.gpg"
	if [[ -n "${BAMEP_BARE_BUILDROOT_KEY:-}" && -s "$BAMEP_BARE_BUILDROOT_KEY" ]]; then
		cp "$BAMEP_BARE_BUILDROOT_KEY" "$keyfile"
		echo "using local signing key: $BAMEP_BARE_BUILDROOT_KEY"
	else
		echo "fetching signing key $BR_KEY_URL"
		run_build wget -q -O "$keyfile" "$BR_KEY_URL"
	fi

	local got_fpr expect_fpr
	expect_fpr="$(printf '%s' "$BR_SIGN_FPR" | tr -d ' ' | tr 'a-f' 'A-F')"
	got_fpr="$(GNUPGHOME="$gpghome" run_build gpg --no-default-keyring --with-colons \
		--show-keys "$keyfile" 2>/dev/null | awk -F: '/^fpr:/ {print $10; exit}')"
	if [[ "$got_fpr" != "$expect_fpr" ]]; then
		echo "signing key fingerprint mismatch" >&2
		echo "  expected (lock): $expect_fpr" >&2
		echo "  key served     : ${got_fpr:-<none>}" >&2
		exit 1
	fi
	echo "signing key fingerprint matches the pin: $expect_fpr"

	GNUPGHOME="$gpghome" run_build gpg --quiet --import "$keyfile"

	# Verify the clearsigned .sign and take the SHA-256 ONLY from gpg's verified
	# output (--decrypt emits just the signed payload, non-zero on a bad sig).
	local verified signed_sha
	if ! verified="$(GNUPGHOME="$gpghome" run_build gpg --status-fd 3 --decrypt "$SIGN_PATH" 3>"$gpghome/status" 2>/dev/null)"; then
		echo "PGP verification of $BR_ARCHIVE.sign FAILED" >&2
		exit 1
	fi
	if ! grep -qE "^\[GNUPG:\] GOODSIG " "$gpghome/status"; then
		echo "no GOODSIG in gpg status - not accepting the signature" >&2
		exit 1
	fi
	# The VALIDSIG status line carries the signing key's fingerprint (first
	# token) and the primary-key fingerprint (last token). Require the pinned
	# fingerprint to be present - not merely "some good signature".
	if ! grep -qE "^\[GNUPG:\] VALIDSIG .*${expect_fpr}" "$gpghome/status"; then
		echo "the good signature is NOT from the pinned key $expect_fpr" >&2
		grep -E "^\[GNUPG:\] VALIDSIG " "$gpghome/status" >&2 || true
		exit 1
	fi
	echo "PGP signature: good, from the pinned Buildroot signing key"

	signed_sha="$(printf '%s\n' "$verified" |
		sed -n "s/^SHA256:[[:space:]]*\([0-9a-f]\{64\}\)[[:space:]].*$BR_ARCHIVE.*/\1/p" | head -n1)"
	[[ -n "$signed_sha" ]] || {
		echo "could not find a signed 'SHA256: <hex>  $BR_ARCHIVE' line in the verified signature" >&2
		exit 1
	}

	local actual
	actual="$(sha256_of "$ARCHIVE_PATH")"
	if [[ "$actual" != "$signed_sha" ]]; then
		echo "downloaded archive does NOT match the PGP-signed SHA-256" >&2
		echo "  signed : $signed_sha" >&2
		echo "  archive: $actual" >&2
		exit 1
	fi
	echo "archive SHA-256 matches the signed value: $actual"

	if [[ -n "$BR_SHA256" && "$BR_SHA256" != "PENDING" && "$BR_SHA256" != "$actual" ]]; then
		echo "bare/buildroot.lock already pins a DIFFERENT SHA-256 ($BR_SHA256)" >&2
		echo "refusing to overwrite it silently - resolve deliberately" >&2
		exit 1
	fi
	sed -i "s|^buildroot_sha256[[:space:]]*=.*|buildroot_sha256 = $actual|" "$LOCK"
	echo "pinned bare/buildroot.lock: buildroot_sha256 = $actual"
}

extract_source() {
	# Extract once from the verified archive into the persistent cache tree;
	# later builds reuse it (so incremental builds stay fast). A version bump in
	# bare/buildroot.lock changes SRC_DIR, forcing a fresh extraction.
	if [[ ! -f "$SRC_DIR/Makefile" ]]; then
		echo "extracting Buildroot $BR_VERSION -> $SRC_DIR"
		mkdir -p "$SRC_DIR"
		run_build tar -xf "$ARCHIVE_PATH" -C "$SRC_DIR" --strip-components=1
	fi
}

# ---- clean (scoped, fail-closed) --------------------------------------
clean() {
	# Only ever remove a directory we can positively identify as the BARE
	# Buildroot output tree: it must be absolute, must contain a Buildroot
	# .config referencing our external, or be empty/absent. Never a bare rm -rf
	# on an arbitrary override path (Bamep fail-closed deletion philosophy).
	local target="$OUT_DIR"
	case "$target" in
		/*) ;;
		*)
			echo "refusing to clean a non-absolute output path: $target" >&2
			exit 1
			;;
	esac
	if [[ ! -e "$target" ]]; then
		echo "nothing to clean ($target does not exist)"
		return 0
	fi
	if [[ ! -d "$target" ]]; then
		echo "refusing to clean $target: not a directory" >&2
		exit 1
	fi
	if [[ -e "$target/.config" ]] && ! grep -q "BR2_EXTERNAL" "$target/.config" 2>/dev/null; then
		echo "refusing to clean $target: .config is not a BR2_EXTERNAL build" >&2
		exit 1
	fi
	local entry ok=1
	for entry in "$target"/*; do
		[[ -e "$entry" ]] || continue
		case "${entry##*/}" in
			.config | build | host | images | staging | target | Makefile | .br2-external.mk | \
				.gitignore | graphs | legal-info | licenses | per-package | .config.old | .stamp_* | \
				.br-external.mk | .cargo | .rustup) ;;
			*)
				echo "unexpected entry in output tree: $entry" >&2
				ok=0
				;;
		esac
	done
	((ok)) || {
		echo "refusing to clean $target: unexpected contents" >&2
		exit 1
	}
	echo "removing generated output tree: $target"
	rm -rf -- "$target"
}

# ---- build -----------------------------------------------------------
build() {
	preflight || exit 3
	acquire_archive
	verify_archive || exit 3
	extract_source
	mkdir -p "$DL_DIR" "$(dirname "$OUT_DIR")"

	local mk=(make -C "$SRC_DIR" BR2_EXTERNAL="$REPO/bare" O="$OUT_DIR" BR2_DL_DIR="$DL_DIR")

	echo "==> configuring ($DEFCONFIG) under a controlled PATH"
	run_build "${mk[@]}" "$DEFCONFIG"

	echo "==> building BARE (Buildroot will fetch package sources into $DL_DIR)"
	run_build "${mk[@]}"

	local bz="$OUT_DIR/images/bzImage" rf="$OUT_DIR/images/rootfs.cpio.gz"
	[[ -s "$bz" && -s "$rf" ]] || {
		echo "expected artifacts missing after build" >&2
		exit 1
	}
	echo
	echo "BARE artifacts:"
	printf '  %-14s %10d bytes  sha256 %s\n' bzImage "$(stat -c%s "$bz")" "$(sha256_of "$bz")"
	printf '  %-14s %10d bytes  sha256 %s\n' rootfs.cpio.gz "$(stat -c%s "$rf")" "$(sha256_of "$rf")"
	echo
	local krel
	krel="$(cat "$OUT_DIR"/build/linux-*/include/config/kernel.release 2>/dev/null | head -n1 || true)"
	[[ -n "$krel" ]] && echo "kernel: $krel"
	echo "prove:  scripts/bve-bare-direct-proof.sh"
	echo "legal:  (cd $SRC_DIR && PATH='$BUILD_PATH' ${mk[*]} legal-info)   # auxiliary; summarise in docs/reference/"
}

case "$MODE" in
	preflight) preflight ;;
	pin) pin ;;
	clean) clean ;;
	build) build ;;
esac
