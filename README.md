# Broccoli DevOps Agent

This is an agentic operations control plane for the Broccoli online judging system. The repository is currently at the readable architecture scaffold stage: the code establishes the core objects, component boundaries, state transitions, and in-memory storage interfaces, but it does not connect to a real Broccoli deployment, OpenAI, SSH, or a database, and it does not modify any machine.

See the [architecture document](./docs/architecture.md) for the complete design and the [Excalidraw source](./docs/broccoli-devops-agent-architecture.excalidraw) for the editable diagram.

## Recommended Reading Order

1. `src/domain/`: start with Snapshot, Issue, Job, ActionRun, Artifact, and EventLog.
2. `src/ports.rs`: learn the boundaries around the Collector, Snapshot Judge, Agent Team (callback sink and cancellation), Agents Platform, Scheduler Policy, Reporter, and Store.
3. `src/scheduler.rs`: see how the AI-integrated Top Scheduler accepts human reports, requests Snapshot captures, triages candidates through the policy model with deterministic fallbacks, creates and supersedes Jobs, handles callbacks, gates ActionRuns, and manages freeze/recovery.
4. `src/store/memory.rs`: see how the initial in-memory Store persists state and assigns event sequence numbers.
5. `src/main.rs`: see how the binary entry point currently performs dependency wiring without running a simulated task.

Code identifiers and all documentation are written in English. Documentation comments focus on why an item exists and where future decisions belong instead of merely repeating its name.

## Current Boundaries

The initial version deliberately does not implement:

- OpenAI Responses API integration (the Scheduler Policy, Snapshot Judge, and
  Agent Team ports are defined but have no model-backed implementations).
- SSH, UFW, Docker, service, or configuration changes.
- PostgreSQL, Redis, object storage, or Broccoli API probes.
- SQLite or cross-process persistence.
- A Reporter implementation or parallel subagents inside a Team (the
  `ReporterPort` boundary is reserved).
- Real Worktree, WASM, or Bundle builds and replacement.

The traits, ActionRun, Artifact, and EventLog boundaries reserve integration points for these capabilities. Scheduler operations that need an unwired port fail with an explicit `MissingDependency` error instead of pretending an external capability exists.

## Validation

```bash
cargo fmt --check
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```
