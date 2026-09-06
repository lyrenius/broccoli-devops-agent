#!/usr/bin/env bash
# Add latency to everything infra-1 sends, without breaking anything.
#
#   scenarios/slow-storage.sh 200ms
#
# At the default 200ms nothing goes Down: tcp.connect and http.status both still succeed,
# so the Snapshot reports every resource Healthy and only the recorded latency moves --
# measured at 210ms for PostgreSQL and 1412ms for the object store, against single-digit
# milliseconds when idle. It is the clearest demonstration that a reachability probe
# registry cannot express "slow", and that a report of "submissions are timing out" has to
# be diagnosed from evidence the probes do not carry.
#
# Past roughly 600ms the picture changes, and usefully so: `broccoli-server` flips to Down
# while every resource under it stays Healthy. Nothing has crashed -- /healthz pings the
# database and the queue with a 2s budget each, and the added round trips exhaust it. That
# is a second, sharper scenario: the only resource that looks broken is the one that is
# still working perfectly.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

DELAY="${1:-200ms}"

log "adding $DELAY of egress latency to $INFRA_VM"
vm_root "$INFRA_VM" "
    apt-get install -y -qq iproute2 >/dev/null 2>&1 || true
    tc qdisc del dev eth0 root 2>/dev/null || true
    tc qdisc add dev eth0 root netem delay $DELAY
    tc qdisc show dev eth0
"

warn "at 200ms expect Healthy everywhere, with latency in the hundreds of milliseconds"
warn "past ~600ms expect broccoli-server Down while its dependencies stay Healthy"
warn "undo with scenarios/restore-all.sh"
