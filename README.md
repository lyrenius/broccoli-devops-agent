# Broccoli DevOps Agent

This is an agentic operations control plane for the Broccoli online judging system. The repository implements the **v0.1 vertical slice**: it loads a static deployment topology, probes real endpoints, builds immutable Snapshots, accepts human reports, dispatches a read-only Operate Job over an exact sanitized Snapshot View, persists everything to disk, and recovers control state after a restart. It does not modify any machine — ActionRun execution, model-backed agents, and SSH stay behind ports for later slices.

See the [architecture document](./docs/architecture.md) for the complete design and the [Excalidraw source](./docs/broccoli-devops-agent-architecture.excalidraw) for the editable diagram.

## Configuration

Two files, both safe to commit — neither holds credentials:

- `config/agent.toml` (copy from [`config/agent.example.toml`](./config/agent.example.toml)): data directory, topology path, and the model relay (`base_url`, `model`, `wire_api`) with the **name** of the environment variable that carries the API key. Export that variable before running a model-backed command.
- `config/topology.toml` (copy from [`config/topology.example.toml`](./config/topology.example.toml)): every machine and endpoint — PostgreSQL, Redis, CephFS/object storage, the API server, frontend, judge workers, and stations — with the read-only probes used to observe them.

`broccoli-devops-agent config show` prints the effective configuration as JSON (key redacted) for a frontend or for checking what the agent will actually use.

## Workspace Layout

The repository is a Cargo workspace with two crates and a strict dependency direction:

- **`broccoli-devops-agent`** (root) — the control plane: domain model, ports, Scheduler, Collector, stores, Teams, and CLI. Its ports (`AgentTeamPort`, `SchedulerPolicyPort`, `SnapshotJudgePort`) are the backend-neutral seam for model-backed work.
- **[`crates/harness`](./crates/harness)** (`broccoli-agent-harness`) — our own model-agnostic agentic loop: typed allowlisted tools, terminal tools for structured output, turn/tool-call budgets, cooperative cancellation, and replayable transcripts. It is generic over its `ModelClient` boundary (where an OpenAI Responses client plugs in) and knows nothing about Broccoli.

The control plane depends on the harness, never the reverse. Model-backed integrations meet the Scheduler only at the ports: `team::HarnessOperateTeam` adapts `AgentTeamPort` onto the harness today, and a codex-backed Team implementing the same port directly is the planned second option — the Scheduler cannot tell any of them apart.

## Running the v0.1 slice

```bash
# 1. Describe your deployment (hosts, ports, probes, dependencies).
cp config/topology.example.toml config/topology.toml

# 2. Capture and display a Snapshot with its coverage gaps.
cargo run -- snapshot

# 3. File a human report; an Operate Job diagnoses from the Snapshot View.
#    With a [model] section in config/agent.toml and the key exported, the harness-backed
#    model Team is used; otherwise the deterministic Team runs. Force either with --team.
cp config/agent.example.toml config/agent.toml       # once; edit base_url/model if needed
export BROCCOLI_MODEL_API_KEY=...                     # never put the key in the file
cargo run -- check-model                              # one round trip to the relay
cargo run -- report --title "Contestants cannot submit" \
    --description "Web submissions time out since 10:12"
cargo run -- report --team readonly --title "..." --description "..."

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
6. `src/team/`: the deterministic read-only Operate Team and the harness-backed `HarnessOperateTeam` — two backends behind one `AgentTeamPort`.
7. `crates/harness/src/`: the agent loop (`agent.rs`), tool registry (`tool.rs`), and `ModelClient` boundary (`client.rs`).
8. `src/store/file.rs`: the file-backed Store that makes restart recovery real (`src/store/memory.rs` remains for tests).
9. `src/runner.rs` and `src/main.rs`: wiring and the operator CLI.

Code identifiers and all documentation are written in English. Documentation comments focus on why an item exists and where future decisions belong instead of merely repeating its name.

## Current Boundaries

The v0.1 slice deliberately does not implement:

- Model-backed Scheduler decisions: the harness, its OpenAI-compatible client,
  and the harness-backed Operate Team are implemented and wired to the relay,
  but the Scheduler Policy and Judger adapters are not, so every Scheduler
  decision point still runs its conservative deterministic fallback.
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
