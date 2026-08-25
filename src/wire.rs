//! The vocabulary the daemon speaks outward.
//!
//! **Deliberately provider-neutral, not tied to any one consumer.** If this
//! carried some specific client's event names, the daemon would be chained to
//! that client and the option of putting other surfaces on top would be gone.
//! Mapping to a consumer's own types is that consumer's job — for the
//! OpenAI-compatible surface it happens in [`crate::openai`].
//!
//! The fields mirror what the backend actually sends (measured, see
//! MESSUNGEN.md), not what a consumer might wish for.

use serde::Deserialize;
use serde::Serialize;

/// Token usage. Everything optional — absence is a value, not a default.
///
/// A missing number is never guessed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

/// One window of the subscription quota.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

/// Events of a running turn.
///
/// `tag = "type"` makes dispatch on the consumer side a trivial lookup of
/// `event["type"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The server accepted the turn.
    ///
    /// `response_id` is not available yet — the Responses API only reports it in
    /// the completion event, which is why it lives in [`Event::Done`].
    Started {
        /// May differ from the requested model (safety routing). `None` when the
        /// server did not report anything different.
        model: Option<String>,
    },
    TextDelta {
        text: String,
    },
    /// Reasoning summary in plain text. Unlike Claude's, it is not redacted
    /// (MESSUNGEN.md §3) — hence no `redacted` flag here.
    ThinkingDelta {
        text: String,
    },
    /// A tool call for **the client** to execute. `arguments` is a string
    /// containing JSON, not a parsed object — that is how the Responses API
    /// delivers it, and re-parsing would lose information when the JSON is
    /// malformed.
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Subscription quota. Arrives once per turn, before the first text.
    RateLimits {
        plan: Option<String>,
        primary: Option<RateLimitWindow>,
        secondary: Option<RateLimitWindow>,
    },
    Done {
        response_id: Option<String>,
        /// `end_turn` | `aborted` — deliberately narrow. Nothing else has been
        /// observed from the backend so far.
        stop_reason: String,
        usage: Option<Usage>,
    },
    /// Separate from `Done`, because `response.failed` and a clean ending are two
    /// different cases (KONTEXT-HARNESS.md §7).
    Failed {
        message: String,
        retryable: bool,
    },
}

/// Request body for `POST /wire/v1/responses`.
///
/// Deliberately thin: `instructions` and `tools` are unrestricted, because the
/// backend does not constrain them (MESSUNGEN.md §1/§2). `tools` is raw JSON and
/// passed through verbatim — the daemon has no opinion on tool schemas.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamRequest {
    pub model: String,
    /// Ready-made `input` items of the Responses API. The caller builds them; no
    /// translation happens here.
    pub input: Vec<serde_json::Value>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub tool_choice: Option<String>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub store: Option<bool>,
    /// Sets the `session-id` header; the backend derives its `prompt_cache_key`
    /// from it. Cache control therefore rests with the caller.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Auth state for `GET /wire/v1/auth`.
#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub workspace_account: bool,
    pub fedramp: bool,
}

/// Written to stdout on startup so the parent process learns port and token.
/// Pattern borrowed from `codex-responses-api-proxy`.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub port: u16,
    pub pid: u32,
    /// Bearer token, minted at startup. See [`crate::serve`] for the reasoning.
    pub token: String,
}
