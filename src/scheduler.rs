//! Minimal readable implementation of the AI-integrated Top Scheduler.
//!
//! The Scheduler is a deterministic harness around an optional Scheduler Policy model. The harness
//! owns state machines, priorities, freeze modes, capability scope, and the EventLog; the model is
//! consulted only at fixed decision points through `SchedulerPolicyPort` and every proposal is
//! validated and clamped before it takes effect. When no policy model is wired, decision points
//! fall back to conservative deterministic defaults (defer to a human, no automatic dispatch), so
//! model downtime never breaks collection, persistence, or recovery.
//!
//! The Scheduler is also the only component that requests Snapshot captures outside the Collector's
//! own periodic schedule, and the sole gateway that moves ActionRuns to execution. Whatever it
//! refuses or sees fail is parked for a human: denials keep their reason on the ActionRun, failed
//! Jobs and actions wait for a review, and a review can send the item back upstream as a revising
//! Job that carries the human's feedback.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::domain::{
    ActionProposal, ActionRun, ActionRunId, ActionStatus, ApprovalState, Artifact, ArtifactKind,
    ContentTrust, Denial, EventRecord, FeedbackOrigin, HealthState, HumanReport, HumanReview,
    Issue, IssueCandidate, IssueId, IssueStatus, Job, JobBrief, JobId, JobOutcome, JobResult,
    JobStatus, NewEvent, PlatformOperationResult, ReviewDecision, Snapshot, SnapshotId,
    SnapshotViewRef, TeamCallback, VerificationEvidence,
};
use crate::error::{AgentError, AgentResult};
use crate::policy::{AuthorityPolicy, OperationClass, ProposalContext};
use crate::ports::{
    AgentsPlatformPort, CallbackAdviceRequest, CaptureRequest, CollectorPort, InspectionPort,
    InspectionRequest, InspectionResult, NextStep, NextStepDecision, SchedulerPolicyPort,
    SnapshotViewBuildRequest, SnapshotViewBuilderPort, StateStore, TriageDecision, TriageRequest,
};
use crate::tr;

/// The refusal every control decision gives an imported archive.
fn archived(issue_id: IssueId) -> AgentError {
    AgentError::InvalidInput(tr!(
        format!("Issue `{issue_id}` is an imported archive and is read-only"),
        format!("问题 `{issue_id}` 是导入的归档记录，只读")
    ))
}

/// Whether the Top Scheduler currently permits dispatch or execution work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerMode {
    /// Accepts problems and creates Jobs normally.
    Running,
    /// Creates no new Jobs, while running Jobs may continue, checkpoint, and execute their actions.
    DispatchFrozen,
    /// Creates no new Jobs and allows no new ActionRuns to begin.
    FullyFrozen,
    /// Reconstructing unfinished control state from the Store.
    Recovering,
}

/// Summary of what Scheduler recovery found and did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySummary {
    /// The mode a human had set last (`Running` when none was recorded). The freezes recovery
    /// itself writes are not counted: they are bookkeeping, not a decision.
    pub previous_mode: SchedulerMode,
    /// An earlier recovery reconciled interrupted work and nobody has resumed since, so the
    /// inbox still holds items no human has looked at.
    pub pending_recovery_review: bool,
    /// Issue IDs that were unfinished when recovery started.
    pub issue_ids: Vec<IssueId>,
    /// Job IDs that were unfinished when recovery started.
    pub job_ids: Vec<JobId>,
    /// ActionRun IDs that were unfinished when recovery started.
    pub action_run_ids: Vec<ActionRunId>,
    /// Jobs that were running with no Team left to run them, now `Failed` and in the inbox.
    pub interrupted_job_ids: Vec<JobId>,
    /// Actions whose execution was interrupted or never evaluated, now failed or denied and in
    /// the inbox; a human must check the machine before retrying.
    pub interrupted_action_ids: Vec<ActionRunId>,
    /// Actions that were awaiting verification and were verified now against a fresh Snapshot.
    pub verified_action_ids: Vec<ActionRunId>,
    /// Revising Jobs whose review record had not been written before the crash, now recorded.
    pub reconstructed_review_job_ids: Vec<JobId>,
    /// The mode recovery left the Scheduler in.
    pub final_mode: SchedulerMode,
}

impl RecoverySummary {
    /// Whether recovery had to change anything: a clean restart has nothing here.
    pub fn touched_anything(&self) -> bool {
        !self.interrupted_job_ids.is_empty()
            || !self.interrupted_action_ids.is_empty()
            || !self.verified_action_ids.is_empty()
            || !self.reconstructed_review_job_ids.is_empty()
    }

    /// Whether dispatch may resume without a human: the last human decision was `Running`,
    /// this recovery touched nothing, and no earlier recovery is still waiting for a look.
    pub fn is_clean_restart(&self) -> bool {
        self.previous_mode == SchedulerMode::Running
            && !self.touched_anything()
            && !self.pending_recovery_review
    }
}

/// How a human closes an Issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueClosure {
    /// The problem is fixed or was not a problem.
    Resolved,
    /// Stop working on it without claiming it is fixed.
    Cancelled,
    /// Give up: the problem stands and automatic work will not continue.
    Failed,
}

impl IssueClosure {
    fn status(self) -> IssueStatus {
        match self {
            Self::Resolved => IssueStatus::Resolved,
            Self::Cancelled => IssueStatus::Cancelled,
            Self::Failed => IssueStatus::Failed,
        }
    }
}

/// Harness-validated outcome of triaging one Issue Candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriageOutcome {
    /// The candidate became a new formal Issue.
    IssueCreated(Box<Issue>),
    /// The candidate's evidence was merged into an existing open Issue.
    MergedInto(IssueId),
    /// The candidate was discarded as noise or an unneeded duplicate.
    Rejected,
    /// No policy model is wired; the candidate was recorded and awaits human triage.
    DeferredToHuman,
}

/// Scheduler that owns global Issue, Job, priority, callback, and freeze/recovery state.
///
/// Only the `StateStore` is mandatory. The Collector, View Builder, Platform, and Policy ports are
/// wired with the `with_*` builder methods; operations that need an unwired port fail with
/// `AgentError::MissingDependency` instead of pretending an external capability exists.
pub struct TopScheduler {
    store: Arc<dyn StateStore>,
    collector: Option<Arc<dyn CollectorPort>>,
    view_builder: Option<Arc<dyn SnapshotViewBuilderPort>>,
    platform: Option<Arc<dyn AgentsPlatformPort>>,
    policy: Option<Arc<dyn SchedulerPolicyPort>>,
    authority: AuthorityPolicy,
    mode: RwLock<SchedulerMode>,
}

impl TopScheduler {
    /// Creates a Scheduler in `Running` state using the given StateStore.
    ///
    /// This function only stores dependencies and does not restore historical state automatically.
    /// Process startup should call `recover` explicitly so a human can inspect the result and decide
    /// when to `resume`.
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self {
            store,
            collector: None,
            view_builder: None,
            platform: None,
            policy: None,
            authority: AuthorityPolicy::default(),
            mode: RwLock::new(SchedulerMode::Running),
        }
    }

    /// Wires the Collector used for Scheduler-requested Snapshot captures.
    pub fn with_collector(mut self, collector: Arc<dyn CollectorPort>) -> Self {
        self.collector = Some(collector);
        self
    }

    /// Wires the Snapshot View Builder used to sanitize Snapshots for Jobs.
    pub fn with_view_builder(mut self, view_builder: Arc<dyn SnapshotViewBuilderPort>) -> Self {
        self.view_builder = Some(view_builder);
        self
    }

    /// Wires the Agents Platform through which ActionRuns execute.
    pub fn with_platform(mut self, platform: Arc<dyn AgentsPlatformPort>) -> Self {
        self.platform = Some(platform);
        self
    }

    /// Wires the Scheduler Policy model consulted at fixed decision points.
    ///
    /// Without a policy, decision points fall back to conservative deterministic defaults.
    pub fn with_policy(mut self, policy: Arc<dyn SchedulerPolicyPort>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Replaces the default action authority policy (the encoded OD-2 matrix with empty lists).
    pub fn with_authority(mut self, authority: AuthorityPolicy) -> Self {
        self.authority = authority;
        self
    }

    /// The action authority policy in force.
    pub fn authority(&self) -> &AuthorityPolicy {
        &self.authority
    }

    /// Returns the current Scheduler mode.
    ///
    /// This method reads only in-process scheduling state and accesses no external system.
    pub async fn mode(&self) -> SchedulerMode {
        *self.mode.read().await
    }

    /// Captures and persists a Snapshot on the Scheduler's behalf.
    ///
    /// This is the control edge from the Scheduler back to the Collector. It serves human-report
    /// intake, Team probe requests, and before/after ActionRun verification; periodic collection
    /// remains the Collector's own schedule.
    pub async fn request_snapshot(&self, request: CaptureRequest) -> AgentResult<Snapshot> {
        let collector = self.require_collector("request_snapshot")?;
        let snapshot = collector.capture_snapshot(request.clone()).await?;
        self.store.insert_snapshot(snapshot.clone()).await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "snapshot.captured",
                    tr!(
                        "The Scheduler requested and persisted a Snapshot",
                        "调度器请求并持久化了一份快照"
                    ),
                )
                .with_payload(json!({
                    "snapshot_id": snapshot.snapshot_id,
                    "request": request,
                })),
            )
            .await?;
        Ok(snapshot)
    }

    /// Accepts a human report and creates an Issue that defaults to the highest priority.
    ///
    /// The report's Snapshot must already be persisted; `accept_human_report_with_capture` obtains
    /// one first. The method records an event containing untrusted human text before creating the
    /// Issue. A future database implementation must insert the event and Issue in one transaction.
    pub async fn accept_human_report(
        &self,
        report: HumanReport,
        snapshot_id: SnapshotId,
    ) -> AgentResult<Issue> {
        self.store.get_snapshot(snapshot_id).await?;

        let report_payload = serde_json::to_value(&report)?;
        let report_event = self
            .store
            .append_event(
                NewEvent::new(
                    "human",
                    "human.issue_reported",
                    tr!("A human reported a problem", "人工上报了一个问题"),
                )
                .with_payload(report_payload)
                .with_artifacts(report.attachment_artifact_ids.clone())
                .with_trust(ContentTrust::UntrustedExternal),
            )
            .await?;

        let issue = Issue::from_human_report(report, snapshot_id, report_event.event_id);
        self.store.insert_issue(issue.clone()).await?;
        self.record_issue_created(&issue).await?;
        Ok(issue)
    }

    /// Captures a fresh Snapshot for a human report, then accepts the report against it.
    ///
    /// This is the normal intake path when no sufficiently recent Snapshot exists.
    pub async fn accept_human_report_with_capture(
        &self,
        report: HumanReport,
        capture: CaptureRequest,
    ) -> AgentResult<Issue> {
        let snapshot = self.request_snapshot(capture).await?;
        self.accept_human_report(report, snapshot.snapshot_id).await
    }

    /// Records an Issue Candidate from the Snapshot Judge without creating a formal Issue.
    ///
    /// A Candidate may contain model interpretation, so the event is recorded with mixed trust.
    /// `triage_candidate` performs deduplication and formal acceptance.
    pub async fn record_issue_candidate(
        &self,
        candidate: IssueCandidate,
    ) -> AgentResult<EventRecord> {
        self.store.get_snapshot(candidate.snapshot_id).await?;
        let payload = serde_json::to_value(&candidate)?;
        self.store
            .append_event(
                NewEvent::new(
                    "snapshot-judge",
                    "snapshot_judge.issue_candidate",
                    tr!(
                        "The Snapshot Judge proposed a potential problem",
                        "快照裁判提出了一个潜在问题"
                    ),
                )
                .with_payload(payload)
                .with_trust(ContentTrust::Mixed),
            )
            .await
    }

    /// Triages one Issue Candidate: record it, consult policy, and apply the validated decision.
    ///
    /// The policy model proposes accept/merge/reject; the harness clamps any proposed priority
    /// below `HumanTop`, verifies that a merge target exists and is still open, and records the
    /// consultation for replay. Without a policy model the candidate is recorded and deferred to a
    /// human — the conservative fallback, never automatic acceptance.
    pub async fn triage_candidate(&self, candidate: IssueCandidate) -> AgentResult<TriageOutcome> {
        let candidate_event = self.record_issue_candidate(candidate.clone()).await?;

        let Some(policy) = &self.policy else {
            self.store
                .append_event(NewEvent::new(
                    "top-scheduler",
                    "scheduler.triage_deferred",
                    tr!(
                        "No policy model is wired; the candidate awaits human triage",
                        "未接入策略模型；候选问题等待人工分诊"
                    ),
                ))
                .await?;
            return Ok(TriageOutcome::DeferredToHuman);
        };

        let request = TriageRequest {
            candidate: candidate.clone(),
            open_issues: self.live_unfinished().await?.0,
        };
        let decision = policy.triage_candidate(&request).await?;
        self.record_policy_consultation("triage_candidate", &request, &decision, None, None)
            .await?;

        match decision {
            TriageDecision::Accept { priority } => {
                let mut issue = Issue::from_candidate(candidate, candidate_event.event_id);
                issue.priority = priority.model_safe();
                self.store.insert_issue(issue.clone()).await?;
                self.record_issue_created(&issue).await?;
                Ok(TriageOutcome::IssueCreated(Box::new(issue)))
            }
            TriageDecision::MergeInto { issue_id } => {
                let mut issue = self.store.get_issue(issue_id).await?;
                if issue.status.is_terminal() {
                    return Err(AgentError::InvalidInput(format!(
                        "policy proposed merging into terminal Issue `{issue_id}`"
                    )));
                }
                issue.evidence_ids.extend(candidate.evidence_ids);
                issue.update_current_snapshot(candidate.snapshot_id);
                self.store.update_issue(issue.clone()).await?;
                self.store
                    .append_event(
                        NewEvent::new(
                            "top-scheduler",
                            "scheduler.candidate_merged",
                            tr!(
                                "The Scheduler merged a candidate into an existing Issue",
                                "调度器将候选问题并入了已有 Issue"
                            ),
                        )
                        .with_issue(issue_id)
                        .with_payload(json!({ "candidate_id": candidate.candidate_id })),
                    )
                    .await?;
                Ok(TriageOutcome::MergedInto(issue_id))
            }
            TriageDecision::Reject => {
                self.store
                    .append_event(
                        NewEvent::new(
                            "top-scheduler",
                            "scheduler.candidate_rejected",
                            tr!(
                                "The Scheduler rejected a candidate as noise or duplicate",
                                "调度器判定候选问题为噪声或重复并已拒绝"
                            ),
                        )
                        .with_payload(json!({ "candidate_id": candidate.candidate_id })),
                    )
                    .await?;
                Ok(TriageOutcome::Rejected)
            }
        }
    }

    /// Builds the sanitized Snapshot View for an Issue and dispatches a Job over it.
    ///
    /// This is the one path from "there is an Issue and a Snapshot" to "a Team has work": the
    /// View is built from the Issue's problem statement, the brief's scope, and the brief's
    /// feedback, stored as an Artifact, and bound into a running Job. The View Artifact is
    /// returned alongside the Job because it is exactly what the Team must be handed.
    pub async fn dispatch_job(
        &self,
        issue_id: IssueId,
        snapshot_id: SnapshotId,
        brief: JobBrief,
        redaction_profile: impl Into<String>,
    ) -> AgentResult<(Job, Artifact)> {
        self.ensure_dispatch_allowed("dispatch_job").await?;
        let view_builder = self.require_view_builder("dispatch_job")?;
        let issue = self.store.get_issue(issue_id).await?;
        if issue.is_archived() {
            return Err(archived(issue_id));
        }
        let snapshot = self.store.get_snapshot(snapshot_id).await?;
        if let Some(revised) = brief.revises_job_id {
            let previous = self.store.get_job(revised).await?;
            if previous.issue_id != issue_id {
                return Err(AgentError::InvalidInput(format!(
                    "Job `{revised}` belongs to Issue `{}`, not `{issue_id}`",
                    previous.issue_id
                )));
            }
        }

        let request = SnapshotViewBuildRequest {
            issue,
            brief,
            redaction_profile: redaction_profile.into(),
        };
        let built = view_builder
            .build_snapshot_view(&snapshot, &request)
            .await?;
        self.store.insert_artifact(built.artifact.clone()).await?;
        let job = self
            .create_job(issue_id, built.snapshot_view, request.brief)
            .await?;
        Ok((job, built.artifact))
    }

    /// Creates and dispatches a Job bound to an immutable Snapshot View for the given Issue.
    ///
    /// The method verifies that the Issue, canonical Snapshot, Snapshot View Artifact, and content
    /// hash all exist and agree, and moves an `Open` or `WaitingForHuman` Issue to
    /// `Investigating`. A revision (a brief with `revises_job_id`) is recorded as such so the
    /// event log shows which human review caused the new pass.
    pub async fn create_job(
        &self,
        issue_id: IssueId,
        snapshot_view: SnapshotViewRef,
        brief: JobBrief,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("create_job").await?;
        let mut issue = self.store.get_issue(issue_id).await?;
        self.validate_snapshot_view(&snapshot_view).await?;

        let revision = brief.revises_job_id.is_some();
        let mut job = Job::new(issue_id, snapshot_view, brief);
        job.transition_to(crate::domain::JobStatus::Running)?;
        self.store.insert_job(job.clone()).await?;

        if issue.can_transition_to(IssueStatus::Investigating) {
            issue.transition_to(IssueStatus::Investigating)?;
            self.store.update_issue(issue).await?;
        }

        if revision {
            self.record_job_event(
                &job,
                "scheduler.job_revised",
                tr!(
                    "The Scheduler dispatched a revising Job carrying human feedback",
                    "调度器派发了携带人工反馈的修订任务"
                ),
            )
            .await?;
        } else if job.continues_job_id.is_some() {
            let summary = tr!(
                format!(
                    "The Scheduler dispatched follow-up pass {} to check the effect of the \
                     previous pass's actions",
                    job.pass_number()
                ),
                format!(
                    "调度器派发了第 {} 轮后续任务，以检查上一轮操作的效果",
                    job.pass_number()
                )
            );
            self.record_job_event(&job, "scheduler.job_continued", summary)
                .await?;
        } else {
            self.record_job_event(
                &job,
                "scheduler.job_dispatched",
                tr!(
                    "The Scheduler created and dispatched a Job",
                    "调度器创建并派发了任务"
                ),
            )
            .await?;
        }
        Ok(job)
    }

    /// Creates a Job that replaces an older Job using a new Snapshot View.
    ///
    /// The method does not modify the old Job's Snapshot. It marks the old Job as `Superseded` and
    /// creates a new ID. The caller supplies the whole brief again, because a new Snapshot may
    /// justify a narrower scope than the old Job held. The current in-memory Store has no
    /// transactions; a future persistence implementation must commit the old Job, new Job, and
    /// event atomically.
    pub async fn supersede_job(
        &self,
        previous_job_id: JobId,
        new_snapshot_view: SnapshotViewRef,
        brief: JobBrief,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("supersede_job").await?;
        self.validate_snapshot_view(&new_snapshot_view).await?;

        let mut previous = self.store.get_job(previous_job_id).await?;
        let mut next = previous.supersede_with(new_snapshot_view, brief)?;
        next.transition_to(crate::domain::JobStatus::Running)?;
        self.store.update_job(previous).await?;
        self.store.insert_job(next.clone()).await?;
        self.record_job_event(
            &next,
            "scheduler.job_superseded",
            tr!(
                "The Scheduler created a superseding Job from a new Snapshot",
                "调度器基于新快照创建了替代任务"
            ),
        )
        .await?;
        Ok(next)
    }

    /// Serves a Team's probe request: capture a new Snapshot, build its View, and supersede the Job.
    ///
    /// This is the `NeedsMoreData` flow from the architecture document. The redaction profile and
    /// brief come from the caller (usually derived from the old Job plus the requested Probes in
    /// `capture.requested_probe_ids`).
    pub async fn resnapshot_and_supersede(
        &self,
        previous_job_id: JobId,
        capture: CaptureRequest,
        redaction_profile: impl Into<String>,
        brief: JobBrief,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("resnapshot_and_supersede")
            .await?;
        let view_builder = self.require_view_builder("resnapshot_and_supersede")?;
        let previous = self.store.get_job(previous_job_id).await?;
        let issue = self.store.get_issue(previous.issue_id).await?;

        let snapshot = self.request_snapshot(capture).await?;
        let view_request = SnapshotViewBuildRequest {
            issue,
            brief,
            redaction_profile: redaction_profile.into(),
        };
        let built = view_builder
            .build_snapshot_view(&snapshot, &view_request)
            .await?;
        self.store.insert_artifact(built.artifact).await?;

        self.supersede_job(previous_job_id, built.snapshot_view, view_request.brief)
            .await
    }

    /// Accepts an Agent Team callback and updates the Job and Issue when a final result is present.
    ///
    /// A callback for a terminal Job is rejected before anything is written, so supersession or
    /// completion cannot leave orphan callback events in the log. When `final_result` is present,
    /// the Job state machine decides the Job's next state and the owning Issue receives the
    /// corresponding lifecycle update where the Issue state machine allows it: a failed or
    /// blocked Job parks the Issue with a human, since the failed Job now sits in the Failed Job
    /// inbox. A future database implementation must place the event and both updates in one
    /// transaction.
    pub async fn handle_callback(&self, callback: TeamCallback) -> AgentResult<Job> {
        let job = self.store.get_job(callback.job_id).await?;
        if job.issue_id != callback.issue_id {
            return Err(AgentError::InvalidInput(format!(
                "callback Issue `{}` does not match the Job's Issue `{}`",
                callback.issue_id, job.issue_id
            )));
        }
        if job.status.is_terminal() {
            return Err(AgentError::InvalidInput(format!(
                "Job `{}` is already `{:?}` and accepts no further callbacks",
                job.job_id, job.status
            )));
        }

        // A forwarded transcript entry is a fact about the running pass, not a message to the
        // Scheduler: it is logged for the live trace and changes nothing.
        if let (Some(step), None) = (&callback.step, &callback.final_result) {
            self.store
                .append_event(
                    NewEvent::new("agent-team", "team.step", callback.summary.clone())
                        .with_issue(callback.issue_id)
                        .with_job(callback.job_id)
                        .with_payload(json!({ "step": step }))
                        .with_trust(ContentTrust::Mixed),
                )
                .await?;
            return Ok(job);
        }

        let payload = serde_json::to_value(&callback)?;
        self.store
            .append_event(
                NewEvent::new("agent-team", "team.callback", callback.summary.clone())
                    .with_issue(callback.issue_id)
                    .with_job(callback.job_id)
                    .with_payload(payload)
                    .with_artifacts(callback.artifact_ids.clone())
                    .with_trust(ContentTrust::Mixed),
            )
            .await?;

        // Usage is recorded before anything else the callback asks for, and as its own event:
        // the tokens were spent whatever the Scheduler decides about the result, and the
        // append-only log is the ledger every cost figure is later summed from.
        if let Some(usage) = &callback.usage {
            self.store
                .append_event(
                    NewEvent::new(
                        "agent-team",
                        crate::usage::USAGE_EVENT_KIND,
                        tr!(
                            format!(
                                "Pass {} spent {} tokens over {} model request(s)",
                                job.pass_number(),
                                usage.total_tokens(),
                                usage.requests
                            ),
                            format!(
                                "第 {} 轮消耗 {} tokens，共 {} 次模型请求",
                                job.pass_number(),
                                usage.total_tokens(),
                                usage.requests
                            )
                        ),
                    )
                    .with_issue(callback.issue_id)
                    .with_job(callback.job_id)
                    .with_payload(serde_json::to_value(usage)?),
                )
                .await?;
        }

        let Some(mut result) = callback.final_result else {
            return Ok(job);
        };
        // A Team's "solved" is a claim, not a fact. It stands only when a real remediation ran
        // on this Issue and every resource the Issue touches is Healthy in the Snapshot the
        // Team reasoned over; otherwise it is recorded as a diagnosis and a human decides.
        if result.outcome == JobOutcome::Solved
            && let Err(reason) = self.solved_is_supported(&job).await?
        {
            result.outcome = JobOutcome::DiagnosisOnly;
            result.unresolved_questions.push(tr!(
                format!(
                    "The Team reported the problem solved, but the Scheduler could not confirm \
                     it and recorded a diagnosis instead: {reason}"
                ),
                format!("团队报告问题已解决，但调度器无法确认，已改记为诊断结论：{reason}")
            ));
            self.store
                .append_event(
                    NewEvent::new(
                        "top-scheduler",
                        "scheduler.result_clamped",
                        tr!(
                            format!("A `solved` result was downgraded to a diagnosis: {reason}"),
                            format!("`solved` 结果已降级为诊断结论：{reason}")
                        ),
                    )
                    .with_issue(job.issue_id)
                    .with_job(job.job_id)
                    .with_payload(json!({ "claimed": "solved", "recorded": "diagnosis_only" })),
                )
                .await?;
        }
        let failed = result.outcome == JobOutcome::Failed;
        let failure_summary = result.summary.clone();

        let mut next = job.clone();
        next.complete(result)?;
        if let Some(usage) = callback.usage {
            next.usage = Some(usage);
        }
        self.store.update_job_if(&job, next.clone()).await?;

        if failed {
            self.store
                .append_event(
                    NewEvent::new(
                        "top-scheduler",
                        "scheduler.job_failed",
                        tr!(
                            format!("The Job failed and awaits human review: {failure_summary}"),
                            format!("任务失败，等待人工审核：{failure_summary}")
                        ),
                    )
                    .with_issue(next.issue_id)
                    .with_job(next.job_id)
                    .with_trust(ContentTrust::Mixed),
                )
                .await?;
        }
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Whether a Team's `Solved` claim on this Job is backed by the record.
    ///
    /// Two conditions, both checked by the harness and never by the model: an ActionRun on the
    /// Issue succeeded with real (not dry-run) evidence, and every resource the Issue touches —
    /// its affected resources plus every action target — is present and Healthy in the Job's
    /// base Snapshot. A report whose symptoms the probes cannot see, or a rehearsal in dry-run
    /// mode, therefore never resolves an Issue on a model's word.
    async fn solved_is_supported(&self, job: &Job) -> AgentResult<Result<(), String>> {
        let issue = self.store.get_issue(job.issue_id).await?;
        let snapshot = self.store.get_snapshot(job.base_snapshot_id()).await?;
        let actions: Vec<ActionRun> = self
            .store
            .list_action_runs()
            .await?
            .into_iter()
            .filter(|action| action.issue_id == job.issue_id)
            .collect();
        let remediated = actions.iter().any(|action| {
            action.status == ActionStatus::Succeeded
                && matches!(
                    action.verification_evidence,
                    Some(VerificationEvidence::Weak | VerificationEvidence::Strong)
                )
        });
        if !remediated {
            return Ok(Err(tr!(
                "no action on this Issue has succeeded with real evidence, so nothing was \
                 remediated; if the problem is gone on its own, a human closes the Issue",
                "该 Issue 上没有任何操作以真实证据成功完成，因此没有实际修复；若问题已自行消失，请由人工关闭"
            )
            .to_string()));
        }
        let mut touched: Vec<String> = issue.affected_resource_ids.clone();
        touched.extend(actions.iter().flat_map(|action| action.target_ids.clone()));
        touched.sort();
        touched.dedup();
        for target in &touched {
            let Some(resource) = snapshot
                .resources
                .iter()
                .find(|resource| &resource.resource_id == target)
            else {
                return Ok(Err(tr!(
                    format!("`{target}` is absent from the Job's Snapshot"),
                    format!("`{target}` 未出现在任务所依据的快照中")
                )));
            };
            if resource.health != HealthState::Healthy {
                return Ok(Err(tr!(
                    format!(
                        "`{target}` is {:?} in the Job's Snapshot, not Healthy",
                        resource.health
                    ),
                    format!(
                        "`{target}` 在任务所依据的快照中的状态为 {:?}，并非 Healthy",
                        resource.health
                    )
                )));
            }
        }
        Ok(Ok(()))
    }

    /// Runs a read-only inspection for a running Job, within its scope.
    ///
    /// This is the gateway for the architecture's "read-only scoped request": no authority
    /// decision and no approval — an inspection changes nothing — but the freeze mode is
    /// checked (`FullyFrozen` and `Recovering` refuse: a human who froze everything does not
    /// want the agent reading machines either), the Job must be running, the request must pass
    /// the same joint scope validation as a proposal and classify as non-mutating, and the
    /// Platform re-checks its half before running. Refusals are results, not errors, so the
    /// Team reads the reason and adapts; every inspection, refused or run, is an event with its
    /// output Artifact.
    pub async fn inspect(
        &self,
        job_id: JobId,
        request: InspectionRequest,
    ) -> AgentResult<InspectionResult> {
        let mode = self.mode().await;
        if matches!(mode, SchedulerMode::FullyFrozen | SchedulerMode::Recovering) {
            return Err(AgentError::SchedulerFrozen {
                mode: format!("{mode:?}"),
                operation: "inspect",
            });
        }
        let platform = self.require_platform("inspect")?;
        let job = self.store.get_job(job_id).await?;
        if job.status != JobStatus::Running {
            return Err(AgentError::InvalidInput(format!(
                "Job `{job_id}` is `{:?}`; only a running Job may inspect",
                job.status
            )));
        }
        let refusal = match self.authority.validate_scope(
            &request.runbook_id,
            &request.target_ids,
            &request.arguments,
            &job.allowed_capabilities,
            &job.allowed_target_ids,
        ) {
            Ok(class) if class.is_mutating() => Some(tr!(
                format!(
                    "`{}` (row {}, {class:?}) changes machine state; propose it as an action \
                     instead of inspecting with it",
                    request.runbook_id,
                    class.row()
                ),
                format!(
                    "`{}`（第 {} 行，{class:?}）会改变机器状态；请改为提议操作，而不是用它进行检查",
                    request.runbook_id,
                    class.row()
                )
            )),
            Ok(_) => None,
            Err((_, reason)) => Some(tr!(
                format!("scope: {reason}"),
                format!("范围校验：{reason}")
            )),
        };
        let result = match refusal {
            Some(reason) => InspectionResult {
                succeeded: false,
                dry_run: false,
                refused: Some(reason.clone()),
                summary: tr!(format!("refused: {reason}"), format!("已拒绝：{reason}")),
                output_artifact_id: None,
            },
            None => {
                let outcome = match platform.inspect(&job, &request).await {
                    Ok(outcome) => outcome,
                    Err(error) => PlatformOperationResult::new(
                        false,
                        None,
                        tr!(
                            format!(
                                "the Agents Platform failed before reporting a result: {error}"
                            ),
                            format!("Agents 平台在返回结果前出错：{error}")
                        ),
                    ),
                };
                InspectionResult {
                    succeeded: outcome.succeeded,
                    dry_run: outcome.dry_run,
                    refused: None,
                    summary: outcome.summary,
                    output_artifact_id: outcome.output_artifact_id,
                }
            }
        };
        self.store
            .append_event(
                NewEvent::new(
                    if result.refused.is_some() {
                        "top-scheduler"
                    } else {
                        "agents-platform"
                    },
                    "platform.inspection",
                    tr!(
                        format!(
                            "Inspection `{}` on {}: {}",
                            request.runbook_id,
                            request.target_ids.join(", "),
                            result.summary
                        ),
                        format!(
                            "检查 `{}`（目标 {}）：{}",
                            request.runbook_id,
                            request.target_ids.join(", "),
                            result.summary
                        )
                    ),
                )
                .with_issue(job.issue_id)
                .with_job(job_id)
                .with_payload(json!({ "request": request, "result": result }))
                .with_artifacts(result.output_artifact_id.into_iter().collect())
                .with_trust(ContentTrust::Mixed),
            )
            .await?;
        Ok(result)
    }

    /// Proposes the validated next step after a Job returned its final result.
    ///
    /// The policy model's proposal is checked against the actual `JobResult` (probe lists must be
    /// non-empty, proposal indexes must exist, questions must not be blank) and recorded for
    /// replay; an invalid proposal is an error, never silently executed. Without a policy model the
    /// deterministic fallback defers to a human. Executing the step remains the caller's decision.
    pub async fn advise_next_step(&self, job_id: JobId) -> AgentResult<NextStep> {
        let job = self.store.get_job(job_id).await?;
        let result = job.result.clone().ok_or_else(|| {
            AgentError::InvalidInput(format!("Job `{job_id}` has no final result to interpret"))
        })?;
        let issue = self.store.get_issue(job.issue_id).await?;

        let Some(policy) = &self.policy else {
            return Ok(NextStep {
                decision: NextStepDecision::AskHuman {
                    question: tr!(
                        format!(
                            "Job `{job_id}` finished with outcome `{:?}`; choose the next step",
                            result.outcome
                        ),
                        format!(
                            "任务 `{job_id}` 以 `{:?}` 结束；请选择下一步",
                            result.outcome
                        )
                    ),
                },
                rationale: tr!(
                    "no policy model is wired; deferring to a human",
                    "未接入策略模型；交由人工决定"
                )
                .to_string(),
            });
        };

        let request = CallbackAdviceRequest {
            issue,
            job: job.clone(),
            result: result.clone(),
        };
        let step = policy.advise_next_step(&request).await?;
        self.record_policy_consultation(
            "advise_next_step",
            &request,
            &step,
            Some(job.issue_id),
            Some(job_id),
        )
        .await?;

        match &step.decision {
            NextStepDecision::Resnapshot {
                requested_probe_ids,
            } if requested_probe_ids.is_empty() => Err(AgentError::InvalidInput(
                "policy proposed a resnapshot without any probes".to_string(),
            )),
            NextStepDecision::CreateActions { proposal_indexes } => {
                if proposal_indexes.is_empty() {
                    return Err(AgentError::InvalidInput(
                        "policy proposed creating actions without selecting any".to_string(),
                    ));
                }
                if let Some(bad) = proposal_indexes
                    .iter()
                    .find(|index| **index >= result.proposed_actions.len())
                {
                    return Err(AgentError::InvalidInput(format!(
                        "policy referenced proposal index {bad} but the result has {} proposals",
                        result.proposed_actions.len()
                    )));
                }
                Ok(step)
            }
            NextStepDecision::AskHuman { question } if question.trim().is_empty() => Err(
                AgentError::InvalidInput("policy asked a human an empty question".to_string()),
            ),
            _ => Ok(step),
        }
    }

    /// Converts a Team's ActionProposal into a persisted ActionRun and applies the authority matrix.
    ///
    /// The before Snapshot is captured through the Collector so every side effect has an auditable
    /// starting state, and its operation mode selects the matrix column. The idempotency key is
    /// claimed first: if another live ActionRun already holds the same key (it may yet run, is
    /// running, or succeeded) this one is denied as a duplicate before anyone is asked to approve
    /// it. Then the whole proposal — runbook, target kinds, the Job's scope and capabilities,
    /// arguments — is validated and the matrix row applies, with the repeat-rate escalation of
    /// rule 5. The returned ActionRun is already `Ready`, `WaitingForApproval`, or `Cancelled`;
    /// a denial keeps its rationale on the ActionRun and the Issue is reconciled, so the item
    /// shows in the Permission Denied inbox. Creation is refused while the Scheduler is
    /// `FullyFrozen` or `Recovering`.
    pub async fn create_action_run(
        &self,
        originating_job_id: JobId,
        proposal: ActionProposal,
        before_capture: CaptureRequest,
        idempotency_key: impl Into<String>,
    ) -> AgentResult<ActionRun> {
        self.ensure_actions_allowed("create_action_run").await?;
        let job = self.store.get_job(originating_job_id).await?;

        let before = self.request_snapshot(before_capture).await?;
        let action = ActionRun::from_proposal(
            job.issue_id,
            originating_job_id,
            job.team_kind,
            proposal,
            before.snapshot_id,
            idempotency_key,
        );
        self.store.insert_action_run(action.clone()).await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.action_created",
                    tr!(
                        "The Scheduler converted a Team proposal into an ActionRun",
                        "调度器将团队的提议转换为一个操作（ActionRun）"
                    ),
                )
                .with_issue(job.issue_id)
                .with_job(originating_job_id)
                .with_action(action.action_run_id)
                .with_payload(serde_json::to_value(&action)?),
            )
            .await?;

        // The idempotency claim is answered under the store's lock: two proposals of the same
        // intent cannot both be admitted, and a retry after a failure can.
        if let Some(holder) = self
            .store
            .claim_idempotency_key(&action.idempotency_key, action.action_run_id)
            .await?
        {
            let rationale = tr!(
                format!(
                    "duplicate: ActionRun {holder} already holds idempotency key `{}` (it may \
                     yet run, is running, or succeeded); a retry is allowed only after it fails \
                     or is cancelled",
                    action.idempotency_key
                ),
                format!(
                    "重复操作：ActionRun {holder} 已持有幂等键 `{}`（它可能尚未执行、正在执行或已成功）；\
                     只有在它失败或被取消后才允许重试",
                    action.idempotency_key
                )
            );
            return self
                .deny_action(
                    action,
                    &job,
                    Denial::by_policy(rationale.clone()),
                    rationale,
                )
                .await;
        }

        // Rule 5: an automatic action that already ran on one of these targets inside the window
        // escalates to approval instead of looping.
        let window = chrono::Duration::from_std(self.authority.auto_repeat_window())
            .unwrap_or_else(|_| chrono::Duration::minutes(10));
        let cutoff = action.created_at - window;
        let recent_auto_repeat = self.store.list_action_runs().await?.iter().any(|previous| {
            previous.action_run_id != action.action_run_id
                && previous.runbook_id == action.runbook_id
                && previous.approval == ApprovalState::NotRequired
                && previous.created_at >= cutoff
                && previous
                    .target_ids
                    .iter()
                    .any(|target| action.target_ids.contains(target))
        });
        let decision = self.authority.decide(&ProposalContext {
            runbook_id: &action.runbook_id,
            target_ids: &action.target_ids,
            arguments: &action.arguments,
            allowed_capabilities: &job.allowed_capabilities,
            allowed_target_ids: &job.allowed_target_ids,
            mode: before.operation_mode,
            recent_auto_repeat,
        });
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.action_authority_evaluated",
                    decision.rationale.clone(),
                )
                .with_issue(job.issue_id)
                .with_job(originating_job_id)
                .with_action(action.action_run_id)
                .with_payload(json!({ "decision": decision })),
            )
            .await?;
        if decision.approval == ApprovalState::Rejected {
            return self
                .deny_action(
                    action,
                    &job,
                    Denial::by_policy(decision.rationale.clone()),
                    decision.rationale,
                )
                .await;
        }
        let mut next = action.clone();
        next.apply_approval(decision.approval)?;
        self.store
            .update_action_run_if(&action, next.clone())
            .await?;
        self.reconcile_issue(job.issue_id).await?;
        Ok(next)
    }

    /// Applies a rule denial to a freshly created ActionRun and reconciles its Issue.
    async fn deny_action(
        &self,
        action: ActionRun,
        job: &Job,
        denial: Denial,
        rationale: String,
    ) -> AgentResult<ActionRun> {
        let mut next = action.clone();
        next.deny(denial)?;
        self.store
            .update_action_run_if(&action, next.clone())
            .await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.action_denied",
                    tr!(
                        format!("Denied by rule; awaiting human review: {rationale}"),
                        format!("已按规则拒绝，等待人工审核：{rationale}")
                    ),
                )
                .with_issue(job.issue_id)
                .with_job(job.job_id)
                .with_action(next.action_run_id),
            )
            .await?;
        self.reconcile_issue(job.issue_id).await?;
        Ok(next)
    }

    /// Records a human's approval of a waiting ActionRun; it becomes `Ready`.
    ///
    /// The write is a compare-and-set against the record as read, so two humans approving at
    /// once cannot both apply: the second gets `Conflict`. Execution is a separate step
    /// (`execute_action`).
    pub async fn approve_action(
        &self,
        action_run_id: ActionRunId,
        approved_by: impl Into<String>,
    ) -> AgentResult<ActionRun> {
        let approved_by = approved_by.into();
        let action = self.store.get_action_run(action_run_id).await?;
        self.ensure_not_archived(action.issue_id).await?;
        let mut next = action.clone();
        next.approve(approved_by.clone())?;
        self.store
            .update_action_run_if(&action, next.clone())
            .await?;
        self.store
            .append_event(
                NewEvent::new(
                    "human",
                    "human.action_approved",
                    tr!(
                        format!("{approved_by} approved the action"),
                        format!("{approved_by} 批准了该操作")
                    ),
                )
                .with_issue(next.issue_id)
                .with_job(next.originating_job_id)
                .with_action(action_run_id)
                .with_payload(json!({ "approved_by": approved_by, "status": next.status })),
            )
            .await?;
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Records a human's rejection of a waiting ActionRun, with their comment.
    ///
    /// The action is cancelled and never executes; the denial stays on it, so it appears in the
    /// Permission Denied inbox where the same or another human decides whether the reason should
    /// go back upstream.
    pub async fn reject_action(
        &self,
        action_run_id: ActionRunId,
        rejected_by: impl Into<String>,
        comment: Option<String>,
    ) -> AgentResult<ActionRun> {
        let rejected_by = rejected_by.into();
        let action = self.store.get_action_run(action_run_id).await?;
        self.ensure_not_archived(action.issue_id).await?;
        let denial = Denial::by_human(rejected_by.clone(), comment);
        let mut next = action.clone();
        next.deny(denial.clone())?;
        self.store
            .update_action_run_if(&action, next.clone())
            .await?;
        self.store
            .append_event(
                NewEvent::new(
                    "human",
                    "human.action_rejected",
                    match &denial.comment {
                        Some(comment) => tr!(
                            format!("{rejected_by} rejected the action: {comment}"),
                            format!("{rejected_by} 拒绝了该操作：{comment}")
                        ),
                        None => tr!(
                            format!("{rejected_by} rejected the action"),
                            format!("{rejected_by} 拒绝了该操作")
                        ),
                    },
                )
                .with_issue(next.issue_id)
                .with_job(next.originating_job_id)
                .with_action(action_run_id)
                .with_payload(json!({ "denial": denial, "status": next.status }))
                .with_trust(ContentTrust::Mixed),
            )
            .await?;
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Records a human's review of a denied or failed ActionRun, taking it out of the inbox.
    ///
    /// Sending the item upstream is the caller's job (a revising Job must exist first, so the
    /// review can name it); this method only persists and events the decision, as a
    /// compare-and-set so a second reviewer gets `Conflict` rather than overwriting the first.
    pub async fn review_action(
        &self,
        action_run_id: ActionRunId,
        review: HumanReview,
    ) -> AgentResult<ActionRun> {
        let action = self.store.get_action_run(action_run_id).await?;
        self.ensure_not_archived(action.issue_id).await?;
        let mut next = action.clone();
        next.record_review(review.clone())?;
        self.store
            .update_action_run_if(&action, next.clone())
            .await?;
        self.record_review_event(
            &review,
            next.issue_id,
            Some(next.originating_job_id),
            Some(action_run_id),
        )
        .await?;
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Records a human's review of a failed Job, taking it out of the Failed Job inbox.
    pub async fn review_job(&self, job_id: JobId, review: HumanReview) -> AgentResult<Job> {
        let job = self.store.get_job(job_id).await?;
        self.ensure_not_archived(job.issue_id).await?;
        let mut next = job.clone();
        next.record_review(review.clone())?;
        self.store.update_job_if(&job, next.clone()).await?;
        self.record_review_event(&review, next.issue_id, Some(job_id), None)
            .await?;
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Executes a `Ready` ActionRun through the Agents Platform.
    ///
    /// The Scheduler is the sole gateway to execution: it re-checks the freeze mode immediately
    /// before starting, and the move to `Running` is a compare-and-set so a concurrent retry gets
    /// `Conflict` instead of a second execution. Platform output is recorded with mixed trust
    /// because it contains machine-produced text. A Platform that errors out (as opposed to
    /// reporting a failed command) is recorded as a failed execution too — the action never stays
    /// `Running` with nobody responsible for it. Platform success only moves the action to
    /// `Verifying`; `verify_action` decides the terminal state.
    pub async fn execute_action(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.ensure_actions_allowed("execute_action").await?;
        let platform = self.require_platform("execute_action")?;

        let ready = self.store.get_action_run(action_run_id).await?;
        let mut running = ready.clone();
        running.start()?;
        self.store
            .update_action_run_if(&ready, running.clone())
            .await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "action.started",
                    tr!(
                        "The Scheduler handed an ActionRun to the Agents Platform",
                        "调度器已将操作交给 Agents 平台执行"
                    ),
                )
                .with_issue(running.issue_id)
                .with_action(action_run_id),
            )
            .await?;

        let result = match platform.execute_action(&running).await {
            Ok(result) => result,
            Err(error) => PlatformOperationResult::new(
                false,
                None,
                tr!(
                    format!("the Agents Platform failed before reporting a result: {error}"),
                    format!("Agents 平台在返回结果前出错：{error}")
                ),
            ),
        };
        let mut next = running.clone();
        next.record_execution_result(result.clone())?;
        self.store
            .update_action_run_if(&running, next.clone())
            .await?;
        self.store
            .append_event(
                NewEvent::new("agents-platform", "action.executed", result.summary.clone())
                    .with_issue(next.issue_id)
                    .with_action(action_run_id)
                    .with_payload(serde_json::to_value(&result)?)
                    .with_artifacts(result.output_artifact_id.into_iter().collect())
                    .with_trust(ContentTrust::Mixed),
            )
            .await?;
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Captures the after Snapshot and records a caller-supplied verification conclusion.
    ///
    /// Kept for callers with their own verifier; `verify_action` is the built-in one.
    pub async fn record_action_verification(
        &self,
        action_run_id: ActionRunId,
        after_capture: CaptureRequest,
        passed: bool,
        summary: impl Into<String>,
    ) -> AgentResult<ActionRun> {
        let action = self.store.get_action_run(action_run_id).await?;
        let after = self.request_snapshot(after_capture).await?;
        let evidence = if passed {
            Some(VerificationEvidence::Strong)
        } else {
            None
        };
        self.record_verification_result(
            action,
            Some(after.snapshot_id),
            passed,
            evidence,
            summary.into(),
        )
        .await
    }

    /// Captures the after Snapshot and verifies the action's postcondition for its class.
    ///
    /// Observe-only classes pass on execution success: the observation is the effect. Mutating
    /// classes must show their effect in the after Snapshot: every target Healthy, observed
    /// after the action started, with every verification Probe the proposal named having run,
    /// and — for a queue purge — the queue-depth metric at zero. A target that was already
    /// Healthy before the action passes with `Weak` evidence, because the check cannot tell the
    /// action's effect from the prior state; a dry run passes with `DryRun` evidence, which is not
    /// evidence of remediation at all and never resolves an Issue. A target missing from the
    /// Snapshot or `Unknown` fails: "we cannot see it" is not "it worked". If the after Snapshot
    /// cannot be captured, that is a verification failure recorded on the action, not an error
    /// that leaves it `Verifying`.
    pub async fn verify_action(
        &self,
        action_run_id: ActionRunId,
        after_capture: CaptureRequest,
    ) -> AgentResult<ActionRun> {
        let action = self.store.get_action_run(action_run_id).await?;
        let after = match self.request_snapshot(after_capture).await {
            Ok(after) => after,
            Err(error) => {
                return self
                    .record_verification_result(
                        action,
                        None,
                        false,
                        None,
                        tr!(
                            format!("after-Snapshot capture failed: {error}; effect unverified"),
                            format!("操作后快照采集失败：{error}；效果未验证")
                        ),
                    )
                    .await;
            }
        };
        let class = self
            .authority
            .registry()
            .classify(&action.runbook_id, &action.arguments);

        if action.dry_run {
            let summary = tr!(
                format!(
                    "dry run: commands were rendered and recorded, not executed; not evidence \
                     of remediation (after Snapshot {})",
                    after.snapshot_id
                ),
                format!(
                    "演练：命令仅被渲染并记录，未实际执行；不构成修复证据（操作后快照 {}）",
                    after.snapshot_id
                )
            );
            return self
                .record_verification_result(
                    action,
                    Some(after.snapshot_id),
                    true,
                    Some(VerificationEvidence::DryRun),
                    summary,
                )
                .await;
        }
        if class.is_some_and(|class| !class.is_mutating()) {
            let summary = tr!(
                format!(
                    "observation completed; no state change expected (after Snapshot {})",
                    after.snapshot_id
                ),
                format!(
                    "观测已完成；不涉及状态变更（操作后快照 {}）",
                    after.snapshot_id
                )
            );
            return self
                .record_verification_result(
                    action,
                    Some(after.snapshot_id),
                    true,
                    Some(VerificationEvidence::Strong),
                    summary,
                )
                .await;
        }

        let before = self
            .store
            .get_snapshot(action.before_snapshot_id)
            .await
            .ok();
        let started_at = action.started_at.unwrap_or(action.created_at);
        let mut lines = Vec::new();
        let mut passed = !action.target_ids.is_empty();
        let mut changed = false;
        for target in &action.target_ids {
            let Some(resource) = after.resources.iter().find(|r| &r.resource_id == target) else {
                passed = false;
                lines.push(tr!(
                    format!("`{target}` is absent from the after Snapshot"),
                    format!("`{target}` 未出现在操作后快照中")
                ));
                continue;
            };
            if resource.health != HealthState::Healthy {
                passed = false;
                lines.push(tr!(
                    format!("`{target}` is {:?}", resource.health),
                    format!("`{target}` 状态为 {:?}", resource.health)
                ));
                continue;
            }
            if resource.observed_at <= started_at {
                passed = false;
                lines.push(tr!(
                    format!(
                        "`{target}` was last observed before the action started; no fresh \
                         evidence"
                    ),
                    format!("`{target}` 的最近观测早于操作开始时间；没有新证据")
                ));
                continue;
            }
            for probe in &action.verification_probe_ids {
                let ran = resource
                    .facts
                    .iter()
                    .any(|fact| fact.name == format!("probe.{probe}"));
                if !ran {
                    passed = false;
                    lines.push(tr!(
                        format!("verification probe `{probe}` did not run on `{target}`"),
                        format!("验证探针 `{probe}` 未在 `{target}` 上运行")
                    ));
                }
            }
            if class == Some(OperationClass::QueuePurge)
                && let Some(depth) = resource
                    .metrics
                    .iter()
                    .find(|metric| metric.name == "queue.depth")
                && depth.value > 0.0
            {
                passed = false;
                lines.push(tr!(
                    format!(
                        "`{target}` still reports queue.depth = {} after the purge",
                        depth.value
                    ),
                    format!("清空队列后 `{target}` 的 queue.depth 仍为 {}", depth.value)
                ));
            }
            let was_healthy = before
                .as_ref()
                .and_then(|snapshot| snapshot.resources.iter().find(|r| &r.resource_id == target))
                .is_some_and(|r| r.health == HealthState::Healthy);
            if was_healthy {
                lines.push(tr!(
                    format!("`{target}` is Healthy, but was already Healthy before the action"),
                    format!("`{target}` 为 Healthy，但操作前就已是 Healthy")
                ));
            } else {
                changed = true;
                lines.push(tr!(
                    format!("`{target}` is Healthy (was not before the action)"),
                    format!("`{target}` 为 Healthy（操作前不是）")
                ));
            }
        }
        let evidence = if !passed {
            None
        } else if changed {
            Some(VerificationEvidence::Strong)
        } else {
            Some(VerificationEvidence::Weak)
        };
        let summary = format!(
            "{}: {}",
            match evidence {
                None => tr!("expected effect absent", "未观察到预期效果"),
                Some(VerificationEvidence::Weak) => tr!(
                    "postcondition holds, but it already held before (weak evidence)",
                    "后置条件成立，但操作前已成立（弱证据）"
                ),
                _ => tr!("expected effect observed", "已观察到预期效果"),
            },
            lines.join("; ")
        );
        self.record_verification_result(action, Some(after.snapshot_id), passed, evidence, summary)
            .await
    }

    /// Persists a verification conclusion for an action already in `Verifying`.
    async fn record_verification_result(
        &self,
        action: ActionRun,
        after_snapshot_id: Option<SnapshotId>,
        passed: bool,
        evidence: Option<VerificationEvidence>,
        summary: String,
    ) -> AgentResult<ActionRun> {
        let mut next = action.clone();
        next.record_verification(after_snapshot_id, passed, evidence, summary.clone())?;
        self.store
            .update_action_run_if(&action, next.clone())
            .await?;
        self.store
            .append_event(
                NewEvent::new("top-scheduler", "verification.recorded", summary)
                    .with_issue(next.issue_id)
                    .with_action(next.action_run_id)
                    .with_payload(json!({
                        "passed": passed,
                        "evidence": evidence,
                        "after_snapshot_id": after_snapshot_id,
                        "status": next.status,
                    })),
            )
            .await?;
        self.reconcile_issue(next.issue_id).await?;
        Ok(next)
    }

    /// Derives an Issue's status from its outstanding work and applies it.
    ///
    /// The rule, in order: a Job still running means `Investigating`; an action ready or running
    /// means `Mitigating`, one awaiting verification means `Verifying`; anything in an inbox —
    /// a permission request, an unreviewed denial or failure, a Job waiting on a human — means
    /// `WaitingForHuman`. With nothing outstanding, the latest pass decides: a `Solved` result,
    /// or an action of the latest Job that succeeded with real (not dry-run) evidence, resolves
    /// the Issue; otherwise it waits for a human to close it or send it on. Acknowledging an
    /// inbox item therefore never resolves an Issue by itself. Terminal Issues are left alone.
    pub async fn reconcile_issue(&self, issue_id: IssueId) -> AgentResult<Issue> {
        let issue = self.store.get_issue(issue_id).await?;
        if issue.status.is_terminal() {
            return Ok(issue);
        }
        let jobs: Vec<Job> = self
            .store
            .list_jobs()
            .await?
            .into_iter()
            .filter(|job| job.issue_id == issue_id)
            .collect();
        let actions: Vec<ActionRun> = self
            .store
            .list_action_runs()
            .await?
            .into_iter()
            .filter(|action| action.issue_id == issue_id)
            .collect();
        let Some((next, reason)) = derive_issue_status(&jobs, &actions) else {
            return Ok(issue);
        };
        if next == issue.status || !issue.can_transition_to(next) {
            return Ok(issue);
        }
        let mut updated = issue.clone();
        updated.transition_to(next)?;
        self.store.update_issue_if(&issue, updated.clone()).await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.issue_reconciled",
                    tr!(
                        format!("Issue moved from {:?} to {next:?}: {reason}", issue.status),
                        format!("Issue 状态由 {:?} 变为 {next:?}：{reason}", issue.status)
                    ),
                )
                .with_issue(issue_id)
                .with_payload(json!({ "from": issue.status, "to": next, "reason": reason })),
            )
            .await?;
        Ok(updated)
    }

    /// Closes an Issue on a human's say-so.
    ///
    /// Resolution is a human judgment when the derived status is `WaitingForHuman`; cancelling
    /// or failing an Issue is always available. Later callbacks and reconciliations leave a
    /// terminal Issue alone.
    pub async fn close_issue(
        &self,
        issue_id: IssueId,
        closure: IssueClosure,
        closed_by: impl Into<String>,
        comment: Option<String>,
    ) -> AgentResult<Issue> {
        let closed_by = closed_by.into();
        let issue = self.store.get_issue(issue_id).await?;
        if issue.is_archived() {
            return Err(archived(issue_id));
        }
        let mut next = issue.clone();
        next.transition_to(closure.status())?;
        self.store.update_issue_if(&issue, next.clone()).await?;
        let comment = comment.filter(|text| !text.trim().is_empty());
        self.store
            .append_event(
                NewEvent::new(
                    "human",
                    "human.issue_closed",
                    match &comment {
                        Some(comment) => tr!(
                            format!("{closed_by} closed the Issue as {closure:?}: {comment}"),
                            format!("{closed_by} 将 Issue 关闭为 {closure:?}：{comment}")
                        ),
                        None => tr!(
                            format!("{closed_by} closed the Issue as {closure:?}"),
                            format!("{closed_by} 将 Issue 关闭为 {closure:?}")
                        ),
                    },
                )
                .with_issue(issue_id)
                .with_payload(
                    json!({ "closure": closure, "closed_by": closed_by, "comment": comment }),
                )
                .with_trust(ContentTrust::Mixed),
            )
            .await?;
        Ok(next)
    }

    /// Records a human review in the EventLog.
    async fn record_review_event(
        &self,
        review: &HumanReview,
        issue_id: IssueId,
        job_id: Option<JobId>,
        action_run_id: Option<ActionRunId>,
    ) -> AgentResult<EventRecord> {
        let summary = match &review.decision {
            crate::domain::ReviewDecision::Acknowledged => tr!(
                format!(
                    "{} acknowledged the item; no further automatic work",
                    review.reviewer
                ),
                format!("{} 已知悉该事项；不再自动处理", review.reviewer)
            ),
            crate::domain::ReviewDecision::SentUpstream { job_id } => tr!(
                format!(
                    "{} sent the item back upstream as Job {job_id}",
                    review.reviewer
                ),
                format!("{} 将该事项送回上游，生成任务 {job_id}", review.reviewer)
            ),
        };
        let mut event = NewEvent::new("human", "human.review_recorded", summary)
            .with_issue(issue_id)
            .with_payload(serde_json::to_value(review)?)
            .with_trust(ContentTrust::Mixed);
        if let Some(job_id) = job_id {
            event = event.with_job(job_id);
        }
        if let Some(action_run_id) = action_run_id {
            event = event.with_action(action_run_id);
        }
        self.store.append_event(event).await
    }

    /// Stops creating new Jobs while allowing running work to continue producing callbacks.
    ///
    /// Initially the mode exists only in process and in the EventLog. After cross-process recovery,
    /// `recover` conservatively returns to `DispatchFrozen`.
    pub async fn freeze_dispatch(&self) -> AgentResult<()> {
        self.set_mode(SchedulerMode::DispatchFrozen).await
    }

    /// Stops creating new Jobs and indicates that no new ActionRun should begin.
    ///
    /// This method does not stop Broccoli, the Collector, or an external process already running. A
    /// future Platform will use checkpoint/cancellation policy to decide how to handle active work.
    pub async fn freeze_all(&self) -> AgentResult<()> {
        self.set_mode(SchedulerMode::FullyFrozen).await
    }

    /// Restores the Scheduler to normal dispatch mode.
    ///
    /// The initial version does not inspect unresolved human choices or conflicts. Later resume policy
    /// will validate those conditions before changing state.
    pub async fn resume(&self) -> AgentResult<()> {
        self.set_mode(SchedulerMode::Running).await
    }

    /// Recovers control state after a restart: enumerate, reconcile, restore the freeze mode.
    ///
    /// Recovery does not pretend interrupted work can be resumed. A Job that was running has no
    /// Team any more, so it is failed into the inbox; an action that was `Running` has an unknown
    /// outcome (the command may or may not have completed), so it is failed into the inbox with
    /// that warning; one that was `Verifying` is verified now when an after-capture request is
    /// supplied, otherwise failed as unverified; one never evaluated or never started is denied
    /// so it can be proposed again. A revising Job whose review record was not written before
    /// the crash gets it written now. Every touched Issue is reconciled.
    ///
    /// The previous process's last persisted mode is read back from the event log. Recovery ends
    /// in `FullyFrozen` if that is what it was, otherwise `DispatchFrozen`, and a human (or the
    /// `serve` startup policy) decides when to resume. Every mode change goes through the evented
    /// path, so the EventLog alone reconstructs mode history.
    pub async fn recover(&self) -> AgentResult<RecoverySummary> {
        self.recover_with(None).await
    }

    /// See [`TopScheduler::recover`]; `after_capture` lets actions that were awaiting
    /// verification be verified against a fresh Snapshot instead of failed as unverified.
    pub async fn recover_with(
        &self,
        after_capture: Option<CaptureRequest>,
    ) -> AgentResult<RecoverySummary> {
        let (previous_mode, pending_recovery_review) = self.persisted_mode().await?;
        self.set_mode(SchedulerMode::Recovering).await?;

        let (issues, jobs, actions) = self.live_unfinished().await?;
        let mut summary = RecoverySummary {
            previous_mode,
            pending_recovery_review,
            issue_ids: issues.iter().map(|issue| issue.issue_id).collect(),
            job_ids: jobs.iter().map(|job| job.job_id).collect(),
            action_run_ids: actions.iter().map(|a| a.action_run_id).collect(),
            interrupted_job_ids: Vec::new(),
            interrupted_action_ids: Vec::new(),
            verified_action_ids: Vec::new(),
            reconstructed_review_job_ids: Vec::new(),
            final_mode: SchedulerMode::DispatchFrozen,
        };
        let mut touched_issues: Vec<IssueId> = Vec::new();

        for job in &jobs {
            if !matches!(job.status, JobStatus::Queued | JobStatus::Running) {
                continue;
            }
            let mut next = job.clone();
            next.complete(JobResult::new(
                JobOutcome::Failed,
                tr!(
                    "interrupted by a controller restart; no Team was running this Job any more",
                    "被控制器重启中断；已没有团队在运行该任务"
                ),
            ))?;
            self.store.update_job_if(job, next).await?;
            self.store
                .append_event(
                    NewEvent::new(
                        "top-scheduler",
                        "scheduler.job_failed",
                        tr!(
                            "The Job was interrupted by a controller restart and awaits human review",
                            "任务被控制器重启中断，等待人工审核"
                        ),
                    )
                    .with_issue(job.issue_id)
                    .with_job(job.job_id),
                )
                .await?;
            summary.interrupted_job_ids.push(job.job_id);
            touched_issues.push(job.issue_id);
        }

        for action in &actions {
            let interrupted = |reason: &str| {
                Denial::by_policy(tr!(
                    format!("interrupted by a controller restart {reason}"),
                    format!("被控制器重启中断{reason}")
                ))
            };
            match action.status {
                ActionStatus::Proposed => {
                    let mut next = action.clone();
                    next.deny(interrupted(tr!(
                        "before authority was evaluated; propose it again if still needed",
                        "，尚未完成权限评估；如仍需要请重新提议"
                    )))?;
                    self.store.update_action_run_if(action, next).await?;
                }
                ActionStatus::Ready => {
                    let mut next = action.clone();
                    next.deny(interrupted(tr!(
                        "before execution started; propose it again if still needed",
                        "，尚未开始执行；如仍需要请重新提议"
                    )))?;
                    self.store.update_action_run_if(action, next).await?;
                }
                ActionStatus::Running => {
                    let mut next = action.clone();
                    next.record_execution_result(PlatformOperationResult::new(
                        false,
                        None,
                        tr!(
                            "interrupted by a controller restart while executing; the command \
                             may or may not have completed — check the machine before retrying",
                            "执行过程中被控制器重启中断；命令可能已完成也可能没有——重试前请先检查机器"
                        ),
                    ))?;
                    self.store.update_action_run_if(action, next).await?;
                }
                ActionStatus::Verifying => match &after_capture {
                    Some(capture) if self.collector.is_some() => {
                        self.verify_action(action.action_run_id, capture.clone())
                            .await?;
                        summary.verified_action_ids.push(action.action_run_id);
                        touched_issues.push(action.issue_id);
                        continue;
                    }
                    _ => {
                        let mut next = action.clone();
                        next.record_verification(
                            None,
                            false,
                            None,
                            tr!(
                                "controller restarted before verification; no after-Snapshot \
                                 was captured, so the effect is unverified",
                                "验证前控制器已重启；未采集操作后快照，效果未验证"
                            ),
                        )?;
                        self.store.update_action_run_if(action, next).await?;
                    }
                },
                ActionStatus::WaitingForApproval => continue,
                _ => continue,
            }
            self.store
                .append_event(
                    NewEvent::new(
                        "top-scheduler",
                        "scheduler.action_interrupted",
                        tr!(
                            format!(
                                "ActionRun was {:?} at restart and was reconciled into the inbox",
                                action.status
                            ),
                            format!("重启时操作处于 {:?} 状态，已整理进收件箱", action.status)
                        ),
                    )
                    .with_issue(action.issue_id)
                    .with_job(action.originating_job_id)
                    .with_action(action.action_run_id),
                )
                .await?;
            summary.interrupted_action_ids.push(action.action_run_id);
            touched_issues.push(action.issue_id);
        }

        // A revising Job proves a human sent something upstream; if the crash came between
        // dispatching it and writing the review, write the review now from the Job's own record.
        for job in self.store.list_jobs().await? {
            let (Some(_), Some(feedback)) = (job.revises_job_id, job.feedback.last()) else {
                continue;
            };
            let review = HumanReview::new(
                feedback.reviewer.clone(),
                ReviewDecision::SentUpstream { job_id: job.job_id },
                feedback.comment.clone(),
            );
            let reconstructed = match &feedback.origin {
                FeedbackOrigin::DeniedAction { action_run_id, .. }
                | FeedbackOrigin::FailedAction { action_run_id, .. } => {
                    let action = self.store.get_action_run(*action_run_id).await?;
                    if action.needs_review() {
                        self.review_action(*action_run_id, review).await?;
                        true
                    } else {
                        false
                    }
                }
                FeedbackOrigin::FailedJob { job_id, .. }
                | FeedbackOrigin::StalledJob { job_id, .. } => {
                    let reviewed = self.store.get_job(*job_id).await?;
                    if reviewed.needs_review() {
                        self.review_job(*job_id, review).await?;
                        true
                    } else {
                        false
                    }
                }
            };
            if reconstructed {
                summary.reconstructed_review_job_ids.push(job.job_id);
                touched_issues.push(job.issue_id);
            }
        }

        for issue in &issues {
            self.reconcile_issue(issue.issue_id).await?;
        }
        touched_issues.sort();
        touched_issues.dedup();

        summary.final_mode = match previous_mode {
            SchedulerMode::FullyFrozen => SchedulerMode::FullyFrozen,
            _ => SchedulerMode::DispatchFrozen,
        };
        self.set_mode(summary.final_mode).await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.recovered",
                    tr!(
                        format!(
                            "The Scheduler reconciled {} interrupted Job(s) and {} action(s) and \
                             is {:?}",
                            summary.interrupted_job_ids.len(),
                            summary.interrupted_action_ids.len(),
                            summary.final_mode
                        ),
                        format!(
                            "调度器整理了 {} 个被中断的任务和 {} 个操作，当前模式 {:?}",
                            summary.interrupted_job_ids.len(),
                            summary.interrupted_action_ids.len(),
                            summary.final_mode
                        )
                    ),
                )
                .with_payload(serde_json::to_value(&summary)?),
            )
            .await?;
        Ok(summary)
    }

    /// Reads the last human-set mode from the event log, and whether an earlier recovery is
    /// still waiting for a human to look at what it reconciled.
    ///
    /// Mode events written between `scheduler.recovery_started` and `scheduler.recovered` are
    /// recovery's own bookkeeping and do not count as a human decision; otherwise every restart
    /// after a recovery would stay frozen forever. A log with no human mode event means
    /// `Running`. `Recovering` is transient and ignored.
    async fn persisted_mode(&self) -> AgentResult<(SchedulerMode, bool)> {
        let mut mode = SchedulerMode::Running;
        let mut inside_recovery = false;
        let mut pending_review = false;
        for event in self.store.list_events().await? {
            match event.kind.as_str() {
                "scheduler.recovery_started" => inside_recovery = true,
                "scheduler.recovered" => {
                    inside_recovery = false;
                    let touched = ["interrupted_job_ids", "interrupted_action_ids"]
                        .iter()
                        .any(|key| {
                            event.payload[key]
                                .as_array()
                                .is_some_and(|ids| !ids.is_empty())
                        });
                    pending_review |= touched;
                }
                "scheduler.resumed" if !inside_recovery => {
                    mode = SchedulerMode::Running;
                    pending_review = false;
                }
                "scheduler.dispatch_frozen" if !inside_recovery => {
                    mode = SchedulerMode::DispatchFrozen;
                }
                "scheduler.fully_frozen" if !inside_recovery => {
                    mode = SchedulerMode::FullyFrozen;
                }
                _ => {}
            }
        }
        Ok((mode, pending_review))
    }

    /// Verifies that the Scheduler currently permits Job creation or supersession.
    ///
    /// Only `Running` permits dispatch. This function does not control callbacks from existing Jobs.
    async fn ensure_dispatch_allowed(&self, operation: &'static str) -> AgentResult<()> {
        let mode = self.mode().await;
        if mode == SchedulerMode::Running {
            Ok(())
        } else {
            Err(AgentError::SchedulerFrozen {
                mode: format!("{mode:?}"),
                operation,
            })
        }
    }

    /// Verifies that the Scheduler currently permits ActionRun creation or execution.
    ///
    /// `Running` and `DispatchFrozen` both allow actions — dispatch freezing stops new Jobs while
    /// active Jobs may still complete their side effects. `FullyFrozen` and `Recovering` refuse.
    async fn ensure_actions_allowed(&self, operation: &'static str) -> AgentResult<()> {
        let mode = self.mode().await;
        match mode {
            SchedulerMode::Running | SchedulerMode::DispatchFrozen => Ok(()),
            SchedulerMode::FullyFrozen | SchedulerMode::Recovering => {
                Err(AgentError::SchedulerFrozen {
                    mode: format!("{mode:?}"),
                    operation,
                })
            }
        }
    }

    /// Returns the wired Collector or a MissingDependency error naming the operation.
    fn require_collector(&self, operation: &'static str) -> AgentResult<&Arc<dyn CollectorPort>> {
        self.collector
            .as_ref()
            .ok_or(AgentError::MissingDependency {
                component: "collector",
                operation,
            })
    }

    /// Returns the wired Snapshot View Builder or a MissingDependency error.
    fn require_view_builder(
        &self,
        operation: &'static str,
    ) -> AgentResult<&Arc<dyn SnapshotViewBuilderPort>> {
        self.view_builder
            .as_ref()
            .ok_or(AgentError::MissingDependency {
                component: "snapshot-view-builder",
                operation,
            })
    }

    /// Returns the wired Agents Platform or a MissingDependency error.
    fn require_platform(
        &self,
        operation: &'static str,
    ) -> AgentResult<&Arc<dyn AgentsPlatformPort>> {
        self.platform.as_ref().ok_or(AgentError::MissingDependency {
            component: "agents-platform",
            operation,
        })
    }

    /// Records one policy consultation with its exact input and output.
    ///
    /// Model output is data, not authority, so the event carries mixed trust; post-contest review
    /// replays these records to audit every model-influenced decision.
    /// IDs of the Issues that are imported archives; see [`Issue::is_archived`].
    pub async fn archived_issue_ids(&self) -> AgentResult<HashSet<IssueId>> {
        Ok(self
            .store
            .list_issues()
            .await?
            .into_iter()
            .filter(Issue::is_archived)
            .map(|issue| issue.issue_id)
            .collect())
    }

    /// Refuses a control decision on an imported archive.
    async fn ensure_not_archived(&self, issue_id: IssueId) -> AgentResult<()> {
        if self.store.get_issue(issue_id).await?.is_archived() {
            return Err(archived(issue_id));
        }
        Ok(())
    }

    /// The unfinished Issues, Jobs, and ActionRuns that belong to this controller.
    ///
    /// An imported archive may well hold a Job that was still running when it was exported;
    /// that is history, not work, so recovery and triage never see it.
    async fn live_unfinished(&self) -> AgentResult<(Vec<Issue>, Vec<Job>, Vec<ActionRun>)> {
        let archived = self.archived_issue_ids().await?;
        let issues = self
            .store
            .list_unfinished_issues()
            .await?
            .into_iter()
            .filter(|issue| !archived.contains(&issue.issue_id))
            .collect();
        let jobs = self
            .store
            .list_unfinished_jobs()
            .await?
            .into_iter()
            .filter(|job| !archived.contains(&job.issue_id))
            .collect();
        let actions = self
            .store
            .list_unfinished_action_runs()
            .await?
            .into_iter()
            .filter(|action| !archived.contains(&action.issue_id))
            .collect();
        Ok((issues, jobs, actions))
    }

    async fn record_policy_consultation<Req, Dec>(
        &self,
        decision_point: &'static str,
        request: &Req,
        decision: &Dec,
        issue_id: Option<IssueId>,
        job_id: Option<JobId>,
    ) -> AgentResult<EventRecord>
    where
        Req: Serialize,
        Dec: Serialize,
    {
        let mut event = NewEvent::new(
            "scheduler-policy",
            "scheduler.policy_consulted",
            tr!(
                "The Scheduler consulted the policy model at a fixed decision point",
                "调度器在固定决策点咨询了策略模型"
            ),
        )
        .with_payload(json!({
            "decision_point": decision_point,
            "request": serde_json::to_value(request)?,
            "decision": serde_json::to_value(decision)?,
        }))
        .with_trust(ContentTrust::Mixed);
        if let Some(issue_id) = issue_id {
            event = event.with_issue(issue_id);
        }
        if let Some(job_id) = job_id {
            event = event.with_job(job_id);
        }
        self.store.append_event(event).await
    }

    /// Verifies that a Snapshot View references an existing canonical Snapshot and SnapshotView Artifact.
    ///
    /// The initial version checks Artifact kind and hash-field agreement. A future Artifact Store will
    /// recompute the content hash and validate the redaction manifest.
    async fn validate_snapshot_view(&self, view: &SnapshotViewRef) -> AgentResult<()> {
        self.store.get_snapshot(view.snapshot_id).await?;
        let artifact = self.store.get_artifact(view.artifact_id).await?;
        if artifact.kind != ArtifactKind::SnapshotView {
            return Err(AgentError::InvalidInput(format!(
                "Artifact `{}` is not a SnapshotView",
                artifact.artifact_id
            )));
        }
        if artifact.content_sha256 != view.content_sha256 {
            return Err(AgentError::InvalidInput(format!(
                "Snapshot View `{}` has a hash that does not match its Artifact",
                artifact.artifact_id
            )));
        }
        Ok(())
    }

    /// Records that the Scheduler created a formal Issue.
    ///
    /// This helper centralizes the event name and relationship fields so public methods do not
    /// assemble the same Event repeatedly.
    async fn record_issue_created(&self, issue: &Issue) -> AgentResult<EventRecord> {
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.issue_created",
                    tr!(
                        "The Scheduler created a formal Issue",
                        "调度器创建了正式 Issue"
                    ),
                )
                .with_issue(issue.issue_id)
                .with_payload(serde_json::to_value(issue)?),
            )
            .await
    }

    /// Records a Job creation, revision, or supersession event.
    ///
    /// This helper records the exact Job structure so post-contest review can reconstruct the
    /// Snapshot View, scope, and feedback used at the time.
    async fn record_job_event(
        &self,
        job: &Job,
        kind: &'static str,
        summary: impl Into<String>,
    ) -> AgentResult<EventRecord> {
        self.store
            .append_event(
                NewEvent::new("top-scheduler", kind, summary)
                    .with_issue(job.issue_id)
                    .with_job(job.job_id)
                    .with_payload(serde_json::to_value(job)?)
                    .with_artifacts(vec![job.snapshot_view.artifact_id]),
            )
            .await
    }

    /// Updates Scheduler mode and writes the corresponding EventLog record.
    ///
    /// Every mode change flows through here — including recovery — so the EventLog alone can
    /// reconstruct mode history. Initially the in-process mode update and event write are not
    /// transactional; persisted Scheduler checkpoints must eliminate this window so restart cannot
    /// misinterpret freeze state.
    async fn set_mode(&self, next: SchedulerMode) -> AgentResult<()> {
        {
            let mut mode = self.mode.write().await;
            *mode = next;
        }

        let (kind, summary) = match next {
            SchedulerMode::Running => (
                "scheduler.resumed",
                tr!(
                    "The Scheduler resumed normal dispatch",
                    "调度器已恢复正常派发"
                ),
            ),
            SchedulerMode::DispatchFrozen => (
                "scheduler.dispatch_frozen",
                tr!(
                    "The Scheduler stopped creating and superseding Jobs",
                    "调度器已停止创建和替代任务"
                ),
            ),
            SchedulerMode::FullyFrozen => (
                "scheduler.fully_frozen",
                tr!(
                    "The Scheduler froze new Jobs and new ActionRuns",
                    "调度器已冻结新任务和新操作"
                ),
            ),
            SchedulerMode::Recovering => (
                "scheduler.recovery_started",
                tr!(
                    "The Scheduler is recovering unfinished state",
                    "调度器正在恢复未完成的状态"
                ),
            ),
        };
        self.store
            .append_event(NewEvent::new("top-scheduler", kind, summary))
            .await?;
        Ok(())
    }
}

/// The Issue status implied by its Jobs and ActionRuns, with the reason, or `None` when no work
/// exists yet (an Issue stays `Open` until something is dispatched).
#[async_trait::async_trait]
impl InspectionPort for TopScheduler {
    /// See [`TopScheduler::inspect`].
    async fn inspect(
        &self,
        job_id: JobId,
        request: InspectionRequest,
    ) -> AgentResult<InspectionResult> {
        TopScheduler::inspect(self, job_id, request).await
    }
}

fn derive_issue_status(jobs: &[Job], actions: &[ActionRun]) -> Option<(IssueStatus, String)> {
    if jobs.is_empty() && actions.is_empty() {
        return None;
    }
    if jobs
        .iter()
        .any(|job| matches!(job.status, JobStatus::Queued | JobStatus::Running))
    {
        return Some((
            IssueStatus::Investigating,
            tr!("a Job is running", "有任务正在运行").to_string(),
        ));
    }
    if actions
        .iter()
        .any(|action| matches!(action.status, ActionStatus::Ready | ActionStatus::Running))
    {
        return Some((
            IssueStatus::Mitigating,
            tr!("an action is ready or executing", "有操作就绪或正在执行").to_string(),
        ));
    }
    if actions
        .iter()
        .any(|action| action.status == ActionStatus::Verifying)
    {
        return Some((
            IssueStatus::Verifying,
            tr!("an action awaits verification", "有操作等待验证").to_string(),
        ));
    }
    let waiting = actions
        .iter()
        .any(|action| action.status == ActionStatus::WaitingForApproval || action.needs_review())
        || jobs.iter().any(|job| {
            job.needs_review()
                || matches!(
                    job.status,
                    JobStatus::WaitingForHuman | JobStatus::Blocked | JobStatus::NeedsResnapshot
                )
        });
    if waiting {
        return Some((
            IssueStatus::WaitingForHuman,
            tr!(
                "an inbox item or a Job waits for a human",
                "有收件箱事项或任务在等待人工"
            )
            .to_string(),
        ));
    }
    let latest_job = jobs.iter().max_by_key(|job| (job.created_at, job.job_id))?;
    if latest_job
        .result
        .as_ref()
        .is_some_and(|result| result.outcome == JobOutcome::Solved)
    {
        return Some((
            IssueStatus::Resolved,
            tr!(
                "the latest pass reported the problem solved",
                "最近一轮处理报告问题已解决"
            )
            .to_string(),
        ));
    }
    let remediated = actions.iter().any(|action| {
        action.originating_job_id == latest_job.job_id
            && action.status == ActionStatus::Succeeded
            && action.verification_evidence != Some(VerificationEvidence::DryRun)
    });
    if remediated {
        return Some((
            IssueStatus::Resolved,
            tr!(
                "an action of the latest pass succeeded with verified evidence",
                "最近一轮处理中有操作成功并通过验证"
            )
            .to_string(),
        ));
    }
    Some((
        IssueStatus::WaitingForHuman,
        tr!(
            "nothing automatic remains; a human closes the Issue or sends it on",
            "没有可自动进行的工作了；由人工关闭该 Issue 或送回继续处理"
        )
        .to_string(),
    ))
}
