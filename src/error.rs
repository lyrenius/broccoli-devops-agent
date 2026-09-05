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

    /// An optimistic update found the stored record changed by someone else first.
    ///
    /// Every state transition is a compare-and-set against the record the caller read, so two
    /// operators approving the same action, or a retry racing its original, cannot both apply.
    #[error("{entity} `{id}` changed concurrently; re-read it and decide again")]
    Conflict {
        /// Kind of object that changed underneath the caller.
        entity: &'static str,
        /// ID of the object.
        id: String,
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

    /// A required port implementation has not been wired into the scaffold yet.
    ///
    /// The scaffold deliberately ships no placeholder adapters, so operations that need the
    /// Collector, View Builder, Platform, or Policy model fail explicitly instead of pretending an
    /// external capability exists.
    #[error("component `{component}` is not wired; cannot perform `{operation}`")]
    MissingDependency {
        /// Port that has no implementation wired in.
        component: &'static str,
        /// Operation that required the missing port.
        operation: &'static str,
    },

    /// Structured event or model-boundary data could not be serialized.
    #[error("JSON serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A filesystem operation failed while loading configuration or persisting state.
    ///
    /// The context names what the store or loader was doing so an operator can find the affected
    /// path without a debugger.
    #[error("I/O failure while {context}: {source}")]
    Io {
        /// What the caller was doing when the failure occurred.
        context: String,
        /// Underlying operating-system error.
        source: std::io::Error,
    },
}
