//! Translation for `POST /v1/chat/completions`.
//!
//! Mapping only — no networking, no state. The handlers live in
//! [`crate::serve`], the upstream in [`crate::client`]. Chat Completions is the
//! one protocol that presses the system prompt into the message list; the
//! Responses API keeps it separate, which is why `system` messages are collected
//! into `instructions` here.
//!
//! Streaming only: every consumer we care about (and the OpenAI SDK itself)
//! streams, and a non-streaming response would need a second accumulation path
//! for no measured benefit.

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::wire::Event;
use crate::wire::StreamRequest;

/// Request body of `POST /v1/chat/completions`.
///
/// Deliberately permissive: unknown fields are ignored (`#[serde(default)]`
/// everywhere), because clients attach a zoo of optional knobs (`temperature`,
/// `top_p`, `user`, …) the backend has no counterpart for. Rejecting them would
/// make the endpoint unusable with stock SDKs.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    /// `reasoning_effort` is the Chat-Completions spelling of what the wire API
    /// calls `effort`. Passed through verbatim; the backend rejects invalid
    /// values with a 400, which is a better error than guessing here.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// String in the classic shape, array of parts in the multimodal shape.
    /// Only the text is carried over; images have no path into the Responses
    /// subscription backend.
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Chat Completions request → wire [`StreamRequest`].
///
/// `system` messages become `instructions` (joined with `\n\n` when there are
/// several — the Responses API has exactly one instruction slot). Everything
/// else becomes an `input` item in message order:
///
/// | Chat | Responses |
/// |---|---|
/// | `user` | `message` / `input_text` |
/// | `assistant` (text) | `message` / `output_text` |
/// | `assistant` (`tool_calls`) | one `function_call` per call |
/// | `tool` | `function_call_output` |
pub fn to_wire(req: &ChatRequest) -> Result<StreamRequest, String> {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    let mut leading_instructions = true;
    for msg in &req.messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                if leading_instructions {
                    if let Some(text) = content_text(&msg.content) {
                        instructions.push(text);
                    }
                } else {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": format!("[{} instructions]\n{}", msg.role, content_text(&msg.content).unwrap_or_default()) }],
                    }));
                }
            }
            "user" => {
                leading_instructions = false;
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content_parts(&msg.content),
                }));
            }
            "assistant" => {
                leading_instructions = false;
                if let Some(text) = content_text(&msg.content)
                    && !text.is_empty()
                {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in msg.tool_calls.iter().flatten() {
                    // Shape per spec: {id, type: "function", function: {name, arguments}}.
                    // `arguments` stays a string — the Responses API wants that too.
                    let function = call.get("function").cloned().unwrap_or(Value::Null);
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                        "name": function.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": function.get("arguments").cloned().unwrap_or(json!("{}")),
                    }));
                }
            }
            "tool" => {
                leading_instructions = false;
                let Some(call_id) = msg.tool_call_id.as_deref().filter(|id| !id.is_empty()) else {
                    return Err("tool message is missing tool_call_id".into());
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": content_text(&msg.content).unwrap_or_default(),
                }));
            }
            other => return Err(format!("unsupported role {other:?}")),
        }
    }

    // Chat shape {type:"function", function:{…}} → Responses shape {type:"function", …}
    // (flat). Unsupported tool types are rejected locally so a request cannot
    // silently lose a capability before it reaches the backend.
    let tools = req
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    if t.get("type").and_then(Value::as_str) != Some("function") {
                        return Err(
                            "unsupported tool type; only function tools are supported".to_string()
                        );
                    }
                    let Some(f) = t.get("function").and_then(Value::as_object) else {
                        return Err("function tool is missing its function definition".to_string());
                    };
                    let Some(name) = f
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                    else {
                        return Err("function tool is missing a name".to_string());
                    };
                    Ok(json!({
                        "type": "function",
                        "name": name,
                        "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": f.get("parameters").cloned().unwrap_or(json!({})),
                        "strict": f.get("strict").and_then(Value::as_bool).unwrap_or(false),
                    }))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        })
        .transpose()?;

    Ok(StreamRequest {
        model: req.model.clone(),
        input,
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
        tools,
        effort: req.reasoning_effort.clone(),
        tool_choice: req.tool_choice.clone(),
        parallel_tool_calls: req.parallel_tool_calls,
        store: Some(false),
        session_id: None,
    })
}

/// Extracts the text from either content shape.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

fn content_parts(content: &Value) -> Value {
    match content {
        Value::String(text) => json!([{ "type": "input_text", "text": text }]),
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| json!({ "type": "input_text", "text": text })),
                    Some("image_url") => part
                        .get("image_url")
                        .and_then(|image| image.get("url"))
                        .map(|url| json!({ "type": "input_image", "image_url": url })),
                    _ => None,
                })
                .collect(),
        ),
        _ => json!([{ "type": "input_text", "text": "" }]),
    }
}

// --- Response direction ----------------------------------------------------

/// One SSE chunk in the Chat-Completions shape. `id`/`created`/`model` are
/// filled by the caller; only `choices` and optional `usage` differ per event.
fn chunk(id: &str, model: &str, choices: Value) -> String {
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": choices,
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

#[derive(Debug, Default)]
pub struct ChatResponseState {
    pub model: Option<String>,
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub usage: Option<crate::wire::Usage>,
    pub stop_reason: Option<String>,
    pub failed: Option<(String, bool)>,
}

impl ChatResponseState {
    fn output_model<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.model.as_deref().unwrap_or(fallback)
    }

    pub fn apply(
        &mut self,
        event: &Event,
        id: &str,
        fallback_model: &str,
        include_usage: bool,
    ) -> Vec<String> {
        if self.failed.is_some() {
            return Vec::new();
        }
        match event {
            Event::Started { model } => {
                self.model = model.clone();
                vec![chunk(
                    id,
                    self.output_model(fallback_model),
                    json!([{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]),
                )]
            }
            Event::TextDelta { text } => {
                self.text.push_str(text);
                vec![chunk(
                    id,
                    self.output_model(fallback_model),
                    json!([{ "index": 0, "delta": { "content": text }, "finish_reason": null }]),
                )]
            }
            Event::ThinkingDelta { text } => {
                self.reasoning.push_str(text);
                vec![chunk(
                    id,
                    self.output_model(fallback_model),
                    json!([{ "index": 0, "delta": { "reasoning_content": text }, "finish_reason": null }]),
                )]
            }
            // A block boundary becomes a paragraph break: `reasoning_content` is
            // one flat string and has no notion of parts. Before the first text
            // there is nothing to separate.
            Event::ThinkingBreak if !self.reasoning.is_empty() => {
                self.reasoning.push_str("\n\n");
                vec![chunk(
                    id,
                    self.output_model(fallback_model),
                    json!([{ "index": 0, "delta": { "reasoning_content": "\n\n" }, "finish_reason": null }]),
                )]
            }
            Event::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let index = self.tool_calls.len() as u64;
                self.tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }));
                vec![chunk(
                    id,
                    self.output_model(fallback_model),
                    json!([{ "index": 0, "delta": { "tool_calls": [{
                        "index": index,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }] }, "finish_reason": null }]),
                )]
            }
            Event::Done {
                stop_reason, usage, ..
            } => {
                self.stop_reason = Some(stop_reason.clone());
                self.usage = usage.clone();
                let finish = if self.tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                let mut out = vec![chunk(
                    id,
                    self.output_model(fallback_model),
                    json!([{ "index": 0, "delta": {}, "finish_reason": finish }]),
                )];
                if include_usage && let Some(usage) = usage {
                    out.push(usage_chunk(id, self.output_model(fallback_model), usage));
                }
                out.push("[DONE]".to_string());
                out
            }
            Event::Failed { message, retryable } => {
                self.failed = Some((message.clone(), *retryable));
                vec![serde_json::to_string(&json!({
                    "error": { "message": message, "type": "api_error", "code": "upstream_error" }
                })).unwrap_or_default(), "[DONE]".to_string()]
            }
            Event::RateLimits { .. } | Event::Reasoning { .. } | Event::ThinkingBreak => Vec::new(),
        }
    }

    pub fn response(&self, id: &str, requested_model: &str) -> Value {
        let model = self.output_model(requested_model);
        let (message, finish_reason) = if self.tool_calls.is_empty() {
            (json!({ "role": "assistant", "content": self.text }), "stop")
        } else {
            (
                json!({ "role": "assistant", "content": null, "tool_calls": self.tool_calls }),
                "tool_calls",
            )
        };
        json!({
            "id": id,
            "object": "chat.completion",
            "created": 0,
            "model": model,
            "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
            "usage": usage_value(self.usage.as_ref()),
        })
    }
}

fn usage_value(usage: Option<&crate::wire::Usage>) -> Value {
    let Some(usage) = usage else {
        return Value::Null;
    };
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
        "prompt_tokens_details": { "cached_tokens": usage.cached_input_tokens },
        "completion_tokens_details": { "reasoning_tokens": usage.reasoning_output_tokens },
    })
}

fn usage_chunk(id: &str, model: &str, usage: &crate::wire::Usage) -> String {
    serde_json::to_string(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [],
        "usage": usage_value(Some(usage)),
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_becomes_instructions() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
            ],
        }))
        .unwrap();
        let wire = to_wire(&req).unwrap();
        assert_eq!(wire.instructions.as_deref(), Some("be brief"));
        assert_eq!(wire.input.len(), 1);
        assert_eq!(wire.input[0]["role"], "user");
    }

    #[test]
    fn tool_round_trip() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Köln\"}" } },
                ]},
                { "role": "tool", "tool_call_id": "call_1", "content": "sunny" },
            ],
            "tools": [{ "type": "function", "function": { "name": "get_weather", "description": "d", "parameters": {"type":"object"} } }],
        }))
        .unwrap();
        let wire = to_wire(&req).unwrap();
        assert_eq!(wire.input.len(), 3);
        assert_eq!(wire.input[1]["type"], "function_call");
        assert_eq!(wire.input[1]["call_id"], "call_1");
        assert_eq!(wire.input[2]["type"], "function_call_output");
        assert_eq!(wire.input[2]["output"], "sunny");
        let tools = wire.tools.unwrap();
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["type"], "function");
    }

    #[test]
    fn preserves_tool_controls_and_strict() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "use it" }],
            "tool_choice": { "type": "function", "function": { "name": "f" } },
            "parallel_tool_calls": true,
            "tools": [{ "type": "function", "function": {
                "name": "f", "parameters": {}, "strict": true
            }}]
        }))
        .unwrap();
        let wire = to_wire(&req).unwrap();
        assert_eq!(wire.tool_choice.unwrap()["type"], "function");
        assert_eq!(wire.parallel_tool_calls, Some(true));
        assert_eq!(wire.tools.unwrap()[0]["strict"], true);
    }

    #[test]
    fn keeps_later_instructions_in_order() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "first" },
                { "role": "user", "content": "one" },
                { "role": "developer", "content": "later" },
                { "role": "user", "content": "two" }
            ]
        }))
        .unwrap();
        let wire = to_wire(&req).unwrap();
        assert_eq!(wire.instructions.as_deref(), Some("first"));
        assert_eq!(wire.input[1]["role"], "user");
        assert_eq!(
            wire.input[1]["content"][0]["text"],
            "[developer instructions]\nlater"
        );
    }

    #[test]
    fn streams_independent_tool_calls_and_usage_only_when_requested() {
        let mut state = ChatResponseState::default();
        let first = state.apply(
            &Event::Started {
                model: Some("routed".into()),
            },
            "id",
            "requested",
            true,
        );
        assert!(first[0].contains("routed"));
        state.apply(
            &Event::ToolCall {
                call_id: "a".into(),
                name: "one".into(),
                arguments: "{}".into(),
            },
            "id",
            "requested",
            true,
        );
        let second = state.apply(
            &Event::ToolCall {
                call_id: "b".into(),
                name: "two".into(),
                arguments: "{}".into(),
            },
            "id",
            "requested",
            true,
        );
        assert!(second[0].contains("\"index\":1"));
        let done = state.apply(
            &Event::Done {
                response_id: None,
                stop_reason: "end_turn".into(),
                usage: Some(crate::wire::Usage {
                    total_tokens: Some(1),
                    ..Default::default()
                }),
            },
            "id",
            "requested",
            true,
        );
        assert!(done[0].contains("tool_calls"));
        assert!(done.iter().any(|line| line.contains("\"choices\":[]")));
        assert_eq!(
            state.response("id", "requested")["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[test]
    fn stream_failure_is_structured_and_not_success() {
        let mut state = ChatResponseState::default();
        let lines = state.apply(
            &Event::Failed {
                message: "upstream down".into(),
                retryable: true,
            },
            "id",
            "m",
            false,
        );
        assert!(lines[0].contains("\"error\""));
        assert!(!lines[0].contains("chat.completion.chunk"));
        assert!(state.failed.is_some());
    }
}
