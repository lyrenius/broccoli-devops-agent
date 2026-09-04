//! Wiring and orchestration for the operator flows.
//!
//! The runner assembles the file store, topology Collector, redacting View Builder, Agents
//! Platform, authority policy, Scheduler, and one Operate Team backend, then drives the flows the
//! CLI exposes: capture-and-display, human report to completed Job, running the Job's proposed
//! actions through the authority matrix, human approval or rejection of held actions, and restart
//! recovery. The Team backend is chosen at wiring time — deterministic, or the model-backed harness
//! Team over the configured relay — and nothing else changes between them. No Scheduler Policy
//! model is wired yet, so every Scheduler decision point still exercises its conservative
//! deterministic fallback — by design.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use broccoli_agent_harness::{AgentConfig as HarnessBudget, ModelClient};
use sha2::{Digest, Sha256};

use crate::collector::TopologyCollector;
use crate::domain::{
    ActionProposal, ActionRun, ActionRunId, ActionStatus, ApprovalState, HumanReport, Issue, Job,
    JobStatus, Snapshot, SnapshotCause, TeamCallback, TeamKind,
};
use crate::error::AgentResult;
use crate::platform::{LocalCommandPlatform, PlatformConfig};
use crate::policy::AuthorityPolicy;
use crate::ports::{
    AgentTeamPort, CaptureRequest, SnapshotViewBuildRequest, SnapshotViewBuilderPort, StateStore,
    TeamCallbackSink, cancel_pair,
};
use crate::scheduler::TopScheduler;
use crate::store::file::FileStateStore;
use crate::team::{HarnessOperateTeam, ReadOnlyOperateTeam};
use crate::topology::DeploymentTopology;
use crate::view::{FileArtifactStore, PROFILE_OPERATE_READONLY, RedactingViewBuilder};

/// Capabilities granted to the read-only Operate Job.
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

/// Fully wired control plane over one data directory and one topology.
pub struct SliceRunner {
    topology: DeploymentTopology,
    store: Arc<FileStateStore>,
    scheduler: Arc<TopScheduler>,
    team: Box<dyn AgentTeamPort>,
    team_label: String,
    view_builder: Arc<RedactingViewBuilder>,
    dry_run: bool,
}

impl SliceRunner {
    /// Wires every component over the given topology, data directory, Team backend, and Platform.
    pub fn wire(
        topology: DeploymentTopology,
        data_dir: &Path,
        backend: TeamBackend,
        platform: PlatformConfig,
    ) -> AgentResult<Self> {
        let store = Arc::new(FileStateStore::open(data_dir)?);
        let artifacts = FileArtifactStore::new(data_dir.join("artifact-bodies"));
        let collector = Arc::new(TopologyCollector::new(
            topology.clone(),
            store.clone() as Arc<dyn StateStore>,
        ));
        let view_builder = Arc::new(RedactingViewBuilder::new(artifacts.clone()));
        let dry_run = platform.dry_run;
        let authority = AuthorityPolicy::new(
            platform.classification.clone(),
            Duration::from_secs(platform.auto_repeat_window_secs),
        );
        let platform = Arc::new(LocalCommandPlatform::new(
            platform,
            artifacts.clone(),
            &topology,
        ));
        let scheduler = Arc::new(
            TopScheduler::new(store.clone())
                .with_collector(collector)
                .with_view_builder(view_builder.clone())
                .with_platform(platform)
                .with_authority(authority),
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
            dry_run,
        })
    }

    /// Returns the human-readable name of the wired Team backend.
    pub fn team_label(&self) -> &str {
        &self.team_label
    }

    /// Whether the Platform records commands instead of executing them.
    pub fn dry_run(&self) -> bool {
        self.dry_run
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

    /// Captures, persists, and returns a Snapshot.
    pub async fn capture(&self, cause: SnapshotCause) -> AgentResult<Snapshot> {
        self.scheduler
            .request_snapshot(self.capture_request(cause))
            .await
    }

    /// Accepts a human report, dispatches an Operate Job, runs the Team, and returns the completed
    /// Job with its Issue. Proposed actions are not run here; see `run_proposals`.
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

    /// Runs every action the Job proposed through the authority matrix.
    ///
    /// Each proposal becomes an ActionRun whose approval the matrix decides. `auto` actions are
    /// executed and verified immediately; `approve` actions are left waiting for a human (see
    /// `approve_action`); `deny` actions are cancelled with the reason in the event log.
    pub async fn run_proposals(&self, job: &Job) -> AgentResult<Vec<ActionRun>> {
        let proposals: Vec<ActionProposal> = job
            .result
            .as_ref()
            .map(|result| result.proposed_actions.clone())
            .unwrap_or_default();
        let mut actions = Vec::with_capacity(proposals.len());
        for proposal in proposals {
            let key = idempotency_key(job, &proposal)?;
            let action = self
                .scheduler
                .create_action_run(
                    job.job_id,
                    proposal,
                    self.capture_request(SnapshotCause::BeforeAction),
                    key,
                )
                .await?;
            let action = if action.status == ActionStatus::Ready {
                self.execute_and_verify(action.action_run_id).await?
            } else {
                action
            };
            actions.push(action);
        }
        Ok(actions)
    }

    /// Executes a `Ready` action through the Platform and verifies its effect.
    pub async fn execute_and_verify(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        let action = self.scheduler.execute_action(action_run_id).await?;
        if action.status != ActionStatus::Verifying {
            return Ok(action);
        }
        self.scheduler
            .verify_action(
                action_run_id,
                self.capture_request(SnapshotCause::AfterAction),
            )
            .await
    }

    /// Records a human approval and runs the action to completion.
    pub async fn approve_action(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.scheduler
            .apply_action_approval(action_run_id, ApprovalState::Approved)
            .await?;
        self.execute_and_verify(action_run_id).await
    }

    /// Records a human rejection; the action is cancelled and never executes.
    pub async fn reject_action(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.scheduler
            .apply_action_approval(action_run_id, ApprovalState::Rejected)
            .await
    }

    /// Lists every ActionRun in creation order.
    pub async fn list_actions(&self) -> AgentResult<Vec<ActionRun>> {
        self.store.list_action_runs().await
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
            if !result.proposed_actions.is_empty() {
                let _ = writeln!(out, "\nproposed actions:");
                for proposal in &result.proposed_actions {
                    let _ = writeln!(
                        out,
                        "  {} on {} — {}",
                        proposal.runbook_id,
                        proposal.target_ids.join(", "),
                        proposal.reason
                    );
                }
            }
        } else if job.status == JobStatus::Running {
            let _ = writeln!(out, "\nthe Job is still running");
        }
        out
    }

    /// Renders ActionRuns as an operator-facing table.
    pub fn render_actions(actions: &[ActionRun], dry_run: bool) -> String {
        let mut out = String::new();
        if actions.is_empty() {
            let _ = writeln!(out, "no actions");
            return out;
        }
        if dry_run {
            let _ = writeln!(
                out,
                "(platform is in dry-run mode: commands are recorded, not executed)"
            );
        }
        for action in actions {
            let _ = writeln!(
                out,
                "{}  {:<18} {:<22} {:<20} approval {:?}",
                action.action_run_id,
                action.runbook_id,
                action.target_ids.join(","),
                format!("{:?}", action.status),
                action.approval
            );
            if let Some(summary) = &action.verification_summary {
                let _ = writeln!(out, "    verification: {summary}");
            }
        }
        out
    }
}

/// Derives a stable idempotency key from the Issue and the proposal's intent.
///
/// Retrying the same proposal for the same Issue reuses the key, so the Platform can refuse to
/// repeat a side effect; a different target or argument set yields a different key.
fn idempotency_key(job: &Job, proposal: &ActionProposal) -> AgentResult<String> {
    let mut hasher = Sha256::new();
    hasher.update(job.issue_id.as_bytes());
    hasher.update(serde_json::to_vec(proposal)?);
    Ok(format!("{:x}", hasher.finalize())[..24].to_string())
}
