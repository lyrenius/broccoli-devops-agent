#!/usr/bin/env bash
# Create the three testbed machines. Idempotent: an existing machine is left alone.
#
# Sizing is written for macOS' bash 3.2 (no associative arrays), so the specs live in a
# case statement rather than a map.

source "$(dirname "$0")/lib.sh"

# CPU/memory caps keep a runaway judge worker from starving the Mac. app-1 carries the
# image builds, so it gets the largest share.
vm_spec() {
    case "$1" in
        "$INFRA_VM") echo "2 4G 32G" ;;
        "$APP_VM")   echo "8 12G 80G" ;;
        "$JUDGE_VM") echo "4 6G 32G" ;;
        *) echo "2 4G 32G" ;;
    esac
}

for vm in "${VMS[@]}"; do
    if orb list -q 2>/dev/null | grep -qx "$vm"; then
        log "$vm already exists; skipping create"
    else
        set -- $(vm_spec "$vm")
        log "creating $vm ($DISTRO, $1 cpus, $2 ram, $3 disk)"
        orb create --arch arm64 --cpus "$1" --memory "$2" --disk "$3" "$DISTRO" "$vm"
    fi
    orb start "$vm" >/dev/null 2>&1 || true
done

log "machines:"
for vm in "${VMS[@]}"; do
    printf '  %-10s %s\n' "$vm" "$(vm_ip "$vm")"
done
