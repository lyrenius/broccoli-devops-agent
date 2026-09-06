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
use broccoli_devops_agent::scheduler::{IssueClosure, RecoverySummary, TopScheduler};
use broccoli_devops_agent::settings::LiveSettings;
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
    /// Save an Issue with its whole pass chain to a JSON file, or load one as a read-only archive.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
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
    /// Show what the model relay has been asked to do, and what it cost.
    Usage,
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

/// Session subcommands.
#[derive(Debug, Subcommand)]
enum SessionsAction {
    /// Write the session file for an Issue: the Issue, every pass with its transcript and the
    /// View it read, every action, every Snapshot, and every event.
    Export {
        /// Issue ID.
        id: Uuid,
        /// Where to write; defaults to `session-<id>.json` in the current directory.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Your name, recorded in the file as the exporter.
        #[arg(long = "as", default_value = "operator")]
        by: String,
    },
    /// Load a session file as a read-only archive: viewable in the consoles, ignored by every
    /// control decision.
    Import {
        /// The session file.
        path: PathBuf,
        /// Your name, recorded with the import.
        #[arg(long = "as", default_value = "operator")]
        by: String,
    },
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
    // The output language is fixed for the life of the process, from the config file.
    broccoli_devops_agent::i18n::set_language(config.agent.language);
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
        model: client.model().to_string(),
        client: Arc::new(client),
        budget: model.harness_budget(),
    })
}

/// Wires the control plane over the topology named by the config.
///
/// Every command goes through here so the relay's price list and the spend ceiling are attached
/// once instead of at eight call sites, where one omission would silently disable the budget.
fn wire_runner(
    config: &AppConfig,
    backend: TeamBackend,
) -> Result<SliceRunner, Box<dyn std::error::Error>> {
    let topology = DeploymentTopology::load(&config.topology.path)?;
    Ok(SliceRunner::wire_with(
        topology,
        &config.data.dir,
        backend,
        config.platform.clone(),
        config.pass_policy(),
    )?
    .with_spend(
        config
            .model
            .as_ref()
            .and_then(|model| model.pricing.clone()),
        config.budget.clone(),
    )
    .with_live_settings(LiveSettings::from_config(config)))
}

/// Prints Team progress to stderr as it is delivered, leaving stdout for the result.
fn spawn_progress_printer(runner: &Arc<SliceRunner>) -> tokio::task::JoinHandle<()> {
    let mut progress = runner.watch_progress();
    tokio::spawn(async move {
        while let Ok(callback) = progress.recv().await {
            // Forwarded transcript entries feed the console's live trace; here the progress
            // lines already say what the pass is doing.
            if callback.step.is_some() {
                continue;
            }
            // Interim callbacks only: the final result is printed properly by the caller.
            if callback.final_result.is_none() {
                eprintln!("  · {}", callback.summary);
            }
        }
    })
}

/// Turns Ctrl-C into a cooperative interruption of whatever pass is running.
///
/// The first press asks the Team to stop, which still produces a final callback, a transcript,
/// and an inbox item a human can act on. A second press is taken as "stop arguing" and exits.
fn spawn_interrupt_handler(runner: &Arc<SliceRunner>) -> tokio::task::JoinHandle<()> {
    let runner = runner.clone();
    tokio::spawn(async move {
        let mut pressed = 0_u32;
        while tokio::signal::ctrl_c().await.is_ok() {
            pressed += 1;
            let stopped = runner
                .cancel_all_passes("operator (Ctrl-C)")
                .await
                .unwrap_or(0);
            if stopped == 0 || pressed > 1 {
                eprintln!("interrupted");
                std::process::exit(130);
            }
            eprintln!(
                "interrupting {stopped} running pass(es) — the transcript is kept and the Job \
                 lands in the Failed inbox; press Ctrl-C again to exit now"
            );
        }
    })
}

/// Runs one CLI command and reports failures as readable errors.
async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = effective_config(&cli)?;
    match cli.command {
        Command::Snapshot => {
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
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
            let runner = Arc::new(wire_runner(&config, select_backend(&config, team)?)?);
            println!("team backend: {}\n", runner.team_label());
            // A model-backed report blocks for minutes. Progress goes to stderr as it happens and
            // Ctrl-C interrupts the pass cooperatively, so the transcript and the partial record
            // survive instead of being lost with the process.
            let printer = spawn_progress_printer(&runner);
            let interrupt = spawn_interrupt_handler(&runner);
            let mut report = HumanReport::new(reporter, title, description);
            report.priority = priority;
            let (issue, job) = runner.handle_report(report).await?;
            print!("{}", SliceRunner::render_report_outcome(&issue, &job));
            let passes = runner.drive_passes(job).await?;
            printer.abort();
            interrupt.abort();
            print!("{}", SliceRunner::render_passes(&passes, runner.dry_run()));
            let issue = runner.store().get_issue(issue.issue_id).await?;
            println!("\nIssue {} · status {:?}", issue.issue_id, issue.status);
            let totals = runner.usage_totals().await?;
            if totals.passes > 0 {
                println!("model usage: {}", totals.one_line());
            }
        }
        Command::Usage => {
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
            let totals = runner.usage_totals().await?;
            println!("{}", totals.one_line());
            for model in &totals.by_model {
                let cost = match (model.cost, totals.currency.as_deref()) {
                    (Some(cost), Some(currency)) => format!(" · {cost:.4} {currency}"),
                    _ => String::new(),
                };
                println!(
                    "  {:<28} {:>3} pass(es)  {:>9} in ({} cached) {:>9} out{}",
                    model.model,
                    model.passes,
                    model.input_tokens,
                    model.cached_input_tokens,
                    model.output_tokens,
                    cost
                );
            }
            if totals.cost.is_none() {
                println!(
                    "no [model.pricing] in the agent config, so tokens are counted but not priced"
                );
            }
            if let Some(budget) = &totals.budget
                && let Some(reason) = &budget.reason
            {
                println!("budget: {reason} — dispatch is frozen until it is raised");
            }
        }
        Command::Recover => {
            // With a topology, interrupted actions are verified against a fresh Snapshot; a
            // missing topology file must not block recovery, so fall back to the store alone.
            let summary = match DeploymentTopology::load(&config.topology.path) {
                Ok(_) => {
                    wire_runner(&config, TeamBackend::ReadOnly)?
                        .recover()
                        .await?
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
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
            let issue = runner.close_issue(id, closure, &by, comment).await?;
            println!("Issue {} · status {:?}", issue.issue_id, issue.status);
        }
        Command::Sessions {
            action: SessionsAction::Export { id, output, by },
        } => {
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
            let bundle = runner.export_session(id, &by).await?;
            let path = output.unwrap_or_else(|| PathBuf::from(format!("session-{id}.json")));
            std::fs::write(&path, serde_json::to_vec_pretty(&bundle)?)?;
            println!(
                "wrote {} · Issue {} · {} pass(es), {} action(s), {} snapshot(s), {} artifact(s), {} event(s)",
                path.display(),
                bundle.issue.issue_id,
                bundle.jobs.len(),
                bundle.action_runs.len(),
                bundle.snapshots.len(),
                bundle.artifacts.len(),
                bundle.events.len()
            );
        }
        Command::Sessions {
            action: SessionsAction::Import { path, by },
        } => {
            let bytes = std::fs::read(&path)?;
            let bundle = serde_json::from_slice(&bytes)?;
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
            let summary = runner.import_session(bundle, &by).await?;
            println!(
                "imported Issue {} “{}” from `{}` (exported by {} on {}) as a read-only archive · {} pass(es), {} action(s), {} snapshot(s), {} artifact(s), {} event(s)",
                summary.issue_id,
                summary.title,
                summary.source_deployment,
                summary.exported_by,
                summary.exported_at.format("%Y-%m-%d %H:%M UTC"),
                summary.jobs,
                summary.action_runs,
                summary.snapshots,
                summary.artifacts,
                summary.events
            );
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
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
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
            let runner = wire_runner(&config, TeamBackend::ReadOnly)?;
            print!("{}", SliceRunner::render_inbox(&runner.inbox().await?));
        }
        Command::Review { item } => {
            let (args, is_job, id) = match &item {
                ReviewItem::Action { id, decision } => (decision, false, *id),
                ReviewItem::Job { id, decision } => (decision, true, *id),
            };
            let decision = args.decision()?;
            let runner = wire_runner(&config, select_backend(&config, args.team)?)?;
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
            let runner = Arc::new(wire_runner(&config, select_backend(&config, team)?)?);

            // Recovery before serving: reconcile what a previous process left behind and
            // restore its freeze state. A clean restart resumes on its own; anything else
            // waits for a human, who can see why in the console.
            let summary = runner.recover().await?;
            print_recovery(&summary);
            if summary.is_clean_restart() && !stay_frozen {
                runner.scheduler().resume().await?;
                println!("clean restart: dispatch resumed");
            } else {
                let why = if stay_frozen {
                    "--stay-frozen was given".to_string()
                } else if summary.touched_anything() {
                    "this recovery reconciled interrupted work into the inbox".to_string()
                } else if summary.pending_recovery_review {
                    "an earlier recovery put items in the inbox and nobody has resumed since"
                        .to_string()
                } else {
                    format!("a human left the Scheduler {:?}", summary.previous_mode)
                };
                println!(
                    "scheduler stays {:?} because {why}: check the inbox, then resume from a console",
                    runner.scheduler().mode().await
                );
            }

            // The Collector's own schedule starts once recovery has settled the Store; it
            // follows the cadence the live settings name, zero included.
            let _periodic = runner.spawn_periodic_capture();
            let cadence = match config.collector.snapshot_interval() {
                Some(interval) => format!("every {} s", interval.as_secs()),
                None => "off ([collector] snapshot_interval_secs = 0)".to_string(),
            };

            let bind = bind.unwrap_or_else(|| config.api.bind.clone());
            println!(
                "serving API on http://{bind} · team backend: {} · dry-run: {} · periodic snapshots: {cadence}",
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
                broccoli_devops_agent::api::ApiState::new(runner, config)
                    .with_recovery(summary)
                    .with_config_path(cli.config.clone()),
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
