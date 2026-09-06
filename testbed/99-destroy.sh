#!/usr/bin/env bash
# Delete the testbed machines and everything on them.
#
# Only touches the three machines this testbed created; any other OrbStack machine is left
# alone. Requires --yes, because the machines carry the deployed Broccoli state.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

[ "${1:-}" = "--yes" ] || {
    echo "This deletes the machines: ${VMS[*]}"
    echo "Re-run with --yes to confirm."
    exit 1
}

for vm in "${VMS[@]}"; do
    if orb list -q 2>/dev/null | grep -qx "$vm"; then
        log "deleting $vm"
        orb delete -f "$vm"
    else
        log "$vm does not exist"
    fi
done
