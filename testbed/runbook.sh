#!/usr/bin/env bash
# Runbook executor for the OrbStack testbed.
#
# The Agents Platform renders one shell command per target and runs it on the controller
# (see src/platform.rs), substituting {target} with a topology resource ID. This script is
# the other half of that contract: it turns a resource ID into the machine and container
# that actually host it, so `config/agent.testbed.toml` can stay a plain list of command
# templates.
#
#   runbook.sh <verb> <resource-id> [args...]
#
# Exit code zero means the command ran, nothing more. The Scheduler still re-probes and
# requires the target to be Healthy in the after-Snapshot before an action counts as
# verified, so a successful restart of a service that stays broken is still a failure.

set -uo pipefail

TESTBED_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

VERB="${1:-}"
TARGET="${2:-}"
shift 2 2>/dev/null || true

[ -n "$VERB" ] && [ -n "$TARGET" ] || {
    echo "usage: runbook.sh <status|logs|restart|start|stop|reboot> <resource-id>" >&2
    exit 64
}

# Resource ID -> "<machine> <container> <compose-dir>".
#
# web-frontend resolves to the server container on purpose: this deployment serves the
# built frontend assets from the same process, so a frontend restart and a server restart
# are the same operation on the same host. The topology still models them separately
# because the authority matrix rates them differently (rows 4 and 5).
resolve() {
    case "$1" in
        postgres-main)      echo "infra-1 postgres-main /opt/broccoli/infra" ;;
        redis-mq)           echo "infra-1 redis-mq /opt/broccoli/infra" ;;
        seaweedfs-storage)  echo "infra-1 seaweedfs-storage /opt/broccoli/infra" ;;
        broccoli-server)    echo "app-1 broccoli-server /opt/broccoli/app" ;;
        web-frontend)       echo "app-1 broccoli-server /opt/broccoli/app" ;;
        worker-1)           echo "judge-1 worker-1 /opt/broccoli/judge" ;;
        *) return 1 ;;
    esac
}

mapping="$(resolve "$TARGET")" || {
    echo "runbook: unknown target '$TARGET'" >&2
    exit 65
}
set -- $mapping "$@"
VM="$1"; CONTAINER="$2"; COMPOSE_DIR="$3"; shift 3

# Every docker call must land on the machine own engine, never the shared host one.
in_vm() { orb -m "$VM" -u root bash -lc "export DOCKER_CONFIG=/etc/docker-cli DOCKER_HOST=unix:///var/run/docker.sock; $*"; }

case "$VERB" in
    status)
        echo "target=$TARGET machine=$VM container=$CONTAINER"
        in_vm "docker inspect -f 'state={{.State.Status}} health={{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}} started={{.State.StartedAt}} restarts={{.RestartCount}}' $CONTAINER"
        ;;
    logs)
        lines="${1:-50}"
        in_vm "docker logs --tail $lines $CONTAINER 2>&1"
        ;;
    restart)
        echo "restarting $CONTAINER on $VM"
        in_vm "cd $COMPOSE_DIR && docker compose restart $CONTAINER"
        ;;
    start)
        echo "starting $CONTAINER on $VM"
        in_vm "cd $COMPOSE_DIR && docker compose start $CONTAINER"
        ;;
    stop)
        echo "stopping $CONTAINER on $VM"
        in_vm "cd $COMPOSE_DIR && docker compose stop $CONTAINER"
        ;;
    reboot)
        # Machine-level, not container-level: the whole VM goes down and comes back.
        echo "rebooting machine $VM"
        orb restart "$VM"
        ;;
    *)
        echo "runbook: unknown verb '$VERB'" >&2
        exit 64
        ;;
esac
