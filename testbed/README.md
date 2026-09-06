# OrbStack testbed

A three-machine stand-in for the contest LAN, used to exercise the control plane against
real services on real hosts instead of localhost ports.

Each machine is a full Linux VM with its own systemd, its own IP, and — importantly — its
own Docker engine, so "restart the worker on judge-1" is genuinely a different host from
"restart the server on app-1". The controller stays on the Mac, exactly as decided for the
real deployment, and reaches the machines over the network.

## Layout

| Machine | Resources | Services |
|---|---|---|
| `infra-1` | `postgres-main`, `redis-mq`, `seaweedfs-storage` | PostgreSQL 17, Redis 7, SeaweedFS (S3 + master) |
| `app-1` | `broccoli-server`, `web-frontend` | `broccoli-server`, serving the baked frontend assets; also the image builder |
| `judge-1` | `worker-1` | one judge worker with isolate, privileged |

Machines are addressed by their OrbStack DNS names (`infra-1.orb.local`), which resolve
both from the Mac and from inside every machine, and survive the restarts the scenarios
cause. Only the fully qualified form resolves; bare `infra-1` does not.

## Setup

```bash
testbed/01-create-machines.sh    # three Ubuntu 24.04 VMs
testbed/02-install-docker.sh     # one Docker engine per machine, proxy-aware
testbed/03-build-images.sh       # build server + worker on app-1, ship worker to judge-1
testbed/04-deploy-infra.sh       # PostgreSQL, Redis, SeaweedFS, and the blob bucket
testbed/05-deploy-app.sh         # broccoli-server
testbed/06-deploy-judge.sh       # judge worker
testbed/status.sh                # what is running and what the Mac can reach
```

Every script is idempotent and safe to re-run. `99-destroy.sh --yes` deletes the machines.

The build is the long step: it compiles the Broccoli workspace twice (server and worker)
inside `app-1`. Set `USE_CN_MIRRORS=true` to route rustup, cargo, and apt through the
Tsinghua mirrors the Dockerfiles already support.

## Driving the agent

```bash
cargo run -- --config config/agent.testbed.toml snapshot
cargo run -- --config config/agent.testbed.toml report \
    --title "Contestants cannot submit" --description "Web submissions time out since 10:12"
cargo run -- --config config/agent.testbed.toml actions list
cargo run -- --config config/agent.testbed.toml serve      # then the web or terminal console
```

`config/agent.testbed.toml` keeps its state in `data-testbed/` so testbed runs never mix
with real operator state, and — unlike the shipped config — it runs the Platform with
`dry_run = false`. Actions really restart containers, through `testbed/runbook.sh`, which
maps a topology resource ID to the machine and container that host it.

## Scenarios

See [`scenarios/README.md`](./scenarios/README.md). The interesting ones are the faults the
v0.1 probe registry *cannot* see: a stopped `worker-1` (no probe, by design) and
`slow-storage.sh` (everything stays Healthy, only latency moves).

## Notes on OrbStack

Three behaviours cost real debugging time here and are worth knowing before changing these
scripts.

**Every machine shares the Mac's Docker CLI config.** OrbStack exports
`DOCKER_CONFIG=/Users/<user>/.config/docker` inside each machine, and the current context
in that file points at the host engine. A `docker compose up` issued inside `infra-1` will
happily create containers *on the Mac* — the three hosts silently collapse into one, and
every cross-machine test becomes meaningless while still appearing to pass. Worse, running
`docker context use default` inside a machine rewrites the shared file and changes the
Mac's own context as a side effect. Hence `VM_DOCKER_ENV` in `lib.sh`: every in-machine
command gets its own `DOCKER_CONFIG` and an explicit socket.

**The Mac's proxy is reachable from machines but not from containers.** OrbStack publishes
the Mac as `host.orb.internal`, which resolves only to an IPv6 address. Machines route
IPv6 and can use the proxy; Docker containers get no IPv6 route, so inside a build the
proxy is simply unreachable and rustup hangs until it times out. Builds therefore run with
`--network=host`, in the machine's own namespace, with the proxy passed as a build arg.

**SeaweedFS binds the address it advertises.** Passing `-ip=infra-1.orb.local` makes the
master try to bind an address the container does not own, and it dies with
`cannot assign requested address`. The service name is used instead, so master, filer,
volume, and the S3 gateway talk over the compose network while external clients go through
the published port.
