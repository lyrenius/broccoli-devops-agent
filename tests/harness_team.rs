//! Proves the port abstraction: the harness-backed Operate Team implements the same
//! `AgentTeamPort` as the deterministic one, driven here by a scripted model backend.

use std::sync::Arc;

use broccoli_agent_harness::AgentConfig;
use broccoli_agent_harness::testing::{ScriptedModelClient, call, text};
use broccoli_devops_agent::AgentResult;
use broccoli_devops_agent::domain::{
    ArtifactKind, HumanReport, Issue, Job, JobOutcome, JobStatus, OperationMode, Snapshot,
    SnapshotCause, TeamCallback, TeamCallbackKind, TeamKind, WorkOrder,
};
use broccoli_devops_agent::ports::{
    AgentTeamPort, SnapshotViewBuildRequest, SnapshotViewBuilderPort, StateStore, TeamCallbackSink,
    cancel_pair,
};
use broccoli_devops_agent::store::memory::InMemoryStateStore;
use broccoli_devops_agent::team::HarnessOperateTeam;
use broccoli_devops_agent::view::{
    FileArtifactStore, PROFILE_OPERATE_READONLY, RedactingViewBuilder,
};
use serde_json::json;
use uuid::Uuid;

/// Sink that records every delivered callback.
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

/// Builds a persisted Snapshot View and a Job bound to it, over a temp artifact store.
async fn job_with_view(
    store: &Arc<InMemoryStateStore>,
    artifacts: &FileArtifactStore,
) -> (Issue, Job, broccoli_devops_agent::domain::Artifact) {
    let snapshot = Snapshot::new(
        Uuid::now_v7(),
        "test-rev",
        SnapshotCause::HumanReport,
        OperationMode::Rehearsal,
    );
    store.insert_snapshot(snapshot.clone()).await.unwrap();

    let issue = Issue::from_human_report(
        HumanReport::new("operator", "Submit errors", "500s on submit"),
        snapshot.snapshot_id,
        Uuid::now_v7(),
    );
    store.insert_issue(issue.clone()).await.unwrap();

    let request = SnapshotViewBuildRequest {
        issue_id: issue.issue_id,
        team_kind: TeamKind::Operate,
        work_order: WorkOrder::new("Diagnose submit failures"),
        allowed_capabilities: vec!["observe.readonly".into()],
        allowed_target_ids: vec!["broccoli-server".into()],
        redaction_profile: PROFILE_OPERATE_READONLY.into(),
    };
    let built = RedactingViewBuilder::new(artifacts.clone())
        .build_snapshot_view(&snapshot, &request)
        .await
        .unwrap();
    store.insert_artifact(built.artifact.clone()).await.unwrap();

    let mut job = Job::new(
        issue.issue_id,
        built.snapshot_view,
        TeamKind::Operate,
        request.work_order,
        request.allowed_capabilities,
        request.allowed_target_ids,
    );
    job.transition_to(JobStatus::Running).unwrap();
    store.insert_job(job.clone()).await.unwrap();
    (issue, job, built.artifact)
}

/// The scripted model reads the view, reports progress, and submits a structured diagnosis;
/// the adapter turns that into ordered callbacks, a DiagnosisOnly result, and a stored transcript.
#[tokio::test]
async fn harness_team_runs_a_job_through_the_same_port() {
    let store = Arc::new(InMemoryStateStore::new());
    let dir = tempfile::tempdir().unwrap();
    let artifacts = FileArtifactStore::new(dir.path());
    let (issue, job, view_artifact) = job_with_view(&store, &artifacts).await;

    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "read_snapshot_view", json!({}))],
        vec![
            text("The server view shows failing dependencies."),
            call(
                "c2",
                "report_progress",
                json!({"summary": "Reading the view"}),
            ),
        ],
        vec![call(
            "c3",
            "submit_diagnosis",
            json!({
                "summary": "broccoli-server is failing because postgres-main is down",
                "unresolved_questions": ["Is the postgres host reachable at all?"],
            }),
        )],
    ]);

    let team = HarnessOperateTeam::new(Arc::new(client), artifacts.clone(), store.clone());
    let sink = CollectingSink::default();
    let (_handle, signal) = cancel_pair();
    team.run_job(&job, &view_artifact, &sink, signal)
        .await
        .unwrap();

    let delivered = sink.delivered.lock().await;
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0].kind(), TeamCallbackKind::Progress);
    assert_eq!(delivered[0].summary, "Reading the view");
    assert_eq!(delivered[1].kind(), TeamCallbackKind::Completed);
    assert_eq!(delivered[1].issue_id, issue.issue_id);

    let result = delivered[1].final_result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::DiagnosisOnly);
    assert!(result.summary.contains("postgres-main is down"));
    assert_eq!(result.unresolved_questions.len(), 1);

    // The full run transcript is registered as a hash-verified DiagnosticBundle Artifact.
    let transcript_id = *result.artifact_ids.first().unwrap();
    let transcript_artifact = store.get_artifact(transcript_id).await.unwrap();
    assert_eq!(transcript_artifact.kind, ArtifactKind::DiagnosticBundle);
    assert_eq!(transcript_artifact.produced_by_job_id, Some(job.job_id));
    let bytes = artifacts.read_verified(&transcript_artifact).unwrap();
    let transcript: broccoli_agent_harness::Transcript = serde_json::from_slice(&bytes).unwrap();
    assert!(transcript.instructions.contains("Diagnose submit failures"));
    assert!(transcript.entries.iter().any(|entry| {
        serde_json::to_string(&entry.item)
            .unwrap()
            .contains("BEGIN UNTRUSTED DATA")
    }));
}

/// A model that ends with prose instead of the terminal tool produces a Failed result — the
/// runtime enforces structured output rather than guessing.
#[tokio::test]
async fn prose_without_submit_diagnosis_fails_the_job() {
    let store = Arc::new(InMemoryStateStore::new());
    let dir = tempfile::tempdir().unwrap();
    let artifacts = FileArtifactStore::new(dir.path());
    let (_issue, job, view_artifact) = job_with_view(&store, &artifacts).await;

    let client = ScriptedModelClient::new(vec![vec![text("Everything looks fine, done!")]]);
    let team = HarnessOperateTeam::new(Arc::new(client), artifacts.clone(), store.clone());
    let sink = CollectingSink::default();
    let (_handle, signal) = cancel_pair();
    team.run_job(&job, &view_artifact, &sink, signal)
        .await
        .unwrap();

    let delivered = sink.delivered.lock().await;
    let result = delivered.last().unwrap().final_result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::Failed);
    assert!(result.summary.contains("without calling submit_diagnosis"));
}

/// A run that exhausts its turn budget also fails cleanly, with the transcript preserved.
#[tokio::test]
async fn exhausted_budget_fails_the_job_with_transcript() {
    let store = Arc::new(InMemoryStateStore::new());
    let dir = tempfile::tempdir().unwrap();
    let artifacts = FileArtifactStore::new(dir.path());
    let (_issue, job, view_artifact) = job_with_view(&store, &artifacts).await;

    let turns = (0..4)
        .map(|i| vec![call(format!("c{i}"), "read_snapshot_view", json!({}))])
        .collect();
    let team = HarnessOperateTeam::new(
        Arc::new(ScriptedModelClient::new(turns)),
        artifacts.clone(),
        store.clone(),
    )
    .with_config(AgentConfig {
        max_model_turns: 2,
        ..AgentConfig::default()
    });

    let sink = CollectingSink::default();
    let (_handle, signal) = cancel_pair();
    team.run_job(&job, &view_artifact, &sink, signal)
        .await
        .unwrap();

    let delivered = sink.delivered.lock().await;
    let result = delivered.last().unwrap().final_result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::Failed);
    assert!(result.summary.contains("model-turn budget"));
    assert!(!result.artifact_ids.is_empty(), "transcript still stored");
}
