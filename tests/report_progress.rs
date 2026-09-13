//! Report correlation is visible before a blocking HTTP report request finishes.
use async_trait::async_trait;
use broccoli_agent_harness::{ModelClient, ModelRequest, ModelTurn, error::HarnessResult};
use broccoli_devops_agent::api::{ApiState, router};
use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{EventRecord, OperationMode};
use broccoli_devops_agent::platform::PlatformConfig;
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::IssueClosure;
use broccoli_devops_agent::topology::{DeploymentInfo, DeploymentTopology};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use uuid::Uuid;

struct WaitingModel(AtomicUsize);
#[async_trait]
impl ModelClient for WaitingModel {
    async fn complete(&self, _: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_reports_have_distinct_early_admission_and_reject_duplicate_ids() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(WaitingModel(AtomicUsize::new(0)));
    let runner = Arc::new(
        SliceRunner::wire(
            DeploymentTopology {
                deployment: DeploymentInfo {
                    id: Uuid::now_v7(),
                    name: "report-scope".into(),
                    topology_revision: "1".into(),
                    operation_mode: OperationMode::Rehearsal,
                },
                resources: vec![],
                dependencies: vec![],
            },
            dir.path(),
            TeamBackend::Harness {
                client: model.clone(),
                budget: Default::default(),
                model: "waiting".into(),
                label: "waiting".into(),
            },
            PlatformConfig::default(),
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            router(Arc::new(ApiState::new(
                runner.clone(),
                AppConfig::default(),
            ))),
        )
        .into_future(),
    );
    let http = reqwest::Client::new();
    let ids = [Uuid::now_v7(), Uuid::now_v7()];
    let mut tasks = vec![];
    for id in ids {
        let (client, url) = (http.clone(), format!("{base}/api/reports"));
        tasks.push(tokio::spawn(async move {
            client.post(url).json(&json!({"report_id":id,"title":"concurrent report","description":"test","reporter":"test"})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap()
        }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while model.0.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tasks.iter().all(|task| !task.is_finished()),
        "both HTTP reports are still waiting for their own model calls"
    );
    let events = http
        .get(format!("{base}/api/events?after=0"))
        .send()
        .await
        .unwrap()
        .json::<Vec<EventRecord>>()
        .await
        .unwrap();
    let mut admitted = vec![];
    for id in ids {
        let operation = events
            .iter()
            .find(|event| {
                event.kind == "operation.started" && event.payload["operation_id"] == json!(id)
            })
            .expect("the request is visible by its client ID before admission");
        let report = events
            .iter()
            .find(|event| {
                event.kind == "human.issue_reported" && event.payload["report_id"] == json!(id)
            })
            .unwrap();
        let created = events
            .iter()
            .find(|event| {
                event.kind == "scheduler.issue_created"
                    && event.payload["source_event_id"] == json!(report.event_id)
            })
            .unwrap();
        assert!(report.sequence < created.sequence);
        assert!(operation.sequence < report.sequence);
        admitted.push(created.issue_id.unwrap());
    }
    assert_ne!(admitted[0], admitted[1]);
    let duplicate = http
        .post(format!("{base}/api/reports"))
        .json(&json!({"report_id":ids[0],"title":"duplicate","description":"test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 409);
    assert_eq!(runner.store().list_issues().await.unwrap().len(), 2);
    for issue in &admitted {
        runner
            .close_issue(*issue, IssueClosure::Cancelled, "test", None)
            .await
            .unwrap();
    }
    for (task, issue) in tasks.into_iter().zip(admitted) {
        let response = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response["issue"]["issue_id"], json!(issue));
    }
    assert!(runner.running_passes().await.is_empty());
    assert!(runner.operations().active().is_empty());
    server.abort();
}
