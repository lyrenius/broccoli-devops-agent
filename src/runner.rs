//! Wiring and orchestration for the v0.1 vertical slice.
//!
//! The runner assembles the file store, topology Collector, redacting View Builder, Scheduler,
//! and one Operate Team backend, then drives the three slice flows: capture-and-display, human
//! report to completed Job, and restart recovery. The Team backend is chosen at wiring time —
//! deterministic, or the model-backed harness Team over the configured relay — and nothing else
//! in the runner changes between them. No Scheduler Policy model is wired yet, so every Scheduler
//! decision point still exercises its conservative deterministic fallback — by design.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use broccoli_agent_harness::{AgentConfig as HarnessBudget, ModelClient};

use crate::collector::TopologyCollector;
use crate::domain::{
    HumanReport, Issue, Job, JobStatus, Snapshot, SnapshotCause, TeamCallback, TeamKind,
};
use crate::error::AgentResult;
use crate::ports::{
    AgentTeamPort, CaptureRequest, SnapshotViewBuildRequest, SnapshotViewBuilderPort, StateStore,
    TeamCallbackSink, cancel_pair,
};
use crate::scheduler::TopScheduler;
use crate::store::file::FileStateStore;
use crate::team::{HarnessOperateTeam, ReadOnlyOperateTeam};
use crate::topology::DeploymentTopology;
use crate::view::{FileArtifactStore, PROFILE_OPERATE_READONLY, RedactingViewBuilder};

/// Capabilities granted to the v0.1 read-only Operate Job.
const READONLY_CAPABILITIES: [&str; 1] = ["observe.readonly"];

/// Sink that routes Team callbacks straight into the Scheduler.
struct SchedulerSink {
    scheduler: Arc<TopScheduler>,
}

#[async_trait]
impl TeamCallbackSink for SchedulerSink {
    /// Every delivery is a `handle_callback` call, so ordering and rejection rules apply as-is.
    async fn deliver(&self, callback: TeamCallback) -> AgentResult<()> {
        self.scheduler.handle_callback(callback).await.map(|_| ())
    }
}

/// Which Operate Team implementation the runner dispatches to.
pub enum TeamBackend {
    /// Deterministic read-only diagnosis; needs no model.
    ReadOnly,
    /// Model-backed diagnosis through the agent harness over the given client.
    Harness {
        /// Model backend the harness talks to.
        client: Arc<dyn ModelClient>,
        /// Run budgets for each Job.
        budget: HarnessBudget,
        /// Human-readable backend name for operator output (e.g. the model name).
        label: String,
    },
}

/// Fully wired v0.1 control plane over one data directory and one topology.
pub struct SliceRunner {
    topology: DeploymentTopology,
    store: Arc<FileStateStore>,
    scheduler: Arc<TopScheduler>,
    team: Box<dyn AgentTeamPort>,
    team_label: String,
    view_builder: Arc<RedactingViewBuilder>,
}

impl SliceRunner {
    /// Wires every component over the given topology, data directory, and Team backend.
    pub fn wire(
        topology: DeploymentTopology,
        data_dir: &Path,
        backend: TeamBackend,
    ) -> AgentResult<Self> {
        let store = Arc::new(FileStateStore::open(data_dir)?);
        let artifacts = FileArtifactStore::new(data_dir.join("artifact-bodies"));
        let collector = Arc::new(TopologyCollector::new(
            topology.clone(),
            store.clone() as Arc<dyn StateStore>,
        ));
        let view_builder = Arc::new(RedactingViewBuilder::new(artifacts.clone()));
        let scheduler = Arc::new(
            TopScheduler::new(store.clone())
                .with_collector(collector)
                .with_view_builder(view_builder.clone()),
        );
        let (team, team_label): (Box<dyn AgentTeamPort>, String) = match backend {
            TeamBackend::ReadOnly => (
                Box::new(ReadOnlyOperateTeam::new(artifacts)),
                "readonly (deterministic)".to_string(),
            ),
            TeamBackend::Harness {
                client,
                budget,
                label,
            } => (
                Box::new(
                    HarnessOperateTeam::new(
                        client,
                        artifacts,
                        store.clone() as Arc<dyn StateStore>,
                    )
                    .with_config(budget),
                ),
                format!("harness ({label})"),
            ),
        };
        Ok(Self {
            topology,
            store,
            scheduler,
            team,
            team_label,
            view_builder,
        })
    }

    /// Returns the human-readable name of the wired Team backend.
    pub fn team_label(&self) -> &str {
        &self.team_label
    }

    /// Returns the shared store, for inspection commands and tests.
    pub fn store(&self) -> Arc<FileStateStore> {
        self.store.clone()
    }

    /// Returns the wired Scheduler.
    pub fn scheduler(&self) -> Arc<TopScheduler> {
        self.scheduler.clone()
    }

    /// Builds the standard capture request for this topology.
    fn capture_request(&self, cause: SnapshotCause) -> CaptureRequest {
        CaptureRequest {
            deployment_id: self.topology.deployment.id,
            topology_revision: self.topology.deployment.topology_revision.clone(),
            cause,
            operation_mode: self.topology.deployment.operation_mode,
            parent_snapshot_id: None,
            requested_probe_ids: Vec::new(),
        }
    }

    /// Slice steps 1–3: capture, persist, and return a Snapshot.
    pub async fn capture(&self, cause: SnapshotCause) -> AgentResult<Snapshot> {
        self.scheduler
            .request_snapshot(self.capture_request(cause))
            .await
    }

    /// Slice steps 5–7: accept a human report, dispatch a read-only Operate Job, run the Team,
    /// and return the completed Job with its Issue.
    pub async fn handle_report(&self, report: HumanReport) -> AgentResult<(Issue, Job)> {
        let issue = self
            .scheduler
            .accept_human_report_with_capture(
                report,
                self.capture_request(SnapshotCause::HumanReport),
            )
            .await?;

        // No policy model is wired, so this is the deterministic work-order fallback.
        let work_order = self
            .scheduler
            .draft_work_order(issue.issue_id, TeamKind::Operate)
            .await?;

        let snapshot = self.store.get_snapshot(issue.opened_snapshot_id).await?;
        let targets: Vec<_> = self
            .topology
            .resources
            .iter()
            .map(|resource| resource.id.clone())
            .collect();
        let build_request = SnapshotViewBuildRequest {
            issue_id: issue.issue_id,
            team_kind: TeamKind::Operate,
            work_order: work_order.clone(),
            allowed_capabilities: READONLY_CAPABILITIES
                .iter()
                .map(ToString::to_string)
                .collect(),
            allowed_target_ids: targets.clone(),
            redaction_profile: PROFILE_OPERATE_READONLY.to_string(),
        };
        let built = self
            .view_builder
            .build_snapshot_view(&snapshot, &build_request)
            .await?;
        self.store.insert_artifact(built.artifact.clone()).await?;

        let job = self
            .scheduler
            .create_job(
                issue.issue_id,
                built.snapshot_view,
                TeamKind::Operate,
                work_order,
                build_request.allowed_capabilities,
                targets,
            )
            .await?;

        let sink = SchedulerSink {
            scheduler: self.scheduler.clone(),
        };
        let (_cancel_handle, cancel_signal) = cancel_pair();
        self.team
            .run_job(&job, &built.artifact, &sink, cancel_signal)
            .await?;

        let completed = self.store.get_job(job.job_id).await?;
        let issue = self.store.get_issue(issue.issue_id).await?;
        Ok((issue, completed))
    }

    /// Renders a Snapshot as an operator-facing text summary.
    pub fn render_snapshot(&self, snapshot: &Snapshot) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Snapshot {} · {} · cause {:?} · mode {:?}",
            snapshot.snapshot_id,
            snapshot.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
            snapshot.cause,
            snapshot.operation_mode,
        );
        let _ = writeln!(out, "topology revision: {}", snapshot.topology_revision);
        let _ = writeln!(out, "\nresources:");
        for resource in &snapshot.resources {
            let latency = resource
                .metrics
                .iter()
                .find(|metric| metric.name.ends_with(".latency"))
                .map(|metric| format!(" ({:.0} ms)", metric.value))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  {:<24} {:<12} {:?}{latency}",
                resource.resource_id,
                format!("{:?}", resource.kind),
                resource.health,
            );
        }
        if snapshot.coverage_gaps.is_empty() {
            let _ = writeln!(out, "\ncoverage gaps: none");
        } else {
            let _ = writeln!(out, "\ncoverage gaps:");
            for gap in &snapshot.coverage_gaps {
                let _ = writeln!(
                    out,
                    "  {:<24} probe {:<14} {}",
                    gap.resource_id, gap.probe_id, gap.reason
                );
            }
        }
        out
    }

    /// Renders a completed report flow as operator-facing text.
    pub fn render_report_outcome(issue: &Issue, job: &Job) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Issue {} · priority {:?} · status {:?}",
            issue.issue_id, issue.priority, issue.status
        );
        let _ = writeln!(
            out,
            "Job   {} · team {:?} · status {:?}",
            job.job_id, job.team_kind, job.status
        );
        if let Some(result) = &job.result {
            let _ = writeln!(out, "\noutcome: {:?}", result.outcome);
            let _ = writeln!(out, "summary: {}", result.summary);
            for question in &result.unresolved_questions {
                let _ = writeln!(out, "open:    {question}");
            }
        } else if job.status == JobStatus::Running {
            let _ = writeln!(out, "\nthe Job is still running");
        }
        out
    }
}
