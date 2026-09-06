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
//!
//! The runner also drives the **investigation loop**: a chain of passes on one Issue, each pass
//! one Team run over one immutable Snapshot. A pass that needs more evidence ends with a probe
//! request and is superseded by a pass over a fresh Snapshot; a pass whose proposals ran can ask
//! for a follow-up pass over the after-Snapshot to check their effect and decide what comes
//! next. Every later pass carries the whole history (`earlier_passes`) in its View. The chain is
//! bounded by the automatic-pass budget, stops whenever something waits for a human (an
//! approval, a denial, a failure), and resumes — with a fresh budget — when a human approves an
//! action or sends an item back upstream.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use broccoli_agent_harness::{AgentConfig as HarnessBudget, ModelClient};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast, watch};

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
}

#[async_trait]
impl TeamCallbackSink for SchedulerSink {
    /// Every delivery is a `handle_callback` call, so ordering and rejection rules apply as-is.
    async fn deliver(&self, callback: TeamCallback) -> AgentResult<()> {
        // Nobody watching is the normal case, and never a reason to fail a callback.
        let _ = self.watchers.send(callback.clone());
        self.scheduler.handle_callback(callback).await.map(|_| ())
    }
}

/// A Team run in flight, and the handle that stops it.
struct RunningJob {
    issue_id: IssueId,
    cancel: CancelHandle,
    started_at: DateTime<Utc>,
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
    /// Further passes the revising Job's chain ran (probe requests, follow-ups), in order.
    #[serde(default)]
    pub follow_ups: Vec<PassOutcome>,
}

/// Fully wired control plane over one data directory and one topology.
pub struct SliceRunner {
    topology: DeploymentTopology,
    store: Arc<FileStateStore>,
    scheduler: Arc<TopScheduler>,
    team: Box<dyn AgentTeamPort>,
    team_label: String,
    artifacts: FileArtifactStore,
    /// Everything that may change while the control plane runs; see [`crate::settings`].
    settings: SharedSettings,
    /// The periodic capture cadence, for the schedule task to follow changes.
    capture_interval: watch::Sender<Duration>,
    /// Team runs in flight, by Job, with the handle that stops each one.
    running: Mutex<HashMap<JobId, RunningJob>>,
    /// Live Team callbacks, for a caller that wants to watch a pass it is blocked on.
    watchers: broadcast::Sender<TeamCallback>,
    /// Serializes review-plus-dispatch so two reviewers of one item cannot both dispatch a
    /// revision; the store's compare-and-set catches what slips past process boundaries.
    review_lock: Mutex<()>,
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
            topology,
            store,
            scheduler,
            team,
            team_label,
            artifacts: artifacts_for_api,
            settings,
            capture_interval: watch::channel(Duration::from_secs(DEFAULT_SNAPSHOT_INTERVAL_SECS)).0,
            running: Mutex::new(HashMap::new()),
            watchers: broadcast::channel(256).0,
            review_lock: Mutex::new(()),
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
        let Some(running) = self.running.lock().await.get(&job_id).map(|running| {
            running.cancel.cancel();
            running.issue_id
        }) else {
            return Ok(false);
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
        let job_ids: Vec<JobId> = self.running.lock().await.keys().copied().collect();
        let mut stopped = 0;
        for job_id in job_ids {
            if self.cancel_pass(job_id, by).await? {
                stopped += 1;
            }
        }
        Ok(stopped)
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
    /// Observation only — it dispatches nothing, so it runs in every Scheduler mode, frozen
    /// included, and the consoles keep a fresh picture of a deployment nobody is allowed to
    /// touch. The only mode it sits out is `Recovering`, while the Store is being reconciled.
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

    /// Captures, persists, and returns a Snapshot.
    pub async fn capture(&self, cause: SnapshotCause) -> AgentResult<Snapshot> {
        self.scheduler
            .request_snapshot(self.capture_request(cause))
            .await
    }

    /// Accepts a human report, dispatches the first Operate pass over the report-time Snapshot,
    /// runs the Team, and returns the finished Job with its Issue. Proposed actions are not run
    /// here, and the chain is not continued; see `drive_passes` (or `run_proposals` for the
    /// single-pass flow).
    pub async fn handle_report(&self, report: HumanReport) -> AgentResult<(Issue, Job)> {
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
        let sink = SchedulerSink {
            scheduler: self.scheduler.clone(),
            watchers: self.watchers.clone(),
        };
        // The handle lives in the registry for as long as the run does, which is what makes an
        // interruption reachable: a console asks for the Job by ID and the Team stops at its next
        // step boundary. Held locally, as it was before, nothing could ever stop a run.
        let (cancel_handle, cancel_signal) = cancel_pair();
        self.running.lock().await.insert(
            job.job_id,
            RunningJob {
                issue_id: job.issue_id,
                cancel: cancel_handle,
                started_at: Utc::now(),
            },
        );
        let outcome = self.team.run_job(&job, view, &sink, cancel_signal).await;
        self.running.lock().await.remove(&job.job_id);
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
        let mut passes = Vec::new();
        let mut job = job;
        loop {
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
        if actions.is_empty() || !requested {
            return Ok(Some(PassStop::Done));
        }
        if actions
            .iter()
            .any(|action| action.status == ActionStatus::WaitingForApproval)
        {
            return Ok(Some(PassStop::WaitingForApproval));
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
        if siblings.iter().any(|sibling| !sibling.status.is_terminal())
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
    ///
    /// When the proposing pass asked for a follow-up and this approval settled its last held
    /// action, the chain resumes: the follow-up pass runs before this returns (its Job is in
    /// the store and the event stream; the approved action is what is returned).
    pub async fn approve_action(
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
        self.scheduler
            .recover_with(Some(self.capture_request(SnapshotCause::AfterAction)))
            .await
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

    /// Computes the inbox from the store.
    pub async fn inbox(&self) -> AgentResult<Inbox> {
        let mut inbox = Inbox::default();
        // An imported archive's open items are history: nobody here can decide them.
        let archived = self.scheduler.archived_issue_ids().await?;
        for action in self.store.list_action_runs().await? {
            if archived.contains(&action.issue_id) {
                continue;
            }
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
            .filter(|job| !archived.contains(&job.issue_id))
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
