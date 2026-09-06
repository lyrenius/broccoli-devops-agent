//! Context history: a running pass forwards its transcript entry by entry as `team.step` events,
//! the stored transcript carries per-turn records, and an Issue with its whole pass chain
//! exports as one JSON document that imports elsewhere as a read-only archive — visible, and
//! ignored by every control decision.

use std::net::TcpListener;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_agent_harness::{AssistantItem, ModelClient, Transcript, Usage};
use broccoli_devops_agent::AgentError;
use broccoli_devops_agent::domain::{HumanReport, JobOutcome, OperationMode, ResourceKind};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{PassPolicy, SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::IssueClosure;
use broccoli_devops_agent::session::{
    ArtifactBody, SESSION_FORMAT, SESSION_VERSION, SessionBundle,
};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::json;
use uuid::Uuid;

fn topology(name: &str, worker_port: u16, redis_port: u16) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: name.into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![
            TopologyResource {
                id: "worker-1".into(),
                kind: ResourceKind::Worker,
                node: None,
                probes: vec![ProbeSpec {
                    target: Some(format!("127.0.0.1:{worker_port}")),
                    url: None,
                    ..ProbeSpec::new("tcp.connect")
                }],
            },
            TopologyResource {
                id: "redis-mq".into(),
                kind: ResourceKind::Redis,
                node: None,
                probes: vec![ProbeSpec {
                    target: Some(format!("127.0.0.1:{redis_port}")),
                    url: None,
                    ..ProbeSpec::new("tcp.connect")
                }],
            },
        ],
        dependencies: Vec::new(),
    }
}

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn platform_config() -> PlatformConfig {
    PlatformConfig {
        dry_run: true,
        runbooks: vec![RunbookCommand {
            id: "mq.purge".into(),
            command: "echo purge {target}".into(),
        }],
        ..PlatformConfig::default()
    }
}

fn scripted_runner(
    dir: &std::path::Path,
    name: &str,
    turns: Vec<Vec<AssistantItem>>,
) -> SliceRunner {
    let client = Arc::new(
        ScriptedModelClient::new(turns).with_usage_per_turn(Usage::reported(1_000, 100, 50)),
    );
    SliceRunner::wire_with(
        topology(name, closed_port(), closed_port()),
        dir,
        TeamBackend::Harness {
            client: client as Arc<dyn ModelClient>,
            budget: Default::default(),
            model: "test-model".into(),
            label: "scripted".into(),
        },
        platform_config(),
        PassPolicy {
            max_auto_passes: 1,
            max_inspections: 2,
        },
    )
    .unwrap()
}

fn readonly_runner(dir: &std::path::Path, name: &str) -> SliceRunner {
    SliceRunner::wire(
        topology(name, closed_port(), closed_port()),
        dir,
        TeamBackend::ReadOnly,
        platform_config(),
    )
    .unwrap()
}

/// A pass that reads the View, reports progress, proposes a queue purge (held for approval in
/// Rehearsal), and concludes.
fn investigation() -> Vec<Vec<AssistantItem>> {
    vec![
        vec![call("c1", "read_snapshot_view", json!({}))],
        vec![
            call(
                "c2",
                "report_progress",
                json!({ "summary": "the queue looks stuck" }),
            ),
            call(
                "c3",
                "propose_action",
                json!({
                    "runbook_id": "mq.purge",
                    "target_ids": ["redis-mq"],
                    "reason": "the queue is backed up",
                    "expected_effect": "submissions flow again",
                }),
            ),
        ],
        vec![call(
            "c4",
            "submit_diagnosis",
            json!({ "summary": "the queue is backed up", "outcome": "diagnosis_only" }),
        )],
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_pass_is_traced_step_by_step_and_its_transcript_records_turns() {
    let dir = tempfile::tempdir().unwrap();
    let runner = scripted_runner(dir.path(), "source", investigation());
    let (issue, job) = runner
        .handle_report(HumanReport::new("tester", "stuck", "submissions time out"))
        .await
        .unwrap();
    let store = runner.store();

    // Every transcript entry went out as a `team.step` event while the pass ran, in order.
    let steps: Vec<_> = store
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "team.step" && event.job_id == Some(job.job_id))
        .collect();
    let indices: Vec<u64> = steps
        .iter()
        .map(|event| event.payload["step"]["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indices, (0..steps.len() as u64).collect::<Vec<_>>());
    let kinds: Vec<&str> = steps
        .iter()
        .map(|event| event.payload["step"]["item"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[0], "user_input", "the inputs are traced first");
    assert!(kinds.contains(&"tool_call") && kinds.contains(&"tool_output"));
    assert_eq!(
        steps[1].payload["step"]["item"]["tool"], "read_snapshot_view",
        "the first thing the model did was read the View"
    );
    assert!(
        steps
            .iter()
            .all(|event| event.issue_id == Some(issue.issue_id))
    );

    // The stored transcript is the same entries, plus one record per model request.
    let job = store.get_job(job.job_id).await.unwrap();
    let artifact = store
        .get_artifact(job.result.as_ref().unwrap().artifact_ids[0])
        .await
        .unwrap();
    let transcript: Transcript =
        serde_json::from_slice(&runner.artifacts().read_verified(&artifact).unwrap()).unwrap();
    assert_eq!(transcript.entries.len(), steps.len());
    for (event, entry) in steps.iter().zip(&transcript.entries) {
        assert_eq!(
            event.payload["step"]["item"],
            serde_json::to_value(&entry.item).unwrap()
        );
    }
    assert_eq!(transcript.turns.len(), 3);
    assert!(
        transcript
            .turns
            .iter()
            .all(|turn| turn.usage == Usage::reported(1_000, 100, 50))
    );
    assert!(
        transcript.turns[0]
            .offered_tools
            .contains(&"propose_action".to_string())
    );
    assert!(
        store
            .list_events()
            .await
            .unwrap()
            .iter()
            .any(|event| event.kind == "team.callback"
                && event.job_id == Some(job.job_id)
                && event.summary.contains("stuck")),
        "the model's own progress line is still a callback event"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_exports_as_one_document_and_imports_as_a_read_only_archive() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = scripted_runner(source_dir.path(), "source", investigation());
    let (issue, job) = source
        .handle_report(HumanReport::new("tester", "stuck", "submissions time out"))
        .await
        .unwrap();
    // The chain after the pass: the proposal goes through the matrix and is held.
    source.drive_passes(job.clone()).await.unwrap();
    assert_eq!(
        source.inbox().await.unwrap().permission_requests.len(),
        1,
        "the purge waits for approval at the source"
    );

    // Export: everything about the Issue, bodies as readable JSON, round-trippable.
    let bundle = source
        .export_session(issue.issue_id, "alice")
        .await
        .unwrap();
    assert_eq!(bundle.format, SESSION_FORMAT);
    assert_eq!(bundle.version, SESSION_VERSION);
    assert_eq!(bundle.exported_by, "alice");
    assert_eq!(bundle.deployment, "source");
    assert_eq!(bundle.jobs.len(), 1);
    assert_eq!(bundle.action_runs.len(), 1);
    assert!(!bundle.snapshots.is_empty());
    assert!(
        bundle.artifacts.len() >= 2,
        "the View the model read and the transcript are both in the file"
    );
    assert!(
        bundle
            .artifacts
            .iter()
            .all(|item| matches!(item.body, ArtifactBody::Json(_))),
        "bodies the control plane wrote are readable JSON in the file"
    );
    let transcript_ids = &bundle.jobs[0].result.as_ref().unwrap().artifact_ids;
    assert!(
        bundle
            .artifacts
            .iter()
            .any(|item| transcript_ids.contains(&item.artifact.artifact_id))
    );
    let kinds: Vec<&str> = bundle.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"human.issue_reported"),
        "the opening report travels too"
    );
    assert!(kinds.contains(&"team.step"));
    assert!(kinds.contains(&"scheduler.action_created"));
    let json = serde_json::to_vec_pretty(&bundle).unwrap();
    let parsed: SessionBundle = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed, bundle);

    // Import into another controller: same records, marked as an archive.
    let target_dir = tempfile::tempdir().unwrap();
    let target = readonly_runner(target_dir.path(), "target");
    let summary = target.import_session(parsed.clone(), "bob").await.unwrap();
    assert_eq!(summary.issue_id, issue.issue_id);
    assert_eq!(summary.jobs, 1);
    assert_eq!(summary.action_runs, 1);
    assert_eq!(summary.artifacts, bundle.artifacts.len());
    assert_eq!(summary.events, bundle.events.len());

    let store = target.store();
    let archived = store.get_issue(issue.issue_id).await.unwrap();
    assert!(archived.is_archived());
    let provenance = archived
        .provenance
        .expect("an imported Issue names its origin");
    assert_eq!(provenance.source_deployment, "source");
    assert_eq!(provenance.exported_by, "alice");
    assert_eq!(provenance.imported_by, "bob");
    let imported_job = store.get_job(job.job_id).await.unwrap();
    assert_eq!(
        imported_job.result.as_ref().unwrap().outcome,
        JobOutcome::DiagnosisOnly
    );

    // The transcript is readable and hash-verified at the target, byte for byte.
    let artifact = store.get_artifact(transcript_ids[0]).await.unwrap();
    let bytes = target.artifacts().read_verified(&artifact).unwrap();
    let original = source
        .artifacts()
        .read_verified(
            &source
                .store()
                .get_artifact(transcript_ids[0])
                .await
                .unwrap(),
        )
        .unwrap();
    assert_eq!(bytes, original);
    assert_ne!(
        artifact.uri,
        bundle
            .artifacts
            .iter()
            .find(|a| a.artifact.artifact_id == artifact.artifact_id)
            .unwrap()
            .artifact
            .uri,
        "the body lives under the target's own root"
    );

    // Events keep their identity and order; the import itself is on the record.
    let events = store.list_events().await.unwrap();
    let imported: Vec<_> = events
        .iter()
        .filter(|e| e.issue_id == Some(issue.issue_id) && e.kind != "human.session_imported")
        .collect();
    assert_eq!(
        imported.len(),
        bundle
            .events
            .iter()
            .filter(|e| e.issue_id == Some(issue.issue_id))
            .count()
    );
    assert!(
        imported
            .iter()
            .zip(
                bundle
                    .events
                    .iter()
                    .filter(|e| e.issue_id == Some(issue.issue_id))
            )
            .all(|(here, there)| here.event_id == there.event_id
                && here.occurred_at == there.occurred_at)
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "human.session_imported" && e.summary.contains("bob"))
    );

    // An archive is read-only: nothing waits for a human, recovery sees no work, its spend is
    // not this controller's, and no decision can touch it.
    assert_eq!(
        target.inbox().await.unwrap().total(),
        0,
        "the source's permission request is history here"
    );
    let recovery = target.recover().await.unwrap();
    assert!(recovery.issue_ids.is_empty() && recovery.action_run_ids.is_empty());
    assert_eq!(target.usage_totals().await.unwrap().passes, 0);
    assert_eq!(source.usage_totals().await.unwrap().passes, 1);
    let refused = target
        .close_issue(issue.issue_id, IssueClosure::Resolved, "bob", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, AgentError::InvalidInput(text) if text.contains("archive")),
        "{refused}"
    );
    let action_id = bundle.action_runs[0].action_run_id;
    let refused = target.approve_action(action_id, "bob").await.unwrap_err();
    assert!(
        matches!(&refused, AgentError::InvalidInput(text) if text.contains("archive")),
        "{refused}"
    );

    // Importing the same session twice is refused, and so is a file whose body was altered.
    assert!(matches!(
        target
            .import_session(parsed.clone(), "bob")
            .await
            .unwrap_err(),
        AgentError::Duplicate {
            entity: "Issue",
            ..
        }
    ));
    let mut altered = parsed;
    if let ArtifactBody::Json(value) = &mut altered.artifacts[0].body {
        value["tampered"] = json!(true);
    }
    let other_dir = tempfile::tempdir().unwrap();
    let other = readonly_runner(other_dir.path(), "other");
    let refused = other.import_session(altered, "bob").await.unwrap_err();
    assert!(
        matches!(&refused, AgentError::InvalidInput(text) if text.contains("hash")),
        "{refused}"
    );
    assert!(
        other.store().get_issue(issue.issue_id).await.is_err(),
        "nothing was written"
    );

    // A file that is not a session file is refused as such.
    let mut wrong = bundle;
    wrong.format = "something-else".into();
    assert!(
        matches!(other.import_session(wrong, "bob").await.unwrap_err(), AgentError::InvalidInput(text) if text.contains("not a session file"))
    );
}
