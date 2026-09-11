#!/usr/bin/env bash
# Runs on controller; all target mappings and verbs are explicit.
set -euo pipefail
[[ $# = 2 ]] || exit 64
verb=$1 target=$2
case "$verb" in status|logs|start|restart|stop|redis-ping|redis-info|postgres-check|postgres-locks|storage-check|resources|app-health) ;; *) exit 64 ;; esac
case "$target" in
  postgres-main|redis-mq|seaweedfs-storage) host=10.0.22.32 ;;
  broccoli-server|web-frontend) host=10.0.19.135 ;;
  worker-1) host=10.0.22.140 ;;
  worker-2) host=10.0.17.176 ;;
  *) echo 'Unknown target' >&2; exit 65 ;;
esac
if [[ "$verb" == app-health ]]; then
  exec python3 /opt/broccoli-agent/cloud/app-health.py "$target"
fi
ssh_args=(-F /dev/null -i /var/lib/broccoli-agent/.ssh/id_ed25519 \
  -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
  -o UserKnownHostsFile=/var/lib/broccoli-agent/.ssh/known_hosts \
  -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=2)
ssh "${ssh_args[@]}" "ubuntu@$host" "$verb $target"
if [[ "$target" == worker-* && ( "$verb" == start || "$verb" == restart ) ]]; then
  # The existing status command is sufficient; no new key or worker SSH privilege is needed.
  for attempt in $(seq 1 40); do
    state=$(ssh "${ssh_args[@]}" "ubuntu@$host" "status $target")
    if python3 -c 'import json,sys; s=sys.stdin.read().strip(); rows=json.loads(s) if s.startswith("[") else [json.loads(x) for x in s.splitlines()]; sys.exit(0 if len(rows)==1 and rows[0].get("State")=="running" and rows[0].get("Health")=="healthy" else 1)' <<< "$state"; then
      sleep 2
      echo "worker readiness confirmed"
      exit 0
    fi
    sleep 1
  done
  echo "worker readiness timeout" >&2
  exit 1
fi
