//! Behavior tests for the agent loop: termination, recovery, limits, and cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use broccoli_agent_harness::testing::{ScriptedModelClient, call, text};
use broccoli_agent_harness::{
    AgentConfig, AgentOutcome, HarnessError, Item, ToolRegistry, ToolSpec, Transcript, Trust,
    cancel_pair, run_agent, tool_fn,
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

/// The turn budget stops a model that never terminates.
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
        &registry(counter),
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
    assert_eq!(report.model_turns, 3);
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
