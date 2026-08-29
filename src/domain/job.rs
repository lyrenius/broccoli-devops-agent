//! Work Orders, Jobs, callbacks, and final results for Agent Teams.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{ArtifactId, EventId, IssueId, JobId, NamedValue, ResourceId, SnapshotViewRef};
use crate::error::{AgentError, AgentResult};

/// Kind of Agent Team that handles a Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamKind {
    /// Team that modifies code, plugins, WASM, or Bundles.
    Develop,
    /// Team that investigates and operates the deployment environment.
    Operate,
}

/// Lifecycle state of a Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Created and waiting for Scheduler dispatch.
    Queued,
    /// An Agent Team is processing the Job.
    Running,
    /// Waiting for a human to add information or select an option.
    WaitingForHuman,
    /// Current Snapshot evidence is insufficient and a new Snapshot is needed.
    NeedsResnapshot,
    /// The Team completed the current work.
    Completed,
    /// Team execution failed.
    Failed,
    /// The Team could continue but is currently blocked by an external condition.
    Blocked,
    /// A new Job based on an updated Snapshot has replaced this Job.
    Superseded,
    /// A human or the Scheduler cancelled the Job.
    Cancelled,
}

impl JobStatus {
    /// Returns whether recovery no longer needs to schedule this state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Superseded | Self::Cancelled
        )
    }
}

/// Explicit work instructions sent from the Scheduler to an Agent Team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkOrder {
    /// Objective the Team must achieve.
    pub objective: String,
    /// Result kinds or content the Team must return.
    pub expected_outputs: Vec<String>,
    /// Additional constraints for investigation or implementation.
    pub constraints: Vec<String>,
}

impl WorkOrder {
    /// Creates a Work Order with only an objective and no output requirements or constraints yet.
    ///
    /// The Scheduler will complete the boundaries before actual dispatch. The initial version does
    /// not derive work instructions automatically from an Issue.
    pub fn new(objective: impl Into<String>) -> Self {
        Self {
            objective: objective.into(),
            expected_outputs: Vec::new(),
            constraints: Vec::new(),
        }
    }
}

/// Observation an Agent Team asks the Collector to add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRequest {
    /// Probe ID registered in the Probe Registry.
    pub probe_id: String,
    /// Resources the Probe should observe.
    pub target_ids: Vec<ResourceId>,
    /// Reason the observation is needed.
    pub reason: String,
}

/// Candidate option returned by a Develop Team to a human and the Scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolutionOption {
    /// Candidate option ID.
    pub option_id: Uuid,
    /// Candidate option title.
    pub title: String,
    /// Candidate option summary.
    pub summary: String,
    /// Repository paths that may be modified.
    pub changed_paths: Vec<String>,
    /// Configuration keys that may be modified.
    pub changed_config_keys: Vec<String>,
    /// System resources that may be affected.
    pub affected_resource_ids: Vec<ResourceId>,
    /// Artifact IDs produced by the option.
    pub artifact_ids: Vec<ArtifactId>,
    /// Build and test summary.
    pub test_summary: String,
    /// Known risks.
    pub risks: Vec<String>,
}

impl SolutionOption {
    /// Creates a candidate option without patches, Artifacts, or test results yet.
    ///
    /// This constructor only helps a Team organize output. The Scheduler still performs conflict
    /// detection and aggregates human choices.
    pub fn new(title: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            option_id: Uuid::now_v7(),
            title: title.into(),
            summary: summary.into(),
            changed_paths: Vec::new(),
            changed_config_keys: Vec::new(),
            affected_resource_ids: Vec::new(),
            artifact_ids: Vec::new(),
            test_summary: String::new(),
            risks: Vec::new(),
        }
    }
}

/// Controlled operation proposal made by an Agent Team to the Scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionProposal {
    /// Runbook ID registered in the Agents Platform.
    pub runbook_id: String,
    /// Resources targeted by the operation.
    pub target_ids: Vec<ResourceId>,
    /// Structured arguments passed to the Runbook.
    pub arguments: Vec<NamedValue>,
    /// Reason the Team requests the operation.
    pub reason: String,
    /// Effect the Team expects from the operation.
    pub expected_effect: String,
    /// Probe IDs used to verify the effect after the operation.
    pub verification_probe_ids: Vec<String>,
}

/// Kind of final result returned by a Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    /// The Team solved the problem and no further action is needed.
    Solved,
    /// The Team completed diagnosis only; the Scheduler must choose the next step.
    DiagnosisOnly,
    /// A Develop Team returned multiple options that require human selection.
    OptionsReady,
    /// Current Snapshot evidence is insufficient and collection must run again.
    NeedsMoreData,
    /// A human must answer a question or grant permission.
    NeedsHuman,
    /// An external condition prevents the Team from continuing.
    Blocked,
    /// The Team could not complete the work.
    Failed,
}

/// Structured result returned when an Agent Team completes or pauses a Job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    /// Result kind.
    pub outcome: JobOutcome,
    /// Team summary of the current conclusion.
    pub summary: String,
    /// Event IDs supporting the conclusion.
    pub evidence_ids: Vec<EventId>,
    /// Artifact IDs produced by the Team.
    pub artifact_ids: Vec<ArtifactId>,
    /// New Probes requested by the Team.
    pub requested_probes: Vec<ProbeRequest>,
    /// Candidate options supplied by the Develop Team.
    pub options: Vec<SolutionOption>,
    /// ActionRuns the Team recommends that the Scheduler create.
    pub proposed_actions: Vec<ActionProposal>,
    /// Questions that remain unanswered.
    pub unresolved_questions: Vec<String>,
}

impl JobResult {
    /// Creates a Team result containing only its kind and summary.
    ///
    /// The Team can add evidence, Artifacts, Probes, options, or action proposals before returning.
    /// This function does not change Job state; `Job::complete` handles that consistently.
    pub fn new(outcome: JobOutcome, summary: impl Into<String>) -> Self {
        Self {
            outcome,
            summary: summary.into(),
            evidence_ids: Vec::new(),
            artifact_ids: Vec::new(),
            requested_probes: Vec::new(),
            options: Vec::new(),
            proposed_actions: Vec::new(),
            unresolved_questions: Vec::new(),
        }
    }
}

/// Kind of callback sent from an Agent Team to the Scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamCallbackKind {
    /// Reports progress without changing the final Job state.
    Progress,
    /// Requests a new Snapshot or additional evidence.
    NeedMoreContext,
    /// Requests human input or selection.
    NeedHumanInput,
    /// The Team has prepared multiple candidate options.
    OptionsReady,
    /// The Team completed investigation or implementation.
    Completed,
    /// The Team is blocked by an external condition.
    Blocked,
    /// Team execution failed.
    Failed,
}

/// A callback returned by an Agent Team to the Top Scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCallback {
    /// Callback ID。
    pub callback_id: Uuid,
    /// Related Issue ID.
    pub issue_id: IssueId,
    /// Related Job ID.
    pub job_id: JobId,
    /// Callback kind.
    pub kind: TeamCallbackKind,
    /// Summary for the Scheduler and humans.
    pub summary: String,
    /// Event IDs referenced by the callback.
    pub evidence_ids: Vec<EventId>,
    /// Artifact IDs referenced by the callback.
    pub artifact_ids: Vec<ArtifactId>,
    /// Final result, present only when the callback ends the current stage.
    pub final_result: Option<JobResult>,
    /// Time at which the callback was created.
    pub created_at: DateTime<Utc>,
}

impl TeamCallback {
    /// Creates a Team callback without a final result yet.
    ///
    /// A progress callback can use this value directly. A callback that ends the current Job stage
    /// should set `final_result` before sending so the Scheduler can call `Job::complete`.
    pub fn new(
        issue_id: IssueId,
        job_id: JobId,
        kind: TeamCallbackKind,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            callback_id: Uuid::now_v7(),
            issue_id,
            job_id,
            kind,
            summary: summary.into(),
            evidence_ids: Vec::new(),
            artifact_ids: Vec::new(),
            final_result: None,
            created_at: Utc::now(),
        }
    }
}

/// One unit of work performed by an Agent Team against a fixed Snapshot View.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Job ID。
    pub job_id: JobId,
    /// ID of the owning Issue.
    pub issue_id: IssueId,
    /// Snapshot View actually presented to this Job.
    pub snapshot_view: SnapshotViewRef,
    /// ID of the older Job replaced by this Job.
    pub supersedes_job_id: Option<JobId>,
    /// Kind of Team executing the Job.
    pub team_kind: TeamKind,
    /// Job lifecycle state.
    pub status: JobStatus,
    /// Work instructions dispatched by the Scheduler.
    pub work_order: WorkOrder,
    /// Agents Platform capabilities the Team may request.
    pub allowed_capabilities: Vec<String>,
    /// Target resources the Team may read or operate.
    pub allowed_target_ids: Vec<ResourceId>,
    /// Final result returned by the Team.
    pub result: Option<JobResult>,
    /// Time at which the Job was created.
    pub created_at: DateTime<Utc>,
    /// Time at which Job execution started.
    pub started_at: Option<DateTime<Utc>>,
    /// Time at which the Job completed, failed, was cancelled, or was superseded.
    pub completed_at: Option<DateTime<Utc>>,
}

impl Job {
    /// Creates a queued Job permanently bound to the given Snapshot View.
    ///
    /// This function does not decide whether the Issue permits this Team kind or inspect Artifact
    /// content. The Scheduler and Snapshot View Builder perform those validations before calling it.
    pub fn new(
        issue_id: IssueId,
        snapshot_view: SnapshotViewRef,
        team_kind: TeamKind,
        work_order: WorkOrder,
        allowed_capabilities: Vec<String>,
        allowed_target_ids: Vec<ResourceId>,
    ) -> Self {
        Self {
            job_id: Uuid::now_v7(),
            issue_id,
            snapshot_view,
            supersedes_job_id: None,
            team_kind,
            status: JobStatus::Queued,
            work_order,
            allowed_capabilities,
            allowed_target_ids,
            result: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        }
    }

    /// Returns the canonical Snapshot ID for this Job.
    ///
    /// This value comes from an irreplaceable Snapshot View reference, so Job exposes no method that
    /// changes its Snapshot.
    pub fn base_snapshot_id(&self) -> crate::domain::SnapshotId {
        self.snapshot_view.snapshot_id
    }

    /// Transitions a Job according to the initial minimal state machine.
    ///
    /// This function maintains start and completion times but does not inspect Scheduler mode, Team
    /// identity, or permissions. The scheduling layer supplies those policies.
    pub fn transition_to(&mut self, next: JobStatus) -> AgentResult<()> {
        if !is_valid_transition(self.status, next) {
            return Err(AgentError::InvalidTransition {
                entity: "Job",
                from: format!("{:?}", self.status),
                to: format!("{next:?}"),
            });
        }

        if next == JobStatus::Running && self.started_at.is_none() {
            self.started_at = Some(Utc::now());
        }
        if next.is_terminal() {
            self.completed_at = Some(Utc::now());
        }
        self.status = next;
        Ok(())
    }

    /// Records the Team's final result and derives the current-stage state from it.
    ///
    /// `NeedsMoreData` moves to `NeedsResnapshot`; results requiring a human or option selection move
    /// to `WaitingForHuman`; failure moves to `Failed`; other results move to `Completed`.
    pub fn complete(&mut self, result: JobResult) -> AgentResult<()> {
        let next = match result.outcome {
            JobOutcome::NeedsMoreData => JobStatus::NeedsResnapshot,
            JobOutcome::NeedsHuman | JobOutcome::OptionsReady => JobStatus::WaitingForHuman,
            JobOutcome::Blocked => JobStatus::Blocked,
            JobOutcome::Failed => JobStatus::Failed,
            JobOutcome::Solved | JobOutcome::DiagnosisOnly => JobStatus::Completed,
        };

        self.transition_to(next)?;
        self.result = Some(result);
        Ok(())
    }

    /// Creates a new Job that replaces this Job with a new Snapshot View.
    ///
    /// The current Job must not be terminal. This method first marks the old Job as `Superseded`,
    /// then copies its Team, capability, and target scope. The Scheduler supplies the new Work Order.
    pub fn supersede_with(
        &mut self,
        new_snapshot_view: SnapshotViewRef,
        new_work_order: WorkOrder,
    ) -> AgentResult<Self> {
        let previous_job_id = self.job_id;
        self.transition_to(JobStatus::Superseded)?;

        let mut next = Self::new(
            self.issue_id,
            new_snapshot_view,
            self.team_kind,
            new_work_order,
            self.allowed_capabilities.clone(),
            self.allowed_target_ids.clone(),
        );
        next.supersedes_job_id = Some(previous_job_id);
        Ok(next)
    }
}

/// Returns whether a Job transition follows the initial flow.
///
/// This function only rejects transitions that would break Snapshot binding or recovery semantics.
/// Finer business rules will be decided later by Scheduler policy.
fn is_valid_transition(current: JobStatus, next: JobStatus) -> bool {
    use JobStatus::{
        Blocked, Cancelled, Completed, Failed, NeedsResnapshot, Queued, Running, Superseded,
        WaitingForHuman,
    };

    matches!(
        (current, next),
        (Queued, Running | Cancelled | Superseded)
            | (
                Running,
                WaitingForHuman
                    | NeedsResnapshot
                    | Completed
                    | Failed
                    | Blocked
                    | Cancelled
                    | Superseded
            )
            | (
                WaitingForHuman,
                Running | Completed | Failed | Cancelled | Superseded
            )
            | (NeedsResnapshot, Superseded | Cancelled)
            | (Blocked, Running | Failed | Cancelled | Superseded)
    )
}
