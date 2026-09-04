//! Model-agnostic agentic harness for the Broccoli DevOps Agent.
//!
//! This crate is one of the two integration options the control plane will offer for model-backed
//! work (the other being a codex-backed adapter). It deliberately knows nothing about Broccoli:
//! no Snapshots, no Issues, no Scheduler. It provides exactly four things —
//!
//! - a typed conversation model with a replayable, serializable [`Transcript`],
//! - a [`ToolRegistry`] of allowlisted tools with JSON-schema parameter specs,
//! - the [`ModelClient`] boundary a concrete model backend implements, and
//! - [`run_agent`], a bounded loop that lets a model call tools until it produces a terminal
//!   result or hits a limit.
//!
//! With the `openai` feature, [`openai::OpenAiClient`] provides a [`ModelClient`] over any
//! OpenAI-compatible endpoint (official API or a relay), in either the Responses or Chat wire
//! format.
//!
//! The layering contract: the control plane's ports (`AgentTeamPort`, `SchedulerPolicyPort`, …)
//! remain the backend-neutral seam. Adapters in the control plane translate those ports onto this
//! harness; a future codex adapter implements the same ports without this crate. Nothing from
//! here may leak into a port signature.
//!
//! The runtime philosophy matches the architecture document: the model proposes, the harness
//! enforces. Unknown tools, malformed arguments, and timeouts become error outputs the model can
//! see and recover from; turn and tool-call budgets stop runaway loops; cancellation is
//! cooperative; and every step lands in the transcript so any run can be replayed byte-for-byte.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod agent;
pub mod client;
pub mod conversation;
pub mod error;
#[cfg(feature = "openai")]
pub mod openai;
pub mod testing;
pub mod tool;

pub use agent::{
    AgentConfig, AgentOutcome, AgentRunReport, CancelHandle, CancelToken, cancel_pair, run_agent,
};
pub use client::{AssistantItem, ModelClient, ModelRequest};
pub use conversation::{Item, Transcript, Trust, fence_untrusted};
pub use error::{HarnessError, HarnessResult};
pub use tool::{Tool, ToolHandler, ToolRegistry, ToolResult, ToolSpec, tool_fn};
