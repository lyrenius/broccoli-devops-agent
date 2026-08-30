//! Issues, Human Reports, and Snapshot Judge candidate problems.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{
    ArtifactId, CandidateId, Confidence, EventId, IssueId, ResourceId, SnapshotId,
};
use crate::error::{AgentError, AgentResult};

/// Source of a formal Issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSource {
    /// Created after the Scheduler accepts a candidate proposed by the Snapshot Judge.
    Judge,
    /// Reported directly by a human operator.
    Human,
}

/// Scheduling priority of an Issue.
///
/// Variants are ordered from low to high for direct comparison by the in-memory Scheduler. Only a
/// Human Report can receive `HumanTop`; Judge recommendations are capped at `Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssuePriority {
    /// Low-priority maintenance item.
    Low,
    /// Normal problem.
    Normal,
    /// Problem that should be handled with priority.
    High,
    /// Problem that clearly affects a core capability.
    Critical,
    /// Highest priority reserved for human reports.
    HumanTop,
}

impl IssuePriority {
    /// Restricts a priority proposed by any model or the Judge to non-human authority.
    ///
    /// Initially this only prevents non-human proposals from producing `HumanTop`. Contest phase,
    /// deduplication, alert source, and other effective-priority rules can be added here later.
    pub fn model_safe(self) -> Self {
        match self {
            Self::HumanTop => Self::Critical,
            other => other,
        }
    }
}

/// State of an Issue in the scheduling lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    /// Created but not yet under investigation.
    Open,
    /// Under investigation by one or more Agent Teams.
    Investigating,
    /// Waiting for a human to provide information or choose an option.
    WaitingForHuman,
    /// A mitigation or repair action is in progress.
    Mitigating,
    /// An action has run and system recovery is being verified.
    Verifying,
    /// The problem has been resolved.
    Resolved,
    /// Investigation or repair failed.
    Failed,
    /// A human or the Scheduler cancelled the problem.
    Cancelled,
}

impl IssueStatus {
    /// Returns whether this state ends the Issue lifecycle.
    ///
    /// Store recovery queries use this function to exclude Issues that need no further work.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Failed | Self::Cancelled)
    }
}

/// Reference to the development workspace dedicated to an Issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentWorkspaceRef {
    /// Worktree ID allocated by the Agents Platform.
    pub worktree_id: String,
    /// ID of the repository being modified.
    pub repository_id: String,
    /// Base Git revision used to create the Worktree.
    pub base_revision: String,
    /// Branch name used by the Worktree.
    pub branch_name: String,
}

/// Problem report submitted by a human operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanReport {
    /// ID of the report itself.
    pub report_id: Uuid,
    /// Identity of the reporter.
    pub reporter: String,
    /// Title suitable for display in a queue.
    pub title: String,
    /// Symptoms and context observed by the human.
    pub description: String,
    /// Priority explicitly chosen by the reporter, or `None` to accept the `HumanTop` default.
    ///
    /// Only a human can put an Issue at `HumanTop`, but a human may deliberately file a low-urgency
    /// report (for example a printer running low on ink) without preempting a critical detected
    /// outage.
    pub priority: Option<IssuePriority>,
    /// Resources the human believes may be affected.
    pub affected_resource_ids: Vec<ResourceId>,
    /// Artifact IDs attached to the report.
    pub attachment_artifact_ids: Vec<ArtifactId>,
    /// Outcome explicitly requested by the human, or `None` when unspecified.
    pub requested_outcome: Option<String>,
    /// Time at which the report was created.
    pub created_at: DateTime<Utc>,
}

impl HumanReport {
    /// Creates a minimal human report.
    ///
    /// The caller can add resources, attachments, and an expected outcome before submission to the
    /// Scheduler. This function does not capture a Snapshot; the Scheduler must separately bind an
    /// already persisted Snapshot.
    pub fn new(
        reporter: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            report_id: Uuid::now_v7(),
            reporter: reporter.into(),
            title: title.into(),
            description: description.into(),
            priority: None,
            affected_resource_ids: Vec::new(),
            attachment_artifact_ids: Vec::new(),
            requested_outcome: None,
            created_at: Utc::now(),
        }
    }
}

/// Potential problem proposed by the Snapshot Judge for one Snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueCandidate {
    /// Candidate problem ID.
    pub candidate_id: CandidateId,
    /// ID of the Snapshot analyzed by the Judge.
    pub snapshot_id: SnapshotId,
    /// Candidate problem title.
    pub title: String,
    /// Judge summary of the potential problem.
    pub summary: String,
    /// Priority recommended by the Judge.
    pub proposed_priority: IssuePriority,
    /// Resources that may be affected.
    pub affected_resource_ids: Vec<ResourceId>,
    /// Event IDs supporting the candidate.
    pub evidence_ids: Vec<EventId>,
    /// Judge confidence in the assessment.
    pub confidence: Confidence,
    /// Stable key used by the Scheduler to merge duplicates.
    pub deduplication_key: String,
    /// Time at which the candidate was created.
    pub created_at: DateTime<Utc>,
}

impl IssueCandidate {
    /// Creates a candidate problem that the Scheduler has not yet accepted.
    ///
    /// This constructor neither creates a formal Issue nor validates the deduplication key; those
    /// decisions belong to the Top Scheduler.
    pub fn new(
        snapshot_id: SnapshotId,
        title: impl Into<String>,
        summary: impl Into<String>,
        proposed_priority: IssuePriority,
        confidence: Confidence,
        deduplication_key: impl Into<String>,
    ) -> Self {
        Self {
            candidate_id: Uuid::now_v7(),
            snapshot_id,
            title: title.into(),
            summary: summary.into(),
            proposed_priority,
            affected_resource_ids: Vec::new(),
            evidence_ids: Vec::new(),
            confidence,
            deduplication_key: deduplication_key.into(),
            created_at: Utc::now(),
        }
    }
}

/// A problem formally tracked by the Top Scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Issue ID.
    pub issue_id: IssueId,
    /// Issue source.
    pub source: IssueSource,
    /// Human Report or Judge Event ID that created the Issue.
    pub source_event_id: EventId,
    /// Issue title.
    pub title: String,
    /// Problem description for the Issue.
    pub description: String,
    /// Scheduling priority computed by the Scheduler.
    pub priority: IssuePriority,
    /// Current lifecycle state.
    pub status: IssueStatus,
    /// Snapshot ID captured when the Issue was created.
    pub opened_snapshot_id: SnapshotId,
    /// Snapshot ID the Scheduler currently considers most relevant.
    pub current_snapshot_id: SnapshotId,
    /// IDs of affected resources.
    pub affected_resource_ids: Vec<ResourceId>,
    /// Event IDs for currently confirmed evidence.
    pub evidence_ids: Vec<EventId>,
    /// Issue-level Worktree reference used by the Develop Team.
    pub development_workspace: Option<DevelopmentWorkspaceRef>,
    /// Time at which the Issue was created.
    pub created_at: DateTime<Utc>,
    /// Time at which the Issue was last updated.
    pub updated_at: DateTime<Utc>,
}

impl Issue {
    /// Converts a Human Report into a formal Issue that defaults to the highest priority.
    ///
    /// `source_event_id` must reference a human-report event already written to the EventLog. The
    /// reporter may deliberately choose a lower priority; when none is chosen the Issue is
    /// `HumanTop`. Only this human path can produce `HumanTop`, preventing a model or Judge from
    /// forging the same priority.
    pub fn from_human_report(
        report: HumanReport,
        snapshot_id: SnapshotId,
        source_event_id: EventId,
    ) -> Self {
        let now = Utc::now();
        Self {
            issue_id: Uuid::now_v7(),
            source: IssueSource::Human,
            source_event_id,
            title: report.title,
            description: report.description,
            priority: report.priority.unwrap_or(IssuePriority::HumanTop),
            status: IssueStatus::Open,
            opened_snapshot_id: snapshot_id,
            current_snapshot_id: snapshot_id,
            affected_resource_ids: report.affected_resource_ids,
            evidence_ids: Vec::new(),
            development_workspace: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Promotes a recorded Judge candidate to a formal Issue.
    ///
    /// This function caps the Judge-proposed priority but does not yet deduplicate candidates or merge
    /// alerts. The Scheduler applies those policies before calling it.
    pub fn from_candidate(candidate: IssueCandidate, source_event_id: EventId) -> Self {
        let now = Utc::now();
        Self {
            issue_id: Uuid::now_v7(),
            source: IssueSource::Judge,
            source_event_id,
            title: candidate.title,
            description: candidate.summary,
            priority: candidate.proposed_priority.model_safe(),
            status: IssueStatus::Open,
            opened_snapshot_id: candidate.snapshot_id,
            current_snapshot_id: candidate.snapshot_id,
            affected_resource_ids: candidate.affected_resource_ids,
            evidence_ids: candidate.evidence_ids,
            development_workspace: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Updates the Snapshot the Scheduler currently uses to understand the Issue.
    ///
    /// This method does not modify existing Jobs. Old Jobs remain bound to their original Snapshot;
    /// new evidence requires a superseding Job.
    pub fn update_current_snapshot(&mut self, snapshot_id: SnapshotId) {
        self.current_snapshot_id = snapshot_id;
        self.updated_at = Utc::now();
    }

    /// Returns whether the minimal state machine allows the given transition from the current state.
    ///
    /// The Scheduler uses this to apply optional lifecycle updates (for example after a Job result)
    /// only when they are legal, without treating an inapplicable update as an error.
    pub fn can_transition_to(&self, next: IssueStatus) -> bool {
        is_valid_transition(self.status, next)
    }

    /// Transitions an Issue according to the minimal state machine.
    ///
    /// This function rejects obvious errors such as reopening a terminal state. Future permissions,
    /// operation-mode rules, and evidence preconditions belong in Scheduler policy rather than this
    /// domain object.
    pub fn transition_to(&mut self, next: IssueStatus) -> AgentResult<()> {
        if !is_valid_transition(self.status, next) {
            return Err(AgentError::InvalidTransition {
                entity: "Issue",
                from: format!("{:?}", self.status),
                to: format!("{next:?}"),
            });
        }

        self.status = next;
        self.updated_at = Utc::now();
        Ok(())
    }
}

/// Returns whether an Issue transition belongs to the initially allowed minimal flow.
///
/// This function encodes only fundamental ordering that should not change with product policy.
/// Contest-phase permissions and automation boundaries are deferred to the Scheduler.
fn is_valid_transition(current: IssueStatus, next: IssueStatus) -> bool {
    use IssueStatus::{
        Cancelled, Failed, Investigating, Mitigating, Open, Resolved, Verifying, WaitingForHuman,
    };

    matches!(
        (current, next),
        (Open, Investigating | Cancelled | Failed)
            | (
                Investigating,
                WaitingForHuman | Mitigating | Resolved | Failed | Cancelled
            )
            | (
                WaitingForHuman,
                Investigating | Mitigating | Failed | Cancelled
            )
            | (Mitigating, Verifying | Failed | Cancelled)
            | (Verifying, Resolved | Mitigating | Failed | Cancelled)
    )
}
