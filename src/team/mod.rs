//! Agent Team implementations behind the backend-neutral `AgentTeamPort`.
//!
//! Two implementations coexist and the Scheduler cannot tell them apart:
//!
//! - [`ReadOnlyOperateTeam`] — deterministic diagnosis straight from the Snapshot View; no model.
//! - [`HarnessOperateTeam`] — the same job driven through the `broccoli-agent-harness` agentic
//!   loop, generic over any `ModelClient` backend.
//!
//! A codex-backed Team is the planned third implementation; it will implement `AgentTeamPort`
//! directly, without the harness. The port stays the only seam the control plane depends on.

mod harness;
mod readonly;

pub use harness::HarnessOperateTeam;
pub use readonly::ReadOnlyOperateTeam;
