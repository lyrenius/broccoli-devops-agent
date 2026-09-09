//! End-to-end action flow: a model proposes operations, the authority matrix decides, the
//! Platform executes in dry-run, and verification judges the effect — across auto, approve,
//! deny, rate-limit escalation, and human approval.

use std::net::TcpListener;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_devops_agent::domain::{
    ActionStatus, ApprovalState, HumanReport, OperationMode, ResourceKind, VerificationEvidence,
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
            model: "scripted-model".into(),
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

    // Row 2, rehearsal: auto → executed as a dry run → verified as a dry run, which succeeds
    // but is labelled as no evidence of remediation.
    assert_eq!(actions[0].runbook_id, "worker.restart");
    assert_eq!(actions[0].approval, ApprovalState::NotRequired);
    assert_eq!(actions[0].status, ActionStatus::Succeeded);
    assert_eq!(
        actions[0].verification_evidence,
        Some(VerificationEvidence::DryRun)
    );
    assert!(
        actions[0]
            .verification_summary
            .as_deref()
            .unwrap()
            .contains("dry run")
    );

    // Row 8, rehearsal: approve → waiting for a human.
    assert_eq!(actions[1].runbook_id, "mq.purge");
    assert_eq!(actions[1].status, ActionStatus::WaitingForApproval);

    // Row 26: human-only → denied → cancelled, never executed.
    assert_eq!(actions[2].runbook_id, "mode.set");
    assert_eq!(actions[2].approval, ApprovalState::Rejected);
    assert_eq!(actions[2].status, ActionStatus::Cancelled);

    // Human approval runs the purge — as a dry run here, so it is recorded as such rather than
    // judged against Redis, which is down. (Live verification is covered in tests/reliability.rs.)
    let purged = runner
        .approve_action(actions[1].action_run_id, "op")
        .await
        .unwrap();
    assert_eq!(purged.approval, ApprovalState::Approved);
    assert_eq!(purged.approved_by.as_deref(), Some("op"));
    assert_eq!(purged.status, ActionStatus::Succeeded);
    assert_eq!(
        purged.verification_evidence,
        Some(VerificationEvidence::DryRun)
    );

    // Rule 5: the same automatic restart on the same target within the window escalates.
    let (_issue, job2) = runner
        .handle_report(HumanReport::new("op", "Still stuck", "again"))
        .await
        .unwrap();
    let again = runner.run_proposals(&job2).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].status, ActionStatus::WaitingForApproval);
    let rejected = runner
        .reject_action(again[0].action_run_id, "op", Some("once was enough".into()))
        .await
        .unwrap();
    assert_eq!(rejected.status, ActionStatus::Cancelled);
    assert_eq!(
        rejected.denial.as_ref().unwrap().comment.as_deref(),
        Some("once was enough")
    );

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
            model: "scripted-model".into(),
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

/// Missing implementations become human tasks before the Platform starts.
#[tokio::test]
async fn unconfigured_runbooks_wait_for_human_without_execution() {
    let dir = tempfile::tempdir().unwrap();
    let client = ScriptedModelClient::new(vec![
        vec![propose("c1", "worker.start", "worker-1")],
        vec![diagnosis("c2")],
    ]);
    let runner = SliceRunner::wire(
        topology(OperationMode::Rehearsal, closed_port(), closed_port()),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(client),
            budget: Default::default(),
            model: "scripted-model".into(),
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
    assert_eq!(actions[0].status, ActionStatus::WaitingForHuman);
    assert!(
        actions[0]
            .human_intervention
            .as_deref()
            .unwrap()
            .contains("no command is configured")
    );
    assert!(actions[0].started_at.is_none());
    assert!(actions[0].execution_artifact_id.is_none());
    assert!(actions[0].denial.is_none());
    assert!(
        runner
            .approve_action(actions[0].action_run_id, "op")
            .await
            .is_err()
    );
    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.blocked_actions.len(), 1);
    assert!(inbox.failed_actions.is_empty());
    assert!(inbox.permission_denied.is_empty());
    assert!(
        inbox.waiting_issues.is_empty(),
        "do not duplicate the same issue in input"
    );
    assert_eq!(
        runner
            .store()
            .get_issue(actions[0].issue_id)
            .await
            .unwrap()
            .status,
        broccoli_devops_agent::domain::IssueStatus::WaitingForHuman
    );
    assert!(
        !runner
            .store()
            .list_events()
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "action.started")
    );
}

/// Unknown runbooks are persisted for human implementation; out-of-scope targets stay denied.
#[tokio::test]
async fn unknown_runbooks_reach_humans_without_weakening_scope_checks() {
    let dir = tempfile::tempdir().unwrap();
    let client = ScriptedModelClient::new(vec![
        vec![propose("c1", "worker.custom_fix", "worker-1")],
        vec![propose("c2", "worker.custom_fix", "outside-scope")],
        vec![propose("c3", "worker.start", "redis-mq")],
        vec![diagnosis("c4")],
    ]);
    let runner = SliceRunner::wire(
        topology(OperationMode::Rehearsal, closed_port(), closed_port()),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(client),
            budget: Default::default(),
            model: "scripted-model".into(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();
    let (_, job) = runner
        .handle_report(HumanReport::new(
            "op",
            "Repair worker",
            "inspect the failure",
        ))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(
        actions.len(),
        3,
        "unknown proposals must survive the model tool boundary"
    );
    assert_eq!(actions[0].status, ActionStatus::WaitingForHuman);
    assert!(
        actions[0]
            .human_intervention
            .as_deref()
            .unwrap()
            .contains("not in the Runbook Registry")
    );
    for denied in &actions[1..] {
        assert_eq!(denied.status, ActionStatus::Cancelled);
        assert!(denied.denial.is_some());
        assert!(denied.started_at.is_none());
    }
    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.blocked_actions.len(), 1);
    assert_eq!(inbox.permission_denied.len(), 2);
    assert!(inbox.failed_actions.is_empty());
}

/// A pending approval cannot run after its implementation is removed from live settings.
#[tokio::test]
async fn approval_rechecks_command_availability_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let runner = SliceRunner::wire(
        topology(OperationMode::Rehearsal, closed_port(), closed_port()),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(ScriptedModelClient::new(vec![
                vec![propose("c1", "mq.purge", "redis-mq")],
                vec![diagnosis("c2")],
            ])),
            budget: Default::default(),
            model: "scripted-model".into(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();
    let (_, job) = runner
        .handle_report(HumanReport::new("op", "Queue", "inspect it"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::WaitingForApproval);
    let mut live = runner.settings().current();
    live.platform.runbooks.clear();
    runner.settings().replace(live);
    let blocked = runner
        .approve_action(actions[0].action_run_id, "op")
        .await
        .unwrap();
    assert_eq!(blocked.status, ActionStatus::WaitingForHuman);
    assert!(blocked.started_at.is_none());
    assert!(blocked.execution_artifact_id.is_none());
    assert_eq!(runner.inbox().await.unwrap().blocked_actions.len(), 1);
}

/// Waiting survives a restart, and revised work is evaluated afresh after configuration is fixed.
#[tokio::test]
async fn blocked_action_survives_restart_and_can_be_revised_after_configuration() {
    use broccoli_devops_agent::runner::{InboxDecision, PassStop};
    let dir = tempfile::tempdir().unwrap();
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let topo = topology(
        OperationMode::Rehearsal,
        worker.local_addr().unwrap().port(),
        closed_port(),
    );
    let runner = SliceRunner::wire(
        topo.clone(),
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(ScriptedModelClient::new(vec![
                vec![propose("c1", "worker.start", "worker-1")],
                vec![diagnosis("c2")],
            ])),
            budget: Default::default(),
            model: "scripted-model".into(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();
    let (_, job) = runner
        .handle_report(HumanReport::new("op", "Worker", "inspect it"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes[0].stop, PassStop::WaitingForHuman);
    let id = passes[0].actions[0].action_run_id;
    drop(runner);
    let restarted = SliceRunner::wire(
        topo,
        dir.path(),
        TeamBackend::Harness {
            client: std::sync::Arc::new(ScriptedModelClient::new(vec![
                vec![propose("c3", "worker.start", "worker-1")],
                vec![diagnosis("c4")],
            ])),
            budget: Default::default(),
            model: "scripted-model".into(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap();
    restarted.recover().await.unwrap();
    assert_eq!(
        restarted.inbox().await.unwrap().blocked_actions[0].action_run_id,
        id
    );
    let mut live = restarted.settings().current();
    live.platform.runbooks.push(RunbookCommand {
        id: "worker.start".into(),
        command: "echo start {target}".into(),
    });
    restarted.settings().replace(live);
    restarted.resume().await.unwrap();
    let reviewed = restarted
        .review_action(
            id,
            "op",
            InboxDecision::SendUpstream,
            Some("implementation configured; reassess".into()),
        )
        .await
        .unwrap();
    assert_eq!(reviewed.reviewed.status, ActionStatus::Cancelled);
    let revision = reviewed.revision.unwrap();
    assert_eq!(revision.actions[0].status, ActionStatus::Succeeded);
    assert_eq!(
        revision.actions[0].verification_evidence,
        Some(VerificationEvidence::DryRun)
    );
    assert!(restarted.inbox().await.unwrap().blocked_actions.is_empty());
}
