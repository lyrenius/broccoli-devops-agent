//! Distinguish a legitimate slow reply from an HTTP deadline, without a real model relay.
use axum::{Json, Router, routing::post};
use broccoli_agent_harness::openai::{OpenAiClient, OpenAiConfig, WireApi};
use broccoli_agent_harness::{ModelClient, ModelRequest, error::HarnessError};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn slow_reply_completes_within_deadline_and_longer_wait_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route("/responses", post(|| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Json(json!({"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}],"usage":{"input_tokens":1,"output_tokens":1}}))
    }));
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    for (timeout, succeeds) in [
        (Duration::from_secs(2), true),
        (Duration::from_millis(20), false),
    ] {
        let client = OpenAiClient::new(OpenAiConfig {
            base_url: base.clone(),
            model: "fixture".into(),
            api_key: "test-only".into(),
            wire_api: WireApi::Responses,
            timeout,
        })
        .unwrap();
        let outcome = client
            .complete(ModelRequest {
                instructions: "Reply OK",
                items: &[],
                tools: &[],
            })
            .await;
        if succeeds {
            assert!(outcome.is_ok());
        } else {
            assert!(matches!(outcome, Err(HarnessError::ModelUnavailable(_))));
        }
    }
    server.abort();
}
