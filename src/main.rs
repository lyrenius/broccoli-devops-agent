//! Operator CLI for the Broccoli DevOps Agent.
//!
//! Read-only commands cover the v0.1 slice: `snapshot` captures and displays system state,
//! `report` runs a human report through Issue, Job, and Team to a persisted result, `recover`
//! rebuilds control state after a restart, and `events` prints the append-only log. `config show`
//! exposes the effective configuration (key redacted) and `check-model` verifies the relay.
//! `report` also runs the Job's proposed actions through the authority matrix; `inbox` shows what
//! waits for a human, `actions` approves or rejects held actions, and `review` acknowledges a
//! denied or failed item or sends it back upstream with feedback. The Platform is in dry-run mode
//! until the operator opts in.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use broccoli_agent_harness::{Item, ToolRegistry, Trust, cancel_pair, run_agent};
use clap::{Parser, Subcommand, ValueEnum};

use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{HumanReport, IssuePriority, SnapshotCause};
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{InboxDecision, SliceRunner, TeamBackend};
use broccoli_devops_agent::scheduler::{
    IssueClosure, RecoverySummary, SchedulerMode, TopScheduler,
};
use broccoli_devops_agent::store::file::FileStateStore;
use broccoli_devops_agent::topology::DeploymentTopology;
use uuid::Uuid;

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
    /// Rebuild control state after a restart: reconcile interrupted work, restore the freeze mode.
    Recover,
    /// Close an Issue by hand: resolved, cancelled, or failed.
    Issues {
        #[command(subcommand)]
        action: IssuesAction,
    },
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
    /// Show everything waiting for a human: permission requests, denials, and failures.
    Inbox,
    /// List, approve, or reject ActionRuns held by the authority matrix.
    Actions {
        #[command(subcommand)]
        action: ActionsAction,
    },
    /// Review a denied or failed inbox item: acknowledge it, or send it back upstream.
    Review {
        #[command(subcommand)]
        item: ReviewItem,
    },
    /// Serve the HTTP + SSE API for the web console and the terminal UI.
    ///
    /// Startup runs recovery first. After a clean restart (the previous process was running and
    /// nothing was interrupted) dispatch resumes automatically; otherwise the Scheduler stays
    /// frozen until a human resumes it from a console.
    Serve {
        /// Override the bind address from the config file.
        #[arg(long)]
        bind: Option<String>,
        /// Team backend for reports filed through the API.
        #[arg(long, value_enum, default_value_t = TeamChoice::Auto)]
        team: TeamChoice,
        /// Stay frozen after recovery even when the restart was clean.
        #[arg(long)]
        stay_frozen: bool,
    },
}

/// ActionRun subcommands.
#[derive(Debug, Subcommand)]
enum ActionsAction {
    /// List every ActionRun with its status and approval state.
    List,
    /// Approve a waiting ActionRun; it executes and is verified immediately.
    Approve {
        /// ActionRun ID from `actions list`.
        id: Uuid,
        /// Your name, recorded with the approval.
        #[arg(long = "as", default_value = "operator")]
        by: String,
    },
    /// Reject a waiting ActionRun; it is cancelled and lands in the Permission Denied inbox.
    Reject {
        /// ActionRun ID from `actions list`.
        id: Uuid,
        /// Why you rejected it; travels upstream if the denial is later sent back.
        #[arg(long)]
        comment: Option<String>,
        /// Your name, recorded with the rejection.
        #[arg(long = "as", default_value = "operator")]
        by: String,
    },
}

/// What kind of inbox item a review targets.
#[derive(Debug, Subcommand)]
enum ReviewItem {
    /// A denied or failed ActionRun.
    Action {
        /// ActionRun ID from `inbox`.
        id: Uuid,
        #[command(flatten)]
        decision: ReviewArgs,
    },
    /// A failed Job.
    Job {
        /// Job ID from `inbox`.
        id: Uuid,
        #[command(flatten)]
        decision: ReviewArgs,
    },
}

/// The human's decision on an inbox item.
#[derive(Debug, clap::Args)]
struct ReviewArgs {
    /// Take note and stop; nothing further happens automatically.
    #[arg(long, conflicts_with = "upstream")]
    acknowledge: bool,
    /// Send the reason and your comment back upstream: a revising Job runs now.
    #[arg(long, conflicts_with = "acknowledge")]
    upstream: bool,
    /// Your feedback for the next pass.
    #[arg(long)]
    comment: Option<String>,
    /// Your name, recorded with the review.
    #[arg(long = "as", default_value = "operator")]
    by: String,
    /// Team backend for a revising Job.
    #[arg(long, value_enum, default_value_t = TeamChoice::Auto)]
    team: TeamChoice,
}

impl ReviewArgs {
    fn decision(&self) -> Result<InboxDecision, Box<dyn std::error::Error>> {
        match (self.acknowledge, self.upstream) {
            (true, false) => Ok(InboxDecision::Acknowledge),
            (false, true) => Ok(InboxDecision::SendUpstream),
            _ => Err("choose exactly one of --acknowledge or --upstream".into()),
        }
    }
}

/// Configuration subcommands.
#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the effective configuration.
    Show,
}

/// Issue subcommands.
#[derive(Debug, Subcommand)]
enum IssuesAction {
    /// Close an Issue.
    Close {
        /// Issue ID.
        id: Uuid,
        /// The problem is fixed or was not a problem.
        #[arg(long, conflicts_with_all = ["cancelled", "failed"])]
        resolved: bool,
        /// Stop working on it without claiming it is fixed.
        #[arg(long, conflicts_with_all = ["resolved", "failed"])]
        cancelled: bool,
        /// Give up: the problem stands.
        #[arg(long, conflicts_with_all = ["resolved", "cancelled"])]
        failed: bool,
        /// Why.
        #[arg(long)]
        comment: Option<String>,
        /// Your name, recorded with the closure.
        #[arg(long = "as", default_value = "operator")]
        by: String,
    },
}

/// Prints a recovery summary in operator-facing lines.
fn print_recovery(summary: &RecoverySummary) {
    println!(
        "recovered control state · previous mode {:?} · now {:?}",
        summary.previous_mode, summary.final_mode
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
    if summary.touched_anything() {
        println!(
            "reconciled: {} interrupted job(s) failed, {} interrupted action(s) failed or denied, \
             {} action(s) verified now, {} review(s) reconstructed — see the inbox",
            summary.interrupted_job_ids.len(),
            summary.interrupted_action_ids.len(),
            summary.verified_action_ids.len(),
            summary.reconstructed_review_job_ids.len()
        );
    }
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
            let runner = SliceRunner::wire(
                topology,
                &config.data.dir,
                TeamBackend::ReadOnly,
                config.platform.clone(),
            )?;
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
            let runner =
                SliceRunner::wire(topology, &config.data.dir, backend, config.platform.clone())?;
            println!("team backend: {}\n", runner.team_label());
            let mut report = HumanReport::new(reporter, title, description);
            report.priority = priority;
            let (issue, job) = runner.handle_report(report).await?;
            print!("{}", SliceRunner::render_report_outcome(&issue, &job));
            let actions = runner.run_proposals(&job).await?;
            if !actions.is_empty() {
                println!("\nactions:");
                print!(
                    "{}",
                    SliceRunner::render_actions(&actions, runner.dry_run())
                );
            }
        }
        Command::Recover => {
            // With a topology, interrupted actions are verified against a fresh Snapshot; a
            // missing topology file must not block recovery, so fall back to the store alone.
            let summary = match DeploymentTopology::load(&config.topology.path) {
                Ok(topology) => {
                    let runner = SliceRunner::wire(
                        topology,
                        &config.data.dir,
                        TeamBackend::ReadOnly,
                        config.platform.clone(),
                    )?;
                    runner.recover().await?
                }
                Err(error) => {
                    eprintln!(
                        "note: topology not loaded ({error}); recovering from the store alone"
                    );
                    let store = Arc::new(FileStateStore::open(&config.data.dir)?);
                    TopScheduler::new(store).recover().await?
                }
            };
            print_recovery(&summary);
        }
        Command::Issues {
            action:
                IssuesAction::Close {
                    id,
                    resolved,
                    cancelled,
                    failed,
                    comment,
                    by,
                },
        } => {
            let closure = match (resolved, cancelled, failed) {
                (true, false, false) => IssueClosure::Resolved,
                (false, true, false) => IssueClosure::Cancelled,
                (false, false, true) => IssueClosure::Failed,
                _ => {
                    return Err("choose exactly one of --resolved, --cancelled, or --failed".into());
                }
            };
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let runner = SliceRunner::wire(
                topology,
                &config.data.dir,
                TeamBackend::ReadOnly,
                config.platform.clone(),
            )?;
            let issue = runner.close_issue(id, closure, &by, comment).await?;
            println!("Issue {} · status {:?}", issue.issue_id, issue.status);
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
        Command::Actions { action } => {
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let runner = SliceRunner::wire(
                topology,
                &config.data.dir,
                TeamBackend::ReadOnly,
                config.platform.clone(),
            )?;
            match action {
                ActionsAction::List => {
                    let actions = runner.list_actions().await?;
                    print!(
                        "{}",
                        SliceRunner::render_actions(&actions, runner.dry_run())
                    );
                }
                ActionsAction::Approve { id, by } => {
                    let action = runner.approve_action(id, &by).await?;
                    print!(
                        "{}",
                        SliceRunner::render_actions(&[action], runner.dry_run())
                    );
                }
                ActionsAction::Reject { id, comment, by } => {
                    let action = runner.reject_action(id, &by, comment).await?;
                    print!(
                        "{}",
                        SliceRunner::render_actions(&[action], runner.dry_run())
                    );
                }
            }
        }
        Command::Inbox => {
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let runner = SliceRunner::wire(
                topology,
                &config.data.dir,
                TeamBackend::ReadOnly,
                config.platform.clone(),
            )?;
            print!("{}", SliceRunner::render_inbox(&runner.inbox().await?));
        }
        Command::Review { item } => {
            let (args, is_job, id) = match &item {
                ReviewItem::Action { id, decision } => (decision, false, *id),
                ReviewItem::Job { id, decision } => (decision, true, *id),
            };
            let decision = args.decision()?;
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let backend = select_backend(&config, args.team)?;
            let runner =
                SliceRunner::wire(topology, &config.data.dir, backend, config.platform.clone())?;
            let revision = if is_job {
                let outcome = runner
                    .review_job(id, &args.by, decision, args.comment.clone())
                    .await?;
                println!("reviewed job {} · {:?}", id, outcome.reviewed.review);
                outcome.revision
            } else {
                let outcome = runner
                    .review_action(id, &args.by, decision, args.comment.clone())
                    .await?;
                println!("reviewed action {} · {:?}", id, outcome.reviewed.review);
                outcome.revision
            };
            if let Some(revision) = revision {
                let issue = runner.store().get_issue(revision.job.issue_id).await?;
                println!("\nrevision · team backend: {}\n", runner.team_label());
                print!(
                    "{}",
                    SliceRunner::render_report_outcome(&issue, &revision.job)
                );
                if !revision.actions.is_empty() {
                    println!("\nactions:");
                    print!(
                        "{}",
                        SliceRunner::render_actions(&revision.actions, runner.dry_run())
                    );
                }
            }
        }
        Command::Serve {
            bind,
            team,
            stay_frozen,
        } => {
            let topology = DeploymentTopology::load(&config.topology.path)?;
            let backend = select_backend(&config, team)?;
            let runner = Arc::new(SliceRunner::wire(
                topology,
                &config.data.dir,
                backend,
                config.platform.clone(),
            )?);

            // Recovery before serving: reconcile what a previous process left behind and
            // restore its freeze state. A clean restart resumes on its own; anything else
            // waits for a human, who can see why in the console.
            let summary = runner.recover().await?;
            print_recovery(&summary);
            let clean_restart =
                summary.previous_mode == SchedulerMode::Running && !summary.touched_anything();
            if clean_restart && !stay_frozen {
                runner.scheduler().resume().await?;
                println!("clean restart: dispatch resumed");
            } else {
                println!(
                    "scheduler stays {:?}: resume from a console when the inbox has been checked",
                    runner.scheduler().mode().await
                );
            }

            let bind = bind.unwrap_or_else(|| config.api.bind.clone());
            println!(
                "serving API on http://{bind} · team backend: {} · dry-run: {}",
                runner.team_label(),
                runner.dry_run()
            );
            if config.api.token.is_empty()
                && !bind.starts_with("127.0.0.1")
                && !bind.starts_with("localhost")
            {
                eprintln!("warning: binding beyond localhost without an API token");
            }
            let state = Arc::new(
                broccoli_devops_agent::api::ApiState::new(runner, config).with_recovery(summary),
            );
            broccoli_devops_agent::api::serve(state, &bind).await?;
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
