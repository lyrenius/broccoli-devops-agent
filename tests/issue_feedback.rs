//! Completed investigations remain actionable, and closure comments remain visible.
use std::net::TcpListener;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_devops_agent::api::{ApiState, router};
use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{
    FeedbackOrigin, HumanReport, IssueStatus, JobStatus, OperationMode, ResourceKind,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn topology(port: u16) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "feedback-test".into(),
            topology_revision: "v1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![TopologyResource {
            id: "redis-mq".into(),
            kind: ResourceKind::Redis,
            node: None,
            probes: vec![ProbeSpec {
                target: Some(format!("127.0.0.1:{port}")),
                ..ProbeSpec::new("tcp.connect")
            }],
        }],
        dependencies: vec![],
    }
}

fn diagnosis(id: &str) -> broccoli_agent_harness::AssistantItem {
    call(
        id,
        "submit_diagnosis",
        json!({"summary":"Need the operator's observations", "unresolved_questions":["What changed?"], "outcome":"diagnosis_only"}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_issue_feedback_reaches_a_fresh_pass_and_closure_notes_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let topo = topology(listener.local_addr().unwrap().port());
    let client = Arc::new(ScriptedModelClient::new(vec![
        vec![diagnosis("initial")],
        vec![call("view", "read_snapshot_view", json!({}))],
        vec![diagnosis("continued")],
    ]));
    let runner = Arc::new(
        SliceRunner::wire(
            topo.clone(),
            dir.path(),
            TeamBackend::Harness {
                client,
                budget: Default::default(),
                model: "scripted".into(),
                label: "scripted".into(),
            },
            PlatformConfig::default(),
        )
        .unwrap(),
    );
    let (issue, job) = runner
        .handle_report(HumanReport::new(
            "operator",
            "Investigate",
            "Need more context",
        ))
        .await
        .unwrap();
    runner.drive_passes(job.clone()).await.unwrap();
    assert_eq!(
        runner
            .store()
            .get_issue(issue.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::WaitingForHuman
    );
    let inbox = runner.inbox().await.unwrap();
    assert!(inbox.failed_jobs.is_empty());
    assert_eq!(inbox.waiting_issues.len(), 1);
    assert_eq!(inbox.total(), 1);
    assert_eq!(inbox.waiting_issues[0].job.status, JobStatus::Completed);

    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            socket,
            router(Arc::new(ApiState::new(
                runner.clone(),
                AppConfig::default(),
            ))),
        )
        .into_future(),
    );
    let http = reqwest::Client::new();
    let feedback_url = format!("{url}/api/issues/{}/feedback", issue.issue_id);
    let body = json!({"expected_job_id":job.job_id,"by":"Alice","comment":"Redis pause has ended; inspect fresh evidence."});
    let empty = http
        .post(&feedback_url)
        .json(&json!({"expected_job_id":job.job_id,"comment":"  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);
    let response = http.post(&feedback_url).json(&body).send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let revision: Value = response.json().await.unwrap();
    let revised_id: Uuid = serde_json::from_value(revision["job"]["job_id"].clone()).unwrap();
    let revised = runner.store().get_job(revised_id).await.unwrap();
    assert_eq!(revised.revises_job_id, Some(job.job_id));
    assert_ne!(revised.base_snapshot_id(), job.base_snapshot_id());
    assert_eq!(
        revised.feedback.last().unwrap().comment.as_deref(),
        body["comment"].as_str()
    );
    assert!(matches!(
        revised.feedback.last().unwrap().origin,
        FeedbackOrigin::IssueComment { .. }
    ));
    let artifact = runner
        .store()
        .get_artifact(revised.snapshot_view.artifact_id)
        .await
        .unwrap();
    let view = std::fs::read_to_string(&artifact.uri).unwrap();
    assert!(view.contains("Redis pause has ended; inspect fresh evidence."));
    assert!(view.contains("issue_comment"));
    assert!(
        runner
            .store()
            .get_job(job.job_id)
            .await
            .unwrap()
            .review
            .is_none()
    );
    assert_eq!(
        runner.inbox().await.unwrap().waiting_issues[0].job.job_id,
        revised_id
    );

    let duplicate = http.post(&feedback_url).json(&body).send().await.unwrap();
    assert_eq!(duplicate.status(), 400);
    assert_eq!(runner.store().list_jobs().await.unwrap().len(), 2);

    let close = http.post(format!("{url}/api/issues/{}/close", issue.issue_id))
        .json(&json!({"outcome":"resolved","by":"Alice","comment":"Recovered; all three submissions passed."})).send().await.unwrap();
    assert_eq!(close.status(), 200);
    let closed: Value = close.json().await.unwrap();
    assert_eq!(
        closed["closure"]["comment"],
        "Recovered; all three submissions passed."
    );
    assert_eq!(closed["closure"]["closed_by"], "Alice");
    assert_eq!(closed["closure"]["outcome"], "resolved");
    assert!(closed["closure"]["closed_at"].is_string());
    assert_eq!(runner.inbox().await.unwrap().total(), 0);
    let rejected = http
        .post(&feedback_url)
        .json(&json!({"expected_job_id":revised_id,"comment":"stale"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 400);
    let stored =
        serde_json::to_value(runner.store().get_issue(issue.issue_id).await.unwrap()).unwrap();
    assert!(
        stored.get("closure").is_none(),
        "closure remains an event projection for older records"
    );
    server.abort();
    drop(runner);
    let restarted = SliceRunner::wire(
        topo,
        dir.path(),
        TeamBackend::ReadOnly,
        PlatformConfig::default(),
    )
    .unwrap();
    restarted.recover().await.unwrap();
    let record = restarted.issue_records().await.unwrap().pop().unwrap();
    assert_eq!(
        record.closure.unwrap().comment.as_deref(),
        Some("Recovered; all three submissions passed.")
    );
    assert!(restarted.inbox().await.unwrap().waiting_issues.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_action_decisions_are_not_duplicated_as_issue_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = Arc::new(ScriptedModelClient::new(vec![vec![
        call(
            "proposal",
            "propose_action",
            json!({"runbook_id":"mq.purge","target_ids":["redis-mq"],"reason":"test","expected_effect":"queue empty"}),
        ),
        diagnosis("finish"),
    ]]));
    let runner = SliceRunner::wire(
        topology(listener.local_addr().unwrap().port()),
        dir.path(),
        TeamBackend::Harness {
            client,
            budget: Default::default(),
            model: "scripted".into(),
            label: "scripted".into(),
        },
        PlatformConfig {
            runbooks: vec![RunbookCommand {
                id: "mq.purge".into(),
                command: "echo purge {target}".into(),
            }],
            ..Default::default()
        },
    )
    .unwrap();
    let (issue, job) = runner
        .handle_report(HumanReport::new("operator", "Check queue", "Need approval"))
        .await
        .unwrap();
    runner.drive_passes(job.clone()).await.unwrap();
    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.permission_requests.len(), 1);
    assert!(inbox.waiting_issues.is_empty());
    assert!(
        runner
            .feedback_issue(
                issue.issue_id,
                job.job_id,
                "Alice",
                "Do not bypass approval".into()
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_waiting_issues_cannot_receive_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let runner = SliceRunner::wire(
        topology(listener.local_addr().unwrap().port()),
        dir.path(),
        TeamBackend::ReadOnly,
        PlatformConfig::default(),
    )
    .unwrap();
    let (issue, job) = runner
        .handle_report(HumanReport::new(
            "operator",
            "Archive fixture",
            "Read only history",
        ))
        .await
        .unwrap();
    runner.drive_passes(job.clone()).await.unwrap();
    let previous = runner.store().get_issue(issue.issue_id).await.unwrap();
    let mut archived = previous.clone();
    archived.provenance = Some(broccoli_devops_agent::domain::SessionProvenance {
        source_deployment: "old deployment".into(),
        source_agent_version: "0.1.0".into(),
        exported_at: chrono::Utc::now(),
        exported_by: "Alice".into(),
        imported_at: chrono::Utc::now(),
        imported_by: "Alice".into(),
    });
    runner
        .store()
        .update_issue_if(&previous, archived)
        .await
        .unwrap();
    assert!(runner.inbox().await.unwrap().waiting_issues.is_empty());
    assert!(
        runner
            .feedback_issue(issue.issue_id, job.job_id, "Alice", "Must not run".into())
            .await
            .is_err()
    );
    assert_eq!(runner.store().list_jobs().await.unwrap().len(), 1);
}
