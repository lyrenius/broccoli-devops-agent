#!/usr/bin/env bash
# One-screen view of the testbed: machines, containers, and what the controller can reach.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

printf '\n%-10s %-16s %s\n' "MACHINE" "ADDRESS" "CONTAINERS"
for vm in "${VMS[@]}"; do
    containers="$(vm_root "$vm" 'docker ps -a --format "{{.Names}}:{{.State}}" | tr "\n" " "' 2>/dev/null || echo "unreachable")"
    printf '%-10s %-16s %s\n' "$vm" "$(vm_ip "$vm")" "$containers"
done

printf '\n%-34s %s\n' "ENDPOINT" "REACHABLE FROM THIS MAC"
check() {
    if nc -z -G 3 "$1" "$2" 2>/dev/null; then printf '%-34s yes\n' "$1:$2"; else printf '%-34s NO\n' "$1:$2"; fi
}
check "$INFRA_HOST" "$PG_PORT"
check "$INFRA_HOST" "$REDIS_PORT"
check "$INFRA_HOST" "$SEAWEED_S3_PORT"
check "$INFRA_HOST" "$SEAWEED_MASTER_PORT"
check "$APP_HOST" "$SERVER_PORT"

code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 4 "http://$APP_HOST:$SERVER_PORT/healthz" || true)"
printf '\n%-34s %s\n' "http://$APP_HOST:$SERVER_PORT/healthz" "${code:-no response}"
echo
