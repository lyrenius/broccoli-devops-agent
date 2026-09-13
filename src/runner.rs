//! Wiring and orchestration for the operator flows.
//!
//! The runner assembles the file store, topology Collector, redacting View Builder, Agents
//! Platform, authority policy, Scheduler, and one Operate Team backend, then drives the flows the
//! CLI and API expose: capture-and-display, human report to completed Job, running the Job's
//! proposed actions through the authority matrix, the inbox (human feedback, permission requests,
//! permission denials, failures) with its human decisions — approve, reject with a comment,
//! acknowledge, or send back upstream as a revising Job that carries the feedback — and restart
//! recovery. The Team backend is chosen at wiring time — deterministic, or the model-backed
//! harness Team over the configured relay — and nothing else changes between them. No Scheduler
//! Policy model is wired yet, so every Scheduler decision point still exercises its conservative
//! deterministic fallback — by design.
//!
//! The runner also drives the **investigation loop**: a chain of passes on one Issue, each pass
//! one Team run over one immutable Snapshot. A pass that needs more evidence ends with a probe
//! request and is superseded by a pass over a fresh Snapshot; a pass whose proposals ran can ask
//! for a follow-up pass over the after-Snapshot to check their effect and decide what comes
//! next. Every later pass carries the whole history (`earlier_passes`) in its View. The chain is
//! bounded by the automatic-pass budget, stops whenever something waits for a human (an
//! approval, a denial, a failure), and resumes — with a fresh budget — when a human approves an
//! action or sends an item back upstream.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use broccoli_agent_harness::{AgentConfig as HarnessBudget, ModelClient};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, broadcast, watch};

use crate::collector::TopologyCollector;
use crate::config::DEFAULT_SNAPSHOT_INTERVAL_SECS;
use crate::domain::{
    ActionProposal, ActionRun, ActionRunId, ActionStatus, Artifact, FeedbackOrigin, HumanFeedback,
    HumanReport, HumanReview, Issue, IssueId, Job, JobBrief, JobId, JobOutcome, JobResult,
    JobStatus, PassActionRecord, PassRecord, ResourceId, ResourceKind, ReviewDecision, Snapshot,
    SnapshotCause, SnapshotId, TeamCallback, TeamKind,
};
use crate::error::{AgentError, AgentResult};
pub use crate::evidence::summarize_execution_record;
use crate::judge::HybridSnapshotJudge;
use crate::platform::{LocalCommandPlatform, PlatformConfig};
use crate::policy::{AuthorityPolicy, OPERATE_CAPABILITIES};
use crate::ports::{
    AgentTeamPort, CancelHandle, CaptureRequest, InspectionPort, InspectionRequest,
    InspectionResult, StateStore, TeamCallbackSink, cancel_pair,
};
use crate::scheduler::{IssueClosure, RecoverySummary, SchedulerMode, TopScheduler};
use crate::session::{self, ImportSummary, SessionBundle};
use crate::settings::{LiveSettings, SharedSettings};
use crate::store::file::FileStateStore;
use crate::team::{HarnessOperateTeam, ReadOnlyOperateTeam};
use crate::topology::DeploymentTopology;
use crate::tr;
use crate::usage::{Pricing, SpendBudget, UsageTotals};
use crate::view::{FileArtifactStore, PROFILE_OPERATE_READONLY, RedactingViewBuilder};

/// Sink that routes Team callbacks straight into the Scheduler, and republishes them.
///
/// The Scheduler's copy is the record; the broadcast is the live view. A CLI command that blocks
/// on a pass for minutes subscribes to it and prints each step as it happens, which is the same
/// information the web console reads from the event stream — one source, two renderings.
struct SchedulerSink {
    scheduler: Arc<TopScheduler>,
    watchers: broadcast::Sender<TeamCallback>,
    running: Arc<Mutex<HashMap<JobId, RunningJob>>>,
}

#[async_trait]
impl TeamCallbackSink for SchedulerSink {
    /// Every delivery is a `handle_callback` call, so ordering and rejection rules apply as-is.
    async fn deliver(&self, callback: TeamCallback) -> AgentResult<()> {
        // Nobody watching is the normal case, and never a reason to fail a callback.
        let _ = self.watchers.send(callback.clone());
        self.scheduler.handle_callback(callback.clone()).await?;
        if callback.step.is_none()
            && let Some(running) = self.running.lock().await.get_mut(&callback.job_id)
            && running
                .last_progress_at
                .is_none_or(|at| callback.created_at >= at)
        {
            running.last_progress_at = Some(callback.created_at);
            running.last_progress = Some(callback.summary);
        }
        Ok(())
    }
}

/// A Team run in flight, and the handle that stops it.
struct RunningJob {
    issue_id: IssueId,
    cancel: Arc<CancelHandle>,
    started_at: DateTime<Utc>,
    last_progress_at: Option<DateTime<Utc>>,
    last_progress: Option<String>,
}

/// A dropped HTTP handler/future must not leave a live cancellation handle in the registry.
struct RunningJobGuard {
    job_id: JobId,
    issue_id: IssueId,
    cancel: Arc<CancelHandle>,
    running: Arc<Mutex<HashMap<JobId, RunningJob>>>,
    scheduler: Arc<TopScheduler>,
    finished: bool,
}

impl RunningJobGuard {
    async fn finish(&mut self) {
        self.running.lock().await.remove(&self.job_id);
        self.scheduler.unregister_job_cancellation(self.job_id);
        self.finished = true;
    }
}

impl Drop for RunningJobGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.cancel.cancel();
        self.scheduler.unregister_job_cancellation(self.job_id);
        let (running, scheduler, id, issue_id) = (
            self.running.clone(),
            self.scheduler.clone(),
            self.job_id,
            self.issue_id,
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                running.lock().await.remove(&id);
                if let Err(error) = scheduler.cancel_job_record(id, "Team execution was abandoned before completion; its request or future was dropped").await {
                    eprintln!("Failed to record cancellation of Job {id}: {error}");
                }
                if let Err(error) = scheduler.reconcile_issue(issue_id).await {
                    eprintln!("Failed to reconcile Issue {issue_id} after Job cancellation: {error}");
                }
            });
        }
    }
}

/// A pass currently running, as the consoles list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningPass {
    /// The Job being run.
    pub job_id: JobId,
    /// The Issue it serves.
    pub issue_id: IssueId,
    /// When the Team run started.
    pub started_at: DateTime<Utc>,
    /// Most recent accepted human-readable Team progress callback.
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
    /// The actual waiting, tool or retry description, in the configured language.
    #[serde(default)]
    pub last_progress: Option<String>,
}

/// Inspection gateway that routes a running Team's read-only requests through the Scheduler.
struct SchedulerInspector {
    scheduler: Arc<TopScheduler>,
}

#[async_trait]
impl InspectionPort for SchedulerInspector {
    /// Every inspection is a `TopScheduler::inspect` call: freeze mode, scope, Platform, event.
    async fn inspect(
        &self,
        job_id: JobId,
        request: InspectionRequest,
    ) -> AgentResult<InspectionResult> {
        self.scheduler.inspect(job_id, request).await
    }
}

/// Budgets for the investigation loop, from the operator's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassPolicy {
    /// Passes the control plane runs on its own for one human report or one send-upstream
    /// review, the first pass included. One means "one pass, then a human"; each probe request
    /// or follow-up spends one.
    pub max_auto_passes: u32,
    /// Read-only inspections a model-backed Team may run in one pass.
    pub max_inspections: u32,
}

impl Default for PassPolicy {
    /// Three passes per chain (observe, act, check), six inspections per pass.
    fn default() -> Self {
        Self {
            max_auto_passes: 3,
            max_inspections: 6,
        }
    }
}

/// Why a chain of passes ended with a given pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassStop {
    /// The pass asked for more evidence and was superseded by the next pass.
    Superseded,
    /// The pass's proposals ran and a follow-up pass was dispatched.
    Continued,
    /// The pass concluded without asking for anything further.
    Done,
    /// The pass wanted to go on, but the automatic-pass budget is spent; a human continues.
    BudgetExhausted,
    /// At least one proposal waits for a human's approval; the approval resumes the chain.
    WaitingForApproval,
    /// Every proposal was denied; the denials wait in the inbox.
    NothingRan,
    /// The Scheduler no longer accepts dispatches.
    Frozen,
    /// The pass failed and waits in the inbox.
    Failed,
    /// The pass ended in a state only a human can move on.
    WaitingForHuman,
}

/// One pass of an investigation chain: the Job, the ActionRuns its proposals became, and why
/// the chain did or did not continue after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassOutcome {
    /// The Job, as the Scheduler left it.
    pub job: Job,
    /// The ActionRuns created from its proposals, already decided by the matrix.
    pub actions: Vec<ActionRun>,
    /// What happened after this pass.
    pub stop: PassStop,
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
        /// Model name, recorded on every usage record so a bill can be attributed.
        model: String,
        /// Human-readable backend name for operator output (e.g. the model name and relay).
        label: String,
    },
}

/// An unresolved Issue and the latest pass a human can continue with feedback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingIssue {
    /// The Issue waiting for human input.
    pub issue: Issue,
    /// The pass the feedback must name, to reject stale or duplicate submissions.
    pub job: Job,
}

/// Human closure information projected from the existing event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueClosureRecord {
    /// Human-selected terminal outcome.
    pub outcome: String,
    /// Operator who closed the Issue.
    pub closed_by: String,
    /// Optional closing explanation.
    pub comment: Option<String>,
    /// Time recorded on the closure event.
    pub closed_at: DateTime<Utc>,
}

/// Issue API projection, including closure comments from older stored sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueRecord {
    /// Existing Issue fields remain at the top level for API compatibility.
    #[serde(flatten)]
    pub issue: Issue,
    /// Closure metadata, when a human closure was recorded.
    pub closure: Option<IssueClosureRecord>,
}

/// The inbox: everything that waits for a human.
///
/// This is a projection over the store, computed on demand, so it can never disagree with the
/// records it is built from. Items leave the inbox only through a recorded human decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    /// Ended investigations needing feedback, without another pending inbox decision.
    #[serde(default)]
    pub waiting_issues: Vec<WaitingIssue>,
    /// Proposals with a missing runbook or executable command, requiring human intervention.
    #[serde(default)]
    pub blocked_actions: Vec<ActionRun>,
    /// Approved or automatically admitted actions waiting to execute, including while frozen.
    #[serde(default)]
    pub queued_actions: Vec<ActionRun>,
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
        self.waiting_issues.len()
            + self.blocked_actions.len()
            + self.permission_requests.len()
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
    /// Further passes the revising Job's chain ran (probe requests, follow-ups), in order.
    #[serde(default)]
    pub follow_ups: Vec<PassOutcome>,
}

/// Fully wired control plane over one data directory and one topology.
pub struct SliceRunner {
    operations: crate::operations::Operations,
    topology: DeploymentTopology,
    store: Arc<FileStateStore>,
    scheduler: Arc<TopScheduler>,
    team: Box<dyn AgentTeamPort>,
    judge: Arc<dyn crate::ports::SnapshotJudgePort>,
    judge_views: Arc<RedactingViewBuilder>,
    snapshot_review_lock: Mutex<()>,
    intake_dispatch_lock: Mutex<()>,
    intake_ready: Notify,
    team_label: String,
    artifacts: FileArtifactStore,
    /// Everything that may change while the control plane runs; see [`crate::settings`].
    settings: SharedSettings,
    /// The periodic capture cadence, for the schedule task to follow changes.
    capture_interval: watch::Sender<Duration>,
    /// Team runs in flight, by Job, with the handle that stops each one.
    running: Arc<Mutex<HashMap<JobId, RunningJob>>>,
    /// Live Team callbacks, for a caller that wants to watch a pass it is blocked on.
    watchers: broadcast::Sender<TeamCallback>,
    /// Serializes review-plus-dispatch so two reviewers of one item cannot both dispatch a
    /// revision; the store's compare-and-set catches what slips past process boundaries.
    review_lock: Mutex<()>,
    /// Serializes queue drains while keeping unrelated API requests responsive.
    queue_lock: Mutex<()>,
    /// An approval and a concurrent resume may see the same Ready action. Serialize that
    /// action through verification, while allowing distinct targets to execute concurrently.
    execution_locks: Mutex<HashMap<ActionRunId, Arc<Mutex<()>>>>,
    /// Claim a follow-up before releasing the lock, then run the Team outside it.
    follow_up_lock: Mutex<()>,
}

impl SliceRunner {
    /// Wires every component with the default pass policy; see [`SliceRunner::wire_with`].
    pub fn wire(
        topology: DeploymentTopology,
        data_dir: &Path,
        backend: TeamBackend,
        platform: PlatformConfig,
    ) -> AgentResult<Self> {
        Self::wire_with(topology, data_dir, backend, platform, PassPolicy::default())
    }

    /// Wires every component over the given topology, data directory, Team backend, Platform,
    /// and investigation-loop budgets.
    pub fn wire_with(
        topology: DeploymentTopology,
        data_dir: &Path,
        backend: TeamBackend,
        platform: PlatformConfig,
        pass_policy: PassPolicy,
    ) -> AgentResult<Self> {
        let store = Arc::new(FileStateStore::open(data_dir)?);
        let artifacts = FileArtifactStore::new(data_dir.join("artifact-bodies"));
        let collector = Arc::new(TopologyCollector::new(
            topology.clone(),
            store.clone() as Arc<dyn StateStore>,
        ));
        let view_builder = Arc::new(RedactingViewBuilder::new(artifacts.clone()));
        // One handle to the live settings, shared with the Platform, the authority matrix, and
        // the Team, so a change on the Settings page reaches every decision that reads it.
        let settings = SharedSettings::new(LiveSettings {
            snapshot_interval: Duration::from_secs(DEFAULT_SNAPSHOT_INTERVAL_SECS),
            pass_policy,
            harness: match &backend {
                TeamBackend::Harness { budget, .. } => budget.clone(),
                TeamBackend::ReadOnly => Default::default(),
            },
            pricing: None,
            budget: SpendBudget::default(),
            platform: platform.clone(),
        });
        let resources: HashMap<ResourceId, ResourceKind> = topology
            .resources
            .iter()
            .map(|resource| (resource.id.clone(), resource.kind))
            .collect();
        let authority = AuthorityPolicy::new(
            platform.classification.clone(),
            Duration::from_secs(platform.auto_repeat_window_secs),
        )
        .with_resources(resources)
        .with_settings(settings.clone());
        let inspection_runbook_ids = LocalCommandPlatform::inspection_runbook_ids(&platform);
        let platform = Arc::new(LocalCommandPlatform::new(
            settings.clone(),
            artifacts.clone(),
            store.clone() as Arc<dyn StateStore>,
            &topology,
        ));
        let artifacts_for_api = artifacts.clone();
        let scheduler = Arc::new(
            TopScheduler::new(store.clone())
                .with_collector(collector)
                .with_view_builder(view_builder.clone())
                .with_platform(platform)
                .with_authority(authority),
        );
        let (review_client, review_model) = match &backend {
            TeamBackend::ReadOnly => (None, String::new()),
            TeamBackend::Harness { client, model, .. } => (Some(client.clone()), model.clone()),
        };
        let judge = Arc::new(HybridSnapshotJudge::new(
            review_client,
            review_model,
            artifacts.clone(),
            store.clone(),
            settings.clone(),
        ));
        let (team, team_label): (Box<dyn AgentTeamPort>, String) = match backend {
            TeamBackend::ReadOnly => (
                Box::new(ReadOnlyOperateTeam::new(artifacts)),
                "readonly (deterministic)".to_string(),
            ),
            TeamBackend::Harness {
                client,
                budget,
                model,
                label,
            } => (
                Box::new(
                    HarnessOperateTeam::new(
                        client,
                        artifacts,
                        store.clone() as Arc<dyn StateStore>,
                    )
                    .with_config(budget)
                    .with_model_name(model)
                    .with_inspection(
                        Arc::new(SchedulerInspector {
                            scheduler: scheduler.clone(),
                        }),
                        inspection_runbook_ids,
                        pass_policy.max_inspections,
                    )
                    .with_settings(settings.clone()),
                ),
                format!("harness ({label})"),
            ),
        };
        Ok(Self {
            operations: crate::operations::Operations::new(store.clone()),
            topology,
            store,
            scheduler,
            team,
            judge,
            judge_views: view_builder,
            snapshot_review_lock: Mutex::new(()),
            intake_dispatch_lock: Mutex::new(()),
            intake_ready: Notify::new(),
            team_label,
            artifacts: artifacts_for_api,
            settings,
            capture_interval: watch::channel(Duration::from_secs(DEFAULT_SNAPSHOT_INTERVAL_SECS)).0,
            running: Arc::new(Mutex::new(HashMap::new())),
            watchers: broadcast::channel(256).0,
            review_lock: Mutex::new(()),
            queue_lock: Mutex::new(()),
            execution_locks: Mutex::new(HashMap::new()),
            follow_up_lock: Mutex::new(()),
        })
    }

    /// Attaches the relay's price list and the cumulative spend ceiling.
    ///
    /// Both are optional and independent: a deployment with prices but no ceiling gets cost
    /// reporting and no halt; one with a token ceiling and no prices gets a halt it can enforce
    /// without knowing what anything costs.
    pub fn with_spend(self, pricing: Option<Pricing>, budget: SpendBudget) -> Self {
        self.settings.replace(LiveSettings {
            pricing,
            budget,
            ..self.settings.current()
        });
        self
    }

    /// Replaces every live setting at once, cadence included; see [`crate::settings`].
    pub fn with_live_settings(self, next: LiveSettings) -> Self {
        self.apply_live(next);
        self
    }

    /// Puts new live settings in force for the next decision that reads them, and tells the
    /// capture schedule its cadence.
    pub fn apply_live(&self, next: LiveSettings) {
        self.capture_interval.send_replace(next.snapshot_interval);
        self.settings.replace(next);
    }

    /// The live settings, as shared with every component that reads them.
    pub fn settings(&self) -> &SharedSettings {
        &self.settings
    }

    /// The investigation-loop budgets in force.
    pub fn pass_policy(&self) -> PassPolicy {
        self.settings.read(|settings| settings.pass_policy)
    }

    /// Activity registry shared by HTTP, CLI and the platform execution scopes.
    pub fn operations(&self) -> &crate::operations::Operations {
        &self.operations
    }

    /// Everything spent at the relay so far, priced when a price list is configured.
    ///
    /// Summed from the EventLog rather than from the Jobs: the log is append-only, so a pass
    /// that was later superseded still has the tokens it really spent counted here.
    pub async fn usage_totals(&self) -> AgentResult<UsageTotals> {
        // What an imported archive spent was billed to whoever ran it; it neither counts here
        // nor eats this controller's ceiling.
        let archived = self.scheduler.archived_issue_ids().await?;
        let events: Vec<_> = self
            .store
            .list_events()
            .await?
            .into_iter()
            .filter(|event| event.issue_id.is_none_or(|id| !archived.contains(&id)))
            .collect();
        let (pricing, budget) = self
            .settings
            .read(|settings| (settings.pricing.clone(), settings.budget.clone()));
        Ok(UsageTotals::from_events(&events, pricing.as_ref(), &budget))
    }

    /// Subscribes to Team callbacks as they are delivered, for live progress in a CLI.
    pub fn watch_progress(&self) -> broadcast::Receiver<TeamCallback> {
        self.watchers.subscribe()
    }

    /// The passes currently running, oldest first.
    pub async fn running_passes(&self) -> Vec<RunningPass> {
        let mut passes: Vec<RunningPass> = self
            .running
            .lock()
            .await
            .iter()
            .map(|(job_id, running)| RunningPass {
                job_id: *job_id,
                issue_id: running.issue_id,
                started_at: running.started_at,
                last_progress_at: running.last_progress_at,
                last_progress: running.last_progress.clone(),
            })
            .collect();
        passes.sort_by_key(|pass| pass.started_at);
        passes
    }

    /// Asks a running pass to stop, and records who asked.
    ///
    /// Cancellation is cooperative: the Team stops at its next step boundary and still delivers a
    /// final callback, so the Job ends as a failure in the inbox with its transcript intact rather
    /// than vanishing mid-flight. Returns whether a run was actually in flight to stop.
    pub async fn cancel_pass(&self, job_id: JobId, by: &str) -> AgentResult<bool> {
        let operation_cancelled = self.operations.cancel_job(job_id, by).await?;
        let Some(running) = self.running.lock().await.get(&job_id).map(|running| {
            running.cancel.cancel();
            running.issue_id
        }) else {
            return Ok(operation_cancelled);
        };
        self.store
            .append_event(
                crate::domain::NewEvent::new(
                    "human",
                    "human.pass_cancelled",
                    tr!(
                        format!("{by} interrupted the running pass"),
                        format!("{by} 中断了正在运行的一轮任务")
                    ),
                )
                .with_issue(running)
                .with_job(job_id),
            )
            .await?;
        Ok(true)
    }

    /// Asks every running pass to stop; returns how many were asked.
    pub async fn cancel_all_passes(&self, by: &str) -> AgentResult<usize> {
        let operation_count = self.operations.cancel_all(by).await?;
        let job_ids: Vec<JobId> = self.running.lock().await.keys().copied().collect();
        let mut stopped = 0;
        for job_id in job_ids {
            if self.cancel_pass(job_id, by).await? {
                stopped += 1;
            }
        }
        Ok(stopped.max(operation_count))
    }

    /// Freezes the Scheduler when the cumulative spend ceiling has been reached.
    ///
    /// Reaching a budget is not an error in itself — the work that spent it was legitimate — so
    /// this only stops what comes next. It reuses the existing full freeze rather than inventing
    /// a second halted state: recovery already restores a freeze across restarts, and an operator
    /// already knows how to resume one (after raising the ceiling).
    async fn freeze_if_budget_spent(&self) -> AgentResult<Option<String>> {
        if !self.settings.read(|settings| settings.budget.is_set()) {
            return Ok(None);
        }
        let totals = self.usage_totals().await?;
        let Some(reason) = totals.budget.and_then(|status| status.reason) else {
            return Ok(None);
        };
        if self.scheduler.mode().await != SchedulerMode::FullyFrozen {
            self.store
                .append_event(crate::domain::NewEvent::new(
                    "top-scheduler",
                    "scheduler.budget_exhausted",
                    tr!(
                        format!(
                            "Dispatch is frozen because {reason}. Raise `[budget]` in the agent                              config, then resume from a console."
                        ),
                        format!(
                            "调度已冻结，因为{reason}。请调高配置中的 `[budget]`，然后在控制台恢复。"
                        )
                    ),
                ))
                .await?;
            self.scheduler.freeze_all().await?;
        }
        Ok(Some(reason))
    }

    /// Refuses to start new model-backed work once the spend ceiling is reached.
    async fn refuse_if_budget_spent(&self, operation: &'static str) -> AgentResult<()> {
        match self.freeze_if_budget_spent().await? {
            None => Ok(()),
            Some(reason) => Err(AgentError::SchedulerFrozen {
                mode: format!("BudgetExhausted ({reason})"),
                operation,
            }),
        }
    }

    /// Returns the human-readable name of the wired Team backend.
    pub fn team_label(&self) -> &str {
        &self.team_label
    }

    /// Whether the Platform records commands instead of executing them.
    pub fn dry_run(&self) -> bool {
        self.settings.read(|settings| settings.platform.dry_run)
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
    /// decide (the matrix, not the scope, decides what needs a human), all resources in scope,
    /// with the Issue's pass history and the remaining automatic-pass budget.
    fn operate_brief(&self, earlier_passes: Vec<PassRecord>, follow_up_budget: u32) -> JobBrief {
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
        .with_history(earlier_passes, follow_up_budget)
    }

    /// The follow-up budget a chain starts with: the policy's passes minus the one being run.
    fn fresh_follow_up_budget(&self) -> u32 {
        self.pass_policy().max_auto_passes.saturating_sub(1)
    }

    /// Starts the Collector's periodic schedule: one Snapshot now, then one every interval the
    /// live settings name.
    ///
    /// Every capture is reviewed and its findings enter the Issue queue. Collection and local
    /// review continue while frozen; the separate intake dispatcher starts work only in Running. The only mode it sits out is `Recovering`, while the Store is being reconciled.
    /// A failed capture is reported on stderr and the schedule continues; a capture that takes
    /// longer than the interval delays the next one rather than piling them up. A changed
    /// cadence takes effect at once, starting with a capture; a cadence of zero pauses the
    /// schedule until it is set again. Aborting the handle stops it.
    pub fn spawn_periodic_capture(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let runner = self.clone();
        let mut cadence = self.capture_interval.subscribe();
        tokio::spawn(async move {
            loop {
                let interval = *cadence.borrow_and_update();
                if interval.is_zero() {
                    if cadence.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                if runner.scheduler.mode().await != SchedulerMode::Recovering
                    && let Err(error) = runner.capture(SnapshotCause::Periodic).await
                {
                    eprintln!("periodic snapshot capture failed: {error}");
                }
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    changed = cadence.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
            }
        })
    }

    /// Selects another Snapshot Judge adapter, preserving the same intake and audit boundary.
    pub fn with_snapshot_judge(mut self, judge: Arc<dyn crate::ports::SnapshotJudgePort>) -> Self {
        self.judge = judge;
        self
    }

    /// Captures and persists a Snapshot. Manual/periodic captures also receive one review;
    /// internal before/after/probe captures do not recursively generate investigations.
    /// Review errors are recorded separately and never discard a successfully stored Snapshot.
    pub async fn capture(&self, cause: SnapshotCause) -> AgentResult<Snapshot> {
        self.operations
            .run("capture", Box::pin(self.capture_work(cause)))
            .await
    }

    async fn capture_work(&self, cause: SnapshotCause) -> AgentResult<Snapshot> {
        let snapshot = self
            .scheduler
            .request_snapshot(self.capture_request(cause))
            .await?;
        if matches!(cause, SnapshotCause::Manual | SnapshotCause::Periodic)
            && let Err(error) = self.review_snapshot(snapshot.snapshot_id).await
        {
            let payload = json!({ "snapshot_id": snapshot.snapshot_id, "status": "failed",
                "summary": tr!("Snapshot saved, but its review failed", "快照已保存，但审查失败"),
                "error": error.to_string(), "updated_at": Utc::now(), "issue_ids": [], "artifact_ids": [] });
            if let Err(log_error) = self
                .store
                .append_event(
                    crate::domain::NewEvent::new(
                        "snapshot-judge",
                        "snapshot_judge.review_failed",
                        error.to_string(),
                    )
                    .with_payload(payload),
                )
                .await
            {
                eprintln!(
                    "Snapshot {} saved; review failed: {error}; recording failure also failed: {log_error}",
                    snapshot.snapshot_id
                );
            }
        }
        Ok(snapshot)
    }

    /// Reviews a stored Snapshot once, records the exact input/output, and feeds valid findings
    /// through Scheduler triage. Repeated reviews of the same ID return the recorded result.
    pub async fn review_snapshot(&self, snapshot_id: SnapshotId) -> AgentResult<Value> {
        let _review =
            crate::operations::cancellable(async { Ok(self.snapshot_review_lock.lock().await) })
                .await?;
        if let Some(event) = self
            .store
            .list_events()
            .await?
            .into_iter()
            .rev()
            .find(|event| {
                event.kind == "snapshot_judge.review_completed"
                    && event.payload["snapshot_id"] == json!(snapshot_id)
            })
        {
            return Ok(event.payload);
        }

        self.operations
            .run(
                "review_snapshot",
                Box::pin(self.review_snapshot_work(snapshot_id)),
            )
            .await
    }

    async fn review_snapshot_work(&self, snapshot_id: SnapshotId) -> AgentResult<Value> {
        let snapshot = self.store.get_snapshot(snapshot_id).await?;
        let view = self.judge_views.build_judge_view(&snapshot)?;
        self.store.insert_artifact(view.clone()).await?;
        let started = self.store.append_event(crate::domain::NewEvent::new(
            "snapshot-judge", "snapshot_judge.review_started", tr!("Reviewing the captured Snapshot", "正在审查刚采集的快照"),
        ).with_payload(json!({ "snapshot_id": snapshot_id, "status": "running", "summary": tr!("Snapshot review in progress", "快照审查进行中"),
            "updated_at": Utc::now(), "issue_ids": [], "artifact_ids": [view.artifact_id], "error": null }))
            .with_artifacts(vec![view.artifact_id])).await?;
        let model_allowed = self.freeze_if_budget_spent().await?.is_none();
        crate::operations::phase(crate::operations::OperationPhase::Model, None, None, None)
            .await?;
        let judgement = crate::operations::cancellable(self.judge.inspect_snapshot(
            snapshot_id,
            &view,
            model_allowed,
        ))
        .await?;
        if let Some(usage) = &judgement.usage {
            let mut payload = serde_json::to_value(usage)?;
            payload["snapshot_id"] = json!(snapshot_id);
            payload["consumer"] = json!("snapshot_judge");
            self.store
                .append_event(
                    crate::domain::NewEvent::new(
                        "snapshot-judge",
                        crate::usage::USAGE_EVENT_KIND,
                        format!("Snapshot review used {} tokens", usage.total_tokens()),
                    )
                    .with_payload(payload),
                )
                .await?;
        }
        self.freeze_if_budget_spent().await?;
        let count = judgement.candidates.len();
        let mut issues = Vec::new();
        let mut errors: Vec<String> = judgement.model_error.into_iter().collect();
        for mut candidate in judgement.candidates {
            if candidate.snapshot_id != snapshot_id {
                errors.push("Judge returned a candidate for another Snapshot".into());
                continue;
            }
            candidate.evidence_ids.push(started.event_id);
            match self.scheduler.triage_candidate(candidate).await {
                Ok(crate::scheduler::TriageOutcome::IssueCreated(issue)) => {
                    issues.push(issue.issue_id)
                }
                Ok(crate::scheduler::TriageOutcome::MergedInto(id)) => issues.push(id),
                Ok(_) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        issues.sort();
        issues.dedup();
        let mut artifacts = vec![view.artifact_id];
        artifacts.extend(judgement.artifact_ids);
        let payload = json!({ "snapshot_id": snapshot_id,
            "status": if errors.is_empty() { "completed" } else { "partial" },
            "summary": judgement.summary, "candidate_count": count,
            "issue_ids": issues, "artifact_ids": artifacts,
            "error": if errors.is_empty() { None } else { Some(errors.join("; ")) }, "updated_at": Utc::now() });
        self.store
            .append_event(
                crate::domain::NewEvent::new(
                    "snapshot-judge",
                    "snapshot_judge.review_completed",
                    judgement.summary,
                )
                .with_payload(payload.clone())
                .with_artifacts(artifacts.clone())
                .with_trust(crate::domain::ContentTrust::Mixed),
            )
            .await?;
        // Bind shared review evidence to each resulting Issue so its Trace and session export
        // include the Judge View and transcript. Usage remains one global billing record.
        for issue_id in &issues {
            self.store
                .append_event(
                    crate::domain::NewEvent::new(
                        "snapshot-judge",
                        "snapshot_judge.issue_reviewed",
                        payload["summary"].as_str().unwrap_or_default(),
                    )
                    .with_issue(*issue_id)
                    .with_payload(payload.clone())
                    .with_artifacts(artifacts.clone())
                    .with_trust(crate::domain::ContentTrust::Mixed),
                )
                .await?;
        }
        self.intake_ready.notify_one();
        Ok(payload)
    }

    /// Most recent review state for the operator consoles, including a review still running.
    pub async fn latest_snapshot_review(&self) -> AgentResult<Option<Value>> {
        Ok(self
            .store
            .list_events()
            .await?
            .into_iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    "snapshot_judge.review_started"
                        | "snapshot_judge.review_completed"
                        | "snapshot_judge.review_failed"
                )
            })
            .map(|event| event.payload))
    }

    /// Runs newly accepted automatic Issues in priority order through the existing Operate
    /// pipeline. Frozen Issues remain Open and durable; merges never spawn a second initial Job.
    pub async fn dispatch_pending_issues(&self) -> AgentResult<()> {
        let _dispatch = self.intake_dispatch_lock.lock().await;
        loop {
            if !self.dispatch_allowed().await || self.freeze_if_budget_spent().await?.is_some() {
                return Ok(());
            }
            let jobs = self.store.list_jobs().await?;
            let mut pending: Vec<_> = self
                .store
                .list_issues()
                .await?
                .into_iter()
                .filter(|issue| {
                    issue.source == crate::domain::IssueSource::Judge
                        && issue.status == crate::domain::IssueStatus::Open
                        && !issue.is_archived()
                        && !jobs.iter().any(|job| job.issue_id == issue.issue_id)
                })
                .collect();
            pending.sort_by(|a, b| {
                b.priority
                    .cmp(&a.priority)
                    .then(a.created_at.cmp(&b.created_at))
            });
            let Some(issue) = pending.into_iter().next() else {
                return Ok(());
            };
            // An Issue may have waited through a freeze or another remediation. Bind its
            // first Operate pass to fresh evidence, without recursively reviewing this capture.
            let dispatched = async {
                let snapshot = self.capture(SnapshotCause::JudgeEvaluation).await?;
                let current = self.store.get_issue(issue.issue_id).await?;
                if current.status.is_terminal() || current.is_archived() {
                    return Err(AgentError::InvalidInput(
                        "queued Issue was closed before dispatch".into(),
                    ));
                }
                let mut updated = current.clone();
                updated.update_current_snapshot(snapshot.snapshot_id);
                self.store.update_issue_if(&current, updated).await?;
                self.scheduler
                    .dispatch_job(
                        issue.issue_id,
                        snapshot.snapshot_id,
                        self.operate_brief(Vec::new(), self.fresh_follow_up_budget()),
                        PROFILE_OPERATE_READONLY,
                    )
                    .await
            }
            .await;
            let outcome = match dispatched {
                Ok((job, view)) => match self.run_team(job, &view).await {
                    Ok(job) => self.drive_passes(job).await.map(|_| ()),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            if let Err(error) = outcome {
                if matches!(error, AgentError::SchedulerFrozen { .. }) {
                    return Ok(());
                }
                let current = self.store.get_issue(issue.issue_id).await?;
                if current.status == crate::domain::IssueStatus::Open {
                    let mut next = current.clone();
                    next.transition_to(crate::domain::IssueStatus::WaitingForHuman)?;
                    self.store.update_issue_if(&current, next).await?;
                }
                self.store
                    .append_event(
                        crate::domain::NewEvent::new(
                            "top-scheduler",
                            "scheduler.intake_failed",
                            error.to_string(),
                        )
                        .with_issue(issue.issue_id),
                    )
                    .await?;
            }
        }
    }

    /// Starts the intake worker separately from the Collector, so a long investigation does
    /// not block later captures. Open automatic Issues are picked up after startup or resume.
    pub fn spawn_intake_dispatch(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let runner = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = runner.dispatch_pending_issues().await {
                    eprintln!("automatic issue dispatch failed: {error}");
                }
                tokio::select! {
                    () = runner.intake_ready.notified() => {},
                    () = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
        })
    }

    /// Accepts a human report, dispatches the first Operate pass over the report-time Snapshot,
    /// runs the Team, and returns the finished Job with its Issue. Proposed actions are not run
    /// here, and the chain is not continued; see `drive_passes` (or `run_proposals` for the
    /// single-pass flow).
    pub async fn handle_report(&self, report: HumanReport) -> AgentResult<(Issue, Job)> {
        self.operations
            .run("handle_report", Box::pin(self.handle_report_work(report)))
            .await
    }

    async fn handle_report_work(&self, report: HumanReport) -> AgentResult<(Issue, Job)> {
        self.refuse_if_budget_spent("accept a human report").await?;
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
                self.operate_brief(Vec::new(), self.fresh_follow_up_budget()),
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
        self.operations
            .run("run_team", Box::pin(self.run_team_work(job, view)))
            .await
    }

    async fn run_team_work(&self, job: Job, view: &Artifact) -> AgentResult<Job> {
        let sink = SchedulerSink {
            scheduler: self.scheduler.clone(),
            watchers: self.watchers.clone(),
            running: self.running.clone(),
        };
        // The handle lives in the registry for as long as the run does, which is what makes an
        // interruption reachable: a console asks for the Job by ID and the Team stops at its next
        // step boundary. Held locally, as it was before, nothing could ever stop a run.
        let (cancel_handle, cancel_signal) = cancel_pair();
        let cancel_handle = Arc::new(cancel_handle);
        let mut guard = RunningJobGuard {
            job_id: job.job_id,
            issue_id: job.issue_id,
            cancel: cancel_handle.clone(),
            running: self.running.clone(),
            scheduler: self.scheduler.clone(),
            finished: false,
        };
        self.scheduler
            .register_job_cancellation(job.job_id, cancel_handle.clone());
        self.running.lock().await.insert(
            job.job_id,
            RunningJob {
                issue_id: job.issue_id,
                cancel: cancel_handle,
                started_at: Utc::now(),
                last_progress_at: None,
                last_progress: None,
            },
        );
        // Register before rechecking, so a concurrent close either cancels the handle or
        // is observed here before any model/tool call starts.
        let issue = self.store.get_issue(job.issue_id).await?;
        let current = self.store.get_job(job.job_id).await?;
        if issue.status.is_terminal() || current.status.is_terminal() {
            self.scheduler
                .cancel_job_record(
                    job.job_id,
                    "Issue or Job finished before Team execution started",
                )
                .await?;
            guard.finish().await;
            return self.store.get_job(job.job_id).await;
        }
        crate::operations::phase(
            crate::operations::OperationPhase::Model,
            Some(job.issue_id),
            Some(job.job_id),
            None,
        )
        .await?;
        let mut cancellation = cancel_signal.clone();
        let work = self.team.run_job(&job, view, &sink, cancel_signal);
        tokio::pin!(work);
        let outcome = tokio::select! {
            outcome = &mut work => outcome,
            () = cancellation.cancelled() => {
                tokio::time::timeout(Duration::from_secs(2), &mut work).await
                    .unwrap_or_else(|_| Err(AgentError::InvalidInput("Team did not stop within cancellation grace period".into())))
            }
        };
        guard.finish().await;
        if let Err(error) = outcome {
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
        // The pass has been billed by now, so this is the first honest moment to check the
        // ceiling. Freezing (rather than erroring) lets the chain stop itself with `Frozen` and
        // keeps the finished pass's result.
        self.freeze_if_budget_spent().await?;
        self.store.get_job(job.job_id).await
    }

    /// Drives the investigation chain from a finished pass until it stops.
    ///
    /// The loop, per pass: a `NeedsMoreData` result captures a fresh Snapshot with the requested
    /// Probes and supersedes the Job with the next pass; a diagnosis runs the proposals through
    /// the matrix and, when the Team asked for a follow-up and every proposal has been executed
    /// or denied, dispatches the next pass over the after-Snapshot; anything else stops. Each
    /// continuation spends one unit of the pass's follow-up budget; at zero the chain stops with
    /// `BudgetExhausted` — a stalled probe request then waits in the Failed inbox, where a human
    /// can send it back upstream with a fresh budget. Anything waiting for a human (an approval,
    /// a denial, a failure) stops the chain too; an approval resumes it.
    pub async fn drive_passes(&self, job: Job) -> AgentResult<Vec<PassOutcome>> {
        self.operations
            .run("drive_passes", Box::pin(self.drive_passes_work(job)))
            .await
    }

    async fn drive_passes_work(&self, job: Job) -> AgentResult<Vec<PassOutcome>> {
        let mut passes = Vec::new();
        let mut job = job;
        loop {
            crate::operations::check()?;
            if self
                .store
                .get_issue(job.issue_id)
                .await?
                .status
                .is_terminal()
            {
                self.scheduler
                    .cancel_job_record(job.job_id, "Issue is terminal; stop the pass chain")
                    .await?;
                passes.push(PassOutcome {
                    job: self.store.get_job(job.job_id).await?,
                    actions: Vec::new(),
                    stop: PassStop::Done,
                });
                break;
            }
            let Some(result) = job.result.clone() else {
                passes.push(PassOutcome {
                    job,
                    actions: Vec::new(),
                    stop: PassStop::WaitingForHuman,
                });
                break;
            };
            match result.outcome {
                JobOutcome::NeedsMoreData => {
                    if job.follow_up_budget == 0 {
                        self.record_budget_exhausted(&job).await?;
                        passes.push(PassOutcome {
                            job,
                            actions: Vec::new(),
                            stop: PassStop::BudgetExhausted,
                        });
                        break;
                    }
                    if !self.dispatch_allowed().await {
                        passes.push(PassOutcome {
                            job,
                            actions: Vec::new(),
                            stop: PassStop::Frozen,
                        });
                        break;
                    }
                    let mut probe_ids: Vec<String> = result
                        .requested_probes
                        .iter()
                        .map(|probe| probe.probe_id.clone())
                        .collect();
                    probe_ids.sort();
                    probe_ids.dedup();
                    let mut capture = self.capture_request(SnapshotCause::AgentProbeRequest);
                    capture.requested_probe_ids = probe_ids;
                    capture.parent_snapshot_id = Some(job.base_snapshot_id());
                    let brief = self.operate_brief(
                        self.pass_history(job.issue_id).await?,
                        job.follow_up_budget - 1,
                    );
                    let next = self
                        .scheduler
                        .resnapshot_and_supersede(
                            job.job_id,
                            capture,
                            PROFILE_OPERATE_READONLY,
                            brief,
                        )
                        .await?;
                    let view = self
                        .store
                        .get_artifact(next.snapshot_view.artifact_id)
                        .await?;
                    passes.push(PassOutcome {
                        job: self.store.get_job(job.job_id).await?,
                        actions: Vec::new(),
                        stop: PassStop::Superseded,
                    });
                    job = self.run_team(next, &view).await?;
                }
                JobOutcome::Solved | JobOutcome::DiagnosisOnly => {
                    let actions = self.run_proposals(&job).await?;
                    if let Some(stop) = self.follow_up_stop(&job, &actions).await? {
                        passes.push(PassOutcome { job, actions, stop });
                        break;
                    }
                    let (next, view) = self.dispatch_follow_up(&job, &actions).await?;
                    passes.push(PassOutcome {
                        job: job.clone(),
                        actions,
                        stop: PassStop::Continued,
                    });
                    job = self.run_team(next, &view).await?;
                }
                JobOutcome::Failed => {
                    passes.push(PassOutcome {
                        job,
                        actions: Vec::new(),
                        stop: PassStop::Failed,
                    });
                    break;
                }
                JobOutcome::NeedsHuman | JobOutcome::OptionsReady | JobOutcome::Blocked => {
                    passes.push(PassOutcome {
                        job,
                        actions: Vec::new(),
                        stop: PassStop::WaitingForHuman,
                    });
                    break;
                }
            }
        }
        Ok(passes)
    }

    /// Why the chain must stop after this pass's proposals were decided — or `None` when a
    /// follow-up pass should be dispatched now.
    async fn follow_up_stop(
        &self,
        job: &Job,
        actions: &[ActionRun],
    ) -> AgentResult<Option<PassStop>> {
        let requested = job
            .result
            .as_ref()
            .is_some_and(|result| result.follow_up_requested);
        if actions
            .iter()
            .any(|action| action.status == ActionStatus::WaitingForHuman)
        {
            return Ok(Some(PassStop::WaitingForHuman));
        }
        if actions
            .iter()
            .any(|action| action.status == ActionStatus::Ready)
        {
            return Ok(Some(PassStop::Frozen));
        }
        if actions
            .iter()
            .any(|action| action.status == ActionStatus::WaitingForApproval)
        {
            return Ok(Some(PassStop::WaitingForApproval));
        }
        if actions.is_empty() || !requested {
            return Ok(Some(PassStop::Done));
        }
        if !actions.iter().any(action_was_executed) {
            return Ok(Some(PassStop::NothingRan));
        }
        if job.follow_up_budget == 0 {
            self.record_budget_exhausted(job).await?;
            return Ok(Some(PassStop::BudgetExhausted));
        }
        if !self.dispatch_allowed().await {
            return Ok(Some(PassStop::Frozen));
        }
        Ok(None)
    }

    /// Dispatches the follow-up pass over the newest after-Snapshot of the executed actions
    /// (or a fresh capture when no after-Snapshot exists), carrying the whole history.
    async fn dispatch_follow_up(
        &self,
        job: &Job,
        actions: &[ActionRun],
    ) -> AgentResult<(Job, Artifact)> {
        let snapshot_id = match actions
            .iter()
            .filter(|action| action_was_executed(action))
            .filter_map(|action| action.after_snapshot_id.map(|id| (action.completed_at, id)))
            .max()
        {
            Some((_, id)) => id,
            None => {
                let mut capture = self.capture_request(SnapshotCause::AgentProbeRequest);
                capture.parent_snapshot_id = Some(job.base_snapshot_id());
                self.scheduler.request_snapshot(capture).await?.snapshot_id
            }
        };
        let brief = self
            .operate_brief(
                self.pass_history(job.issue_id).await?,
                job.follow_up_budget.saturating_sub(1),
            )
            .continuing(job.job_id);
        self.scheduler
            .dispatch_job(job.issue_id, snapshot_id, brief, PROFILE_OPERATE_READONLY)
            .await
    }

    /// Resumes a chain after a human approved one of a pass's held actions.
    ///
    /// The follow-up runs only when the pass asked for one, has budget left, every action of the
    /// pass has settled (another may still wait for approval), at least one executed, and no
    /// follow-up for the pass exists yet — so two approvals of two actions of one pass yield one
    /// follow-up, after the second.
    async fn follow_up_after_approval(
        &self,
        action: &ActionRun,
    ) -> AgentResult<Option<Vec<PassOutcome>>> {
        let follow_up_guard = self.follow_up_lock.lock().await;
        if self
            .store
            .get_issue(action.issue_id)
            .await?
            .status
            .is_terminal()
        {
            return Ok(None);
        }
        let job = self.store.get_job(action.originating_job_id).await?;
        let requested = job
            .result
            .as_ref()
            .is_some_and(|result| result.follow_up_requested);
        if !requested || job.follow_up_budget == 0 {
            return Ok(None);
        }
        let siblings: Vec<ActionRun> = self
            .store
            .list_action_runs()
            .await?
            .into_iter()
            .filter(|candidate| candidate.originating_job_id == job.job_id)
            .collect();
        if !job.proposals_materialized(&siblings)
            || siblings.iter().any(|sibling| !sibling.status.is_terminal())
            || !siblings.iter().any(action_was_executed)
        {
            return Ok(None);
        }
        let already = self
            .store
            .list_jobs()
            .await?
            .iter()
            .any(|candidate| candidate.continues_job_id == Some(job.job_id));
        if already || !self.dispatch_allowed().await {
            return Ok(None);
        }
        let (next, view) = self.dispatch_follow_up(&job, &siblings).await?;
        drop(follow_up_guard);
        let next = self.run_team(next, &view).await?;
        Ok(Some(self.drive_passes(next).await?))
    }

    /// Whether the Scheduler currently accepts new Jobs.
    async fn dispatch_allowed(&self) -> bool {
        self.scheduler.mode().await == SchedulerMode::Running
    }

    /// Records that a pass wanted to continue when no automatic pass was left.
    async fn record_budget_exhausted(&self, job: &Job) -> AgentResult<()> {
        self.store
            .append_event(
                crate::domain::NewEvent::new(
                    "top-scheduler",
                    "scheduler.pass_budget_exhausted",
                    tr!(
                        format!(
                            "Pass {} wanted to continue, but the automatic-pass budget ({}) is \
                             spent; a human continues from the inbox",
                            job.pass_number(),
                            self.pass_policy().max_auto_passes
                        ),
                        format!(
                            "第 {} 轮希望继续，但自动轮次预算（{}）已用尽；请由人工从收件箱继续",
                            job.pass_number(),
                            self.pass_policy().max_auto_passes
                        )
                    ),
                )
                .with_issue(job.issue_id)
                .with_job(job.job_id),
            )
            .await?;
        Ok(())
    }

    /// Renders every earlier pass on the Issue, with its actions and their sanitized evidence,
    /// as the history the next pass carries.
    pub async fn pass_history(&self, issue_id: IssueId) -> AgentResult<Vec<PassRecord>> {
        let actions: Vec<ActionRun> = self
            .store
            .list_action_runs()
            .await?
            .into_iter()
            .filter(|action| action.issue_id == issue_id)
            .collect();
        let mut history = Vec::new();
        for job in self
            .store
            .list_jobs()
            .await?
            .into_iter()
            .filter(|job| job.issue_id == issue_id)
        {
            let mut records = Vec::new();
            for action in actions
                .iter()
                .filter(|action| action.originating_job_id == job.job_id)
            {
                records.push(PassActionRecord {
                    action_run_id: action.action_run_id,
                    runbook_id: action.runbook_id.clone(),
                    target_ids: action.target_ids.clone(),
                    arguments: action.arguments.clone(),
                    status: action.status,
                    approval: action.approval,
                    dry_run: action.dry_run,
                    denial_reason: action.denial.as_ref().map(|denial| match &denial.comment {
                        Some(comment) => format!("{} — {comment}", denial.reason),
                        None => denial.reason.clone(),
                    }),
                    execution_summary: action.execution_summary.clone(),
                    human_intervention: action.human_intervention.clone(),
                    verification_summary: action.verification_summary.clone(),
                    verification_evidence: action.verification_evidence,
                    evidence: self.execution_evidence(action).await,
                });
            }
            let result = job.result.as_ref();
            history.push(PassRecord {
                job_id: job.job_id,
                supersedes_job_id: job.supersedes_job_id,
                revises_job_id: job.revises_job_id,
                continues_job_id: job.continues_job_id,
                outcome: result.map(|result| result.outcome),
                summary: result
                    .map(|result| result.summary.clone())
                    .unwrap_or_default(),
                unresolved_questions: result
                    .map(|result| result.unresolved_questions.clone())
                    .unwrap_or_default(),
                requested_probes: result
                    .map(|result| result.requested_probes.clone())
                    .unwrap_or_default(),
                actions: records,
                created_at: job.created_at,
            });
        }
        Ok(history)
    }

    /// Runs every action the Job proposed through the authority matrix.
    ///
    /// Each proposal becomes an ActionRun whose approval the matrix decides. `auto` actions are
    /// executed and verified immediately; `approve` actions wait in the Permission Request inbox
    /// (see `approve_action`); `deny` actions are cancelled with the rule's reason on them and
    /// wait in the Permission Denied inbox.
    pub async fn run_proposals(&self, job: &Job) -> AgentResult<Vec<ActionRun>> {
        self.operations
            .run("run_proposals", Box::pin(self.run_proposals_work(job)))
            .await
    }

    async fn run_proposals_work(&self, job: &Job) -> AgentResult<Vec<ActionRun>> {
        let proposals: Vec<ActionProposal> = job
            .result
            .as_ref()
            .map(|result| result.proposed_actions.clone())
            .unwrap_or_default();
        let mut actions = Vec::with_capacity(proposals.len());
        for proposal in proposals {
            crate::operations::check()?;
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
            // A later proposal may retry an earlier failure and needs its own before-Snapshot.
            // Reconciliation sees the full proposal list, so a partial batch cannot resolve.
            if action.status == ActionStatus::Ready {
                actions.push(self.execute_and_verify(action.action_run_id).await?);
            } else {
                actions.push(action);
            }
        }
        self.scheduler.reconcile_issue(job.issue_id).await?;
        Ok(actions)
    }

    /// Executes a `Ready` action and verifies it, or leaves it durably queued while frozen.
    /// Concurrent callers for the same action observe its current result without replaying it.
    pub async fn execute_and_verify(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.operations
            .run(
                "execute_and_verify",
                Box::pin(self.execute_and_verify_work(action_run_id)),
            )
            .await
    }

    async fn execute_and_verify_work(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        let lock = self
            .execution_locks
            .lock()
            .await
            .entry(action_run_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _execution = lock.lock().await;
        let current = self.store.get_action_run(action_run_id).await?;
        if current.status != ActionStatus::Ready {
            return Ok(current);
        }
        let action = match self.scheduler.execute_action(action_run_id).await {
            Ok(action) => action,
            Err(AgentError::SchedulerFrozen { .. }) => {
                return self.store.get_action_run(action_run_id).await;
            }
            Err(error) => return Err(error),
        };
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

    /// Records a human approval and executes it when allowed. During a full freeze the Ready
    /// action remains in the execution queue; resuming drains that queue without re-approval.
    ///
    /// When the proposing pass asked for a follow-up and this approval settled its last held
    /// action, the chain resumes: the follow-up pass runs before this returns (its Job is in
    /// the store and the event stream; the approved action is what is returned).
    pub async fn approve_action(
        &self,
        action_run_id: ActionRunId,
        approved_by: &str,
    ) -> AgentResult<ActionRun> {
        self.operations
            .run(
                "approve_action",
                Box::pin(self.approve_action_work(action_run_id, approved_by)),
            )
            .await
    }

    async fn approve_action_work(
        &self,
        action_run_id: ActionRunId,
        approved_by: &str,
    ) -> AgentResult<ActionRun> {
        self.scheduler
            .approve_action(action_run_id, approved_by)
            .await?;
        let action = self.execute_and_verify(action_run_id).await?;
        self.follow_up_after_approval(&action).await?;
        Ok(action)
    }

    /// Resumes dispatch and drains the durable execution queue in creation order. Each start
    /// rechecks freeze mode, so a new full freeze stops the drain after the admitted action.
    pub async fn resume(&self) -> AgentResult<()> {
        let _drain = self.queue_lock.lock().await;
        self.scheduler.resume().await?;
        self.intake_ready.notify_one();
        let archived = self.scheduler.archived_issue_ids().await?;
        let queued: Vec<_> = self
            .store
            .list_action_runs()
            .await?
            .into_iter()
            .filter(|action| {
                action.status == ActionStatus::Ready && !archived.contains(&action.issue_id)
            })
            .collect();
        for pending in queued {
            let action = self.execute_and_verify(pending.action_run_id).await?;
            if action.status == ActionStatus::Ready {
                break;
            }
            self.follow_up_after_approval(&action).await?;
        }
        Ok(())
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
        self.operations
            .run(
                "review_action",
                Box::pin(self.review_action_work(action_run_id, reviewer, decision, comment)),
            )
            .await
    }

    async fn review_action_work(
        &self,
        action_run_id: ActionRunId,
        reviewer: &str,
        decision: InboxDecision,
        comment: Option<String>,
    ) -> AgentResult<ReviewOutcome<ActionRun>> {
        let _serialized =
            crate::operations::cancellable(async { Ok(self.review_lock.lock().await) }).await?;
        let action = self.store.get_action_run(action_run_id).await?;
        if !action.needs_review() {
            return Err(AgentError::InvalidInput(format!(
                "ActionRun `{action_run_id}` is not in the inbox"
            )));
        }
        let origin = if let Some(reason) = &action.human_intervention {
            FeedbackOrigin::BlockedAction {
                action_run_id,
                runbook_id: action.runbook_id.clone(),
                target_ids: action.target_ids.clone(),
                reason: reason.clone(),
            }
        } else {
            match &action.denial {
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
            }
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
        self.operations
            .run(
                "review_job",
                Box::pin(self.review_job_work(job_id, reviewer, decision, comment)),
            )
            .await
    }

    async fn review_job_work(
        &self,
        job_id: JobId,
        reviewer: &str,
        decision: InboxDecision,
        comment: Option<String>,
    ) -> AgentResult<ReviewOutcome<Job>> {
        let _serialized =
            crate::operations::cancellable(async { Ok(self.review_lock.lock().await) }).await?;
        let job = self.store.get_job(job_id).await?;
        if !job.needs_review() {
            return Err(AgentError::InvalidInput(format!(
                "Job `{job_id}` is not in the Failed Job inbox"
            )));
        }
        let summary = job
            .result
            .as_ref()
            .map(|result| result.summary.clone())
            .unwrap_or_else(|| tr!("no result was recorded", "未记录任何结果").to_string());
        let origin = if job.status == JobStatus::NeedsResnapshot {
            FeedbackOrigin::StalledJob {
                job_id,
                summary,
                requested_probe_ids: job
                    .result
                    .as_ref()
                    .map(|result| {
                        result
                            .requested_probes
                            .iter()
                            .map(|probe| probe.probe_id.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        } else {
            FeedbackOrigin::FailedJob { job_id, summary }
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
                let mut passes = self.drive_passes(job).await?.into_iter();
                let first = passes
                    .next()
                    .expect("drive_passes returns at least the pass it was given");
                Ok(ReviewOutcome {
                    reviewed,
                    revision: Some(Revision {
                        job: first.job,
                        actions: first.actions,
                        follow_ups: passes.collect(),
                    }),
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

    /// Bundles an Issue with its passes, actions, Snapshots, Artifacts, and events into one
    /// document; see [`crate::session`].
    pub async fn export_session(
        &self,
        issue_id: IssueId,
        exported_by: &str,
    ) -> AgentResult<SessionBundle> {
        session::export_session(
            self.store.as_ref(),
            &self.artifacts,
            issue_id,
            exported_by,
            &self.topology.deployment.name,
        )
        .await
    }

    /// Loads a session file as a read-only archive; see [`crate::session`].
    pub async fn import_session(
        &self,
        bundle: SessionBundle,
        imported_by: &str,
    ) -> AgentResult<ImportSummary> {
        session::import_session(self.store.as_ref(), &self.artifacts, bundle, imported_by).await
    }

    /// Recovers control state after a restart, verifying interrupted actions against a fresh
    /// Snapshot; see `TopScheduler::recover_with`.
    pub async fn recover(&self) -> AgentResult<RecoverySummary> {
        let _reviews = self.snapshot_review_lock.lock().await;
        let summary = self
            .scheduler
            .recover_with(Some(self.capture_request(SnapshotCause::AfterAction)))
            .await?;
        // Reviews have no Job to recover. End any old in-flight marker explicitly so a
        // controller with periodic collection disabled does not display a permanent spinner.
        let mut unfinished = HashMap::new();
        for event in self.store.list_events().await? {
            let Ok(id) = serde_json::from_value::<SnapshotId>(event.payload["snapshot_id"].clone())
            else {
                continue;
            };
            match event.kind.as_str() {
                "snapshot_judge.review_started" => {
                    unfinished.insert(id, (event.sequence, event.payload));
                }
                "snapshot_judge.review_completed" | "snapshot_judge.review_failed" => {
                    unfinished.remove(&id);
                }
                _ => {}
            }
        }
        let mut unfinished: Vec<_> = unfinished.into_values().collect();
        unfinished.sort_by_key(|(sequence, _)| *sequence);
        for (_, mut payload) in unfinished {
            let reason = tr!(
                "Snapshot review was interrupted by a controller restart; the saved Snapshot is retained. Capture again to review current evidence.",
                "快照审查被控制器重启中断，已保存的快照仍保留；再次采集可审查当前证据。"
            );
            payload["status"] = json!("failed");
            payload["summary"] = json!(reason);
            payload["error"] = json!(reason);
            payload["updated_at"] = json!(Utc::now());
            self.store
                .append_event(
                    crate::domain::NewEvent::new(
                        "snapshot-judge",
                        "snapshot_judge.review_failed",
                        reason,
                    )
                    .with_payload(payload),
                )
                .await?;
        }
        Ok(summary)
    }

    /// Dispatches the revising Job for upstream feedback over a fresh Snapshot, with the whole
    /// history and a fresh automatic-pass budget: a human's decision starts a new chain.
    async fn dispatch_revision(
        &self,
        issue_id: IssueId,
        snapshot_id: SnapshotId,
        revises_job_id: JobId,
        feedback: Vec<HumanFeedback>,
    ) -> AgentResult<(Job, Artifact)> {
        let history = self.pass_history(issue_id).await?;
        self.scheduler
            .dispatch_job(
                issue_id,
                snapshot_id,
                self.operate_brief(history, self.fresh_follow_up_budget())
                    .revising(revises_job_id, feedback),
                PROFILE_OPERATE_READONLY,
            )
            .await
    }

    /// Lists every ActionRun in creation order.
    pub async fn list_actions(&self) -> AgentResult<Vec<ActionRun>> {
        self.store.list_action_runs().await
    }

    /// Projects closure notes from events, including comments written before this API field
    /// existed. No migration or rewrite of the original Issue records is required.
    pub async fn issue_records(&self) -> AgentResult<Vec<IssueRecord>> {
        let mut closures = HashMap::new();
        for event in self.store.list_events().await? {
            if event.kind != "human.issue_closed" {
                continue;
            }
            if let (Some(id), Some(outcome), Some(closed_by)) = (
                event.issue_id,
                event.payload["closure"].as_str(),
                event.payload["closed_by"].as_str(),
            ) {
                closures.insert(
                    id,
                    IssueClosureRecord {
                        outcome: outcome.to_string(),
                        closed_by: closed_by.to_string(),
                        comment: event.payload["comment"].as_str().map(str::to_string),
                        closed_at: event.occurred_at,
                    },
                );
            }
        }
        Ok(self
            .store
            .list_issues()
            .await?
            .into_iter()
            .map(|issue| IssueRecord {
                closure: closures.remove(&issue.issue_id),
                issue,
            })
            .collect())
    }

    /// Sends a substantive human comment into a new pass over fresh evidence. The caller must
    /// name the pass it saw, so a double click or stale browser cannot silently create two turns.
    pub async fn feedback_issue(
        &self,
        issue_id: IssueId,
        expected_job_id: JobId,
        reviewer: &str,
        comment: String,
    ) -> AgentResult<Revision> {
        self.operations
            .run(
                "feedback_issue",
                Box::pin(self.feedback_issue_work(issue_id, expected_job_id, reviewer, comment)),
            )
            .await
    }

    async fn feedback_issue_work(
        &self,
        issue_id: IssueId,
        expected_job_id: JobId,
        reviewer: &str,
        comment: String,
    ) -> AgentResult<Revision> {
        let _serialized =
            crate::operations::cancellable(async { Ok(self.review_lock.lock().await) }).await?;
        let comment = comment.trim().to_string();
        if comment.is_empty() {
            return Err(AgentError::InvalidInput(
                tr!("Feedback comment must not be empty", "反馈内容不能为空").into(),
            ));
        }
        self.refuse_if_budget_spent("continue an Issue with human feedback")
            .await?;
        let item = self.inbox().await?.waiting_issues.into_iter()
            .find(|item| item.issue.issue_id == issue_id)
            .ok_or_else(|| AgentError::InvalidInput(
                tr!("Issue is not awaiting feedback; handle its pending decision or refresh its state", "此问题当前不接受反馈，请先处理待办决策或刷新状态").into()
            ))?;
        if item.job.job_id != expected_job_id {
            return Err(AgentError::InvalidInput(
                tr!(
                    "This feedback refers to an older pass; refresh first",
                    "这条反馈对应旧轮次，请刷新后重试"
                )
                .into(),
            ));
        }
        let mut feedback = item.job.feedback.clone();
        let note = HumanFeedback::new(
            FeedbackOrigin::IssueComment {
                job_id: expected_job_id,
                summary: item
                    .job
                    .result
                    .as_ref()
                    .map(|r| r.summary.clone())
                    .unwrap_or_default(),
            },
            reviewer,
            Some(comment.clone()),
        );
        feedback.push(note.clone());
        let snapshot = self.capture(SnapshotCause::HumanFeedback).await?;
        let (job, view) = self
            .dispatch_revision(issue_id, snapshot.snapshot_id, expected_job_id, feedback)
            .await?;
        self.store.append_event(crate::domain::NewEvent::new(
            "human", "human.issue_feedback", format!("{reviewer}: {comment}"),
        ).with_issue(issue_id).with_job(job.job_id)
            .with_payload(json!({ "feedback": note, "previous_job_id": expected_job_id, "job_id": job.job_id }))
            .with_trust(crate::domain::ContentTrust::Mixed)).await?;
        let job = self.run_team(job, &view).await?;
        let mut passes = self.drive_passes(job).await?.into_iter();
        let first = passes
            .next()
            .expect("drive_passes includes its initial pass");
        Ok(Revision {
            job: first.job,
            actions: first.actions,
            follow_ups: passes.collect(),
        })
    }

    /// Computes the inbox from the store.
    pub async fn inbox(&self) -> AgentResult<Inbox> {
        let mut inbox = Inbox::default();
        // An imported archive's open items are history: nobody here can decide them.
        let archived = self.scheduler.archived_issue_ids().await?;
        for action in self.store.list_action_runs().await? {
            if archived.contains(&action.issue_id) {
                continue;
            }
            if action.status == ActionStatus::Ready {
                inbox.queued_actions.push(action);
            } else if action.status == ActionStatus::WaitingForApproval {
                inbox.permission_requests.push(action);
            } else if action.status == ActionStatus::WaitingForHuman && action.review.is_none() {
                inbox.blocked_actions.push(action);
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
            .filter(|job| !archived.contains(&job.issue_id))
            .filter(Job::needs_review)
            .collect();
        // Keep approval and failure decisions in their existing buckets. A completed
        // diagnosis with no such decision must still offer a way to continue the Issue.
        let blocked: HashSet<_> = inbox
            .queued_actions
            .iter()
            .chain(&inbox.permission_requests)
            .chain(&inbox.permission_denied)
            .chain(&inbox.failed_actions)
            .chain(&inbox.blocked_actions)
            .map(|action| action.issue_id)
            .chain(inbox.failed_jobs.iter().map(|job| job.issue_id))
            .collect();
        let jobs = self.store.list_jobs().await?;
        for issue in self.store.list_issues().await? {
            if issue.is_archived()
                || issue.status != crate::domain::IssueStatus::WaitingForHuman
                || blocked.contains(&issue.issue_id)
            {
                continue;
            }
            if let Some(job) = jobs
                .iter()
                .filter(|job| job.issue_id == issue.issue_id)
                .max_by_key(|job| (job.created_at, job.job_id))
                .filter(|job| job.status == JobStatus::Completed)
            {
                inbox.waiting_issues.push(WaitingIssue {
                    issue,
                    job: job.clone(),
                });
            }
        }
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
            if let Some(reason) = &action.human_intervention {
                let _ = writeln!(out, "    awaiting human: {reason}");
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

    /// Renders the passes of an investigation chain as operator-facing text.
    pub fn render_passes(passes: &[PassOutcome], dry_run: bool) -> String {
        let mut out = String::new();
        for (index, pass) in passes.iter().enumerate() {
            let _ = writeln!(
                out,
                "\npass {} · job {} · status {:?}{}{}",
                index + 1,
                pass.job.job_id,
                pass.job.status,
                pass.job
                    .supersedes_job_id
                    .map(|id| format!(" · supersedes {id}"))
                    .unwrap_or_default(),
                pass.job
                    .continues_job_id
                    .map(|id| format!(" · follows {id}"))
                    .unwrap_or_default(),
            );
            if let Some(result) = &pass.job.result {
                let _ = writeln!(out, "outcome: {:?} — {}", result.outcome, result.summary);
                for probe in &result.requested_probes {
                    let _ = writeln!(
                        out,
                        "probe:   {} on {} — {}",
                        probe.probe_id,
                        probe.target_ids.join(", "),
                        probe.reason
                    );
                }
                for question in &result.unresolved_questions {
                    let _ = writeln!(out, "open:    {question}");
                }
            }
            if !pass.actions.is_empty() {
                let _ = writeln!(out, "actions:");
                out.push_str(&Self::render_actions(&pass.actions, dry_run));
            }
            let _ = writeln!(out, "then:    {:?}", pass.stop);
        }
        out
    }

    /// Renders the inbox as operator-facing text.
    pub fn render_inbox(inbox: &Inbox) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "awaiting human feedback ({}):",
            inbox.waiting_issues.len()
        );
        for item in &inbox.waiting_issues {
            let _ = writeln!(
                out,
                "  Issue {} · Job {} · {}",
                item.issue.issue_id, item.job.job_id, item.issue.title
            );
        }
        let _ = writeln!(
            out,
            "actions needing human implementation ({}):",
            inbox.blocked_actions.len()
        );
        out.push_str(&Self::render_actions(&inbox.blocked_actions, false));
        let _ = writeln!(out, "execution queue ({}):", inbox.queued_actions.len());
        for action in &inbox.queued_actions {
            let _ = writeln!(
                out,
                "  {}  {} on {} — queued; resumes when execution is allowed ({:?})",
                action.action_run_id,
                action.runbook_id,
                action.target_ids.join(","),
                action.approval
            );
        }
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

/// Whether an ActionRun reached the Platform: it ran (well or badly) rather than being denied
/// or still waiting.
fn action_was_executed(action: &ActionRun) -> bool {
    matches!(
        action.status,
        ActionStatus::Succeeded | ActionStatus::Failed | ActionStatus::VerificationFailed
    )
}
