//! End-to-end action flow: a model proposes operations, the authority matrix decides, the
//! Platform executes in dry-run, and verification judges the effect — across auto, approve,
//! deny, rate-limit escalation, and human approval.

use std::net::TcpListener;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_devops_agent::domain::{
    ActionStatus, ApprovalState, HumanReport, OperationMode, ResourceKind,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::json;
use uuid::Uuid;

/// A topology with one reachable worker (a local listener) and one unreachable Redis.
fn topology(mode: OperationMode, worker_port: u16, redis_port: u16) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "actions-test".into(),
            topology_revision: "t1".into(),
            operation_mode: mode,
        },
        resources: vec![
            TopologyResource {
                id: "worker-1".into(),
                kind: ResourceKind::Worker,
                node: None,
                probes: vec![ProbeSpec {
                    probe: "tcp.connect".into(),
                    target: Some(format!("127.0.0.1:{worker_port}")),
                    url: None,
                }],
            },
            TopologyResource {
                id: "redis-mq".into(),
                kind: ResourceKind::Redis,
                node: None,
                probes: vec![ProbeSpec {
                    probe: "tcp.connect".into(),
                    target: Some(format!("127.0.0.1:{redis_port}")),
                    url: None,
                }],
            },
        ],
        dependencies: Vec::new(),
    }
}

/// Reserves a closed port by binding and immediately dropping a listener.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn platform_config() -> PlatformConfig {
    PlatformConfig {
        dry_run: true,
        runbooks: vec![
            RunbookCommand {
                id: "worker.restart".into(),
                command: "echo restart {target}".into(),
            },
            RunbookCommand {
                id: "mq.purge".into(),
                command: "echo purge {target}".into(),
            },
        ],
        ..PlatformConfig::default()
    }
}

fn propose(id: &str, runbook: &str, target: &str) -> broccoli_agent_harness::AssistantItem {
    call(
        id,
        "propose_action",
        json!({
            "runbook_id": runbook,
            "target_ids": [target],
            "reason": "evidence in the view",
            "expected_effect": "the target becomes healthy",
        }),
    )
}

fn diagnosis(id: &str) -> broccoli_agent_harness::AssistantItem {
    call(id, "submit_diagnosis", json!({ "summary": "diagnosed" }))
}

/// Auto rows execute and verify, approve rows wait, deny rows are cancelled — and a repeated
/// automatic action escalates to approval.
#[tokio::test]
async fn matrix_drives_auto_approve_deny_and_rate_limit() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let worker_port = worker.local_addr().unwrap().port();
    let redis_port = closed_port();
    let dir = tempfile::tempdir().unwrap();

    // Two reports: the second repeats the worker restart inside the repeat window.
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "read_snapshot_view", json!({}))],
        vec![
            propose("c2", "worker.restart", "worker-1"),
            propose("c3", "mq.purge", "redis-mq"),
            propose("c4", "mode.set", "worker-1"),
        ],
        vec![diagnosis("c5")],
        vec![propose("c6", "worker.restart", "worker-1")],
        vec![diagnosis("c7")],
    ]);
    let runner = SliceRunner::wire(
        topology(OperationMode::Rehearsal, worker_port, redis_port),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(client),
            budget: Default::default(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();

    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions.len(), 3);

    // Row 2, rehearsal: auto → executed (dry run) → verified: the worker listener is Healthy.
    assert_eq!(actions[0].runbook_id, "worker.restart");
    assert_eq!(actions[0].approval, ApprovalState::NotRequired);
    assert_eq!(actions[0].status, ActionStatus::Succeeded);
    assert!(
        actions[0]
            .verification_summary
            .as_deref()
            .unwrap()
            .contains("Healthy")
    );

    // Row 8, rehearsal: approve → waiting for a human.
    assert_eq!(actions[1].runbook_id, "mq.purge");
    assert_eq!(actions[1].status, ActionStatus::WaitingForApproval);

    // Row 26: human-only → denied → cancelled, never executed.
    assert_eq!(actions[2].runbook_id, "mode.set");
    assert_eq!(actions[2].approval, ApprovalState::Rejected);
    assert_eq!(actions[2].status, ActionStatus::Cancelled);

    // Human approval runs the purge; the after Snapshot still shows Redis down, so the command's
    // exit code zero is not accepted as success.
    let purged = runner
        .approve_action(actions[1].action_run_id)
        .await
        .unwrap();
    assert_eq!(purged.approval, ApprovalState::Approved);
    assert_eq!(purged.status, ActionStatus::VerificationFailed);

    // Rule 5: the same automatic restart on the same target within the window escalates.
    let (_issue, job2) = runner
        .handle_report(HumanReport::new("op", "Still stuck", "again"))
        .await
        .unwrap();
    let again = runner.run_proposals(&job2).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].status, ActionStatus::WaitingForApproval);
    let rejected = runner.reject_action(again[0].action_run_id).await.unwrap();
    assert_eq!(rejected.status, ActionStatus::Cancelled);

    let all = runner.list_actions().await.unwrap();
    assert_eq!(all.len(), 4);
    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds
            .iter()
            .any(|k| k == "scheduler.action_authority_evaluated")
    );
    assert!(kinds.iter().any(|k| k == "action.executed"));
    assert!(kinds.iter().any(|k| k == "verification.recorded"));
}

/// During a live contest the matrix denies a queue purge outright.
#[tokio::test]
async fn contest_mode_denies_destructive_rows() {
    let dir = tempfile::tempdir().unwrap();
    let client = ScriptedModelClient::new(vec![
        vec![propose("c1", "mq.purge", "redis-mq")],
        vec![diagnosis("c2")],
    ]);
    let runner = SliceRunner::wire(
        topology(OperationMode::ContestLocked, closed_port(), closed_port()),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(client),
            budget: Default::default(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "mid-contest"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::Cancelled);
    assert_eq!(actions[0].approval, ApprovalState::Rejected);
}

/// A runbook without a configured command is refused by the Platform, not silently skipped.
#[tokio::test]
async fn platform_refuses_unconfigured_runbooks() {
    let dir = tempfile::tempdir().unwrap();
    let client = ScriptedModelClient::new(vec![
        vec![propose("c1", "station.restart", "worker-1")],
        vec![diagnosis("c2")],
    ]);
    let runner = SliceRunner::wire(
        topology(OperationMode::Rehearsal, closed_port(), closed_port()),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(client),
            budget: Default::default(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Printer", "stuck"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    // station.restart is auto in rehearsal, so it reaches the Platform — which has no command.
    assert_eq!(actions[0].status, ActionStatus::Failed);
}
