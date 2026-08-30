//! The model-backend boundary.

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
/// execute tools themselves — execution belongs to the harness so allowlisting, timeouts, and the
/// transcript stay in one place.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Produces the model's next turn for the given conversation.
    async fn complete(&self, request: ModelRequest<'_>) -> HarnessResult<Vec<AssistantItem>>;
}
