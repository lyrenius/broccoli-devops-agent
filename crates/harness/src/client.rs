//! The model-backend boundary.

use std::ops::AddAssign;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation::Item;
use crate::error::HarnessResult;
use crate::tool::ToolSpec;

/// One item the model produced in a single turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AssistantItem {
    /// Prose output.
    Text {
        /// The model's text.
        text: String,
    },
    /// A requested tool invocation.
    ToolCall {
        /// Backend-assigned call ID.
        call_id: String,
        /// Requested tool name.
        tool: String,
        /// Model-supplied arguments.
        arguments: Value,
    },
}

/// Token counts for one model request, or the sum over a whole run.
///
/// Backends report these; the harness never estimates them. A relay that omits the usage block is
/// recorded honestly in `requests_without_usage` instead of being counted as zero, because a
/// silent zero would understate a bill rather than admit the gap.
///
/// `cached_input_tokens` is the part of `input_tokens` the backend served from its prompt cache —
/// a subset, not an addition, which is how both wire formats report it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens billed as input, cached ones included.
    pub input_tokens: u64,
    /// Portion of `input_tokens` that hit the backend's prompt cache.
    pub cached_input_tokens: u64,
    /// Tokens the model generated, reasoning tokens included where the backend counts them.
    pub output_tokens: u64,
    /// Model requests this figure covers.
    pub requests: u32,
    /// Requests whose response carried no usage block, so their tokens are unknown.
    pub requests_without_usage: u32,
}

impl Usage {
    /// Counts for one request that reported its usage.
    pub fn reported(input_tokens: u64, cached_input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            requests: 1,
            requests_without_usage: 0,
        }
    }

    /// One request whose response said nothing about tokens.
    pub fn unreported() -> Self {
        Self {
            requests: 1,
            requests_without_usage: 1,
            ..Self::default()
        }
    }

    /// Input plus output tokens; what a token budget is measured against.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Whether every request counted here reported its usage.
    pub fn is_complete(&self) -> bool {
        self.requests_without_usage == 0
    }
}

impl AddAssign for Usage {
    /// Accumulates one request's counts into a running total.
    fn add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.requests_without_usage = self
            .requests_without_usage
            .saturating_add(other.requests_without_usage);
    }
}

/// One assistant turn: what the model produced, and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurn {
    /// Text and tool calls, in the order the backend returned them.
    pub items: Vec<AssistantItem>,
    /// Token counts the backend reported for this request.
    pub usage: Usage,
}

impl ModelTurn {
    /// A turn whose backend reported no usage.
    pub fn new(items: Vec<AssistantItem>) -> Self {
        Self {
            items,
            usage: Usage::unreported(),
        }
    }

    /// A turn with the backend's reported counts attached.
    pub fn with_usage(items: Vec<AssistantItem>, usage: Usage) -> Self {
        Self { items, usage }
    }
}

/// Everything a backend needs to produce the next model turn.
#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    /// System/developer instructions for the run.
    pub instructions: &'a str,
    /// The conversation so far, in order.
    pub items: &'a [Item],
    /// Tools the model may call this turn.
    pub tools: &'a [ToolSpec],
}

/// A concrete model backend: OpenAI Responses, a local model, or a scripted test double.
///
/// This is the harness's only external dependency point. Implementations translate the typed
/// request into their wire format and the wire response back into [`AssistantItem`]s; they never
/// execute tools — execution belongs to the harness so allowlisting, timeouts, and the transcript
/// stay in one place. They also report the backend's own token counts, which is why the return
/// type is a [`ModelTurn`] rather than the items alone: usage is a fact only the backend has, and
/// an adapter that discards it makes cost accounting impossible further up.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Produces the model's next turn for the given conversation.
    async fn complete(&self, request: ModelRequest<'_>) -> HarnessResult<ModelTurn>;
}
