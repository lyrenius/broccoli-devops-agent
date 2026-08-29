//! Interfaces between the core domain and the Collector, model, Agent Team, Platform, and persistence.
//!
//! Every port depends only on domain types. The initial version provides only an in-memory
//! `StateStore`; the other ports deliberately have no placeholder implementations so callers cannot
//! mistake them for available external capabilities.

use async_trait::async_trait;

use crate::domain::{
    ActionRun, ActionRunId, Artifact, ArtifactId, DeploymentId, EventRecord, Issue, IssueCandidate,
    IssueId, Job, JobId, NewEvent, OperationMode, PlatformOperationResult, ResourceId, Snapshot,
    SnapshotCause, SnapshotId, SnapshotViewRef, TeamCallback, TeamKind, WorkOrder,
};
use crate::error::AgentResult;

/// Describes one Snapshot capture request.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotViewBuildRequest {
    /// ID of the Issue served by the View.
    pub issue_id: IssueId,
    /// Kind of Team that will receive the View.
    pub team_kind: TeamKind,
    /// Work Order dispatched with the View.
    pub work_order: WorkOrder,
    /// Platform capabilities the Job may request.
    pub allowed_capabilities: Vec<String>,
    /// Resources the Job may access.
    pub allowed_target_ids: Vec<ResourceId>,
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
#[async_trait]
pub trait SnapshotJudgePort: Send + Sync {
    /// Analyzes the given Snapshot and returns zero or more Issue Candidates.
    ///
    /// A Candidate is only a proposal and cannot directly create an Issue, assign priority, or
    /// execute an action. The Top Scheduler handles deduplication and acceptance.
    async fn inspect_snapshot(&self, snapshot: &Snapshot) -> AgentResult<Vec<IssueCandidate>>;
}

/// Boundary that converts a canonical Snapshot into a Job-visible View.
#[async_trait]
pub trait SnapshotViewBuilderPort: Send + Sync {
    /// Builds a sanitized Snapshot View from the Team, Work Order, and capability scope.
    ///
    /// The result contains both the Artifact and its stable reference. A concrete implementation must
    /// remove credentials and untrusted instructions while retaining dependencies, revisions,
    /// coverage gaps, and other information needed for cross-component reasoning.
    async fn build_snapshot_view(
        &self,
        snapshot: &Snapshot,
        request: &SnapshotViewBuildRequest,
    ) -> AgentResult<SnapshotViewBuildResult>;
}

/// Agent Team boundary that executes a Develop or Operate Job.
#[async_trait]
pub trait AgentTeamPort: Send + Sync {
    /// Returns the Team kind represented by this implementation.
    fn team_kind(&self) -> TeamKind;

    /// Executes one unit of Team work using the Job and exact Snapshot View Artifact.
    ///
    /// The Team can only return results or request more evidence through a callback. It cannot replace
    /// the Job Snapshot itself or bypass the Agents Platform to operate a machine directly.
    async fn run_job(&self, job: &Job, snapshot_view: &Artifact) -> AgentResult<TeamCallback>;
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
}

/// Persistence boundary for domain objects and the append-only EventLog.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Inserts an immutable Snapshot; a duplicate ID must return an error.
    async fn insert_snapshot(&self, snapshot: Snapshot) -> AgentResult<()>;

    /// Reads a Snapshot by ID; returns NotFound when absent.
    async fn get_snapshot(&self, snapshot_id: SnapshotId) -> AgentResult<Snapshot>;

    /// Inserts a new Issue; a duplicate ID must return an error.
    async fn insert_issue(&self, issue: Issue) -> AgentResult<()>;

    /// Updates an existing Issue; returns NotFound when absent.
    async fn update_issue(&self, issue: Issue) -> AgentResult<()>;

    /// Reads an Issue by ID; returns NotFound when absent.
    async fn get_issue(&self, issue_id: IssueId) -> AgentResult<Issue>;

    /// Lists non-terminal Issues that recovery must still consider.
    async fn list_unfinished_issues(&self) -> AgentResult<Vec<Issue>>;

    /// Inserts a new Job; a duplicate ID must return an error.
    async fn insert_job(&self, job: Job) -> AgentResult<()>;

    /// Updates an existing Job; returns NotFound when absent.
    async fn update_job(&self, job: Job) -> AgentResult<()>;

    /// Reads a Job by ID; returns NotFound when absent.
    async fn get_job(&self, job_id: JobId) -> AgentResult<Job>;

    /// Lists non-terminal Jobs that recovery must still consider.
    async fn list_unfinished_jobs(&self) -> AgentResult<Vec<Job>>;

    /// Inserts a new ActionRun; a duplicate ID must return an error.
    async fn insert_action_run(&self, action: ActionRun) -> AgentResult<()>;

    /// Updates an existing ActionRun; returns NotFound when absent.
    async fn update_action_run(&self, action: ActionRun) -> AgentResult<()>;

    /// Reads an ActionRun by ID; returns NotFound when absent.
    async fn get_action_run(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun>;

    /// Lists non-terminal ActionRuns that recovery must still consider.
    async fn list_unfinished_action_runs(&self) -> AgentResult<Vec<ActionRun>>;

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
