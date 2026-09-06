//! The bounded agent loop: model turns, tool execution, limits, retries, and cancellation.

use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};

use crate::client::{AssistantItem, ModelClient, ModelRequest, Usage};
use crate::conversation::{Item, Transcript, TranscriptEntry, Trust, TurnRecord};
use crate::error::{HarnessError, HarnessResult};
use crate::tool::{ToolRegistry, ToolSpec};

/// Budgets for one agent run.
///
/// Limits are the harness's answer to runaway loops: a model that keeps calling tools, keeps
/// hitting errors, or never terminates is stopped deterministically and the outcome says so.
/// Two refinements keep a stopped run useful: the model is warned before the tool budget runs
/// out, and once a budget is exhausted it gets a bounded number of wrap-up turns in which only
/// the terminal tools are offered — so "out of budget" usually still ends in a structured result,
/// and a hard `LimitReached` is reserved for a model that will not conclude even when asked to.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum model turns before the run enters its wrap-up turns.
    pub max_model_turns: u32,
    /// Maximum tool executions (including refused and failed calls) before the run enters its
    /// wrap-up turns.
    pub max_tool_calls: u32,
    /// Wall-clock timeout applied to each individual tool execution.
    pub tool_timeout: Duration,
    /// When this many tool calls remain, the loop tells the model so it can plan to conclude.
    /// Zero disables the warning.
    pub warn_at_remaining_tool_calls: u32,
    /// Extra model turns granted after a budget is exhausted, with only terminal tools offered.
    /// Zero stops the run the moment a budget runs out.
    pub wrap_up_turns: u32,
    /// Maximum tokens (input plus output, as the backend reports them) before the run enters
    /// its wrap-up turns. Zero disables the limit, which is the default: a backend that reports
    /// no usage would otherwise never reach a limit expressed in tokens.
    pub max_total_tokens: u64,
    /// Retries of one model request after a transient backend failure
    /// ([`HarnessError::ModelUnavailable`]) before the run fails.
    pub max_model_retries: u32,
    /// Base delay before a retry; the n-th retry waits n times this.
    pub retry_backoff: Duration,
}

impl Default for AgentConfig {
    /// Conservative defaults suitable for short operational tasks.
    fn default() -> Self {
        Self {
            max_model_turns: 8,
            max_tool_calls: 16,
            tool_timeout: Duration::from_secs(30),
            warn_at_remaining_tool_calls: 3,
            wrap_up_turns: 1,
            max_total_tokens: 0,
            max_model_retries: 2,
            retry_backoff: Duration::from_secs(2),
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

/// One step the loop reached, reported while the run is still going.
///
/// A run is a sequence of slow remote calls, so "what is it doing right now" cannot be answered
/// from the finished report. These steps are the answer: an adapter forwards them to whatever
/// operators are watching, and the counters travel with each one so a console can render
/// "turn 3/8 · 5/16 tool calls · 12.4k tokens" without keeping its own state.
#[derive(Debug, Clone, PartialEq)]
pub enum RunStep {
    /// The loop is about to ask the model for a turn.
    TurnStarted,
    /// A transient backend failure is being retried after a backoff.
    Retrying {
        /// Which retry this is, counting from one.
        attempt: u32,
        /// What the backend said.
        reason: String,
    },
    /// The model answered; the turn's items are in the transcript.
    TurnCompleted {
        /// Tool calls the turn requested.
        tool_calls: u32,
        /// Whether the turn carried prose as well.
        text: bool,
    },
    /// A tool is about to run.
    ToolStarted {
        /// Tool name.
        tool: String,
    },
    /// A tool call finished, was refused, failed, or timed out.
    ToolFinished {
        /// Tool name.
        tool: String,
        /// Whether the model receives an error output for this call.
        is_error: bool,
    },
    /// A budget ran out; only the terminal tools are on offer from here.
    WrappingUp {
        /// Which budget ran out.
        reason: String,
    },
}

/// A [`RunStep`] with the run's counters at the moment it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct RunProgress {
    /// What just happened.
    pub step: RunStep,
    /// Model turns started so far.
    pub model_turns: u32,
    /// The turn budget from [`AgentConfig`].
    pub max_model_turns: u32,
    /// Tool calls consumed so far.
    pub tool_calls: u32,
    /// The tool-call budget from [`AgentConfig`].
    pub max_tool_calls: u32,
    /// Tokens billed so far, as reported by the backend.
    pub usage: Usage,
}

/// Receives [`RunProgress`] as the run proceeds.
///
/// Deliberately synchronous and infallible: an observer is expected to hand the step to a channel
/// or a counter and return immediately. Progress reporting must never be able to block, fail, or
/// otherwise change how a run ends.
pub trait ProgressObserver: Send + Sync {
    /// Handles one progress step.
    fn observe(&self, progress: RunProgress);

    /// Sees every transcript entry the moment it is appended, with its index in the transcript.
    ///
    /// The initial items are observed too, so a live trace can show the run from its inputs on
    /// and end up equal to the stored transcript entry for entry. The default does nothing;
    /// observers that only count steps need not care.
    fn observe_item(&self, index: usize, entry: &TranscriptEntry) {
        let _ = (index, entry);
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
    /// A budget in [`AgentConfig`] was exhausted and the wrap-up turns did not produce a
    /// terminal result either.
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
    /// Model turns consumed, wrap-up turns included.
    pub model_turns: u32,
    /// Tool calls consumed (including refused and failed calls; budget refusals excluded).
    pub tool_calls: u32,
    /// Model requests that were retried after a transient backend failure.
    pub model_retries: u32,
    /// Token counts summed over every model request the run made, as the backend reported them.
    /// A run against a relay that reports no usage still counts its requests, so the gap is
    /// visible rather than silently priced at zero.
    pub usage: Usage,
    /// Whether a budget ran out and the model was asked to conclude in a wrap-up turn. A
    /// `Structured` outcome with this flag set was reached under pressure, not at the model's
    /// own pace; adapters may want to say so.
    pub wrapped_up: bool,
}

/// Runs the agent loop until a terminal result, a limit, or cancellation.
///
/// The contract with the model: unknown tools, malformed arguments, handler errors, and timeouts
/// come back as error tool-outputs it can read and recover from; a successful call of a tool whose
/// spec is `terminal` ends the run with that call's value; a turn with no tool calls ends the run
/// with the turn's text. When the tool budget gets low the model is told; when a budget is
/// exhausted the model gets its wrap-up turns with only the terminal tools on offer, and every
/// non-terminal call in those turns is refused. A transient backend failure is retried with
/// backoff inside the retry budget; a permanent one ends the run with an error. Every item is
/// appended to the transcript before the loop continues, so an aborted run is still fully
/// auditable.
pub async fn run_agent(
    client: &dyn ModelClient,
    registry: &ToolRegistry,
    config: &AgentConfig,
    instructions: &str,
    initial_items: Vec<Item>,
    cancel: CancelToken,
) -> HarnessResult<AgentRunReport> {
    run_agent_observed(
        client,
        registry,
        config,
        instructions,
        initial_items,
        cancel,
        None,
    )
    .await
}

/// [`run_agent`] with a [`ProgressObserver`] watching each step as it happens.
///
/// Same loop, same result; the observer only sees it happen. Adapters use this to stream progress
/// to operators while a run that takes minutes is still in flight.
pub async fn run_agent_observed(
    client: &dyn ModelClient,
    registry: &ToolRegistry,
    config: &AgentConfig,
    instructions: &str,
    initial_items: Vec<Item>,
    mut cancel: CancelToken,
    progress: Option<&dyn ProgressObserver>,
) -> HarnessResult<AgentRunReport> {
    let mut transcript = Transcript::new(instructions);
    // Every item goes through here, so the observer sees the transcript grow exactly as it is
    // written — including the inputs, so a live view starts where the stored one does.
    macro_rules! append {
        ($item:expr) => {{
            transcript.push($item);
            if let Some(observer) = progress {
                let index = transcript.entries.len() - 1;
                observer.observe_item(index, &transcript.entries[index]);
            }
        }};
    }
    for item in initial_items {
        append!(item);
    }
    let all_specs = registry.specs();
    let terminal_specs: Vec<ToolSpec> = all_specs
        .iter()
        .filter(|spec| spec.terminal)
        .cloned()
        .collect();
    let terminal_names = terminal_specs
        .iter()
        .map(|spec| format!("`{}`", spec.name))
        .collect::<Vec<_>>()
        .join(", ");

    let mut model_turns: u32 = 0;
    let mut tool_calls: u32 = 0;
    let mut model_retries: u32 = 0;
    let mut usage = Usage::default();
    let mut wrap_up_used: u32 = 0;
    let mut warned = false;
    let mut wrapped_up = false;
    // Set once a budget runs out; from then on every turn is a wrap-up turn.
    let mut exhausted: Option<String> = None;

    macro_rules! finish {
        ($outcome:expr) => {
            return Ok(AgentRunReport {
                outcome: $outcome,
                transcript,
                model_turns,
                tool_calls,
                model_retries,
                usage,
                wrapped_up,
            })
        };
    }

    macro_rules! report {
        ($step:expr) => {
            if let Some(observer) = progress {
                observer.observe(RunProgress {
                    step: $step,
                    model_turns,
                    max_model_turns: config.max_model_turns,
                    tool_calls,
                    max_tool_calls: config.max_tool_calls,
                    usage,
                });
            }
        };
    }

    loop {
        if cancel.is_cancelled() {
            finish!(AgentOutcome::Cancelled);
        }
        if exhausted.is_none() {
            if model_turns >= config.max_model_turns {
                exhausted = Some(format!(
                    "model-turn budget ({}) exhausted",
                    config.max_model_turns
                ));
            } else if tool_calls >= config.max_tool_calls {
                exhausted = Some(format!(
                    "tool-call budget ({}) exhausted",
                    config.max_tool_calls
                ));
            } else if config.max_total_tokens > 0 && usage.total_tokens() >= config.max_total_tokens
            {
                // Spending is bounded like every other resource, and by the same mechanism: the
                // model gets its wrap-up turn, so a run stopped on cost still concludes.
                exhausted = Some(format!(
                    "token budget ({} tokens) exhausted at {}",
                    config.max_total_tokens,
                    usage.total_tokens()
                ));
            }
        }
        let wrapping_up = exhausted.is_some();
        if let Some(reason) = &exhausted {
            if wrap_up_used >= config.wrap_up_turns || terminal_specs.is_empty() {
                finish!(AgentOutcome::LimitReached {
                    reason: reason.clone(),
                });
            }
            wrap_up_used += 1;
            wrapped_up = true;
            report!(RunStep::WrappingUp {
                reason: reason.clone(),
            });
            append!(Item::Notice {
                text: format!(
                    "{reason}. This is a wrap-up turn: only the terminal tool(s) {terminal_names} \
                     are available now. Call one immediately with your best conclusion from the \
                     evidence you already have; any other tool call will be refused."
                ),
            });
        }
        model_turns += 1;
        report!(RunStep::TurnStarted);

        let items = transcript.items();
        let offered = if wrapping_up {
            &terminal_specs
        } else {
            &all_specs
        };
        let request = ModelRequest {
            instructions,
            items: &items,
            tools: offered,
        };
        let turn_started_at = Utc::now();
        let retries_before = model_retries;
        let first_entry = transcript.entries.len();
        // Cancellation may arrive while the backend is thinking or while a retry waits; racing
        // keeps the loop honest about "cooperative" instead of waiting out a slow model call.
        let turn = loop {
            let attempt = tokio::select! {
                attempt = client.complete(request) => attempt,
                () = cancel.cancelled() => {
                    // The request was issued and then abandoned. Whether the backend billed it
                    // is unknowable from here, so it is counted as a request whose usage was
                    // never reported rather than as one that never happened.
                    usage += Usage::unreported();
                    finish!(AgentOutcome::Cancelled)
                }
            };
            match attempt {
                Ok(turn) => break turn,
                Err(HarnessError::ModelUnavailable(reason))
                    if model_retries < config.max_model_retries =>
                {
                    model_retries += 1;
                    report!(RunStep::Retrying {
                        attempt: model_retries,
                        reason,
                    });
                    let backoff = config.retry_backoff * model_retries;
                    tokio::select! {
                        () = tokio::time::sleep(backoff) => {}
                        () = cancel.cancelled() => finish!(AgentOutcome::Cancelled),
                    }
                }
                Err(error) => return Err(error),
            }
        };
        // The turn is billed whether or not it was usable, so the counters take it first.
        usage += turn.usage;
        transcript.record_turn(TurnRecord {
            turn: model_turns,
            started_at: turn_started_at,
            finished_at: Utc::now(),
            first_entry,
            usage: turn.usage,
            retries: model_retries - retries_before,
            wrap_up: wrapping_up,
            offered_tools: offered.iter().map(|spec| spec.name.clone()).collect(),
        });
        if turn.items.is_empty() {
            return Err(HarnessError::Model(
                "the backend returned an empty turn".into(),
            ));
        }

        let mut texts = Vec::new();
        let mut calls = Vec::new();
        for item in turn.items {
            match item {
                AssistantItem::Text { text } => {
                    append!(Item::AssistantText { text: text.clone() });
                    texts.push(text);
                }
                AssistantItem::ToolCall {
                    call_id,
                    tool,
                    arguments,
                } => {
                    append!(Item::ToolCall {
                        call_id: call_id.clone(),
                        tool: tool.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((call_id, tool, arguments));
                }
            }
        }

        report!(RunStep::TurnCompleted {
            tool_calls: calls.len() as u32,
            text: !texts.is_empty(),
        });

        if calls.is_empty() {
            finish!(AgentOutcome::Text(texts.join("\n")));
        }

        for (call_id, tool_name, arguments) in calls {
            if cancel.is_cancelled() {
                finish!(AgentOutcome::Cancelled);
            }
            macro_rules! refuse {
                ($message:expr) => {
                    append!(Item::ToolOutput {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        output: json!({ "error": $message }),
                        is_error: true,
                        trust: Trust::Trusted,
                    })
                };
            }
            report!(RunStep::ToolStarted {
                tool: tool_name.clone(),
            });
            macro_rules! finished {
                ($is_error:expr) => {
                    report!(RunStep::ToolFinished {
                        tool: tool_name.clone(),
                        is_error: $is_error,
                    })
                };
            }
            let tool = registry.get(&tool_name);
            if wrapping_up && !tool.is_some_and(|tool| tool.spec.terminal) {
                refuse!(format!(
                    "refused: the run is wrapping up and only the terminal tool(s) \
                     {terminal_names} may be called"
                ));
                finished!(true);
                continue;
            }
            if !wrapping_up && tool_calls >= config.max_tool_calls {
                // The budget ran out inside this turn; the remaining calls are refused and the
                // next turn is a wrap-up turn.
                exhausted = Some(format!(
                    "tool-call budget ({}) exhausted",
                    config.max_tool_calls
                ));
                refuse!(format!(
                    "refused: the tool-call budget ({}) is exhausted; call one of the terminal \
                     tool(s) {terminal_names} to conclude",
                    config.max_tool_calls
                ));
                finished!(true);
                continue;
            }
            tool_calls += 1;

            let Some(tool) = tool else {
                refuse!(format!(
                    "tool `{tool_name}` is not in the allowlist for this run"
                ));
                finished!(true);
                continue;
            };

            let executed =
                tokio::time::timeout(config.tool_timeout, tool.handler.call(arguments)).await;
            match executed {
                Err(_) => {
                    refuse!(format!(
                        "tool `{tool_name}` timed out after {:?}",
                        config.tool_timeout
                    ));
                    finished!(true);
                }
                Ok(Err(message)) => {
                    refuse!(message);
                    finished!(true);
                }
                Ok(Ok(value)) => {
                    let terminal = tool.spec.terminal;
                    append!(Item::ToolOutput {
                        call_id,
                        tool: tool_name.clone(),
                        output: value.clone(),
                        is_error: false,
                        trust: Trust::Mixed,
                    });
                    finished!(false);
                    if terminal {
                        finish!(AgentOutcome::Structured {
                            tool: tool_name,
                            value,
                        });
                    }
                }
            }

            // The warning goes out once, when the budget first reaches the threshold, so the
            // model can plan its remaining calls instead of being cut off mid-investigation.
            let remaining = config.max_tool_calls.saturating_sub(tool_calls);
            if !warned
                && config.warn_at_remaining_tool_calls > 0
                && remaining <= config.warn_at_remaining_tool_calls
                && !terminal_specs.is_empty()
            {
                warned = true;
                append!(Item::Notice {
                    text: format!(
                        "{remaining} tool call(s) remain in this run's budget. Plan to conclude: \
                         finish with one of the terminal tool(s) {terminal_names} before the budget \
                         runs out."
                    ),
                });
            }
        }
    }
}
