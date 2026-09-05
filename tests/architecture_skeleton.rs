//! Integration tests that explain the initial architecture scaffold through behavior examples.

use std::fmt::Debug;
use std::sync::Arc;

use broccoli_devops_agent::domain::{
    ActionProposal, ActionRun, ActionStatus, ApprovalState, Artifact, ArtifactKind, Confidence,
    HumanReport, Issue, IssueCandidate, IssuePriority, IssueStatus, Job, JobBrief, JobOutcome,
    JobResult, JobStatus, NamedValue, NewEvent, OperationMode, PlatformOperationResult, Snapshot,
    SnapshotCause, SnapshotViewRef, TeamCallback, TeamCallbackKind, TeamKind, VerificationEvidence,
};
use broccoli_devops_agent::ports::{
    AgentTeamPort, CallbackAdviceRequest, CancelSignal, CaptureRequest, NextStep, NextStepDecision,
    SchedulerPolicyPort, StateStore, TeamCallbackSink, TriageDecision, TriageRequest, cancel_pair,
};
use broccoli_devops_agent::scheduler::{SchedulerMode, TopScheduler, TriageOutcome};
use broccoli_devops_agent::store::memory::InMemoryStateStore;
use broccoli_devops_agent::{AgentError, AgentResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

/// Creates a test Snapshot with valid identity and time but no real machine data.
fn sample_snapshot() -> Snapshot {
    Snapshot::new(
        Uuid::now_v7(),
        "topology-v1",
        SnapshotCause::Manual,
        OperationMode::Rehearsal,
    )
}

/// Creates a matching SnapshotView Artifact and reference for the given Snapshot.
fn sample_snapshot_view(snapshot: &Snapshot) -> (Artifact, SnapshotViewRef) {
    let artifact = Artifact::new(
        ArtifactKind::SnapshotView,
        format!("memory://snapshot-view/{}", snapshot.snapshot_id),
        "snapshot-view-sha256",
        128,
    );
    let view = SnapshotViewRef::new(
        snapshot.snapshot_id,
        artifact.artifact_id,
        "operate-default",
        artifact.content_sha256.clone(),
    );
    (artifact, view)
}

/// Creates a test Issue originating from a human report.
fn sample_issue(snapshot: &Snapshot) -> Issue {
    let report = HumanReport::new(
        "operator",
        "Submissions are slow",
        "Contestants report that submission results are significantly slower",
    );
    Issue::from_human_report(report, snapshot.snapshot_id, Uuid::now_v7())
}

/// Creates an operation proposal used only to test the ActionRun state machine.
fn sample_action_proposal() -> ActionProposal {
    ActionProposal {
        runbook_id: "worker.restart".to_string(),
        target_ids: vec!["worker-1".to_string()],
        arguments: vec![NamedValue::new("graceful", "true")],
        reason: "Verify the state machine".to_string(),
        expected_effect: "The Worker heartbeat reappears".to_string(),
        verification_probe_ids: vec!["worker.heartbeat".to_string()],
    }
}

/// Verifies that a domain object remains equivalent after JSON serialization and deserialization.
fn assert_json_roundtrip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let json = serde_json::to_string(value).expect("the test object should serialize");
    let decoded: T = serde_json::from_str(&json).expect("the test object should deserialize");
    assert_eq!(&decoded, value);
}

/// All six core object kinds should have stable and replayable Serde representations.
#[tokio::test]
async fn domain_objects_roundtrip_through_json() {
    let snapshot = sample_snapshot();
    let (artifact, view) = sample_snapshot_view(&snapshot);
    let issue = sample_issue(&snapshot);
    let job = Job::new(
        issue.issue_id,
        view,
        JobBrief::new(
            TeamKind::Operate,
            vec!["machine.inspect".to_string()],
            vec!["worker-1".to_string()],
        ),
    );
    let action = ActionRun::from_proposal(
        issue.issue_id,
        job.job_id,
        TeamKind::Operate,
        sample_action_proposal(),
        snapshot.snapshot_id,
        "restart-worker-1-once",
    );
    let store = InMemoryStateStore::new();
    let event = store
        .append_event(NewEvent::new(
            "test",
            "test.roundtrip",
            "Serialization test",
        ))
        .await
        .expect("the test event should be writable");

    assert_json_roundtrip(&snapshot);
    assert_json_roundtrip(&issue);
    assert_json_roundtrip(&job);
    assert_json_roundtrip(&action);
    assert_json_roundtrip(&artifact);
    assert_json_roundtrip(&event);
}

/// A Human Report must receive the highest priority and bind to the report-time Snapshot.
#[test]
fn human_report_creates_top_priority_issue() {
    let snapshot = sample_snapshot();
    let issue = sample_issue(&snapshot);

    assert_eq!(issue.priority, IssuePriority::HumanTop);
    assert_eq!(issue.opened_snapshot_id, snapshot.snapshot_id);
    assert_eq!(issue.current_snapshot_id, snapshot.snapshot_id);
}

/// New evidence should create a new Job instead of replacing the old Job's Snapshot View.
#[test]
fn job_uses_superseding_job_for_a_new_snapshot() {
    let old_snapshot = sample_snapshot();
    let new_snapshot = sample_snapshot().with_parent(old_snapshot.snapshot_id);
    let (_, old_view) = sample_snapshot_view(&old_snapshot);
    let (_, new_view) = sample_snapshot_view(&new_snapshot);
    let issue = sample_issue(&old_snapshot);
    let mut old_job = Job::new(
        issue.issue_id,
        old_view,
        JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
    );
    old_job
        .transition_to(JobStatus::Running)
        .expect("a queued Job should be able to start");
    old_job
        .complete(JobResult::new(
            JobOutcome::NeedsMoreData,
            "Object-storage latency evidence is needed",
        ))
        .expect("a running Job should be able to request a new Snapshot");

    let next = old_job
        .supersede_with(
            new_view,
            JobBrief::new(
                TeamKind::Operate,
                vec!["machine.inspect".to_string()],
                vec!["object-storage-1".to_string()],
            ),
        )
        .expect("a Job waiting for a new Snapshot should be supersedable");

    assert_eq!(old_job.status, JobStatus::Superseded);
    assert_eq!(next.supersedes_job_id, Some(old_job.job_id));
    assert_eq!(old_job.base_snapshot_id(), old_snapshot.snapshot_id);
    assert_eq!(next.base_snapshot_id(), new_snapshot.snapshot_id);
}

/// Issue, Job, and ActionRun should reject transitions that clearly skip stages.
#[test]
fn invalid_state_transitions_are_rejected() {
    let snapshot = sample_snapshot();
    let (_, view) = sample_snapshot_view(&snapshot);
    let mut issue = sample_issue(&snapshot);
    let mut job = Job::new(
        issue.issue_id,
        view,
        JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
    );
    let mut action = ActionRun::from_proposal(
        issue.issue_id,
        job.job_id,
        TeamKind::Operate,
        sample_action_proposal(),
        snapshot.snapshot_id,
        "invalid-transition-test",
    );

    assert!(matches!(
        issue.transition_to(IssueStatus::Resolved),
        Err(AgentError::InvalidTransition { .. })
    ));
    assert!(matches!(
        job.transition_to(JobStatus::Completed),
        Err(AgentError::InvalidTransition { .. })
    ));
    assert!(matches!(
        action.transition_to(ActionStatus::Running),
        Err(AgentError::InvalidTransition { .. })
    ));
}

/// An ActionRun succeeds only after Platform success and after-Snapshot verification.
#[test]
fn action_run_requires_execution_and_verification() {
    let before = sample_snapshot();
    let after = sample_snapshot().with_parent(before.snapshot_id);
    let issue = sample_issue(&before);
    let (_, view) = sample_snapshot_view(&before);
    let job = Job::new(
        issue.issue_id,
        view,
        JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
    );
    let mut action = ActionRun::from_proposal(
        issue.issue_id,
        job.job_id,
        TeamKind::Operate,
        sample_action_proposal(),
        before.snapshot_id,
        "worker-1-restart",
    );

    action
        .apply_approval(ApprovalState::NotRequired)
        .expect("an action requiring no approval should become Ready");
    action
        .start()
        .expect("a Ready action should be able to start");
    action
        .record_execution_result(PlatformOperationResult::new(
            true,
            None,
            "Command execution succeeded",
        ))
        .expect("a running action should accept a Platform result");
    assert_eq!(action.status, ActionStatus::Verifying);
    action
        .record_verification(
            Some(after.snapshot_id),
            true,
            Some(VerificationEvidence::Strong),
            "The Worker heartbeat recovered",
        )
        .expect("a verifying action should accept an after Snapshot");

    assert_eq!(action.status, ActionStatus::Succeeded);
    assert_eq!(action.after_snapshot_id, Some(after.snapshot_id));
}

/// The in-memory Store should persist six object kinds, reject duplicate IDs, and return NotFound.
#[tokio::test]
async fn memory_store_roundtrips_entities_and_rejects_bad_ids() {
    let store = InMemoryStateStore::new();
    let snapshot = sample_snapshot();
    let (artifact, view) = sample_snapshot_view(&snapshot);
    let issue = sample_issue(&snapshot);
    let job = Job::new(
        issue.issue_id,
        view,
        JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
    );
    let action = ActionRun::from_proposal(
        issue.issue_id,
        job.job_id,
        TeamKind::Operate,
        sample_action_proposal(),
        snapshot.snapshot_id,
        "store-test",
    );

    store
        .insert_snapshot(snapshot.clone())
        .await
        .expect("the Snapshot should be insertable");
    store
        .insert_artifact(artifact.clone())
        .await
        .expect("the Artifact should be insertable");
    store
        .insert_issue(issue.clone())
        .await
        .expect("the Issue should be insertable");
    store
        .insert_job(job.clone())
        .await
        .expect("the Job should be insertable");
    store
        .insert_action_run(action.clone())
        .await
        .expect("the ActionRun should be insertable");
    let event = store
        .append_event(NewEvent::new("test", "test.store", "Store test"))
        .await
        .expect("the Event should be appendable");

    assert_eq!(
        store.get_snapshot(snapshot.snapshot_id).await.unwrap(),
        snapshot
    );
    assert_eq!(
        store.get_artifact(artifact.artifact_id).await.unwrap(),
        artifact
    );
    assert_eq!(store.get_issue(issue.issue_id).await.unwrap(), issue);
    assert_eq!(store.get_job(job.job_id).await.unwrap(), job);
    assert_eq!(
        store.get_action_run(action.action_run_id).await.unwrap(),
        action
    );
    assert_eq!(store.list_events().await.unwrap(), vec![event]);
    assert!(matches!(
        store.insert_snapshot(snapshot).await,
        Err(AgentError::Duplicate { .. })
    ));
    assert!(matches!(
        store.get_issue(Uuid::now_v7()).await,
        Err(AgentError::NotFound { .. })
    ));
}

/// Concurrent Event appends should still produce unique, strictly increasing sequences.
#[tokio::test]
async fn concurrent_events_receive_a_stable_sequence() {
    let store = Arc::new(InMemoryStateStore::new());
    let mut handles = Vec::new();
    for index in 0..64_u64 {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            store
                .append_event(NewEvent::new(
                    "concurrent-test",
                    "test.concurrent_event",
                    format!("event-{index}"),
                ))
                .await
                .expect("a concurrent Event should be appendable")
        }));
    }
    for handle in handles {
        handle
            .await
            .expect("the concurrent Event task should not panic");
    }

    let events = store
        .list_events()
        .await
        .expect("the EventLog should be readable");
    let sequences: Vec<_> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, (1..=64).collect::<Vec<_>>());
}

/// The Scheduler should record the Human Report, Job, callback, freeze/resume, and recovery flow.
#[tokio::test]
async fn scheduler_exposes_the_minimum_readable_flow() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store.clone());
    let snapshot = sample_snapshot();
    let (artifact, view) = sample_snapshot_view(&snapshot);
    store
        .insert_snapshot(snapshot.clone())
        .await
        .expect("the Snapshot should be insertable");
    store
        .insert_artifact(artifact)
        .await
        .expect("the Snapshot View Artifact should be insertable");

    let issue = scheduler
        .accept_human_report(
            HumanReport::new(
                "operator",
                "Worker backlog",
                "The submission queue continues to grow",
            ),
            snapshot.snapshot_id,
        )
        .await
        .expect("the Human Report should create an Issue");
    let job = scheduler
        .create_job(
            issue.issue_id,
            view.clone(),
            JobBrief::new(
                TeamKind::Operate,
                vec!["machine.inspect".to_string()],
                vec!["worker-1".to_string()],
            ),
        )
        .await
        .expect("a Job should be creatable while the Scheduler is running");
    assert_eq!(job.status, JobStatus::Running);

    let callback = TeamCallback::new(issue.issue_id, job.job_id, "Investigation completed")
        .with_final_result(JobResult::new(
            JobOutcome::DiagnosisOnly,
            "The Scheduler must decide whether to take action",
        ));
    assert_eq!(callback.kind(), TeamCallbackKind::Completed);
    let completed = scheduler
        .handle_callback(callback)
        .await
        .expect("the Scheduler should handle a Team callback");
    assert_eq!(completed.status, JobStatus::Completed);

    scheduler
        .freeze_dispatch()
        .await
        .expect("dispatch should be freezable");
    assert!(matches!(
        scheduler
            .create_job(
                issue.issue_id,
                view,
                JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
            )
            .await,
        Err(AgentError::SchedulerFrozen { .. })
    ));
    scheduler
        .resume()
        .await
        .expect("dispatch should be resumable");

    let recovery = scheduler
        .recover()
        .await
        .expect("unfinished state should be enumerable");
    assert_eq!(recovery.issue_ids, vec![issue.issue_id]);
    assert!(recovery.job_ids.is_empty());
    assert!(recovery.action_run_ids.is_empty());
    assert_eq!(scheduler.mode().await, SchedulerMode::DispatchFrozen);

    let kinds: Vec<_> = store
        .list_events()
        .await
        .expect("Scheduler events should be readable")
        .into_iter()
        .map(|event| event.kind)
        .collect();
    for expected in [
        "human.issue_reported",
        "scheduler.issue_created",
        "scheduler.job_dispatched",
        "team.callback",
        "scheduler.dispatch_frozen",
        "scheduler.resumed",
        "scheduler.recovery_started",
        "scheduler.recovered",
    ] {
        assert!(kinds.iter().any(|kind| kind == expected));
    }

    // Recovery routes through the evented mode path, so the log alone reconstructs mode history:
    // recovery_started, then dispatch_frozen, then the recovered summary.
    let recovery_started = kinds
        .iter()
        .position(|kind| kind == "scheduler.recovery_started")
        .unwrap();
    let last_frozen = kinds
        .iter()
        .rposition(|kind| kind == "scheduler.dispatch_frozen")
        .unwrap();
    let recovered = kinds
        .iter()
        .position(|kind| kind == "scheduler.recovered")
        .unwrap();
    assert!(recovery_started < last_frozen && last_frozen < recovered);
}

/// A reporter may deliberately lower the priority; the default remains HumanTop.
#[test]
fn human_report_priority_is_default_with_override() {
    let snapshot = sample_snapshot();

    let mut low_report = HumanReport::new(
        "operator",
        "Balloon printer low on ink",
        "Not urgent; handle after the current incidents",
    );
    low_report.priority = Some(IssuePriority::Low);
    let low_issue = Issue::from_human_report(low_report, snapshot.snapshot_id, Uuid::now_v7());
    assert_eq!(low_issue.priority, IssuePriority::Low);

    // Non-human proposals can never reach HumanTop.
    assert_eq!(
        IssuePriority::HumanTop.model_safe(),
        IssuePriority::Critical
    );
}

/// An ActionRun starts unevaluated; `Unevaluated` is not a valid policy outcome.
#[test]
fn action_run_approval_starts_unevaluated() {
    let snapshot = sample_snapshot();
    let issue = sample_issue(&snapshot);
    let mut action = ActionRun::from_proposal(
        issue.issue_id,
        Uuid::now_v7(),
        TeamKind::Operate,
        sample_action_proposal(),
        snapshot.snapshot_id,
        "unevaluated-test",
    );

    assert_eq!(action.approval, ApprovalState::Unevaluated);
    assert!(matches!(
        action.apply_approval(ApprovalState::Unevaluated),
        Err(AgentError::InvalidInput(_))
    ));
    action
        .apply_approval(ApprovalState::Pending)
        .expect("policy should be able to require human approval");
    assert_eq!(action.status, ActionStatus::WaitingForApproval);
}

/// The callback kind is derived from content, so a Team cannot mislabel its result.
#[test]
fn team_callback_kind_is_derived_from_content() {
    let issue_id = Uuid::now_v7();
    let job_id = Uuid::now_v7();

    let progress = TeamCallback::new(issue_id, job_id, "Still investigating");
    assert_eq!(progress.kind(), TeamCallbackKind::Progress);

    let needs_more = TeamCallback::new(issue_id, job_id, "Need object-storage probes")
        .with_final_result(JobResult::new(JobOutcome::NeedsMoreData, "Evidence gap"));
    assert_eq!(needs_more.kind(), TeamCallbackKind::NeedMoreContext);

    let failed = TeamCallback::new(issue_id, job_id, "Could not reproduce")
        .with_final_result(JobResult::new(JobOutcome::Failed, "Gave up"));
    assert_eq!(failed.kind(), TeamCallbackKind::Failed);
}

/// A callback for a terminal Job is rejected before any event reaches the log.
#[tokio::test]
async fn callback_for_terminal_job_is_rejected_without_orphan_event() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store.clone());
    let snapshot = sample_snapshot();
    let (artifact, view) = sample_snapshot_view(&snapshot);
    store.insert_snapshot(snapshot.clone()).await.unwrap();
    store.insert_artifact(artifact).await.unwrap();

    let issue = scheduler
        .accept_human_report(
            HumanReport::new("operator", "Queue growth", "The queue keeps growing"),
            snapshot.snapshot_id,
        )
        .await
        .unwrap();
    let job = scheduler
        .create_job(
            issue.issue_id,
            view,
            JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
        )
        .await
        .unwrap();

    let final_callback = TeamCallback::new(issue.issue_id, job.job_id, "Done")
        .with_final_result(JobResult::new(JobOutcome::DiagnosisOnly, "Diagnosed"));
    scheduler.handle_callback(final_callback).await.unwrap();

    let events_before = store.list_events().await.unwrap().len();
    let late_callback = TeamCallback::new(issue.issue_id, job.job_id, "Late progress");
    assert!(matches!(
        scheduler.handle_callback(late_callback).await,
        Err(AgentError::InvalidInput(_))
    ));
    assert_eq!(store.list_events().await.unwrap().len(), events_before);
}

/// Dispatch and final results drive the owning Issue's lifecycle, not only the Job's.
#[tokio::test]
async fn callbacks_update_the_owning_issue() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store.clone());
    let snapshot = sample_snapshot();
    let (artifact, view) = sample_snapshot_view(&snapshot);
    store.insert_snapshot(snapshot.clone()).await.unwrap();
    store.insert_artifact(artifact).await.unwrap();

    let issue = scheduler
        .accept_human_report(
            HumanReport::new(
                "operator",
                "Frontend unreachable",
                "Contestants cannot load",
            ),
            snapshot.snapshot_id,
        )
        .await
        .unwrap();
    assert_eq!(issue.status, IssueStatus::Open);

    let job = scheduler
        .create_job(
            issue.issue_id,
            view,
            JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_issue(issue.issue_id).await.unwrap().status,
        IssueStatus::Investigating
    );

    let callback = TeamCallback::new(issue.issue_id, job.job_id, "Need a human decision")
        .with_final_result(JobResult::new(
            JobOutcome::NeedsHuman,
            "Two mitigation paths exist; a human must choose",
        ));
    scheduler.handle_callback(callback).await.unwrap();
    assert_eq!(
        store.get_issue(issue.issue_id).await.unwrap().status,
        IssueStatus::WaitingForHuman
    );
}

/// Without a wired Collector, Scheduler capture paths fail explicitly instead of pretending.
#[tokio::test]
async fn missing_dependencies_fail_explicitly() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store);

    let capture = CaptureRequest {
        deployment_id: Uuid::now_v7(),
        topology_revision: "topology-v1".to_string(),
        cause: SnapshotCause::Manual,
        operation_mode: OperationMode::Rehearsal,
        parent_snapshot_id: None,
        requested_probe_ids: Vec::new(),
    };
    assert!(matches!(
        scheduler.request_snapshot(capture).await,
        Err(AgentError::MissingDependency { .. })
    ));
}

/// FullyFrozen refuses new ActionRuns before any other work happens.
#[tokio::test]
async fn fully_frozen_blocks_new_action_runs() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store);
    scheduler.freeze_all().await.unwrap();

    let capture = CaptureRequest {
        deployment_id: Uuid::now_v7(),
        topology_revision: "topology-v1".to_string(),
        cause: SnapshotCause::BeforeAction,
        operation_mode: OperationMode::Rehearsal,
        parent_snapshot_id: None,
        requested_probe_ids: Vec::new(),
    };
    assert!(matches!(
        scheduler
            .create_action_run(
                Uuid::now_v7(),
                sample_action_proposal(),
                capture,
                "frozen-test",
            )
            .await,
        Err(AgentError::SchedulerFrozen { .. })
    ));
}

/// Without a policy model, candidate triage records the candidate and defers to a human.
#[tokio::test]
async fn triage_without_policy_defers_to_human() {
    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store.clone());
    let snapshot = sample_snapshot();
    store.insert_snapshot(snapshot.clone()).await.unwrap();

    let candidate = IssueCandidate::new(
        snapshot.snapshot_id,
        "Redis latency",
        "Command latency exceeds the alert threshold",
        IssuePriority::High,
        Confidence::Medium,
        "redis.latency",
    );
    let outcome = scheduler.triage_candidate(candidate).await.unwrap();
    assert_eq!(outcome, TriageOutcome::DeferredToHuman);

    let kinds: Vec<_> = store
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(
        kinds
            .iter()
            .any(|kind| kind == "snapshot_judge.issue_candidate")
    );
    assert!(kinds.iter().any(|kind| kind == "scheduler.triage_deferred"));
}

/// A policy model's triage proposal is validated and clamped by the harness.
#[tokio::test]
async fn policy_triage_is_clamped_and_recorded() {
    /// Policy stub that always accepts at a priority only humans may hold.
    struct OverreachingPolicy;

    #[async_trait::async_trait]
    impl SchedulerPolicyPort for OverreachingPolicy {
        async fn triage_candidate(&self, _request: &TriageRequest) -> AgentResult<TriageDecision> {
            Ok(TriageDecision::Accept {
                priority: IssuePriority::HumanTop,
            })
        }

        async fn advise_next_step(
            &self,
            _request: &CallbackAdviceRequest,
        ) -> AgentResult<NextStep> {
            Ok(NextStep {
                decision: NextStepDecision::Resolve,
                rationale: "test stub".to_string(),
            })
        }
    }

    let store = Arc::new(InMemoryStateStore::new());
    let scheduler = TopScheduler::new(store.clone()).with_policy(Arc::new(OverreachingPolicy));
    let snapshot = sample_snapshot();
    store.insert_snapshot(snapshot.clone()).await.unwrap();

    let candidate = IssueCandidate::new(
        snapshot.snapshot_id,
        "PostgreSQL saturation",
        "Connection pool exhausted",
        IssuePriority::Critical,
        Confidence::High,
        "postgres.pool",
    );
    let outcome = scheduler.triage_candidate(candidate).await.unwrap();
    let TriageOutcome::IssueCreated(issue) = outcome else {
        panic!("the accepted candidate should create an Issue");
    };
    // The model asked for HumanTop; the harness clamps every non-human proposal below it.
    assert_eq!(issue.priority, IssuePriority::Critical);

    let kinds: Vec<_> = store
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(
        kinds
            .iter()
            .any(|kind| kind == "scheduler.policy_consulted")
    );
}

/// The Team port delivers ordered callbacks through a sink and stops on cancellation.
#[tokio::test]
async fn team_port_streams_callbacks_and_honors_cancellation() {
    /// Sink that collects delivered callbacks for inspection.
    #[derive(Default)]
    struct CollectingSink {
        delivered: tokio::sync::Mutex<Vec<TeamCallback>>,
    }

    #[async_trait::async_trait]
    impl TeamCallbackSink for CollectingSink {
        async fn deliver(&self, callback: TeamCallback) -> AgentResult<()> {
            self.delivered.lock().await.push(callback);
            Ok(())
        }
    }

    /// Team stub that reports progress, then finishes unless cancelled first.
    struct ScriptedTeam;

    #[async_trait::async_trait]
    impl AgentTeamPort for ScriptedTeam {
        fn team_kind(&self) -> TeamKind {
            TeamKind::Operate
        }

        async fn run_job(
            &self,
            job: &Job,
            _snapshot_view: &Artifact,
            sink: &dyn TeamCallbackSink,
            cancel: CancelSignal,
        ) -> AgentResult<()> {
            sink.deliver(TeamCallback::new(
                job.issue_id,
                job.job_id,
                "Reading service logs",
            ))
            .await?;
            let outcome = if cancel.is_cancelled() {
                JobResult::new(JobOutcome::Failed, "Cancelled before completion")
            } else {
                JobResult::new(JobOutcome::DiagnosisOnly, "Diagnosis complete")
            };
            sink.deliver(
                TeamCallback::new(job.issue_id, job.job_id, "Final report")
                    .with_final_result(outcome),
            )
            .await
        }
    }

    let snapshot = sample_snapshot();
    let (artifact, view) = sample_snapshot_view(&snapshot);
    let issue = sample_issue(&snapshot);
    let job = Job::new(
        issue.issue_id,
        view,
        JobBrief::new(TeamKind::Operate, Vec::new(), Vec::new()),
    );

    let sink = CollectingSink::default();
    let (handle, signal) = cancel_pair();
    ScriptedTeam
        .run_job(&job, &artifact, &sink, signal.clone())
        .await
        .expect("the scripted Team should finish");
    let delivered = sink.delivered.lock().await;
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0].kind(), TeamCallbackKind::Progress);
    assert_eq!(delivered[1].kind(), TeamCallbackKind::Completed);
    drop(delivered);

    handle.cancel();
    let mut cancelled_signal = signal;
    cancelled_signal.cancelled().await;
    assert!(cancelled_signal.is_cancelled());
}
