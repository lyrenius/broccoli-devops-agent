//! Missing access to a data source must not be reported as a confirmed resource outage.
use axum::{Json, Router, http::StatusCode, routing::get};
use broccoli_devops_agent::collector::TopologyCollector;
use broccoli_devops_agent::domain::{
    HealthState, OperationMode, ResourceKind, Snapshot, SnapshotCause,
};
use broccoli_devops_agent::ports::{CaptureRequest, CollectorPort};
use broccoli_devops_agent::store::memory::InMemoryStateStore;
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

async fn api(
    status: StatusCode,
    workers: Value,
    overview: Value,
) -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/api/v1/admin/system/workers",
            get(move || {
                let body = workers.clone();
                async move { (status, Json(body)) }
            }),
        )
        .route(
            "/api/v1/admin/system/overview",
            get(move || {
                let body = overview.clone();
                async move { (status, Json(body)) }
            }),
        );
    (base, tokio::spawn(axum::serve(listener, app).into_future()))
}

async fn capture(base: &str, authenticated: bool) -> Snapshot {
    let token_env = format!("COVERAGE_TEST_{}", Uuid::now_v7().simple());
    let topology = DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "coverage".into(),
            topology_revision: "1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![
            TopologyResource {
                id: "worker-1".into(),
                node: None,
                kind: ResourceKind::Worker,
                probes: vec![ProbeSpec {
                    url: Some(base.into()),
                    token_env: Some(token_env.clone()),
                    ..ProbeSpec::new("broccoli.worker")
                }],
            },
            TopologyResource {
                id: "redis-mq".into(),
                node: None,
                kind: ResourceKind::Redis,
                probes: vec![
                    ProbeSpec {
                        target: Some(base.trim_start_matches("http://").into()),
                        ..ProbeSpec::new("tcp.connect")
                    },
                    ProbeSpec {
                        url: Some(base.into()),
                        token_env: Some(token_env.clone()),
                        queue: Some("operation_tasks".into()),
                        max: Some(5.0),
                        ..ProbeSpec::new("broccoli.queue")
                    },
                ],
            },
        ],
        dependencies: vec![],
    };
    let request = CaptureRequest {
        deployment_id: topology.deployment.id,
        topology_revision: "1".into(),
        cause: SnapshotCause::Manual,
        operation_mode: OperationMode::Rehearsal,
        parent_snapshot_id: None,
        requested_probe_ids: vec![],
    };
    let collector = TopologyCollector::new(topology, Arc::new(InMemoryStateStore::new()));
    let collector = if authenticated {
        collector.with_secret(token_env, "coverage-only-token")
    } else {
        collector
    };
    collector.capture_snapshot(request).await.unwrap()
}

fn health(snapshot: &Snapshot, id: &str) -> HealthState {
    snapshot
        .resources
        .iter()
        .find(|resource| resource.resource_id == id)
        .unwrap()
        .health
}
fn queue(depth: u32) -> Value {
    json!({"queues":[{"name":"operation_tasks","depth":depth}]})
}

#[tokio::test]
async fn missing_credentials_and_unavailable_api_are_coverage_gaps() {
    for status in [
        StatusCode::OK,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let (base, server) = api(status, json!({"workers":[]}), queue(0)).await;
        // The 200 case deliberately has no credentials; the other cases send a rejected token.
        let snapshot = capture(&base, status != StatusCode::OK).await;
        assert_eq!(
            health(&snapshot, "worker-1"),
            HealthState::Unknown,
            "{status}"
        );
        assert_eq!(
            health(&snapshot, "redis-mq"),
            HealthState::Unknown,
            "successful TCP must not hide missing queue observations"
        );
        assert_eq!(snapshot.coverage_gaps.len(), 2);
        assert!(
            snapshot
                .coverage_gaps
                .iter()
                .any(|gap| gap.probe_id == "broccoli.worker")
        );
        assert!(
            snapshot
                .coverage_gaps
                .iter()
                .any(|gap| gap.probe_id == "broccoli.queue")
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("coverage-only-token")
        );
        server.abort();
    }
}

#[tokio::test]
async fn malformed_api_data_is_unknown_not_empty_or_zero() {
    for (workers, overview) in [
        (json!({}), json!({})),
        (json!({"workers":[{}]}), json!({"queues":[{}]})),
        (
            json!({"workers":[
                {"id":"worker-1","stale":false,"seconds_since_last_seen":2,"in_flight":0},
                {"id":"worker-1","stale":false,"seconds_since_last_seen":2,"in_flight":0}
            ]}),
            json!({"queues":[{"name":"operation_tasks","depth":0},{"name":"operation_tasks","depth":7}]}),
        ),
        (
            json!({"workers":[{"id":"worker-1","stale":false}]}),
            json!({"queues":[{"name":"operation_tasks"}]}),
        ),
        (
            json!({"workers":[{"id":"worker-1","stale":false,"seconds_since_last_seen":-1,"in_flight":0}]}),
            json!({"queues":[]}),
        ),
    ] {
        let (base, server) = api(StatusCode::OK, workers, overview).await;
        let snapshot = capture(&base, true).await;
        assert_eq!(health(&snapshot, "worker-1"), HealthState::Unknown);
        assert_eq!(health(&snapshot, "redis-mq"), HealthState::Unknown);
        assert_eq!(snapshot.coverage_gaps.len(), 2);
        server.abort();
    }
}

#[tokio::test]
async fn valid_absence_or_threshold_violation_remains_an_observed_problem() {
    let (base, server) = api(StatusCode::OK, json!({"workers":[]}), queue(7)).await;
    let snapshot = capture(&base, true).await;
    assert_eq!(health(&snapshot, "worker-1"), HealthState::Down);
    assert_eq!(health(&snapshot, "redis-mq"), HealthState::Degraded);
    assert!(snapshot.coverage_gaps.is_empty());
    server.abort();
    let (base, server) = api(StatusCode::OK, json!({"workers":[{"id":"worker-1","stale":false,"seconds_since_last_seen":2,"in_flight":0}]}), queue(0)).await;
    let snapshot = capture(&base, true).await;
    assert_eq!(health(&snapshot, "worker-1"), HealthState::Healthy);
    assert_eq!(health(&snapshot, "redis-mq"), HealthState::Healthy);
    assert!(snapshot.coverage_gaps.is_empty());
    server.abort();
}
