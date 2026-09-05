#!/usr/bin/env bash
# Standalone demonstration of the corrected Gate B7 comparison logic.
# Not part of the harness scripts; used only to prove the fix's behavior
# under four scenarios, isolated from any real staging/network state.
set -uo pipefail

expected() {
    printf '%s\n' \
        "ipxeboot/x86_64-sb/autoexec.ipxe" \
        "ipxeboot/x86_64-sb/snponly-shim.efi" \
        "ipxeboot/x86_64-sb/snponly.efi" \
        | LC_ALL=C sort
}

check() {
    local label="$1"; local locale="$2"; shift 2
    local actual
    actual="$(printf '%s\n' "$@" | LC_ALL=C sort)"
    local exp
    exp="$(expected)"
    echo "== ${label} (actual list built, then generated under LC_COLLATE=${locale} for realism where relevant) =="
    if [ "${actual}" = "${exp}" ]; then
        echo "RESULT: PASS (gate would accept)"
    else
        echo "RESULT: FAIL (gate would abort) - diff:"
        diff <(printf '%s\n' "${exp}") <(printf '%s\n' "${actual}") || true
    fi
    echo
}

echo "############################################"
echo "1) OLD ORDERING CASE - the exact order that caused the original abort"
echo "   (as if produced by 'find | sort' under LC_COLLATE=pt_BR.UTF-8)"
echo "############################################"
check "old pt_BR ordering" "pt_BR.UTF-8" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly.efi" \
    "ipxeboot/x86_64-sb/snponly-shim.efi"

echo "############################################"
echo "2) EXTRA-FILE CASE - an unexpected ipxe.efi present"
echo "############################################"
check "extra file (ipxe.efi)" "C" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly-shim.efi" \
    "ipxeboot/x86_64-sb/snponly.efi" \
    "ipxeboot/x86_64-sb/ipxe.efi"

echo "############################################"
echo "3) MISSING-FILE CASE - snponly.efi absent"
echo "############################################"
check "missing file (snponly.efi)" "C" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly-shim.efi"

echo "############################################"
echo "4) DUPLICATE-ENTRY CASE - snponly.efi listed twice, set is otherwise correct"
echo "############################################"
check "duplicate entry (snponly.efi x2)" "C" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly-shim.efi" \
    "ipxeboot/x86_64-sb/snponly.efi" \
    "ipxeboot/x86_64-sb/snponly.efi"

echo "############################################"
echo "5) CONTROL - the correct, non-duplicated set, in yet another order"
echo "############################################"
check "correct set, arbitrary order" "C" \
    "ipxeboot/x86_64-sb/snponly.efi" \
    "ipxeboot/x86_64-sb/autoexec.ipxe" \
    "ipxeboot/x86_64-sb/snponly-shim.efi"
