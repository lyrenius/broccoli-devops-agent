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
//! own periodic schedule, and the sole gateway that moves ActionRuns to execution.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::domain::{
    ActionProposal, ActionRun, ActionRunId, ApprovalState, ArtifactKind, ContentTrust, EventRecord,
    HumanReport, Issue, IssueCandidate, IssueId, IssueStatus, Job, JobId, JobOutcome, NewEvent,
    ResourceId, Snapshot, SnapshotId, SnapshotViewRef, TeamCallback, TeamKind, WorkOrder,
};
use crate::error::{AgentError, AgentResult};
use crate::policy::AuthorityPolicy;
use crate::ports::{
    AgentsPlatformPort, CallbackAdviceRequest, CaptureRequest, CollectorPort, NextStep,
    NextStepDecision, SchedulerPolicyPort, SnapshotViewBuildRequest, SnapshotViewBuilderPort,
    StateStore, TriageDecision, TriageRequest, WorkOrderDraftRequest,
};

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

/// Summary of unfinished objects found during Scheduler recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySummary {
    /// Issue IDs that still need attention.
    pub issue_ids: Vec<IssueId>,
    /// Job IDs that still need attention.
    pub job_ids: Vec<JobId>,
    /// ActionRun IDs that still need attention.
    pub action_run_ids: Vec<ActionRunId>,
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
                    "The Scheduler requested and persisted a Snapshot",
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
                    "A human reported a problem",
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
                    "The Snapshot Judge proposed a potential problem",
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
                    "No policy model is wired; the candidate awaits human triage",
                ))
                .await?;
            return Ok(TriageOutcome::DeferredToHuman);
        };

        let request = TriageRequest {
            candidate: candidate.clone(),
            open_issues: self.store.list_unfinished_issues().await?,
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
                            "The Scheduler merged a candidate into an existing Issue",
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
                            "The Scheduler rejected a candidate as noise or duplicate",
                        )
                        .with_payload(json!({ "candidate_id": candidate.candidate_id })),
                    )
                    .await?;
                Ok(TriageOutcome::Rejected)
            }
        }
    }

    /// Drafts a Work Order for a new Job on the given Issue.
    ///
    /// With a policy model the draft is consulted, recorded, and validated (an empty objective is
    /// replaced by the deterministic fallback). Without one, a minimal deterministic Work Order is
    /// derived from the Issue itself.
    pub async fn draft_work_order(
        &self,
        issue_id: IssueId,
        team_kind: TeamKind,
    ) -> AgentResult<WorkOrder> {
        let issue = self.store.get_issue(issue_id).await?;
        let fallback = WorkOrder::new(format!("Investigate: {}", issue.title));

        let Some(policy) = &self.policy else {
            return Ok(fallback);
        };

        let request = WorkOrderDraftRequest {
            issue: issue.clone(),
            team_kind,
        };
        let draft = policy.draft_work_order(&request).await?;
        self.record_policy_consultation("draft_work_order", &request, &draft, Some(issue_id), None)
            .await?;
        if draft.objective.trim().is_empty() {
            return Ok(fallback);
        }
        Ok(draft)
    }

    /// Creates and dispatches a Job bound to an immutable Snapshot View for the given Issue.
    ///
    /// The method verifies that the Issue, canonical Snapshot, Snapshot View Artifact, and content
    /// hash all exist and agree, and moves an `Open` Issue to `Investigating`. The initial version
    /// neither selects nor invokes a Team automatically; the caller supplies the Work Order and
    /// scope explicitly.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_job(
        &self,
        issue_id: IssueId,
        snapshot_view: SnapshotViewRef,
        team_kind: TeamKind,
        work_order: WorkOrder,
        allowed_capabilities: Vec<String>,
        allowed_target_ids: Vec<ResourceId>,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("create_job").await?;
        let mut issue = self.store.get_issue(issue_id).await?;
        self.validate_snapshot_view(&snapshot_view).await?;

        let mut job = Job::new(
            issue_id,
            snapshot_view,
            team_kind,
            work_order,
            allowed_capabilities,
            allowed_target_ids,
        );
        job.transition_to(crate::domain::JobStatus::Running)?;
        self.store.insert_job(job.clone()).await?;

        if issue.can_transition_to(IssueStatus::Investigating) {
            issue.transition_to(IssueStatus::Investigating)?;
            self.store.update_issue(issue).await?;
        }

        self.record_job_event(
            &job,
            "scheduler.job_dispatched",
            "The Scheduler created and dispatched a Job",
        )
        .await?;
        Ok(job)
    }

    /// Creates a Job that replaces an older Job using a new Snapshot View.
    ///
    /// The method does not modify the old Job's Snapshot. It marks the old Job as `Superseded` and
    /// creates a new ID. The caller re-derives capability and target scope explicitly, because a
    /// new Snapshot may justify a narrower scope than the old Job held. The current in-memory Store
    /// has no transactions; a future persistence implementation must commit the old Job, new Job,
    /// and event atomically.
    pub async fn supersede_job(
        &self,
        previous_job_id: JobId,
        new_snapshot_view: SnapshotViewRef,
        new_work_order: WorkOrder,
        allowed_capabilities: Vec<String>,
        allowed_target_ids: Vec<ResourceId>,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("supersede_job").await?;
        self.validate_snapshot_view(&new_snapshot_view).await?;

        let mut previous = self.store.get_job(previous_job_id).await?;
        let mut next = previous.supersede_with(
            new_snapshot_view,
            new_work_order,
            allowed_capabilities,
            allowed_target_ids,
        )?;
        next.transition_to(crate::domain::JobStatus::Running)?;
        self.store.update_job(previous).await?;
        self.store.insert_job(next.clone()).await?;
        self.record_job_event(
            &next,
            "scheduler.job_superseded",
            "The Scheduler created a superseding Job from a new Snapshot",
        )
        .await?;
        Ok(next)
    }

    /// Serves a Team's probe request: capture a new Snapshot, build its View, and supersede the Job.
    ///
    /// This is the `NeedsMoreData` flow from the architecture document. The redaction profile,
    /// Work Order, and scope come from the caller (usually derived from the old Job plus the
    /// requested Probes in `capture.requested_probe_ids`).
    #[allow(clippy::too_many_arguments)]
    pub async fn resnapshot_and_supersede(
        &self,
        previous_job_id: JobId,
        capture: CaptureRequest,
        redaction_profile: impl Into<String>,
        new_work_order: WorkOrder,
        allowed_capabilities: Vec<String>,
        allowed_target_ids: Vec<ResourceId>,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("resnapshot_and_supersede")
            .await?;
        let view_builder = self.require_view_builder("resnapshot_and_supersede")?;
        let previous = self.store.get_job(previous_job_id).await?;

        let snapshot = self.request_snapshot(capture).await?;
        let view_request = SnapshotViewBuildRequest {
            issue_id: previous.issue_id,
            team_kind: previous.team_kind,
            work_order: new_work_order.clone(),
            allowed_capabilities: allowed_capabilities.clone(),
            allowed_target_ids: allowed_target_ids.clone(),
            redaction_profile: redaction_profile.into(),
        };
        let built = view_builder
            .build_snapshot_view(&snapshot, &view_request)
            .await?;
        self.store.insert_artifact(built.artifact).await?;

        self.supersede_job(
            previous_job_id,
            built.snapshot_view,
            new_work_order,
            allowed_capabilities,
            allowed_target_ids,
        )
        .await
    }

    /// Accepts an Agent Team callback and updates the Job and Issue when a final result is present.
    ///
    /// A callback for a terminal Job is rejected before anything is written, so supersession or
    /// completion cannot leave orphan callback events in the log. When `final_result` is present,
    /// the Job state machine decides the Job's next state and the owning Issue receives the
    /// corresponding lifecycle update where the Issue state machine allows it. A future database
    /// implementation must place the event and both updates in one transaction.
    pub async fn handle_callback(&self, callback: TeamCallback) -> AgentResult<Job> {
        let mut job = self.store.get_job(callback.job_id).await?;
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

        if let Some(result) = callback.final_result {
            let issue_next = match result.outcome {
                JobOutcome::NeedsHuman | JobOutcome::OptionsReady => {
                    Some(IssueStatus::WaitingForHuman)
                }
                JobOutcome::Solved => Some(IssueStatus::Resolved),
                JobOutcome::DiagnosisOnly
                | JobOutcome::NeedsMoreData
                | JobOutcome::Blocked
                | JobOutcome::Failed => None,
            };

            job.complete(result)?;
            self.store.update_job(job.clone()).await?;

            if let Some(next) = issue_next {
                let mut issue = self.store.get_issue(job.issue_id).await?;
                if issue.can_transition_to(next) {
                    issue.transition_to(next)?;
                    self.store.update_issue(issue).await?;
                }
            }
        }
        Ok(job)
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
                    question: format!(
                        "Job `{job_id}` finished with outcome `{:?}`; choose the next step",
                        result.outcome
                    ),
                },
                rationale: "no policy model is wired; deferring to a human".to_string(),
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
    /// starting state, and its operation mode selects the matrix column. The decision — auto,
    /// approve, or deny, with the repeat-rate escalation of rule 5 — is applied immediately and
    /// recorded, so the returned ActionRun is already `Ready`, `WaitingForApproval`, or
    /// `Cancelled`. Creation is refused while the Scheduler is `FullyFrozen` or `Recovering`. The
    /// idempotency key comes from the caller so retries of the same intent reuse the same key.
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
        let mut action = ActionRun::from_proposal(
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
                    "The Scheduler converted a Team proposal into an ActionRun",
                )
                .with_issue(job.issue_id)
                .with_job(originating_job_id)
                .with_action(action.action_run_id)
                .with_payload(serde_json::to_value(&action)?),
            )
            .await?;

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
        let decision = self.authority.decide(
            &action.runbook_id,
            &action.arguments,
            before.operation_mode,
            recent_auto_repeat,
        );
        action.apply_approval(decision.approval)?;
        self.store.update_action_run(action.clone()).await?;
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
                .with_payload(json!({ "decision": decision, "status": action.status })),
            )
            .await?;
        Ok(action)
    }

    /// Applies an approval result computed by Scheduler policy or given by a human.
    ///
    /// The domain object enforces the state machine; this method persists the result and records
    /// who-approved-what in the EventLog.
    pub async fn apply_action_approval(
        &self,
        action_run_id: ActionRunId,
        approval: ApprovalState,
    ) -> AgentResult<ActionRun> {
        let mut action = self.store.get_action_run(action_run_id).await?;
        action.apply_approval(approval)?;
        self.store.update_action_run(action.clone()).await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.action_approval_applied",
                    "The Scheduler applied an approval decision to an ActionRun",
                )
                .with_issue(action.issue_id)
                .with_action(action_run_id)
                .with_payload(json!({ "approval": approval, "status": action.status })),
            )
            .await?;
        Ok(action)
    }

    /// Executes a `Ready` ActionRun through the Agents Platform.
    ///
    /// The Scheduler is the sole gateway to execution: it re-checks the freeze mode immediately
    /// before starting, and Platform output is recorded with mixed trust because it contains
    /// machine-produced text. Platform success only moves the action to `Verifying`;
    /// `record_action_verification` decides the terminal state.
    pub async fn execute_action(&self, action_run_id: ActionRunId) -> AgentResult<ActionRun> {
        self.ensure_actions_allowed("execute_action").await?;
        let platform = self.require_platform("execute_action")?;

        let mut action = self.store.get_action_run(action_run_id).await?;
        action.start()?;
        self.store.update_action_run(action.clone()).await?;
        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "action.started",
                    "The Scheduler handed an ActionRun to the Agents Platform",
                )
                .with_issue(action.issue_id)
                .with_action(action_run_id),
            )
            .await?;

        let result = platform.execute_action(&action).await?;
        action.record_execution_result(result.clone())?;
        self.store.update_action_run(action.clone()).await?;
        self.store
            .append_event(
                NewEvent::new("agents-platform", "action.executed", result.summary.clone())
                    .with_issue(action.issue_id)
                    .with_action(action_run_id)
                    .with_payload(serde_json::to_value(&result)?)
                    .with_artifacts(result.output_artifact_id.into_iter().collect())
                    .with_trust(ContentTrust::Mixed),
            )
            .await?;
        Ok(action)
    }

    /// Captures the after Snapshot and records the effect-verification conclusion.
    ///
    /// The absence of an expected effect is a verification failure even when the underlying command
    /// succeeded. Initially the caller supplies the boolean result; a later verifier will compute
    /// it from the action's verification Probes.
    pub async fn record_action_verification(
        &self,
        action_run_id: ActionRunId,
        after_capture: CaptureRequest,
        passed: bool,
        summary: impl Into<String>,
    ) -> AgentResult<ActionRun> {
        let mut action = self.store.get_action_run(action_run_id).await?;
        let after = self.request_snapshot(after_capture).await?;
        let summary = summary.into();
        action.record_verification(after.snapshot_id, passed, summary.clone())?;
        self.store.update_action_run(action.clone()).await?;
        self.store
            .append_event(
                NewEvent::new("top-scheduler", "verification.recorded", summary)
                    .with_issue(action.issue_id)
                    .with_action(action_run_id)
                    .with_payload(json!({
                        "passed": passed,
                        "after_snapshot_id": after.snapshot_id,
                        "status": action.status,
                    })),
            )
            .await?;
        Ok(action)
    }

    /// Captures the after Snapshot and verifies the action's expected effect deterministically.
    ///
    /// v0.1 verification is the rule "every target the action touched must be Healthy in the
    /// after Snapshot". A target missing from the Snapshot counts as unverified, and a successful
    /// command with no visible effect is still a verification failure — exit code zero is not
    /// success. Later verifiers will evaluate the action's own verification Probes.
    pub async fn verify_action(
        &self,
        action_run_id: ActionRunId,
        after_capture: CaptureRequest,
    ) -> AgentResult<ActionRun> {
        let action = self.store.get_action_run(action_run_id).await?;
        let after = self.request_snapshot(after_capture).await?;
        let mut lines = Vec::new();
        let mut passed = !action.target_ids.is_empty();
        for target in &action.target_ids {
            match after.resources.iter().find(|r| &r.resource_id == target) {
                Some(resource) if resource.health == crate::domain::HealthState::Healthy => {
                    lines.push(format!("`{target}` is Healthy"));
                }
                Some(resource) => {
                    passed = false;
                    lines.push(format!("`{target}` is {:?}", resource.health));
                }
                None => {
                    passed = false;
                    lines.push(format!("`{target}` is absent from the after Snapshot"));
                }
            }
        }
        let summary = format!(
            "{}: {}",
            if passed {
                "expected effect observed"
            } else {
                "expected effect absent"
            },
            lines.join("; ")
        );
        self.record_verification_result(action, after.snapshot_id, passed, summary)
            .await
    }

    /// Persists a verification conclusion for an action already in `Verifying`.
    async fn record_verification_result(
        &self,
        mut action: ActionRun,
        after_snapshot_id: SnapshotId,
        passed: bool,
        summary: String,
    ) -> AgentResult<ActionRun> {
        action.record_verification(after_snapshot_id, passed, summary.clone())?;
        self.store.update_action_run(action.clone()).await?;
        self.store
            .append_event(
                NewEvent::new("top-scheduler", "verification.recorded", summary)
                    .with_issue(action.issue_id)
                    .with_action(action.action_run_id)
                    .with_payload(json!({
                        "passed": passed,
                        "after_snapshot_id": after_snapshot_id,
                        "status": action.status,
                    })),
            )
            .await?;
        Ok(action)
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

    /// Enumerates unfinished Issues, Jobs, and ActionRuns from the Store and returns a recovery summary.
    ///
    /// The method does not reconnect Agent Teams or replay ActionRuns. After recovery the Scheduler
    /// remains `DispatchFrozen`, waiting for a human to inspect the summary and call `resume`. Both
    /// mode changes go through the evented path so the EventLog alone can reconstruct mode history.
    pub async fn recover(&self) -> AgentResult<RecoverySummary> {
        self.set_mode(SchedulerMode::Recovering).await?;

        let issues = self.store.list_unfinished_issues().await?;
        let jobs = self.store.list_unfinished_jobs().await?;
        let actions = self.store.list_unfinished_action_runs().await?;
        let summary = RecoverySummary {
            issue_ids: issues.into_iter().map(|issue| issue.issue_id).collect(),
            job_ids: jobs.into_iter().map(|job| job.job_id).collect(),
            action_run_ids: actions
                .into_iter()
                .map(|action| action.action_run_id)
                .collect(),
        };

        self.set_mode(SchedulerMode::DispatchFrozen).await?;

        self.store
            .append_event(
                NewEvent::new(
                    "top-scheduler",
                    "scheduler.recovered",
                    "The Scheduler enumerated unfinished state and kept dispatch frozen",
                )
                .with_payload(serde_json::to_value(&summary)?),
            )
            .await?;
        Ok(summary)
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
            "The Scheduler consulted the policy model at a fixed decision point",
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
                    "The Scheduler created a formal Issue",
                )
                .with_issue(issue.issue_id)
                .with_payload(serde_json::to_value(issue)?),
            )
            .await
    }

    /// Records a Job creation or supersession event.
    ///
    /// This helper records the exact Job structure so post-contest review can reconstruct the
    /// Snapshot View and Work Order used at the time.
    async fn record_job_event(
        &self,
        job: &Job,
        kind: &'static str,
        summary: &'static str,
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
            SchedulerMode::Running => {
                ("scheduler.resumed", "The Scheduler resumed normal dispatch")
            }
            SchedulerMode::DispatchFrozen => (
                "scheduler.dispatch_frozen",
                "The Scheduler stopped creating and superseding Jobs",
            ),
            SchedulerMode::FullyFrozen => (
                "scheduler.fully_frozen",
                "The Scheduler froze new Jobs and new ActionRuns",
            ),
            SchedulerMode::Recovering => (
                "scheduler.recovery_started",
                "The Scheduler is recovering unfinished state",
            ),
        };
        self.store
            .append_event(NewEvent::new("top-scheduler", kind, summary))
            .await?;
        Ok(())
    }
}
