#![cfg(feature = "openai")]

use broccoli_agent_harness::ToolSpec;
use broccoli_agent_harness::context::{GenerationConfig, estimate_input};
use broccoli_agent_harness::openai::{OpenAiClient, OpenAiConfig, WireApi};
use broccoli_agent_harness::{HarnessError, Item, ModelClient, ModelRequest, Trust};
use serde_json::json;
use std::{net::TcpListener, time::Duration};

fn client(wire_api: WireApi, generation: GenerationConfig, base_url: String) -> OpenAiClient {
    OpenAiClient::new(OpenAiConfig {
        model: "gpt-4o".into(),
        api_key: "local-test".into(),
        wire_api,
        generation,
        base_url,
        timeout: Duration::from_secs(2),
    })
    .unwrap()
}

#[test]
fn both_protocols_receive_reasoning_and_output_limits() {
    for wire in [WireApi::Responses, WireApi::Chat] {
        let options = GenerationConfig {
            context_window_tokens: 8192,
            max_output_tokens: None,
            reasoning_effort: "low".into(),
        };
        let client = client(wire, options, "http://127.0.0.1:1".into());
        let request = ModelRequest {
            instructions: "system",
            items: &[],
            tools: &[],
        };
        let body = client.request_body(request).unwrap();
        match wire {
            WireApi::Responses => {
                assert_eq!(body["reasoning"]["effort"], "low");
                assert_eq!(body["max_output_tokens"], 4096);
            }
            WireApi::Chat => {
                assert_eq!(body["reasoning_effort"], "low");
                assert_eq!(body["max_completion_tokens"], 4096);
            }
        }
        let original = super_client(wire).request_body(request).unwrap();
        assert!(original.get("reasoning").is_none());
        assert!(original.get("reasoning_effort").is_none());
        assert!(original.get("max_output_tokens").is_none());
        assert!(original.get("max_completion_tokens").is_none());
    }
}

fn super_client(wire: WireApi) -> OpenAiClient {
    client(
        wire,
        GenerationConfig::default(),
        "http://127.0.0.1:1".into(),
    )
}

#[tokio::test]
async fn overflow_retains_input_and_never_reaches_http() {
    for wire in [WireApi::Responses, WireApi::Chat] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let options = GenerationConfig {
            context_window_tokens: 100,
            max_output_tokens: Some(1),
            ..Default::default()
        };
        let client = client(
            wire,
            options,
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let items = vec![Item::UserInput {
            text: "中文现场排查".repeat(200),
            trust: Trust::Trusted,
        }];
        let before = items.clone();
        let err = client
            .complete(ModelRequest {
                instructions: "full system",
                items: &items,
                tools: &[],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::ContextLimit(_)));
        assert!(err.to_string().contains("history retained"));
        assert_eq!(items, before);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn tools_and_tool_results_are_included_in_preflight() {
    let tools = vec![ToolSpec {
        name: "inspect".into(),
        description: "Detailed schema description ".repeat(100),
        parameters: json!({"type":"object"}),
        terminal: false,
    }];
    let items = vec![Item::ToolOutput {
        call_id: "c1".into(),
        tool: "inspect".into(),
        output: json!({"log":"中文日志".repeat(1000)}),
        is_error: false,
        trust: Trust::Mixed,
    }];
    for wire in [WireApi::Responses, WireApi::Chat] {
        let full = super_client(wire)
            .request_body(ModelRequest {
                instructions: "instructions",
                items: &items,
                tools: &tools,
            })
            .unwrap();
        let small = super_client(wire)
            .request_body(ModelRequest {
                instructions: "instructions",
                items: &[],
                tools: &[],
            })
            .unwrap();
        assert!(estimate_input("gpt-4o", &full).0 > estimate_input("gpt-4o", &small).0 + 1000);
        let (unknown, method) = estimate_input("local-unknown-model", &full);
        assert!(unknown >= full.to_string().len() as u64);
        assert!(method.contains("unknown model tokenizer"));
    }
}

#[test]
fn contradictory_limits_are_rejected() {
    assert!(
        GenerationConfig {
            context_window_tokens: 4096,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        GenerationConfig {
            max_output_tokens: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        GenerationConfig {
            reasoning_effort: " ".into(),
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}
