//! Request-local context checks. Estimates here are never billing usage.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiktoken_rs::tokenizer::{Tokenizer, get_tokenizer};

use crate::error::{HarnessError, HarnessResult};

/// Startup-only generation options, independent of a run's cumulative spending budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenerationConfig {
    /// Input plus reserved output window; zero disables the local preflight check.
    pub context_window_tokens: u64,
    /// Output limit. With a context limit, omission reserves and sends 4096 tokens.
    pub max_output_tokens: Option<u64>,
    /// `auto` omits the wire option; other values are passed to the configured provider.
    pub reasoning_effort: String,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            context_window_tokens: 0,
            max_output_tokens: None,
            reasoning_effort: "auto".into(),
        }
    }
}

impl GenerationConfig {
    /// Output reservation and wire limit, retaining the provider default for old configurations.
    pub fn output_limit(&self) -> Option<u64> {
        self.max_output_tokens
            .or((self.context_window_tokens > 0).then_some(4096))
    }

    /// Rejects contradictory limits before any HTTP request can be issued.
    pub fn validate(&self) -> HarnessResult<()> {
        if self.max_output_tokens == Some(0) {
            return Err(HarnessError::Model(
                "max_output_tokens must be positive".into(),
            ));
        }
        if self.context_window_tokens > 0
            && self.output_limit().unwrap_or(0) >= self.context_window_tokens
        {
            return Err(HarnessError::Model(
                "context_window_tokens must exceed max_output_tokens (4096 when omitted)".into(),
            ));
        }
        if self.reasoning_effort.trim().is_empty() {
            return Err(HarnessError::Model(
                "reasoning_effort must be auto or a non-empty provider value".into(),
            ));
        }
        Ok(())
    }

    /// Checks the fully rendered input and tools without changing the request or transcript.
    pub fn check(&self, model: &str, body: &Value) -> HarnessResult<()> {
        self.validate()?;
        if self.context_window_tokens == 0 {
            return Ok(());
        }
        let (input, method) = estimate_input(model, body);
        let output = self.output_limit().unwrap_or(0);
        if input.saturating_add(output) > self.context_window_tokens {
            return Err(HarnessError::ContextLimit(format!(
                "estimated input {input} tokens ({method}) + reserved output {output} exceeds context_window_tokens {}; request not sent, history retained. Increase context_window_tokens or lower max_output_tokens",
                self.context_window_tokens
            )));
        }
        Ok(())
    }
}

/// Counts the rendered input, including schemas, with framing headroom. Provider framing varies,
/// so even a matching BPE is a preflight estimate, not a replacement for response usage.
pub fn estimate_input(model: &str, body: &Value) -> (u64, &'static str) {
    let text = body.to_string();
    let tokenizer = get_tokenizer(model)
        .or_else(|| model.starts_with("gpt-5.").then_some(Tokenizer::O200kBase));
    let (tokens, method) = match tokenizer {
        Some(Tokenizer::O200kBase) => (
            tiktoken_rs::o200k_base_singleton()
                .encode_ordinary(&text)
                .len(),
            "o200k_base estimate",
        ),
        Some(Tokenizer::Cl100kBase) => (
            tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(&text)
                .len(),
            "cl100k_base estimate",
        ),
        _ => (
            text.len(),
            "conservative UTF-8 byte estimate; unknown model tokenizer",
        ),
    };
    let items = body["input"]
        .as_array()
        .or_else(|| body["messages"].as_array())
        .map_or(0, Vec::len);
    let tools = body["tools"].as_array().map_or(0, Vec::len);
    (
        (tokens as u64).saturating_add(64 + items as u64 * 32 + tools as u64 * 16),
        method,
    )
}
