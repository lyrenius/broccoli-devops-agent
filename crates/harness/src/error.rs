//! Errors the harness surfaces to its callers.

use thiserror::Error;

/// Unified result type for the harness crate.
pub type HarnessResult<T> = Result<T, HarnessError>;

/// Failures that end an agent run abnormally.
///
/// Model-visible problems (an unknown tool, bad arguments, a tool timeout) are deliberately NOT
/// errors: they become error tool-outputs inside the conversation so the model can recover, and
/// the budgets in [`crate::AgentConfig`] bound how long recovery may take. A transient backend
/// failure is retried by the loop before it becomes an error; a permanent one is not.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// The model backend failed permanently or returned something unusable (a malformed body,
    /// a rejected request); retrying the same request would not help.
    #[error("model backend failure: {0}")]
    Model(String),

    /// The model backend could not be reached or asked for a retry (a connection failure, a
    /// request timeout, HTTP 429 or a 5xx). The loop retries these within its retry budget.
    #[error("model backend unavailable: {0}")]
    ModelUnavailable(String),

    /// Two tools were registered under the same name.
    #[error("tool `{0}` is already registered")]
    DuplicateTool(String),

    /// Conversation or transcript data could not be serialized.
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}
