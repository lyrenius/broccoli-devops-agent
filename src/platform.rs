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
//! The same executor serves two callers. An ActionRun — approved or automatically allowed by the
//! Scheduler — may run any registered runbook. An inspection — a running Team asking to look at
//! a status or a log tail before it proposes anything — may run only runbooks whose operation
//! class is non-mutating; the Platform refuses everything else no matter who asks, so a Team
//! cannot restart a service by calling it an inspection.
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

use crate::domain::{
    ActionRun, ActionRunId, ArtifactKind, Job, JobId, NamedValue, PlatformOperationResult,
    ResourceId, ResourceKind,
};
use crate::error::AgentResult;
use crate::policy::{ClassificationLists, RunbookRegistry, SHELL_METACHARACTERS};
use crate::ports::{AgentsPlatformPort, InspectionRequest, StateStore};
use crate::settings::SharedSettings;
use crate::topology::DeploymentTopology;
use crate::tr;
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

/// Who an execution record is attributed to.
#[derive(Debug, Clone, Copy)]
enum Producer {
    /// An ActionRun the Scheduler handed over.
    Action(ActionRunId),
    /// A running Job inspecting within its scope.
    Job(JobId),
}

/// Command-executing Platform over the operator's runbook templates.
pub struct LocalCommandPlatform {
    /// Dry-run, timeouts, classification, and commands — read at each operation, so a change
    /// on the Settings page applies to the next one.
    settings: SharedSettings,
    artifacts: FileArtifactStore,
    store: Arc<dyn StateStore>,
    resources: HashMap<ResourceId, ResourceKind>,
    lanes: ExecutionLanes,
}

impl LocalCommandPlatform {
    /// Builds the Platform over the shared live settings (whose `platform` section is the
    /// operator's config), the artifact body store, the state store (where ActionOutput
    /// Artifacts are registered), and the topology's resource catalog.
    pub fn new(
        settings: SharedSettings,
        artifacts: FileArtifactStore,
        store: Arc<dyn StateStore>,
        topology: &DeploymentTopology,
    ) -> Self {
        Self {
            settings,
            artifacts,
            store,
            resources: topology
                .resources
                .iter()
                .map(|resource| (resource.id.clone(), resource.kind))
                .collect(),
            lanes: ExecutionLanes::default(),
        }
    }

    /// The Platform section of the live settings, as of now.
    fn config(&self) -> PlatformConfig {
        self.settings.read(|settings| settings.platform.clone())
    }

    /// The Runbook Registry over the classification lists as of now.
    fn registry(&self) -> RunbookRegistry {
        RunbookRegistry::new(
            self.settings
                .read(|settings| settings.platform.classification.clone()),
        )
    }

    /// Runbook IDs that are configured with a command and classify as non-mutating — the set a
    /// Team may call as inspections. Used to build the Team's tool allowlist.
    pub fn inspection_runbook_ids(config: &PlatformConfig) -> Vec<String> {
        let registry = RunbookRegistry::new(config.classification.clone());
        let mut ids: Vec<String> = config
            .runbooks
            .iter()
            .filter(|runbook| {
                registry
                    .classify(&runbook.id, &[])
                    .is_some_and(|class| !class.is_mutating())
            })
            .map(|runbook| runbook.id.clone())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Re-validates a request's scope against the catalog: known targets of an allowed kind
    /// and shell-safe arguments. The Job-level checks (capabilities, target scope) happened in
    /// the Scheduler; this is the machine-side half. An inspection must additionally classify
    /// as non-mutating.
    fn validate(
        &self,
        runbook_id: &str,
        target_ids: &[ResourceId],
        arguments: &[NamedValue],
        inspection: bool,
    ) -> Result<(), (String, serde_json::Value)> {
        let unknown: Vec<_> = target_ids
            .iter()
            .filter(|target| !self.resources.contains_key(*target))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err((
                tr!(
                    format!("refused: unknown target(s) {}", unknown.join(", ")),
                    format!("已拒绝：未知目标 {}", unknown.join(", "))
                ),
                json!({ "refused": "unknown targets", "targets": unknown }),
            ));
        }
        let Some(class) = self.registry().classify(runbook_id, arguments) else {
            return Err((
                tr!(
                    format!("refused: runbook `{runbook_id}` is not in the Runbook Registry"),
                    format!("已拒绝：runbook `{runbook_id}` 不在 Runbook 注册表中")
                ),
                json!({ "refused": "unknown runbook", "runbook_id": runbook_id }),
            ));
        };
        if inspection && class.is_mutating() {
            return Err((
                tr!(
                    format!(
                        "refused: `{runbook_id}` (row {}, {class:?}) changes machine state and \
                         cannot run as an inspection; propose it as an action instead",
                        class.row()
                    ),
                    format!(
                        "已拒绝：`{runbook_id}`（第 {} 行，{class:?}）会改变机器状态，不能作为检查运行；请改为提议操作",
                        class.row()
                    )
                ),
                json!({ "refused": "mutating runbook as inspection", "runbook_id": runbook_id }),
            ));
        }
        if let Some(kinds) = class.target_kinds() {
            for target in target_ids {
                let kind = self.resources[target];
                if !kinds.contains(&kind) {
                    return Err((
                        tr!(
                            format!(
                                "refused: `{runbook_id}` (row {}) does not apply to `{target}`, a \
                                 {kind:?} resource",
                                class.row()
                            ),
                            format!(
                                "已拒绝：`{runbook_id}`（第 {} 行）不适用于 `{target}`（其类型为 {kind:?}）",
                                class.row()
                            )
                        ),
                        json!({ "refused": "target kind", "target": target, "kind": kind }),
                    ));
                }
            }
        }
        if let Err(reason) = RunbookRegistry::validate_arguments(runbook_id, arguments) {
            return Err((
                tr!(format!("refused: {reason}"), format!("已拒绝：{reason}")),
                json!({ "refused": reason }),
            ));
        }
        Ok(())
    }

    /// Whether the Platform is in dry-run mode.
    pub fn dry_run(&self) -> bool {
        self.settings.read(|settings| settings.platform.dry_run)
    }

    /// Renders one command template for one target, or explains what is missing.
    fn render(
        &self,
        template: &str,
        target: &str,
        arguments: &[NamedValue],
    ) -> Result<String, String> {
        let mut rendered = template.replace("{target}", target);
        while let Some(start) = rendered.find("{arg:") {
            let end = rendered[start..]
                .find('}')
                .map(|offset| start + offset)
                .ok_or_else(|| "unterminated `{arg:` placeholder".to_string())?;
            let name = &rendered[start + 5..end];
            let value = arguments
                .iter()
                .find(|argument| argument.name == name)
                .map(|argument| argument.value.clone())
                .ok_or_else(|| {
                    tr!(
                        format!("runbook requires argument `{name}`"),
                        format!("runbook 需要参数 `{name}`")
                    )
                })?;
            if value.contains(SHELL_METACHARACTERS) {
                return Err(tr!(
                    format!("argument `{name}` contains shell metacharacters"),
                    format!("参数 `{name}` 包含 shell 元字符")
                ));
            }
            rendered.replace_range(start..=end, &value);
        }
        Ok(rendered)
    }

    /// Stores and registers the execution record, and returns the result.
    async fn finish(
        &self,
        producer: Producer,
        succeeded: bool,
        summary: String,
        record: serde_json::Value,
    ) -> AgentResult<PlatformOperationResult> {
        let bytes = serde_json::to_vec_pretty(&record)?;
        let artifact = self.artifacts.write(ArtifactKind::ActionOutput, &bytes)?;
        let artifact = match producer {
            Producer::Action(action_run_id) => artifact.produced_by_action(action_run_id),
            Producer::Job(job_id) => artifact.produced_by_job(job_id),
        };
        self.store.insert_artifact(artifact.clone()).await?;
        Ok(PlatformOperationResult::new(
            succeeded,
            Some(artifact.artifact_id),
            summary,
        ))
    }

    /// Validates, renders, and (unless dry-run) executes one runbook once per target.
    ///
    /// Refusals are reported as failed results, not errors: the record says exactly why the
    /// Platform declined. This is the execution block proper for both ActionRuns and
    /// inspections; only the validation differs.
    async fn run(
        &self,
        runbook_id: &str,
        target_ids: &[ResourceId],
        arguments: &[NamedValue],
        producer: Producer,
        inspection: bool,
    ) -> AgentResult<PlatformOperationResult> {
        if let Err((summary, record)) = self.validate(runbook_id, target_ids, arguments, inspection)
        {
            return self.finish(producer, false, summary, record).await;
        }
        let config = self.config();
        let Some(runbook) = config
            .runbooks
            .iter()
            .find(|runbook| runbook.id == runbook_id)
        else {
            return self
                .finish(
                    producer,
                    false,
                    tr!(
                        format!("refused: no command is configured for runbook `{runbook_id}`"),
                        format!("已拒绝：runbook `{runbook_id}` 未配置命令")
                    ),
                    json!({ "refused": "no command configured", "runbook_id": runbook_id }),
                )
                .await;
        };

        let mut commands = Vec::new();
        for target in target_ids {
            match self.render(&runbook.command, target, arguments) {
                Ok(command) => commands.push((target.clone(), command)),
                Err(reason) => {
                    return self
                        .finish(
                            producer,
                            false,
                            tr!(format!("refused: {reason}"), format!("已拒绝：{reason}")),
                            json!({ "refused": reason, "target": target }),
                        )
                        .await;
                }
            }
        }

        if config.dry_run {
            return self
                .finish(
                    producer,
                    true,
                    tr!(
                        format!(
                            "dry run: would execute {} command(s) for `{runbook_id}`",
                            commands.len()
                        ),
                        format!("演练：本应为 `{runbook_id}` 执行 {} 条命令", commands.len())
                    ),
                    json!({ "dry_run": true, "commands": commands }),
                )
                .await
                .map(PlatformOperationResult::as_dry_run);
        }

        // The execution block proper: hold the lanes for every target, then run the
        // per-target executors in order. Each executor reaps its process before the next
        // starts, and the lanes are released only after the last one has. Inspections take
        // the lanes too, so a log read never interleaves with a restart of the same target.
        let _lanes = self.lanes.acquire(target_ids).await;
        let timeout = Duration::from_secs(config.command_timeout_secs);
        let mut runs = Vec::new();
        let mut all_ok = true;
        let mut problems = Vec::new();
        for (target, command) in &commands {
            let outcome = run_command(command, timeout).await;
            if !outcome.succeeded() {
                all_ok = false;
                let exit_code = outcome
                    .exit_code
                    .map_or("none".to_string(), |code| code.to_string());
                problems.push(match (&outcome.spawn_error, outcome.timed_out) {
                    (Some(error), _) => tr!(
                        format!("`{target}`: could not start ({error})"),
                        format!("`{target}`：无法启动（{error}）")
                    ),
                    (None, true) => tr!(
                        format!(
                            "`{target}`: timed out after {}s and was killed",
                            timeout.as_secs()
                        ),
                        format!("`{target}`：{} 秒后超时并已被终止", timeout.as_secs())
                    ),
                    (None, false) => tr!(
                        format!("`{target}`: exit code {exit_code}"),
                        format!("`{target}`：退出码 {exit_code}")
                    ),
                });
            }
            let mut entry = serde_json::to_value(&outcome)?;
            entry["target"] = json!(target);
            entry["command"] = json!(command);
            runs.push(entry);
        }
        self.finish(
            producer,
            all_ok,
            {
                let outcome_text = if all_ok {
                    tr!("all exited 0", "全部以退出码 0 结束").to_string()
                } else {
                    problems.join("; ")
                };
                tr!(
                    format!(
                        "executed {} command(s) for `{runbook_id}`: {outcome_text}",
                        commands.len()
                    ),
                    format!(
                        "已为 `{runbook_id}` 执行 {} 条命令：{outcome_text}",
                        commands.len()
                    )
                )
            },
            json!({ "dry_run": false, "runs": runs }),
        )
        .await
    }
}

#[async_trait]
impl AgentsPlatformPort for LocalCommandPlatform {
    /// Validates, renders, and (unless dry-run) executes the runbook once per target.
    ///
    /// Refusals are reported as failed results, not errors: the ActionRun records exactly why the
    /// Platform declined, and the Scheduler treats it like any other failed execution.
    async fn execute_action(&self, action: &ActionRun) -> AgentResult<PlatformOperationResult> {
        self.run(
            &action.runbook_id,
            &action.target_ids,
            &action.arguments,
            Producer::Action(action.action_run_id),
            false,
        )
        .await
    }

    /// Runs a non-mutating runbook for a running Job and records the output as an Artifact
    /// produced by that Job. A mutating runbook is refused here regardless of who asked.
    async fn inspect(
        &self,
        job: &Job,
        request: &InspectionRequest,
    ) -> AgentResult<PlatformOperationResult> {
        self.run(
            &request.runbook_id,
            &request.target_ids,
            &request.arguments,
            Producer::Job(job.job_id),
            true,
        )
        .await
    }
}
