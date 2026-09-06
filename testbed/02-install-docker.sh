#!/usr/bin/env bash
# Install and configure a Docker engine inside each machine.
#
# Each machine runs its own dockerd, so a container stopped on judge-1 is genuinely
# stopped on a different host than app-1 -- which is the whole point of the testbed.
# Installs run in parallel; per-machine logs land in testbed/.log/.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
mkdir -p "$LOGDIR"

PROXY="$(proxy_url || true)"

install_one() {
    local vm="$1"

    orb -m "$vm" -u root bash -lc '
        set -euo pipefail
        export DEBIAN_FRONTEND=noninteractive
        if command -v docker >/dev/null 2>&1; then
            echo "docker already installed"
        else
            apt-get update -qq
            apt-get install -y -qq ca-certificates curl gnupg
            install -m 0755 -d /etc/apt/keyrings
            curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
                -o /etc/apt/keyrings/docker.asc
            chmod a+r /etc/apt/keyrings/docker.asc
            echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo $VERSION_CODENAME) stable" \
                > /etc/apt/sources.list.d/docker.list
            apt-get update -qq
            apt-get install -y -qq docker-ce docker-ce-cli containerd.io \
                docker-buildx-plugin docker-compose-plugin
        fi

        # Give this machine its own Docker CLI config. Without it the machine inherits
        # DOCKER_CONFIG from the Mac and every docker command is aimed at the host
        # engine (see VM_DOCKER_ENV in lib.sh). The profile drop-in sorts after the
        # OrbStack 999- script so interactive shells get the same treatment.
        mkdir -p /etc/docker-cli /etc/systemd/system/docker.service.d
        printf "export DOCKER_CONFIG=/etc/docker-cli\nexport DOCKER_HOST=unix:///var/run/docker.sock\n" \
            > /etc/profile.d/zzz-testbed-docker.sh
        chmod 0644 /etc/profile.d/zzz-testbed-docker.sh
    '

    # dockerd needs the proxy in its own systemd environment: the pull happens in the
    # daemon, not in the CLI, so exporting it in a shell would change nothing.
    if [ -n "$PROXY" ]; then
        orb -m "$vm" -u root tee /etc/systemd/system/docker.service.d/http-proxy.conf >/dev/null <<CONF
[Service]
Environment="HTTP_PROXY=$PROXY"
Environment="HTTPS_PROXY=$PROXY"
Environment="NO_PROXY=$NO_PROXY_LIST"
CONF
    else
        orb -m "$vm" -u root rm -f /etc/systemd/system/docker.service.d/http-proxy.conf
    fi

    vm_root "$vm" '
        set -euo pipefail
        systemctl daemon-reload
        systemctl enable --now docker
        systemctl restart docker
        for _ in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done
        docker info --format "engine {{.ServerVersion}} on {{.OperatingSystem}} ({{.Name}}) proxy={{.HTTPProxy}}"
    '
}

if [ -n "$PROXY" ]; then log "machines will pull through $PROXY"; else warn "no Mac proxy detected; machines will pull directly"; fi

pids=""
for vm in "${VMS[@]}"; do
    log "configuring docker on $vm (log: $LOGDIR/docker-$vm.log)"
    install_one "$vm" >"$LOGDIR/docker-$vm.log" 2>&1 &
    pids="$pids $!"
done

rc=0
for pid in $pids; do wait "$pid" || rc=1; done

for vm in "${VMS[@]}"; do
    printf '  %-10s %s\n' "$vm" "$(tail -1 "$LOGDIR/docker-$vm.log")"
done
[ "$rc" -eq 0 ] || { warn "at least one machine failed; see $LOGDIR"; exit 1; }
