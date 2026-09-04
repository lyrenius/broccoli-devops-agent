//! End-to-end tests for the v0.1 vertical slice: topology, probes, snapshots, human report,
//! read-only Operate Job, and restart recovery — all against a temporary data directory and
//! local sockets, no external network.

use std::sync::Arc;

use broccoli_devops_agent::AgentError;
use broccoli_devops_agent::collector::TopologyCollector;
use broccoli_devops_agent::domain::{
    HealthState, HumanReport, IssuePriority, IssueStatus, JobOutcome, JobStatus, NewEvent,
    OperationMode, SnapshotCause,
};
use broccoli_devops_agent::platform::PlatformConfig;
use broccoli_devops_agent::ports::{CaptureRequest, CollectorPort, StateStore};
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::TopScheduler;
use broccoli_devops_agent::store::file::FileStateStore;
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyDependency, TopologyResource,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

/// Binds a listener that accepts connections for the duration of the test.
async fn tcp_service() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    (listener, addr)
}

/// Serves minimal `HTTP/1.1 200 OK` responses on a local socket.
async fn http_service() -> String {
    let (listener, addr) = tcp_service().await;
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
            });
        }
    });
    addr
}

/// Accepts connections on a plain TCP socket for `tcp.connect` probes.
async fn accepting_service() -> String {
    let (listener, addr) = tcp_service().await;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            drop(stream);
        }
    });
    addr
}

/// Returns an address that refuses connections: bind, take the port, drop the listener.
async fn refused_target() -> String {
    let (listener, addr) = tcp_service().await;
    drop(listener);
    addr
}

/// Builds a three-resource test topology: one healthy TCP, one HTTP, one down, one unprobed.
async fn test_topology() -> DeploymentTopology {
    let healthy = accepting_service().await;
    let http = http_service().await;
    let down = refused_target().await;

    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "slice-test".to_string(),
            topology_revision: "test-rev-1".to_string(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![
            TopologyResource {
                id: "redis-mq".to_string(),
                kind: serde_json::from_value(serde_json::json!("redis")).unwrap(),
                node: Some("infra-1".to_string()),
                probes: vec![ProbeSpec {
                    probe: "tcp.connect".to_string(),
                    target: Some(healthy),
                    url: None,
                }],
            },
            TopologyResource {
                id: "broccoli-server".to_string(),
                kind: serde_json::from_value(serde_json::json!("broccoli_server")).unwrap(),
                node: Some("app-1".to_string()),
                probes: vec![ProbeSpec {
                    probe: "http.status".to_string(),
                    target: None,
                    url: Some(format!("http://{http}/healthz")),
                }],
            },
            TopologyResource {
                id: "postgres-main".to_string(),
                kind: serde_json::from_value(serde_json::json!("postgresql")).unwrap(),
                node: Some("infra-1".to_string()),
                probes: vec![ProbeSpec {
                    probe: "tcp.connect".to_string(),
                    target: Some(down),
                    url: None,
                }],
            },
            TopologyResource {
                id: "worker-1".to_string(),
                kind: serde_json::from_value(serde_json::json!("worker")).unwrap(),
                node: Some("judge-1".to_string()),
                probes: Vec::new(),
            },
        ],
        dependencies: vec![TopologyDependency {
            from: "broccoli-server".to_string(),
            to: "postgres-main".to_string(),
            relation: "sql".to_string(),
            critical: true,
        }],
    }
}

/// The example topology file must always parse; it is the operator's template.
#[test]
fn example_topology_parses() {
    let text = std::fs::read_to_string("config/topology.example.toml").unwrap();
    let topology = DeploymentTopology::from_toml(&text).unwrap();
    assert!(topology.resources.len() >= 5);
    assert!(!topology.dependencies.is_empty());
}

/// A topology with an unknown dependency endpoint must fail at load, not at capture.
#[test]
fn topology_validation_rejects_unknown_references() {
    let text = r#"
        [deployment]
        id = "0198c936-5f2a-7000-8000-4a6f8c2d91aa"
        name = "bad"
        topology_revision = "r1"
        operation_mode = "rehearsal"

        [[resources]]
        id = "a"
        kind = "redis"

        [[dependencies]]
        from = "a"
        to = "missing"
        relation = "x"
    "#;
    assert!(matches!(
        DeploymentTopology::from_toml(text),
        Err(AgentError::InvalidInput(_))
    ));
}

/// The Collector classifies healthy, down, and unprobed resources and records evidence events.
#[tokio::test]
async fn collector_probes_and_reports_gaps() {
    let topology = test_topology().await;
    let store = Arc::new(broccoli_devops_agent::store::memory::InMemoryStateStore::new());
    let collector = TopologyCollector::new(topology.clone(), store.clone());

    let snapshot = collector
        .capture_snapshot(CaptureRequest {
            deployment_id: topology.deployment.id,
            topology_revision: topology.deployment.topology_revision.clone(),
            cause: SnapshotCause::Manual,
            operation_mode: OperationMode::Rehearsal,
            parent_snapshot_id: None,
            requested_probe_ids: vec!["disk.smart".to_string()],
        })
        .await
        .unwrap();

    let health_of = |id: &str| {
        snapshot
            .resources
            .iter()
            .find(|resource| resource.resource_id == id)
            .unwrap()
            .health
    };
    assert_eq!(health_of("redis-mq"), HealthState::Healthy);
    assert_eq!(health_of("broccoli-server"), HealthState::Healthy);
    assert_eq!(health_of("postgres-main"), HealthState::Down);
    assert_eq!(health_of("worker-1"), HealthState::Unknown);

    // Unprobed resource and unknown requested probe both surface as explicit gaps.
    assert!(
        snapshot
            .coverage_gaps
            .iter()
            .any(|gap| gap.resource_id == "worker-1")
    );
    assert!(
        snapshot
            .coverage_gaps
            .iter()
            .any(|gap| gap.probe_id == "disk.smart")
    );

    // Each observed resource produced one evidence event referenced by the snapshot.
    let events = store.list_events().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "collector.resource_observed")
            .count(),
        4
    );
    assert!(!snapshot.evidence_ids.is_empty());
}

/// The full slice: report → Issue → View → Job → Team diagnosis → persisted result → recovery
/// across a simulated controller restart, with event sequencing continuing where it left off.
#[tokio::test]
async fn human_report_flows_to_diagnosis_and_survives_restart() {
    let topology = test_topology().await;
    let data_dir = tempfile::tempdir().unwrap();

    let (issue_id, job_id) = {
        let runner = SliceRunner::wire(
            topology.clone(),
            data_dir.path(),
            TeamBackend::ReadOnly,
            PlatformConfig::default(),
        )
        .unwrap();
        let (issue, job) = runner
            .handle_report(HumanReport::new(
                "operator",
                "Server errors on submit",
                "Contestants see 500s when submitting",
            ))
            .await
            .unwrap();

        assert_eq!(issue.priority, IssuePriority::HumanTop);
        assert_eq!(issue.status, IssueStatus::Investigating);
        assert_eq!(job.status, JobStatus::Completed);
        let result = job.result.as_ref().unwrap();
        assert_eq!(result.outcome, JobOutcome::DiagnosisOnly);
        assert!(result.summary.contains("postgres-main"));
        assert!(result.summary.contains("critically depends"));
        assert!(!result.unresolved_questions.is_empty());
        (issue.issue_id, job.job_id)
    };

    // "Restart the controller": a fresh store over the same directory, no shared memory.
    let store = Arc::new(FileStateStore::open(data_dir.path()).unwrap());
    let scheduler = TopScheduler::new(store.clone());
    let summary = scheduler.recover().await.unwrap();
    assert_eq!(summary.issue_ids, vec![issue_id]);
    assert!(
        summary.job_ids.is_empty(),
        "completed jobs need no recovery"
    );

    let job = store.get_job(job_id).await.unwrap();
    assert_eq!(job.status, JobStatus::Completed);

    // Event sequencing continues from the persisted log rather than restarting at 1.
    let before = store.list_events().await.unwrap();
    let last = before.last().unwrap().sequence;
    let appended = store
        .append_event(NewEvent::new("test", "test.after_restart", "sequencing"))
        .await
        .unwrap();
    assert_eq!(appended.sequence, last + 1);
}

/// The View redacts secret-shaped facts and fences probe detail as untrusted data.
#[tokio::test]
async fn snapshot_view_redacts_and_fences_untrusted_text() {
    use broccoli_devops_agent::domain::{NamedValue, Snapshot};
    use broccoli_devops_agent::ports::{SnapshotViewBuildRequest, SnapshotViewBuilderPort};
    use broccoli_devops_agent::view::{
        FileArtifactStore, PROFILE_OPERATE_READONLY, RedactingViewBuilder,
    };

    let mut snapshot = Snapshot::new(
        Uuid::now_v7(),
        "test-rev",
        SnapshotCause::Manual,
        OperationMode::Rehearsal,
    );
    let mut resource = broccoli_devops_agent::domain::ResourceState::new(
        "redis-mq",
        None,
        serde_json::from_value(serde_json::json!("redis")).unwrap(),
        HealthState::Healthy,
        chrono::Utc::now(),
    );
    resource
        .facts
        .push(NamedValue::new("redis_password", "hunter2"));
    resource
        .facts
        .push(NamedValue::new("probe.tcp.connect", "connected"));
    resource.facts.push(NamedValue::new("version", "7.2"));
    snapshot.resources.push(resource);

    let dir = tempfile::tempdir().unwrap();
    let builder = RedactingViewBuilder::new(FileArtifactStore::new(dir.path()));
    let request = SnapshotViewBuildRequest {
        issue_id: Uuid::now_v7(),
        team_kind: broccoli_devops_agent::domain::TeamKind::Operate,
        work_order: broccoli_devops_agent::domain::WorkOrder::new("test"),
        allowed_capabilities: Vec::new(),
        allowed_target_ids: Vec::new(),
        redaction_profile: PROFILE_OPERATE_READONLY.to_string(),
    };
    let built = builder
        .build_snapshot_view(&snapshot, &request)
        .await
        .unwrap();

    let body = std::fs::read_to_string(&built.artifact.uri).unwrap();
    assert!(!body.contains("hunter2"), "secrets must never enter a View");
    assert!(!body.contains("redis_password"));
    assert!(body.contains("untrusted_data"));
    assert!(body.contains("7.2"));
    assert_eq!(
        built.snapshot_view.content_sha256,
        built.artifact.content_sha256
    );

    // An unknown profile is refused rather than silently under-redacting.
    let mut weaker = request.clone();
    weaker.redaction_profile = "everything-v1".to_string();
    assert!(matches!(
        builder.build_snapshot_view(&snapshot, &weaker).await,
        Err(AgentError::InvalidInput(_))
    ));
}
