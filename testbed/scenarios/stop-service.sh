#!/usr/bin/env bash
# Stop one service, by topology resource ID.
#
#   scenarios/stop-service.sh redis-mq
#
# The container is stopped rather than killed so the failure looks like a service that
# went away cleanly, which is what most real outages look like from the outside.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

TARGET="${1:-}"
[ -n "$TARGET" ] || { echo "usage: stop-service.sh <resource-id>" >&2; exit 64; }

log "stopping $TARGET"
"$TESTBED_DIR/runbook.sh" stop "$TARGET"

if [ "$TARGET" = "worker-1" ]; then
    warn "worker-1 has no probe in the topology: this fault is invisible to a Snapshot"
    warn "expect the agent to report a coverage gap, not a Down resource"
fi
