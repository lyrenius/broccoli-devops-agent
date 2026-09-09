//! Event history must remain searchable and pageable beyond the live tail window.
use broccoli_devops_agent::api::{ApiState, router};
use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{EventRecord, NewEvent, OperationMode};
use broccoli_devops_agent::platform::PlatformConfig;
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::topology::{DeploymentInfo, DeploymentTopology};
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn history_search_and_backward_cursor_do_not_lose_old_matches() {
    let dir = tempfile::tempdir().unwrap();
    let issue_id = Uuid::now_v7();
    let runner = Arc::new(
        SliceRunner::wire(
            DeploymentTopology {
                deployment: DeploymentInfo {
                    id: Uuid::now_v7(),
                    name: "history-test".into(),
                    topology_revision: "test".into(),
                    operation_mode: OperationMode::Rehearsal,
                },
                resources: vec![],
                dependencies: vec![],
            },
            dir.path(),
            TeamBackend::ReadOnly,
            PlatformConfig::default(),
        )
        .unwrap(),
    );
    for n in 1..=260 {
        let summary = if [5, 120, 240].contains(&n) {
            "Needle evidence"
        } else {
            "unrelated probe"
        };
        runner
            .store()
            .append_event(NewEvent::new("tester", "test.event", summary).with_issue(issue_id))
            .await
            .unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/events", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            router(Arc::new(ApiState::new(runner, AppConfig::default()))),
        )
        .into_future(),
    );
    let http = reqwest::Client::new();
    let get = |query: String| {
        let http = http.clone();
        let base = base.clone();
        async move {
            http.get(format!("{base}?{query}"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Vec<EventRecord>>()
                .await
                .unwrap()
        }
    };
    let tail = get("limit=200".into()).await;
    assert_eq!(tail.len(), 200);
    assert_eq!(tail[0].sequence, 61);
    let recent = get(format!("q=NEEDLE&limit=2&issue_id={issue_id}")).await;
    assert_eq!(
        recent.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![120, 240]
    );
    let older = get(format!(
        "q=needle&limit=2&before={}&issue_id={issue_id}",
        recent[0].sequence
    ))
    .await;
    assert_eq!(
        older.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![5]
    );
    let bounded = get("q=needle&after=5&before=240".into()).await;
    assert_eq!(
        bounded.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![120]
    );
    assert!(
        get(format!("q=needle&issue_id={}", Uuid::now_v7()))
            .await
            .is_empty()
    );
    assert!(get("q=not-present".into()).await.is_empty());
    server.abort();
}
