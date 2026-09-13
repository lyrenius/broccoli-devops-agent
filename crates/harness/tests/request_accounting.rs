#![cfg(feature = "openai")]

use async_trait::async_trait;
use broccoli_agent_harness::openai::{OpenAiClient, OpenAiConfig, WireApi};
use broccoli_agent_harness::{
    AgentConfig, AgentOutcome, HarnessError, HarnessResult, ModelClient, ModelRequest, ModelTurn,
    RequestObserver, RequestRecord, RequestStatus, RunObservers, ToolRegistry, Usage, cancel_pair,
    run_agent_recorded,
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Recorder(Mutex<Vec<RequestRecord>>);
#[async_trait]
impl RequestObserver for Recorder {
    async fn record(&self, record: &RequestRecord) -> HarnessResult<()> {
        self.0.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[tokio::test]
async fn usage_survives_an_unparseable_assistant_response_in_both_protocols() {
    for wire in [WireApi::Responses, WireApi::Chat] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                let raw = String::from_utf8_lossy(&bytes);
                if let Some(end) = raw.find("\r\n\r\n") {
                    let length = raw[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body = r#"{"usage":{"input_tokens":1000,"output_tokens":100}}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let raw = String::from_utf8(bytes).unwrap();
            let payload = raw.split("\r\n\r\n").nth(1).unwrap();
            serde_json::from_str::<serde_json::Value>(payload).unwrap()
        });
        let client = OpenAiClient::new(OpenAiConfig {
            base_url: format!("http://{addr}"),
            model: "test-model".into(),
            api_key: "local-test".into(),
            wire_api: wire,
            timeout: Duration::from_secs(2),
            generation: broccoli_agent_harness::context::GenerationConfig {
                reasoning_effort: "low".into(),
                ..Default::default()
            },
        })
        .unwrap();
        let observer = Recorder::default();
        let (_handle, token) = cancel_pair();
        let report = run_agent_recorded(
            &client,
            &ToolRegistry::new(),
            &AgentConfig::default(),
            "test",
            vec![],
            token,
            RunObservers {
                progress: None,
                requests: Some(&observer),
            },
        )
        .await
        .unwrap();
        assert!(matches!(report.outcome, AgentOutcome::Failed { .. }));
        assert_eq!(report.usage, Usage::reported(1000, 0, 100));
        {
            let records = observer.0.lock().unwrap();
            assert_eq!(records.len(), 2);
            assert_eq!(records[1].status, RequestStatus::Failed);
            assert_eq!(records[1].usage.unwrap().total_tokens(), 1100);
        }
        let sent = server.await.unwrap();
        match wire {
            WireApi::Responses => assert_eq!(sent["reasoning"]["effort"], "low"),
            WireApi::Chat => assert_eq!(sent["reasoning_effort"], "low"),
        }
    }
}

struct CountingClient {
    calls: Arc<AtomicUsize>,
    refuse: bool,
}
#[async_trait]
impl ModelClient for CountingClient {
    fn validate_request(&self, _: ModelRequest<'_>) -> HarnessResult<()> {
        if self.refuse {
            Err(HarnessError::ContextLimit("local refusal".into()))
        } else {
            Ok(())
        }
    }
    async fn complete(&self, _: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelTurn::new(vec![]))
    }
}

#[tokio::test]
async fn local_preflight_refusal_creates_no_api_attempt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let recorder = Recorder::default();
    let (_handle, token) = cancel_pair();
    let report = run_agent_recorded(
        &CountingClient {
            calls: calls.clone(),
            refuse: true,
        },
        &ToolRegistry::new(),
        &AgentConfig::default(),
        "test",
        vec![],
        token,
        RunObservers {
            progress: None,
            requests: Some(&recorder),
        },
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(report.usage.requests, 0);
    assert!(recorder.0.lock().unwrap().is_empty());
    assert_eq!(report.transcript.failure.unwrap().stage, "context");
}

#[tokio::test]
async fn cancellation_during_the_start_write_is_recorded_as_not_sent() {
    struct StopOnStart {
        recorder: Recorder,
        handle: Mutex<Option<broccoli_agent_harness::CancelHandle>>,
    }
    #[async_trait]
    impl RequestObserver for StopOnStart {
        async fn record(&self, record: &RequestRecord) -> HarnessResult<()> {
            self.recorder.record(record).await?;
            if record.status == RequestStatus::Started {
                self.handle.lock().unwrap().take().unwrap().cancel();
            }
            Ok(())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (handle, token) = cancel_pair();
    let observer = StopOnStart {
        recorder: Recorder::default(),
        handle: Mutex::new(Some(handle)),
    };
    let report = run_agent_recorded(
        &CountingClient {
            calls: calls.clone(),
            refuse: false,
        },
        &ToolRegistry::new(),
        &AgentConfig::default(),
        "test",
        vec![],
        token,
        RunObservers {
            progress: None,
            requests: Some(&observer),
        },
    )
    .await
    .unwrap();
    assert_eq!(report.outcome, AgentOutcome::Cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(report.usage.requests, 0);
    assert_eq!(
        observer.recorder.0.lock().unwrap()[1].status,
        RequestStatus::NotSent
    );
}

#[test]
fn partial_or_invalid_usage_is_marked_incomplete() {
    let partial =
        broccoli_agent_harness::openai::parse_usage(&json!({"usage":{"input_tokens":1000}}));
    assert_eq!(partial.input_tokens, 1000);
    assert_eq!(partial.output_tokens, 0);
    assert_eq!(partial.requests_without_usage, 1);
    let invalid = broccoli_agent_harness::openai::parse_usage(
        &json!({"usage":{"input_tokens":-1,"output_tokens":2.5}}),
    );
    assert_eq!(invalid.total_tokens(), 0);
    assert_eq!(invalid.requests_without_usage, 1);
}
