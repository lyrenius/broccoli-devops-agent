//! Structured errors shared across the crate.

use thiserror::Error;

/// Unified result type for Broccoli DevOps Agent.
pub type AgentResult<T> = Result<T, AgentError>;

/// Errors returned by the domain, storage, and scheduler scaffold.
///
/// The initial version retains only error categories on which callers can act. Network and database
/// adapters can add transport details later without leaking arbitrary string errors into the domain
/// layer.
#[derive(Debug, Error)]
pub enum AgentError {
    /// An insert found an existing object with the same ID.
    #[error("{entity} `{id}` already exists")]
    Duplicate {
        /// Kind of object that conflicts.
        entity: &'static str,
        /// ID of the conflicting object.
        id: String,
    },

    /// A query or update could not find its target.
    #[error("{entity} `{id}` was not found")]
    NotFound {
        /// Kind of object that was not found.
        entity: &'static str,
        /// ID of the missing object.
        id: String,
    },

    /// An object attempted a transition that its current state does not allow.
    #[error("{entity} cannot transition from `{from}` to `{to}`")]
    InvalidTransition {
        /// Kind of object whose transition was rejected.
        entity: &'static str,
        /// State before the requested transition.
        from: String,
        /// Requested destination state.
        to: String,
    },

    /// The current scheduler mode forbids the requested operation.
    #[error("Scheduler mode `{mode}` does not allow `{operation}`")]
    SchedulerFrozen {
        /// Current Scheduler mode.
        mode: String,
        /// Scheduling operation that was rejected.
        operation: &'static str,
    },

    /// Input objects contain an unacceptable inconsistency.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Structured event or model-boundary data could not be serialized.
    #[error("JSON serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}
