//! Execution control and reliability: joint scope authorization, idempotency claims and
//! compare-and-set transitions, process-group kill on timeout, restart recovery, derived Issue
//! status with explicit closure, class-specific verification evidence, business probes, and
//! failure evidence — the findings of the second design review, each pinned by a test.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_devops_agent::AgentError;
use broccoli_devops_agent::collector::TopologyCollector;
use broccoli_devops_agent::domain::{
    ActionStatus, DenialSource, HealthState, HumanReport, IssueStatus, JobStatus, OperationMode,
    ResourceKind, SnapshotCause, VerificationEvidence,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::{CaptureRequest, CollectorPort, StateStore};
use broccoli_devops_agent::runner::{InboxDecision, SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::{IssueClosure, SchedulerMode, TopScheduler};
use broccoli_devops_agent::store::file::FileStateStore;
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

fn resource(id: &str, kind: ResourceKind, port: u16) -> TopologyResource {
    TopologyResource {
        id: id.into(),
        kind,
        node: None,
        probes: vec![ProbeSpec {
            target: Some(format!("127.0.0.1:{port}")),
            ..ProbeSpec::new("tcp.connect")
        }],
    }
}

/// A worker, the API server, and Redis, each on the given port.
fn topology(
    mode: OperationMode,
    worker_port: u16,
    server_port: u16,
    redis_port: u16,
) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "reliability-test".into(),
            topology_revision: "t1".into(),
            operation_mode: mode,
        },
        resources: vec![
            resource("worker-1", ResourceKind::Worker, worker_port),
            resource("broccoli-server", ResourceKind::BroccoliServer, server_port),
            resource("redis-mq", ResourceKind::Redis, redis_port),
        ],
        dependencies: Vec::new(),
    }
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
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

fn echo_platform() -> PlatformConfig {
    PlatformConfig {
        dry_run: true,
        runbooks: vec![
            RunbookCommand {
                id: "worker.restart".into(),
                command: "echo restart {target}".into(),
            },
            RunbookCommand {
                id: "server.restart".into(),
                command: "echo restart {target}".into(),
            },
            RunbookCommand {
                id: "service.status".into(),
                command: "echo status {target}".into(),
            },
        ],
        ..PlatformConfig::default()
    }
}

fn runner(
    dir: &std::path::Path,
    topology: DeploymentTopology,
    platform: PlatformConfig,
    turns: Vec<Vec<broccoli_agent_harness::AssistantItem>>,
) -> SliceRunner {
    SliceRunner::wire(
        topology,
        dir,
        TeamBackend::Harness {
            client: Arc::new(ScriptedModelClient::new(turns)),
            budget: Default::default(),
            label: "scripted".into(),
        },
        platform,
    )
    .unwrap()
}

/// Finding 1: the worker row cannot be borrowed for the API server during a contest.
#[tokio::test]
async fn worker_restart_cannot_target_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let (server, server_port) = listener();
    let runner = runner(
        dir.path(),
        topology(
            OperationMode::ContestLocked,
            worker_port,
            server_port,
            closed_port(),
        ),
        echo_platform(),
        vec![
            vec![
                // Legitimate: auto in a contest.
                propose("c1", "worker.restart", "worker-1"),
                // Borrowing the worker row for the server: denied by scope, not approved.
                propose("c2", "worker.restart", "broccoli-server"),
                // The honest proposal for the server: approval required in a contest.
                propose("c3", "server.restart", "broccoli-server"),
            ],
            vec![diagnosis("c4")],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Slow judging", "queue growing"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::Succeeded);
    assert_eq!(actions[1].status, ActionStatus::Cancelled);
    let denial = actions[1].denial.as_ref().unwrap();
    assert_eq!(denial.source, DenialSource::Policy);
    assert!(
        denial.reason.contains("does not apply"),
        "{}",
        denial.reason
    );
    assert!(denial.reason.contains("BroccoliServer"));
    assert_eq!(actions[2].status, ActionStatus::WaitingForApproval);
    drop((worker, server));
}

/// Finding 2: a retry after a failure is admitted but escalated by the repeat rule, and two
/// concurrent approvals of one action cannot both apply — the action executes once.
#[tokio::test]
async fn retries_escalate_and_concurrent_approvals_apply_once() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let platform = PlatformConfig {
        runbooks: vec![RunbookCommand {
            id: "mq.purge".into(),
            command: "echo purge {target}".into(),
        }],
        ..PlatformConfig::default()
    };
    let runner = runner(
        dir.path(),
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        platform,
        vec![
            vec![
                // No command is configured for worker.restart: the first fails at the Platform,
                // which releases its idempotency key; the identical second is admitted but
                // escalated to approval by the repeat rule, so a failing restart cannot loop.
                propose("c1", "worker.restart", "worker-1"),
                propose("c2", "worker.restart", "worker-1"),
                // Needs approval in rehearsal.
                propose("c3", "mq.purge", "redis-mq"),
            ],
            vec![diagnosis("c4")],
            // Second report: the same worker restart under a different Issue is its own key.
            vec![propose("c5", "worker.restart", "worker-1")],
            vec![diagnosis("c6")],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Worker down", "no heartbeat"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::Failed);
    assert_eq!(actions[1].status, ActionStatus::WaitingForApproval);
    assert!(
        actions[1].denial.is_none(),
        "a retry after failure is not a duplicate"
    );
    assert_eq!(actions[2].status, ActionStatus::WaitingForApproval);
    let waiting = actions[2].action_run_id;

    // Two operators approve at once: exactly one wins, the other gets a conflict or an invalid
    // transition, and the action executes once.
    let (a, b) = tokio::join!(
        runner.approve_action(waiting, "alice"),
        runner.approve_action(waiting, "bob")
    );
    let outcomes = [a, b];
    assert_eq!(
        outcomes.iter().filter(|r| r.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    let loser = outcomes.iter().find(|r| r.is_err()).unwrap();
    assert!(matches!(
        loser,
        Err(AgentError::Conflict { .. }) | Err(AgentError::InvalidTransition { .. })
    ));
    let executed = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "action.started" && e.action_run_id == Some(waiting))
        .count();
    assert_eq!(executed, 1);

    let (_issue2, job2) = runner
        .handle_report(HumanReport::new("op", "Worker still down", "again"))
        .await
        .unwrap();
    let again = runner.run_proposals(&job2).await.unwrap();
    assert!(again[0].denial.is_none());
    drop(worker);
}

/// Finding 2, the other half: a duplicate proposed while the original still holds its claim is
/// denied as a duplicate before anyone is asked to approve it.
#[tokio::test]
async fn duplicate_while_claim_is_held_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let runner = runner(
        dir.path(),
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        PlatformConfig {
            runbooks: vec![RunbookCommand {
                id: "mq.purge".into(),
                command: "echo purge {target}".into(),
            }],
            ..PlatformConfig::default()
        },
        vec![
            vec![
                // mq.purge needs approval in rehearsal: the first waits and holds its claim,
                // so the identical second proposal is a duplicate.
                propose("c1", "mq.purge", "redis-mq"),
                propose("c2", "mq.purge", "redis-mq"),
            ],
            vec![diagnosis("c3")],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::WaitingForApproval);
    assert_eq!(actions[1].status, ActionStatus::Cancelled);
    let denial = actions[1].denial.as_ref().unwrap();
    assert!(denial.reason.contains("duplicate"), "{}", denial.reason);
    assert!(
        denial
            .reason
            .contains(&actions[0].action_run_id.to_string())
    );
    drop(worker);
}

/// Finding 3: a command that outlives its timeout is killed with its process group and reaped
/// before the result is reported, so it cannot keep modifying the machine afterwards.
#[tokio::test]
async fn timed_out_commands_are_killed_and_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("wrote-after-timeout");
    let (worker, worker_port) = listener();
    let platform = PlatformConfig {
        dry_run: false,
        command_timeout_secs: 1,
        runbooks: vec![RunbookCommand {
            id: "worker.restart".into(),
            // A helper subprocess (the subshell) that would write the marker after the parent's
            // timeout. With a group kill, neither survives.
            command: format!("(sleep 2; touch {}) & sleep 3", marker.display()),
        }],
        ..PlatformConfig::default()
    };
    let runner = runner(
        dir.path(),
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        platform,
        vec![
            vec![propose("c1", "worker.restart", "worker-1")],
            vec![diagnosis("c2")],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Worker hung", "restart it"))
        .await
        .unwrap();
    let started = Instant::now();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the timeout must not wait for the command: {:?}",
        started.elapsed()
    );
    assert_eq!(actions[0].status, ActionStatus::Failed);
    let summary = actions[0].execution_summary.as_deref().unwrap();
    assert!(summary.contains("timed out"), "{summary}");
    assert!(summary.contains("killed"), "{summary}");

    // Nothing from the process group survived to write the marker.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !marker.exists(),
        "a child of the timed-out command kept running"
    );
    drop(worker);
}

/// Finding 4: a restart mid-work is reconciled, the persisted freeze mode is restored, and a
/// review that was never written for a revising Job is reconstructed.
#[tokio::test]
async fn recovery_reconciles_interrupted_work_and_restores_the_freeze() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let topo = topology(
        OperationMode::Rehearsal,
        worker_port,
        closed_port(),
        closed_port(),
    );
    let (issue_id, running_job_id, running_action_id, waiting_action_id) = {
        let mut platform = echo_platform();
        platform.runbooks.push(RunbookCommand {
            id: "mq.purge".into(),
            command: "echo purge {target}".into(),
        });
        let runner = runner(
            dir.path(),
            topo.clone(),
            platform,
            vec![
                vec![
                    propose("c1", "worker.restart", "worker-1"),
                    propose("c2", "mq.purge", "redis-mq"),
                ],
                vec![diagnosis("c3")],
            ],
        );
        let (issue, job) = runner
            .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
            .await
            .unwrap();
        let actions = runner.run_proposals(&job).await.unwrap();
        assert_eq!(actions[0].status, ActionStatus::Succeeded);
        assert_eq!(actions[1].status, ActionStatus::WaitingForApproval);

        // Simulate the crash: rewrite persisted records as if the process died mid-flight —
        // the Job running again, the first action mid-execution — and freeze everything first
        // so recovery has a mode to restore.
        runner.scheduler().freeze_all().await.unwrap();
        let store = runner.store();
        let mut job_running = store.get_job(job.job_id).await.unwrap();
        job_running.status = JobStatus::Running;
        job_running.completed_at = None;
        job_running.result = None;
        store.update_job(job_running).await.unwrap();
        let mut mid_flight = store
            .get_action_run(actions[0].action_run_id)
            .await
            .unwrap();
        mid_flight.status = ActionStatus::Running;
        mid_flight.completed_at = None;
        mid_flight.verification_summary = None;
        store.update_action_run(mid_flight).await.unwrap();
        (
            issue.issue_id,
            job.job_id,
            actions[0].action_run_id,
            actions[1].action_run_id,
        )
    };

    // "Restart": a fresh runner over the same directory.
    let restarted =
        SliceRunner::wire(topo, dir.path(), TeamBackend::ReadOnly, echo_platform()).unwrap();
    let summary = restarted.recover().await.unwrap();
    assert_eq!(summary.previous_mode, SchedulerMode::FullyFrozen);
    assert_eq!(summary.final_mode, SchedulerMode::FullyFrozen);
    assert_eq!(
        restarted.scheduler().mode().await,
        SchedulerMode::FullyFrozen
    );
    assert_eq!(summary.interrupted_job_ids, vec![running_job_id]);
    assert_eq!(summary.interrupted_action_ids, vec![running_action_id]);
    assert!(summary.touched_anything());
    assert!(!summary.is_clean_restart());

    let store = restarted.store();
    let job = store.get_job(running_job_id).await.unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.result.unwrap().summary.contains("controller restart"));
    let action = store.get_action_run(running_action_id).await.unwrap();
    assert_eq!(action.status, ActionStatus::Failed);
    assert!(
        action
            .execution_summary
            .as_deref()
            .unwrap()
            .contains("may or may not have completed")
    );
    // The permission request survived the restart untouched; the Issue waits for a human.
    assert_eq!(
        store
            .get_action_run(waiting_action_id)
            .await
            .unwrap()
            .status,
        ActionStatus::WaitingForApproval
    );
    assert_eq!(
        store.get_issue(issue_id).await.unwrap().status,
        IssueStatus::WaitingForHuman
    );
    let inbox = restarted.inbox().await.unwrap();
    assert_eq!(inbox.failed_jobs.len(), 1);
    assert_eq!(inbox.failed_actions.len(), 1);
    assert_eq!(inbox.permission_requests.len(), 1);

    // Mode history is in the log: recovery_started, then fully_frozen again, then recovered.
    let kinds: Vec<_> = store
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    let started = kinds
        .iter()
        .rposition(|k| k == "scheduler.recovery_started")
        .unwrap();
    let frozen = kinds
        .iter()
        .rposition(|k| k == "scheduler.fully_frozen")
        .unwrap();
    let recovered = kinds
        .iter()
        .rposition(|k| k == "scheduler.recovered")
        .unwrap();
    assert!(started < frozen && frozen < recovered);
    assert!(kinds.iter().any(|k| k == "scheduler.action_interrupted"));

    // A clean restart after resuming touches nothing and reports the running mode.
    restarted.scheduler().resume().await.unwrap();
    let again = SliceRunner::wire(
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        dir.path(),
        TeamBackend::ReadOnly,
        echo_platform(),
    )
    .unwrap();
    let clean = again.recover().await.unwrap();
    assert_eq!(clean.previous_mode, SchedulerMode::Running);
    assert!(!clean.touched_anything());
    assert!(clean.is_clean_restart());
    assert_eq!(clean.final_mode, SchedulerMode::DispatchFrozen);

    // Recovery's own freeze is bookkeeping, not a human decision: a third restart with nothing
    // resumed in between is still a clean restart.
    let third = SliceRunner::wire(
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        dir.path(),
        TeamBackend::ReadOnly,
        echo_platform(),
    )
    .unwrap();
    let still_clean = third.recover().await.unwrap();
    assert_eq!(still_clean.previous_mode, SchedulerMode::Running);
    assert!(still_clean.is_clean_restart());
    drop(worker);
}

/// Finding 4, review reconstruction: a revising Job without a written review gets one.
#[tokio::test]
async fn recovery_reconstructs_a_missing_review() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let topo = topology(
        OperationMode::Rehearsal,
        worker_port,
        closed_port(),
        closed_port(),
    );
    let runner = runner(
        dir.path(),
        topo.clone(),
        PlatformConfig {
            runbooks: vec![RunbookCommand {
                id: "mq.purge".into(),
                command: "echo purge {target}".into(),
            }],
            ..PlatformConfig::default()
        },
        vec![
            vec![propose("c1", "mq.purge", "redis-mq")],
            vec![diagnosis("c2")],
            vec![diagnosis("c3")],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    runner
        .reject_action(
            actions[0].action_run_id,
            "alice",
            Some("no purges today".into()),
        )
        .await
        .unwrap();
    let outcome = runner
        .review_action(
            actions[0].action_run_id,
            "alice",
            InboxDecision::SendUpstream,
            Some("look at the worker".into()),
        )
        .await
        .unwrap();
    let revision_id = outcome.revision.unwrap().job.job_id;

    // Crash between dispatching the revision and writing the review: erase the review.
    let store = runner.store();
    let mut unreviewed = store
        .get_action_run(actions[0].action_run_id)
        .await
        .unwrap();
    unreviewed.review = None;
    store.update_action_run(unreviewed).await.unwrap();
    assert_eq!(runner.inbox().await.unwrap().permission_denied.len(), 1);

    let restarted =
        SliceRunner::wire(topo, dir.path(), TeamBackend::ReadOnly, echo_platform()).unwrap();
    let summary = restarted.recover().await.unwrap();
    assert_eq!(summary.reconstructed_review_job_ids, vec![revision_id]);
    let reviewed = restarted
        .store()
        .get_action_run(actions[0].action_run_id)
        .await
        .unwrap();
    let review = reviewed.review.unwrap();
    assert_eq!(review.reviewer, "alice");
    assert_eq!(review.comment.as_deref(), Some("look at the worker"));
    assert!(
        restarted
            .inbox()
            .await
            .unwrap()
            .permission_denied
            .is_empty()
    );
    drop(worker);
}

/// Finding 5: Issue status follows its outstanding work, and a human can close it explicitly.
#[tokio::test]
async fn issue_status_is_derived_and_closable() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let runner = runner(
        dir.path(),
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        PlatformConfig {
            dry_run: false,
            ..echo_platform()
        },
        vec![
            // Report 1: diagnosis only.
            vec![diagnosis("c1")],
            // Report 2: a restart that succeeds with weak evidence (the worker was healthy).
            vec![propose("c2", "worker.restart", "worker-1")],
            vec![diagnosis("c3")],
            // Report 3: a restart of the closed-port server: verification fails.
            vec![propose("c4", "server.restart", "broccoli-server")],
            vec![diagnosis("c5")],
        ],
    );
    let (diagnosed, job1) = runner
        .handle_report(HumanReport::new(
            "op",
            "Just looking",
            "is everything fine?",
        ))
        .await
        .unwrap();
    assert_eq!(job1.status, JobStatus::Completed);
    assert_eq!(diagnosed.status, IssueStatus::WaitingForHuman);
    let closed = runner
        .close_issue(
            diagnosed.issue_id,
            IssueClosure::Resolved,
            "alice",
            Some("nothing to do".into()),
        )
        .await
        .unwrap();
    assert_eq!(closed.status, IssueStatus::Resolved);
    assert!(
        runner
            .close_issue(diagnosed.issue_id, IssueClosure::Cancelled, "alice", None)
            .await
            .is_err(),
        "terminal Issues stay closed"
    );

    let (fixed, job2) = runner
        .handle_report(HumanReport::new("op", "Worker slow", "restart it"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job2).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::Succeeded);
    assert_eq!(
        actions[0].verification_evidence,
        Some(VerificationEvidence::Weak),
        "the worker was already Healthy before the restart"
    );
    assert_eq!(
        runner
            .store()
            .get_issue(fixed.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::Resolved
    );

    let (broken, job3) = runner
        .handle_report(HumanReport::new("op", "Server down", "restart it"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job3).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::VerificationFailed);
    assert_eq!(actions[0].verification_evidence, None);
    assert_eq!(
        runner
            .store()
            .get_issue(broken.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::WaitingForHuman
    );
    // Acknowledging the failure does not resolve the Issue; cancelling it does close it.
    runner
        .review_action(
            actions[0].action_run_id,
            "bob",
            InboxDecision::Acknowledge,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        runner
            .store()
            .get_issue(broken.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::WaitingForHuman
    );
    let cancelled = runner
        .close_issue(broken.issue_id, IssueClosure::Cancelled, "bob", None)
        .await
        .unwrap();
    assert_eq!(cancelled.status, IssueStatus::Cancelled);
    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.iter().any(|k| k == "scheduler.issue_reconciled"));
    assert!(kinds.iter().any(|k| k == "human.issue_closed"));
    drop(worker);
}

/// Finding 6: a dry run is labelled as such and never resolves an Issue; a Platform that errors
/// out is a failed execution, not a stuck action (finding 7).
#[tokio::test]
async fn dry_runs_are_not_remediation_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let runner = runner(
        dir.path(),
        topology(
            OperationMode::Rehearsal,
            worker_port,
            closed_port(),
            closed_port(),
        ),
        echo_platform(),
        vec![
            vec![propose("c1", "worker.restart", "worker-1")],
            vec![diagnosis("c2")],
        ],
    );
    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Worker slow", "restart it"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::Succeeded);
    assert!(actions[0].dry_run);
    assert_eq!(
        actions[0].verification_evidence,
        Some(VerificationEvidence::DryRun)
    );
    assert!(
        actions[0]
            .verification_summary
            .as_deref()
            .unwrap()
            .contains("not evidence of remediation")
    );
    assert_eq!(
        runner
            .store()
            .get_issue(issue.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::WaitingForHuman,
        "a dry run cannot resolve an Issue"
    );
    drop(worker);
}

/// Finding 6, probes: queue depth and JSON counters become metrics with health thresholds, and
/// slow reachability becomes Degraded.
#[tokio::test]
async fn business_probes_measure_and_judge() {
    // A fake Redis answering LLEN with 7, and a fake API answering a JSON document.
    let redis = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redis_port = redis.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = redis.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0_u8; 256];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(b":7\r\n").await;
            });
        }
    });
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = api.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf).await;
                // Deliberately slow, to trip the latency threshold.
                tokio::time::sleep(Duration::from_millis(150)).await;
                let body = r#"{"workers":{"online":2},"mode":"contest"}"#;
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });

    let topology = DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "probes".into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![
            TopologyResource {
                id: "redis-mq".into(),
                kind: ResourceKind::Redis,
                node: None,
                probes: vec![ProbeSpec {
                    target: Some(format!("127.0.0.1:{redis_port}")),
                    key: Some("judge:queue".into()),
                    metric: Some("queue.depth".into()),
                    max: Some(5.0),
                    ..ProbeSpec::new("redis.llen")
                }],
            },
            TopologyResource {
                id: "broccoli-server".into(),
                kind: ResourceKind::BroccoliServer,
                node: None,
                probes: vec![
                    ProbeSpec {
                        url: Some(format!("http://127.0.0.1:{api_port}/api/status")),
                        pointer: Some("/workers/online".into()),
                        metric: Some("workers.online".into()),
                        min: Some(1.0),
                        ..ProbeSpec::new("http.json")
                    },
                    ProbeSpec {
                        url: Some(format!("http://127.0.0.1:{api_port}/api/status")),
                        pointer: Some("/mode".into()),
                        expect: Some("contest".into()),
                        ..ProbeSpec::new("http.json")
                    },
                    ProbeSpec {
                        url: Some(format!("http://127.0.0.1:{api_port}/healthz")),
                        degraded_above_ms: Some(50.0),
                        ..ProbeSpec::new("http.status")
                    },
                ],
            },
        ],
        dependencies: Vec::new(),
    };
    let store = Arc::new(broccoli_devops_agent::store::memory::InMemoryStateStore::new());
    let collector = TopologyCollector::new(topology.clone(), store);
    let snapshot = collector
        .capture_snapshot(CaptureRequest {
            deployment_id: topology.deployment.id,
            topology_revision: "t1".into(),
            cause: SnapshotCause::Manual,
            operation_mode: OperationMode::Rehearsal,
            parent_snapshot_id: None,
            requested_probe_ids: Vec::new(),
        })
        .await
        .unwrap();

    let redis_state = snapshot
        .resources
        .iter()
        .find(|r| r.resource_id == "redis-mq")
        .unwrap();
    assert_eq!(
        redis_state.health,
        HealthState::Degraded,
        "7 entries > max 5"
    );
    let depth = redis_state
        .metrics
        .iter()
        .find(|m| m.name == "queue.depth")
        .unwrap();
    assert_eq!(depth.value, 7.0);

    let server_state = snapshot
        .resources
        .iter()
        .find(|r| r.resource_id == "broccoli-server")
        .unwrap();
    assert_eq!(
        server_state.health,
        HealthState::Degraded,
        "latency over threshold"
    );
    let online = server_state
        .metrics
        .iter()
        .find(|m| m.name == "workers.online")
        .unwrap();
    assert_eq!(online.value, 2.0);
    assert!(
        server_state
            .facts
            .iter()
            .any(|f| f.name == "probe.http.json" && f.value.contains("/mode = contest"))
    );
    assert!(
        server_state
            .facts
            .iter()
            .any(|f| f.name == "probe.http.status" && f.value.contains("latency"))
    );
}

/// Finding 7: an after-Snapshot that cannot be captured fails verification on the record
/// instead of leaving the action `Verifying`.
#[tokio::test]
async fn failed_after_capture_is_recorded_not_propagated() {
    let dir = tempfile::tempdir().unwrap();
    let (worker, worker_port) = listener();
    let topo = topology(
        OperationMode::Rehearsal,
        worker_port,
        closed_port(),
        closed_port(),
    );
    let runner = runner(
        dir.path(),
        topo.clone(),
        echo_platform(),
        vec![
            vec![propose("c1", "worker.restart", "worker-1")],
            vec![diagnosis("c2")],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Worker slow", "restart it"))
        .await
        .unwrap();
    // Create the action but verify it through a capture for a deployment the Collector does
    // not serve, which the Collector refuses.
    let action = runner
        .scheduler()
        .create_action_run(
            job.job_id,
            job.result.as_ref().unwrap().proposed_actions[0].clone(),
            CaptureRequest {
                deployment_id: topo.deployment.id,
                topology_revision: "t1".into(),
                cause: SnapshotCause::BeforeAction,
                operation_mode: OperationMode::Rehearsal,
                parent_snapshot_id: None,
                requested_probe_ids: Vec::new(),
            },
            "capture-failure-test",
        )
        .await
        .unwrap();
    let executed = runner
        .scheduler()
        .execute_action(action.action_run_id)
        .await
        .unwrap();
    assert_eq!(executed.status, ActionStatus::Verifying);
    let verified = runner
        .scheduler()
        .verify_action(
            action.action_run_id,
            CaptureRequest {
                deployment_id: Uuid::now_v7(),
                topology_revision: "t1".into(),
                cause: SnapshotCause::AfterAction,
                operation_mode: OperationMode::Rehearsal,
                parent_snapshot_id: None,
                requested_probe_ids: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(verified.status, ActionStatus::VerificationFailed);
    assert_eq!(verified.after_snapshot_id, None);
    assert!(
        verified
            .verification_summary
            .as_deref()
            .unwrap()
            .contains("after-Snapshot capture failed")
    );
    assert_eq!(runner.inbox().await.unwrap().failed_actions.len(), 1);
    drop(worker);
}

/// The file store survives a crash mid-write: a half-written temp file is ignored on reopen.
#[tokio::test]
async fn file_store_writes_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStateStore::open(dir.path()).unwrap();
    let issue = broccoli_devops_agent::domain::Issue::from_human_report(
        HumanReport::new("op", "t", "d"),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    store.insert_issue(issue.clone()).await.unwrap();
    // A crash left a truncated temp file next to the document.
    std::fs::write(
        dir.path()
            .join("issues")
            .join(format!("{}.json.tmp", issue.issue_id)),
        "{\"issue_id\": \"trunc",
    )
    .unwrap();
    let reopened = FileStateStore::open(dir.path()).unwrap();
    assert_eq!(reopened.get_issue(issue.issue_id).await.unwrap(), issue);

    // The compare-and-set refuses a stale write.
    let mut stale = issue.clone();
    stale.title = "stale".into();
    let mut fresh = issue.clone();
    fresh.title = "fresh".into();
    reopened
        .update_issue_if(&issue, fresh.clone())
        .await
        .unwrap();
    assert!(matches!(
        reopened.update_issue_if(&issue, stale).await,
        Err(AgentError::Conflict { .. })
    ));
    let _ = TopScheduler::new(Arc::new(reopened));
}

/// Finding 6, Broccoli probes: a worker is observed through the admin API's heartbeat list and
/// a queue through the overview, logging in with credentials that never touch the topology.
#[tokio::test]
async fn broccoli_probes_read_heartbeats_and_queues() {
    use broccoli_devops_agent::collector::DEFAULT_LOGIN_ENV;

    // A fake Broccoli server: login issues a token; admin routes require it.
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = api.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = api.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 8192];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let line = request.lines().next().unwrap_or("").to_string();
                let authorized = request.contains("authorization: Bearer jwt-1")
                    || request.contains("Authorization: Bearer jwt-1");
                let (status, body) = if line.starts_with("POST /api/v1/auth/login") {
                    if request.contains("\"username\":\"probe\"")
                        && request.contains("\"password\":\"s3cret\"")
                    {
                        ("200 OK", r#"{"token":"jwt-1","id":1,"username":"probe","roles":["admin"],"permissions":["system:view"]}"#.to_string())
                    } else {
                        (
                            "401 Unauthorized",
                            r#"{"code":"INVALID_CREDENTIALS"}"#.to_string(),
                        )
                    }
                } else if !authorized {
                    (
                        "401 Unauthorized",
                        r#"{"code":"TOKEN_MISSING"}"#.to_string(),
                    )
                } else if line.starts_with("GET /api/v1/admin/system/workers") {
                    ("200 OK", r#"{"workers":[
                        {"id":"worker-1","started_at":"2026-09-05T00:00:00Z","last_seen":"2026-09-05T00:00:03Z","seconds_since_last_seen":3,"stale":false,"in_flight":2,"max_concurrency":4,"sandbox_backend":"isolate","version":"0.3.0","hostname":"judge-1"},
                        {"id":"worker-2","started_at":"2026-09-05T00:00:00Z","last_seen":"2026-09-05T00:00:00Z","seconds_since_last_seen":12,"stale":true,"in_flight":0,"max_concurrency":4,"sandbox_backend":"isolate","version":"0.3.0","hostname":"judge-2"}
                    ]}"#.to_string())
                } else if line.starts_with("GET /api/v1/admin/system/overview") {
                    ("200 OK", r#"{"workers":[],"queues":[{"name":"operation_tasks","depth":7,"breakdown":{"queued":5,"processing":2}}],"submissions_in_progress":2,"dlq_unresolved_count":1}"#.to_string())
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });

    let base = format!("http://127.0.0.1:{api_port}");
    let topology = DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "broccoli-probes".into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![
            TopologyResource {
                id: "worker-1".into(),
                kind: ResourceKind::Worker,
                node: None,
                probes: vec![ProbeSpec {
                    url: Some(base.clone()),
                    ..ProbeSpec::new("broccoli.worker")
                }],
            },
            TopologyResource {
                id: "worker-2".into(),
                kind: ResourceKind::Worker,
                node: None,
                probes: vec![ProbeSpec {
                    url: Some(base.clone()),
                    ..ProbeSpec::new("broccoli.worker")
                }],
            },
            TopologyResource {
                id: "worker-3".into(),
                kind: ResourceKind::Worker,
                node: None,
                probes: vec![ProbeSpec {
                    url: Some(base.clone()),
                    ..ProbeSpec::new("broccoli.worker")
                }],
            },
            TopologyResource {
                id: "redis-mq".into(),
                kind: ResourceKind::Redis,
                node: None,
                probes: vec![ProbeSpec {
                    url: Some(base.clone()),
                    queue: Some("operation_tasks".into()),
                    metric: Some("queue.depth".into()),
                    max: Some(5.0),
                    ..ProbeSpec::new("broccoli.queue")
                }],
            },
        ],
        dependencies: Vec::new(),
    };
    let capture = CaptureRequest {
        deployment_id: topology.deployment.id,
        topology_revision: "t1".into(),
        cause: SnapshotCause::Manual,
        operation_mode: OperationMode::Rehearsal,
        parent_snapshot_id: None,
        requested_probe_ids: Vec::new(),
    };

    // Without the login, every Broccoli probe fails with the variable's name, never a panic.
    let store = Arc::new(broccoli_devops_agent::store::memory::InMemoryStateStore::new());
    let blind = TopologyCollector::new(topology.clone(), store.clone());
    let snapshot = blind.capture_snapshot(capture.clone()).await.unwrap();
    let worker = snapshot
        .resources
        .iter()
        .find(|r| r.resource_id == "worker-1")
        .unwrap();
    assert_eq!(worker.health, HealthState::Down);
    assert!(
        worker
            .facts
            .iter()
            .any(|f| f.name == "probe.broccoli.worker" && f.value.contains(DEFAULT_LOGIN_ENV))
    );

    let collector = TopologyCollector::new(topology.clone(), store)
        .with_secret(DEFAULT_LOGIN_ENV, "probe:s3cret");
    let snapshot = collector.capture_snapshot(capture).await.unwrap();
    let state = |id: &str| {
        snapshot
            .resources
            .iter()
            .find(|r| r.resource_id == id)
            .unwrap()
            .clone()
    };
    let live = state("worker-1");
    assert_eq!(live.health, HealthState::Healthy);
    assert_eq!(
        live.metrics
            .iter()
            .find(|m| m.name == "worker.in_flight")
            .unwrap()
            .value,
        2.0
    );
    assert!(
        live.facts
            .iter()
            .any(|f| f.name == "probe.broccoli.worker.hostname" && f.value == "judge-1")
    );
    assert_eq!(
        state("worker-2").health,
        HealthState::Degraded,
        "stale heartbeat"
    );
    assert_eq!(
        state("worker-3").health,
        HealthState::Down,
        "no heartbeat at all"
    );

    let queue = state("redis-mq");
    assert_eq!(queue.health, HealthState::Degraded, "depth 7 > max 5");
    assert_eq!(
        queue
            .metrics
            .iter()
            .find(|m| m.name == "queue.depth")
            .unwrap()
            .value,
        7.0
    );
    assert!(
        queue
            .metrics
            .iter()
            .any(|m| m.name == "broccoli.dlq_unresolved" && m.value == 1.0)
    );
}
