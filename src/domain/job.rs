//! Jobs, callbacks, and final results for Agent Teams.
//!
//! An Issue says what is wrong; a Job is one bounded pass by one Team over one Snapshot View.
//! There is no separate work-order layer: the Team reads the problem statement from its View,
//! and the Job carries only the scope the Scheduler granted plus any human feedback from earlier
//! passes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{
    ActionRunId, ActionStatus, ApprovalState, ArtifactId, EventId, HumanFeedback, HumanReview,
    IssueId, JobId, NamedValue, ResourceId, SnapshotViewRef, VerificationEvidence,
};
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

/// One ActionRun of an earlier pass, as the next pass gets to see it.
///
/// Everything here is control-plane authored except `evidence`, which quotes machine output and
/// is fenced as untrusted data when the View is rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassActionRecord {
    /// The ActionRun.
    pub action_run_id: ActionRunId,
    /// Runbook that was proposed.
    pub runbook_id: String,
    /// Targets it named.
    pub target_ids: Vec<ResourceId>,
    /// Arguments it carried.
    pub arguments: Vec<NamedValue>,
    /// Where the ActionRun ended up (or still is).
    pub status: ActionStatus,
    /// What the authority matrix or a human decided.
    pub approval: ApprovalState,
    /// Whether the Platform only rendered the commands.
    pub dry_run: bool,
    /// The denial's reason, when it was denied.
    pub denial_reason: Option<String>,
    /// The Platform's own account of the execution.
    pub execution_summary: Option<String>,
    /// The Scheduler's verification conclusion.
    pub verification_summary: Option<String>,
    /// How much the verification proved.
    pub verification_evidence: Option<VerificationEvidence>,
    /// Sanitized execution output (exit codes, the tail of stderr and stdout). Machine text.
    pub evidence: Option<String>,
}

/// One earlier pass on the same Issue: what the Team concluded and what came of it.
///
/// Copied into every later pass so a Team sees the whole investigation so far — what was tried,
/// what was refused, what ran and whether it worked — instead of rediscovering it. The Team's own
/// words (`summary`, `unresolved_questions`) are model output and are fenced as untrusted data
/// when rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassRecord {
    /// The earlier Job.
    pub job_id: JobId,
    /// The Job it superseded, if any.
    pub supersedes_job_id: Option<JobId>,
    /// The Job it revised on human feedback, if any.
    pub revises_job_id: Option<JobId>,
    /// The completed pass whose actions it re-observed, if any.
    pub continues_job_id: Option<JobId>,
    /// How the pass ended, when it has.
    pub outcome: Option<JobOutcome>,
    /// The Team's summary.
    pub summary: String,
    /// Questions the Team left open.
    pub unresolved_questions: Vec<String>,
    /// Observations the Team asked for.
    pub requested_probes: Vec<ProbeRequest>,
    /// The ActionRuns its proposals became.
    pub actions: Vec<PassActionRecord>,
    /// When the pass was created.
    pub created_at: DateTime<Utc>,
}

/// What the Scheduler grants a Job: which Team, what it may touch, what humans said about
/// earlier passes on the same Issue, what those passes did, and how many more automatic passes
/// the Scheduler is still willing to run.
///
/// This is a constructor argument, not a layer of its own: every field lands flat on the Job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBrief {
    /// Kind of Team that will run the Job.
    pub team_kind: TeamKind,
    /// Agents Platform capabilities the Team may request.
    pub allowed_capabilities: Vec<String>,
    /// Target resources the Team may read or operate.
    pub allowed_target_ids: Vec<ResourceId>,
    /// Human feedback from earlier passes, oldest first.
    pub feedback: Vec<HumanFeedback>,
    /// The Job whose outcome a human sent back upstream, when this is a revision.
    pub revises_job_id: Option<JobId>,
    /// The completed pass whose actions this pass re-observes, when this is a follow-up.
    pub continues_job_id: Option<JobId>,
    /// Earlier passes on the Issue, oldest first.
    pub earlier_passes: Vec<PassRecord>,
    /// How many further automatic passes the Scheduler will grant after this one. Zero means
    /// this is the last automatic pass: the Team must conclude with what it has, and only a
    /// human can start another.
    pub follow_up_budget: u32,
}

impl JobBrief {
    /// A first-pass brief with no feedback, no history, and no further automatic passes.
    pub fn new(
        team_kind: TeamKind,
        allowed_capabilities: Vec<String>,
        allowed_target_ids: Vec<ResourceId>,
    ) -> Self {
        Self {
            team_kind,
            allowed_capabilities,
            allowed_target_ids,
            feedback: Vec::new(),
            revises_job_id: None,
            continues_job_id: None,
            earlier_passes: Vec::new(),
            follow_up_budget: 0,
        }
    }

    /// Turns the brief into a revision of an earlier Job, carrying the accumulated feedback.
    pub fn revising(mut self, revises_job_id: JobId, feedback: Vec<HumanFeedback>) -> Self {
        self.revises_job_id = Some(revises_job_id);
        self.feedback = feedback;
        self
    }

    /// Turns the brief into a follow-up of a completed pass whose actions have run.
    pub fn continuing(mut self, continues_job_id: JobId) -> Self {
        self.continues_job_id = Some(continues_job_id);
        self
    }

    /// Attaches the Issue's pass history and the remaining automatic-pass budget.
    pub fn with_history(mut self, earlier_passes: Vec<PassRecord>, follow_up_budget: u32) -> Self {
        self.earlier_passes = earlier_passes;
        self.follow_up_budget = follow_up_budget;
        self
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
    /// Whether the Team wants another pass over a fresh Snapshot once its proposals have run,
    /// to check their effect and decide what comes next. Honoured only within the automatic
    /// pass budget, and only once every proposal has been executed or denied.
    #[serde(default)]
    pub follow_up_requested: bool,
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
            follow_up_requested: false,
        }
    }
}

/// Kind of callback sent from an Agent Team to the Scheduler.
///
/// The kind is derived from the callback content by `TeamCallback::kind` rather than stored, so a
/// Team cannot send a label that contradicts its own result.
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
    /// Callback ID.
    pub callback_id: Uuid,
    /// Related Issue ID.
    pub issue_id: IssueId,
    /// Related Job ID.
    pub job_id: JobId,
    /// Summary for the Scheduler and humans.
    pub summary: String,
    /// Event IDs referenced by the callback.
    pub evidence_ids: Vec<EventId>,
    /// Artifact IDs referenced by the callback.
    pub artifact_ids: Vec<ArtifactId>,
    /// Final result, present only when the callback ends the current stage.
    pub final_result: Option<JobResult>,
    /// What the pass spent, present on the callback that ends a model-backed pass.
    #[serde(default)]
    pub usage: Option<ModelUsage>,
    /// Time at which the callback was created.
    pub created_at: DateTime<Utc>,
}

impl TeamCallback {
    /// Creates a Team callback without a final result yet.
    ///
    /// A progress callback can use this value directly. A callback that ends the current Job stage
    /// should set `final_result` before sending so the Scheduler can call `Job::complete`.
    pub fn new(issue_id: IssueId, job_id: JobId, summary: impl Into<String>) -> Self {
        Self {
            callback_id: Uuid::now_v7(),
            issue_id,
            job_id,
            summary: summary.into(),
            evidence_ids: Vec::new(),
            artifact_ids: Vec::new(),
            final_result: None,
            usage: None,
            created_at: Utc::now(),
        }
    }

    /// Sets the final result that ends the current Job stage.
    ///
    /// This builder consumes and returns `self` so Teams can assemble a callback fluently.
    pub fn with_final_result(mut self, result: JobResult) -> Self {
        self.final_result = Some(result);
        self
    }

    /// Reports what the pass spent. A deterministic Team spends nothing and never calls this.
    pub fn with_usage(mut self, usage: ModelUsage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Derives the callback kind from its content.
    ///
    /// A callback with no final result reports progress; otherwise the kind follows the result's
    /// outcome. Deriving instead of storing prevents a Team from labelling a failure as `Completed`
    /// or vice versa.
    pub fn kind(&self) -> TeamCallbackKind {
        match &self.final_result {
            None => TeamCallbackKind::Progress,
            Some(result) => match result.outcome {
                JobOutcome::NeedsMoreData => TeamCallbackKind::NeedMoreContext,
                JobOutcome::NeedsHuman => TeamCallbackKind::NeedHumanInput,
                JobOutcome::OptionsReady => TeamCallbackKind::OptionsReady,
                JobOutcome::Blocked => TeamCallbackKind::Blocked,
                JobOutcome::Failed => TeamCallbackKind::Failed,
                JobOutcome::Solved | JobOutcome::DiagnosisOnly => TeamCallbackKind::Completed,
            },
        }
    }
}

/// What one model-backed pass spent, as the backend reported it.
///
/// Token counts are facts the backend states and the control plane records; money is not stored
/// here. Prices change, are configured per deployment, and are not part of what happened — so a
/// cost is computed from these counts and the configured price list whenever one is displayed
/// ([`crate::usage`]), and the record itself stays true no matter what the price list says later.
///
/// `requests_without_usage` counts responses that carried no usage block at all. It exists so a
/// relay that reports nothing cannot masquerade as a free pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Model that was billed.
    pub model: String,
    /// Prompt tokens, cached ones included.
    pub input_tokens: u64,
    /// Portion of `input_tokens` served from the backend's prompt cache.
    pub cached_input_tokens: u64,
    /// Tokens the model generated.
    pub output_tokens: u64,
    /// Model requests the pass made.
    pub requests: u32,
    /// Requests whose response reported no usage, so their tokens are unknown.
    pub requests_without_usage: u32,
}

impl ModelUsage {
    /// Input plus output tokens.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Whether every request in this record reported its usage.
    pub fn is_complete(&self) -> bool {
        self.requests_without_usage == 0
    }

    /// Adds another record's counts into this one; the model name of the first is kept.
    pub fn absorb(&mut self, other: &Self) {
        if self.model.is_empty() {
            self.model = other.model.clone();
        }
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.requests_without_usage = self
            .requests_without_usage
            .saturating_add(other.requests_without_usage);
    }
}

/// One unit of work performed by an Agent Team against a fixed Snapshot View.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Job ID.
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
    /// Agents Platform capabilities the Team may request.
    pub allowed_capabilities: Vec<String>,
    /// Target resources the Team may read or operate.
    pub allowed_target_ids: Vec<ResourceId>,
    /// Human feedback from earlier passes on this Issue, oldest first.
    ///
    /// Copied into every revising Job so the Team sees the whole exchange, and rendered into the
    /// Snapshot View so it is part of the replayable input.
    #[serde(default)]
    pub feedback: Vec<HumanFeedback>,
    /// The Job a human sent back upstream, when this Job is the resulting revision.
    #[serde(default)]
    pub revises_job_id: Option<JobId>,
    /// The completed pass whose actions this Job re-observes, when this Job is a follow-up.
    #[serde(default)]
    pub continues_job_id: Option<JobId>,
    /// Earlier passes on this Issue, oldest first, rendered into the View so the Team sees the
    /// whole investigation and the replayable input is complete on its own.
    #[serde(default)]
    pub earlier_passes: Vec<PassRecord>,
    /// Further automatic passes the Scheduler will grant after this one; zero means the Team
    /// must conclude here.
    #[serde(default)]
    pub follow_up_budget: u32,
    /// A human's review of this Job, once given. A Job that failed, or that needed more data
    /// when no automatic pass was left, waits in the Failed inbox until it has one.
    #[serde(default)]
    pub review: Option<HumanReview>,
    /// Final result returned by the Team.
    pub result: Option<JobResult>,
    /// What this pass spent at the model relay, once it has finished. Absent on a Job run by a
    /// deterministic Team, and on one that never reached its final callback.
    #[serde(default)]
    pub usage: Option<ModelUsage>,
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
    pub fn new(issue_id: IssueId, snapshot_view: SnapshotViewRef, brief: JobBrief) -> Self {
        Self {
            job_id: Uuid::now_v7(),
            issue_id,
            snapshot_view,
            supersedes_job_id: None,
            team_kind: brief.team_kind,
            status: JobStatus::Queued,
            allowed_capabilities: brief.allowed_capabilities,
            allowed_target_ids: brief.allowed_target_ids,
            feedback: brief.feedback,
            revises_job_id: brief.revises_job_id,
            continues_job_id: brief.continues_job_id,
            earlier_passes: brief.earlier_passes,
            follow_up_budget: brief.follow_up_budget,
            review: None,
            result: None,
            usage: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        }
    }

    /// Records a human's review of a Job in the Failed inbox.
    ///
    /// Only a Job that failed or stalled needing more data is reviewable, and only once: the
    /// review is what takes the Job out of the inbox, so a second review would have nothing to
    /// act on.
    pub fn record_review(&mut self, review: HumanReview) -> AgentResult<()> {
        if !matches!(self.status, JobStatus::Failed | JobStatus::NeedsResnapshot) {
            return Err(AgentError::InvalidInput(format!(
                "Job `{}` is `{:?}`, not `Failed` or `NeedsResnapshot`; only those are reviewed",
                self.job_id, self.status
            )));
        }
        if self.review.is_some() {
            return Err(AgentError::InvalidInput(format!(
                "Job `{}` has already been reviewed",
                self.job_id
            )));
        }
        self.review = Some(review);
        Ok(())
    }

    /// Whether this Job sits in the Failed inbox: it failed, or it needs more data and no
    /// automatic pass was left to fetch it, and nobody has reviewed it yet.
    pub fn needs_review(&self) -> bool {
        matches!(self.status, JobStatus::Failed | JobStatus::NeedsResnapshot)
            && self.review.is_none()
    }

    /// One-based number of this pass on its Issue.
    pub fn pass_number(&self) -> usize {
        self.earlier_passes.len() + 1
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
    /// The current Job must not be terminal. This method first marks the old Job as `Superseded`
    /// and copies nothing implicitly: the Scheduler supplies the whole brief again, because a new
    /// Snapshot may justify a narrower scope than the old Job held.
    pub fn supersede_with(
        &mut self,
        new_snapshot_view: SnapshotViewRef,
        brief: JobBrief,
    ) -> AgentResult<Self> {
        let previous_job_id = self.job_id;
        self.transition_to(JobStatus::Superseded)?;

        let mut next = Self::new(self.issue_id, new_snapshot_view, brief);
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
