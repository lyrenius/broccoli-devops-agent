//! Agent configuration: model backend, data directory, and topology location.
//!
//! One TOML file (`config/agent.toml`) tells the control plane where its state lives, which
//! topology describes the deployment, and how to reach the model relay. Secrets are never in the
//! file: the model API key is read from the environment variable the file names, so the config
//! can be committed, shared, and shown to a frontend without leaking credentials.

use std::path::{Path, PathBuf};
use std::time::Duration;

use broccoli_agent_harness::AgentConfig as HarnessBudget;
use broccoli_agent_harness::openai::{OpenAiClient, OpenAiConfig, WireApi};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{AgentError, AgentResult};
use crate::i18n::Language;
use crate::platform::PlatformConfig;
use crate::runner::PassPolicy;
use crate::usage::{Pricing, SpendBudget};

/// Default environment variable holding the model API key.
pub const DEFAULT_API_KEY_ENV: &str = "BROCCOLI_MODEL_API_KEY";

/// Where the file-backed store and artifact bodies live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataConfig {
    /// Data directory; created on first use.
    pub dir: PathBuf,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("data"),
        }
    }
}

/// The Collector's own schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorConfig {
    /// Seconds between periodic Snapshots while `serve` runs; the first one is captured as soon
    /// as the API is up. Observation only: it runs in every Scheduler mode, frozen included, so
    /// the consoles keep a fresh picture, and it never dispatches anything. Zero disables it.
    pub snapshot_interval_secs: u64,
}

/// Two minutes: often enough that the Overview is never far behind the deployment, rare enough
/// that a contest day's captures stay in the thousands of events.
pub const DEFAULT_SNAPSHOT_INTERVAL_SECS: u64 = 120;

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            snapshot_interval_secs: DEFAULT_SNAPSHOT_INTERVAL_SECS,
        }
    }
}

impl CollectorConfig {
    /// The capture cadence, or `None` when periodic capture is switched off.
    pub fn snapshot_interval(&self) -> Option<Duration> {
        (self.snapshot_interval_secs > 0).then(|| Duration::from_secs(self.snapshot_interval_secs))
    }
}

/// Where the deployment topology file lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TopologyConfig {
    /// Path to the topology TOML.
    pub path: PathBuf,
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("config/topology.toml"),
        }
    }
}

/// Model relay settings for the harness-backed Teams and, later, the Scheduler Policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// OpenAI-compatible base URL including the API prefix, e.g. `https://api.thuics.icu/v1`.
    pub base_url: String,
    /// Model name sent with every request.
    pub model: String,
    /// Environment variable that holds the API key. The key itself is never stored here.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Endpoint shape the relay speaks; `responses` by default, `chat` for relays without it.
    #[serde(default)]
    pub wire_api: WireApi,
    /// Per-request timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum model turns per agent run.
    #[serde(default = "default_max_model_turns")]
    pub max_model_turns: u32,
    /// Maximum tool calls per agent run.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// Maximum read-only inspections (status queries, log tails) the model may run per pass.
    #[serde(default = "default_max_inspections")]
    pub max_inspections: u32,
    /// Maximum tokens one pass may spend before the model is asked to conclude with what it has.
    /// Zero, the default, leaves the turn and tool-call budgets as the only per-pass limits.
    #[serde(default)]
    pub max_tokens_per_run: u64,
    /// What the relay charges, per million tokens. Without it tokens are still counted; only the
    /// money cannot be, so every cost reads as absent instead of as zero.
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

fn default_api_key_env() -> String {
    DEFAULT_API_KEY_ENV.to_string()
}

/// Whether `name` is a plausible environment variable name rather than a value.
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn default_timeout_secs() -> u64 {
    120
}
fn default_max_model_turns() -> u32 {
    8
}
fn default_max_tool_calls() -> u32 {
    16
}
fn default_max_inspections() -> u32 {
    PassPolicy::default().max_inspections
}
fn default_max_auto_passes() -> u32 {
    PassPolicy::default().max_auto_passes
}

impl ModelConfig {
    /// Returns whether the named API-key environment variable is set and non-empty.
    pub fn api_key_present(&self) -> bool {
        std::env::var(&self.api_key_env).is_ok_and(|value| !value.trim().is_empty())
    }

    /// Builds the relay client, reading the API key from the environment.
    ///
    /// A missing key is a configuration error at startup, never a silent unauthenticated call.
    pub fn build_client(&self) -> AgentResult<OpenAiClient> {
        let api_key = std::env::var(&self.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                AgentError::InvalidInput(format!(
                    "model API key environment variable `{}` is not set",
                    self.api_key_env
                ))
            })?;
        OpenAiClient::new(OpenAiConfig {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key,
            wire_api: self.wire_api,
            timeout: Duration::from_secs(self.timeout_secs),
        })
        .map_err(|error| AgentError::InvalidInput(format!("model client: {error}")))
    }

    /// Converts the configured budgets into harness run limits.
    pub fn harness_budget(&self) -> HarnessBudget {
        HarnessBudget {
            max_model_turns: self.max_model_turns,
            max_tool_calls: self.max_tool_calls,
            max_total_tokens: self.max_tokens_per_run,
            ..HarnessBudget::default()
        }
    }
}

/// Agent-wide behaviour: the output language and the investigation-loop budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    /// `en` or `zh-CN`. Fixed for the life of the process; the consoles switch their own
    /// language at runtime independently.
    pub language: Language,
    /// Passes the control plane runs on its own per human report or per send-upstream review,
    /// the first pass included: a probe request or a follow-up after actions spends one. One
    /// means a single pass and then a human.
    pub max_auto_passes: u32,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            language: Language::default(),
            max_auto_passes: default_max_auto_passes(),
        }
    }
}

/// HTTP API section of the agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Socket address to bind; localhost by default so the console is not reachable from the LAN.
    pub bind: String,
    /// Optional bearer token every request must carry. Empty means no authentication — only
    /// acceptable while bound to localhost.
    pub token: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:4720".to_string(),
            token: String::new(),
        }
    }
}

/// The complete agent configuration file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Agent-wide behaviour (output language).
    pub agent: AgentSection,
    /// Store and artifact location.
    pub data: DataConfig,
    /// Topology file location.
    pub topology: TopologyConfig,
    /// The Collector's periodic capture schedule.
    pub collector: CollectorConfig,
    /// Model relay; absent means only the deterministic Team is available.
    pub model: Option<ModelConfig>,
    /// Agents Platform: runbook commands, dry-run, and classification lists.
    pub platform: PlatformConfig,
    /// HTTP API for the web console and TUI.
    pub api: ApiConfig,
    /// Cumulative spend ceiling across the data directory; reaching it freezes the Scheduler.
    pub budget: SpendBudget,
}

impl AppConfig {
    /// Parses a configuration from TOML text.
    pub fn from_toml(text: &str) -> AgentResult<Self> {
        let config: Self = toml::from_str(text).map_err(|error| {
            AgentError::InvalidInput(format!("agent config parse failed: {error}"))
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Rejects a config that would put a secret where a variable name belongs.
    ///
    /// `api_key_env` must be the name of an environment variable. When it holds anything else —
    /// typically the key itself, pasted in by mistake — the error explains the fix without
    /// echoing the value, so the secret does not end up in a terminal or a log.
    pub fn validate(&self) -> AgentResult<()> {
        if let Some(model) = &self.model
            && !is_env_var_name(&model.api_key_env)
        {
            return Err(AgentError::InvalidInput(format!(
                "[model].api_key_env must be the NAME of an environment variable (for example \
                 `{DEFAULT_API_KEY_ENV}`), but the config holds a value that is not one — it \
                 looks like the key itself. Remove it from the file, set \
                 `api_key_env = \"{DEFAULT_API_KEY_ENV}\"`, and export the key: \
                 `export {DEFAULT_API_KEY_ENV}=...`"
            )));
        }
        Ok(())
    }

    /// Loads a configuration file from disk.
    pub fn load(path: &Path) -> AgentResult<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| AgentError::Io {
            context: format!("reading agent config `{}`", path.display()),
            source,
        })?;
        Self::from_toml(&text)
    }

    /// Loads the file if it exists, otherwise returns the defaults.
    ///
    /// This keeps the CLI usable before an operator has written a config file: the deterministic
    /// Team and the file store need nothing beyond the defaults.
    pub fn load_or_default(path: &Path) -> AgentResult<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    /// The investigation-loop budgets: passes from `[agent]`, inspections from `[model]`.
    pub fn pass_policy(&self) -> PassPolicy {
        PassPolicy {
            max_auto_passes: self.agent.max_auto_passes.max(1),
            max_inspections: self
                .model
                .as_ref()
                .map_or(PassPolicy::default().max_inspections, |model| {
                    model.max_inspections
                }),
        }
    }

    /// Renders the effective configuration for display or a frontend, never including the key.
    pub fn effective_json(&self) -> AgentResult<Value> {
        let mut value = serde_json::to_value(self)?;
        if let Some(model) = &self.model {
            value["model"]["api_key_present"] = json!(model.api_key_present());
        }
        value["api"]["token"] = json!(if self.api.token.is_empty() { "" } else { "***" });
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key pasted into `api_key_env` is refused at load time without being echoed back.
    #[test]
    fn a_pasted_key_is_refused_without_being_echoed() {
        let error = AppConfig::from_toml(
            r#"
            [model]
            base_url = "https://relay.example/v1"
            model = "m"
            api_key_env = "sk-1234567890abcdef"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("must be the NAME of an environment variable"));
        assert!(
            !error.contains("sk-1234"),
            "the secret must not be echoed: {error}"
        );
        assert!(is_env_var_name("BROCCOLI_MODEL_API_KEY"));
        assert!(!is_env_var_name("sk-abc"));
        assert!(!is_env_var_name(""));
    }

    #[test]
    fn defaults_apply_when_sections_are_omitted() {
        let config = AppConfig::from_toml("").unwrap();
        assert_eq!(config.data.dir, PathBuf::from("data"));
        assert_eq!(config.topology.path, PathBuf::from("config/topology.toml"));
        assert!(config.model.is_none());
        assert_eq!(
            config.collector.snapshot_interval(),
            Some(Duration::from_secs(120)),
            "periodic capture is on by default, every two minutes"
        );
    }

    #[test]
    fn the_capture_cadence_is_configurable_and_zero_disables_it() {
        let tuned = AppConfig::from_toml("[collector]\nsnapshot_interval_secs = 30\n").unwrap();
        assert_eq!(
            tuned.collector.snapshot_interval(),
            Some(Duration::from_secs(30))
        );
        let off = AppConfig::from_toml("[collector]\nsnapshot_interval_secs = 0\n").unwrap();
        assert_eq!(off.collector.snapshot_interval(), None);
        assert_eq!(
            off.effective_json().unwrap()["collector"]["snapshot_interval_secs"],
            0
        );
    }

    #[test]
    fn model_section_parses_with_defaults_and_overrides() {
        let config = AppConfig::from_toml(
            r#"
            [model]
            base_url = "https://api.thuics.icu/v1"
            model = "gpt-5.6-sol"
            wire_api = "chat"
            "#,
        )
        .unwrap();
        assert_eq!(config.pass_policy(), PassPolicy::default());
        let model = config.model.unwrap();
        assert_eq!(model.api_key_env, DEFAULT_API_KEY_ENV);
        assert_eq!(model.wire_api, WireApi::Chat);
        assert_eq!(model.timeout_secs, 120);
        assert_eq!(model.harness_budget().max_model_turns, 8);

        let tuned = AppConfig::from_toml(
            r#"
            [agent]
            max_auto_passes = 0
            [model]
            base_url = "https://api.thuics.icu/v1"
            model = "gpt-5.6-sol"
            max_inspections = 2
            "#,
        )
        .unwrap();
        assert_eq!(tuned.pass_policy().max_inspections, 2);
        assert_eq!(
            tuned.pass_policy().max_auto_passes,
            1,
            "zero passes would mean no work at all; clamped to one"
        );
    }

    #[test]
    fn effective_json_never_contains_the_key() {
        let config = AppConfig::from_toml(
            r#"
            [model]
            base_url = "https://api.thuics.icu/v1"
            model = "gpt-5.6-sol"
            api_key_env = "BROCCOLI_TEST_KEY_THAT_IS_UNSET"
            "#,
        )
        .unwrap();
        let json = config.effective_json().unwrap();
        assert_eq!(json["model"]["api_key_present"], false);
        assert!(json["model"].get("api_key").is_none());
        assert!(
            config.model.unwrap().build_client().is_err(),
            "an unset key must be a startup error"
        );
    }
}
