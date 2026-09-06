//! Interfaces between the core domain and the Collector, models, Agent Teams, Platform, Reporter,
//! and persistence.
//!
//! Every port depends only on domain types. The initial version provides only an in-memory
//! `StateStore`; the other ports deliberately have no placeholder implementations so callers cannot
//! mistake them for available external capabilities.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::{
    ActionRun, ActionRunId, Artifact, ArtifactId, DeploymentId, EventRecord, Issue, IssueCandidate,
    IssueId, IssuePriority, Job, JobBrief, JobId, JobResult, NamedValue, NewEvent, OperationMode,
    PlatformOperationResult, ResourceId, Snapshot, SnapshotCause, SnapshotId, SnapshotViewRef,
    TeamCallback, TeamKind,
};
use crate::error::AgentResult;

/// Describes one Snapshot capture request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRequest {
    /// ID of the deployment instance to capture.
    pub deployment_id: DeploymentId,
    /// Current topology revision.
    pub topology_revision: String,
    /// Reason collection was triggered.
    pub cause: SnapshotCause,
    /// Current operation phase.
    pub operation_mode: OperationMode,
    /// Parent of the Snapshot being captured.
    pub parent_snapshot_id: Option<SnapshotId>,
    /// Additional Probe IDs requested for this capture.
    pub requested_probe_ids: Vec<String>,
}

/// Describes the Snapshot View requested by the Scheduler for a Job.
///
/// The View is the Team's whole world: the problem statement comes from the Issue, the scope and
/// any human feedback from the brief, and the evidence from the Snapshot. Nothing else is handed
/// to a Team, so nothing else needs to be replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotViewBuildRequest {
    /// The Issue the Job serves; its title and description are the problem statement.
    pub issue: Issue,
    /// Team kind, scope, and human feedback the Job carries.
    pub brief: JobBrief,
    /// Name of the redaction profile to use.
    pub redaction_profile: String,
}

/// Artifact and reference produced by the Snapshot View Builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotViewBuildResult {
    /// Artifact containing the exact View content.
    pub artifact: Artifact,
    /// Snapshot View reference persisted in the Job.
    pub snapshot_view: SnapshotViewRef,
}

/// Collector boundary that produces canonical Snapshots.
#[async_trait]
pub trait CollectorPort: Send + Sync {
    /// Captures the system as requested and returns an unpersisted immutable Snapshot.
    ///
    /// A concrete implementation runs Probes, normalizes time and units, and records coverage gaps
    /// explicitly. This interface neither persists the Snapshot nor judges anomalies.
    async fn capture_snapshot(&self, request: CaptureRequest) -> AgentResult<Snapshot>;
}

/// Snapshot Judge boundary that proposes potential problems from one Snapshot.
///
/// The Judge is hybrid: deterministic alert rules always run, and a model may additionally
/// correlate evidence. Because model reasoning is involved, the Judge receives a sanitized Judge
/// View built with its own redaction profile rather than the canonical Snapshot, so raw untrusted
/// text never enters model context. The View Artifact is stored, making Judge input replayable
/// exactly like Job input.
#[async_trait]
pub trait SnapshotJudgePort: Send + Sync {
    /// Analyzes the sanitized Judge View of one Snapshot and returns zero or more Issue Candidates.
    ///
    /// `snapshot_id` names the canonical Snapshot the View was built from so candidates reference
    /// it. A Candidate is only a proposal and cannot directly create an Issue, assign priority, or
    /// execute an action. The Top Scheduler handles deduplication and acceptance.
    async fn inspect_snapshot(
        &self,
        snapshot_id: SnapshotId,
        judge_view: &Artifact,
    ) -> AgentResult<Vec<IssueCandidate>>;
}

/// Boundary that converts a canonical Snapshot into a Job-visible View.
#[async_trait]
pub trait SnapshotViewBuilderPort: Send + Sync {
    /// Builds a sanitized Snapshot View for the Issue, Team, scope, and feedback in the request.
    ///
    /// The result contains both the Artifact and its stable reference. A concrete implementation must
    /// remove credentials and fence untrusted text (the human report's own words included) while
    /// retaining dependencies, revisions, coverage gaps, and other information needed for
    /// cross-component reasoning.
    async fn build_snapshot_view(
        &self,
        snapshot: &Snapshot,
        request: &SnapshotViewBuildRequest,
    ) -> AgentResult<SnapshotViewBuildResult>;
}

/// Creates a linked cancellation handle and signal.
///
/// The Scheduler keeps the handle; the signal travels with a running Job so supersession, freezing,
/// or human cancellation can stop Team work cooperatively. Dropping the handle also cancels the
/// signal so an abandoned Job cannot run forever unnoticed.
pub fn cancel_pair() -> (CancelHandle, CancelSignal) {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    (CancelHandle { sender }, CancelSignal { receiver })
}

/// Scheduler-side handle that requests cancellation of one running Job.
#[derive(Debug)]
pub struct CancelHandle {
    sender: tokio::sync::watch::Sender<bool>,
}

impl CancelHandle {
    /// Requests cooperative cancellation.
    ///
    /// Cancellation is a request, not preemption: the Team decides where it can safely stop and
    /// should still deliver a final callback describing what was abandoned.
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }
}

/// Team-side signal observed while executing a Job.
#[derive(Debug, Clone)]
pub struct CancelSignal {
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl CancelSignal {
    /// Returns whether cancellation has been requested.
    ///
    /// A dropped `CancelHandle` counts as cancelled so orphaned work stops rather than running
    /// without an owner.
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow() || self.receiver.has_changed().is_err()
    }

    /// Waits until cancellation is requested.
    ///
    /// Long-running Team steps can race this future against their own work to react promptly.
    pub async fn cancelled(&mut self) {
        loop {
            if *self.receiver.borrow_and_update() {
                return;
            }
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Receiver for the callbacks a Team emits while a Job runs.
///
/// The design requires multiple callbacks per Job (progress, probe requests, then a final result),
/// so the Team pushes into a sink instead of returning one value. The Scheduler side of the sink
/// records every callback in the EventLog before acting on it.
#[async_trait]
pub trait TeamCallbackSink: Send + Sync {
    /// Delivers one callback to the Scheduler.
    ///
    /// Delivery must preserve per-Job order. An error tells the Team the Scheduler no longer
    /// accepts callbacks for this Job (for example after supersession) and it should stop.
    async fn deliver(&self, callback: TeamCallback) -> AgentResult<()>;
}

/// Agent Team boundary that executes a Develop or Operate Job.
#[async_trait]
pub trait AgentTeamPort: Send + Sync {
    /// Returns the Team kind represented by this implementation.
    fn team_kind(&self) -> TeamKind;

    /// Executes one unit of Team work using the Job and exact Snapshot View Artifact.
    ///
    /// The Team reports through the sink: zero or more interim callbacks, then exactly one callback
    /// carrying `final_result`. It must watch `cancel` and stop cooperatively when signalled. It
    /// cannot replace the Job Snapshot itself or bypass the Agents Platform to operate a machine
    /// directly.
    async fn run_job(
        &self,
        job: &Job,
        snapshot_view: &Artifact,
        sink: &dyn TeamCallbackSink,
        cancel: CancelSignal,
    ) -> AgentResult<()>;
}

/// A read-only inspection a running Team asks for directly, within its Job's scope.
///
/// This is the architecture's "read-only scoped request": an Observe-class Runbook (a status
/// query, a log tail, an allowlisted read-only database query) run through the Platform while
/// the Job is still reasoning, with the output handed back to the Team. It is not an ActionRun —
/// nothing changes on the machine — but it is scoped, validated, evented, and its output is an
/// Artifact, exactly like a side effect would be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionRequest {
    /// Runbook ID; must classify as a non-mutating operation class.
    pub runbook_id: String,
    /// Resources to inspect; each must be inside the Job's target scope.
    pub target_ids: Vec<ResourceId>,
    /// Structured arguments for the Runbook.
    pub arguments: Vec<NamedValue>,
    /// Why the Team wants to look.
    pub reason: String,
}

/// What an inspection produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionResult {
    /// Whether every command exited zero (always true for a dry run).
    pub succeeded: bool,
    /// Whether the Platform only rendered the commands.
    pub dry_run: bool,
    /// Why the request was refused before anything ran, when it was.
    pub refused: Option<String>,
    /// The Platform's summary of what happened.
    pub summary: String,
    /// The ActionOutput Artifact holding the complete output, when anything was recorded.
    pub output_artifact_id: Option<ArtifactId>,
}

/// Agents Platform boundary for controlled machine, repository, and build operations.
#[async_trait]
pub trait AgentsPlatformPort: Send + Sync {
    /// Executes an ActionRun that has reached `Ready`.
    ///
    /// A concrete implementation must check capability, target, idempotency key, and timeout and must
    /// write complete output as an Artifact. Platform success does not mean the problem is resolved;
    /// the Scheduler still needs after-Snapshot verification.
    async fn execute_action(&self, action: &ActionRun) -> AgentResult<PlatformOperationResult>;

    /// Runs a read-only inspection on behalf of a running Job.
    ///
    /// A concrete implementation must refuse any Runbook that classifies as a mutating operation
    /// class, re-check target kinds and arguments as it does for an ActionRun, and store the
    /// complete output as an Artifact produced by the Job. A refusal is a failed result with the
    /// reason, never an exception.
    async fn inspect(
        &self,
        job: &Job,
        request: &InspectionRequest,
    ) -> AgentResult<PlatformOperationResult>;
}

/// Gateway through which a running Team asks for a read-only inspection.
///
/// The Scheduler implements this: it checks the freeze mode and the Job's scope, hands the
/// request to the Platform, and records the inspection in the EventLog. No authority-matrix
/// decision and no approval is involved — that is what makes inspections distinct from
/// ActionRuns — but nothing reaches a machine unscoped or unrecorded.
#[async_trait]
pub trait InspectionPort: Send + Sync {
    /// Runs one inspection for the given Job and returns what it produced.
    async fn inspect(
        &self,
        job_id: JobId,
        request: InspectionRequest,
    ) -> AgentResult<InspectionResult>;
}

/// Input for one candidate-triage consultation with the Scheduler Policy model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageRequest {
    /// Candidate under triage.
    pub candidate: IssueCandidate,
    /// Currently open Issues, provided so the model can propose merges instead of duplicates.
    pub open_issues: Vec<Issue>,
}

/// Model-proposed triage decision for one Issue Candidate.
///
/// Every variant is a proposal: the Scheduler harness clamps priorities, verifies referenced
/// Issues, and may still refuse the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum TriageDecision {
    /// Accept the candidate as a new formal Issue at the given priority.
    ///
    /// The harness caps model-proposed priority below `HumanTop`; only humans reach that level.
    Accept {
        /// Proposed scheduling priority.
        priority: IssuePriority,
    },
    /// Attach the candidate's evidence to an existing open Issue instead of opening a new one.
    MergeInto {
        /// ID of the open Issue that already covers this problem.
        issue_id: IssueId,
    },
    /// Discard the candidate as noise or a duplicate not worth tracking.
    Reject,
}

/// Input for interpreting a Job's final result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallbackAdviceRequest {
    /// Issue that owns the Job.
    pub issue: Issue,
    /// Job that finished the current stage.
    pub job: Job,
    /// Final result returned by the Team.
    pub result: JobResult,
}

/// Model-proposed next step after a Job's final result.
///
/// The harness validates each variant before acting: probe IDs must be non-empty and registered,
/// proposal indexes must exist in the `JobResult`, and terminal decisions must satisfy the Issue
/// state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "step")]
pub enum NextStepDecision {
    /// Capture a new Snapshot with the given Probes and dispatch a superseding Job.
    Resnapshot {
        /// Probe IDs to include in the new capture.
        requested_probe_ids: Vec<String>,
    },
    /// Convert the listed proposals from the `JobResult` into ActionRuns.
    CreateActions {
        /// Zero-based indexes into `JobResult::proposed_actions`.
        proposal_indexes: Vec<usize>,
    },
    /// Put the Issue in front of a human with a concrete question.
    AskHuman {
        /// Question the human must answer before automatic work continues.
        question: String,
    },
    /// Consider the Issue resolved.
    Resolve,
    /// Record failure and stop automatic work on the Issue.
    GiveUp,
}

/// A validated-or-rejected next-step proposal together with the model's rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextStep {
    /// Proposed next step.
    pub decision: NextStepDecision,
    /// Model or fallback rationale, recorded for post-contest review.
    pub rationale: String,
}

/// Scheduler Policy boundary: the model side of the AI-integrated Top Scheduler.
///
/// The Scheduler harness owns state machines, permissions, and invariants; this port is consulted
/// only at fixed decision points with typed requests and responses. Every consultation must be
/// recorded in the EventLog with its input and output so decisions can be replayed. When the model
/// is unavailable the Scheduler falls back to conservative deterministic defaults instead of
/// failing, so model downtime never breaks collection, persistence, or recovery.
#[async_trait]
pub trait SchedulerPolicyPort: Send + Sync {
    /// Proposes how to triage one Issue Candidate against the currently open Issues.
    async fn triage_candidate(&self, request: &TriageRequest) -> AgentResult<TriageDecision>;

    /// Proposes the next step after a Job returns its final result.
    async fn advise_next_step(&self, request: &CallbackAdviceRequest) -> AgentResult<NextStep>;
}

/// Reporter boundary that renders human-facing status reports from Snapshots.
///
/// The Reporter consumes Snapshots from the observation path and never dispatches work or mutates
/// machines. The first implementation should render deterministically; model summarization is an
/// optional layer on top.
#[async_trait]
pub trait ReporterPort: Send + Sync {
    /// Renders a status report for one Snapshot and returns the report Artifact.
    async fn render_status_report(&self, snapshot: &Snapshot) -> AgentResult<Artifact>;
}

/// Persistence boundary for domain objects and the append-only EventLog.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Inserts an immutable Snapshot; a duplicate ID must return an error.
    async fn insert_snapshot(&self, snapshot: Snapshot) -> AgentResult<()>;

    /// Reads a Snapshot by ID; returns NotFound when absent.
    async fn get_snapshot(&self, snapshot_id: SnapshotId) -> AgentResult<Snapshot>;

    /// Lists every Snapshot in creation order, for operator views.
    async fn list_snapshots(&self) -> AgentResult<Vec<Snapshot>>;

    /// Inserts a new Issue; a duplicate ID must return an error.
    async fn insert_issue(&self, issue: Issue) -> AgentResult<()>;

    /// Updates an existing Issue; returns NotFound when absent.
    async fn update_issue(&self, issue: Issue) -> AgentResult<()>;

    /// Updates an Issue only if the stored record still equals `expected`; otherwise `Conflict`.
    ///
    /// This is the compare-and-set every Scheduler transition uses: read, decide, write what you
    /// read. Two operators acting on one record cannot both apply.
    async fn update_issue_if(&self, expected: &Issue, next: Issue) -> AgentResult<()>;

    /// Reads an Issue by ID; returns NotFound when absent.
    async fn get_issue(&self, issue_id: IssueId) -> AgentResult<Issue>;

    /// Lists non-terminal Issues that recovery must still consider.
    async fn list_unfinished_issues(&self) -> AgentResult<Vec<Issue>>;

    /// Lists every Issue in creation order, for operator views.
    async fn list_issues(&self) -> AgentResult<Vec<Issue>>;

    /// Inserts a new Job; a duplicate ID must return an error.
    async fn insert_job(&self, job: Job) -> AgentResult<()>;

    /// Updates an existing Job; returns NotFound when absent.
    async fn update_job(&self, job: Job) -> AgentResult<()>;

    /// Updates a Job only if the stored record still equals `expected`; otherwise `Conflict`.
    async fn update_job_if(&self, expected: &Job, next: Job) -> AgentResult<()>;

    /// Reads a Job by ID; returns NotFound when absent.
    async fn get_job(&self, job_id: JobId) -> AgentResult<Job>;

    /// Lists non-terminal Jobs that recovery must still consider.
    async fn list_unfinished_jobs(&self) -> AgentResult<Vec<Job>>;

    /// Lists every Job in creation order, for operator views.
    async fn list_jobs(&self) -> AgentResult<Vec<Job>>;

    /// Inserts a new ActionRun; a duplicate ID must return an error.
    async fn insert_action_run(&self, action: ActionRun) -> AgentResult<()>;

    /// Updates an existing ActionRun; returns NotFound when absent.
    async fn update_action_run(&self, action: ActionRun) -> AgentResult<()>;

    /// Updates an ActionRun only if the stored record still equals `expected`; otherwise
    /// `Conflict`. Approval, start, execution result, verification, and review all go through
    /// this, so a retry or a second operator gets a refusal instead of a duplicate effect.
    async fn update_action_run_if(&self, expected: &ActionRun, next: ActionRun) -> AgentResult<()>;

    /// Claims an idempotency key for an ActionRun.
    ///
    /// Returns `Ok(None)` when the key is free (or held by this ActionRun) and `Ok(Some(holder))`
    /// when another ActionRun with the same key still holds it — it may yet run, is running, or
    /// succeeded. Cancelled and failed runs release their key. The check and the answer happen
    /// under the store's lock, so two proposals of the same intent cannot both be admitted.
    async fn claim_idempotency_key(
        &self,
        key: &str,
        action_run_id: ActionRunId,
    ) -> AgentResult<Option<ActionRunId>>;

    /// Reads an ActionRun by ID; returns NotFound when absent.
    async fn get_action_run(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun>;

    /// Lists non-terminal ActionRuns that recovery must still consider.
    async fn list_unfinished_action_runs(&self) -> AgentResult<Vec<ActionRun>>;

    /// Lists every ActionRun in creation order, for operator views and the repeat-rate rule.
    async fn list_action_runs(&self) -> AgentResult<Vec<ActionRun>>;

    /// Inserts immutable Artifact metadata; a duplicate ID must return an error.
    async fn insert_artifact(&self, artifact: Artifact) -> AgentResult<()>;

    /// Reads Artifact metadata by ID; returns NotFound when absent.
    async fn get_artifact(&self, artifact_id: ArtifactId) -> AgentResult<Artifact>;

    /// Assigns a monotonically increasing sequence and appends a new event to the EventLog.
    async fn append_event(&self, event: NewEvent) -> AgentResult<EventRecord>;

    /// Returns all events in the current Store in sequence order.
    ///
    /// The initial interface supports only full reads. A real database implementation will add time,
    /// Issue, Job, and ActionRun filters without changing append-only semantics.
    async fn list_events(&self) -> AgentResult<Vec<EventRecord>>;
}
