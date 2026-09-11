use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use broccoli_agent_harness::error::HarnessResult;
use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_agent_harness::{ModelClient, ModelRequest, ModelTurn};
use broccoli_devops_agent::domain::{
    ActionProposal, ActionStatus, HumanReport, IssueStatus, JobBrief, JobStatus, OperationMode,
    ResourceKind, SnapshotCause, TeamKind,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::{CaptureRequest, StateStore};
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::IssueClosure;
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::json;
use uuid::Uuid;

#[derive(Default)]
struct BlockedModel {
    entered: AtomicUsize,
    dropped: Arc<AtomicUsize>,
}
struct ModelCallGuard(Arc<AtomicUsize>);
impl Drop for ModelCallGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl ModelClient for BlockedModel {
    async fn complete(&self, _request: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        let _guard = ModelCallGuard(self.dropped.clone());
        self.entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}
fn topology() -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "lifecycle".into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![TopologyResource {
            id: "worker-1".into(),
            kind: ResourceKind::Worker,
            node: None,
            probes: vec![ProbeSpec {
                target: Some("127.0.0.1:1".into()),
                ..ProbeSpec::new("tcp.connect")
            }],
        }],
        dependencies: vec![],
    }
}
fn runner(dir: &std::path::Path, client: Arc<dyn ModelClient>) -> Arc<SliceRunner> {
    Arc::new(
        SliceRunner::wire(
            topology(),
            dir,
            TeamBackend::Harness {
                client,
                budget: Default::default(),
                model: "blocked-test".into(),
                label: "blocked-test".into(),
            },
            PlatformConfig::default(),
        )
        .unwrap(),
    )
}
async fn entered(model: &BlockedModel, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while model.entered.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resolving_issue_cancels_its_model_call_and_stops_followups() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockedModel::default());
    let runner = runner(dir.path(), model.clone());
    let owner = runner.clone();
    let task = tokio::spawn(async move {
        owner
            .handle_report(HumanReport::new("op", "test", "test"))
            .await
    });
    entered(&model, 1).await;
    let active = runner.running_passes().await.remove(0);
    runner
        .close_issue(
            active.issue_id,
            IssueClosure::Resolved,
            "op",
            Some("already fixed".into()),
        )
        .await
        .unwrap();
    let (issue, job) = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(issue.status, IssueStatus::Resolved);
    assert_eq!(job.status, JobStatus::Cancelled);
    assert!(job.completed_at.is_some());
    assert!(runner.running_passes().await.is_empty());
    assert_eq!(model.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(runner.drive_passes(job.clone()).await.unwrap().len(), 1);
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
    assert!(
        runner
            .scheduler()
            .create_job(
                issue.issue_id,
                job.snapshot_view.clone(),
                JobBrief::new(TeamKind::Operate, vec![], vec![])
            )
            .await
            .is_err()
    );
    assert!(
        runner
            .scheduler()
            .supersede_job(
                job.job_id,
                job.snapshot_view,
                JobBrief::new(TeamKind::Operate, vec![], vec![])
            )
            .await
            .is_err()
    );
    // Repeating close is harmless and can also clean up legacy records.
    runner
        .close_issue(issue.issue_id, IssueClosure::Resolved, "op", None)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_one_issue_does_not_cancel_another_issue() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockedModel::default());
    let runner = runner(dir.path(), model.clone());
    let one = runner.clone();
    let first = tokio::spawn(async move {
        one.handle_report(HumanReport::new("op", "one", "one"))
            .await
    });
    entered(&model, 1).await;
    let first_id = runner.running_passes().await[0].issue_id;
    let two = runner.clone();
    let second = tokio::spawn(async move {
        two.handle_report(HumanReport::new("op", "two", "two"))
            .await
    });
    entered(&model, 2).await;
    runner
        .close_issue(first_id, IssueClosure::Resolved, "op", None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let remaining = runner.running_passes().await;
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0].issue_id, first_id);
    assert_eq!(model.dropped.load(Ordering::SeqCst), 1);
    runner
        .close_issue(remaining[0].issue_id, IssueClosure::Cancelled, "op", None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_request_cleans_runtime_registry_and_persisted_job() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockedModel::default());
    let runner = runner(dir.path(), model.clone());
    let owner = runner.clone();
    let task = tokio::spawn(async move {
        owner
            .handle_report(HumanReport::new("op", "disconnect", "disconnect"))
            .await
    });
    entered(&model, 1).await;
    let id = runner.running_passes().await[0].job_id;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runner.running_passes().await.is_empty()
                && runner.store().get_job(id).await.unwrap().status == JobStatus::Cancelled
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(model.dropped.load(Ordering::SeqCst), 1);
    assert!(!runner.cancel_pass(id, "op").await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn close_cancels_unstarted_actions_and_prevents_execution() {
    let dir = tempfile::tempdir().unwrap();
    let topo = topology();
    let capture = CaptureRequest {
        deployment_id: topo.deployment.id,
        topology_revision: "t1".into(),
        cause: SnapshotCause::BeforeAction,
        operation_mode: OperationMode::Rehearsal,
        parent_snapshot_id: None,
        requested_probe_ids: vec![],
    };
    let marker = dir.path().join("must-not-run");
    let platform = PlatformConfig {
        dry_run: false,
        runbooks: ["worker.restart", "machine.reboot"]
            .into_iter()
            .map(|id| RunbookCommand {
                id: id.into(),
                command: format!("touch {}", marker.display()),
            })
            .collect(),
        ..PlatformConfig::default()
    };
    let model = ScriptedModelClient::new(vec![vec![call(
        "done",
        "submit_diagnosis",
        json!({"summary":"needs operations"}),
    )]]);
    let runner = SliceRunner::wire(
        topo,
        dir.path(),
        TeamBackend::Harness {
            client: Arc::new(model),
            budget: Default::default(),
            model: "scripted".into(),
            label: "scripted".into(),
        },
        platform,
    )
    .unwrap();
    let (issue, job) = runner
        .handle_report(HumanReport::new("op", "actions", "actions"))
        .await
        .unwrap();
    let mut ids = vec![];
    for id in ["worker.restart", "machine.reboot"] {
        let a = runner
            .scheduler()
            .create_action_run(
                job.job_id,
                ActionProposal {
                    runbook_id: id.into(),
                    target_ids: vec!["worker-1".into()],
                    arguments: vec![],
                    reason: "test".into(),
                    expected_effect: "test".into(),
                    verification_probe_ids: vec![],
                },
                capture.clone(),
                id,
            )
            .await
            .unwrap();
        ids.push(a.action_run_id);
    }
    runner
        .close_issue(issue.issue_id, IssueClosure::Cancelled, "op", None)
        .await
        .unwrap();
    for id in ids {
        assert_eq!(
            runner.store().get_action_run(id).await.unwrap().status,
            ActionStatus::Cancelled
        );
        assert!(runner.scheduler().execute_action(id).await.is_err());
        assert!(runner.approve_action(id, "op").await.is_err());
    }
    assert!(!marker.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_cancels_legacy_running_jobs_of_a_resolved_issue() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModelClient::new(vec![vec![call(
        "done",
        "submit_diagnosis",
        json!({"summary":"observed"}),
    )]]);
    let runner = runner(dir.path(), Arc::new(model));
    let (issue, mut job) = runner
        .handle_report(HumanReport::new("op", "legacy", "legacy"))
        .await
        .unwrap();
    runner
        .close_issue(issue.issue_id, IssueClosure::Resolved, "op", None)
        .await
        .unwrap();
    // Recreate the persisted shape left by the old lifecycle bug, with no executing task.
    job.status = JobStatus::Running;
    job.completed_at = None;
    runner.store().update_job(job.clone()).await.unwrap();
    runner.scheduler().recover().await.unwrap();
    assert_eq!(
        runner.store().get_job(job.job_id).await.unwrap().status,
        JobStatus::Cancelled
    );
    assert_eq!(
        runner
            .store()
            .get_issue(issue.issue_id)
            .await
            .unwrap()
            .status,
        IssueStatus::Resolved
    );
}
