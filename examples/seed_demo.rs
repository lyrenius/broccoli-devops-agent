//! Seeds a demo data directory with every inbox category, using a scripted model instead of a
//! relay, so the consoles can be tried without a model or a deployment:
//!
//! ```bash
//! cargo run --example seed_demo -- data-demo
//! cargo run -- serve --data data-demo --topology data-demo/topology.toml --team readonly
//! ```
//!
//! The seeded state holds one permission request (a queue purge), one rule denial (an
//! operation-mode change no Team may hold), one failed action (a worker start the Platform has no
//! command for), and one failed Job (a model that answered in prose). Reviewing an item in the
//! console with "send back upstream" then runs the deterministic Team as the revision, so the
//! whole loop is visible.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use broccoli_agent_harness::testing::{ScriptedModelClient, call, text};
use broccoli_devops_agent::domain::HumanReport;
use broccoli_devops_agent::platform::{PlatformConfig, RunbookCommand};
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::topology::DeploymentTopology;
use serde_json::json;

fn propose(
    id: &str,
    runbook: &str,
    target: &str,
    reason: &str,
    effect: &str,
) -> broccoli_agent_harness::AssistantItem {
    call(
        id,
        "propose_action",
        json!({
            "runbook_id": runbook,
            "target_ids": [target],
            "reason": reason,
            "expected_effect": effect,
        }),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "data-demo".into()),
    );
    std::fs::create_dir_all(&data_dir)?;

    // A tiny topology on closed local ports: everything probes as Down.
    let closed = || {
        TcpListener::bind("127.0.0.1:0")
            .map(|l| l.local_addr().unwrap().port())
            .unwrap()
    };
    let topology_text = format!(
        r#"
[deployment]
id = "0198c936-5f2a-7000-8000-4a6f8c2d9dd0"
name = "demo"
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

[[resources]]
id = "broccoli-server"
kind = "broccoli_server"
node = "app-1"
probes = [{{ probe = "http.status", url = "http://127.0.0.1:{}/healthz" }}]
"#,
        closed(),
        closed(),
        closed()
    );
    let topology_path = data_dir.join("topology.toml");
    std::fs::write(&topology_path, topology_text.trim_start())?;
    let topology = DeploymentTopology::load(&topology_path)?;

    let client = ScriptedModelClient::new(vec![
        // Report 1: three proposals with three different fates.
        vec![call("c1", "read_snapshot_view", json!({}))],
        vec![
            propose(
                "c2",
                "mq.purge",
                "redis-mq",
                "The queue holds stale entries from before the outage",
                "Pending stale work is dropped and judging resumes",
            ),
            propose(
                "c3",
                "mode.set",
                "broccoli-server",
                "Switch to post-contest mode to unblock maintenance",
                "Maintenance operations become permitted",
            ),
            propose(
                "c4",
                "worker.start",
                "worker-1",
                "A second worker slot is configured but idle",
                "worker-1 starts and drains the queue",
            ),
        ],
        vec![call(
            "c5",
            "submit_diagnosis",
            json!({
                "summary": "redis-mq and worker-1 are down; broccoli-server cannot reach its queue",
                "unresolved_questions": ["Is the judge host itself reachable?"],
            }),
        )],
        // Report 2: prose instead of the terminal tool — a failed Job.
        vec![text("The printer looks fine to me, nothing to do.")],
    ]);
    let platform = PlatformConfig {
        dry_run: true,
        runbooks: vec![
            RunbookCommand {
                id: "mq.purge".into(),
                command: "echo purge {target}".into(),
            },
            RunbookCommand {
                id: "worker.restart".into(),
                command: "echo restart {target}".into(),
            },
        ],
        ..PlatformConfig::default()
    };
    let runner = SliceRunner::wire(
        topology,
        &data_dir,
        TeamBackend::Harness {
            client: Arc::new(client),
            budget: Default::default(),
            label: "scripted demo".into(),
        },
        platform,
    )?;

    let (_issue, job) = runner
        .handle_report(HumanReport::new(
            "alice",
            "Submissions are not being judged",
            "The queue has been growing since 10:12 and no verdicts come back",
        ))
        .await?;
    let actions = runner.run_proposals(&job).await?;
    let (_issue, _job) = runner
        .handle_report(HumanReport::new(
            "bob",
            "Balloon printer offline",
            "The station in hall B stopped printing",
        ))
        .await?;

    let inbox = runner.inbox().await?;
    println!("seeded {}", data_dir.display());
    println!("  actions: {}", actions.len());
    print!("{}", SliceRunner::render_inbox(&inbox));
    println!(
        "\nserve it with:\n  cargo run -- serve --data {0} --topology {0}/topology.toml --team readonly",
        data_dir.display()
    );
    Ok(())
}
