//! Agents Platform v0.1: executes registered runbooks as operator-configured commands.
//!
//! The Platform is the single execution gateway. This implementation keeps that promise with a
//! data-driven design: the operator maps each runbook ID to a command template in the agent
//! config (for example `ssh {target} sudo systemctl restart broccoli-worker`), so credentials
//! stay with the machine's SSH agent and never enter the agent's config or a model's context.
//! Every request is validated before anything runs — the runbook must have a command, every
//! target must exist in the topology — and complete output is stored as an Artifact.
//!
//! `dry_run` (the default) renders and records the commands without executing them, so the whole
//! pipeline can be rehearsed before a deployment exists.
//!
//! Inside the Platform sits the execution block the architecture diagram calls "DevOps Agents &
//! Scheduler": the per-target executors that run one runbook on one host, and the lane scheduler
//! that serializes them so two approved actions never operate on the same resource at once.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::domain::{ActionRun, ArtifactKind, PlatformOperationResult, ResourceId};
use crate::error::AgentResult;
use crate::policy::ClassificationLists;
use crate::ports::AgentsPlatformPort;
use crate::topology::DeploymentTopology;
use crate::view::FileArtifactStore;

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

/// Command-executing Platform over the operator's runbook templates.
pub struct LocalCommandPlatform {
    config: PlatformConfig,
    artifacts: FileArtifactStore,
    known_targets: HashSet<ResourceId>,
    lanes: ExecutionLanes,
}

impl LocalCommandPlatform {
    /// Builds the Platform; targets are validated against the topology's resource IDs.
    pub fn new(
        config: PlatformConfig,
        artifacts: FileArtifactStore,
        topology: &DeploymentTopology,
    ) -> Self {
        Self {
            config,
            artifacts,
            known_targets: topology
                .resources
                .iter()
                .map(|resource| resource.id.clone())
                .collect(),
            lanes: ExecutionLanes::default(),
        }
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
            if value
                .chars()
                .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '\n'))
            {
                return Err(format!("argument `{name}` contains shell metacharacters"));
            }
            rendered.replace_range(start..=end, &value);
        }
        Ok(rendered)
    }

    /// Stores the execution record and returns the result.
    fn finish(
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
        let unknown: Vec<_> = action
            .target_ids
            .iter()
            .filter(|target| !self.known_targets.contains(*target))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return self.finish(
                action,
                false,
                format!("refused: unknown target(s) {}", unknown.join(", ")),
                json!({ "refused": "unknown targets", "targets": unknown }),
            );
        }
        let Some(runbook) = self
            .config
            .runbooks
            .iter()
            .find(|runbook| runbook.id == action.runbook_id)
        else {
            return self.finish(
                action,
                false,
                format!(
                    "refused: no command is configured for runbook `{}`",
                    action.runbook_id
                ),
                json!({ "refused": "no command configured", "runbook_id": action.runbook_id }),
            );
        };

        let mut commands = Vec::new();
        for target in &action.target_ids {
            match self.render(&runbook.command, target, action) {
                Ok(command) => commands.push((target.clone(), command)),
                Err(reason) => {
                    return self.finish(
                        action,
                        false,
                        format!("refused: {reason}"),
                        json!({ "refused": reason, "target": target }),
                    );
                }
            }
        }

        if self.config.dry_run {
            return self.finish(
                action,
                true,
                format!(
                    "dry run: would execute {} command(s) for `{}`",
                    commands.len(),
                    action.runbook_id
                ),
                json!({ "dry_run": true, "commands": commands }),
            );
        }

        // The execution block proper: hold the lanes for every target, then run the
        // per-target executors in order.
        let _lanes = self.lanes.acquire(&action.target_ids).await;
        let mut runs = Vec::new();
        let mut all_ok = true;
        for (target, command) in &commands {
            let outcome = tokio::time::timeout(
                Duration::from_secs(self.config.command_timeout_secs),
                Command::new("sh").arg("-c").arg(command).output(),
            )
            .await;
            let entry = match outcome {
                Ok(Ok(output)) => {
                    let ok = output.status.success();
                    all_ok &= ok;
                    json!({
                        "target": target,
                        "command": command,
                        "exit_code": output.status.code(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": String::from_utf8_lossy(&output.stderr),
                    })
                }
                Ok(Err(error)) => {
                    all_ok = false;
                    json!({ "target": target, "command": command, "spawn_error": error.to_string() })
                }
                Err(_) => {
                    all_ok = false;
                    json!({ "target": target, "command": command, "timed_out": true })
                }
            };
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
                    "all exited 0"
                } else {
                    "at least one failed"
                }
            ),
            json!({ "dry_run": false, "runs": runs }),
        )
    }
}
