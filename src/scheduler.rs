//! Minimal readable implementation of the Top Scheduler.
//!
//! The initial Scheduler manages only domain objects, Snapshot binding, callbacks, and recovery
//! state. It does not call the Collector, a model, an Agent Team, or the Agents Platform. A later
//! orchestration loop will build on these methods.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::domain::{
    ArtifactKind, ContentTrust, EventRecord, HumanReport, Issue, IssueCandidate, IssueId, Job,
    JobId, NewEvent, ResourceId, SnapshotViewRef, TeamCallback, TeamKind, WorkOrder,
};
use crate::error::{AgentError, AgentResult};
use crate::ports::StateStore;

/// Whether the Top Scheduler currently permits dispatch or execution work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerMode {
    /// Accepts problems and creates Jobs normally.
    Running,
    /// Creates no new Jobs, while running Jobs may continue and return checkpoints.
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
    pub action_run_ids: Vec<crate::domain::ActionRunId>,
}

/// Scheduler that owns global Issue, Job, priority, callback, and freeze/recovery state.
pub struct TopScheduler {
    store: Arc<dyn StateStore>,
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
            mode: RwLock::new(SchedulerMode::Running),
        }
    }

    /// Returns the current Scheduler mode.
    ///
    /// This method reads only in-process scheduling state and accesses no external system.
    pub async fn mode(&self) -> SchedulerMode {
        *self.mode.read().await
    }

    /// Accepts a human report and creates a deterministically highest-priority Issue.
    ///
    /// The report's Snapshot must already be persisted. The method records an event containing
    /// untrusted human text before creating the Issue. A future database implementation must insert
    /// the event and Issue in one transaction.
    pub async fn accept_human_report(
        &self,
        report: HumanReport,
        snapshot_id: crate::domain::SnapshotId,
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

    /// Records an Issue Candidate from the Snapshot Judge without creating a formal Issue.
    ///
    /// A Candidate may contain model interpretation, so the event is recorded with mixed trust.
    /// Deduplication, priority, and formal acceptance will be implemented in later Scheduler policy.
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

    /// Creates and dispatches a Job bound to an immutable Snapshot View for the given Issue.
    ///
    /// The method verifies that the Issue, canonical Snapshot, Snapshot View Artifact, and content
    /// hash all exist and agree. The initial version neither selects nor invokes a Team automatically;
    /// the caller supplies the Work Order and scope explicitly.
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
        self.store.get_issue(issue_id).await?;
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
    /// creates a new ID. The current in-memory Store has no transactions; a future persistence
    /// implementation must commit the old Job, new Job, and event atomically.
    pub async fn supersede_job(
        &self,
        previous_job_id: JobId,
        new_snapshot_view: SnapshotViewRef,
        new_work_order: WorkOrder,
    ) -> AgentResult<Job> {
        self.ensure_dispatch_allowed("supersede_job").await?;
        self.validate_snapshot_view(&new_snapshot_view).await?;

        let mut previous = self.store.get_job(previous_job_id).await?;
        let mut next = previous.supersede_with(new_snapshot_view, new_work_order)?;
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

    /// Accepts an Agent Team callback and updates the Job when a final result is present.
    ///
    /// The callback always enters the EventLog first. When `final_result` is present, the Job state
    /// machine decides whether to complete, wait for a human, block, or collect again. A future
    /// database implementation must place the event and Job update in one transaction.
    pub async fn handle_callback(&self, callback: TeamCallback) -> AgentResult<Job> {
        let mut job = self.store.get_job(callback.job_id).await?;
        if job.issue_id != callback.issue_id {
            return Err(AgentError::InvalidInput(format!(
                "callback Issue `{}` does not match the Job's Issue `{}`",
                callback.issue_id, job.issue_id
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
            job.complete(result)?;
            self.store.update_job(job.clone()).await?;
        }
        Ok(job)
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
    /// remains `DispatchFrozen`, waiting for a human to inspect the summary and call `resume`.
    pub async fn recover(&self) -> AgentResult<RecoverySummary> {
        {
            let mut mode = self.mode.write().await;
            *mode = SchedulerMode::Recovering;
        }

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

        {
            let mut mode = self.mode.write().await;
            *mode = SchedulerMode::DispatchFrozen;
        }

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
    /// Initially the in-process mode update and event write are not transactional. Persisted Scheduler
    /// checkpoints must eliminate this window so restart cannot misinterpret freeze state.
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
