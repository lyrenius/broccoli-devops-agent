# Fault-injection scenarios

Each script breaks the testbed in one specific way, so the control plane can be watched
forming a Snapshot, judging it, and proposing actions against a fault whose ground truth
is known in advance.

Run one, then drive the agent:

```bash
testbed/scenarios/stop-service.sh redis-mq
cargo run -- --config config/agent.testbed.toml snapshot
cargo run -- --config config/agent.testbed.toml report \
    --title "Submissions are not being judged" \
    --description "Queue is not draining since 10:12"
testbed/scenarios/restore-all.sh
```

The scenarios differ in what the Collector can and cannot see, which is the point:

| Scenario | What the v0.1 probes see | What the agent has to work out |
|---|---|---|
| `stop-service.sh redis-mq` | `redis-mq` Down | Which dependents are affected, and in what order to recover |
| `stop-service.sh broccoli-server` | server and frontend Down | Whether the cause is the server or something under it |
| `stop-service.sh worker-1` | **nothing** — the worker has no probe | The failure is invisible to probes; only a human report or queue depth reveals it |
| `partition.sh app-1 infra-1` | server Down, infra all Healthy | Infrastructure is fine on its own; the link between two hosts is not |
| `slow-storage.sh` | everything Healthy, latency 100x higher | Latency, not availability -- reachability probes cannot express it |
| `slow-storage.sh 600ms` | server Down, its dependencies Healthy | Nothing crashed: the server health budget ran out, so the only resource that looks broken is the one still working |

`stop-service.sh worker-1` and `slow-storage.sh` are the two that should *fail* to show up
in a Snapshot. That is the coverage gap working as designed, not a bug: the topology
declares the worker unprobed, and the agent is supposed to say so rather than guess.
