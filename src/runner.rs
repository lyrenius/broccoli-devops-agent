//! Wiring and orchestration for the operator flows.
//!
//! The runner assembles the file store, topology Collector, redacting View Builder, Agents
//! Platform, authority policy, Scheduler, and one Operate Team backend, then drives the flows the
//! CLI and API expose: capture-and-display, human report to completed Job, running the Job's
//! proposed actions through the authority matrix, the three-category inbox (permission requests,
//! permission denials, failures) with its human decisions — approve, reject with a comment,
//! acknowledge, or send back upstream as a revising Job that carries the feedback — and restart
//! recovery. The Team backend is chosen at wiring time — deterministic, or the model-backed
//! harness Team over the configured relay — and nothing else changes between them. No Scheduler
//! Policy model is wired yet, so every Scheduler decision point still exercises its conservative
//! deterministic fallback — by design.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use broccoli_agent_harness::{AgentConfig as HarnessBudget, ModelClient};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::collector::TopologyCollector;
use crate::domain::{
    ActionProposal, ActionRun, ActionRunId, ActionStatus, Artifact, FeedbackOrigin, HumanFeedback,
    HumanReport, HumanReview, Issue, IssueId, Job, JobBrief, JobId, JobOutcome, JobResult,
    JobStatus, ResourceId, ResourceKind, ReviewDecision, Snapshot, SnapshotCause, SnapshotId,
    TeamCallback, TeamKind,
};
use crate::error::{AgentError, AgentResult};
use crate::platform::{LocalCommandPlatform, PlatformConfig};
use crate::policy::{AuthorityPolicy, OPERATE_CAPABILITIES};
use crate::ports::{AgentTeamPort, CaptureRequest, StateStore, TeamCallbackSink, cancel_pair};
use crate::scheduler::{IssueClosure, RecoverySummary, TopScheduler};
use crate::store::file::FileStateStore;
use crate::team::{HarnessOperateTeam, ReadOnlyOperateTeam};
use crate::topology::DeploymentTopology;
use crate::tr;
use crate::view::{FileArtifactStore, PROFILE_OPERATE_READONLY, RedactingViewBuilder};

/// Characters of execution evidence handed to the next pass, at most.
const EVIDENCE_LIMIT: usize = 2000;

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

/// The inbox: everything that waits for a human, in its three categories.
///
/// This is a projection over the store, computed on demand, so it can never disagree with the
/// records it is built from. Items leave the inbox only through a recorded human decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    /// Actions the matrix holds for human approval.
    pub permission_requests: Vec<ActionRun>,
    /// Actions denied by rule or by a human, not yet reviewed.
    pub permission_denied: Vec<ActionRun>,
    /// Jobs that failed, not yet reviewed.
    pub failed_jobs: Vec<Job>,
    /// Actions whose execution or verification failed, not yet reviewed.
    pub failed_actions: Vec<ActionRun>,
}

impl Inbox {
    /// Number of items waiting for a human across every category.
    pub fn total(&self) -> usize {
        self.permission_requests.len()
            + self.permission_denied.len()
            + self.failed_jobs.len()
            + self.failed_actions.len()
    }
}

/// What a human decided about a denied or failed inbox item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxDecision {
    /// Take note and stop; no further automatic work follows from this item.
    Acknowledge,
    /// Send the reason and comment back upstream: a revising Job runs and its proposals are
    /// evaluated again.
    SendUpstream,
}

/// Outcome of a human review: the reviewed item, and the revision it caused, if any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewOutcome<T> {
    /// The reviewed Job or ActionRun with its review recorded.
    pub reviewed: T,
    /// The revising Job and the ActionRuns its proposals became, when sent upstream.
    pub revision: Option<Revision>,
}

/// A revising Job and the ActionRuns created from its proposals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    /// The Job that carried the feedback.
    pub job: Job,
    /// The ActionRuns its proposals became, already evaluated by the matrix.
    pub actions: Vec<ActionRun>,
}

/// Fully wired control plane over one data directory and one topology.
pub struct SliceRunner {
    topology: DeploymentTopology,
    store: Arc<FileStateStore>,
    scheduler: Arc<TopScheduler>,
    team: Box<dyn AgentTeamPort>,
    team_label: String,
    artifacts: FileArtifactStore,
    dry_run: bool,
    /// Serializes review-plus-dispatch so two reviewers of one item cannot both dispatch a
    /// revision; the store's compare-and-set catches what slips past process boundaries.
    review_lock: Mutex<()>,
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
        let resources: HashMap<ResourceId, ResourceKind> = topology
            .resources
            .iter()
            .map(|resource| (resource.id.clone(), resource.kind))
            .collect();
        let authority = AuthorityPolicy::new(
            platform.classification.clone(),
            Duration::from_secs(platform.auto_repeat_window_secs),
        )
        .with_resources(resources);
        let platform = Arc::new(LocalCommandPlatform::new(
            platform,
            artifacts.clone(),
            store.clone() as Arc<dyn StateStore>,
            &topology,
        ));
        let artifacts_for_api = artifacts.clone();
        let scheduler = Arc::new(
            TopScheduler::new(store.clone())
                .with_collector(collector)
                .with_view_builder(view_builder)
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
            artifacts: artifacts_for_api,
            dry_run,
            review_lock: Mutex::new(()),
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

    /// The artifact body store, for serving transcripts and Views to operator UIs.
    pub fn artifacts(&self) -> &FileArtifactStore {
        &self.artifacts
    }

    /// The deployment's topology.
    pub fn topology(&self) -> &DeploymentTopology {
        &self.topology
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

    /// The Operate scope over every resource in the topology: every capability the matrix can
    /// decide (the matrix, not the scope, decides what needs a human), all resources in scope.
    fn operate_brief(&self) -> JobBrief {
        JobBrief::new(
            TeamKind::Operate,
            OPERATE_CAPABILITIES
                .iter()
                .map(ToString::to_string)
                .collect(),
            self.topology
                .resources
                .iter()
                .map(|resource| resource.id.clone())
                .collect(),
        )
    }

    /// Captures, persists, and returns a Snapshot.
    pub async fn capture(&self, cause: SnapshotCause) -> AgentResult<Snapshot> {
        self.scheduler
            .request_snapshot(self.capture_request(cause))
            .await
    }

    /// Accepts a human report, dispatches an Operate Job over the report-time Snapshot, runs the
    /// Team, and returns the finished Job with its Issue. Proposed actions are not run here; see
    /// `run_proposals`.
    pub async fn handle_report(&self, report: HumanReport) -> AgentResult<(Issue, Job)> {
        let issue = self
            .scheduler
            .accept_human_report_with_capture(
                report,
                self.capture_request(SnapshotCause::HumanReport),
            )
            .await?;
        let (job, view) = self
            .scheduler
            .dispatch_job(
                issue.issue_id,
                issue.opened_snapshot_id,
                self.operate_brief(),
                PROFILE_OPERATE_READONLY,
            )
            .await?;
        let job = self.run_team(job, &view).await?;
        let issue = self.store.get_issue(issue.issue_id).await?;
        Ok((issue, job))
    }

    /// Runs the wired Team over a dispatched Job and returns the Job as the Scheduler left it.
    ///
    /// A Team that errors out instead of delivering a final callback still ends the Job: the
    /// error becomes a `Failed` result, so the Job lands in the Failed Job inbox rather than
    /// staying `Running` forever with nobody responsible for it.
    async fn run_team(&self, job: Job, view: &Artifact) -> AgentResult<Job> {
        let sink = SchedulerSink {
            scheduler: self.scheduler.clone(),
        };
        let (_cancel_handle, cancel_signal) = cancel_pair();
        if let Err(error) = self.team.run_job(&job, view, &sink, cancel_signal).await {
            let current = self.store.get_job(job.job_id).await?;
            if !current.status.is_terminal() {
                sink.deliver(
                    TeamCallback::new(
                        job.issue_id,
                        job.job_id,
                        tr!("The Team backend failed", "团队后端出错"),
                    )
                    .with_final_result(JobResult::new(
                        JobOutcome::Failed,
                        tr!(
                            format!("the Team backend failed: {error}"),
                            format!("团队后端出错：{error}")
                        ),
                    )),
                )
                .await?;
            }
        }
        self.store.get_job(job.job_id).await
    }

    /// Runs every action the Job proposed through the authority matrix.
    ///
    /// Each proposal becomes an ActionRun whose approval the matrix decides. `auto` actions are
    /// executed and verified immediately; `approve` actions wait in the Permission Request inbox
    /// (see `approve_action`); `deny` actions are cancelled with the rule's reason on them and
    /// wait in the Permission Denied inbox.
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

    /// Records a human approval by name and runs the action to completion.
    pub async fn approve_action(
        &self,
        action_run_id: ActionRunId,
        approved_by: &str,
    ) -> AgentResult<ActionRun> {
        self.scheduler
            .approve_action(action_run_id, approved_by)
            .await?;
        self.execute_and_verify(action_run_id).await
    }

    /// Records a human rejection with its comment; the action is cancelled and never executes.
    ///
    /// The denial stays visible in the Permission Denied inbox until it is reviewed there.
    pub async fn reject_action(
        &self,
        action_run_id: ActionRunId,
        rejected_by: &str,
        comment: Option<String>,
    ) -> AgentResult<ActionRun> {
        self.scheduler
            .reject_action(action_run_id, rejected_by, comment)
            .await
    }

    /// Applies a human's inbox decision to a denied or failed action.
    ///
    /// `Acknowledge` records the review and stops. `SendUpstream` first captures a fresh Snapshot
    /// and dispatches a revising Job whose brief carries every earlier feedback item plus this
    /// one (the denial's reason and comment, or the failure summary), records the review naming
    /// that Job, runs the Team, and evaluates the new proposals — the feedback participates in
    /// the next pass rather than being displayed in history.
    pub async fn review_action(
        &self,
        action_run_id: ActionRunId,
        reviewer: &str,
        decision: InboxDecision,
        comment: Option<String>,
    ) -> AgentResult<ReviewOutcome<ActionRun>> {
        let _serialized = self.review_lock.lock().await;
        let action = self.store.get_action_run(action_run_id).await?;
        if !action.needs_review() {
            return Err(AgentError::InvalidInput(format!(
                "ActionRun `{action_run_id}` is not in the inbox"
            )));
        }
        let origin = match &action.denial {
            Some(denial) => FeedbackOrigin::DeniedAction {
                action_run_id,
                runbook_id: action.runbook_id.clone(),
                target_ids: action.target_ids.clone(),
                denial: denial.clone(),
            },
            None => FeedbackOrigin::FailedAction {
                action_run_id,
                runbook_id: action.runbook_id.clone(),
                target_ids: action.target_ids.clone(),
                summary: match (&action.verification_summary, &action.execution_summary) {
                    (Some(verification), _) => verification.clone(),
                    (None, Some(execution)) => execution.clone(),
                    (None, None) => tr!(
                        format!("execution ended as {:?}", action.status),
                        format!("执行以 {:?} 结束", action.status)
                    ),
                },
                evidence: self.execution_evidence(&action).await,
            },
        };
        self.review(
            action.issue_id,
            action.originating_job_id,
            origin,
            reviewer,
            decision,
            comment,
            |review| async move { self.scheduler.review_action(action_run_id, review).await },
        )
        .await
    }

    /// Applies a human's inbox decision to a failed Job; see `review_action`.
    pub async fn review_job(
        &self,
        job_id: JobId,
        reviewer: &str,
        decision: InboxDecision,
        comment: Option<String>,
    ) -> AgentResult<ReviewOutcome<Job>> {
        let _serialized = self.review_lock.lock().await;
        let job = self.store.get_job(job_id).await?;
        if !job.needs_review() {
            return Err(AgentError::InvalidInput(format!(
                "Job `{job_id}` is not in the Failed Job inbox"
            )));
        }
        let origin = FeedbackOrigin::FailedJob {
            job_id,
            summary: job
                .result
                .as_ref()
                .map(|result| result.summary.clone())
                .unwrap_or_else(|| tr!("no result was recorded", "未记录任何结果").to_string()),
        };
        self.review(
            job.issue_id,
            job_id,
            origin,
            reviewer,
            decision,
            comment,
            |review| async move { self.scheduler.review_job(job_id, review).await },
        )
        .await
    }

    /// Shared review flow: optionally dispatch the revising Job, then record the review.
    #[allow(clippy::too_many_arguments)]
    async fn review<T, F, Fut>(
        &self,
        issue_id: IssueId,
        prior_job_id: JobId,
        origin: FeedbackOrigin,
        reviewer: &str,
        decision: InboxDecision,
        comment: Option<String>,
        record: F,
    ) -> AgentResult<ReviewOutcome<T>>
    where
        F: FnOnce(HumanReview) -> Fut,
        Fut: std::future::Future<Output = AgentResult<T>>,
    {
        match decision {
            InboxDecision::Acknowledge => {
                let review = HumanReview::new(reviewer, ReviewDecision::Acknowledged, comment);
                let reviewed = record(review).await?;
                Ok(ReviewOutcome {
                    reviewed,
                    revision: None,
                })
            }
            InboxDecision::SendUpstream => {
                let prior = self.store.get_job(prior_job_id).await?;
                let mut feedback = prior.feedback.clone();
                feedback.push(HumanFeedback::new(origin, reviewer, comment.clone()));
                let snapshot = self.capture(SnapshotCause::HumanFeedback).await?;
                let (job, view) = self
                    .dispatch_revision(issue_id, snapshot.snapshot_id, prior_job_id, feedback)
                    .await?;
                // The review names the revising Job before the Team runs, so a Team crash still
                // leaves a complete record of what the human decided.
                let review = HumanReview::new(
                    reviewer,
                    ReviewDecision::SentUpstream { job_id: job.job_id },
                    comment,
                );
                let reviewed = record(review).await?;
                let job = self.run_team(job, &view).await?;
                let actions = self.run_proposals(&job).await?;
                Ok(ReviewOutcome {
                    reviewed,
                    revision: Some(Revision { job, actions }),
                })
            }
        }
    }

    /// Reads the action's ActionOutput Artifact and condenses it into sanitized evidence: exit
    /// codes, timeouts, refusal reasons, and the tail of stderr and stdout with secret-shaped
    /// lines removed. `None` when no output was recorded.
    async fn execution_evidence(&self, action: &ActionRun) -> Option<String> {
        let artifact_id = action.execution_artifact_id?;
        let artifact = self.store.get_artifact(artifact_id).await.ok()?;
        let bytes = self.artifacts.read_verified(&artifact).ok()?;
        let record: Value = serde_json::from_slice(&bytes).ok()?;
        Some(summarize_execution_record(&record))
    }

    /// Closes an Issue on a human's say-so; see `TopScheduler::close_issue`.
    pub async fn close_issue(
        &self,
        issue_id: IssueId,
        closure: IssueClosure,
        closed_by: &str,
        comment: Option<String>,
    ) -> AgentResult<Issue> {
        self.scheduler
            .close_issue(issue_id, closure, closed_by, comment)
            .await
    }

    /// Recovers control state after a restart, verifying interrupted actions against a fresh
    /// Snapshot; see `TopScheduler::recover_with`.
    pub async fn recover(&self) -> AgentResult<RecoverySummary> {
        self.scheduler
            .recover_with(Some(self.capture_request(SnapshotCause::AfterAction)))
            .await
    }

    /// Dispatches the revising Job for upstream feedback over a fresh Snapshot.
    async fn dispatch_revision(
        &self,
        issue_id: IssueId,
        snapshot_id: SnapshotId,
        revises_job_id: JobId,
        feedback: Vec<HumanFeedback>,
    ) -> AgentResult<(Job, Artifact)> {
        self.scheduler
            .dispatch_job(
                issue_id,
                snapshot_id,
                self.operate_brief().revising(revises_job_id, feedback),
                PROFILE_OPERATE_READONLY,
            )
            .await
    }

    /// Lists every ActionRun in creation order.
    pub async fn list_actions(&self) -> AgentResult<Vec<ActionRun>> {
        self.store.list_action_runs().await
    }

    /// Computes the inbox from the store.
    pub async fn inbox(&self) -> AgentResult<Inbox> {
        let mut inbox = Inbox::default();
        for action in self.store.list_action_runs().await? {
            if action.status == ActionStatus::WaitingForApproval {
                inbox.permission_requests.push(action);
            } else if action.review.is_none() && action.denial.is_some() {
                inbox.permission_denied.push(action);
            } else if action.review.is_none() && action.has_failed() {
                inbox.failed_actions.push(action);
            }
        }
        inbox.failed_jobs = self
            .store
            .list_jobs()
            .await?
            .into_iter()
            .filter(Job::needs_review)
            .collect();
        Ok(inbox)
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
            "Job   {} · team {:?} · status {:?}{}",
            job.job_id,
            job.team_kind,
            job.status,
            job.revises_job_id
                .map(|id| format!(" · revises {id}"))
                .unwrap_or_default()
        );
        for item in &job.feedback {
            let _ = writeln!(out, "feedback: {}", item.describe());
        }
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
            if let Some(denial) = &action.denial {
                let _ = writeln!(
                    out,
                    "    denied ({:?}): {}{}",
                    denial.source,
                    denial.reason,
                    denial
                        .comment
                        .as_deref()
                        .map(|comment| format!(" — {comment}"))
                        .unwrap_or_default()
                );
            }
            if let Some(summary) = &action.execution_summary {
                let _ = writeln!(out, "    execution:    {summary}");
            }
            if let Some(summary) = &action.verification_summary {
                let _ = writeln!(
                    out,
                    "    verification: {summary}{}",
                    action
                        .verification_evidence
                        .map(|evidence| format!(" [{evidence:?} evidence]"))
                        .unwrap_or_default()
                );
            }
            if let Some(review) = &action.review {
                let _ = writeln!(
                    out,
                    "    reviewed by {}: {:?}",
                    review.reviewer, review.decision
                );
            }
        }
        out
    }

    /// Renders the inbox as operator-facing text.
    pub fn render_inbox(inbox: &Inbox) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "permission requests ({}):",
            inbox.permission_requests.len()
        );
        for action in &inbox.permission_requests {
            let _ = writeln!(
                out,
                "  {}  {} on {} — {}",
                action.action_run_id,
                action.runbook_id,
                action.target_ids.join(","),
                action.reason
            );
        }
        let _ = writeln!(
            out,
            "permission denied ({}):",
            inbox.permission_denied.len()
        );
        for action in &inbox.permission_denied {
            let denial = action.denial.as_ref();
            let _ = writeln!(
                out,
                "  {}  {} on {} — {}{}",
                action.action_run_id,
                action.runbook_id,
                action.target_ids.join(","),
                denial.map_or("", |d| d.reason.as_str()),
                denial
                    .and_then(|d| d.comment.as_deref())
                    .map(|comment| format!(" — {comment}"))
                    .unwrap_or_default()
            );
        }
        let _ = writeln!(out, "failed jobs ({}):", inbox.failed_jobs.len());
        for job in &inbox.failed_jobs {
            let _ = writeln!(
                out,
                "  {}  {}",
                job.job_id,
                job.result
                    .as_ref()
                    .map_or("no result", |result| result.summary.as_str())
            );
        }
        let _ = writeln!(out, "failed actions ({}):", inbox.failed_actions.len());
        for action in &inbox.failed_actions {
            let _ = writeln!(
                out,
                "  {}  {} on {} — {:?}: {}",
                action.action_run_id,
                action.runbook_id,
                action.target_ids.join(","),
                action.status,
                action
                    .verification_summary
                    .as_deref()
                    .or(action.execution_summary.as_deref())
                    .unwrap_or("—")
            );
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

/// Fact-name or line fragments that mark secret-shaped output; such lines are dropped.
const EVIDENCE_SECRET_MARKERS: [&str; 6] = [
    "password",
    "secret",
    "token",
    "credential",
    "api_key",
    "authorization",
];

/// Condenses an ActionOutput record into a short, sanitized evidence string for the next pass.
///
/// Machine output is untrusted: it is trimmed to a tail, lines that look like they carry a secret
/// are replaced, and the whole thing is capped, so it can be fenced into a View without carrying
/// a credential or a prompt injection of unbounded size along.
pub fn summarize_execution_record(record: &Value) -> String {
    fn tail(text: &str, lines: usize, chars: usize) -> String {
        let kept: Vec<&str> = text
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| {
                let lower = line.to_ascii_lowercase();
                if EVIDENCE_SECRET_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker))
                {
                    "[line redacted: secret-shaped]"
                } else {
                    line
                }
            })
            .collect();
        let joined = kept.join("\n");
        if joined.len() > chars {
            format!("…{}", &joined[joined.len() - chars..])
        } else {
            joined
        }
    }

    let mut parts = Vec::new();
    if let Some(refused) = record["refused"].as_str() {
        parts.push(format!("refused: {refused}"));
    }
    if record["dry_run"].as_bool() == Some(true) {
        parts.push("dry run: commands were rendered, not executed".to_string());
    }
    for run in record["runs"].as_array().into_iter().flatten() {
        let target = run["target"].as_str().unwrap_or("?");
        let status = if run["spawn_error"].is_string() {
            format!(
                "could not start: {}",
                run["spawn_error"].as_str().unwrap_or("")
            )
        } else if run["timed_out"].as_bool() == Some(true) {
            "timed out and was killed".to_string()
        } else {
            format!(
                "exit code {}",
                run["exit_code"]
                    .as_i64()
                    .map_or("none".to_string(), |code| code.to_string())
            )
        };
        let mut line = format!("target {target}: {status}");
        for (name, key) in [("stderr", "stderr"), ("stdout", "stdout")] {
            if let Some(text) = run[key].as_str()
                && !text.trim().is_empty()
            {
                line.push_str(&format!("; {name} tail: {}", tail(text, 12, 600)));
            }
        }
        parts.push(line);
    }
    let mut evidence = parts.join("\n");
    if evidence.len() > EVIDENCE_LIMIT {
        evidence.truncate(EVIDENCE_LIMIT);
        evidence.push('…');
    }
    evidence
}
