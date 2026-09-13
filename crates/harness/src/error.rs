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
    /// A response supplied usage even though it could not produce a usable assistant turn.
    #[error("model response failure: {reason}")]
    Response {
        /// Response parsing or provider error.
        reason: String,
        /// Counts reported in the response, including explicit usage gaps.
        usage: crate::Usage,
        /// Whether the provider asked for a retry.
        retryable: bool,
    },
    /// The local context check refused the request before it reached the provider.
    #[error("context limit: {0}")]
    ContextLimit(String),
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

impl HarnessError {
    /// Whether the next attempt may recover without changing the request.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ModelUnavailable(_)
                | Self::Response {
                    retryable: true,
                    ..
                }
        )
    }
    /// Counts returned before a parsing/provider failure, otherwise one unknown-usage request.
    pub fn usage(&self) -> crate::Usage {
        match self {
            Self::Response { usage, .. } => *usage,
            Self::ContextLimit(_) => crate::Usage::default(),
            _ => crate::Usage::unreported(),
        }
    }
}
