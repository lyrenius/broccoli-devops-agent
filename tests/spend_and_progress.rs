//! Token accounting, the spend ceiling that halts dispatch, live progress, and interruption.
//!
//! These four belong in one file because they are one story: a model-backed pass is slow and it
//! costs money, so the control plane has to say what it is doing, say what it has spent, stop
//! when the operator's ceiling is reached, and stop when a human says so.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_agent_harness::{
    AssistantItem, HarnessResult, ModelClient, ModelRequest, ModelTurn, Usage,
};
use broccoli_devops_agent::domain::{
    HumanReport, JobOutcome, JobStatus, OperationMode, ResourceKind,
};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{PassPolicy, SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::SchedulerMode;
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use broccoli_devops_agent::usage::{Pricing, SpendBudget, UsageTotals};
use serde_json::json;
use uuid::Uuid;

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn topology() -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "spend-test".into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![TopologyResource {
            id: "worker-1".into(),
            kind: ResourceKind::Worker,
            node: None,
            probes: vec![ProbeSpec {
                target: Some(format!("127.0.0.1:{}", closed_port())),
                url: None,
                ..ProbeSpec::new("tcp.connect")
            }],
        }],
        dependencies: Vec::new(),
    }
}

fn platform_config() -> PlatformConfig {
    PlatformConfig {
        dry_run: true,
        runbooks: vec![RunbookCommand {
            id: "service.status".into(),
            command: "echo status {target}".into(),
        }],
        ..PlatformConfig::default()
    }
}

fn diagnose(id: &str, summary: &str) -> AssistantItem {
    call(
        id,
        "submit_diagnosis",
        json!({ "summary": summary, "outcome": "diagnosis_only" }),
    )
}

fn pricing() -> Pricing {
    Pricing {
        input_per_mtok: 3.0,
        cached_input_per_mtok: Some(0.3),
        output_per_mtok: 15.0,
        currency: "USD".into(),
    }
}

/// A model that takes its time, so a test can observe a pass while it is still running.
///
/// Every other double here answers instantly, which is exactly the condition under which
/// "progress" and "interruption" cannot be tested at all.
struct SlowModelClient {
    inner: ScriptedModelClient,
    delay: Duration,
}

#[async_trait::async_trait]
impl ModelClient for SlowModelClient {
    async fn complete(&self, request: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        tokio::time::sleep(self.delay).await;
        self.inner.complete(request).await
    }
}

/// A runner over the given model backend.
fn runner_with(
    dir: &std::path::Path,
    client: Arc<dyn ModelClient>,
    budget: SpendBudget,
) -> SliceRunner {
    SliceRunner::wire_with(
        topology(),
        dir,
        TeamBackend::Harness {
            client,
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
    .with_spend(Some(pricing()), budget)
}

/// A runner whose scripted model answers instantly and reports the given per-turn usage.
fn runner(
    dir: &std::path::Path,
    turns: Vec<Vec<AssistantItem>>,
    usage: Usage,
    budget: SpendBudget,
) -> SliceRunner {
    let client = Arc::new(ScriptedModelClient::new(turns).with_usage_per_turn(usage));
    runner_with(dir, client as Arc<dyn ModelClient>, budget)
}

fn report(title: &str) -> HumanReport {
    HumanReport::new("tester", title, "the worker stopped answering")
}

/// What a pass spent lands on the Job, in the EventLog, and in the priced totals.
#[tokio::test(flavor = "multi_thread")]
async fn a_pass_records_what_it_spent_and_the_totals_price_it() {
    let dir = tempfile::tempdir().unwrap();
    let runner = runner(
        dir.path(),
        vec![
            vec![call("c1", "read_snapshot_view", json!({}))],
            vec![diagnose("c2", "the worker is unreachable")],
        ],
        // 1M input of which 500k cached, 100k output, per turn; two turns.
        Usage::reported(1_000_000, 500_000, 100_000),
        SpendBudget::default(),
    );

    let (_issue, job) = runner.handle_report(report("worker down")).await.unwrap();
    let job = runner.store().get_job(job.job_id).await.unwrap();

    let usage = job.usage.expect("a model-backed pass records its usage");
    assert_eq!(usage.model, "test-model");
    assert_eq!(usage.input_tokens, 2_000_000);
    assert_eq!(usage.cached_input_tokens, 1_000_000);
    assert_eq!(usage.output_tokens, 200_000);
    assert_eq!(usage.requests, 2);
    assert!(usage.is_complete());

    // The EventLog is the ledger the totals are summed from.
    let events = runner.store().list_events().await.unwrap();
    let recorded: Vec<_> = events
        .iter()
        .filter(|event| event.kind == broccoli_devops_agent::usage::USAGE_EVENT_KIND)
        .collect();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].job_id, Some(job.job_id));

    let totals = runner.usage_totals().await.unwrap();
    assert_eq!(totals.passes, 1);
    assert_eq!(totals.total_tokens, 2_200_000);
    // 1M fresh input at 3.00, 1M cached at 0.30, 200k output at 15.00.
    let cost = totals.cost.expect("a configured price list prices the run");
    assert!((cost - (3.0 + 0.3 + 3.0)).abs() < 1e-9, "got {cost}");
    assert_eq!(totals.by_model[0].model, "test-model");
}

/// Reaching the ceiling freezes dispatch and refuses the next report, and the reason says why.
#[tokio::test(flavor = "multi_thread")]
async fn the_spend_ceiling_halts_dispatch_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let runner = runner(
        dir.path(),
        vec![
            vec![diagnose("c1", "first look")],
            vec![diagnose("c2", "never reached")],
        ],
        Usage::reported(60_000, 0, 10_000),
        SpendBudget {
            max_total_tokens: 50_000,
            max_total_cost: 0.0,
        },
    );

    // The first report is accepted — the ceiling is only reached by running it.
    let (_issue, job) = runner.handle_report(report("first")).await.unwrap();
    let job = runner.store().get_job(job.job_id).await.unwrap();
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(
        job.result.unwrap().outcome,
        JobOutcome::DiagnosisOnly,
        "the pass that spent the budget still delivers its result"
    );

    // Having spent it, the Scheduler is fully frozen and says why in the log.
    assert_eq!(runner.scheduler().mode().await, SchedulerMode::FullyFrozen);
    let events = runner.store().list_events().await.unwrap();
    let frozen = events
        .iter()
        .find(|event| event.kind == "scheduler.budget_exhausted")
        .expect("the halt is recorded");
    assert!(frozen.summary.contains("token budget is spent"));
    assert!(
        frozen.summary.contains("[budget]"),
        "the operator is told how to raise it: {}",
        frozen.summary
    );

    // And the next report is refused rather than quietly spending more.
    let error = runner.handle_report(report("second")).await.unwrap_err();
    assert!(error.to_string().contains("BudgetExhausted"), "got {error}");

    let totals = runner.usage_totals().await.unwrap();
    let status = totals
        .budget
        .as_ref()
        .expect("a configured ceiling is reported");
    assert!(status.exceeded);
    assert!((status.used_fraction - 1.0).abs() < 1e-9);
    assert!(totals.one_line().contains("100% of budget"));
}

/// Tokens are counted even when they cannot be priced, and a silent relay is visible as a gap.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_reports_no_usage_is_a_gap_not_a_free_run() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(ScriptedModelClient::new(vec![vec![diagnose(
        "c1",
        "the worker is unreachable",
    )]]));
    let runner = SliceRunner::wire_with(
        topology(),
        dir.path(),
        TeamBackend::Harness {
            client: client as Arc<dyn ModelClient>,
            budget: Default::default(),
            model: "quiet-model".into(),
            label: "scripted".into(),
        },
        platform_config(),
        PassPolicy {
            max_auto_passes: 1,
            max_inspections: 2,
        },
    )
    .unwrap();

    runner.handle_report(report("worker down")).await.unwrap();

    let totals = runner.usage_totals().await.unwrap();
    assert_eq!(totals.passes, 1);
    assert_eq!(totals.total_tokens, 0);
    assert_eq!(totals.requests_without_usage, 1);
    assert_eq!(
        totals.cost, None,
        "no price list was configured, so no cost is claimed"
    );
    assert!(
        totals.one_line().contains("reported no usage"),
        "the gap is stated: {}",
        totals.one_line()
    );
}

/// Progress reaches a watcher while the pass is still running, and the pass can be interrupted.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_pass_reports_progress_and_can_be_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    // The script never concludes: without an interruption this pass would run to its turn budget.
    let turns: Vec<Vec<AssistantItem>> = (0..64)
        .map(|index| vec![call(format!("c{index}"), "read_snapshot_view", json!({}))])
        .collect();
    let client = Arc::new(SlowModelClient {
        inner: ScriptedModelClient::new(turns).with_usage_per_turn(Usage::reported(1_000, 0, 100)),
        delay: Duration::from_millis(50),
    });
    let runner = Arc::new(runner_with(
        dir.path(),
        client as Arc<dyn ModelClient>,
        SpendBudget::default(),
    ));

    let mut progress = runner.watch_progress();
    let reporting = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.handle_report(report("stuck")).await })
    };

    // Watch the pass advance, then interrupt it partway through — after a turn has been billed,
    // so the test can also check that what was already spent is not lost.
    let mut lines: Vec<String> = Vec::new();
    let watched = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(callback) = progress.recv().await {
            let seen = callback.summary.contains("Running `read_snapshot_view`");
            lines.push(callback.summary);
            if seen {
                return;
            }
        }
    })
    .await;
    assert!(watched.is_ok(), "progress must arrive while the pass runs");

    let running = runner.running_passes().await;
    let pass = running
        .first()
        .expect("the pass is registered while it runs");
    assert!(
        runner.cancel_pass(pass.job_id, "alice").await.unwrap(),
        "a running pass can be stopped by ID"
    );
    assert!(
        !runner.cancel_pass(Uuid::now_v7(), "alice").await.unwrap(),
        "a Job that is not running has nothing to interrupt"
    );

    let (_issue, job) = reporting.await.unwrap().unwrap();
    let job = runner.store().get_job(job.job_id).await.unwrap();
    let result = job
        .result
        .expect("an interrupted pass still delivers a result");
    assert_eq!(result.outcome, JobOutcome::Failed);
    assert!(
        result.summary.contains("cancelled"),
        "got {}",
        result.summary
    );
    assert!(
        !result.artifact_ids.is_empty(),
        "the transcript of an interrupted run is kept"
    );
    let usage = job
        .usage
        .expect("an interrupted run still reports what it spent");
    assert!(
        usage.total_tokens() > 0,
        "tokens billed before the interruption are kept"
    );
    assert!(
        !usage.is_complete(),
        "the request that was abandoned mid-flight is counted as one whose cost is unknown, \
         not as one that never happened"
    );

    // The watcher saw the pass advance, not just its conclusion.
    while let Ok(callback) = progress.try_recv() {
        lines.push(callback.summary);
    }
    assert!(
        lines.iter().any(|line| line.contains("model turn 1/")),
        "progress is delivered while the run is going: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Running `read_snapshot_view`")),
        "each tool call is announced: {lines:?}"
    );

    // The human's interruption is on the record with the name of whoever asked for it.
    let events = runner.store().list_events().await.unwrap();
    let cancelled = events
        .iter()
        .find(|event| event.kind == "human.pass_cancelled")
        .expect("an interruption is a human decision and is recorded as one");
    assert!(cancelled.summary.contains("alice"));

    // Totals still add up over an interrupted pass.
    let totals = runner.usage_totals().await.unwrap();
    assert_eq!(totals.passes, 1);
    assert!(totals.total_tokens > 0);
    assert_eq!(
        totals,
        UsageTotals::from_events(
            &runner.store().list_events().await.unwrap(),
            Some(&pricing()),
            &SpendBudget::default(),
        )
    );
}

#[tokio::test]
async fn model_failure_keeps_known_usage_and_a_roundtrippable_complete_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let controller = runner(
        dir.path(),
        vec![vec![
            call("v", "read_snapshot_view", json!({})),
            call(
                "p",
                "propose_action",
                json!({"runbook_id":"worker.restart","target_ids":["worker-1"],"reason":"proposal before failure","expected_effect":"healthy"}),
            ),
        ]],
        Usage::reported(1000, 0, 100),
        SpendBudget::default(),
    );
    let (issue, job) = controller
        .handle_report(report("failure history"))
        .await
        .unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.usage.as_ref().unwrap().total_tokens(), 1100);
    assert!(job.result.as_ref().unwrap().proposed_actions.is_empty());
    let passes = controller.drive_passes(job.clone()).await.unwrap();
    assert!(passes.iter().all(|pass| pass.actions.is_empty()));
    let bundle = controller
        .export_session(issue.issue_id, "test")
        .await
        .unwrap();
    let transcript_artifact = bundle
        .artifacts
        .iter()
        .find(|a| {
            job.result
                .as_ref()
                .unwrap()
                .artifact_ids
                .contains(&a.artifact.artifact_id)
        })
        .unwrap();
    let broccoli_devops_agent::session::ArtifactBody::Json(value) = &transcript_artifact.body
    else {
        panic!("readable transcript")
    };
    let transcript: broccoli_agent_harness::Transcript =
        serde_json::from_value(value.clone()).unwrap();
    assert_eq!(transcript.failure.as_ref().unwrap().turn, 2);
    assert_eq!(transcript.failure.as_ref().unwrap().stage, "model");
    assert!(!transcript.instructions.is_empty());
    assert!(transcript.entries.iter().any(|e|matches!(&e.item,broccoli_agent_harness::Item::ToolOutput{tool,..} if tool=="read_snapshot_view")));
    assert_eq!(transcript.turns[0].usage.total_tokens(), 1100);
    let dest = tempfile::tempdir().unwrap();
    let other = runner(
        dest.path(),
        vec![],
        Usage::default(),
        SpendBudget::default(),
    );
    let imported = other
        .import_session(
            serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap(),
            "test",
        )
        .await
        .unwrap();
    let restored = other
        .export_session(imported.issue_id, "test")
        .await
        .unwrap();
    let restored_transcript = restored
        .artifacts
        .iter()
        .find(|a| a.artifact.artifact_id == transcript_artifact.artifact.artifact_id)
        .unwrap();
    assert_eq!(restored_transcript.body, transcript_artifact.body);
    assert_eq!(
        other.usage_totals().await.unwrap().total_tokens,
        0,
        "imported history is not this deployment's bill"
    );
}

#[tokio::test]
async fn archive_storage_failure_is_explicit_and_keeps_known_spend() {
    struct BreakArtifactStorage(std::path::PathBuf);
    #[async_trait::async_trait]
    impl ModelClient for BreakArtifactStorage {
        async fn complete(&self, _: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
            std::fs::rename(self.0.join("artifact-bodies"), self.0.join("saved-bodies")).unwrap();
            std::fs::write(self.0.join("artifact-bodies"), b"blocked directory").unwrap();
            Ok(ModelTurn::with_usage(
                vec![diagnose("d", "complete")],
                Usage::reported(1000, 0, 100),
            ))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let runner = runner_with(
        dir.path(),
        Arc::new(BreakArtifactStorage(dir.path().into())),
        SpendBudget::default(),
    );
    let (_, job) = runner
        .handle_report(report("archive failure"))
        .await
        .unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    let result = job.result.unwrap();
    assert!(result.summary.contains("Transcript archival failed"));
    assert!(result.artifact_ids.is_empty());
    assert_eq!(job.usage.unwrap().total_tokens(), 1100);
}

#[tokio::test(flavor = "multi_thread")]
async fn each_response_is_billed_while_the_next_request_is_still_running() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(SlowModelClient {
        inner: ScriptedModelClient::new(vec![
            vec![call("r1", "read_snapshot_view", json!({}))],
            vec![diagnose("r2", "done")],
        ])
        .with_usage_per_turn(Usage::reported(1000, 0, 100)),
        delay: Duration::from_millis(300),
    });
    let controller = Arc::new(runner_with(dir.path(), client, SpendBudget::default()));
    let pending = {
        let controller = controller.clone();
        tokio::spawn(async move { controller.handle_report(report("live ledger")).await })
    };
    let live = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let totals = controller.usage_totals().await.unwrap();
            if totals.calls.len() == 2 && totals.calls[1].request.status == "started" {
                break totals;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!pending.is_finished());
    assert_eq!(live.input_tokens, 1000);
    assert_eq!(live.output_tokens, 100);
    assert_eq!(live.requests, 2);
    assert_eq!(live.requests_without_usage, 1);
    assert!((live.cost.unwrap() - 0.0045).abs() < 1e-9);
    assert_eq!(live.calls[0].request.status, "succeeded");
    pending.await.unwrap().unwrap();
    let totals = controller.usage_totals().await.unwrap();
    assert_eq!(totals.total_tokens, 2200);
    assert_eq!(totals.requests, 2);
    assert_eq!(totals.passes, 1);
    assert_eq!(totals.requests_without_usage, 0);
    assert!((totals.cost.unwrap() - 0.009).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_are_attempts_with_unknown_usage_and_final_summaries_are_not_double_billed() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(
        ScriptedModelClient::new(vec![vec![diagnose("d", "done")]])
            .with_transient_failures(1)
            .with_usage_per_turn(Usage::reported(1000, 200, 100)),
    );
    let controller = runner_with(dir.path(), client, SpendBudget::default());
    let mut live = controller.settings().current();
    live.harness.retry_backoff = Duration::from_millis(1);
    controller.settings().replace(live);
    let (_, job) = controller
        .handle_report(report("retry ledger"))
        .await
        .unwrap();
    let totals = controller.usage_totals().await.unwrap();
    assert_eq!(totals.requests, 2);
    assert_eq!(totals.requests_without_usage, 1);
    assert_eq!(totals.total_tokens, 1100);
    assert_eq!(totals.passes, 1);
    assert_eq!(job.usage.unwrap().requests, 2);
    assert_eq!(totals.calls[0].request.status, "failed");
    assert_eq!(totals.calls[1].request.status, "succeeded");
    assert_ne!(
        totals.calls[0].request.request_id,
        totals.calls[1].request.request_id
    );
    assert!((totals.cost.unwrap() - 0.00396).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_global_budget_cancels_another_in_flight_model_request() {
    use std::sync::atomic::{AtomicU32, Ordering};
    struct ConcurrentModel {
        entered: AtomicU32,
        barrier: tokio::sync::Barrier,
    }
    #[async_trait::async_trait]
    impl ModelClient for ConcurrentModel {
        async fn complete(&self, _: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
            let id = self.entered.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait().await;
            if id == 0 {
                Ok(ModelTurn::with_usage(
                    vec![diagnose("d", "complete")],
                    Usage::reported(1000, 0, 100),
                ))
            } else {
                std::future::pending().await
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(ConcurrentModel {
        entered: AtomicU32::new(0),
        barrier: tokio::sync::Barrier::new(2),
    });
    let controller = Arc::new(runner_with(
        dir.path(),
        client.clone(),
        SpendBudget {
            max_total_tokens: 1000,
            max_total_cost: 0.0,
        },
    ));
    let a = {
        let c = controller.clone();
        tokio::spawn(async move { c.handle_report(report("concurrent A")).await })
    };
    let b = {
        let c = controller.clone();
        tokio::spawn(async move { c.handle_report(report("concurrent B")).await })
    };
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
        (a.await.unwrap().unwrap(), b.await.unwrap().unwrap())
    })
    .await
    .unwrap();
    assert!(a.1.status == JobStatus::Completed || b.1.status == JobStatus::Completed);
    assert!(a.1.status == JobStatus::Failed || b.1.status == JobStatus::Failed);
    assert_eq!(client.entered.load(Ordering::SeqCst), 2);
    assert_eq!(
        controller.scheduler().mode().await,
        SchedulerMode::FullyFrozen
    );
    let totals = controller.usage_totals().await.unwrap();
    assert_eq!(totals.total_tokens, 1100);
    assert_eq!(totals.requests, 2);
    assert_eq!(totals.requests_without_usage, 1);
}

#[tokio::test]
async fn ledger_replay_deduplicates_events_and_recovers_unfinished_attempts() {
    use broccoli_devops_agent::{
        domain::{ModelUsage, NewEvent},
        usage::{REQUEST_FINISHED, REQUEST_STARTED, RequestUsage},
    };
    let dir = tempfile::tempdir().unwrap();
    let controller = runner(dir.path(), vec![], Usage::default(), SpendBudget::default());
    let namespace = Uuid::now_v7();
    let mut record = RequestUsage {
        request_id: format!("{namespace}:1"),
        namespace,
        model: "test-model".into(),
        job_id: Some(namespace),
        issue_id: None,
        turn: 1,
        started_at: chrono::Utc::now(),
        finished_at: None,
        status: "started".into(),
        usage: None,
        error: None,
    };
    controller
        .store()
        .append_event(
            NewEvent::new("test", REQUEST_STARTED, "started")
                .with_job(namespace)
                .with_payload(serde_json::to_value(&record).unwrap()),
        )
        .await
        .unwrap();
    record.status = "succeeded".into();
    record.finished_at = Some(chrono::Utc::now());
    record.usage = Some(ModelUsage {
        model: "test-model".into(),
        input_tokens: 1000,
        output_tokens: 100,
        requests: 1,
        ..Default::default()
    });
    for _ in 0..2 {
        controller
            .store()
            .append_event(
                NewEvent::new("test", REQUEST_FINISHED, "finished")
                    .with_job(namespace)
                    .with_payload(serde_json::to_value(&record).unwrap()),
            )
            .await
            .unwrap();
    }
    controller
        .store()
        .append_event(
            NewEvent::new("test", "model.usage", "legacy summary")
                .with_job(namespace)
                .with_payload(serde_json::to_value(record.usage.clone().unwrap()).unwrap()),
        )
        .await
        .unwrap();
    let namespace = Uuid::now_v7();
    record.namespace = namespace;
    record.request_id = format!("{namespace}:1");
    record.job_id = None;
    record.status = "started".into();
    record.usage = None;
    record.finished_at = None;
    controller
        .store()
        .append_event(
            NewEvent::new("test", REQUEST_STARTED, "orphan")
                .with_payload(serde_json::to_value(&record).unwrap()),
        )
        .await
        .unwrap();
    let before = controller.usage_totals().await.unwrap();
    assert_eq!(before.total_tokens, 1100);
    assert_eq!(before.requests, 2);
    assert_eq!(before.requests_without_usage, 1);
    drop(controller);
    let restarted = runner(dir.path(), vec![], Usage::default(), SpendBudget::default());
    restarted.recover().await.unwrap();
    restarted.recover().await.unwrap();
    let after = restarted.usage_totals().await.unwrap();
    assert_eq!(after.total_tokens, 1100);
    assert_eq!(after.requests, 2);
    assert_eq!(after.requests_without_usage, 1);
    assert_eq!(
        after
            .calls
            .iter()
            .filter(|call| call.request.status == "interrupted")
            .count(),
        1
    );
    assert_eq!(after.calls.len(), 2);
}
