//! The inbox and its feedback loop: denials keep their reason, failures wait for a human, and a
//! review can send the item back upstream as a revising Job whose Team actually sees the feedback.

use std::net::TcpListener;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call, text};
use broccoli_devops_agent::AgentResult;
use broccoli_devops_agent::domain::{
    ActionStatus, DenialSource, FeedbackOrigin, HumanFeedback, HumanReport, IssueStatus,
    JobOutcome, JobStatus, OperationMode, ResourceKind, ReviewDecision, SnapshotCause,
    TeamCallback,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::{AgentTeamPort, StateStore, TeamCallbackSink, cancel_pair};
use broccoli_devops_agent::runner::{InboxDecision, SliceRunner, TeamBackend};
use broccoli_devops_agent::team::ReadOnlyOperateTeam;
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use broccoli_devops_agent::view::{FileArtifactStore, PROFILE_OPERATE_READONLY};
use serde_json::json;
use uuid::Uuid;

/// One reachable worker and one unreachable Redis.
fn topology(worker_port: u16, redis_port: u16) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "inbox-test".into(),
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
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

fn harness_runner(
    dir: &std::path::Path,
    worker_port: u16,
    turns: Vec<Vec<broccoli_agent_harness::AssistantItem>>,
) -> SliceRunner {
    SliceRunner::wire(
        topology(worker_port, closed_port()),
        dir,
        TeamBackend::Harness {
            client: Arc::new(ScriptedModelClient::new(turns)),
            budget: Default::default(),
            label: "scripted".into(),
        },
        platform_config(),
    )
    .unwrap()
}

/// A rule denial and a human rejection both land in the Permission Denied inbox with their
/// reason and comment, and the Issue is parked with a human.
#[tokio::test]
async fn denials_keep_their_reason_and_wait_in_the_inbox() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runner = harness_runner(
        dir.path(),
        worker.local_addr().unwrap().port(),
        vec![
            vec![
                propose("c1", "mq.purge", "redis-mq"),
                propose("c2", "mode.set", "worker-1"),
            ],
            vec![diagnosis("c3")],
        ],
    );
    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();

    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.permission_requests.len(), 1);
    assert_eq!(inbox.permission_denied.len(), 1);
    assert!(inbox.failed_jobs.is_empty() && inbox.failed_actions.is_empty());
    let by_rule = &inbox.permission_denied[0];
    assert_eq!(by_rule.runbook_id, "mode.set");
    let denial = by_rule.denial.as_ref().unwrap();
    assert_eq!(denial.source, DenialSource::Policy);
    assert!(denial.reason.contains("row 26"), "{}", denial.reason);
    assert_eq!(
        runner
            .store()
            .get_issue(issue.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::WaitingForHuman
    );

    // A human rejection carries the human's comment and joins the same inbox category.
    let rejected = runner
        .reject_action(
            actions[0].action_run_id,
            "alice",
            Some("a purge drops pending submissions".into()),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status, ActionStatus::Cancelled);
    let denial = rejected.denial.as_ref().unwrap();
    assert_eq!(denial.source, DenialSource::Human);
    assert_eq!(denial.decided_by.as_deref(), Some("alice"));
    assert_eq!(
        denial.comment.as_deref(),
        Some("a purge drops pending submissions")
    );
    let inbox = runner.inbox().await.unwrap();
    assert!(inbox.permission_requests.is_empty());
    assert_eq!(inbox.permission_denied.len(), 2);

    // Acknowledging takes an item out of the inbox and records who decided.
    let outcome = runner
        .review_action(
            by_rule.action_run_id,
            "alice",
            InboxDecision::Acknowledge,
            Some("mode changes are ours to make".into()),
        )
        .await
        .unwrap();
    assert!(outcome.revision.is_none());
    assert_eq!(
        outcome.reviewed.review.as_ref().unwrap().decision,
        ReviewDecision::Acknowledged
    );
    assert_eq!(runner.inbox().await.unwrap().permission_denied.len(), 1);
    // Reviewing twice is refused: the inbox is not a place to leave notes.
    assert!(
        runner
            .review_action(
                by_rule.action_run_id,
                "bob",
                InboxDecision::Acknowledge,
                None
            )
            .await
            .is_err()
    );

    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    for expected in [
        "scheduler.action_denied",
        "human.action_rejected",
        "human.review_recorded",
    ] {
        assert!(kinds.iter().any(|kind| kind == expected), "{expected}");
    }
}

/// Sending a denial upstream dispatches a revising Job that carries the reason and comment,
/// runs the Team with that feedback in front of it, and evaluates the new proposals.
#[tokio::test]
async fn sending_a_denial_upstream_runs_a_revision_with_the_feedback() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runner = harness_runner(
        dir.path(),
        worker.local_addr().unwrap().port(),
        vec![
            // Pass 1: propose a purge (held for approval).
            vec![propose("c1", "mq.purge", "redis-mq")],
            vec![diagnosis("c2")],
            // Pass 2 (the revision): read the view, then propose the restart instead.
            vec![call("c3", "read_snapshot_view", json!({}))],
            vec![propose("c4", "worker.restart", "worker-1")],
            vec![call(
                "c5",
                "submit_diagnosis",
                json!({ "summary": "restarting the worker instead of purging" }),
            )],
        ],
    );
    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    runner
        .reject_action(
            actions[0].action_run_id,
            "alice",
            Some("a purge drops pending submissions".into()),
        )
        .await
        .unwrap();

    let outcome = runner
        .review_action(
            actions[0].action_run_id,
            "alice",
            InboxDecision::SendUpstream,
            Some("restart the worker instead".into()),
        )
        .await
        .unwrap();
    let revision = outcome
        .revision
        .expect("sending upstream creates a revision");
    assert_eq!(
        outcome.reviewed.review.as_ref().unwrap().decision,
        ReviewDecision::SentUpstream {
            job_id: revision.job.job_id
        }
    );

    // The revising Job is a new pass on the same Issue, bound to a fresh Snapshot, carrying the
    // feedback with both the denial and the reviewer's comment.
    assert_eq!(revision.job.issue_id, issue.issue_id);
    assert_eq!(revision.job.revises_job_id, Some(job.job_id));
    assert_ne!(revision.job.base_snapshot_id(), job.base_snapshot_id());
    assert_eq!(revision.job.status, JobStatus::Completed);
    assert_eq!(revision.job.feedback.len(), 1);
    let feedback = &revision.job.feedback[0];
    assert_eq!(
        feedback.comment.as_deref(),
        Some("restart the worker instead")
    );
    let FeedbackOrigin::DeniedAction { denial, .. } = &feedback.origin else {
        panic!("feedback about a denied action");
    };
    assert_eq!(
        denial.comment.as_deref(),
        Some("a purge drops pending submissions")
    );

    // The Team saw the feedback: it is in the View Artifact and in the model's input.
    let view = runner
        .store()
        .get_artifact(revision.job.snapshot_view.artifact_id)
        .await
        .unwrap();
    let view_body = String::from_utf8(runner.artifacts().read_verified(&view).unwrap()).unwrap();
    assert!(view_body.contains("human_feedback"));
    assert!(view_body.contains("restart the worker instead"));
    let transcript_id = revision.job.result.as_ref().unwrap().artifact_ids[0];
    let transcript = runner.store().get_artifact(transcript_id).await.unwrap();
    let transcript_body =
        String::from_utf8(runner.artifacts().read_verified(&transcript).unwrap()).unwrap();
    assert!(transcript_body.contains("revision pass"));
    assert!(transcript_body.contains("a purge drops pending submissions"));

    // The revised proposal went through the matrix like any other: auto, executed, verified.
    assert_eq!(revision.actions.len(), 1);
    assert_eq!(revision.actions[0].runbook_id, "worker.restart");
    assert_eq!(revision.actions[0].status, ActionStatus::Succeeded);
    assert!(runner.inbox().await.unwrap().permission_denied.is_empty());

    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(kinds.iter().any(|kind| kind == "scheduler.job_revised"));
    let snapshots = runner.store().list_snapshots().await.unwrap();
    assert!(
        snapshots
            .iter()
            .any(|snapshot| snapshot.cause == SnapshotCause::HumanFeedback)
    );
}

/// A Job that fails waits in the Failed Job inbox until a human reviews it; a Team backend that
/// errors out is recorded the same way instead of leaving the Job running forever.
#[tokio::test]
async fn failed_jobs_wait_for_review() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runner = harness_runner(
        dir.path(),
        worker.local_addr().unwrap().port(),
        // First report: prose instead of the terminal tool. Second report: no script at all,
        // so the model client itself errors.
        vec![vec![text("Everything looks fine, done!")]],
    );
    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Printer", "stuck"))
        .await
        .unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(
        runner
            .store()
            .get_issue(issue.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::WaitingForHuman
    );

    let (_issue2, job2) = runner
        .handle_report(HumanReport::new("op", "Printer again", "still stuck"))
        .await
        .unwrap();
    assert_eq!(job2.status, JobStatus::Failed);
    assert!(
        job2.result
            .as_ref()
            .unwrap()
            .summary
            .contains("Team backend failed")
    );

    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.failed_jobs.len(), 2);

    let outcome = runner
        .review_job(
            job.job_id,
            "bob",
            InboxDecision::Acknowledge,
            Some("known model hiccup".into()),
        )
        .await
        .unwrap();
    assert!(outcome.revision.is_none());
    assert_eq!(outcome.reviewed.review.as_ref().unwrap().reviewer, "bob");
    assert_eq!(runner.inbox().await.unwrap().failed_jobs.len(), 1);

    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(kinds.iter().any(|kind| kind == "scheduler.job_failed"));
}

/// An action the Platform refuses is a failure for the Failed inbox, and sending it upstream
/// tells the next pass what went wrong.
#[tokio::test]
async fn failed_actions_can_be_sent_upstream() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runner = harness_runner(
        dir.path(),
        worker.local_addr().unwrap().port(),
        vec![
            // worker.start is auto in rehearsal but has no configured command → refused.
            vec![propose("c1", "worker.start", "worker-1")],
            vec![diagnosis("c2")],
            // Revision: no proposals this time.
            vec![call(
                "c3",
                "submit_diagnosis",
                json!({ "summary": "no runbook can restart this station" }),
            )],
        ],
    );
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Printer", "stuck"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();
    assert_eq!(actions[0].status, ActionStatus::Failed);
    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.failed_actions.len(), 1);
    assert!(inbox.permission_denied.is_empty());

    let outcome = runner
        .review_action(
            actions[0].action_run_id,
            "carol",
            InboxDecision::SendUpstream,
            None,
        )
        .await
        .unwrap();
    let revision = outcome.revision.unwrap();
    let FeedbackOrigin::FailedAction {
        summary, evidence, ..
    } = &revision.job.feedback[0].origin
    else {
        panic!("feedback about a failed action");
    };
    assert!(summary.contains("no command is configured"), "{summary}");
    // The Platform's refusal travels upstream as sanitized evidence, fenced in the View.
    assert!(
        evidence
            .as_deref()
            .unwrap()
            .contains("refused: no command configured")
    );
    let view = runner
        .store()
        .get_artifact(revision.job.snapshot_view.artifact_id)
        .await
        .unwrap();
    let view_body = String::from_utf8(runner.artifacts().read_verified(&view).unwrap()).unwrap();
    assert!(view_body.contains("execution_evidence"));
    assert!(revision.actions.is_empty());
    assert!(runner.inbox().await.unwrap().failed_actions.is_empty());
}

/// The deterministic Team also acknowledges feedback, so a revision pass without a model still
/// visibly took the humans into account.
#[tokio::test]
async fn readonly_team_acknowledges_feedback_in_its_diagnosis() {
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

    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runner = SliceRunner::wire(
        topology(worker.local_addr().unwrap().port(), closed_port()),
        dir.path(),
        TeamBackend::ReadOnly,
        platform_config(),
    )
    .unwrap();
    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    assert!(runner.inbox().await.unwrap().failed_jobs.is_empty());

    let feedback = vec![HumanFeedback::new(
        FeedbackOrigin::FailedJob {
            job_id: job.job_id,
            summary: "the first pass missed the queue depth".into(),
        },
        "dave",
        Some("look at redis first".into()),
    )];
    let snapshot = runner.capture(SnapshotCause::HumanFeedback).await.unwrap();
    let brief = broccoli_devops_agent::domain::JobBrief::new(
        broccoli_devops_agent::domain::TeamKind::Operate,
        vec!["observe.readonly".into()],
        vec!["worker-1".into(), "redis-mq".into()],
    )
    .revising(job.job_id, feedback);
    let (revision, view) = runner
        .scheduler()
        .dispatch_job(
            issue.issue_id,
            snapshot.snapshot_id,
            brief,
            PROFILE_OPERATE_READONLY,
        )
        .await
        .unwrap();

    let team = ReadOnlyOperateTeam::new(FileArtifactStore::new(dir.path().join("artifact-bodies")));
    let sink = CollectingSink::default();
    let (_handle, signal) = cancel_pair();
    team.run_job(&revision, &view, &sink, signal).await.unwrap();
    let delivered = sink.delivered.lock().await;
    let result = delivered.last().unwrap().final_result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::DiagnosisOnly);
    assert!(result.summary.contains("Revision pass"));
    assert!(result.summary.contains("look at redis first"));
    assert!(result.summary.contains("redis-mq"));
}
