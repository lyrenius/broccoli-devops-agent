//! Snapshots and their internal resource state.
//!
//! A Snapshot is the immutable factual baseline for Agent reasoning. The Collector can assemble
//! fields before persistence, but after insertion into `StateStore`, callers express new state by
//! creating a child Snapshot rather than mutating it in place.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{
    ArtifactId, DeploymentId, EventId, HealthState, OperationMode, ResourceId, Severity, SnapshotId,
};

/// Reason the current Snapshot was generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCause {
    /// Snapshot produced by periodic collection.
    Periodic,
    /// Snapshot produced when the Snapshot Judge requests an evaluation.
    JudgeEvaluation,
    /// Snapshot captured immediately after a human reports a problem.
    HumanReport,
    /// Snapshot captured when a human sends a denied or failed item back upstream for another pass.
    HumanFeedback,
    /// Snapshot produced after an Agent Team requests additional evidence.
    AgentProbeRequest,
    /// Snapshot recorded before executing an ActionRun.
    BeforeAction,
    /// Snapshot used for verification after executing an ActionRun.
    AfterAction,
    /// Snapshot manually triggered by an operator.
    Manual,
}

/// Kind of resource represented in a Snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// PostgreSQL database service.
    ///
    /// The serde alias lets topology files use the natural spelling while the canonical
    /// serialization in Snapshots and events stays unchanged.
    #[serde(alias = "postgresql")]
    PostgreSql,
    /// Redis message queue and state storage.
    Redis,
    /// SeaweedFS or another compatible object store.
    ObjectStorage,
    /// Broccoli HTTP/API service.
    BroccoliServer,
    /// Contestant-facing Web frontend.
    Frontend,
    /// Optional gateway or load balancer in front of the Server.
    Gateway,
    /// Worker that compiles, runs, and checks submissions.
    Worker,
    /// Station connected to printers that retrieves print jobs.
    PrinterStation,
    /// Station that handles balloon tasks.
    BalloonStation,
    /// A concrete printer connected to a Station.
    Printer,
    /// Representative access probe in the contestant network.
    NetworkVantage,
    /// Broccoli DevOps Agent's own control plane.
    AgentControlPlane,
}

/// Numeric metric in a Snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Stable metric name, such as `worker.in_flight`.
    pub name: String,
    /// Metric value.
    pub value: f64,
    /// Explicit unit, such as `tasks`, `bytes`, or `seconds`.
    pub unit: String,
    /// Aggregation window in seconds; instantaneous values use zero.
    pub window_secs: u64,
}

impl Metric {
    /// Creates a metric with an explicit unit and window.
    ///
    /// This constructor does not validate metric naming conventions. A future Collector metric
    /// catalog will constrain names and units consistently.
    pub fn new(
        name: impl Into<String>,
        value: f64,
        unit: impl Into<String>,
        window_secs: u64,
    ) -> Self {
        Self {
            name: name.into(),
            value,
            unit: unit.into(),
            window_secs,
        }
    }
}

/// Observed state of one resource in a Snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceState {
    /// Stable resource ID.
    pub resource_id: ResourceId,
    /// ID of the hosting machine, or `None` for resources not tied to one machine.
    pub node_id: Option<ResourceId>,
    /// Resource kind.
    pub kind: ResourceKind,
    /// Health state derived from current evidence.
    pub health: HealthState,
    /// Time at which this state was actually observed.
    pub observed_at: DateTime<Utc>,
    /// Structured non-numeric facts.
    pub facts: Vec<crate::domain::NamedValue>,
    /// Numeric metrics.
    pub metrics: Vec<Metric>,
    /// Event IDs supporting this state.
    pub evidence_ids: Vec<EventId>,
}

impl ResourceState {
    /// Creates a minimal resource state with no facts or metrics initially.
    ///
    /// The Collector can add facts, metrics, and evidence before persisting the Snapshot. A future
    /// Collector implementation will validate consistency between `Unknown` and missing evidence.
    pub fn new(
        resource_id: impl Into<ResourceId>,
        node_id: Option<ResourceId>,
        kind: ResourceKind,
        health: HealthState,
        observed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            resource_id: resource_id.into(),
            node_id,
            kind,
            health,
            observed_at,
            facts: Vec::new(),
            metrics: Vec::new(),
            evidence_ids: Vec::new(),
        }
    }
}

/// Directed dependency between two resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    /// ID of the dependent resource.
    pub from_resource_id: ResourceId,
    /// ID of the resource being depended upon.
    pub to_resource_id: ResourceId,
    /// Dependency relationship name, such as `redis_mq` or `s3_blob`.
    pub relation: String,
    /// Whether failure of the dependency blocks a core capability of the dependent resource.
    pub critical: bool,
}

/// Summary of an alert that is active in the Snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    /// Stable ID assigned by the alerting system.
    pub alert_id: String,
    /// Alert severity.
    pub severity: Severity,
    /// Affected resources.
    pub affected_resource_ids: Vec<ResourceId>,
    /// Stable reason code used by rules and deduplication.
    pub reason_code: String,
    /// Short explanation for the Scheduler and humans.
    pub summary: String,
    /// Event IDs supporting the alert.
    pub evidence_ids: Vec<EventId>,
}

/// Recent system change that may affect fault diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemChange {
    /// Change-record ID.
    pub change_id: String,
    /// Time at which the change occurred.
    pub occurred_at: DateTime<Utc>,
    /// Resources affected by the change.
    pub affected_resource_ids: Vec<ResourceId>,
    /// Change kind, such as `config`, `bundle`, or `service_restart`.
    pub kind: String,
    /// Change summary.
    pub summary: String,
    /// Actor that performed the change.
    pub actor: String,
    /// Event IDs supporting the change record.
    pub evidence_ids: Vec<EventId>,
}

/// Observation in a Snapshot that is missing, failed, or expired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    /// ID of the resource without sufficient observation.
    pub resource_id: ResourceId,
    /// ID of the Probe that should have produced data.
    pub probe_id: String,
    /// Reason for the current gap.
    pub reason: String,
    /// Time of the most recent successful observation, or `None` if none ever succeeded.
    pub last_success_at: Option<DateTime<Utc>>,
}

/// Exact revision of code, an image, plugin, WASM module, or Bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionRef {
    /// Resource or build target to which the revision applies.
    pub target_id: ResourceId,
    /// Revision kind, such as `git_commit` or `image_digest`.
    pub revision_kind: String,
    /// Unambiguous revision value.
    pub revision: String,
}

/// Immutable system state used by an Agent for reasoning at a point in time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Snapshot ID.
    pub snapshot_id: SnapshotId,
    /// Parent Snapshot ID expressing supplemental collection or before/after relationships.
    pub parent_snapshot_id: Option<SnapshotId>,
    /// ID of the owning deployment instance.
    pub deployment_id: DeploymentId,
    /// Topology revision used to create the Snapshot.
    pub topology_revision: String,
    /// Time at which the Snapshot was created.
    pub created_at: DateTime<Utc>,
    /// Reason the Snapshot was created.
    pub cause: SnapshotCause,
    /// Operation phase active at creation time.
    pub operation_mode: OperationMode,
    /// State of every observed resource.
    pub resources: Vec<ResourceState>,
    /// Resource dependency relationships.
    pub dependencies: Vec<DependencyEdge>,
    /// Active alerts.
    pub active_alerts: Vec<Alert>,
    /// Recent changes relevant to diagnosis.
    pub recent_changes: Vec<SystemChange>,
    /// Explicitly recorded observation gaps.
    pub coverage_gaps: Vec<CoverageGap>,
    /// Code and deployment artifact revisions.
    pub revisions: Vec<RevisionRef>,
    /// Index of Event IDs supporting the complete Snapshot.
    pub evidence_ids: Vec<EventId>,
}

impl Snapshot {
    /// Creates a Snapshot that does not yet contain resource observations.
    ///
    /// This function generates a UUID v7 and current timestamp and only establishes the Collector's
    /// assembly container. Initially the Collector may fill public fields before insertion into the
    /// Store; after insertion the Snapshot must be treated as immutable.
    pub fn new(
        deployment_id: DeploymentId,
        topology_revision: impl Into<String>,
        cause: SnapshotCause,
        operation_mode: OperationMode,
    ) -> Self {
        Self {
            snapshot_id: Uuid::now_v7(),
            parent_snapshot_id: None,
            deployment_id,
            topology_revision: topology_revision.into(),
            created_at: Utc::now(),
            cause,
            operation_mode,
            resources: Vec::new(),
            dependencies: Vec::new(),
            active_alerts: Vec::new(),
            recent_changes: Vec::new(),
            coverage_gaps: Vec::new(),
            revisions: Vec::new(),
            evidence_ids: Vec::new(),
        }
    }

    /// Marks this Snapshot as a successor to another Snapshot.
    ///
    /// This builder consumes and returns `self` for assembly and never modifies the persisted parent.
    pub fn with_parent(mut self, parent_snapshot_id: SnapshotId) -> Self {
        self.parent_snapshot_id = Some(parent_snapshot_id);
        self
    }
}

/// Reference to the sanitized Snapshot View actually visible to a Job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotViewRef {
    /// Canonical Snapshot ID on which the View is based.
    pub snapshot_id: SnapshotId,
    /// Artifact ID containing the exact View content.
    pub artifact_id: ArtifactId,
    /// Name of the redaction profile used to generate the View.
    pub redaction_profile: String,
    /// SHA-256 of the View content, used to guarantee exact replay.
    pub content_sha256: String,
}

impl SnapshotViewRef {
    /// Creates a replayable Snapshot View reference.
    ///
    /// This function only records references and a hash; it does not read Artifact content. A future
    /// Snapshot View Builder will guarantee consistency among the Artifact, Snapshot, and hash.
    pub fn new(
        snapshot_id: SnapshotId,
        artifact_id: ArtifactId,
        redaction_profile: impl Into<String>,
        content_sha256: impl Into<String>,
    ) -> Self {
        Self {
            snapshot_id,
            artifact_id,
            redaction_profile: redaction_profile.into(),
            content_sha256: content_sha256.into(),
        }
    }
}
