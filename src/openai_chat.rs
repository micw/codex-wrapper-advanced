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
    /// `reasoning_effort` is the Chat-Completions spelling of what the wire API
    /// calls `effort`. Passed through verbatim; the backend rejects invalid
    /// values with a 400, which is a better error than guessing here.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub stream: Option<bool>,
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

    for msg in &req.messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(text) = content_text(&msg.content) {
                    instructions.push(text);
                }
            }
            "user" => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": content_text(&msg.content).unwrap_or_default() }],
            })),
            "assistant" => {
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
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": msg.tool_call_id.clone().unwrap_or_default(),
                "output": content_text(&msg.content).unwrap_or_default(),
            })),
            other => return Err(format!("unsupported role {other:?}")),
        }
    }

    // Chat shape {type:"function", function:{…}} → Responses shape {type:"function", …}
    // (flat). Unknown tool types are dropped rather than rejected: the backend
    // would refuse them, and a client sending e.g. web_search hints should not
    // lose the whole request over it.
    let tools = req.tools.as_ref().map(|tools| {
        Value::Array(
            tools
                .iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
                .map(|t| {
                    let f = t.get("function").cloned().unwrap_or(Value::Null);
                    json!({
                        "type": "function",
                        "name": f.get("name").cloned().unwrap_or(Value::Null),
                        "description": f.get("description").cloned().unwrap_or(json!("")),
                        "parameters": f.get("parameters").cloned().unwrap_or(json!({})),
                        "strict": false,
                    })
                })
                .collect(),
        )
    });

    Ok(StreamRequest {
        model: req.model.clone(),
        input,
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
        tools,
        effort: req.reasoning_effort.clone(),
        tool_choice: None,
        parallel_tool_calls: None,
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

/// Maps a wire [`Event`] to zero or more Chat-Completions SSE lines.
///
/// Tool calls are emitted fragmented (index-based), because that is what the
/// protocol specifies and what clients accumulate — wyai does exactly that.
pub fn from_wire(event: &Event, id: &str, model: &str) -> Vec<String> {
    match event {
        Event::Started { .. } => vec![chunk(
            id,
            model,
            json!([{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]),
        )],
        Event::TextDelta { text } => vec![chunk(
            id,
            model,
            json!([{ "index": 0, "delta": { "content": text }, "finish_reason": null }]),
        )],
        // Chat Completions has no first-class thinking; `reasoning_content` is
        // the de-facto standard (DeepSeek & friends) and wyai understands it.
        Event::ThinkingDelta { text } => vec![chunk(
            id,
            model,
            json!([{ "index": 0, "delta": { "reasoning_content": text }, "finish_reason": null }]),
        )],
        Event::ToolCall {
            call_id,
            name,
            arguments,
        } => vec![chunk(
            id,
            model,
            json!([{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }] },
                "finish_reason": null,
            }]),
        )],
        Event::Done {
            stop_reason, usage, ..
        } => {
            let finish = match stop_reason.as_str() {
                "aborted" => "stop",
                _ => "stop",
            };
            let mut out = vec![chunk(
                id,
                model,
                json!([{ "index": 0, "delta": {}, "finish_reason": finish }]),
            )];
            if let Some(usage) = usage {
                let payload = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": model,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": usage.input_tokens,
                        "completion_tokens": usage.output_tokens,
                        "total_tokens": usage.total_tokens,
                        "prompt_tokens_details": { "cached_tokens": usage.cached_input_tokens },
                        "completion_tokens_details": { "reasoning_tokens": usage.reasoning_output_tokens },
                    },
                });
                out.push(serde_json::to_string(&payload).unwrap_or_default());
            }
            out.push("[DONE]".to_string());
            out
        }
        // Rate limits and replay reasoning have no Chat-Completions counterpart.
        // Dropping them is the honest mapping — inventing fields would chain the
        // surface to this daemon's internals.
        Event::RateLimits { .. } | Event::Reasoning { .. } => Vec::new(),
        Event::Failed { message, .. } => {
            // Mid-stream there is no status code anymore; the error becomes a
            // chunk so the client sees *something* rather than a silent end.
            vec![
                chunk(
                    id,
                    model,
                    json!([{ "index": 0, "delta": { "content": format!("\n[error] {message}") }, "finish_reason": null }]),
                ),
                chunk(
                    id,
                    model,
                    json!([{ "index": 0, "delta": {}, "finish_reason": "stop" }]),
                ),
                "[DONE]".to_string(),
            ]
        }
    }
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
    fn done_maps_to_finish_and_usage() {
        let event = Event::Done {
            response_id: Some("r".into()),
            stop_reason: "end_turn".into(),
            usage: Some(crate::wire::Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                ..Default::default()
            }),
        };
        let chunks = from_wire(&event, "id", "m");
        assert_eq!(chunks.last().unwrap(), "[DONE]");
        assert!(chunks[0].contains("\"finish_reason\":\"stop\""));
        assert!(chunks[1].contains("\"prompt_tokens\":10"));
    }
}
