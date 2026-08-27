//! codex-api-wrapper — the ChatGPT subscription via the official Codex crates.
//!
//! The library is the actual product; the binary next to it is a thin shell.
//! Two consumers are intended:
//!
//! * the CLI (`codex-api-wrapper ask|models|login`) for exploratory testing,
//! * the daemon (`codex-api-wrapper serve`) as a local REST API for other
//!   processes to talk to.
//!
//! # Layering
//!
//! ```text
//!   wire    — the vocabulary spoken outward, provider-neutral
//!   client  — Codex types -> wire; knowledge of codex-api ends here
//!   auth    — official OAuth flow, own CODEX_HOME
//!   listen  — transport and access control
//!   openai  — wire -> OpenAI format (pure translation)
//!   metrics — what the running process observed; reads wire, writes nothing
//!   serve   — REST API on top of client + auth
//! ```
//!
//! The rule that matters for maintenance: **no Codex type appears above
//! `client`.** That keeps how the backend is reached replaceable without
//! consumers noticing.
//!
//! # What is deliberately missing
//!
//! `codex-core`. Upstream keeps the login and responses paths separate from it,
//! so the entire agent machinery (tools, sandbox, rollout, session store) drops
//! out. See the README.

pub mod auth;
pub mod client;
pub mod limits;
pub mod listen;
pub mod metrics;
pub mod models;
pub mod openai_chat;
pub mod openai_responses;
pub mod serve;
pub mod wire;

/// Default model.
///
/// Measured 2026-08-24: the `gpt-5.x-codex` slugs from the prompt files in the
/// upstream repo no longer exist. The backend serves gpt-5.6-{sol,terra,luna},
/// gpt-5.5, gpt-5.4{,-mini}. Run `models` before changing this.
pub const DEFAULT_MODEL: &str = "gpt-5.6-sol";

/// Appended to `/models` as the `client_version` query parameter. Taken from the
/// most recent upstream release tag, because the repo checkout itself carries
/// `0.0.0` (the real version is only substituted at release time).
pub const DEFAULT_CLIENT_VERSION: &str = "0.150.0";

/// Builds an `input` array holding a single user message.
///
/// The daemon translates nothing — it accepts ready-made Responses items. This
/// helper exists for the CLI and for tests so the most common case does not have
/// to be spelled out every time.
pub fn user_input(text: &str) -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }]
    })]
}
