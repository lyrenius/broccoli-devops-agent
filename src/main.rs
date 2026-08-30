//! Operator CLI for the Broccoli DevOps Agent v0.1 vertical slice.
//!
//! Four read-only commands cover the slice: `snapshot` captures and displays system state,
//! `report` runs a human report through Issue, Job, and Team to a persisted result, `recover`
//! rebuilds control state after a restart, and `events` prints the append-only log. No command
//! mutates a machine; ActionRun execution stays out of v0.1 by design.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use broccoli_devops_agent::domain::{HumanReport, IssuePriority, SnapshotCause};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::SliceRunner;
use broccoli_devops_agent::scheduler::TopScheduler;
use broccoli_devops_agent::store::file::FileStateStore;
use broccoli_devops_agent::topology::DeploymentTopology;

/// Command-line arguments for the v0.1 slice.
#[derive(Debug, Parser)]
#[command(name = "broccoli-devops-agent", version, about)]
struct Cli {
    /// Path to the deployment topology TOML file.
    #[arg(long, global = true, default_value = "config/topology.toml")]
    topology: PathBuf,
    /// Data directory for the file-backed store and artifact bodies.
    #[arg(long, global = true, default_value = "data")]
    data: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// The v0.1 operator commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Capture a Snapshot of the deployment and display it with its coverage gaps.
    Snapshot,
    /// File a human report; dispatches a read-only Operate Job and prints its diagnosis.
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
    },
    /// Rebuild control state from the data directory and list unfinished work.
    Recover,
    /// Print the append-only event log.
    Events {
        /// Show only the last N events.
        #[arg(long)]
        tail: Option<usize>,
    },
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

/// Runs one CLI command and reports failures as readable errors.
async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Snapshot => {
            let topology = DeploymentTopology::load(&cli.topology)?;
            let runner = SliceRunner::wire(topology, &cli.data)?;
            let snapshot = runner.capture(SnapshotCause::Manual).await?;
            print!("{}", runner.render_snapshot(&snapshot));
        }
        Command::Report {
            title,
            description,
            reporter,
            priority,
        } => {
            let topology = DeploymentTopology::load(&cli.topology)?;
            let runner = SliceRunner::wire(topology, &cli.data)?;
            let mut report = HumanReport::new(reporter, title, description);
            report.priority = priority;
            let (issue, job) = runner.handle_report(report).await?;
            print!("{}", SliceRunner::render_report_outcome(&issue, &job));
        }
        Command::Recover => {
            // Recovery needs only the store; a missing topology file must not block it.
            let store = std::sync::Arc::new(FileStateStore::open(&cli.data)?);
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
            let store = FileStateStore::open(&cli.data)?;
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
