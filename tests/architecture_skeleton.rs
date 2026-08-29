//! Integration tests that explain the initial architecture scaffold through behavior examples.

use std::fmt::Debug;
use std::sync::Arc;

use broccoli_devops_agent::AgentError;
use broccoli_devops_agent::domain::{
    ActionProposal, ActionRun, ActionStatus, ApprovalState, Artifact, ArtifactKind, HumanReport,
    Issue, IssuePriority, IssueStatus, Job, JobOutcome, JobResult, JobStatus, NamedValue, NewEvent,
    OperationMode, PlatformOperationResult, Snapshot, SnapshotCause, SnapshotViewRef, TeamCallback,
    TeamCallbackKind, TeamKind, WorkOrder,
};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::scheduler::{SchedulerMode, TopScheduler};
use broccoli_devops_agent::store::memory::InMemoryStateStore;
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
        TeamKind::Operate,
        WorkOrder::new("Analyze submission latency"),
        vec!["machine.inspect".to_string()],
        vec!["worker-1".to_string()],
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
        TeamKind::Operate,
        WorkOrder::new("Investigate the old Snapshot"),
        Vec::new(),
        Vec::new(),
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
            WorkOrder::new("Continue investigation with the new Snapshot"),
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
        TeamKind::Operate,
        WorkOrder::new("State transition test"),
        Vec::new(),
        Vec::new(),
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
        TeamKind::Operate,
        WorkOrder::new("Restart the Worker"),
        Vec::new(),
        Vec::new(),
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
        .record_verification(after.snapshot_id, true, "The Worker heartbeat recovered")
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
        TeamKind::Operate,
        WorkOrder::new("Store test"),
        Vec::new(),
        Vec::new(),
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
            TeamKind::Operate,
            WorkOrder::new("Investigate the Worker fleet and queue"),
            vec!["machine.inspect".to_string()],
            vec!["worker-1".to_string()],
        )
        .await
        .expect("a Job should be creatable while the Scheduler is running");
    assert_eq!(job.status, JobStatus::Running);

    let mut callback = TeamCallback::new(
        issue.issue_id,
        job.job_id,
        TeamCallbackKind::Completed,
        "Investigation completed",
    );
    callback.final_result = Some(JobResult::new(
        JobOutcome::DiagnosisOnly,
        "The Scheduler must decide whether to take action",
    ));
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
                TeamKind::Operate,
                WorkOrder::new("This Job must not be created while frozen"),
                Vec::new(),
                Vec::new(),
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
        "scheduler.recovered",
    ] {
        assert!(kinds.iter().any(|kind| kind == expected));
    }
}
