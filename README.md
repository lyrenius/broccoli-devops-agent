# Broccoli DevOps Agent

This is an agentic operations control plane for the Broccoli online judging system. The repository implements the **v0.1 vertical slice**: it loads a static deployment topology, probes real endpoints, builds immutable Snapshots, accepts human reports, dispatches a read-only Operate Job over an exact sanitized Snapshot View, persists everything to disk, and recovers control state after a restart. It does not modify any machine — ActionRun execution, model-backed agents, and SSH stay behind ports for later slices.

See the [architecture document](./docs/architecture.md) for the complete design and the [Excalidraw source](./docs/broccoli-devops-agent-architecture.excalidraw) for the editable diagram.

## Running the v0.1 slice

```bash
# 1. Describe your deployment (hosts, ports, probes, dependencies).
cp config/topology.example.toml config/topology.toml

# 2. Capture and display a Snapshot with its coverage gaps.
cargo run -- snapshot

# 3. File a human report; a read-only Operate Job diagnoses from the Snapshot View.
cargo run -- report --title "Contestants cannot submit" \
    --description "Web submissions time out since 10:12"

# 4. Simulate a controller restart and recover control state.
cargo run -- recover

# 5. Inspect the append-only event log.
cargo run -- events --tail 20
```

State lives under `./data/` (override with `--data`): one JSON document per Snapshot, Issue, Job, and Artifact record, artifact bodies under `data/artifact-bodies/`, and an append-only `data/events.jsonl`. Human reports default to the human-reserved top priority; pass `--priority low|normal|high|critical` to file lower.

## Recommended Reading Order

1. `src/domain/`: start with Snapshot, Issue, Job, ActionRun, Artifact, and EventLog.
2. `src/ports.rs`: learn the boundaries around the Collector, Snapshot Judge, Agent Team (callback sink and cancellation), Agents Platform, Scheduler Policy, Reporter, and Store.
3. `src/scheduler.rs`: see how the AI-integrated Top Scheduler accepts human reports, requests Snapshot captures, triages candidates through the policy model with deterministic fallbacks, creates and supersedes Jobs, handles callbacks, gates ActionRuns, and manages freeze/recovery.
4. `src/topology.rs` and `src/collector.rs`: the static deployment map and the probe-driven Collector behind `CollectorPort`.
5. `src/view.rs`: the redacting Snapshot View Builder and content-hashed artifact store.
6. `src/team.rs`: the deterministic read-only Operate Team behind `AgentTeamPort`.
7. `src/store/file.rs`: the file-backed Store that makes restart recovery real (`src/store/memory.rs` remains for tests).
8. `src/runner.rs` and `src/main.rs`: wiring and the operator CLI.

Code identifiers and all documentation are written in English. Documentation comments focus on why an item exists and where future decisions belong instead of merely repeating its name.

## Current Boundaries

The v0.1 slice deliberately does not implement:

- OpenAI Responses API integration (the Scheduler Policy, Snapshot Judge, and
  model-backed Agent Team ports are defined; every Scheduler decision point runs
  its conservative deterministic fallback).
- SSH, UFW, Docker, service, or configuration changes — no ActionRun executes.
- Authenticated PostgreSQL, Redis, object-storage, or Broccoli API probes; the
  v0.1 Probe Registry is `tcp.connect` and plain-HTTP `http.status` only.
- SQLite (the file-backed store keeps the same `StateStore` contract for a
  drop-in swap).
- A Reporter implementation or parallel subagents inside a Team (the
  `ReporterPort` boundary is reserved).
- Real Worktree, WASM, or Bundle builds and replacement.

Scheduler operations that need an unwired port fail with an explicit `MissingDependency` error instead of pretending an external capability exists.

## Validation

```bash
cargo fmt --check
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```
