# Broccoli DevOps Agent

This is an agentic operations control plane for the Broccoli online judging system. It loads a static deployment topology, probes real endpoints, builds immutable Snapshots, accepts human reports, dispatches an Operate Job over an exact sanitized Snapshot View (through a deterministic Team or a model-backed one), runs the Job's proposed actions through an approved authority matrix into the Agents Platform, verifies their effect, parks every denial and failure in a three-category inbox for a human, sends human feedback back upstream as a revising Job, persists everything to disk, and recovers control state after a restart. Machines are touched only through the runbook commands you configure, and only once you leave dry-run.

See the [architecture document](./docs/architecture.md) for the complete design and the [Excalidraw source](./docs/broccoli-devops-agent-architecture.excalidraw) for the editable diagram.

## Configuration

Two files, both safe to commit — neither holds credentials:

- `config/agent.toml` (copy from [`config/agent.example.toml`](./config/agent.example.toml)): data directory, topology path, and the model relay (`base_url`, `model`, `wire_api`) with the **name** of the environment variable that carries the API key. Export that variable before running a model-backed command.
- `config/topology.toml` (copy from [`config/topology.example.toml`](./config/topology.example.toml)): every machine and endpoint — PostgreSQL, Redis, CephFS/object storage, the API server, frontend, judge workers, and stations — with the read-only probes used to observe them.

`broccoli-devops-agent config show` prints the effective configuration as JSON (key redacted) for a frontend or for checking what the agent will actually use.

## Operator UIs

The control plane exposes an HTTP + SSE API (`serve`), and two user interfaces are pure clients of it — one process, two consoles, no UI-only state:

```bash
# Terminal 1: the control plane API (localhost:4720 by default; see [api] in config/agent.toml).
cargo run -- serve

# Terminal 2: the web console — plain React + Vite, no Broccoli dependencies.
cd web && pnpm install && pnpm dev          # http://localhost:5180, /api proxied to :4720

# Or the terminal console.
cargo run -p broccoli-tui                   # --api http://127.0.0.1:4720 --token ...
```

The web console has the inbox in its three categories — permission requests (approve, or reject with a comment), permission denials (who refused and why; send back upstream or acknowledge), and failures (jobs and actions; the same two decisions) — plus the live Snapshot with coverage gaps, issues and jobs with their feedback and transcript links, a live event stream, freeze/resume controls, and the human-report form. Every decision records the operator's name. The TUI covers the same operations from a terminal: `1-4` screens, `j/k` select, `a` approve, `r` reject, `b` send back upstream, `x` acknowledge (the last three prompt for a comment), `s` snapshot, `f`/`F`/`u` freeze dispatch, freeze all, resume; `--as NAME` sets the recorded operator. Set `api.token` in the config (and pass `--token` to the TUI) before binding beyond localhost.

To try the consoles without a model or a deployment, seed a demo data directory that already holds one item of each inbox category, then serve it with the deterministic Team:

```bash
cargo run --example seed_demo -- data-demo
cargo run -- serve --data data-demo --topology data-demo/topology.toml --team readonly
```

## Workspace Layout

The repository is a Cargo workspace with three crates and a strict dependency direction:

- **`broccoli-devops-agent`** (root) — the control plane: domain model, ports, Scheduler, Collector, stores, Platform, authority policy, Teams, the HTTP API, and CLI. Its ports (`AgentTeamPort`, `SchedulerPolicyPort`, `SnapshotJudgePort`) are the backend-neutral seam for model-backed work.
- **[`crates/harness`](./crates/harness)** (`broccoli-agent-harness`) — our own model-agnostic agentic loop: typed allowlisted tools, terminal tools for structured output, turn/tool-call budgets, cooperative cancellation, and replayable transcripts. It is generic over its `ModelClient` boundary (the OpenAI-compatible relay client lives behind its `openai` feature) and knows nothing about Broccoli.
- **[`crates/tui`](./crates/tui)** (`broccoli-tui`) — the terminal console, a pure HTTP client of the API.
- **[`web/`](./web)** — the web console (React 19 + Vite + TypeScript), also a pure API client, served by Vite separately.

The control plane depends on the harness, never the reverse; the UIs depend on nothing but the API. Model-backed integrations meet the Scheduler only at the ports: `team::HarnessOperateTeam` adapts `AgentTeamPort` onto the harness today, and a codex-backed Team implementing the same port directly is the planned second option — the Scheduler cannot tell any of them apart.

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

## Actions and the inbox

When a Job proposes operations, `report` runs each one through the approved authority matrix ([docs/action-authority.md](./docs/action-authority.md), encoded in `src/policy.rs`): `auto` rows execute through the Agents Platform and are verified against an after-Snapshot immediately, `approve` rows wait in the **Permission Request** inbox, `deny` rows are cancelled with the rule's rationale kept on the ActionRun and wait in the **Permission Denied** inbox. A human rejection lands in the same denied inbox with the human's comment. Jobs that fail, and actions whose execution or verification fails, wait in the **Failed** inbox. A denied or failed item is reviewed in one of two ways: *acknowledge* it, or *send it back upstream* — the agent then runs a revising Job over a fresh Snapshot with the denial reason, the failure summary, and your comments in front of it, and its new proposals go through the matrix again ([docs/architecture.md §4.10](./docs/architecture.md)).

```bash
cargo run -- inbox                                            # the three categories
cargo run -- actions approve <id> --as alice                  # executes and verifies a waiting action
cargo run -- actions reject <id> --as alice --comment "..."   # cancels it; comment travels with it
cargo run -- review action <id> --upstream --comment "..."    # revising Job runs now with the feedback
cargo run -- review job <id> --acknowledge                    # recorded; no further automatic work
cargo run -- actions list                                     # every ActionRun with denial and review
```

The Platform executes runbooks as the commands you map in `config/agent.toml` under `[[platform.runbooks]]` (for example `ssh {target} sudo systemctl restart broccoli-worker`); credentials stay with your SSH agent. It starts in **dry-run** mode — commands are rendered and recorded as Artifacts, not executed — until you set `dry_run = false`. Verification treats a command's exit code zero as evidence only: the target must be Healthy in the after-Snapshot, or the action ends as `VerificationFailed`.

## Recommended Reading Order

1. `src/domain/`: start with Snapshot, Issue, Job, ActionRun, Artifact, and EventLog; `review.rs` holds denials, reviews, and the feedback that travels upstream.
2. `src/ports.rs`: learn the boundaries around the Collector, Snapshot Judge, Agent Team (callback sink and cancellation), Agents Platform, Scheduler Policy, Reporter, and Store.
3. `src/scheduler.rs`: see how the AI-integrated Top Scheduler accepts human reports, requests Snapshot captures, triages candidates through the policy model with deterministic fallbacks, creates and supersedes Jobs, handles callbacks, gates ActionRuns, and manages freeze/recovery.
4. `src/topology.rs` and `src/collector.rs`: the static deployment map and the probe-driven Collector behind `CollectorPort`.
5. `src/view.rs`: the redacting Snapshot View Builder and content-hashed artifact store.
6. `src/team/`: the deterministic read-only Operate Team and the harness-backed `HarnessOperateTeam` — two backends behind one `AgentTeamPort`.
7. `crates/harness/src/`: the agent loop (`agent.rs`), tool registry (`tool.rs`), and `ModelClient` boundary (`client.rs`).
8. `src/store/file.rs`: the file-backed Store that makes restart recovery real (`src/store/memory.rs` remains for tests).
9. `src/runner.rs` and `src/main.rs`: wiring, the inbox projection, the review flows, and the operator CLI.
10. `src/api.rs`, `crates/tui/`, and `web/src/`: the HTTP + SSE API and its two consoles.

Code identifiers and all documentation are written in English. Documentation comments focus on why an item exists and where future decisions belong instead of merely repeating its name.

## Current Boundaries

The v0.1 slice deliberately does not implement:

- Model-backed Scheduler decisions: the harness, its OpenAI-compatible client,
  and the harness-backed Operate Team are implemented and wired to the relay,
  but the Scheduler Policy and Judger adapters are not, so every Scheduler
  decision point still runs its conservative deterministic fallback.
- Real machine mutation out of the box: the Platform executes only the runbook commands you configure, and stays in dry-run until you opt in.
- Deciding an Issue's fate from the inbox: acknowledging a denial or failure records the review and stops, but does not resolve or cancel the Issue.
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
