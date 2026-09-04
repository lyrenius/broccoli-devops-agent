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
use crate::platform::PlatformConfig;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

fn default_api_key_env() -> String {
    DEFAULT_API_KEY_ENV.to_string()
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
            ..HarnessBudget::default()
        }
    }
}

/// The complete agent configuration file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Store and artifact location.
    pub data: DataConfig,
    /// Topology file location.
    pub topology: TopologyConfig,
    /// Model relay; absent means only the deterministic Team is available.
    pub model: Option<ModelConfig>,
    /// Agents Platform: runbook commands, dry-run, and classification lists.
    pub platform: PlatformConfig,
}

impl AppConfig {
    /// Parses a configuration from TOML text.
    pub fn from_toml(text: &str) -> AgentResult<Self> {
        toml::from_str(text).map_err(|error| {
            AgentError::InvalidInput(format!("agent config parse failed: {error}"))
        })
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

    /// Renders the effective configuration for display or a frontend, never including the key.
    pub fn effective_json(&self) -> AgentResult<Value> {
        let mut value = serde_json::to_value(self)?;
        if let Some(model) = &self.model {
            value["model"]["api_key_present"] = json!(model.api_key_present());
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_sections_are_omitted() {
        let config = AppConfig::from_toml("").unwrap();
        assert_eq!(config.data.dir, PathBuf::from("data"));
        assert_eq!(config.topology.path, PathBuf::from("config/topology.toml"));
        assert!(config.model.is_none());
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
        let model = config.model.unwrap();
        assert_eq!(model.api_key_env, DEFAULT_API_KEY_ENV);
        assert_eq!(model.wire_api, WireApi::Chat);
        assert_eq!(model.timeout_secs, 120);
        assert_eq!(model.harness_budget().max_model_turns, 8);
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
