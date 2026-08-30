//! Test doubles for driving the harness without a real model backend.

use std::collections::VecDeque;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::{AssistantItem, ModelClient, ModelRequest};
use crate::error::{HarnessError, HarnessResult};

/// A [`ModelClient`] that replays a fixed sequence of turns.
///
/// Each `complete` call pops the next scripted turn; running past the script is a backend error,
/// which is exactly how an exhausted double should fail — loudly.
pub struct ScriptedModelClient {
    turns: Mutex<VecDeque<Vec<AssistantItem>>>,
}

impl ScriptedModelClient {
    /// Creates a client that will play the given turns in order.
    pub fn new(turns: Vec<Vec<AssistantItem>>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
        }
    }
}

#[async_trait]
impl ModelClient for ScriptedModelClient {
    /// Pops and returns the next scripted turn.
    async fn complete(&self, _request: ModelRequest<'_>) -> HarnessResult<Vec<AssistantItem>> {
        self.turns
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| HarnessError::Model("scripted client ran out of turns".into()))
    }
}

/// Builds a text assistant item.
pub fn text(text: impl Into<String>) -> AssistantItem {
    AssistantItem::Text { text: text.into() }
}

/// Builds a tool-call assistant item.
pub fn call(
    call_id: impl Into<String>,
    tool: impl Into<String>,
    arguments: Value,
) -> AssistantItem {
    AssistantItem::ToolCall {
        call_id: call_id.into(),
        tool: tool.into(),
        arguments,
    }
}
