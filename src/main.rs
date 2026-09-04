//! Operator CLI for the Broccoli DevOps Agent.
//!
//! Read-only commands cover the v0.1 slice: `snapshot` captures and displays system state,
//! `report` runs a human report through Issue, Job, and Team to a persisted result, `recover`
//! rebuilds control state after a restart, and `events` prints the append-only log. `config show`
//! exposes the effective configuration (key redacted) and `check-model` verifies the relay. No
//! command mutates a machine; ActionRun execution stays out until the authority matrix is approved.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use broccoli_agent_harness::{Item, ToolRegistry, Trust, cancel_pair, run_agent};
use clap::{Parser, Subcommand, ValueEnum};

use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{HumanReport, IssuePriority, SnapshotCause};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::TopScheduler;
use broccoli_devops_agent::store::file::FileStateStore;
use broccoli_devops_agent::topology::DeploymentTopology;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(name = "broccoli-devops-agent", version, about)]
struct Cli {
    /// Agent configuration file; defaults apply when it does not exist.
    #[arg(long, global = true, default_value = "config/agent.toml")]
    config: PathBuf,
    /// Override the topology path from the config file.
    #[arg(long, global = true)]
    topology: Option<PathBuf>,
    /// Override the data directory from the config file.
    #[arg(long, global = true)]
    data: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// Which Operate Team backend `report` should dispatch to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TeamChoice {
    /// The model-backed harness Team when `[model]` is configured, otherwise deterministic.
    Auto,
    /// Deterministic read-only diagnosis; never contacts a model.
    Readonly,
    /// Model-backed diagnosis through the agent harness; requires `[model]` and the API key.
    Harness,
}

/// The operator commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Capture a Snapshot of the deployment and display it with its coverage gaps.
    Snapshot,
    /// File a human report; dispatches an Operate Job and prints its diagnosis.
    Report {
        /// Short title for the problem.
        #[arg(long)]
        title: String,
        /// Symptoms and context as observed.
        #[arg(long)]
        description: String,
        /// Reporter identity recorded in the event log.
        #[arg(long, default_value = "operator")]
        reporter: String,
        /// Priority override; omitting it files at the human-reserved top priority.
        #[arg(long, value_parser = parse_priority)]
        priority: Option<IssuePriority>,
        /// Team backend to dispatch to.
        #[arg(long, value_enum, default_value_t = TeamChoice::Auto)]
        team: TeamChoice,
    },
    /// Rebuild control state from the data directory and list unfinished work.
    Recover,
    /// Print the append-only event log.
    Events {
        /// Show only the last N events.
        #[arg(long)]
        tail: Option<usize>,
    },
    /// Show the effective configuration as JSON (the API key is never included).
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Send one trivial request to the configured model relay and report the result.
    CheckModel,
}

/// Configuration subcommands.
#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the effective configuration.
    Show,
}

/// Parses the operator-facing priority names.
fn parse_priority(text: &str) -> Result<IssuePriority, String> {
    match text {
        "low" => Ok(IssuePriority::Low),
        "normal" => Ok(IssuePriority::Normal),
        "high" => Ok(IssuePriority::High),
        "critical" => Ok(IssuePriority::Critical),
        "top" => Ok(IssuePriority::HumanTop),
        other => Err(format!(
            "unknown priority `{other}`; use low, normal, high, critical, or top"
        )),
    }
}

/// Resolves the effective config with CLI overrides applied.
fn effective_config(cli: &Cli) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let mut config = AppConfig::load_or_default(&cli.config)?;
    if let Some(topology) = &cli.topology {
        config.topology.path = topology.clone();
    }
    if let Some(data) = &cli.data {
        config.data.dir = data.clone();
    }
    Ok(config)
}

/// Chooses and builds the Team backend for a report run.
fn select_backend(
    config: &AppConfig,
    choice: TeamChoice,
) -> Result<TeamBackend, Box<dyn std::error::Error>> {
    let wants_model = match choice {
        TeamChoice::Readonly => false,
        TeamChoice::Harness => true,
        TeamChoice::Auto => config.model.is_some(),
    };
    if !wants_model {
        return Ok(TeamBackend::ReadOnly);
    }
    let model = config.model.as_ref().ok_or(
        "`--team harness` needs a [model] section in the agent config (see config/agent.example.toml)",
    )?;
    let client = model.build_client()?;
    Ok(TeamBackend::Harness {
        label: format!("{} via {}", client.model(), client.base_url()),
        client: Arc::new(client),
        budget: model.harness_budget(),
    })
}

/// Runs one CLI command and reports failures as readable errors.
async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = effective_config(&cli)?;
    match cli.command {
        Command::Snapshot => {
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let runner = SliceRunner::wire(topology, &config.data.dir, TeamBackend::ReadOnly)?;
            let snapshot = runner.capture(SnapshotCause::Manual).await?;
            print!("{}", runner.render_snapshot(&snapshot));
        }
        Command::Report {
            title,
            description,
            reporter,
            priority,
            team,
        } => {
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let backend = select_backend(&config, team)?;
            let runner = SliceRunner::wire(topology, &config.data.dir, backend)?;
            println!("team backend: {}\n", runner.team_label());
            let mut report = HumanReport::new(reporter, title, description);
            report.priority = priority;
            let (issue, job) = runner.handle_report(report).await?;
            print!("{}", SliceRunner::render_report_outcome(&issue, &job));
        }
        Command::Recover => {
            // Recovery needs only the store; a missing topology file must not block it.
            let store = Arc::new(FileStateStore::open(&config.data.dir)?);
            let scheduler = TopScheduler::new(store);
            let summary = scheduler.recover().await?;
            println!(
                "recovered control state · mode {:?}",
                scheduler.mode().await
            );
            println!("unfinished issues:  {}", summary.issue_ids.len());
            for id in &summary.issue_ids {
                println!("  {id}");
            }
            println!("unfinished jobs:    {}", summary.job_ids.len());
            for id in &summary.job_ids {
                println!("  {id}");
            }
            println!("unfinished actions: {}", summary.action_run_ids.len());
            for id in &summary.action_run_ids {
                println!("  {id}");
            }
        }
        Command::Events { tail } => {
            let store = FileStateStore::open(&config.data.dir)?;
            let events = store.list_events().await?;
            let skip = tail.map_or(0, |n| events.len().saturating_sub(n));
            for event in events.into_iter().skip(skip) {
                println!(
                    "{:>5}  {}  {:<32} {:<14} {}",
                    event.sequence,
                    event.occurred_at.format("%H:%M:%S"),
                    event.kind,
                    event.actor,
                    event.summary
                );
            }
        }
        Command::Config {
            action: ConfigAction::Show,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&config.effective_json()?)?
            );
        }
        Command::CheckModel => {
            let model = config
                .model
                .as_ref()
                .ok_or("no [model] section in the agent config (see config/agent.example.toml)")?;
            let client = model.build_client()?;
            println!(
                "contacting {} (model {}, {:?} wire format)…",
                client.base_url(),
                client.model(),
                model.wire_api
            );
            let (_handle, token) = cancel_pair();
            let started = std::time::Instant::now();
            let report = run_agent(
                &client,
                &ToolRegistry::new(),
                &model.harness_budget(),
                "You are a connectivity check. Reply with exactly the word OK and nothing else.",
                vec![Item::UserInput {
                    text: "ping".into(),
                    trust: Trust::Trusted,
                }],
                token,
            )
            .await?;
            println!(
                "reply after {:.1}s: {:?}",
                started.elapsed().as_secs_f64(),
                report.outcome
            );
        }
    }
    Ok(())
}

/// CLI entry point; prints one readable error line on failure.
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
