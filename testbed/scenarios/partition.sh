#!/usr/bin/env bash
# Cut the network between two machines, leaving both of them running.
#
#   scenarios/partition.sh app-1 infra-1
#
# This is the scenario a per-resource probe cannot explain on its own. Every service is up
# and every service is healthy when probed from the controller, but app-1 can no longer
# reach PostgreSQL, Redis, or the blob store, so the server fails while its dependencies
# all report Healthy.
#
# The rules go in DOCKER-USER, not OUTPUT: container traffic is forwarded across the
# bridge rather than originating on the host, so an OUTPUT rule would miss it entirely.
# They live in a dedicated chain so restore-all.sh can remove exactly what was added.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

FROM_VM="${1:-$APP_VM}"
TO_VM="${2:-$INFRA_VM}"
TO_IP="$(vm_ip "$TO_VM")"
[ -n "$TO_IP" ] || { warn "cannot resolve $TO_VM"; exit 1; }

log "partitioning $FROM_VM from $TO_VM ($TO_IP)"
vm_root "$FROM_VM" "
    iptables -N TESTBED 2>/dev/null || true
    iptables -C DOCKER-USER -j TESTBED 2>/dev/null || iptables -I DOCKER-USER -j TESTBED
    iptables -C OUTPUT -j TESTBED 2>/dev/null || iptables -I OUTPUT -j TESTBED
    iptables -C TESTBED -d $TO_IP -j DROP 2>/dev/null || iptables -A TESTBED -d $TO_IP -j DROP
    iptables -S TESTBED
"

warn "from the controller every resource still probes fine except the ones on $FROM_VM"
warn "undo with scenarios/restore-all.sh"
