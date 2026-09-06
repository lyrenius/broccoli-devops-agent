//! Token accounting and the spend ceiling that halts dispatch.
//!
//! A model-backed pass costs money, so the control plane has to say what it spent, price it only
//! when it can, and stop when the operator's ceiling is reached.

use std::net::TcpListener;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_agent_harness::{AssistantItem, ModelClient, Usage};
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
use broccoli_devops_agent::usage::{Pricing, SpendBudget};
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
