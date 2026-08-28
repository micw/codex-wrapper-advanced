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
    /// Boundary between two thinking blocks.
    ///
    /// The backend delivers the summary in several parts, each a titled
    /// paragraph (MESSUNGEN.md §3). Without the boundary they run together into
    /// one line — `**First title**…**Second title**`. Carries no text: a
    /// consumer with no notion of blocks ignores the event and loses nothing but
    /// the paragraph break.
    ThinkingBreak,
    /// A tool call for **the client** to execute. `arguments` is a string
    /// containing JSON, not a parsed object — that is how the Responses API
    /// delivers it, and re-parsing would lose information when the JSON is
    /// malformed.
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// A completed reasoning item — the replay payload for follow-up turns.
    ///
    /// Measured (MESSUNGEN.md §9): the backend **verifies** `encrypted_content`
    /// cryptographically. A single altered byte yields
    /// `400 … could not be verified`. That is why `item` is passed through as
    /// opaque JSON instead of being rebuilt from individual fields — whoever
    /// replays it echoes it unchanged.
    ///
    /// Leaving it out is allowed and free: a turn without a reasoning item is
    /// accepted, and one without `encrypted_content` demonstrably contributes
    /// nothing (identical token count).
    Reasoning {
        /// To be returned to `input` unchanged. Do not write into it.
        item: serde_json::Value,
        /// Plain-text summary, already extracted for display.
        summary: Vec<String>,
    },
    /// Subscription quota. Arrives **once** per turn, before the first text.
    ///
    /// Built from the response headers, not from the upstream events: those come
    /// one per header family and carry no group identity, which makes two of them
    /// indistinguishable on a model with its own quota. See
    /// [`crate::limits`] for the projection and the one rule behind it.
    RateLimits(crate::limits::TurnLimits),
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
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub store: Option<bool>,
    /// The prompt cache key: sets the `session-id` header and the body field of
    /// the same name.
    ///
    /// **Optional, and absence is not neutral.** Left out, one is derived from
    /// the invariant head of the conversation
    /// ([`crate::client::cache_key`]) — because a request without a key does not
    /// route to the machine holding its prefix and therefore almost never hits
    /// the cache. Whoever names one overrides that; the only requirement is that
    /// the value stays **the same across the turns of a conversation**. Its shape
    /// is free — measured, the backend accepts arbitrary strings.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Auth state for `GET /wire/v1/auth`.
///
/// Never contains a token — only **whether** and **as whom** we are signed in,
/// plus the deadlines. The latter make the refresh cycle observable instead of
/// noticing it only when it fails (DEPLOY.md §1).
#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub workspace_account: bool,
    pub fedramp: bool,

    /// Access token expiry (RFC 3339). Measured lifetime: 10 days. `codex-login`
    /// refreshes 5 minutes before that — but only when someone calls `auth()`;
    /// there is no background timer.
    pub access_token_expires_at: Option<String>,
    /// Seconds until then. Negative means already expired, and the next call
    /// triggers the refresh.
    pub access_token_expires_in_seconds: Option<i64>,
    /// Timestamp of the last successful refresh (RFC 3339).
    ///
    /// **The genuinely interesting value.** Once its distance from now exceeds
    /// the token lifetime, a refresh is either due or has failed. The *refresh*
    /// token's own lifetime cannot be determined from outside (opaque,
    /// server-side) — observing is the only means.
    pub last_refresh: Option<String>,
    pub last_refresh_age_seconds: Option<i64>,
}

/// Response of `GET /ready`.
///
/// Separate from `/health`, and that is not cosmetic: tie **liveness** to the
/// sign-in state and Kubernetes will kill the container in a loop before anyone
/// can exec in and sign in. `/health` therefore only says "the process is
/// alive", `/ready` says "it can work".
///
/// Needs no key — a probe cannot send one. It therefore carries operational
/// state only, no identity.
#[derive(Debug, Clone, Serialize)]
pub struct ReadyStatus {
    pub ready: bool,
    /// `ok` | `not_authenticated` | `refresh_failed` | `token_expired`
    pub reason: &'static str,
    /// The upstream's own wording on `refresh_failed` — it says whether the token
    /// expired, was already used, or was revoked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token_expires_in_seconds: Option<i64>,
}

/// Response of `GET /wire/v1/info` — what a consumer is talking to.
///
/// Deliberately narrow. The **contract** version already sits in the path
/// (`/wire/v1`); a field for it here would be a second truth about the same
/// thing. And capabilities belong where they apply: what the models can do is
/// in `/wire/v1/models`, how full the quota is in `/wire/v1/usage`. A third
/// place summarising both would be a copy that goes stale.
///
/// `version` is the release version. A consumer branching on it branches on the
/// wrong thing — that is what the path version is for. It is good for logs, bug
/// reports and the question whether a deployment already runs the new build.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceInfo {
    pub service: &'static str,
    pub version: &'static str,
}

impl ServiceInfo {
    /// Both values come from `Cargo.toml` via `env!`, so they cannot drift from
    /// the crate they describe — the failure mode a hand-kept constant has.
    pub fn current() -> Self {
        Self {
            service: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

impl Default for ServiceInfo {
    fn default() -> Self {
        Self::current()
    }
}

/// Written to stdout on startup so the parent process knows where to connect.
/// Pattern borrowed from `codex-responses-api-proxy`.
///
/// Carries **no secret**: access depends on the unix socket's permissions or on
/// configured API keys, not on anything stated here.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    /// `unix:/path` or `http://127.0.0.1:8080`.
    pub listen: String,
    pub pid: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The narrowness **is** the contract: two fields, and anything growing here
    /// has to be a decision rather than an oversight.
    #[test]
    fn service_info_stays_narrow() {
        let value = serde_json::to_value(ServiceInfo::current()).expect("serialises");
        let object = value.as_object().expect("an object");
        assert_eq!(
            object.len(),
            2,
            "info carries service and version, nothing else"
        );
        assert_eq!(object["service"], "codex-api-wrapper");
        assert!(
            !object["version"].as_str().unwrap_or_default().is_empty(),
            "version comes from Cargo.toml and can never be empty"
        );
    }
}
