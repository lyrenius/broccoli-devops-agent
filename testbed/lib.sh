#!/usr/bin/env bash
# Shared settings for the OrbStack testbed.
#
# The testbed stands in for the contest LAN: three Linux machines, each with its own
# Docker engine and its own address, so the control plane running on the Mac probes real
# cross-machine endpoints instead of localhost ports.
#
# Machines are addressed by their OrbStack DNS name (`infra-1.orb.local`) rather than by
# IP: the names resolve from the Mac and from inside every machine, and they survive the
# restarts that the fault-injection scenarios cause. Note that only the fully qualified
# form resolves — bare `infra-1` does not.

set -euo pipefail

TESTBED_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$TESTBED_DIR/.." && pwd)"
LOGDIR="$TESTBED_DIR/.log"

# Machine names double as the topology's `node` identifiers.
INFRA_VM="infra-1"
APP_VM="app-1"
JUDGE_VM="judge-1"
VMS=("$INFRA_VM" "$APP_VM" "$JUDGE_VM")

INFRA_HOST="$INFRA_VM.orb.local"
APP_HOST="$APP_VM.orb.local"
JUDGE_HOST="$JUDGE_VM.orb.local"

# app-1 is also the builder: it needs the server image anyway, so only the worker image
# has to travel between engines.
BUILDER_VM="$APP_VM"

DISTRO="ubuntu:24.04"
BROCCOLI_SRC="/Users/lyre/Desktop/Projects/THUSAAC/broccoli"

# Credentials for the testbed only. These are deliberately weak and deliberately
# committed: nothing here is reachable from outside this Mac, and the control plane must
# never be handed real secrets.
PG_USER="broccoli"
PG_PASSWORD="broccoli_pg_secret"
PG_DB="broccoli"
REDIS_PASSWORD="broccoli_redis_secret"
S3_ACCESS_KEY="broccoli_s3_access"
S3_SECRET_KEY="broccoli_s3_secret"
S3_BUCKET="broccoli-blobs"
JWT_SECRET="testbed-only-jwt-secret-not-a-real-one-0123456789"

# First-run admin, so scenarios can drive the Broccoli API without a manual setup step.
ADMIN_USER="admin"
ADMIN_PASSWORD="testbed-admin-pw"

# Every service keeps its natural port: the machines are separate hosts, so there is no
# localhost collision to work around.
PG_PORT=5432
REDIS_PORT=6379
SEAWEED_MASTER_PORT=9333
SEAWEED_S3_PORT=8333
SERVER_PORT=3000

# Outbound HTTP proxy for the machines.
#
# The Mac reaches Docker Hub through a local proxy; the machines route straight out and
# get their registry connections cut. OrbStack exposes the Mac to every machine as
# host.orb.internal, so the machines borrow the same proxy. Derived from the Mac
# environment, and empty when the Mac has no proxy -- in which case setup skips it.
proxy_url() {
    local p="${HTTPS_PROXY:-${HTTP_PROXY:-}}"
    [ -n "$p" ] || return 0
    echo "$p" | sed -e 's|127\.0\.0\.1|host.orb.internal|' -e 's|localhost|host.orb.internal|'
}

# Never proxy the testbed itself: machine-to-machine and container-to-container traffic
# must stay on the OrbStack network, or the probes would measure the proxy.
NO_PROXY_LIST="localhost,127.0.0.1,::1,.orb.local,.orb.internal,.local,192.168.0.0/16,172.16.0.0/12,10.0.0.0/8"

vm_host() { echo "$1.orb.local"; }

# IPv4 on the OrbStack bridge, resolved through the same DNS the topology uses.
vm_ip() { dscacheutil -q host -a name "$1.orb.local" 2>/dev/null | awk '/^ip_address/{print $2; exit}'; }

# Docker CLI environment forced on every in-machine command.
#
# OrbStack exports DOCKER_CONFIG=/Users/<user>/.config/docker inside each machine, so all
# three machines and the Mac share one CLI config -- including its current context, which
# points at the host engine. Left alone, a "docker compose up" issued inside infra-1
# silently creates containers on the Mac instead, collapsing the three hosts into one and
# making every cross-machine test meaningless. Each machine therefore gets its own CLI
# config directory and an explicit socket.
VM_DOCKER_ENV="DOCKER_CONFIG=/etc/docker-cli DOCKER_HOST=unix:///var/run/docker.sock"

# Run a command inside a machine as root, pinned to that machine own docker engine.
vm_root() {
    local vm="$1"; shift
    orb -m "$vm" -u root bash -lc "export $VM_DOCKER_ENV; $*"
}

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }
