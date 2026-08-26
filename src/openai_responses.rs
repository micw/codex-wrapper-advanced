//! Translation for `POST /v1/responses` — the OpenAI Responses API.
//!
//! Mapping only — no networking, no state. The handler lives in
//! [`crate::serve`], the upstream in [`crate::client`]. Chat Completions
//! ([`crate::openai_chat`]) stays next to this: it has the wider reach, this one
//! the better shape.
//!
//! # What it buys over Chat Completions
//!
//! 1. **Thinking is a typed item.** Chat has to press the reasoning summary into
//!    `reasoning_content`, an OpenRouter/DeepSeek dialect that is no part of the
//!    OpenAI spec. Here it is a `reasoning` item with `summary` parts — where the
//!    thinking belongs, and separate from the answer text.
//! 2. **Mixed turns.** The text item is closed before the `function_call` items
//!    open, and both end up in the envelope. Chat cannot express that: text and
//!    `tool_calls` share one delta object.
//! 3. **Reasoning replay.** The completed reasoning item goes out verbatim,
//!    `encrypted_content` included, so a client can hand it back on the next
//!    turn (MESSUNGEN.md §9).
//!
//! # Thinking is appended, never overwritten
//!
//! The progress arrives as `response.reasoning_summary_text.delta`, which every
//! client **appends** — the summary is real text here, so there is nothing to
//! replace. (The Claude wrapper has to overwrite one summary part in place,
//! because its CLI redacts the thinking and only a token count is left to show.)
//! `reasoning_summary_part.done` merely repeats the finished part; the running
//! text is never rewritten.
//!
//! # Thinking needs no configuration
//!
//! The summary is requested for every turn ([`crate::client::build_body`]), so
//! the thinking shows up without a client having to ask. `effort` only steers
//! *how much* is thought, and both spellings are accepted — `reasoning.effort`
//! and the Chat-Completions `reasoning_effort` — because clients send both.
//!
//! # Deliberately not supported: server-side state
//!
//! `previous_response_id` is rejected. Silently ignoring it would answer with
//! half the conversation missing — a wrong answer instead of an error. `store` is
//! accepted and ignored, because nothing is ever persisted here.

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::wire::Event;
use crate::wire::StreamRequest;
use crate::wire::Usage;

// --- Request direction ------------------------------------------------------

/// Request body of `POST /v1/responses`.
///
/// Permissive for the same reason as [`crate::openai_chat::ChatRequest`]:
/// clients attach knobs the backend has no counterpart for, and rejecting them
/// would make the endpoint unusable with stock SDKs. The exceptions are the
/// fields that would change the *meaning* of the turn — those are rejected
/// rather than ignored.
#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    /// A string, or ready-made items in the Responses shape.
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    /// The Chat-Completions spelling of `reasoning.effort`, accepted as a
    /// fallback.
    ///
    /// Not part of the Responses spec, but Open WebUI sends it: its
    /// `convert_to_responses_payload` rewrites messages, tools and
    /// `max_tokens`, and leaves `reasoning_effort` at the top level untouched.
    /// Ignoring it would silently drop the effort the user chose.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    /// Accepted and ignored — nothing is persisted here, so there is nothing to
    /// store.
    #[serde(default)]
    pub store: Option<bool>,
    /// Rejected in [`to_wire`]: we hold no conversation state.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Rejected in [`to_wire`]: without `store` there would be nothing to poll.
    #[serde(default)]
    pub background: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ReasoningConfig {
    /// Passed through verbatim; the backend rejects invalid values with a 400,
    /// which is a better error than guessing here. Without it the backend picks
    /// its own effort — the summary comes either way.
    #[serde(default)]
    pub effort: Option<String>,
    /// `auto` | `concise` | `detailed`. Ignored: [`crate::client::build_body`]
    /// asks for `auto`, and the server raises that on its own for the higher
    /// effort levels (MESSUNGEN.md §3).
    #[serde(default)]
    pub summary: Option<String>,
}

/// Responses request → wire [`StreamRequest`].
///
/// Barely a translation: the wire vocabulary already *is* the Responses shape.
/// The work is validation — and normalising the message content, because clients
/// send the Chat spellings (`text`, `image_url`) here too.
pub fn to_wire(req: &ResponsesRequest) -> Result<StreamRequest, String> {
    if req.previous_response_id.is_some() {
        return Err(
            "previous_response_id is not supported: this server keeps no state. \
             Send the whole conversation in `input`."
                .into(),
        );
    }
    if req.background == Some(true) {
        return Err(
            "background is not supported: nothing is stored, so there would be nothing to poll for"
                .into(),
        );
    }

    let input = match &req.input {
        Value::String(text) => crate::user_input(text),
        Value::Array(items) => items
            .iter()
            .map(normalise_item)
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("input must be a string or an array of items".into()),
    };
    if input.is_empty() {
        return Err("input is empty".into());
    }

    Ok(StreamRequest {
        model: req.model.clone(),
        input,
        instructions: req.instructions.clone(),
        tools: accepted_tools(req)?,
        effort: effort(req),
        tool_choice: req.tool_choice.clone(),
        parallel_tool_calls: req.parallel_tool_calls,
        store: Some(false),
        session_id: None,
    })
}

/// The reasoning effort, from either spelling. Without one the backend picks its
/// own; the summary is requested regardless.
fn effort(req: &ResponsesRequest) -> Option<String> {
    req.reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort.clone())
        .or_else(|| req.reasoning_effort.clone())
}

/// The tools that go upstream — and, unchanged, into the envelope.
///
/// Unsupported types are **rejected, not dropped**. Filtering them out while
/// echoing the request's raw `tools` back is the trap from KONTEXT-HARNESS.md §7:
/// the client believes its tool is registered and waits for a call that can never
/// come. A 400 says so immediately.
fn accepted_tools(req: &ResponsesRequest) -> Result<Option<Value>, String> {
    req.tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| match tool.get("type").and_then(Value::as_str) {
                    Some("function") => {
                        if tool
                            .get("name")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                        {
                            return Err("function tool is missing a name".to_string());
                        }
                        Ok(tool.clone())
                    }
                    // `web_search`, `file_search`, … would run inside OpenAI's
                    // infrastructure, which this server is not.
                    Some(other) => Err(format!(
                        "unsupported tool type {other:?}: only function tools are supported"
                    )),
                    None => Err("tool is missing its type".to_string()),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        })
        .transpose()
}

/// Normalises one `input` item.
///
/// Everything that is not a message — `function_call`, `function_call_output`
/// and above all `reasoning` — passes through **untouched**: the Responses shape
/// is the wire shape, and a reasoning item's `encrypted_content` is verified
/// server-side. Rebuilding it from fields would break the turn (MESSUNGEN.md §9).
fn normalise_item(item: &Value) -> Result<Value, String> {
    let Some(object) = item.as_object() else {
        return Err("input items must be objects".into());
    };
    let kind = object.get("type").and_then(Value::as_str);
    if kind.is_some_and(|kind| kind != "message") {
        return Ok(item.clone());
    }
    let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
    let content = object.get("content").unwrap_or(&Value::Null);
    Ok(json!({
        "type": "message",
        "role": role,
        "content": content_parts(content, role),
    }))
}

/// Message content → Responses content parts.
///
/// The text type follows the role: what the assistant said is `output_text`,
/// everything else `input_text`. An unknown part is passed on rather than
/// dropped — then the backend's error names it, instead of the turn quietly
/// losing content.
fn content_parts(content: &Value, role: &str) -> Value {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Value::String(text) => json!([{ "type": text_type, "text": text }]),
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .map(|part| match part.get("type").and_then(Value::as_str) {
                    // Chat spellings a client may still send.
                    Some("text") => json!({
                        "type": text_type,
                        "text": part.get("text").and_then(Value::as_str).unwrap_or(""),
                    }),
                    Some("image_url") => json!({
                        "type": "input_image",
                        "image_url": part.get("image_url")
                            .and_then(|image| image.get("url"))
                            .or_else(|| part.get("image_url"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    }),
                    _ => part.clone(),
                })
                .collect(),
        ),
        Value::Null => json!([{ "type": text_type, "text": "" }]),
        other => json!([{ "type": text_type, "text": other.to_string() }]),
    }
}

// --- Response direction -----------------------------------------------------

/// The item currently being streamed. While open it is *not* yet in `output`,
/// so its `output_index` is always `output.len()`.
#[derive(Debug)]
enum Open {
    None,
    Reasoning {
        id: String,
        /// The summary part currently filling.
        summary_index: usize,
        /// Its text so far.
        part: String,
        /// The parts already closed by a [`Event::ThinkingBreak`].
        done_parts: Vec<String>,
    },
    Message {
        id: String,
        text: String,
    },
}

/// Accumulates one turn and renders it as Responses events.
///
/// The same state serves both directions: streaming reads the events off
/// [`Self::apply`], non-streaming throws them away and reads [`Self::response`]
/// at the end. One accumulation path, no second implementation to drift.
#[derive(Debug)]
pub struct ResponsesState {
    id: String,
    /// The requested model, replaced by the routed one on [`Event::Started`].
    model: String,
    instructions: Option<String>,
    tools: Value,
    tool_choice: Value,
    parallel_tool_calls: bool,
    /// Every streaming event carries one; SDKs use it to detect gaps.
    sequence_number: u64,
    created: bool,
    output: Vec<Value>,
    open: Open,
    /// Where a reconstructed reasoning item was parked, so the authoritative one
    /// from upstream can overwrite that slot instead of landing next to it.
    reasoning_slot: Option<usize>,
    usage: Option<Usage>,
    status: &'static str,
    pub failed: Option<(String, bool)>,
}

impl ResponsesState {
    pub fn new(id: String, req: &ResponsesRequest) -> Self {
        Self {
            id,
            model: req.model.clone(),
            instructions: req.instructions.clone(),
            // The accepted set, not the raw request — see `accepted_tools`.
            tools: accepted_tools(req).ok().flatten().unwrap_or(json!([])),
            tool_choice: req.tool_choice.clone().unwrap_or(json!("auto")),
            parallel_tool_calls: req.parallel_tool_calls.unwrap_or(false),
            sequence_number: 0,
            created: false,
            output: Vec::new(),
            open: Open::None,
            reasoning_slot: None,
            usage: None,
            status: "completed",
            failed: None,
        }
    }

    /// One turn event → the Responses events it produces, as
    /// `(event name, JSON payload)`.
    pub fn apply(&mut self, event: &Event) -> Vec<(&'static str, String)> {
        if self.failed.is_some() {
            return Vec::new();
        }
        let model = match event {
            Event::Started { model } => model.as_deref(),
            _ => None,
        };
        let mut out = self.ensure_created(model);

        match event {
            Event::Started { model } => {
                if let Some(model) = model {
                    self.model = model.clone();
                }
            }

            Event::ThinkingDelta { text } => {
                out.extend(self.open_reasoning());
                let mut current = None;
                if let Open::Reasoning {
                    id,
                    summary_index,
                    part,
                    ..
                } = &mut self.open
                {
                    part.push_str(text);
                    current = Some((id.clone(), *summary_index));
                }
                if let Some((id, summary_index)) = current {
                    let index = self.output.len();
                    out.push(self.event(
                        "response.reasoning_summary_text.delta",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "summary_index": summary_index,
                            "delta": text,
                        }),
                    ));
                }
            }

            // Closes the current summary part and opens the next one. Nothing is
            // rewritten — the finished part keeps the text it has.
            Event::ThinkingBreak => {
                let mut current = None;
                if let Open::Reasoning {
                    id,
                    summary_index,
                    part,
                    done_parts,
                } = &mut self.open
                {
                    let finished = std::mem::take(part);
                    current = Some((id.clone(), *summary_index, finished.clone()));
                    done_parts.push(finished);
                    *summary_index += 1;
                }
                if let Some((id, summary_index, finished)) = current {
                    let index = self.output.len();
                    out.extend(self.finish_summary_part(&id, index, summary_index, &finished));
                    out.push(self.event(
                        "response.reasoning_summary_part.added",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "summary_index": summary_index + 1,
                            "part": { "type": "summary_text", "text": "" },
                        }),
                    ));
                }
            }

            // The authoritative reasoning item, `encrypted_content` included. It
            // replaces our reconstruction: replay depends on it going out
            // verbatim, so not a single byte is rebuilt here.
            Event::Reasoning { item, .. } => {
                if matches!(self.open, Open::Message { .. }) {
                    out.extend(self.close_open());
                }
                if let Open::Reasoning {
                    id,
                    summary_index,
                    part,
                    ..
                } = std::mem::replace(&mut self.open, Open::None)
                {
                    let index = self.output.len();
                    out.extend(self.finish_summary_part(&id, index, summary_index, &part));
                    out.push(self.event(
                        "response.output_item.done",
                        json!({ "output_index": index, "item": item }),
                    ));
                    self.output.push(item.clone());
                } else if let Some(index) = self.reasoning_slot {
                    // The text started before the item arrived, so our
                    // reconstruction is already parked. Overwrite that slot
                    // instead of adding a second reasoning item.
                    out.push(self.event(
                        "response.output_item.done",
                        json!({ "output_index": index, "item": item }),
                    ));
                    self.output[index] = item.clone();
                } else {
                    // Nothing was streamed — a reasoning item without a summary.
                    // It still goes out: without it the client cannot replay.
                    let index = self.output.len();
                    out.push(self.event(
                        "response.output_item.added",
                        json!({ "output_index": index, "item": item }),
                    ));
                    out.push(self.event(
                        "response.output_item.done",
                        json!({ "output_index": index, "item": item }),
                    ));
                    self.output.push(item.clone());
                }
                self.reasoning_slot = None;
            }

            Event::TextDelta { text } => {
                out.extend(self.open_message());
                let mut current = None;
                if let Open::Message { id, text: buffer } = &mut self.open {
                    buffer.push_str(text);
                    current = Some(id.clone());
                }
                if let Some(id) = current {
                    let index = self.output.len();
                    out.push(self.event(
                        "response.output_text.delta",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "content_index": 0,
                            "delta": text,
                        }),
                    ));
                }
            }

            Event::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                out.extend(self.close_open());
                let id = format!("fc_{}", uuid::Uuid::new_v4().simple());
                let index = self.output.len();
                // The arguments arrive complete, not in pieces. The delta is sent
                // anyway: clients that render the call live expect one, and one
                // whole delta is a legal stream.
                out.push(self.event(
                    "response.output_item.added",
                    json!({
                        "output_index": index,
                        "item": {
                            "type": "function_call",
                            "id": &id,
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress",
                        },
                    }),
                ));
                out.push(self.event(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": &id, "output_index": index, "delta": arguments }),
                ));
                out.push(self.event(
                    "response.function_call_arguments.done",
                    json!({ "item_id": &id, "output_index": index, "arguments": arguments }),
                ));
                let item = json!({
                    "type": "function_call",
                    "id": id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                });
                out.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": index, "item": item }),
                ));
                self.output.push(item);
            }

            Event::Done {
                stop_reason, usage, ..
            } => {
                out.extend(self.close_open());
                self.usage = usage.clone();
                self.status = if stop_reason == "aborted" {
                    "incomplete"
                } else {
                    "completed"
                };
                // The terminal event stays `response.completed` even when the
                // status inside says `incomplete`. The spec would call for
                // `response.incomplete`, but clients do not act on it — Open
                // WebUI's handler returns no metadata for it, so `usage` and the
                // done signal are lost and the message never finishes. Status and
                // `incomplete_details` are in the envelope either way.
                let response = self.envelope(self.status, None);
                out.push(self.event("response.completed", json!({ "response": response })));
            }

            Event::Failed { message, retryable } => {
                self.status = "failed";
                let error = json!({ "code": "upstream_error", "message": message });
                let response = self.envelope("failed", Some(error));
                out.push(self.event("response.failed", json!({ "response": response })));
                self.failed = Some((message.clone(), *retryable));
            }

            Event::RateLimits { .. } => {}
        }

        out
    }

    /// The final envelope for the non-streaming path.
    pub fn response(&self) -> Value {
        self.envelope(self.status, None)
    }

    // --- Item bookkeeping ---------------------------------------------------

    fn ensure_created(&mut self, model: Option<&str>) -> Vec<(&'static str, String)> {
        if self.created {
            return Vec::new();
        }
        self.created = true;
        if let Some(model) = model {
            self.model = model.to_string();
        }
        let response = self.envelope("in_progress", None);
        vec![
            self.event("response.created", json!({ "response": response })),
            self.event("response.in_progress", json!({ "response": response })),
        ]
    }

    fn open_reasoning(&mut self) -> Vec<(&'static str, String)> {
        if matches!(self.open, Open::Reasoning { .. }) {
            return Vec::new();
        }
        let mut out = self.close_open();
        let id = format!("rs_{}", uuid::Uuid::new_v4().simple());
        let index = self.output.len();
        out.push(self.event(
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": { "type": "reasoning", "id": &id, "summary": [] },
            }),
        ));
        out.push(self.event(
            "response.reasoning_summary_part.added",
            json!({
                "item_id": &id,
                "output_index": index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": "" },
            }),
        ));
        self.open = Open::Reasoning {
            id,
            summary_index: 0,
            part: String::new(),
            done_parts: Vec::new(),
        };
        self.reasoning_slot = None;
        out
    }

    fn open_message(&mut self) -> Vec<(&'static str, String)> {
        if matches!(self.open, Open::Message { .. }) {
            return Vec::new();
        }
        let mut out = self.close_open();
        let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let index = self.output.len();
        out.push(self.event(
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": {
                    "type": "message",
                    "id": &id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ));
        out.push(self.event(
            "response.content_part.added",
            json!({
                "item_id": &id,
                "output_index": index,
                "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] },
            }),
        ));
        self.open = Open::Message {
            id,
            text: String::new(),
        };
        out
    }

    fn close_open(&mut self) -> Vec<(&'static str, String)> {
        match std::mem::replace(&mut self.open, Open::None) {
            Open::None => Vec::new(),
            Open::Message { id, text } => {
                let index = self.output.len();
                let part = json!({ "type": "output_text", "text": text, "annotations": [] });
                let item = json!({
                    "type": "message",
                    "id": id,
                    "status": "completed",
                    "role": "assistant",
                    "content": [part],
                });
                let mut out = vec![
                    self.event(
                        "response.output_text.done",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "content_index": 0,
                            "text": text,
                        }),
                    ),
                    self.event(
                        "response.content_part.done",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "content_index": 0,
                            "part": part,
                        }),
                    ),
                ];
                out.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": index, "item": item }),
                ));
                self.output.push(item);
                out
            }
            Open::Reasoning {
                id,
                summary_index,
                part,
                mut done_parts,
            } => {
                let index = self.output.len();
                let mut out = self.finish_summary_part(&id, index, summary_index, &part);
                done_parts.push(part);
                // A stand-in until `Event::Reasoning` delivers the real item.
                // Carries no `encrypted_content` — it is for display, not replay.
                let item = json!({
                    "type": "reasoning",
                    "id": id,
                    "status": "completed",
                    "summary": done_parts.iter()
                        .map(|text| json!({ "type": "summary_text", "text": text }))
                        .collect::<Vec<_>>(),
                    "content": [],
                });
                out.push(self.event(
                    "response.output_item.done",
                    json!({ "output_index": index, "item": item }),
                ));
                self.output.push(item);
                self.reasoning_slot = Some(index);
                out
            }
        }
    }

    fn finish_summary_part(
        &mut self,
        id: &str,
        index: usize,
        summary_index: usize,
        text: &str,
    ) -> Vec<(&'static str, String)> {
        vec![
            self.event(
                "response.reasoning_summary_text.done",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "summary_index": summary_index,
                    "text": text,
                }),
            ),
            self.event(
                "response.reasoning_summary_part.done",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "summary_index": summary_index,
                    "part": { "type": "summary_text", "text": text },
                }),
            ),
        ]
    }

    // --- Rendering ----------------------------------------------------------

    fn event(&mut self, name: &'static str, mut payload: Value) -> (&'static str, String) {
        if let Some(map) = payload.as_object_mut() {
            map.insert("type".to_string(), json!(name));
            map.insert("sequence_number".to_string(), json!(self.sequence_number));
        }
        self.sequence_number += 1;
        (name, serde_json::to_string(&payload).unwrap_or_default())
    }

    /// `created_at` stays 0 for the same reason as `created` in
    /// [`crate::openai_chat`]: a moving timestamp would make otherwise identical
    /// responses differ.
    ///
    /// The `id` is ours, not the backend's `response_id`. It is announced in
    /// `response.created` before upstream reports one, and an id that changes
    /// mid-stream is worse than one that never matches an OpenAI dashboard.
    fn envelope(&self, status: &str, error: Option<Value>) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": 0,
            "status": status,
            "model": self.model,
            "output": self.output,
            "instructions": self.instructions,
            "tools": self.tools,
            "tool_choice": self.tool_choice,
            "parallel_tool_calls": self.parallel_tool_calls,
            "store": false,
            "metadata": {},
            "error": error,
            "incomplete_details": (status == "incomplete")
                .then(|| json!({ "reason": "aborted" })),
            "usage": self.usage.as_ref().map(usage_value),
        })
    }
}

fn usage_value(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": { "cached_tokens": usage.cached_input_tokens },
        "output_tokens": usage.output_tokens,
        "output_tokens_details": { "reasoning_tokens": usage.reasoning_output_tokens },
        "total_tokens": usage.total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: Value) -> ResponsesRequest {
        serde_json::from_value(body).unwrap()
    }

    fn state() -> ResponsesState {
        ResponsesState::new(
            "resp_test".to_string(),
            &request(json!({ "model": "m", "input": "hi" })),
        )
    }

    /// Collects the payloads of one event kind.
    fn payloads(lines: &[(&'static str, String)], name: &str) -> Vec<Value> {
        lines
            .iter()
            .filter(|(kind, _)| *kind == name)
            .map(|(_, data)| serde_json::from_str(data).unwrap())
            .collect()
    }

    #[test]
    fn string_input_becomes_a_user_message() {
        let wire = to_wire(&request(json!({ "model": "m", "input": "hi" }))).unwrap();
        assert_eq!(wire.input.len(), 1);
        assert_eq!(wire.input[0]["role"], "user");
        assert_eq!(wire.input[0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn reasoning_items_pass_through_untouched() {
        // Replay depends on `encrypted_content` arriving byte-identical.
        let item = json!({
            "type": "reasoning",
            "id": "rs_upstream",
            "summary": [{ "type": "summary_text", "text": "**Thinking**" }],
            "encrypted_content": "gAAAAA…",
        });
        let wire = to_wire(&request(json!({
            "model": "m",
            "input": [item.clone(), { "type": "message", "role": "user", "content": "go on" }],
        })))
        .unwrap();
        assert_eq!(wire.input[0], item);
        assert_eq!(wire.input[1]["content"][0]["type"], "input_text");
    }

    #[test]
    fn assistant_text_keeps_its_output_spelling() {
        let wire = to_wire(&request(json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": "hi" },
                { "type": "message", "role": "assistant", "content": "hello" },
            ],
        })))
        .unwrap();
        assert_eq!(wire.input[1]["content"][0]["type"], "output_text");
    }

    #[test]
    fn state_is_rejected_rather_than_ignored() {
        let err = to_wire(&request(json!({
            "model": "m",
            "input": "hi",
            "previous_response_id": "resp_1",
        })))
        .unwrap_err();
        assert!(err.contains("previous_response_id"));
    }

    #[test]
    fn built_in_tools_are_rejected_not_dropped() {
        let err = to_wire(&request(json!({
            "model": "m",
            "input": "hi",
            "tools": [{ "type": "web_search" }],
        })))
        .unwrap_err();
        assert!(err.contains("web_search"));
    }

    #[test]
    fn effort_comes_from_the_reasoning_object() {
        let wire = to_wire(&request(json!({
            "model": "m",
            "input": "hi",
            "reasoning": { "effort": "high", "summary": "detailed" },
        })))
        .unwrap();
        assert_eq!(wire.effort.as_deref(), Some("high"));
    }

    #[test]
    fn the_chat_spelling_of_effort_is_accepted_too() {
        // Open WebUI leaves `reasoning_effort` at the top level when it converts
        // a chat payload to the Responses shape. Without this the chosen effort
        // is silently lost.
        let wire = to_wire(&request(json!({
            "model": "m",
            "input": "hi",
            "reasoning_effort": "high",
        })))
        .unwrap();
        assert_eq!(wire.effort.as_deref(), Some("high"));

        // The Responses spelling wins where both are present.
        let wire = to_wire(&request(json!({
            "model": "m",
            "input": "hi",
            "reasoning": { "effort": "low" },
            "reasoning_effort": "high",
        })))
        .unwrap();
        assert_eq!(wire.effort.as_deref(), Some("low"));
    }

    #[test]
    fn thinking_is_appended_never_overwritten() {
        let mut state = state();
        let mut lines = state.apply(&Event::ThinkingDelta {
            text: "**Counting**".into(),
        });
        lines.extend(state.apply(&Event::ThinkingDelta {
            text: " items".into(),
        }));

        let deltas = payloads(&lines, "response.reasoning_summary_text.delta");
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0]["delta"], "**Counting**");
        assert_eq!(deltas[1]["delta"], " items");
        // Same part, so the client appends into it instead of replacing.
        assert_eq!(deltas[0]["summary_index"], deltas[1]["summary_index"]);
        // The item opens exactly once.
        assert_eq!(payloads(&lines, "response.output_item.added").len(), 1);
    }

    #[test]
    fn a_block_boundary_opens_the_next_summary_part() {
        let mut state = state();
        let mut lines = state.apply(&Event::ThinkingDelta {
            text: "first".into(),
        });
        lines.extend(state.apply(&Event::ThinkingBreak));
        lines.extend(state.apply(&Event::ThinkingDelta {
            text: "second".into(),
        }));

        let done = payloads(&lines, "response.reasoning_summary_part.done");
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["part"]["text"], "first");
        let added = payloads(&lines, "response.reasoning_summary_part.added");
        assert_eq!(added.len(), 2);
        assert_eq!(added[1]["summary_index"], 1);
        let deltas = payloads(&lines, "response.reasoning_summary_text.delta");
        assert_eq!(deltas[1]["summary_index"], 1);
    }

    #[test]
    fn the_upstream_reasoning_item_replaces_the_reconstruction() {
        let item = json!({
            "type": "reasoning",
            "id": "rs_upstream",
            "summary": [{ "type": "summary_text", "text": "thought" }],
            "encrypted_content": "gAAAAA…",
        });
        let mut state = state();
        state.apply(&Event::ThinkingDelta {
            text: "thought".into(),
        });
        let lines = state.apply(&Event::Reasoning {
            item: item.clone(),
            summary: vec!["thought".into()],
        });
        let done = payloads(&lines, "response.output_item.done");
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["item"], item);

        state.apply(&Event::Done {
            response_id: None,
            stop_reason: "end_turn".into(),
            usage: None,
        });
        // Exactly one reasoning item, and it is the one that can be replayed.
        assert_eq!(state.response()["output"], json!([item]));
    }

    #[test]
    fn a_late_reasoning_item_does_not_land_next_to_its_reconstruction() {
        let item = json!({ "type": "reasoning", "id": "rs_upstream", "summary": [] });
        let mut state = state();
        state.apply(&Event::ThinkingDelta {
            text: "thought".into(),
        });
        // Text first — that closes the reconstruction.
        state.apply(&Event::TextDelta {
            text: "answer".into(),
        });
        state.apply(&Event::Reasoning {
            item: item.clone(),
            summary: vec![],
        });
        state.apply(&Event::Done {
            response_id: None,
            stop_reason: "end_turn".into(),
            usage: None,
        });

        let output = state.response()["output"].clone();
        assert_eq!(output.as_array().unwrap().len(), 2);
        assert_eq!(output[0], item);
        assert_eq!(output[1]["type"], "message");
    }

    #[test]
    fn text_is_closed_before_a_tool_call_opens() {
        let mut state = state();
        state.apply(&Event::TextDelta {
            text: "let me look".into(),
        });
        let lines = state.apply(&Event::ToolCall {
            call_id: "call_1".into(),
            name: "get_weather".into(),
            arguments: "{\"city\":\"Köln\"}".into(),
        });
        let done = payloads(&lines, "response.output_item.done");
        assert_eq!(done[0]["item"]["type"], "message");
        assert_eq!(done[0]["item"]["content"][0]["text"], "let me look");
        assert_eq!(done[1]["item"]["type"], "function_call");
        assert_eq!(done[1]["item"]["call_id"], "call_1");
        // Both survive into the envelope — the mixed turn Chat cannot express.
        state.apply(&Event::Done {
            response_id: None,
            stop_reason: "end_turn".into(),
            usage: None,
        });
        assert_eq!(state.response()["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_abort_stays_a_completed_event_with_an_incomplete_status() {
        let mut state = state();
        state.apply(&Event::TextDelta {
            text: "half".into(),
        });
        let lines = state.apply(&Event::Done {
            response_id: None,
            stop_reason: "aborted".into(),
            usage: Some(Usage {
                total_tokens: Some(7),
                ..Default::default()
            }),
        });
        let completed = payloads(&lines, "response.completed");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0]["response"]["status"], "incomplete");
        assert_eq!(
            completed[0]["response"]["incomplete_details"]["reason"],
            "aborted"
        );
        assert_eq!(completed[0]["response"]["usage"]["total_tokens"], 7);
        assert!(payloads(&lines, "response.incomplete").is_empty());
    }

    #[test]
    fn a_failure_is_structured_and_not_a_success() {
        let mut state = state();
        let lines = state.apply(&Event::Failed {
            message: "upstream down".into(),
            retryable: true,
        });
        let failed = payloads(&lines, "response.failed");
        assert_eq!(failed[0]["response"]["error"]["message"], "upstream down");
        assert!(state.failed.is_some());
        // Nothing follows a failure.
        assert!(
            state
                .apply(&Event::TextDelta { text: "x".into() })
                .is_empty()
        );
    }

    #[test]
    fn the_sequence_number_counts_without_gaps() {
        let mut state = state();
        let mut lines = state.apply(&Event::Started {
            model: Some("routed".into()),
        });
        lines.extend(state.apply(&Event::TextDelta { text: "hi".into() }));
        let numbers: Vec<u64> = lines
            .iter()
            .map(|(_, data)| {
                serde_json::from_str::<Value>(data).unwrap()["sequence_number"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(numbers, (0..numbers.len() as u64).collect::<Vec<_>>());
        // The routed model, not the requested one.
        let created = payloads(&lines, "response.created");
        assert_eq!(created[0]["response"]["model"], "routed");
    }
}
