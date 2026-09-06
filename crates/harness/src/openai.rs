//! OpenAI-compatible HTTP backend for the harness.
//!
//! Works against the official API or any relay that speaks the same wire format (sub2api and
//! similar). Two wire formats are supported because relays differ: the Responses API (the
//! default) and Chat Completions. Both translate the harness's typed conversation into the
//! provider's shape and back into [`AssistantItem`]s; neither executes tools — that stays in the
//! loop so allowlisting, timeouts, and the transcript remain in one place.
//!
//! The API key is supplied by the caller (typically resolved from an environment variable by the
//! control plane's config loader); this module never reads files or the environment itself.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::{AssistantItem, ModelClient, ModelRequest, ModelTurn, Usage};
use crate::conversation::Item;
use crate::error::{HarnessError, HarnessResult};
use crate::tool::ToolSpec;

/// Which OpenAI-compatible endpoint shape to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    /// `POST {base_url}/responses` — the Responses API.
    #[default]
    Responses,
    /// `POST {base_url}/chat/completions` — Chat Completions with function tools.
    Chat,
}

/// Connection settings for one OpenAI-compatible backend.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Base URL including the API prefix, for example `https://api.thuics.icu/v1`.
    pub base_url: String,
    /// Model name sent with every request.
    pub model: String,
    /// Bearer token; never logged and never written to disk by the harness.
    pub api_key: String,
    /// Endpoint shape to use.
    pub wire_api: WireApi,
    /// Per-request timeout.
    pub timeout: Duration,
}

/// [`ModelClient`] over an OpenAI-compatible HTTP endpoint.
pub struct OpenAiClient {
    http: reqwest::Client,
    config: OpenAiConfig,
}

impl OpenAiClient {
    /// Builds a client; a base URL without a scheme is assumed to be HTTPS.
    pub fn new(mut config: OpenAiConfig) -> HarnessResult<Self> {
        if !config.base_url.contains("://") {
            config.base_url = format!("https://{}", config.base_url);
        }
        config.base_url = config.base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| HarnessError::Model(format!("http client build failed: {error}")))?;
        Ok(Self { http, config })
    }

    /// Returns the configured model name.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Returns the normalized base URL.
    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// Posts one JSON body and returns the parsed JSON response, mapping HTTP failures to errors.
    async fn post(&self, path: &str, body: &Value) -> HarnessResult<Value> {
        let url = format!("{}{}", self.config.base_url, path);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                // A connection failure or request timeout is transient by nature: nothing about
                // the request was judged. The loop retries these.
                HarnessError::ModelUnavailable(format!("request to {url} failed: {error}"))
            })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| HarnessError::Model(format!("reading response failed: {error}")))?;
        if !status.is_success() {
            let snippet: String = text.chars().take(600).collect();
            let message = format!("{url} returned HTTP {status}: {snippet}");
            return Err(if is_transient_status(status.as_u16()) {
                HarnessError::ModelUnavailable(message)
            } else {
                HarnessError::Model(message)
            });
        }
        serde_json::from_str(&text)
            .map_err(|error| HarnessError::Model(format!("invalid JSON from {url}: {error}")))
    }
}

/// HTTP statuses that mean "try again", not "the request was wrong": rate limits, timeouts, and
/// server-side failures. Everything else (400, 401, 404, ...) is permanent for this request.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

#[async_trait]
impl ModelClient for OpenAiClient {
    /// Sends the conversation in the configured wire format and parses the assistant turn.
    ///
    /// The response's `usage` block travels back with the items: it is the only place the real
    /// token counts exist, and dropping it here would make every figure upstream a guess.
    async fn complete(&self, request: ModelRequest<'_>) -> HarnessResult<ModelTurn> {
        let response = match self.config.wire_api {
            WireApi::Responses => {
                let body = responses_request(&self.config.model, request);
                self.post("/responses", &body).await?
            }
            WireApi::Chat => {
                let body = chat_request(&self.config.model, request);
                self.post("/chat/completions", &body).await?
            }
        };
        let items = match self.config.wire_api {
            WireApi::Responses => parse_responses_output(&response)?,
            WireApi::Chat => parse_chat_output(&response)?,
        };
        Ok(ModelTurn::with_usage(items, parse_usage(&response)))
    }
}

/// Reads the token counts out of a response body, in either wire format.
///
/// The Responses API names them `input_tokens`/`output_tokens`, Chat Completions
/// `prompt_tokens`/`completion_tokens`, and relays are not always consistent about which they
/// emit — so both spellings are accepted and the first one present wins. A body with no usable
/// `usage` object yields [`Usage::unreported`]: the request is counted, its tokens are not
/// invented.
pub fn parse_usage(response: &Value) -> Usage {
    let usage = &response["usage"];
    if !usage.is_object() {
        return Usage::unreported();
    }
    let count = |names: &[&str]| -> Option<u64> {
        names.iter().find_map(|name| {
            usage[*name]
                .as_u64()
                .or_else(|| usage[*name].as_f64().map(|v| v as u64))
        })
    };
    let input = count(&["input_tokens", "prompt_tokens"]);
    let output = count(&["output_tokens", "completion_tokens"]);
    if input.is_none() && output.is_none() {
        return Usage::unreported();
    }
    // Both formats nest the cache hit one level down, under differently named details objects.
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_tokens_details"]["cached_tokens"].as_u64())
        .or_else(|| usage["cached_tokens"].as_u64())
        .unwrap_or(0);
    let input = input.unwrap_or(0);
    Usage::reported(input, cached.min(input), output.unwrap_or(0))
}

/// Renders a harness notice as message text. Both wire formats carry it in the user role — the
/// only role every relay accepts mid-conversation — with a prefix that says who is speaking.
fn notice_text(text: &str) -> String {
    format!("[harness notice] {text}")
}

/// Encodes tool arguments the way both wire formats expect: a JSON string.
fn arguments_string(arguments: &Value) -> String {
    arguments.to_string()
}

/// Decodes an arguments string; unparseable text is kept verbatim so the tool handler can reject
/// it with a model-visible error instead of the harness failing.
fn parse_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Builds a Responses API request body.
pub fn responses_request(model: &str, request: ModelRequest<'_>) -> Value {
    let input: Vec<Value> = request
        .items
        .iter()
        .map(|item| match item {
            Item::UserInput { text, .. } => json!({ "role": "user", "content": text }),
            Item::Notice { text } => json!({ "role": "user", "content": notice_text(text) }),
            Item::AssistantText { text } => json!({ "role": "assistant", "content": text }),
            Item::ToolCall {
                call_id,
                tool,
                arguments,
            } => json!({
                "type": "function_call",
                "call_id": call_id,
                "name": tool,
                "arguments": arguments_string(arguments),
            }),
            Item::ToolOutput {
                call_id, output, ..
            } => json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output.to_string(),
            }),
        })
        .collect();
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|spec: &ToolSpec| {
            json!({
                "type": "function",
                "name": spec.name,
                "description": spec.description,
                "parameters": spec.parameters,
            })
        })
        .collect();
    json!({
        "model": model,
        "instructions": request.instructions,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "store": false,
    })
}

/// Parses a Responses API body into assistant items, ignoring reasoning and unknown item types.
pub fn parse_responses_output(response: &Value) -> HarnessResult<Vec<AssistantItem>> {
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        return Err(HarnessError::Model(format!("backend error: {error}")));
    }
    let output = response["output"]
        .as_array()
        .ok_or_else(|| HarnessError::Model("response has no `output` array".into()))?;
    let mut items = Vec::new();
    for entry in output {
        match entry["type"].as_str() {
            Some("message") => {
                let text: String = entry["content"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|part| part["type"] == "output_text")
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    items.push(AssistantItem::Text { text });
                }
            }
            Some("function_call") => {
                let raw = entry["arguments"].as_str().unwrap_or("{}");
                items.push(AssistantItem::ToolCall {
                    call_id: entry["call_id"].as_str().unwrap_or_default().to_string(),
                    tool: entry["name"].as_str().unwrap_or_default().to_string(),
                    arguments: parse_arguments(raw),
                });
            }
            _ => {}
        }
    }
    Ok(items)
}

/// Builds a Chat Completions request body.
///
/// Consecutive assistant text and tool calls from one turn are merged into a single assistant
/// message, which is the shape Chat Completions requires.
pub fn chat_request(model: &str, request: ModelRequest<'_>) -> Value {
    let mut messages = vec![json!({ "role": "system", "content": request.instructions })];
    let mut pending_text: Vec<String> = Vec::new();
    let mut pending_calls: Vec<Value> = Vec::new();

    let flush = |messages: &mut Vec<Value>, text: &mut Vec<String>, calls: &mut Vec<Value>| {
        if text.is_empty() && calls.is_empty() {
            return;
        }
        let mut message = json!({ "role": "assistant" });
        message["content"] = if text.is_empty() {
            Value::Null
        } else {
            Value::String(text.join("\n"))
        };
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls.clone());
        }
        messages.push(message);
        text.clear();
        calls.clear();
    };

    for item in request.items {
        match item {
            Item::AssistantText { text } => pending_text.push(text.clone()),
            Item::ToolCall {
                call_id,
                tool,
                arguments,
            } => pending_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": { "name": tool, "arguments": arguments_string(arguments) },
            })),
            Item::UserInput { text, .. } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls);
                messages.push(json!({ "role": "user", "content": text }));
            }
            Item::Notice { text } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls);
                messages.push(json!({ "role": "user", "content": notice_text(text) }));
            }
            Item::ToolOutput {
                call_id, output, ..
            } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output.to_string(),
                }));
            }
        }
    }
    flush(&mut messages, &mut pending_text, &mut pending_calls);

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|spec| {
            json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.parameters,
                },
            })
        })
        .collect();
    let mut body = json!({ "model": model, "messages": messages });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    body
}

/// Parses a Chat Completions body into assistant items.
pub fn parse_chat_output(response: &Value) -> HarnessResult<Vec<AssistantItem>> {
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        return Err(HarnessError::Model(format!("backend error: {error}")));
    }
    let message = &response["choices"][0]["message"];
    if message.is_null() {
        return Err(HarnessError::Model(
            "response has no `choices[0].message`".into(),
        ));
    }
    let mut items = Vec::new();
    if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
        items.push(AssistantItem::Text {
            text: text.to_string(),
        });
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        for call in calls {
            let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
            items.push(AssistantItem::ToolCall {
                call_id: call["id"].as_str().unwrap_or_default().to_string(),
                tool: call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                arguments: parse_arguments(raw),
            });
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Trust;

    fn sample_items() -> Vec<Item> {
        vec![
            Item::UserInput {
                text: "look".into(),
                trust: Trust::Trusted,
            },
            Item::AssistantText {
                text: "Reading.".into(),
            },
            Item::ToolCall {
                call_id: "c1".into(),
                tool: "read".into(),
                arguments: json!({"k": 1}),
            },
            Item::ToolOutput {
                call_id: "c1".into(),
                tool: "read".into(),
                output: json!({"ok": true}),
                is_error: false,
                trust: Trust::Mixed,
            },
        ]
    }

    fn sample_tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read".into(),
            description: "Reads".into(),
            parameters: json!({"type": "object", "properties": {}}),
            terminal: false,
        }]
    }

    #[test]
    fn responses_request_encodes_items_and_tools() {
        let items = sample_items();
        let tools = sample_tools();
        let body = responses_request(
            "gpt-test",
            ModelRequest {
                instructions: "sys",
                items: &items,
                tools: &tools,
            },
        );
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], "sys");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["arguments"], "{\"k\":1}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn responses_output_parses_text_and_calls_and_skips_reasoning() {
        let response = json!({
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Hello"}]},
                {"type": "function_call", "call_id": "x1", "name": "read",
                 "arguments": "{\"k\":2}"}
            ]
        });
        let items = parse_responses_output(&response).unwrap();
        assert_eq!(
            items,
            vec![
                AssistantItem::Text {
                    text: "Hello".into()
                },
                AssistantItem::ToolCall {
                    call_id: "x1".into(),
                    tool: "read".into(),
                    arguments: json!({"k": 2}),
                },
            ]
        );
    }

    #[test]
    fn chat_request_merges_assistant_turns_and_encodes_tool_messages() {
        let items = sample_items();
        let tools = sample_tools();
        let body = chat_request(
            "gpt-test",
            ModelRequest {
                instructions: "sys",
                items: &items,
                tools: &tools,
            },
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Reading.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn chat_output_parses_calls_and_keeps_bad_arguments_visible() {
        let response = json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "t1", "type": "function",
                    "function": {"name": "read", "arguments": "not json"}}]
            }}]
        });
        let items = parse_chat_output(&response).unwrap();
        assert_eq!(
            items,
            vec![AssistantItem::ToolCall {
                call_id: "t1".into(),
                tool: "read".into(),
                arguments: Value::String("not json".into()),
            }]
        );
    }

    #[test]
    fn backend_errors_surface_as_model_errors() {
        let response = json!({"error": {"message": "quota"}});
        assert!(matches!(
            parse_responses_output(&response),
            Err(HarnessError::Model(_))
        ));
        assert!(matches!(
            parse_chat_output(&response),
            Err(HarnessError::Model(_))
        ));
    }

    #[test]
    fn usage_is_read_from_either_wire_format_and_never_invented() {
        let responses = json!({
            "output": [],
            "usage": {
                "input_tokens": 1200,
                "output_tokens": 340,
                "input_tokens_details": {"cached_tokens": 1024}
            }
        });
        assert_eq!(parse_usage(&responses), Usage::reported(1200, 1024, 340));

        let chat = json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 90,
                "completion_tokens": 10,
                "prompt_tokens_details": {"cached_tokens": 64},
                "total_tokens": 100
            }
        });
        assert_eq!(parse_usage(&chat), Usage::reported(90, 64, 10));

        // A relay that reports nothing is counted as one request with unknown tokens, not as a
        // free one: a silent zero would understate the bill.
        let silent = parse_usage(&json!({"choices": []}));
        assert_eq!(silent.total_tokens(), 0);
        assert!(!silent.is_complete());
        assert_eq!(silent.requests, 1);
        assert!(!parse_usage(&json!({"usage": {"total_tokens": 7}})).is_complete());

        // A cache figure larger than the input it is a subset of cannot be trusted to exceed it.
        let odd = parse_usage(&json!({
            "usage": {"input_tokens": 10, "output_tokens": 1, "cached_tokens": 99}
        }));
        assert_eq!(odd.cached_input_tokens, 10);
    }

    #[test]
    fn scheme_and_trailing_slash_are_normalized() {
        let client = OpenAiClient::new(OpenAiConfig {
            base_url: "api.thuics.icu/v1/".into(),
            model: "m".into(),
            api_key: "k".into(),
            wire_api: WireApi::Responses,
            timeout: Duration::from_secs(5),
        })
        .unwrap();
        assert_eq!(client.base_url(), "https://api.thuics.icu/v1");
    }
}
