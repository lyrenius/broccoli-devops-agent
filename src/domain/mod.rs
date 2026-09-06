//! Core domain objects for the system.
//!
//! These types do not depend on OpenAI, SSH, or a database implementation. They describe semantics
//! the Agent must preserve under any adapter and are therefore the first part of the project to
//! understand.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod action;
pub mod artifact;
pub mod event;
pub mod issue;
pub mod job;
pub mod review;
pub mod snapshot;
pub mod trace;

pub use action::*;
pub use artifact::*;
pub use event::*;
pub use issue::*;
pub use job::*;
pub use review::*;
pub use snapshot::*;
pub use trace::*;

/// ID of a deployment instance.
pub type DeploymentId = Uuid;
/// ID of a Snapshot.
pub type SnapshotId = Uuid;
/// ID of an Issue.
pub type IssueId = Uuid;
/// ID of a Job.
pub type JobId = Uuid;
/// ID of an ActionRun.
pub type ActionRunId = Uuid;
/// ID of an Artifact.
pub type ArtifactId = Uuid;
/// ID of an Event.
pub type EventId = Uuid;
/// ID of an Issue Candidate.
pub type CandidateId = Uuid;
/// ID of an Agents Platform operation.
pub type PlatformOperationId = Uuid;
/// Stable ID of a machine, service, queue, Station, or another resource.
pub type ResourceId = String;

/// Operational phase currently active for the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationMode {
    /// Deployment or pre-contest rehearsal phase.
    Rehearsal,
    /// Locked phase while a contest is running.
    ContestLocked,
    /// Archival and review phase after a contest.
    PostContest,
}

/// Health level of a resource or the overall system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// Sufficient fresh evidence indicates normal operation.
    Healthy,
    /// The resource still works, but at least one capability has degraded.
    Degraded,
    /// The resource is unavailable or a core capability has failed.
    Down,
    /// Current evidence is insufficient to determine health.
    Unknown,
    /// The latest observation is too old to represent current state.
    Stale,
}

/// Severity of an issue, alert, or human report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A core contest capability is severely affected and needs immediate attention.
    Critical,
    /// Impact is significant and should be handled with priority.
    High,
    /// A clear problem exists but has not yet caused severe impact.
    Medium,
    /// A low-risk or non-urgent problem.
    Low,
    /// Status or background information only.
    Info,
}

/// Confidence assigned by a model or rule to a judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Multiple independent pieces of evidence consistently support the judgment.
    High,
    /// Existing evidence supports the judgment, but important unknowns remain.
    Medium,
    /// Only limited evidence supports the judgment.
    Low,
    /// Confidence cannot currently be estimated reasonably.
    Unknown,
}

/// A simple parameter consisting of a name and value.
///
/// Explicit key-value arrays are used instead of arbitrary JSON maps so future implementations can
/// apply allowlist validation and generate strict model DTOs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedValue {
    /// Parameter name.
    pub name: String,
    /// String representation of the parameter value.
    pub value: String,
}

impl NamedValue {
    /// Creates a parameter with an explicit name and value.
    ///
    /// This function does not interpret the value's business meaning. A concrete Runbook or adapter
    /// will later validate its type and allowed range.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}
