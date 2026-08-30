//! The bounded agent loop: model turns, tool execution, limits, and cancellation.

use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{AssistantItem, ModelClient, ModelRequest};
use crate::conversation::{Item, Transcript, Trust};
use crate::error::{HarnessError, HarnessResult};
use crate::tool::ToolRegistry;

/// Budgets for one agent run.
///
/// Limits are the harness's answer to runaway loops: a model that keeps calling tools, keeps
/// hitting errors, or never terminates is stopped deterministically and the outcome says so.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum model turns before the run is stopped.
    pub max_model_turns: u32,
    /// Maximum tool executions (including refused and failed calls) before the run is stopped.
    pub max_tool_calls: u32,
    /// Wall-clock timeout applied to each individual tool execution.
    pub tool_timeout: Duration,
}

impl Default for AgentConfig {
    /// Conservative defaults suitable for short operational tasks.
    fn default() -> Self {
        Self {
            max_model_turns: 8,
            max_tool_calls: 16,
            tool_timeout: Duration::from_secs(30),
        }
    }
}

/// Creates a linked cancellation handle and token for one run.
///
/// The harness has its own pair (instead of borrowing the control plane's) so this crate stays
/// dependency-free in that direction; adapters bridge the two.
pub fn cancel_pair() -> (CancelHandle, CancelToken) {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    (CancelHandle { sender }, CancelToken { receiver })
}

/// Caller-side handle that requests cooperative cancellation of a run.
#[derive(Debug)]
pub struct CancelHandle {
    sender: tokio::sync::watch::Sender<bool>,
}

impl CancelHandle {
    /// Requests cancellation; the loop stops at the next step boundary.
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }
}

/// Run-side token the loop observes between steps.
#[derive(Debug, Clone)]
pub struct CancelToken {
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl CancelToken {
    /// Returns whether cancellation has been requested (a dropped handle counts as cancelled).
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow() || self.receiver.has_changed().is_err()
    }

    /// Waits until cancellation is requested.
    pub async fn cancelled(&mut self) {
        loop {
            if *self.receiver.borrow_and_update() {
                return;
            }
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

/// How one agent run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    /// A terminal tool was called successfully; `value` is its validated output.
    Structured {
        /// Name of the terminal tool.
        tool: String,
        /// The validated result value.
        value: Value,
    },
    /// The model finished with prose and no tool calls.
    Text(String),
    /// Cancellation was requested and honored.
    Cancelled,
    /// A budget in [`AgentConfig`] was exhausted.
    LimitReached {
        /// Which limit stopped the run.
        reason: String,
    },
}

/// The outcome of a run plus everything needed to audit it.
#[derive(Debug, Clone)]
pub struct AgentRunReport {
    /// How the run ended.
    pub outcome: AgentOutcome,
    /// The complete replayable transcript.
    pub transcript: Transcript,
    /// Model turns consumed.
    pub model_turns: u32,
    /// Tool calls consumed (including refused and failed calls).
    pub tool_calls: u32,
}

/// Runs the agent loop until a terminal result, a limit, or cancellation.
///
/// The contract with the model: unknown tools, malformed arguments, handler errors, and timeouts
/// come back as error tool-outputs it can read and recover from; a successful call of a tool whose
/// spec is `terminal` ends the run with that call's value; a turn with no tool calls ends the run
/// with the turn's text. Every item is appended to the transcript before the loop continues, so an
/// aborted run is still fully auditable.
pub async fn run_agent(
    client: &dyn ModelClient,
    registry: &ToolRegistry,
    config: &AgentConfig,
    instructions: &str,
    initial_items: Vec<Item>,
    mut cancel: CancelToken,
) -> HarnessResult<AgentRunReport> {
    let mut transcript = Transcript::new(instructions);
    for item in initial_items {
        transcript.push(item);
    }
    let specs = registry.specs();
    let mut model_turns: u32 = 0;
    let mut tool_calls: u32 = 0;

    let finish = |outcome, transcript, model_turns, tool_calls| {
        Ok(AgentRunReport {
            outcome,
            transcript,
            model_turns,
            tool_calls,
        })
    };

    loop {
        if cancel.is_cancelled() {
            return finish(AgentOutcome::Cancelled, transcript, model_turns, tool_calls);
        }
        if model_turns >= config.max_model_turns {
            return finish(
                AgentOutcome::LimitReached {
                    reason: format!("model-turn budget ({}) exhausted", config.max_model_turns),
                },
                transcript,
                model_turns,
                tool_calls,
            );
        }
        model_turns += 1;

        let items = transcript.items();
        let request = ModelRequest {
            instructions,
            items: &items,
            tools: &specs,
        };
        // Cancellation may arrive while the backend is thinking; racing keeps the loop honest
        // about "cooperative" instead of waiting out a slow model call.
        let turn = tokio::select! {
            turn = client.complete(request) => turn?,
            () = cancel.cancelled() => {
                return finish(AgentOutcome::Cancelled, transcript, model_turns, tool_calls);
            }
        };
        if turn.is_empty() {
            return Err(HarnessError::Model(
                "the backend returned an empty turn".into(),
            ));
        }

        let mut texts = Vec::new();
        let mut calls = Vec::new();
        for item in turn {
            match item {
                AssistantItem::Text { text } => {
                    transcript.push(Item::AssistantText { text: text.clone() });
                    texts.push(text);
                }
                AssistantItem::ToolCall {
                    call_id,
                    tool,
                    arguments,
                } => {
                    transcript.push(Item::ToolCall {
                        call_id: call_id.clone(),
                        tool: tool.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((call_id, tool, arguments));
                }
            }
        }

        if calls.is_empty() {
            return finish(
                AgentOutcome::Text(texts.join("\n")),
                transcript,
                model_turns,
                tool_calls,
            );
        }

        for (call_id, tool_name, arguments) in calls {
            if cancel.is_cancelled() {
                return finish(AgentOutcome::Cancelled, transcript, model_turns, tool_calls);
            }
            if tool_calls >= config.max_tool_calls {
                return finish(
                    AgentOutcome::LimitReached {
                        reason: format!("tool-call budget ({}) exhausted", config.max_tool_calls),
                    },
                    transcript,
                    model_turns,
                    tool_calls,
                );
            }
            tool_calls += 1;

            let Some(tool) = registry.get(&tool_name) else {
                transcript.push(Item::ToolOutput {
                    call_id,
                    tool: tool_name.clone(),
                    output: json!({
                        "error": format!("tool `{tool_name}` is not in the allowlist for this run")
                    }),
                    is_error: true,
                    trust: Trust::Trusted,
                });
                continue;
            };

            let executed =
                tokio::time::timeout(config.tool_timeout, tool.handler.call(arguments)).await;
            match executed {
                Err(_) => {
                    transcript.push(Item::ToolOutput {
                        call_id,
                        tool: tool_name.clone(),
                        output: json!({
                            "error": format!(
                                "tool `{tool_name}` timed out after {:?}",
                                config.tool_timeout
                            )
                        }),
                        is_error: true,
                        trust: Trust::Trusted,
                    });
                }
                Ok(Err(message)) => {
                    transcript.push(Item::ToolOutput {
                        call_id,
                        tool: tool_name.clone(),
                        output: json!({ "error": message }),
                        is_error: true,
                        trust: Trust::Trusted,
                    });
                }
                Ok(Ok(value)) => {
                    let terminal = tool.spec.terminal;
                    transcript.push(Item::ToolOutput {
                        call_id,
                        tool: tool_name.clone(),
                        output: value.clone(),
                        is_error: false,
                        trust: Trust::Mixed,
                    });
                    if terminal {
                        return finish(
                            AgentOutcome::Structured {
                                tool: tool_name,
                                value,
                            },
                            transcript,
                            model_turns,
                            tool_calls,
                        );
                    }
                }
            }
        }
    }
}
