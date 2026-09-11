#!/usr/bin/env bash
# Installed root-owned at /usr/local/sbin/broccoli-agent-dispatch.
# The dedicated controller key can only invoke this dispatcher.
set -euo pipefail
[[ $# = 1 ]] || exit 64
read -r verb target extra <<< "$1"
[[ -z "${extra:-}" ]] || exit 64
role=$(cat /etc/broccoli-node-role)
case "$role:$target" in
  infra:postgres-main) service=db ;;
  infra:redis-mq) service=redis ;;
  infra:seaweedfs-storage) service=seaweedfs ;;
  app:broccoli-server|app:web-frontend) service=server ;;
  worker-1:worker-1|worker-2:worker-2) service=worker ;;
  *) echo 'Target is not hosted on this node' >&2; exit 65 ;;
esac
cd /opt/broccoli
compose=(/usr/bin/docker compose --env-file .env -f compose.yaml)
case "$verb" in
  redis-ping)
    [[ "$role:$target" == "infra:redis-mq" ]] || exit 65
    exec /usr/bin/python3 /usr/local/lib/broccoli/redis-health.py
    ;;
  redis-info)
    [[ "$role:$target" == "infra:redis-mq" ]] || exit 65
    exec /usr/bin/python3 /usr/local/lib/broccoli/redis-health.py --info
    ;;
  postgres-check|postgres-locks|storage-check|resources)
    [[ "$role" == infra ]] || exit 65
    exec /usr/bin/python3 /usr/local/lib/broccoli/infra-ops.py "$verb" "$target"
    ;;
  status) "${compose[@]}" ps --all --format json "$service" 2>&1 | /usr/bin/python3 /usr/local/lib/broccoli/redact-output.py ;;
  logs) "${compose[@]}" logs --no-color --tail 100 "$service" 2>&1 | /usr/bin/python3 /usr/local/lib/broccoli/redact-output.py ;;
  start|restart|stop)
    "${compose[@]}" "$verb" "$service"
    if [[ "$role" == infra && "$verb" != stop ]]; then
      /usr/bin/python3 /usr/local/lib/broccoli/infra-ops.py wait "$target"
    fi
    ;;
  *) echo 'Unsupported operation' >&2; exit 64 ;;
esac
