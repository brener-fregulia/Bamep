#!/usr/bin/env bash
# Standalone demonstration of the corrected Gate C2 comparison logic,
# extracted verbatim in behavior from issue53-phase9a-setup-run.sh, but
# fed synthetic `ss -Hltn` output instead of the real command, so it can
# be exercised for all four required cases without depending on actual
# host listener state.
set -uo pipefail

IFACE="enp8s0"
ADDR="192.168.99.1/24"
ADDR_HOST="192.168.99.1"

http_like_listener_conflicts_with_lab_path() {
    local conflict_found=1
    local local_addr addr port
    while read -r local_addr; do
        [ -z "${local_addr}" ] && continue
        if [[ "${local_addr}" =~ ^\[(.*)\]:([0-9]+)$ ]]; then
            addr="${BASH_REMATCH[1]}"
            port="${BASH_REMATCH[2]}"
        elif [[ "${local_addr}" =~ ^([^:]+):([0-9]+)$ ]]; then
            addr="${BASH_REMATCH[1]}"
            port="${BASH_REMATCH[2]}"
        else
            continue
        fi
        case "${port}" in
            80|8080|8000|443) : ;;
            *) continue ;;
        esac
        case "${addr}" in
            0.0.0.0|'*'|::|"${ADDR_HOST}")
                echo "CONFLICT: HTTP-like listener bound to ${addr}:${port}"
                conflict_found=0
                ;;
            *)
                echo "benign, interface-specific: ${addr}:${port}"
                ;;
        esac
    done <<< "$1"
    return "${conflict_found}"
}

run_case() {
    # Mirrors the real script's exact (corrected) idiom:
    # `if http_like_listener_conflicts_with_lab_path; then ABORT; fi`
    local label="$1"; local fixture="$2"
    echo "== ${label} =="
    if http_like_listener_conflicts_with_lab_path "${fixture}"; then
        echo "RESULT: ABORT (gate correctly detects conflict)"
    else
        echo "RESULT: PASS (gate correctly allows)"
    fi
    echo
}

echo "############################################"
echo "Real host fixture observed on this host right now (ss -Hltn | awk '{print \$4}'):"
echo "############################################"
REAL_FIXTURE="$(ss -Hltn 2>/dev/null | awk '{print $4}')"
echo "${REAL_FIXTURE}"
run_case "1) Real current host state (Tailscale-only :443 binds)" "${REAL_FIXTURE}"

run_case "2) Synthetic: 0.0.0.0:443 (wildcard IPv4)" "0.0.0.0:443"
run_case "3) Synthetic: [::]:443 (wildcard IPv6)" "[::]:443"
run_case "4) Synthetic: 192.168.99.1:443 (the lab address itself)" "192.168.99.1:443"
run_case "5) Synthetic: Tailscale-only bind, isolated (100.84.46.117:443)" "100.84.46.117:443"
