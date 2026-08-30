//! Artifact metadata for Snapshot Views, logs, patches, and build outputs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{ActionRunId, ArtifactId, JobId, NamedValue};

/// Kind of content stored by an Artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Sanitized Snapshot View actually presented to a Job.
    SnapshotView,
    /// Raw log fragment retained by the Collector.
    RawLog,
    /// Packaged diagnostic material.
    DiagnosticBundle,
    /// Status report intended for a human operator.
    StatusReport,
    /// Git patch produced by a Develop Team.
    GitPatch,
    /// Build, static-analysis, or test report.
    TestReport,
    /// Installable WASM module.
    Wasm,
    /// Broccoli release Bundle.
    ReleaseBundle,
    /// Configuration patch that the Agents Platform can apply.
    ConfigPatch,
    /// Complete output saved after an Agents Platform operation.
    ActionOutput,
}

/// A large object or output that needs independent addressing.
///
/// The Store retains only metadata and a content location; the initial version does not put file
/// bodies into in-memory objects. A future filesystem or object-storage adapter must verify
/// `content_sha256` before making an Artifact available to the Scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Artifact ID.
    pub artifact_id: ArtifactId,
    /// Artifact content kind.
    pub kind: ArtifactKind,
    /// ID of the Job that produced the Artifact.
    pub produced_by_job_id: Option<JobId>,
    /// ID of the ActionRun that produced the Artifact.
    pub produced_by_action_run_id: Option<ActionRunId>,
    /// Content location; the initial version treats it as an opaque reference.
    pub uri: String,
    /// SHA-256 of the content.
    pub content_sha256: String,
    /// Content size in bytes.
    pub size_bytes: u64,
    /// Additional metadata for querying and display.
    pub metadata: Vec<NamedValue>,
    /// Time at which the Artifact was created.
    pub created_at: DateTime<Utc>,
}

impl Artifact {
    /// Creates Artifact metadata that is not yet bound to a producer.
    ///
    /// This function does not read `uri` or recompute the hash; a real Artifact Store must validate
    /// content when it is integrated.
    pub fn new(
        kind: ArtifactKind,
        uri: impl Into<String>,
        content_sha256: impl Into<String>,
        size_bytes: u64,
    ) -> Self {
        Self {
            artifact_id: Uuid::now_v7(),
            kind,
            produced_by_job_id: None,
            produced_by_action_run_id: None,
            uri: uri.into(),
            content_sha256: content_sha256.into(),
            size_bytes,
            metadata: Vec::new(),
            created_at: Utc::now(),
        }
    }

    /// Marks the Artifact as produced by the specified Job.
    ///
    /// This builder is only for assembly; after persistence, a different producer relationship must
    /// be expressed with a new Artifact.
    pub fn produced_by_job(mut self, job_id: JobId) -> Self {
        self.produced_by_job_id = Some(job_id);
        self
    }

    /// Marks the Artifact as produced by the specified ActionRun.
    ///
    /// This method records only provenance and does not imply that ActionRun verification succeeded.
    pub fn produced_by_action(mut self, action_run_id: ActionRunId) -> Self {
        self.produced_by_action_run_id = Some(action_run_id);
        self
    }
}
