//! The agent's output language is a startup setting: with `zh-CN` every human-facing string the
//! control plane writes — diagnoses, denial rationales, verification and execution summaries,
//! event summaries, feedback text — comes out in Simplified Chinese, and the model is told to
//! answer in it. This file sets the language for its own process, so it must stay separate from
//! the English test binaries.

use std::net::TcpListener;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call};
use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{ActionStatus, HumanReport, OperationMode, ResourceKind};
use broccoli_devops_agent::i18n::{Language, language, set_language};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{InboxDecision, SliceRunner, TeamBackend};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::json;
use uuid::Uuid;

fn topology(worker_port: u16) -> DeploymentTopology {
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "i18n-test".into(),
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
                    ..ProbeSpec::new("tcp.connect")
                }],
            },
            TopologyResource {
                id: "redis-mq".into(),
                kind: ResourceKind::Redis,
                node: None,
                probes: Vec::new(),
            },
        ],
        dependencies: Vec::new(),
    }
}

/// The config names the language; the process-wide setting and the model instruction follow.
#[test]
fn config_selects_the_language() {
    let config = AppConfig::from_toml("[agent]\nlanguage = \"zh-CN\"\n").unwrap();
    assert_eq!(config.agent.language, Language::ZhCn);
    assert_eq!(config.agent.language.tag(), "zh-CN");
    assert!(
        config
            .agent
            .language
            .model_instruction()
            .contains("简体中文")
    );
    let default = AppConfig::from_toml("").unwrap();
    assert_eq!(default.agent.language, Language::En);
    assert_eq!(default.agent.language.model_instruction(), "");
    assert_eq!(
        AppConfig::from_toml("[agent]\nlanguage = \"zh\"\n")
            .unwrap()
            .agent
            .language,
        Language::ZhCn
    );
}

/// With zh-CN set, the deterministic diagnosis, the rule denial, the human feedback, the
/// Platform summary, and the event log are all written in Chinese, and the model is asked to
/// answer in Chinese — while identifiers stay as they are.
#[tokio::test]
async fn agent_output_follows_the_configured_language() {
    set_language(Language::ZhCn);
    assert_eq!(language(), Language::ZhCn);

    let worker = TcpListener::bind("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let topo = topology(worker.local_addr().unwrap().port());

    // Deterministic Team: the diagnosis names the unprobed resource in Chinese.
    let readonly = SliceRunner::wire(
        topo.clone(),
        dir.path().join("readonly").as_path(),
        TeamBackend::ReadOnly,
        PlatformConfig::default(),
    )
    .unwrap();
    let (_issue, job) = readonly
        .handle_report(HumanReport::new("op", "队列卡住", "没有判题结果"))
        .await
        .unwrap();
    let result = job.result.unwrap();
    assert!(result.summary.contains("观测盲区"), "{}", result.summary);
    assert!(
        result.unresolved_questions[0].contains("没有观测结果"),
        "{:?}",
        result.unresolved_questions
    );

    // Model-backed Team: a denied proposal, a failed execution, and the feedback loop.
    let client = ScriptedModelClient::new(vec![
        vec![
            call(
                "c1",
                "propose_action",
                json!({ "runbook_id": "mode.set", "target_ids": ["worker-1"], "reason": "r", "expected_effect": "e" }),
            ),
            call(
                "c2",
                "propose_action",
                json!({ "runbook_id": "worker.start", "target_ids": ["worker-1"], "reason": "r", "expected_effect": "e" }),
            ),
        ],
        vec![call(
            "c3",
            "submit_diagnosis",
            json!({ "summary": "已诊断" }),
        )],
        vec![call(
            "c4",
            "submit_diagnosis",
            json!({ "summary": "修订完成" }),
        )],
    ]);
    let runner = SliceRunner::wire(
        topo,
        dir.path().join("harness").as_path(),
        TeamBackend::Harness {
            client: Arc::new(client),
            budget: Default::default(),
            model: "scripted-model".into(),
            label: "scripted".into(),
        },
        PlatformConfig {
            runbooks: vec![RunbookCommand {
                id: "worker.restart".into(),
                command: "echo x".into(),
            }],
            ..PlatformConfig::default()
        },
    )
    .unwrap();
    let (_issue, job) = runner
        .handle_report(HumanReport::new("op", "工作节点无心跳", "判题停滞"))
        .await
        .unwrap();
    let actions = runner.run_proposals(&job).await.unwrap();

    let denied = &actions[0];
    assert_eq!(denied.status, ActionStatus::Cancelled);
    let reason = &denied.denial.as_ref().unwrap().reason;
    assert!(reason.contains("范围校验"), "{reason}");
    assert!(
        reason.contains("`operate.mode`"),
        "identifiers survive: {reason}"
    );

    let failed = &actions[1];
    assert_eq!(failed.status, ActionStatus::Failed);
    assert!(
        failed
            .execution_summary
            .as_deref()
            .unwrap()
            .contains("未配置命令")
    );

    // The model was told to write Chinese; the transcript's instructions carry the line.
    let transcript_id = job.result.as_ref().unwrap().artifact_ids[0];
    let transcript = runner.store().get_artifact(transcript_id).await.unwrap();
    let body = String::from_utf8(runner.artifacts().read_verified(&transcript).unwrap()).unwrap();
    assert!(body.contains("简体中文"));

    // Feedback sent upstream is described in Chinese for the next pass.
    let outcome = runner
        .review_action(
            denied.action_run_id,
            "alice",
            InboxDecision::SendUpstream,
            Some("换一种做法".into()),
        )
        .await
        .unwrap();
    let revision = outcome.revision.unwrap();
    let described = revision.job.feedback[0].describe();
    assert!(described.contains("已被拒绝"), "{described}");
    assert!(described.contains("审核人 alice"), "{described}");

    let summaries: Vec<String> = runner
        .store()
        .list_events()
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.summary)
        .collect();
    assert!(
        summaries
            .iter()
            .any(|s| s.contains("调度器创建并派发了任务"))
    );
    assert!(summaries.iter().any(|s| s.contains("已按规则拒绝")));
    assert!(summaries.iter().any(|s| s.contains("送回上游")));
    drop(worker);
}
