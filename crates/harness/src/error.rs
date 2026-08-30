//! Errors the harness surfaces to its callers.

use thiserror::Error;

/// Unified result type for the harness crate.
pub type HarnessResult<T> = Result<T, HarnessError>;

/// Failures that end an agent run abnormally.
///
/// Model-visible problems (an unknown tool, bad arguments, a tool timeout) are deliberately NOT
/// errors: they become error tool-outputs inside the conversation so the model can recover, and
/// the budgets in [`crate::AgentConfig`] bound how long recovery may take.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// The model backend failed or returned something unusable.
    #[error("model backend failure: {0}")]
    Model(String),

    /// Two tools were registered under the same name.
    #[error("tool `{0}` is already registered")]
    DuplicateTool(String),

    /// Conversation or transcript data could not be serialized.
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}
