//! Proves the port abstraction: the harness-backed Operate Team implements the same
//! `AgentTeamPort` as the deterministic one, driven here by a scripted model backend.

use std::sync::Arc;

use broccoli_agent_harness::AgentConfig;
use broccoli_agent_harness::testing::{ScriptedModelClient, call, text};
use broccoli_devops_agent::AgentResult;
use broccoli_devops_agent::domain::{
    ArtifactKind, HumanReport, Issue, Job, JobBrief, JobOutcome, JobStatus, OperationMode,
    Snapshot, SnapshotCause, TeamCallback, TeamCallbackKind, TeamKind,
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
        issue: issue.clone(),
        brief: JobBrief::new(
            TeamKind::Operate,
            vec!["observe.readonly".into()],
            vec!["broccoli-server".into()],
        ),
        redaction_profile: PROFILE_OPERATE_READONLY.into(),
    };
    let built = RedactingViewBuilder::new(artifacts.clone())
        .build_snapshot_view(&snapshot, &request)
        .await
        .unwrap();
    store.insert_artifact(built.artifact.clone()).await.unwrap();

    let mut job = Job::new(issue.issue_id, built.snapshot_view, request.brief);
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
    // Exactly one callback ends the pass, and it comes last; everything before it is progress,
    // delivered while the run was still going.
    let (last, progress) = delivered.split_last().unwrap();
    assert!(
        progress
            .iter()
            .all(|callback| callback.kind() == TeamCallbackKind::Progress)
    );
    assert_eq!(last.kind(), TeamCallbackKind::Completed);
    assert_eq!(last.issue_id, issue.issue_id);

    // The loop's own steps and the model's `report_progress` share one ordered stream, so an
    // operator watching the console sees the pass advance rather than a silent gap.
    let lines: Vec<&str> = progress
        .iter()
        .map(|callback| callback.summary.as_str())
        .collect();
    assert!(
        lines.iter().any(|line| line.contains("model turn 1/")),
        "each model turn is announced: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Running `read_snapshot_view`")),
        "each tool call is announced: {lines:?}"
    );
    assert_eq!(
        lines.iter().position(|line| *line == "Reading the view"),
        lines
            .iter()
            .position(|line| line.contains("Running `report_progress`"))
            .map(|index| index + 1),
        "the model's own progress line follows the step that produced it"
    );

    let result = last.final_result.as_ref().unwrap();
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
    assert!(transcript.instructions.contains("broccoli-server"));
    // The problem statement reaches the model only inside the fenced View, never in the
    // instructions: the reporter's words are data.
    assert!(!transcript.instructions.contains("Submit errors"));
    let view_output = transcript
        .entries
        .iter()
        .map(|entry| serde_json::to_string(&entry.item).unwrap())
        .find(|text| text.contains("BEGIN UNTRUSTED DATA"))
        .expect("the model read the fenced View");
    assert!(view_output.contains("Submit errors"));
    assert!(view_output.contains("500s on submit"));
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
