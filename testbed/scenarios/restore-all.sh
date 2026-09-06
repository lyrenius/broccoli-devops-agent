#!/usr/bin/env bash
# Undo every scenario: start all services and clear any injected network rules.
#
# Safe to run at any time, including when nothing is broken.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"

# partition.sh puts its DROP rules in a dedicated TESTBED chain reached from both
# DOCKER-USER and OUTPUT, so removal is exact: unhook the jumps, then drop the chain.
log "clearing injected network rules"
for vm in "${VMS[@]}"; do
    vm_root "$vm" "
        iptables -D DOCKER-USER -j TESTBED 2>/dev/null || true
        iptables -D OUTPUT -j TESTBED 2>/dev/null || true
        iptables -F TESTBED 2>/dev/null || true
        iptables -X TESTBED 2>/dev/null || true
        true
    " >/dev/null 2>&1 || true
done

log "restoring traffic control on infra-1"
vm_root "$INFRA_VM" "tc qdisc del dev eth0 root 2>/dev/null; true" >/dev/null 2>&1 || true

log "starting all services"
vm_root "$INFRA_VM" "cd /opt/broccoli/infra && docker compose up -d" >/dev/null
vm_root "$APP_VM" "cd /opt/broccoli/app && docker compose up -d" >/dev/null 2>&1 || warn "app tier not deployed yet"
vm_root "$JUDGE_VM" "cd /opt/broccoli/judge && docker compose up -d" >/dev/null 2>&1 || warn "judge tier not deployed yet"

log "waiting for the server to answer /healthz"
for _ in $(seq 1 40); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$APP_HOST:$SERVER_PORT/healthz" || true)" = "200" ] && break
    sleep 3
done

"$TESTBED_DIR/status.sh"
