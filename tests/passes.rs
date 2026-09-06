//! The investigation loop end to end: a pass that asks for probes is superseded by a pass over a
//! fresh Snapshot; a pass whose proposals ran gets a follow-up pass that sees the whole history;
//! the pass budget bounds the chain; a human's approval resumes it; "solved" is a claim the
//! Scheduler checks; and inspections are scoped, read-only, budgeted, and recorded.

use std::net::TcpListener;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_agent_harness::{AssistantItem, ModelClient};
use broccoli_devops_agent::AgentError;
use broccoli_devops_agent::domain::{
    ActionStatus, FeedbackOrigin, HumanReport, IssueStatus, JobBrief, JobOutcome, JobResult,
    JobStatus, NamedValue, OperationMode, ResourceKind, SnapshotCause, TeamCallback, TeamKind,
    VerificationEvidence,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::policy::OPERATE_CAPABILITIES;
use broccoli_devops_agent::ports::{InspectionRequest, StateStore};
use broccoli_devops_agent::runner::{
    InboxDecision, PassPolicy, PassStop, SliceRunner, TeamBackend,
};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use broccoli_devops_agent::view::PROFILE_OPERATE_READONLY;
use serde_json::json;
use uuid::Uuid;

/// A topology with one reachable worker (a local listener) and one unreachable Redis.
fn topology(worker_port: u16, redis_port: u16) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "passes-test".into(),
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

fn platform_config(dry_run: bool) -> PlatformConfig {
    PlatformConfig {
        dry_run,
        runbooks: vec![
            RunbookCommand {
                id: "service.status".into(),
                command: "echo status {target}".into(),
            },
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

fn read(id: &str) -> AssistantItem {
    call(id, "read_snapshot_view", json!({}))
}

fn propose(id: &str, runbook: &str, target: &str) -> AssistantItem {
    call(
        id,
        "propose_action",
        json!({
            "runbook_id": runbook,
            "target_ids": [target],
            "reason": "evidence in the view",
            "expected_effect": "the target becomes healthy",
            "verification_probe_ids": ["tcp.connect"],
        }),
    )
}

fn submit(id: &str, summary: &str, outcome: &str, follow_up: bool) -> AssistantItem {
    call(
        id,
        "submit_diagnosis",
        json!({ "summary": summary, "outcome": outcome, "follow_up": follow_up }),
    )
}

fn request_probes(id: &str, target: &str) -> AssistantItem {
    call(
        id,
        "request_probes",
        json!({
            "probes": [{
                "probe_id": "tcp.connect",
                "target_ids": [target],
                "reason": "confirm the observation is current",
            }],
            "summary": "the view is not enough; re-probe",
        }),
    )
}

struct Harness {
    runner: SliceRunner,
    client: Arc<ScriptedModelClient>,
}

fn harness(
    dir: &std::path::Path,
    worker_port: u16,
    dry_run: bool,
    policy: PassPolicy,
    turns: Vec<Vec<AssistantItem>>,
) -> Harness {
    let client = Arc::new(ScriptedModelClient::new(turns));
    let runner = SliceRunner::wire_with(
        topology(worker_port, closed_port()),
        dir,
        TeamBackend::Harness {
            client: client.clone() as Arc<dyn ModelClient>,
            budget: Default::default(),
            model: "scripted-model".into(),
            label: "scripted".into(),
        },
        platform_config(dry_run),
        policy,
    )
    .unwrap();
    Harness { runner, client }
}

fn view_body(runner: &SliceRunner, job: &broccoli_devops_agent::domain::Job) -> String {
    let store = runner.store();
    let artifact =
        futures_lite_block_on(store.get_artifact(job.snapshot_view.artifact_id)).unwrap();
    String::from_utf8(runner.artifacts().read_verified(&artifact).unwrap()).unwrap()
}

/// Small helper so the synchronous view reader can call an async store method.
fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// Probe request → superseding pass → proposal with follow-up → follow-up pass that sees the
/// whole history, at the pass budget, with a dry-run "solved" clamped to a diagnosis.
#[tokio::test(flavor = "multi_thread")]
async fn probe_request_and_follow_up_chain_carries_history() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let Harness { runner, client } = harness(
        dir.path(),
        worker.local_addr().unwrap().port(),
        true,
        PassPolicy::default(),
        vec![
            // Pass 1: needs more evidence.
            vec![read("c1")],
            vec![request_probes("c2", "worker-1")],
            // Pass 2 (superseding): propose, and ask to check the effect.
            vec![read("c3")],
            vec![propose("c4", "worker.restart", "worker-1")],
            vec![submit("c5", "restart the worker", "diagnosis_only", true)],
            // Pass 3 (follow-up, last automatic pass): claims solved.
            vec![read("c6")],
            vec![submit(
                "c7",
                "worker healthy after restart",
                "solved",
                false,
            )],
        ],
    );

    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Worker down", "no heartbeat"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes.len(), 3);
    let stops: Vec<_> = passes.iter().map(|pass| pass.stop).collect();
    assert_eq!(
        stops,
        vec![PassStop::Superseded, PassStop::Continued, PassStop::Done]
    );

    // Pass 1 asked for probes and was superseded.
    let first = &passes[0].job;
    assert_eq!(first.status, JobStatus::Superseded);
    let result = first.result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::NeedsMoreData);
    assert_eq!(result.requested_probes[0].probe_id, "tcp.connect");
    assert_eq!(result.requested_probes[0].target_ids, vec!["worker-1"]);
    assert_eq!(first.follow_up_budget, 2);

    // Pass 2 reasons over a fresh Snapshot captured for the request, with pass 1 in its history.
    let second = &passes[1].job;
    assert_eq!(second.supersedes_job_id, Some(first.job_id));
    assert_ne!(second.base_snapshot_id(), first.base_snapshot_id());
    assert_eq!(second.follow_up_budget, 1);
    assert_eq!(second.earlier_passes.len(), 1);
    assert_eq!(second.earlier_passes[0].job_id, first.job_id);
    assert_eq!(passes[1].actions.len(), 1);
    assert_eq!(passes[1].actions[0].status, ActionStatus::Succeeded);
    assert_eq!(
        passes[1].actions[0].verification_evidence,
        Some(VerificationEvidence::DryRun)
    );
    assert_eq!(
        passes[1].actions[0].verification_probe_ids,
        vec!["tcp.connect"]
    );

    // Pass 3 follows pass 2 over its after-Snapshot and sees both earlier passes, actions and
    // all, in the View; it is the last automatic pass, so probe requests are not offered.
    let third = &passes[2].job;
    assert_eq!(third.continues_job_id, Some(second.job_id));
    assert_eq!(
        Some(third.base_snapshot_id()),
        passes[1].actions[0].after_snapshot_id
    );
    assert_eq!(third.follow_up_budget, 0);
    assert_eq!(third.pass_number(), 3);
    assert_eq!(third.earlier_passes.len(), 2);
    let action_record = &third.earlier_passes[1].actions[0];
    assert_eq!(action_record.runbook_id, "worker.restart");
    assert!(action_record.dry_run);
    assert!(
        action_record
            .evidence
            .as_deref()
            .unwrap()
            .contains("would run on worker-1")
    );
    let body = view_body(&runner, third);
    assert!(body.contains("\"earlier_passes\""));
    assert!(body.contains("\"pass_number\": 3"));
    assert!(body.contains("would run on worker-1: echo restart worker-1"));
    assert!(body.contains("the view is not enough; re-probe"));
    let offered = client.offered_tools().await;
    assert!(offered[0].iter().any(|tool| tool == "request_probes"));
    assert!(
        !offered
            .last()
            .unwrap()
            .iter()
            .any(|tool| tool == "request_probes")
    );

    // A dry run never remediates, so the model's "solved" is recorded as a diagnosis.
    let result = third.result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::DiagnosisOnly);
    assert!(
        result
            .unresolved_questions
            .iter()
            .any(|q| q.contains("could not confirm"))
    );
    let issue = runner.store().get_issue(issue.issue_id).await.unwrap();
    assert_eq!(issue.status, IssueStatus::WaitingForHuman);

    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    for expected in [
        "scheduler.job_superseded",
        "scheduler.job_continued",
        "scheduler.result_clamped",
    ] {
        assert!(kinds.iter().any(|k| k == expected), "missing {expected}");
    }
    let snapshots = runner.store().list_snapshots().await.unwrap();
    assert!(
        snapshots
            .iter()
            .any(|s| s.cause == SnapshotCause::AgentProbeRequest)
    );
}

/// After a real (not dry-run) remediation, a follow-up pass may declare the problem solved and
/// the Issue resolves; without any remediation the same claim is clamped.
#[tokio::test(flavor = "multi_thread")]
async fn solved_needs_a_real_remediation() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let Harness { runner, .. } = harness(
        dir.path(),
        worker.local_addr().unwrap().port(),
        false,
        PassPolicy::default(),
        vec![
            // Report A: restart for real, then confirm.
            vec![read("c1")],
            vec![propose("c2", "worker.restart", "worker-1")],
            vec![submit("c3", "restarting", "diagnosis_only", true)],
            vec![read("c4")],
            vec![submit("c5", "healthy after the restart", "solved", false)],
            // Report B: claims solved having done nothing.
            vec![read("c6")],
            vec![submit("c7", "looks fine to me", "solved", false)],
        ],
    );

    let (issue_a, job) = runner
        .handle_report(HumanReport::new("op", "Worker down", "no heartbeat"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes.len(), 2);
    assert_eq!(passes[0].actions[0].status, ActionStatus::Succeeded);
    assert_eq!(
        passes[0].actions[0].verification_evidence,
        Some(VerificationEvidence::Weak),
        "the worker was healthy before too"
    );
    assert_eq!(
        passes[1].job.result.as_ref().unwrap().outcome,
        JobOutcome::Solved
    );
    let issue_a = runner.store().get_issue(issue_a.issue_id).await.unwrap();
    assert_eq!(issue_a.status, IssueStatus::Resolved);

    let (issue_b, job) = runner
        .handle_report(HumanReport::new("op", "Something odd", "not sure"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes.len(), 1);
    let result = passes[0].job.result.as_ref().unwrap();
    assert_eq!(result.outcome, JobOutcome::DiagnosisOnly);
    assert!(
        result
            .unresolved_questions
            .iter()
            .any(|q| q.contains("nothing was remediated"))
    );
    let issue_b = runner.store().get_issue(issue_b.issue_id).await.unwrap();
    assert_eq!(issue_b.status, IssueStatus::WaitingForHuman);
}

/// A held action stops the chain; the human's approval executes it and resumes the chain with
/// the follow-up pass the proposing pass asked for. Denied-only proposals do not continue.
#[tokio::test(flavor = "multi_thread")]
async fn approval_resumes_the_chain_and_denials_do_not() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let Harness { runner, .. } = harness(
        dir.path(),
        worker.local_addr().unwrap().port(),
        true,
        PassPolicy::default(),
        vec![
            // Report A: a purge needs approval in rehearsal (row 8).
            vec![read("c1")],
            vec![propose("c2", "mq.purge", "redis-mq")],
            vec![submit("c3", "purge the queue", "diagnosis_only", true)],
            // ... the follow-up after approval:
            vec![read("c4")],
            vec![submit(
                "c5",
                "queue purged; watching",
                "diagnosis_only",
                false,
            )],
            // Report B: a human-only row is denied; nothing ran, so no follow-up.
            vec![read("c6")],
            vec![propose("c7", "mode.set", "worker-1")],
            vec![submit("c8", "switch modes", "diagnosis_only", true)],
        ],
    );

    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Queue stuck", "nothing judges"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0].stop, PassStop::WaitingForApproval);
    assert_eq!(
        passes[0].actions[0].status,
        ActionStatus::WaitingForApproval
    );
    assert_eq!(runner.store().list_jobs().await.unwrap().len(), 1);

    let approved = runner
        .approve_action(passes[0].actions[0].action_run_id, "alice")
        .await
        .unwrap();
    assert_eq!(approved.status, ActionStatus::Succeeded);
    let jobs = runner.store().list_jobs().await.unwrap();
    assert_eq!(jobs.len(), 2, "the approval dispatched the follow-up pass");
    let follow_up = &jobs[1];
    assert_eq!(follow_up.continues_job_id, Some(passes[0].job.job_id));
    assert_eq!(follow_up.status, JobStatus::Completed);
    assert_eq!(
        follow_up.result.as_ref().unwrap().summary,
        "queue purged; watching"
    );
    assert_eq!(
        follow_up.earlier_passes[0].actions[0].approval,
        broccoli_devops_agent::domain::ApprovalState::Approved
    );

    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "Mode", "switch it"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0].stop, PassStop::NothingRan);
    assert_eq!(passes[0].actions[0].status, ActionStatus::Cancelled);
    assert_eq!(runner.store().list_jobs().await.unwrap().len(), 3);
}

/// Inspections run only non-mutating runbooks, only in scope, only a budgeted number of times,
/// and never while everything is frozen; each one is an event with an Artifact, and the model
/// reads the output fenced as untrusted data.
#[tokio::test(flavor = "multi_thread")]
async fn inspections_are_read_only_scoped_budgeted_and_recorded() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let Harness { runner, .. } = harness(
        dir.path(),
        worker.local_addr().unwrap().port(),
        false,
        PassPolicy {
            max_auto_passes: 1,
            max_inspections: 1,
        },
        vec![
            vec![read("c1")],
            vec![call(
                "c2",
                "inspect",
                json!({ "runbook_id": "service.status", "target_ids": ["worker-1"], "reason": "look" }),
            )],
            // Over budget, and a mutating runbook is not even in the tool's allowlist.
            vec![call(
                "c3",
                "inspect",
                json!({ "runbook_id": "service.status", "target_ids": ["redis-mq"], "reason": "look" }),
            )],
            vec![call(
                "c4",
                "inspect",
                json!({ "runbook_id": "worker.restart", "target_ids": ["worker-1"], "reason": "sneaky" }),
            )],
            vec![submit("c5", "inspected", "diagnosis_only", false)],
        ],
    );

    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "Worker odd", "check it"))
        .await
        .unwrap();
    let passes = runner.drive_passes(job).await.unwrap();
    assert_eq!(passes[0].stop, PassStop::Done);
    let job = &passes[0].job;
    assert_eq!(job.status, JobStatus::Completed);

    let transcript_id = job.result.as_ref().unwrap().artifact_ids[0];
    let transcript = runner.store().get_artifact(transcript_id).await.unwrap();
    let transcript: broccoli_agent_harness::Transcript =
        serde_json::from_slice(&runner.artifacts().read_verified(&transcript).unwrap()).unwrap();
    let outputs: Vec<(bool, String)> = transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            broccoli_agent_harness::Item::ToolOutput {
                tool,
                output,
                is_error,
                ..
            } if tool == "inspect" => Some((*is_error, output.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 3);
    assert!(!outputs[0].0);
    assert!(outputs[0].1.contains("BEGIN UNTRUSTED DATA"));
    assert!(outputs[0].1.contains("status worker-1"));
    assert!(outputs[1].0 && outputs[1].1.contains("inspection budget"));
    assert!(outputs[2].0 && outputs[2].1.contains("not an inspection runbook"));

    // The one inspection that ran is an event whose Artifact is produced by the Job.
    let events = runner.store().list_events().await.unwrap();
    let inspections: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "platform.inspection")
        .collect();
    assert_eq!(inspections.len(), 1);
    let artifact = runner
        .store()
        .get_artifact(inspections[0].artifact_ids[0])
        .await
        .unwrap();
    assert_eq!(artifact.produced_by_job_id, Some(job.job_id));
    // No ActionRun was created for it.
    assert!(runner.list_actions().await.unwrap().is_empty());

    // The gateway itself: a running Job, then the refusals the tool allowlist would otherwise
    // have hidden — a mutating runbook, an out-of-scope target — and the freeze.
    let scheduler = runner.scheduler();
    let brief = JobBrief::new(
        TeamKind::Operate,
        OPERATE_CAPABILITIES
            .iter()
            .map(ToString::to_string)
            .collect(),
        vec!["worker-1".into()],
    );
    let (running, _view) = scheduler
        .dispatch_job(
            issue.issue_id,
            issue.opened_snapshot_id,
            brief,
            PROFILE_OPERATE_READONLY,
        )
        .await
        .unwrap();
    let request = |runbook: &str, target: &str| InspectionRequest {
        runbook_id: runbook.into(),
        target_ids: vec![target.into()],
        arguments: vec![NamedValue::new("unused", "x")],
        reason: "test".into(),
    };
    let mutating = scheduler
        .inspect(running.job_id, request("worker.restart", "worker-1"))
        .await
        .unwrap();
    assert!(
        mutating
            .refused
            .as_deref()
            .unwrap()
            .contains("changes machine state")
    );
    let outside = scheduler
        .inspect(running.job_id, request("service.status", "redis-mq"))
        .await
        .unwrap();
    assert!(outside.refused.as_deref().unwrap().contains("target scope"));
    let fine = scheduler
        .inspect(running.job_id, request("service.status", "worker-1"))
        .await
        .unwrap();
    assert!(fine.succeeded && fine.refused.is_none());

    scheduler.freeze_all().await.unwrap();
    let frozen = scheduler
        .inspect(running.job_id, request("service.status", "worker-1"))
        .await
        .unwrap_err();
    assert!(matches!(frozen, AgentError::SchedulerFrozen { .. }));
    // The completed Job cannot inspect either.
    scheduler.resume().await.unwrap();
    let done = scheduler
        .inspect(job.job_id, request("service.status", "worker-1"))
        .await
        .unwrap_err();
    assert!(matches!(done, AgentError::InvalidInput(_)));
}

/// A Team that needs more data when no automatic pass is left waits in the Failed inbox; a
/// human can send it back upstream, which starts a new chain with a fresh budget.
#[tokio::test(flavor = "multi_thread")]
async fn stalled_probe_request_waits_for_a_human_and_can_be_sent_back() {
    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let Harness { runner, .. } = harness(
        dir.path(),
        worker.local_addr().unwrap().port(),
        true,
        PassPolicy {
            max_auto_passes: 1,
            max_inspections: 0,
        },
        vec![
            // The revising pass after the human sends the stalled Job back.
            vec![read("c1")],
            vec![submit(
                "c2",
                "re-probed; nothing wrong",
                "diagnosis_only",
                false,
            )],
        ],
    );
    // A stalled Job cannot come from the harness Team (it is not offered the tool when the
    // budget is zero), so stage one through the Scheduler as any other Team could produce it.
    let scheduler = runner.scheduler();
    let snapshot = runner.capture(SnapshotCause::Manual).await.unwrap();
    let issue = scheduler
        .accept_human_report(
            HumanReport::new("op", "Worker odd", "check it"),
            snapshot.snapshot_id,
        )
        .await
        .unwrap();
    let brief = JobBrief::new(
        TeamKind::Operate,
        OPERATE_CAPABILITIES
            .iter()
            .map(ToString::to_string)
            .collect(),
        vec!["worker-1".into()],
    );
    let (job, _view) = scheduler
        .dispatch_job(
            issue.issue_id,
            snapshot.snapshot_id,
            brief,
            PROFILE_OPERATE_READONLY,
        )
        .await
        .unwrap();
    let mut result = JobResult::new(JobOutcome::NeedsMoreData, "need the heartbeat");
    result
        .requested_probes
        .push(broccoli_devops_agent::domain::ProbeRequest {
            probe_id: "broccoli.worker".into(),
            target_ids: vec!["worker-1".into()],
            reason: "heartbeat".into(),
        });
    scheduler
        .handle_callback(
            TeamCallback::new(issue.issue_id, job.job_id, "done").with_final_result(result),
        )
        .await
        .unwrap();
    let job = runner.store().get_job(job.job_id).await.unwrap();
    assert_eq!(job.status, JobStatus::NeedsResnapshot);

    let passes = runner.drive_passes(job.clone()).await.unwrap();
    assert_eq!(passes[0].stop, PassStop::BudgetExhausted);
    let inbox = runner.inbox().await.unwrap();
    assert_eq!(inbox.failed_jobs.len(), 1);
    assert_eq!(inbox.failed_jobs[0].job_id, job.job_id);
    let issue = runner.store().get_issue(issue.issue_id).await.unwrap();
    assert_eq!(issue.status, IssueStatus::WaitingForHuman);

    let outcome = runner
        .review_job(
            job.job_id,
            "alice",
            InboxDecision::SendUpstream,
            Some("go ahead and re-probe".into()),
        )
        .await
        .unwrap();
    let revision = outcome.revision.unwrap();
    assert_eq!(revision.job.revises_job_id, Some(job.job_id));
    assert_eq!(revision.job.status, JobStatus::Completed);
    assert_eq!(revision.job.earlier_passes.len(), 1);
    assert!(matches!(
        &revision.job.feedback[0].origin,
        FeedbackOrigin::StalledJob { requested_probe_ids, .. }
            if requested_probe_ids == &vec!["broccoli.worker".to_string()]
    ));
    assert!(
        revision.job.feedback[0]
            .describe()
            .contains("needed more observations")
    );
    assert!(runner.inbox().await.unwrap().failed_jobs.is_empty());
    let kinds: Vec<_> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(kinds.iter().any(|k| k == "scheduler.pass_budget_exhausted"));
}
