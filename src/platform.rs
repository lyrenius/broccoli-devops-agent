//! Agents Platform v0.1: executes registered runbooks as operator-configured commands.
//!
//! The Platform is the single execution gateway. This implementation keeps that promise with a
//! data-driven design: the operator maps each runbook ID to a command template in the agent
//! config (for example `ssh {target} sudo systemctl restart broccoli-worker`), so credentials
//! stay with the machine's SSH agent and never enter the agent's config or a model's context.
//! Every request is validated before anything runs — the runbook must have a command, every
//! target must exist in the topology and be of a kind the runbook's operation class applies to,
//! arguments must be shell-safe — and complete output is stored and registered as an Artifact.
//! The Scheduler already validated the same scope against the Job; the Platform checks again
//! because it is the last gate before a machine changes.
//!
//! `dry_run` (the default) renders and records the commands without executing them, so the whole
//! pipeline can be rehearsed before a deployment exists.
//!
//! Inside the Platform sits the execution block the architecture diagram calls "DevOps Agents &
//! Scheduler": the per-target executors that run one runbook on one host, and the lane scheduler
//! that serializes them so two approved actions never operate on the same resource at once. An
//! executor owns its child process: on timeout the whole process group is killed and reaped
//! before the result is reported and the lanes released, so "failed" never means "still running".

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::domain::{ActionRun, ArtifactKind, PlatformOperationResult, ResourceId, ResourceKind};
use crate::error::AgentResult;
use crate::policy::{ClassificationLists, RunbookRegistry, SHELL_METACHARACTERS};
use crate::ports::{AgentsPlatformPort, StateStore};
use crate::topology::DeploymentTopology;
use crate::view::FileArtifactStore;

/// Bytes of stdout or stderr kept per command in the ActionOutput Artifact.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// One runbook's command template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookCommand {
    /// Runbook ID from the Runbook Registry.
    pub id: String,
    /// Shell command template; `{target}` and `{arg:name}` are substituted per target.
    pub command: String,
}

/// Platform section of the agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformConfig {
    /// Render and record commands without executing them.
    pub dry_run: bool,
    /// Wall-clock limit for each executed command.
    pub command_timeout_secs: u64,
    /// Window for the automatic-repeat escalation rule.
    pub auto_repeat_window_secs: u64,
    /// Classification lists for config keys and firewall addresses.
    pub classification: ClassificationLists,
    /// Command templates per runbook.
    pub runbooks: Vec<RunbookCommand>,
}

impl Default for PlatformConfig {
    /// Dry-run by default: nothing executes until an operator opts in.
    fn default() -> Self {
        Self {
            dry_run: true,
            command_timeout_secs: 60,
            auto_repeat_window_secs: 600,
            classification: ClassificationLists::default(),
            runbooks: Vec::new(),
        }
    }
}

/// Serializes execution per target resource.
///
/// Approvals can arrive concurrently from several consoles; a restart and a status query for the
/// same worker must still run one after the other, in the order they were admitted. Lanes are
/// acquired in sorted target order so multi-target actions cannot deadlock each other.
#[derive(Default)]
struct ExecutionLanes {
    lanes: std::sync::Mutex<HashMap<ResourceId, Arc<Mutex<()>>>>,
}

impl ExecutionLanes {
    /// Returns the lane for one target, creating it on first use.
    fn lane(&self, target: &str) -> Arc<Mutex<()>> {
        let mut lanes = self
            .lanes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        lanes
            .entry(target.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Holds every lane the action needs until the returned guards drop.
    async fn acquire(&self, targets: &[ResourceId]) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut sorted: Vec<&ResourceId> = targets.iter().collect();
        sorted.sort();
        sorted.dedup();
        let mut guards = Vec::with_capacity(sorted.len());
        for target in sorted {
            guards.push(self.lane(target).lock_owned().await);
        }
        guards
    }
}

/// What one executed command produced, with the process accounted for.
#[derive(Debug, Clone, Serialize)]
struct CommandOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    killed: bool,
    output_truncated: bool,
    spawn_error: Option<String>,
}

impl CommandOutcome {
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && self.spawn_error.is_none()
    }
}

/// Runs one shell command in its own process group with a wall-clock limit.
///
/// On timeout the group receives SIGKILL and the child is reaped before this returns, so no
/// command outlives the result that reports on it. Output is captured concurrently and capped.
async fn run_command(command: &str, timeout: Duration) -> CommandOutcome {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                killed: false,
                output_truncated: false,
                spawn_error: Some(error.to_string()),
            };
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_capped = |pipe: Option<tokio::process::ChildStdout>| async move {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    };
    let read_capped_err = |pipe: Option<tokio::process::ChildStderr>| async move {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    };
    let stdout_task = tokio::spawn(read_capped(stdout));
    let stderr_task = tokio::spawn(read_capped_err(stderr));

    let (exit_code, timed_out, killed) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false, false),
        Ok(Err(_)) => (None, false, false),
        Err(_) => {
            // Kill the whole group so helpers spawned by the runbook die with it, then reap the
            // child. `kill(1)` is used instead of a raw libc call because this crate forbids
            // unsafe code; the group id equals the child's pid because of `process_group(0)`.
            if let Some(pid) = pid {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{pid}")])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            (None, true, true)
        }
    };
    // Readers finish when the pipes close; after a group kill that is immediate. The guard is
    // for a stray grandchild that escaped the group and still holds the pipe.
    let collect = |task: tokio::task::JoinHandle<Vec<u8>>| async move {
        match tokio::time::timeout(Duration::from_secs(5), task).await {
            Ok(Ok(bytes)) => bytes,
            _ => Vec::new(),
        }
    };
    let mut stdout_bytes = collect(stdout_task).await;
    let mut stderr_bytes = collect(stderr_task).await;
    let output_truncated = stdout_bytes.len() > OUTPUT_LIMIT || stderr_bytes.len() > OUTPUT_LIMIT;
    stdout_bytes.truncate(OUTPUT_LIMIT);
    stderr_bytes.truncate(OUTPUT_LIMIT);
    CommandOutcome {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        timed_out,
        killed,
        output_truncated,
        spawn_error: None,
    }
}

/// Command-executing Platform over the operator's runbook templates.
pub struct LocalCommandPlatform {
    config: PlatformConfig,
    artifacts: FileArtifactStore,
    store: Arc<dyn StateStore>,
    registry: RunbookRegistry,
    resources: HashMap<ResourceId, ResourceKind>,
    lanes: ExecutionLanes,
}

impl LocalCommandPlatform {
    /// Builds the Platform over the operator's config, the artifact body store, the state store
    /// (where ActionOutput Artifacts are registered), and the topology's resource catalog.
    pub fn new(
        config: PlatformConfig,
        artifacts: FileArtifactStore,
        store: Arc<dyn StateStore>,
        topology: &DeploymentTopology,
    ) -> Self {
        let registry = RunbookRegistry::new(config.classification.clone());
        Self {
            config,
            artifacts,
            store,
            registry,
            resources: topology
                .resources
                .iter()
                .map(|resource| (resource.id.clone(), resource.kind))
                .collect(),
            lanes: ExecutionLanes::default(),
        }
    }

    /// Re-validates the proposal's scope against the catalog: known targets of an allowed kind
    /// and shell-safe arguments. The Job-level checks (capabilities, target scope) happened in
    /// the Scheduler; this is the machine-side half.
    fn validate(&self, action: &ActionRun) -> Result<(), (String, serde_json::Value)> {
        let unknown: Vec<_> = action
            .target_ids
            .iter()
            .filter(|target| !self.resources.contains_key(*target))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err((
                format!("refused: unknown target(s) {}", unknown.join(", ")),
                json!({ "refused": "unknown targets", "targets": unknown }),
            ));
        }
        let Some(class) = self
            .registry
            .classify(&action.runbook_id, &action.arguments)
        else {
            return Err((
                format!(
                    "refused: runbook `{}` is not in the Runbook Registry",
                    action.runbook_id
                ),
                json!({ "refused": "unknown runbook", "runbook_id": action.runbook_id }),
            ));
        };
        if let Some(kinds) = class.target_kinds() {
            for target in &action.target_ids {
                let kind = self.resources[target];
                if !kinds.contains(&kind) {
                    return Err((
                        format!(
                            "refused: `{}` (row {}) does not apply to `{target}`, a {kind:?} resource",
                            action.runbook_id,
                            class.row()
                        ),
                        json!({ "refused": "target kind", "target": target, "kind": kind }),
                    ));
                }
            }
        }
        if let Err(reason) =
            RunbookRegistry::validate_arguments(&action.runbook_id, &action.arguments)
        {
            return Err((format!("refused: {reason}"), json!({ "refused": reason })));
        }
        Ok(())
    }

    /// Whether the Platform is in dry-run mode.
    pub fn dry_run(&self) -> bool {
        self.config.dry_run
    }

    /// Renders one command template for one target, or explains what is missing.
    fn render(&self, template: &str, target: &str, action: &ActionRun) -> Result<String, String> {
        let mut rendered = template.replace("{target}", target);
        while let Some(start) = rendered.find("{arg:") {
            let end = rendered[start..]
                .find('}')
                .map(|offset| start + offset)
                .ok_or_else(|| "unterminated `{arg:` placeholder".to_string())?;
            let name = &rendered[start + 5..end];
            let value = action
                .arguments
                .iter()
                .find(|argument| argument.name == name)
                .map(|argument| argument.value.clone())
                .ok_or_else(|| format!("runbook requires argument `{name}`"))?;
            if value.contains(SHELL_METACHARACTERS) {
                return Err(format!("argument `{name}` contains shell metacharacters"));
            }
            rendered.replace_range(start..=end, &value);
        }
        Ok(rendered)
    }

    /// Stores and registers the execution record, and returns the result.
    async fn finish(
        &self,
        action: &ActionRun,
        succeeded: bool,
        summary: String,
        record: serde_json::Value,
    ) -> AgentResult<PlatformOperationResult> {
        let bytes = serde_json::to_vec_pretty(&record)?;
        let artifact = self
            .artifacts
            .write(ArtifactKind::ActionOutput, &bytes)?
            .produced_by_action(action.action_run_id);
        self.store.insert_artifact(artifact.clone()).await?;
        Ok(PlatformOperationResult::new(
            succeeded,
            Some(artifact.artifact_id),
            summary,
        ))
    }
}

#[async_trait]
impl AgentsPlatformPort for LocalCommandPlatform {
    /// Validates, renders, and (unless dry-run) executes the runbook once per target.
    ///
    /// Refusals are reported as failed results, not errors: the ActionRun records exactly why the
    /// Platform declined, and the Scheduler treats it like any other failed execution.
    async fn execute_action(&self, action: &ActionRun) -> AgentResult<PlatformOperationResult> {
        if let Err((summary, record)) = self.validate(action) {
            return self.finish(action, false, summary, record).await;
        }
        let Some(runbook) = self
            .config
            .runbooks
            .iter()
            .find(|runbook| runbook.id == action.runbook_id)
        else {
            return self
                .finish(
                    action,
                    false,
                    format!(
                        "refused: no command is configured for runbook `{}`",
                        action.runbook_id
                    ),
                    json!({ "refused": "no command configured", "runbook_id": action.runbook_id }),
                )
                .await;
        };

        let mut commands = Vec::new();
        for target in &action.target_ids {
            match self.render(&runbook.command, target, action) {
                Ok(command) => commands.push((target.clone(), command)),
                Err(reason) => {
                    return self
                        .finish(
                            action,
                            false,
                            format!("refused: {reason}"),
                            json!({ "refused": reason, "target": target }),
                        )
                        .await;
                }
            }
        }

        if self.config.dry_run {
            return self
                .finish(
                    action,
                    true,
                    format!(
                        "dry run: would execute {} command(s) for `{}`",
                        commands.len(),
                        action.runbook_id
                    ),
                    json!({ "dry_run": true, "commands": commands }),
                )
                .await
                .map(PlatformOperationResult::as_dry_run);
        }

        // The execution block proper: hold the lanes for every target, then run the
        // per-target executors in order. Each executor reaps its process before the next
        // starts, and the lanes are released only after the last one has.
        let _lanes = self.lanes.acquire(&action.target_ids).await;
        let timeout = Duration::from_secs(self.config.command_timeout_secs);
        let mut runs = Vec::new();
        let mut all_ok = true;
        let mut problems = Vec::new();
        for (target, command) in &commands {
            let outcome = run_command(command, timeout).await;
            if !outcome.succeeded() {
                all_ok = false;
                problems.push(match (&outcome.spawn_error, outcome.timed_out) {
                    (Some(error), _) => format!("`{target}`: could not start ({error})"),
                    (None, true) => format!(
                        "`{target}`: timed out after {}s and was killed",
                        timeout.as_secs()
                    ),
                    (None, false) => format!(
                        "`{target}`: exit code {}",
                        outcome
                            .exit_code
                            .map_or("none".to_string(), |code| code.to_string())
                    ),
                });
            }
            let mut entry = serde_json::to_value(&outcome)?;
            entry["target"] = json!(target);
            entry["command"] = json!(command);
            runs.push(entry);
        }
        self.finish(
            action,
            all_ok,
            format!(
                "executed {} command(s) for `{}`: {}",
                commands.len(),
                action.runbook_id,
                if all_ok {
                    "all exited 0".to_string()
                } else {
                    problems.join("; ")
                }
            ),
            json!({ "dry_run": false, "runs": runs }),
        )
        .await
    }
}
