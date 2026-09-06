//! The Collector's own schedule: `serve` captures a Snapshot at startup and then on a fixed
//! cadence, in every Scheduler mode, until the schedule is stopped.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{OperationMode, ResourceKind, SnapshotCause};
use broccoli_devops_agent::platform::PlatformConfig;
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use uuid::Uuid;

fn topology() -> DeploymentTopology {
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "periodic-test".into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![TopologyResource {
            id: "worker-1".into(),
            kind: ResourceKind::Worker,
            node: None,
            probes: vec![ProbeSpec {
                target: Some(format!("127.0.0.1:{closed}")),
                url: None,
                ..ProbeSpec::new("tcp.connect")
            }],
        }],
        dependencies: Vec::new(),
    }
}

async fn periodic_count(runner: &SliceRunner) -> usize {
    runner
        .store()
        .list_snapshots()
        .await
        .unwrap()
        .iter()
        .filter(|snapshot| snapshot.cause == SnapshotCause::Periodic)
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshots_are_captured_on_a_cadence_in_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(
        SliceRunner::wire(
            topology(),
            dir.path(),
            TeamBackend::ReadOnly,
            PlatformConfig::default(),
        )
        .unwrap(),
    );
    assert_eq!(periodic_count(&runner).await, 0);

    // The default cadence is two minutes; the test runs a fast one.
    assert_eq!(
        AppConfig::default().collector.snapshot_interval(),
        Some(Duration::from_secs(120))
    );
    let mut live = runner.settings().current();
    live.snapshot_interval = Duration::from_millis(40);
    runner.apply_live(live);
    let schedule = runner.spawn_periodic_capture();

    // The first capture is immediate, the rest follow the cadence.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after_start = periodic_count(&runner).await;
    assert!(after_start >= 2, "got {after_start} periodic Snapshot(s)");

    // Freezing everything stops dispatch and actions, not observation.
    runner.scheduler().freeze_all().await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let while_frozen = periodic_count(&runner).await;
    assert!(
        while_frozen > after_start,
        "captures continue while frozen: {after_start} → {while_frozen}"
    );

    // Every capture is on the record with its cause.
    let events = runner.store().list_events().await.unwrap();
    let logged = events
        .iter()
        .filter(|event| {
            event.kind == "snapshot.captured" && event.payload["request"]["cause"] == "periodic"
        })
        .count();
    assert_eq!(logged, while_frozen);

    // A cadence of zero pauses the schedule; setting one again resumes it at once.
    let mut live = runner.settings().current();
    live.snapshot_interval = Duration::ZERO;
    runner.apply_live(live);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let paused = periodic_count(&runner).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(periodic_count(&runner).await, paused, "paused at zero");
    let mut live = runner.settings().current();
    live.snapshot_interval = Duration::from_millis(30);
    runner.apply_live(live);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(periodic_count(&runner).await > paused, "resumed");

    // Stopping the schedule stops the captures.
    schedule.abort();
    let _ = schedule.await;
    let stopped = periodic_count(&runner).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(periodic_count(&runner).await, stopped);
}
