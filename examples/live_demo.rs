//! Serves the API over a scripted, deliberately slow model, so the console's live trace can be
//! watched without a model relay or a deployment:
//!
//! ```bash
//! cargo run --example live_demo -- data-live      # API on http://127.0.0.1:4720
//! cd web && pnpm dev                              # console on http://localhost:5180
//! # or, beside a running `serve`: `-- data-live 127.0.0.1:4721` and
//! # `BROCCOLI_API=http://127.0.0.1:4721 pnpm dev --port 5181`
//! ```
//!
//! Every report filed from the console (or with `report`) runs the same scripted
//! investigation: read the View, report progress, propose a queue purge (held for approval in
//! Rehearsal mode), and conclude — one model turn every two seconds, so the transcript can be
//! seen growing entry by entry on the Trace page, and the Export button then saves the whole
//! session as one JSON file.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use broccoli_agent_harness::testing::{call, text};
use broccoli_agent_harness::{
    AssistantItem, HarnessResult, ModelClient, ModelRequest, ModelTurn, Usage,
};
use broccoli_devops_agent::api::{ApiState, serve};
use broccoli_devops_agent::config::{AppConfig, DataConfig, TopologyConfig};
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::settings::LiveSettings;
use broccoli_devops_agent::topology::DeploymentTopology;
use serde_json::json;
use tokio::sync::Mutex;

/// Plays one fixed investigation over and over, one turn per request, slowly.
struct CyclingModelClient {
    turns: Vec<Vec<AssistantItem>>,
    position: Mutex<usize>,
    delay: Duration,
}

#[async_trait]
impl ModelClient for CyclingModelClient {
    async fn complete(&self, _request: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        tokio::time::sleep(self.delay).await;
        let mut position = self.position.lock().await;
        let items = self.turns[*position % self.turns.len()].clone();
        *position += 1;
        Ok(ModelTurn::with_usage(
            items,
            Usage::reported(9_800, 6_000, 420),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "data-live".into()),
    );
    std::fs::create_dir_all(&data_dir)?;

    let closed = || {
        TcpListener::bind("127.0.0.1:0")
            .map(|l| l.local_addr().unwrap().port())
            .unwrap()
    };
    let topology_text = format!(
        r#"
[deployment]
id = "0198c936-5f2a-7000-8000-4a6f8c2d9dd1"
name = "live-demo"
topology_revision = "demo-1"
operation_mode = "rehearsal"

[[resources]]
id = "redis-mq"
kind = "redis"
node = "infra-1"
probes = [{{ probe = "tcp.connect", target = "127.0.0.1:{}" }}]

[[resources]]
id = "worker-1"
kind = "worker"
node = "judge-1"
probes = [{{ probe = "tcp.connect", target = "127.0.0.1:{}" }}]
"#,
        closed(),
        closed()
    );
    let topology_path = data_dir.join("topology.toml");
    std::fs::write(&topology_path, topology_text.trim_start())?;
    let topology = DeploymentTopology::load(&topology_path)?;

    let client = CyclingModelClient {
        turns: vec![
            vec![call("c1", "read_snapshot_view", json!({}))],
            vec![
                text("redis-mq is down and worker-1 with it; the queue cannot drain."),
                call(
                    "c2",
                    "report_progress",
                    json!({ "summary": "Correlating the Redis outage with the idle worker" }),
                ),
            ],
            vec![call(
                "c3",
                "propose_action",
                json!({
                    "runbook_id": "mq.purge",
                    "target_ids": ["redis-mq"],
                    "reason": "The queue holds stale entries from before the outage",
                    "expected_effect": "Pending stale work is dropped and judging resumes",
                }),
            )],
            vec![call(
                "c4",
                "submit_diagnosis",
                json!({
                    "summary": "redis-mq is unreachable, so worker-1 idles; purge the stale queue once Redis is back",
                    "outcome": "diagnosis_only",
                    "unresolved_questions": ["Is the Redis host itself reachable?"],
                }),
            )],
        ],
        position: Mutex::new(0),
        delay: Duration::from_secs(2),
    };
    let platform = PlatformConfig {
        dry_run: true,
        runbooks: vec![RunbookCommand {
            id: "mq.purge".into(),
            command: "echo purge {target}".into(),
        }],
        ..PlatformConfig::default()
    };
    // The config the Settings page shows is the one the runner was wired from, so what the
    // page edits is what the Platform executes.
    let config = AppConfig {
        data: DataConfig {
            dir: data_dir.clone(),
        },
        topology: TopologyConfig {
            path: topology_path.clone(),
        },
        platform: platform.clone(),
        ..AppConfig::default()
    };
    let runner = Arc::new(
        SliceRunner::wire(
            topology,
            &data_dir,
            TeamBackend::Harness {
                client: Arc::new(client),
                budget: Default::default(),
                model: "scripted-live-model".into(),
                label: "scripted live demo".into(),
            },
            platform,
        )?
        .with_live_settings(LiveSettings::from_config(&config)),
    );
    let summary = runner.recover().await?;
    if summary.is_clean_restart() {
        runner.scheduler().resume().await?;
    }
    let _periodic = runner.spawn_periodic_capture();

    // A second argument picks the bind address, so the demo can sit beside a real `serve`.
    let bind = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:4720".into());
    println!(
        "live demo: API on http://{bind} · data in {} · file a report from the console and open its Trace",
        data_dir.display()
    );
    // The Settings page writes into the demo's own directory, never into config/agent.toml.
    let state = Arc::new(
        ApiState::new(runner, config)
            .with_recovery(summary)
            .with_config_path(data_dir.join("agent.toml")),
    );
    serve(state, &bind).await
}
