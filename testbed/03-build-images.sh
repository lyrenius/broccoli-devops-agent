#!/usr/bin/env bash
# Build the Broccoli server and worker images on the builder machine, then hand the
# worker image to judge-1.
#
# Builds run inside app-1 against the source tree that OrbStack already mounts at the
# same path, so nothing is copied to the VM by hand. They are sequential on purpose:
# both Dockerfiles share the BuildKit cache mounts for the cargo registry and
# /app/target, and concurrent builds would serialise on those locks anyway.
#
# Set USE_CN_MIRRORS=true to route rustup/crates/apt through the Tsinghua mirrors that
# the Dockerfiles already support.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
mkdir -p "$LOGDIR"

USE_CN_MIRRORS="${USE_CN_MIRRORS:-false}"

# The daemon proxy covers base-image pulls, but rustup, cargo, apt, pnpm, and the isolate
# clone from GitHub all run inside build containers and need their own egress.
#
# Two things are required, not one. The proxy address has to be passed as a build arg
# (BuildKit treats these three names as predefined, so no Dockerfile change is needed),
# and the build has to run with --network=host. OrbStack publishes the Mac to a machine as
# host.orb.internal, but that name resolves only to an IPv6 address, and Docker containers
# get no IPv6 route -- so from a normal build container the proxy is simply unreachable
# and rustup hangs until it times out. Running the build in the machine own network
# namespace, where the IPv6 address does route, fixes it.
# Both Dockerfiles declare `ARG CARGO_BUILD_JOBS` with no default and then
# `ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}`, so leaving it unset exports an *empty*
# value and cargo refuses to start:
#   error: could not parse ``. Number of parallel jobs should be `default` or a number.
# It has to be passed explicitly. Defaults to the builder core count.
BUILD_JOBS="${BUILD_JOBS:-$(orb -m "$BUILDER_VM" nproc 2>/dev/null || echo 4)}"

PROXY="$(proxy_url || true)"
PROXY_ARGS=""
JOB_ARGS="--build-arg CARGO_BUILD_JOBS=$BUILD_JOBS"
if [ -n "$PROXY" ]; then
    PROXY_ARGS="--build-arg HTTP_PROXY=$PROXY --build-arg HTTPS_PROXY=$PROXY --build-arg NO_PROXY=$NO_PROXY_LIST"
fi
SERVER_IMAGE="broccoli-server:testbed"
WORKER_IMAGE="broccoli-worker:testbed"
# runtime-icpc carries gcc/g++/python3/JDK, so the testbed can judge a real submission
# instead of only reporting that a worker process is alive.
WORKER_TARGET="runtime-icpc"

log "building $SERVER_IMAGE on $BUILDER_VM (cn_mirrors=$USE_CN_MIRRORS, jobs=$BUILD_JOBS)"
vm_root "$BUILDER_VM" "
    set -euo pipefail
    cd '$BROCCOLI_SRC'
    DOCKER_BUILDKIT=1 docker build --network=host $PROXY_ARGS $JOB_ARGS \
        --build-arg USE_CN_MIRRORS=$USE_CN_MIRRORS \
        -f Dockerfile.server -t $SERVER_IMAGE .
"

log "building $WORKER_IMAGE ($WORKER_TARGET) on $BUILDER_VM"
vm_root "$BUILDER_VM" "
    set -euo pipefail
    cd '$BROCCOLI_SRC'
    DOCKER_BUILDKIT=1 docker build --network=host $PROXY_ARGS $JOB_ARGS \
        --build-arg USE_CN_MIRRORS=$USE_CN_MIRRORS \
        --target $WORKER_TARGET \
        -f Dockerfile.worker -t $WORKER_IMAGE .
"

# The two engines share no storage, so the worker image travels as a stream between
# them. /tmp on the Mac would work too, but piping avoids a multi-GB temp file.
log "shipping $WORKER_IMAGE from $BUILDER_VM to $JUDGE_VM"
orb -m "$BUILDER_VM" -u root bash -lc "export $VM_DOCKER_ENV; docker save $WORKER_IMAGE" \
    | orb -m "$JUDGE_VM" -u root bash -lc "export $VM_DOCKER_ENV; docker load"

log "images:"
vm_root "$BUILDER_VM" "docker images --format '  {{.Repository}}:{{.Tag}} {{.Size}}' | grep broccoli"
vm_root "$JUDGE_VM" "docker images --format '  {{.Repository}}:{{.Tag}} {{.Size}}' | grep broccoli"
