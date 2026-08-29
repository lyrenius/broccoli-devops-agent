//! ActionRun state machine and Platform results for real side effects.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::job::{ActionProposal, TeamKind};
use crate::domain::{
    ActionRunId, ArtifactId, IssueId, JobId, NamedValue, PlatformOperationId, ResourceId,
    SnapshotId,
};
use crate::error::{AgentError, AgentResult};

/// Current approval state of an ActionRun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// The current operation mode and policy do not require human approval.
    NotRequired,
    /// Human approval is pending.
    Pending,
    /// A human has approved the action.
    Approved,
    /// A human has rejected the action.
    Rejected,
}

/// Lifecycle state of an ActionRun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// An Agent Team proposed the action, but policy has not evaluated it yet.
    Proposed,
    /// Policy requires human approval.
    WaitingForApproval,
    /// Preconditions and approval allow the action to enter the execution queue.
    Ready,
    /// The Agents Platform is executing the action.
    Running,
    /// The Platform reported success and a new Snapshot is being used to verify the effect.
    Verifying,
    /// Both execution and verification succeeded.
    Succeeded,
    /// Action execution failed.
    Failed,
    /// Execution succeeded but did not produce the expected effect.
    VerificationFailed,
    /// The action was cancelled before or during execution.
    Cancelled,
}

impl ActionStatus {
    /// Returns whether this state ends the ActionRun lifecycle.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::VerificationFailed | Self::Cancelled
        )
    }
}

/// Minimal result returned after the Agents Platform executes an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformOperationResult {
    /// ID assigned by the Platform to the underlying operation.
    pub operation_id: PlatformOperationId,
    /// Whether the underlying execution succeeded.
    pub succeeded: bool,
    /// Artifact ID containing complete stdout, stderr, or diagnostic output.
    pub output_artifact_id: Option<ArtifactId>,
    /// Execution summary suitable for display and logging.
    pub summary: String,
}

impl PlatformOperationResult {
    /// Creates a Platform operation result.
    ///
    /// This function does not interpret a successful command exit code as problem resolution. The
    /// Scheduler still needs an after Snapshot and must invoke ActionRun verification.
    pub fn new(
        succeeded: bool,
        output_artifact_id: Option<ArtifactId>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            operation_id: Uuid::now_v7(),
            succeeded,
            output_artifact_id,
            summary: summary.into(),
        }
    }
}

/// A recoverable and auditable system side effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRun {
    /// ActionRun ID。
    pub action_run_id: ActionRunId,
    /// ID of the owning Issue.
    pub issue_id: IssueId,
    /// ID of the Job that proposed the action.
    pub originating_job_id: JobId,
    /// Kind of Team that requested the action.
    pub requested_by_team: TeamKind,
    /// Name of the component that executes the action; fixed to `agents-platform` initially.
    pub executed_by: String,
    /// ID of the Runbook registered in the Agents Platform.
    pub runbook_id: String,
    /// Resources targeted by the action.
    pub target_ids: Vec<ResourceId>,
    /// Structured arguments passed to the Runbook.
    pub arguments: Vec<NamedValue>,
    /// Current lifecycle state.
    pub status: ActionStatus,
    /// Current approval state.
    pub approval: ApprovalState,
    /// Snapshot ID recorded before execution.
    pub before_snapshot_id: SnapshotId,
    /// Snapshot ID used for verification after execution.
    pub after_snapshot_id: Option<SnapshotId>,
    /// Idempotency key that prevents retries from repeating the side effect.
    pub idempotency_key: String,
    /// Underlying operation ID returned by the Agents Platform.
    pub platform_operation_id: Option<PlatformOperationId>,
    /// Artifact ID containing complete Agents Platform output.
    pub execution_artifact_id: Option<ArtifactId>,
    /// Probe IDs to run after the operation.
    pub verification_probe_ids: Vec<String>,
    /// Verification result summary.
    pub verification_summary: Option<String>,
    /// Time at which the ActionRun was created.
    pub created_at: DateTime<Utc>,
    /// Time at which execution actually started.
    pub started_at: Option<DateTime<Utc>>,
    /// Time at which the ActionRun entered a terminal state.
    pub completed_at: Option<DateTime<Utc>>,
}

impl ActionRun {
    /// Converts an Agent Team ActionProposal into an unexecuted ActionRun.
    ///
    /// This function only copies the structured proposal and records the before Snapshot. Scheduler
    /// policy computes approval requirements and writes them through `apply_approval`; a model cannot
    /// decide whether approval is required.
    pub fn from_proposal(
        issue_id: IssueId,
        originating_job_id: JobId,
        requested_by_team: TeamKind,
        proposal: ActionProposal,
        before_snapshot_id: SnapshotId,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            action_run_id: Uuid::now_v7(),
            issue_id,
            originating_job_id,
            requested_by_team,
            executed_by: "agents-platform".to_string(),
            runbook_id: proposal.runbook_id,
            target_ids: proposal.target_ids,
            arguments: proposal.arguments,
            status: ActionStatus::Proposed,
            approval: ApprovalState::Pending,
            before_snapshot_id,
            after_snapshot_id: None,
            idempotency_key: idempotency_key.into(),
            platform_operation_id: None,
            execution_artifact_id: None,
            verification_probe_ids: proposal.verification_probe_ids,
            verification_summary: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        }
    }

    /// Applies an approval result produced by Scheduler policy or a human.
    ///
    /// `NotRequired` and `Approved` move the action to `Ready`; `Pending` waits; `Rejected`
    /// cancels it. The caller will later record the approver and policy rationale.
    pub fn apply_approval(&mut self, approval: ApprovalState) -> AgentResult<()> {
        let next = match approval {
            ApprovalState::NotRequired | ApprovalState::Approved => ActionStatus::Ready,
            ApprovalState::Pending => ActionStatus::WaitingForApproval,
            ApprovalState::Rejected => ActionStatus::Cancelled,
        };
        self.transition_to(next)?;
        self.approval = approval;
        Ok(())
    }

    /// Marks an action with satisfied approval and preconditions as running.
    ///
    /// This function only advances domain state; actual execution belongs to the Agents Platform.
    pub fn start(&mut self) -> AgentResult<()> {
        self.transition_to(ActionStatus::Running)
    }

    /// Records the execution result returned by the Agents Platform.
    ///
    /// Platform success only moves to `Verifying`, never directly to `Succeeded`; failure moves to
    /// the terminal `Failed` state. Complete output is retained through an Artifact reference.
    pub fn record_execution_result(&mut self, result: PlatformOperationResult) -> AgentResult<()> {
        if self.status != ActionStatus::Running {
            return Err(AgentError::InvalidTransition {
                entity: "ActionRun",
                from: format!("{:?}", self.status),
                to: "PlatformOperationResult".to_string(),
            });
        }

        self.platform_operation_id = Some(result.operation_id);
        self.execution_artifact_id = result.output_artifact_id;
        self.transition_to(if result.succeeded {
            ActionStatus::Verifying
        } else {
            ActionStatus::Failed
        })
    }

    /// Records the after Snapshot and effect-verification conclusion.
    ///
    /// A true `passed` value moves to `Succeeded`; otherwise it moves to `VerificationFailed`.
    /// Initially the caller supplies the boolean result; a later verifier will compute it from
    /// `verification_probe_ids`.
    pub fn record_verification(
        &mut self,
        after_snapshot_id: SnapshotId,
        passed: bool,
        summary: impl Into<String>,
    ) -> AgentResult<()> {
        if self.status != ActionStatus::Verifying {
            return Err(AgentError::InvalidTransition {
                entity: "ActionRun",
                from: format!("{:?}", self.status),
                to: "VerificationResult".to_string(),
            });
        }

        self.after_snapshot_id = Some(after_snapshot_id);
        self.verification_summary = Some(summary.into());
        self.transition_to(if passed {
            ActionStatus::Succeeded
        } else {
            ActionStatus::VerificationFailed
        })
    }

    /// Transitions an ActionRun according to the initial minimal state machine.
    ///
    /// This method updates timestamps and rejects transitions that skip approval, execution, or
    /// verification. Finer contest policy belongs to the Scheduler and Agents Platform, not the
    /// domain object.
    pub fn transition_to(&mut self, next: ActionStatus) -> AgentResult<()> {
        if !is_valid_transition(self.status, next) {
            return Err(AgentError::InvalidTransition {
                entity: "ActionRun",
                from: format!("{:?}", self.status),
                to: format!("{next:?}"),
            });
        }

        if next == ActionStatus::Running && self.started_at.is_none() {
            self.started_at = Some(Utc::now());
        }
        if next.is_terminal() {
            self.completed_at = Some(Utc::now());
        }
        self.status = next;
        Ok(())
    }
}

/// Returns whether an ActionRun transition follows the initial approval, execution, and verification order.
fn is_valid_transition(current: ActionStatus, next: ActionStatus) -> bool {
    use ActionStatus::{
        Cancelled, Failed, Proposed, Ready, Running, Succeeded, VerificationFailed, Verifying,
        WaitingForApproval,
    };

    matches!(
        (current, next),
        (Proposed, WaitingForApproval | Ready | Cancelled)
            | (WaitingForApproval, Ready | Cancelled)
            | (Ready, Running | Cancelled)
            | (Running, Verifying | Failed | Cancelled)
            | (Verifying, Succeeded | VerificationFailed | Failed)
    )
}
