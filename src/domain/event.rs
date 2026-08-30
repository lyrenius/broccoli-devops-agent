//! Inputs and persisted records for the append-only EventLog.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::{ActionRunId, ArtifactId, EventId, IssueId, JobId};

/// Trust boundary of information contained in an event body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentTrust {
    /// Content was produced entirely by controlled system components.
    TrustedSystem,
    /// Content came from contestants, external programs, or another untrusted source.
    UntrustedExternal,
    /// Content mixes system fields with external text and must be treated as untrusted.
    Mixed,
}

/// An event that has not yet received a global sequence number from the Store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    /// Event ID generated at creation time rather than assigned by the Store.
    pub event_id: EventId,
    /// Time at which the event actually occurred.
    pub occurred_at: DateTime<Utc>,
    /// Component or human identity that produced the event.
    pub actor: String,
    /// Stable event name, such as `scheduler.job_dispatched`.
    pub kind: String,
    /// Related Issue ID.
    pub issue_id: Option<IssueId>,
    /// Related Job ID.
    pub job_id: Option<JobId>,
    /// Related ActionRun ID.
    pub action_run_id: Option<ActionRunId>,
    /// Short summary for operators.
    pub summary: String,
    /// Structured body specific to this event.
    pub payload: Value,
    /// Artifact IDs referenced by the event.
    pub artifact_ids: Vec<ArtifactId>,
    /// Trust boundary of the event content.
    pub trust: ContentTrust,
}

impl NewEvent {
    /// Creates a minimal system event.
    ///
    /// Content defaults to controlled system output and is not bound to an Issue, Job, or ActionRun.
    /// Callers that include external text must explicitly lower the trust level with `with_trust`.
    pub fn new(
        actor: impl Into<String>,
        kind: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            occurred_at: Utc::now(),
            actor: actor.into(),
            kind: kind.into(),
            issue_id: None,
            job_id: None,
            action_run_id: None,
            summary: summary.into(),
            payload: Value::Null,
            artifact_ids: Vec::new(),
            trust: ContentTrust::TrustedSystem,
        }
    }

    /// Binds the event to an Issue.
    ///
    /// This method consumes and returns the draft, making it suitable for chained EventLog input
    /// construction.
    pub fn with_issue(mut self, issue_id: IssueId) -> Self {
        self.issue_id = Some(issue_id);
        self
    }

    /// Binds the event to a Job.
    ///
    /// This method only establishes traceability and does not check whether the Job belongs to the
    /// same Issue. A transactional Store implementation will later enforce cross-object consistency.
    pub fn with_job(mut self, job_id: JobId) -> Self {
        self.job_id = Some(job_id);
        self
    }

    /// Binds the event to an ActionRun.
    ///
    /// This method records only the relationship and does not change ActionRun state.
    pub fn with_action(mut self, action_run_id: ActionRunId) -> Self {
        self.action_run_id = Some(action_run_id);
        self
    }

    /// Sets the structured body specific to the event.
    ///
    /// The caller must already have redacted and size-limited the body. The initial EventLog does not
    /// automatically inspect sensitive fields.
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// Sets the Artifacts referenced by the event.
    ///
    /// This method does not check whether an Artifact is already persisted; a transactional
    /// persistence implementation will enforce that integrity later.
    pub fn with_artifacts(mut self, artifact_ids: Vec<ArtifactId>) -> Self {
        self.artifact_ids = artifact_ids;
        self
    }

    /// Explicitly sets the event content trust boundary.
    ///
    /// A future Snapshot View Builder will use this field to prevent untrusted text from being
    /// interpreted as Agent instructions.
    pub fn with_trust(mut self, trust: ContentTrust) -> Self {
        self.trust = trust;
        self
    }
}

/// An event persisted in the EventLog with a global sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Monotonically increasing sequence number assigned by the Store.
    pub sequence: u64,
    /// Event ID.
    pub event_id: EventId,
    /// Time at which the event actually occurred.
    pub occurred_at: DateTime<Utc>,
    /// Component or human identity that produced the event.
    pub actor: String,
    /// Stable event name.
    pub kind: String,
    /// Related Issue ID.
    pub issue_id: Option<IssueId>,
    /// Related Job ID.
    pub job_id: Option<JobId>,
    /// Related ActionRun ID.
    pub action_run_id: Option<ActionRunId>,
    /// Short human-facing summary.
    pub summary: String,
    /// Structured body specific to this event.
    pub payload: Value,
    /// Artifact IDs referenced by the event.
    pub artifact_ids: Vec<ArtifactId>,
    /// Trust boundary of the event content.
    pub trust: ContentTrust,
}

impl EventRecord {
    /// Combines a new event with its Store-assigned sequence into an immutable record.
    ///
    /// Only Store implementations call this function, preventing business components from forging
    /// EventLog order.
    pub(crate) fn from_new(sequence: u64, event: NewEvent) -> Self {
        Self {
            sequence,
            event_id: event.event_id,
            occurred_at: event.occurred_at,
            actor: event.actor,
            kind: event.kind,
            issue_id: event.issue_id,
            job_id: event.job_id,
            action_run_id: event.action_run_id,
            summary: event.summary,
            payload: event.payload,
            artifact_ids: event.artifact_ids,
            trust: event.trust,
        }
    }
}
