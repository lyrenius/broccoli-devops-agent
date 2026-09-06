//! Test doubles for driving the harness without a real model backend.

use std::collections::VecDeque;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::{AssistantItem, ModelClient, ModelRequest, ModelTurn, Usage};
use crate::error::{HarnessError, HarnessResult};

/// A [`ModelClient`] that replays a fixed sequence of turns.
///
/// Each `complete` call pops the next scripted turn; running past the script is a backend error,
/// which is exactly how an exhausted double should fail — loudly. A number of transient failures
/// can be scripted ahead of the turns to exercise the loop's retry path.
pub struct ScriptedModelClient {
    turns: Mutex<VecDeque<Vec<AssistantItem>>>,
    transient_failures: Mutex<u32>,
    /// Tools offered on each request, in order, for asserting what the model was allowed to see.
    offered_tools: Mutex<Vec<Vec<String>>>,
    /// Usage every scripted turn reports; `None` mimics a relay that reports none.
    usage_per_turn: Option<Usage>,
}

impl ScriptedModelClient {
    /// Creates a client that will play the given turns in order.
    pub fn new(turns: Vec<Vec<AssistantItem>>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            transient_failures: Mutex::new(0),
            offered_tools: Mutex::new(Vec::new()),
            usage_per_turn: None,
        }
    }

    /// Makes every scripted turn report the given token counts, as a real backend would.
    pub fn with_usage_per_turn(mut self, usage: Usage) -> Self {
        self.usage_per_turn = Some(usage);
        self
    }

    /// Makes the first `count` requests fail with [`HarnessError::ModelUnavailable`] before the
    /// scripted turns start playing.
    pub fn with_transient_failures(mut self, count: u32) -> Self {
        self.transient_failures = Mutex::new(count);
        self
    }

    /// The tool names offered on each request so far, one list per model turn.
    pub async fn offered_tools(&self) -> Vec<Vec<String>> {
        self.offered_tools.lock().await.clone()
    }
}

#[async_trait]
impl ModelClient for ScriptedModelClient {
    /// Pops and returns the next scripted turn.
    async fn complete(&self, request: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        {
            let mut failures = self.transient_failures.lock().await;
            if *failures > 0 {
                *failures -= 1;
                return Err(HarnessError::ModelUnavailable(
                    "scripted transient failure".into(),
                ));
            }
        }
        self.offered_tools
            .lock()
            .await
            .push(request.tools.iter().map(|spec| spec.name.clone()).collect());
        let items = self
            .turns
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| HarnessError::Model("scripted client ran out of turns".into()))?;
        Ok(match self.usage_per_turn {
            Some(usage) => ModelTurn::with_usage(items, usage),
            None => ModelTurn::new(items),
        })
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
