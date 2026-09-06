//! Behavior tests for the agent loop: termination, recovery, limits, and cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use broccoli_agent_harness::testing::{ScriptedModelClient, call, text};
use broccoli_agent_harness::{
    AgentConfig, AgentOutcome, HarnessError, Item, ProgressObserver, RunProgress, RunStep,
    ToolRegistry, ToolSpec, Transcript, TranscriptEntry, Trust, Usage, cancel_pair, run_agent,
    run_agent_observed, tool_fn,
};
use serde_json::json;

/// Registers an `add` tool and a terminal `finish` tool used across the tests.
fn registry(counter: Arc<AtomicU32>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(
            ToolSpec {
                name: "add".into(),
                description: "Adds a and b".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                    "required": ["a", "b"],
                }),
                terminal: false,
            },
            tool_fn(move |arguments| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let a = arguments["a"].as_f64().ok_or("`a` must be a number")?;
                    let b = arguments["b"].as_f64().ok_or("`b` must be a number")?;
                    Ok(json!({ "sum": a + b }))
                }
            }),
        )
        .unwrap();
    registry
        .register(
            ToolSpec {
                name: "finish".into(),
                description: "Submit the final structured answer".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                }),
                terminal: true,
            },
            tool_fn(|arguments| async move {
                if arguments["answer"].as_str().unwrap_or("").is_empty() {
                    return Err("`answer` must be a non-empty string".into());
                }
                Ok(arguments)
            }),
        )
        .unwrap();
    registry
}

/// A tool call followed by a terminal tool ends the run with the structured value.
#[tokio::test]
async fn terminal_tool_ends_the_run_with_structured_output() {
    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "add", json!({"a": 2, "b": 3}))],
        vec![
            text("The sum is 5."),
            call("c2", "finish", json!({"answer": "5"})),
        ],
    ]);
    let (_handle, token) = cancel_pair();

    let report = run_agent(
        &client,
        &registry(counter.clone()),
        &AgentConfig::default(),
        "Add the numbers, then finish.",
        vec![Item::UserInput {
            text: "2 + 3".into(),
            trust: Trust::Trusted,
        }],
        token,
    )
    .await
    .unwrap();

    assert_eq!(
        report.outcome,
        AgentOutcome::Structured {
            tool: "finish".into(),
            value: json!({"answer": "5"}),
        }
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(report.model_turns, 2);
    assert_eq!(report.tool_calls, 2);

    // The transcript replays: serialize, deserialize, and the items survive byte-for-byte.
    let encoded = serde_json::to_string(&report.transcript).unwrap();
    let decoded: Transcript = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, report.transcript);
    assert_eq!(decoded.entries.len(), 6); // input, call, output, text, call, output
}

/// A turn with no tool calls ends the run as prose.
#[tokio::test]
async fn plain_text_turn_ends_the_run() {
    let client = ScriptedModelClient::new(vec![vec![text("Nothing to do.")]]);
    let (_handle, token) = cancel_pair();
    let report = run_agent(
        &client,
        &ToolRegistry::new(),
        &AgentConfig::default(),
        "Do nothing.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert_eq!(report.outcome, AgentOutcome::Text("Nothing to do.".into()));
}

/// Unknown tools and handler errors come back as model-visible outputs, and the model recovers.
#[tokio::test]
async fn model_recovers_from_unknown_tool_and_bad_arguments() {
    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "reboot_machine", json!({}))],
        vec![call("c2", "add", json!({"a": "not-a-number", "b": 1}))],
        vec![call("c3", "finish", json!({"answer": "recovered"}))],
    ]);
    let (_handle, token) = cancel_pair();

    let report = run_agent(
        &client,
        &registry(counter),
        &AgentConfig::default(),
        "Recover from mistakes.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();

    assert!(matches!(report.outcome, AgentOutcome::Structured { .. }));
    let errors: Vec<_> = report
        .transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::ToolOutput {
                is_error: true,
                output,
                ..
            } => Some(output["error"].as_str().unwrap_or("").to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2);
    assert!(errors[0].contains("not in the allowlist"));
    assert!(errors[1].contains("must be a number"));
}

/// The turn budget stops a model that never terminates: after the budget it gets one wrap-up
/// turn with only the terminal tool on offer, its non-terminal call there is refused, and the
/// run ends as a limit.
#[tokio::test]
async fn turn_budget_stops_a_looping_model() {
    let counter = Arc::new(AtomicU32::new(0));
    let turns = (0..10)
        .map(|i| vec![call(format!("c{i}"), "add", json!({"a": 1, "b": 1}))])
        .collect();
    let client = ScriptedModelClient::new(turns);
    let (_handle, token) = cancel_pair();

    let config = AgentConfig {
        max_model_turns: 3,
        ..AgentConfig::default()
    };
    let report = run_agent(
        &client,
        &registry(counter.clone()),
        &config,
        "Loop.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert!(matches!(
        report.outcome,
        AgentOutcome::LimitReached { ref reason } if reason.contains("model-turn budget")
    ));
    assert_eq!(
        report.model_turns, 4,
        "three budgeted turns plus one wrap-up turn"
    );
    assert!(report.wrapped_up);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "the wrap-up call was refused, not run"
    );
    let offered = client.offered_tools().await;
    assert_eq!(offered[3], vec!["finish".to_string()]);
    let refusal = report
        .transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::ToolOutput {
                is_error: true,
                output,
                ..
            } => output["error"].as_str().map(ToString::to_string),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert!(refusal.contains("wrapping up"));

    // Without wrap-up turns the run stops the moment the budget runs out.
    let client = ScriptedModelClient::new(
        (0..10)
            .map(|i| vec![call(format!("c{i}"), "add", json!({"a": 1, "b": 1}))])
            .collect(),
    );
    let (_handle, token) = cancel_pair();
    let report = run_agent(
        &client,
        &registry(Arc::new(AtomicU32::new(0))),
        &AgentConfig {
            max_model_turns: 3,
            wrap_up_turns: 0,
            ..AgentConfig::default()
        },
        "Loop.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert!(matches!(report.outcome, AgentOutcome::LimitReached { .. }));
    assert_eq!(report.model_turns, 3);
    assert!(!report.wrapped_up);
}

/// A model that runs out of tool budget but concludes in its wrap-up turn still ends
/// structured — flagged as wrapped up so the adapter can say the conclusion was forced.
#[tokio::test]
async fn wrap_up_turn_lets_the_model_conclude() {
    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "add", json!({"a": 1, "b": 1}))],
        vec![call("c2", "add", json!({"a": 1, "b": 1}))],
        // Budget exhausted: the wrap-up turn offers only `finish`, and the model takes it.
        vec![call("c3", "finish", json!({"answer": "2, under pressure"}))],
    ]);
    let (_handle, token) = cancel_pair();
    let report = run_agent(
        &client,
        &registry(counter),
        &AgentConfig {
            max_tool_calls: 2,
            warn_at_remaining_tool_calls: 0,
            ..AgentConfig::default()
        },
        "Add, then finish.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert!(matches!(
        report.outcome,
        AgentOutcome::Structured { ref tool, .. } if tool == "finish"
    ));
    assert!(report.wrapped_up);
    assert_eq!(report.model_turns, 3);
    let offered = client.offered_tools().await;
    assert_eq!(offered[0], vec!["add".to_string(), "finish".to_string()]);
    assert_eq!(offered[2], vec!["finish".to_string()]);
    let notices: Vec<_> = report
        .transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::Notice { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("tool-call budget (2) exhausted"));
    assert!(notices[0].contains("wrap-up"));
}

/// The low-budget warning is issued exactly once, when the threshold is first reached, and
/// calls past the budget inside one turn are refused rather than executed.
#[tokio::test]
async fn low_budget_warning_is_issued_once_and_overflow_is_refused() {
    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "add", json!({"a": 1, "b": 1}))],
        vec![call("c2", "add", json!({"a": 1, "b": 1}))],
        // Three calls in one turn with one call of budget left: one runs, two are refused.
        vec![
            call("c3", "add", json!({"a": 1, "b": 1})),
            call("c4", "add", json!({"a": 1, "b": 1})),
            call("c5", "add", json!({"a": 1, "b": 1})),
        ],
        vec![call("c6", "finish", json!({"answer": "done"}))],
    ]);
    let (_handle, token) = cancel_pair();
    let report = run_agent(
        &client,
        &registry(counter.clone()),
        &AgentConfig {
            max_tool_calls: 3,
            warn_at_remaining_tool_calls: 1,
            ..AgentConfig::default()
        },
        "Add, then finish.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert!(matches!(report.outcome, AgentOutcome::Structured { .. }));
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    assert_eq!(
        report.tool_calls, 4,
        "three adds plus the terminal call; budget refusals are not executions"
    );
    let notices: Vec<_> = report
        .transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::Notice { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices.len(),
        2,
        "one low-budget warning, one wrap-up notice"
    );
    assert!(notices[0].contains("1 tool call(s) remain"));
    let refusals = report
        .transcript
        .entries
        .iter()
        .filter(|entry| {
            matches!(&entry.item, Item::ToolOutput { is_error: true, output, .. }
                if output["error"].as_str().unwrap_or("").contains("budget"))
        })
        .count();
    assert_eq!(refusals, 2);
}

/// Transient backend failures are retried within the retry budget; past it they are errors.
#[tokio::test]
async fn transient_backend_failures_are_retried() {
    let client = ScriptedModelClient::new(vec![vec![text("Fine now.")]]).with_transient_failures(2);
    let (_handle, token) = cancel_pair();
    let config = AgentConfig {
        max_model_retries: 2,
        retry_backoff: std::time::Duration::from_millis(1),
        ..AgentConfig::default()
    };
    let report = run_agent(
        &client,
        &ToolRegistry::new(),
        &config,
        "x",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert_eq!(report.outcome, AgentOutcome::Text("Fine now.".into()));
    assert_eq!(report.model_retries, 2);
    assert_eq!(report.model_turns, 1, "retries are not turns");

    let client = ScriptedModelClient::new(vec![vec![text("never")]]).with_transient_failures(3);
    let (_handle, token) = cancel_pair();
    let error = run_agent(
        &client,
        &ToolRegistry::new(),
        &config,
        "x",
        Vec::new(),
        token,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, HarnessError::ModelUnavailable(_)));
}

/// The tool-call budget counts refused calls too, so error loops cannot run forever.
#[tokio::test]
async fn tool_budget_counts_refused_calls() {
    let counter = Arc::new(AtomicU32::new(0));
    let turns = (0..5)
        .map(|i| vec![call(format!("c{i}"), "nope", json!({}))])
        .collect();
    let client = ScriptedModelClient::new(turns);
    let (_handle, token) = cancel_pair();

    let config = AgentConfig {
        max_tool_calls: 2,
        ..AgentConfig::default()
    };
    let report = run_agent(
        &client,
        &registry(counter),
        &config,
        "Fail.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert!(matches!(
        report.outcome,
        AgentOutcome::LimitReached { ref reason } if reason.contains("tool-call budget")
    ));
}

/// Cancellation between steps ends the run with a preserved transcript.
#[tokio::test]
async fn cancellation_is_honored_between_steps() {
    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![vec![call("c1", "add", json!({"a": 1, "b": 1}))]]);
    let (handle, token) = cancel_pair();
    handle.cancel();

    let report = run_agent(
        &client,
        &registry(counter),
        &AgentConfig::default(),
        "Never starts.",
        Vec::new(),
        token,
    )
    .await
    .unwrap();
    assert_eq!(report.outcome, AgentOutcome::Cancelled);
    assert_eq!(report.model_turns, 0);
}

/// An exhausted backend is a harness error, not a silent stop.
#[tokio::test]
async fn exhausted_backend_is_a_model_error() {
    let client = ScriptedModelClient::new(Vec::new());
    let (_handle, token) = cancel_pair();
    let error = run_agent(
        &client,
        &ToolRegistry::new(),
        &AgentConfig::default(),
        "x",
        Vec::new(),
        token,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, HarnessError::Model(_)));
}

/// Every request's token counts are summed into the report, and a relay that reports none is
/// recorded as a gap rather than as a free run.
#[tokio::test]
async fn usage_is_accumulated_across_turns_and_gaps_are_visible() {
    let counter = Arc::new(AtomicU32::new(0));
    let script = || {
        vec![
            vec![call("c1", "add", json!({"a": 2, "b": 3}))],
            vec![call("c2", "finish", json!({"answer": "5"}))],
        ]
    };
    let client =
        ScriptedModelClient::new(script()).with_usage_per_turn(Usage::reported(1_000, 800, 120));
    let (_handle, token) = cancel_pair();

    let report = run_agent(
        &client,
        &registry(counter.clone()),
        &AgentConfig::default(),
        "Add the numbers, then finish.",
        vec![Item::UserInput {
            text: "2 + 3".into(),
            trust: Trust::Trusted,
        }],
        token,
    )
    .await
    .unwrap();

    assert_eq!(report.usage.input_tokens, 2_000);
    assert_eq!(report.usage.cached_input_tokens, 1_600);
    assert_eq!(report.usage.output_tokens, 240);
    assert_eq!(report.usage.total_tokens(), 2_240);
    assert_eq!(report.usage.requests, 2);
    assert!(report.usage.is_complete());

    // The same run against a relay that reports nothing: requests counted, tokens unknown.
    let silent = ScriptedModelClient::new(script());
    let (_handle, token) = cancel_pair();
    let report = run_agent(
        &silent,
        &registry(counter),
        &AgentConfig::default(),
        "Add the numbers, then finish.",
        vec![Item::UserInput {
            text: "2 + 3".into(),
            trust: Trust::Trusted,
        }],
        token,
    )
    .await
    .unwrap();
    assert_eq!(report.usage.total_tokens(), 0);
    assert_eq!(report.usage.requests, 2);
    assert_eq!(report.usage.requests_without_usage, 2);
    assert!(!report.usage.is_complete());
}

/// A spent token budget stops the run through the wrap-up path, so it still concludes.
#[tokio::test]
async fn token_budget_ends_the_run_with_a_wrap_up_turn() {
    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "add", json!({"a": 1, "b": 1}))],
        vec![call("c2", "add", json!({"a": 2, "b": 2}))],
        vec![call("c3", "finish", json!({"answer": "out of budget"}))],
    ])
    .with_usage_per_turn(Usage::reported(400, 0, 100));
    let (_handle, token) = cancel_pair();

    let report = run_agent(
        &client,
        &registry(counter),
        &AgentConfig {
            // Two turns cost 1000 tokens; the third turn is only reached to wrap up.
            max_total_tokens: 900,
            ..AgentConfig::default()
        },
        "Keep adding.",
        vec![Item::UserInput {
            text: "go".into(),
            trust: Trust::Trusted,
        }],
        token,
    )
    .await
    .unwrap();

    assert!(report.wrapped_up, "the budget must force a wrap-up turn");
    assert_eq!(
        report.outcome,
        AgentOutcome::Structured {
            tool: "finish".into(),
            value: json!({"answer": "out of budget"}),
        },
        "a run stopped on cost still ends in a structured result"
    );
    // Only the terminal tool is on offer once the budget is spent.
    assert_eq!(client.offered_tools().await[2], vec!["finish".to_string()]);
    assert!(
        report
            .transcript
            .items()
            .iter()
            .any(|item| matches!(item, Item::Notice { text } if text.contains("token budget"))),
        "the model must be told which budget stopped it"
    );
}

/// The observer sees each turn and tool call while the run is still going, with live counters.
#[tokio::test]
async fn progress_is_observed_step_by_step() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<RunProgress>>);
    impl ProgressObserver for Recorder {
        fn observe(&self, progress: RunProgress) {
            self.0.lock().unwrap().push(progress);
        }
    }

    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "add", json!({"a": 2, "b": 3}))],
        vec![call("c2", "nope", json!({}))],
        vec![call("c3", "finish", json!({"answer": "5"}))],
    ])
    .with_usage_per_turn(Usage::reported(100, 0, 10));
    let (_handle, token) = cancel_pair();
    let recorder = Recorder::default();

    let report = run_agent_observed(
        &client,
        &registry(counter),
        &AgentConfig::default(),
        "Add the numbers, then finish.",
        vec![Item::UserInput {
            text: "2 + 3".into(),
            trust: Trust::Trusted,
        }],
        token,
        Some(&recorder),
    )
    .await
    .unwrap();

    let steps = recorder.0.lock().unwrap().clone();
    let names: Vec<&str> = steps
        .iter()
        .map(|progress| match &progress.step {
            RunStep::TurnStarted => "turn",
            RunStep::TurnCompleted { .. } => "turn-done",
            RunStep::ToolStarted { .. } => "tool",
            RunStep::ToolFinished { .. } => "tool-done",
            RunStep::Retrying { .. } => "retry",
            RunStep::WrappingUp { .. } => "wrap-up",
        })
        .collect();
    assert_eq!(
        names,
        vec![
            "turn",
            "turn-done",
            "tool",
            "tool-done", // add
            "turn",
            "turn-done",
            "tool",
            "tool-done", // unknown tool, refused
            "turn",
            "turn-done",
            "tool",
            "tool-done", // finish
        ]
    );
    assert!(
        matches!(&steps[7].step, RunStep::ToolFinished { tool, is_error } if tool == "nope" && *is_error),
        "a refused call is reported as finished with an error"
    );

    // Counters travel with each step, so a console can render progress without its own state.
    let first_turn = &steps[0];
    assert_eq!(first_turn.model_turns, 1);
    assert_eq!(
        first_turn.max_model_turns,
        AgentConfig::default().max_model_turns
    );
    assert_eq!(first_turn.usage.total_tokens(), 0, "nothing billed yet");
    let last = steps.last().unwrap();
    assert_eq!(last.usage.total_tokens(), 330);
    assert_eq!(last.usage, report.usage);
}

/// The observer sees every transcript entry as it is appended — inputs included — and the
/// stored transcript records each model request's timing, cost, retries, and offered tools.
#[tokio::test]
async fn every_entry_is_observed_and_every_turn_is_recorded() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(usize, TranscriptEntry)>>);
    impl ProgressObserver for Recorder {
        fn observe(&self, _progress: RunProgress) {}
        fn observe_item(&self, index: usize, entry: &TranscriptEntry) {
            self.0.lock().unwrap().push((index, entry.clone()));
        }
    }

    let counter = Arc::new(AtomicU32::new(0));
    let client = ScriptedModelClient::new(vec![
        vec![call("c1", "add", json!({"a": 2, "b": 3}))],
        vec![
            text("adding up"),
            call("c2", "finish", json!({"answer": "5"})),
        ],
    ])
    .with_usage_per_turn(Usage::reported(100, 10, 20))
    .with_transient_failures(1);
    let (_handle, token) = cancel_pair();
    let recorder = Recorder::default();

    let report = run_agent_observed(
        &client,
        &registry(counter),
        &AgentConfig {
            retry_backoff: Duration::from_millis(1),
            ..AgentConfig::default()
        },
        "Add the numbers, then finish.",
        vec![Item::UserInput {
            text: "2 + 3".into(),
            trust: Trust::Trusted,
        }],
        token,
        Some(&recorder),
    )
    .await
    .unwrap();

    // Observed entries are the transcript, entry for entry and index for index.
    let observed = recorder.0.lock().unwrap().clone();
    assert_eq!(observed.len(), report.transcript.entries.len());
    for (position, (index, entry)) in observed.iter().enumerate() {
        assert_eq!(*index, position);
        assert_eq!(entry, &report.transcript.entries[position]);
    }
    assert!(
        matches!(&observed[0].1.item, Item::UserInput { text, .. } if text == "2 + 3"),
        "the inputs are observed first"
    );

    // Two model requests, each with its own usage, the first answered after one retry.
    let turns = &report.transcript.turns;
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].turn, 1);
    assert_eq!(turns[0].retries, 1, "the transient failure was retried");
    assert_eq!(turns[1].retries, 0);
    assert_eq!(turns[0].usage, Usage::reported(100, 10, 20));
    assert!(turns[0].finished_at >= turns[0].started_at);
    assert_eq!(
        turns[0].first_entry, 1,
        "the first turn's items follow the input"
    );
    assert!(turns[1].first_entry > turns[0].first_entry);
    assert!(
        matches!(&report.transcript.entries[turns[1].first_entry].item, Item::AssistantText { text } if text == "adding up"),
        "first_entry points at the turn's first item"
    );
    assert!(turns.iter().all(|turn| !turn.wrap_up));
    assert!(turns[0].offered_tools.contains(&"add".to_string()));
    assert!(turns[0].offered_tools.contains(&"finish".to_string()));

    // The turn records survive the round trip through JSON, and an old transcript without
    // them still loads.
    let json = serde_json::to_string(&report.transcript).unwrap();
    let back: Transcript = serde_json::from_str(&json).unwrap();
    assert_eq!(back, report.transcript);
    let old: Transcript = serde_json::from_str(r#"{"instructions":"x","entries":[]}"#).unwrap();
    assert!(old.turns.is_empty());
}
