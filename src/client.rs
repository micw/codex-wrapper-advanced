//! Request path against `chatgpt.com/backend-api/codex`.
//!
//! Translates `codex_api::ResponseEvent` into the neutral vocabulary of
//! [`crate::wire`]. Knowledge of the Codex types lives only here; everything
//! above (daemon, CLI) sees `wire::Event` exclusively.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use codex_api::AuthError;
use codex_api::AuthHeadersFuture;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider;
use codex_api::ResponseEvent;
use codex_api::ResponsesClient;
use codex_api::RetryConfig;
use codex_api::SharedAuthProvider;
use codex_http_client::HttpTransport;
use codex_http_client::RequestBody;
use codex_http_client::ReqwestTransport;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use futures::Stream;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use serde_json::json;

use crate::wire::Event;
use crate::wire::RateLimitWindow;
use crate::wire::StreamRequest;
use crate::wire::Usage;

/// Base URL of the subscription backend. `ResponsesClient` appends `responses`.
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

// --- Auth provider ---------------------------------------------------------

/// Connects the `AuthManager` to `codex_api`'s `AuthProvider` trait.
///
/// Fetches the token on *every* request via `AuthManager::auth()` instead of
/// snapshotting it once. In a long-lived daemon that is the difference between
/// "runs" and "falls over after an hour": a refresh takes effect immediately for
/// all further requests, because they all share the same `AuthManager`.
struct ManagedChatGptAuth {
    manager: Arc<AuthManager>,
}

fn apply_auth_headers(auth: &CodexAuth, headers: &mut HeaderMap) {
    if let Ok(token) = auth.get_token()
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(http::header::AUTHORIZATION, value);
    }
    if let Some(account_id) = auth.get_account_id()
        && let Ok(value) = HeaderValue::from_str(&account_id)
    {
        headers.insert("ChatGPT-Account-ID", value);
    }
    if auth.is_fedramp_account() {
        headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
    }
}

impl AuthProvider for ManagedChatGptAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        // Synchronous path (telemetry): cache only, no refresh.
        if let Some(auth) = self.manager.auth_cached() {
            apply_auth_headers(&auth, headers);
        }
    }

    fn resolve_auth_headers(&self) -> AuthHeadersFuture<'_> {
        Box::pin(async move {
            let auth = self
                .manager
                .auth()
                .await
                .ok_or_else(|| AuthError::Build("not signed in".to_string()))?;
            let mut headers = HeaderMap::new();
            apply_auth_headers(&auth, &mut headers);
            if !headers.contains_key(http::header::AUTHORIZATION) {
                return Err(AuthError::Build(
                    "no bearer token available (no ChatGPT login?)".to_string(),
                ));
            }
            Ok(headers)
        })
    }
}

// --- Provider / transport --------------------------------------------------

/// Retries are off.
///
/// A 429 here is information for the caller (quota exhausted), not a glitch for
/// the transport to paper over. Whoever wants to retry sees the `Failed` event
/// with `retryable` and decides for themselves.
fn provider() -> Provider {
    Provider {
        name: "chatgpt".to_string(),
        base_url: CHATGPT_BASE_URL.to_string(),
        query_params: None,
        // Supplies the user agent and `originator: codex_cli_rs` from upstream.
        headers: default_client::default_headers(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(500),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(300),
    }
}

fn transport() -> ReqwestTransport {
    ReqwestTransport::from_http_client(default_client::create_client())
}

// --- Request body ----------------------------------------------------------

/// Builds the Responses body from a [`StreamRequest`].
///
/// `instructions` and `tools` pass through unchanged — the backend constrains
/// neither (MESSUNGEN.md §1/§2). When `instructions` is absent the field is
/// omitted rather than set to `null`: "no system prompt" is a different statement
/// from "empty system prompt".
pub fn build_body(req: &StreamRequest) -> Value {
    let mut body = json!({
        "model": req.model,
        "input": req.input,
        "tool_choice": req
            .tool_choice
            .clone()
            .unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": req.parallel_tool_calls.unwrap_or(false),
        "store": req.store.unwrap_or(false),
        "stream": true,
        // Without `store` the client has to hand reasoning items back itself,
        // and that needs the encrypted content.
        "include": ["reasoning.encrypted_content"],
    });

    let map = body.as_object_mut().expect("object");
    if let Some(instructions) = &req.instructions {
        map.insert("instructions".to_string(), json!(instructions));
    }
    // Empty array rather than `null`: "this client has no tools" is a different
    // statement from "the field is missing".
    map.insert(
        "tools".to_string(),
        req.tools.clone().unwrap_or_else(|| json!([])),
    );
    if let Some(effort) = &req.effort {
        map.insert(
            "reasoning".to_string(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }
    body
}

// --- Errors ----------------------------------------------------------------

/// Failure while starting a turn, carrying the upstream's state.
///
/// Exists so a 400 from the backend reaches the caller as a 400 — wording
/// included. `format!("{err:?}")` on `ApiError` would be fatal here: it produces
/// a debug dump including every response header and buries the actual message in
/// it. Exactly the "silently swallowed" class that KONTEXT-HARNESS.md §7
/// criticises in the Claude wrapper.
#[derive(Debug)]
pub struct UpstreamError {
    /// The upstream's status, if there was one. `None` for transport or auth
    /// errors that never reached the backend.
    pub status: Option<u16>,
    pub message: String,
    pub body: Option<Value>,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP {status}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for UpstreamError {}

impl UpstreamError {
    fn local(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
            body: None,
        }
    }
}

fn parse_error_body(body: Option<&str>) -> Option<Value> {
    body.and_then(|body| serde_json::from_str(body).ok())
}

impl From<codex_api::ApiError> for UpstreamError {
    fn from(err: codex_api::ApiError) -> Self {
        use codex_api::ApiError as E;
        use codex_http_client::TransportError as T;

        match err {
            // The interesting case: the backend answered. Pass the body through
            // verbatim, drop the headers (they are in the daemon log).
            E::Transport(T::Http {
                status, body, url, ..
            }) => {
                let message = body
                    .as_deref()
                    .unwrap_or("no response from upstream")
                    .to_string();
                Self {
                    status: Some(status.as_u16()),
                    body: parse_error_body(body.as_deref()),
                    message: if body.is_some() {
                        message
                    } else {
                        format!("{message}: {url:?}")
                    },
                }
            }
            E::Api { status, message } => Self {
                status: Some(status.as_u16()),
                message,
                body: None,
            },
            E::InvalidRequest { message } => Self {
                status: Some(400),
                message,
                body: None,
            },
            E::QuotaExceeded => Self {
                status: Some(429),
                message: "quota exhausted".to_string(),
                body: None,
            },
            E::RateLimit(message) => Self {
                status: Some(429),
                message,
                body: None,
            },
            E::ServerOverloaded => Self {
                status: Some(503),
                message: "server overloaded".to_string(),
                body: None,
            },
            other => Self::local(other.to_string()),
        }
    }
}

// --- Client ----------------------------------------------------------------

/// Stateless per call, but shares the `AuthManager` and HTTP client.
///
/// Arbitrarily many `stream()` calls may run concurrently — exactly what the
/// daemon needs so one process can serve several requests.
#[derive(Clone)]
pub struct Client {
    manager: Arc<AuthManager>,
}

impl Client {
    pub fn new(manager: Arc<AuthManager>) -> Self {
        Self { manager }
    }

    /// Starts a turn and yields the events as a stream.
    ///
    /// Dropping the stream aborts the request — that is the cancellation HTTP
    /// brings along for free.
    pub async fn stream(
        &self,
        req: StreamRequest,
    ) -> Result<impl Stream<Item = Event> + use<>, UpstreamError> {
        let body = build_body(&req);
        let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth {
            manager: self.manager.clone(),
        });
        let client = ResponsesClient::new(transport(), provider(), auth);

        let mut headers = HeaderMap::new();
        if let Some(session_id) = &req.session_id {
            headers.extend(codex_api::build_session_headers(
                Some(session_id.clone()),
                None,
            ));
        }

        let upstream = client
            .stream(body, headers, Compression::None, /*turn_state*/ None)
            .await?;

        Ok(map_events(upstream))
    }

    /// The subscription's model list.
    pub async fn models(&self, client_version: &str) -> Result<Vec<Value>, UpstreamError> {
        let provider = provider();
        let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth {
            manager: self.manager.clone(),
        });
        let client = codex_api::ModelsClient::new(transport(), provider.clone(), auth);
        let url =
            codex_api::ModelsClient::<ReqwestTransport>::request_url(&provider, client_version);

        let (models, _etag) = client.list_models(url, HeaderMap::new()).await?;

        // Via serde_json so the daemon passes the upstream structure through
        // unchanged. What the backend declares should not be filtered here — the
        // consumer makes that choice.
        Ok(models
            .into_iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect())
    }
}

// --- Event translation -----------------------------------------------------

/// Maps `ResponseEvent` onto [`Event`].
///
/// `ServerModel` arrives before `Created` but carries the interesting piece of
/// information. It is therefore remembered and only emitted together with
/// `Created` as `Started` — otherwise there would be either two start events or
/// one without a model name.
fn map_events(
    upstream: impl Stream<Item = Result<ResponseEvent, codex_api::ApiError>>,
) -> impl Stream<Item = Event> {
    let state = ServerModelSlot::default();
    upstream.flat_map(move |item| {
        let events = match item {
            Ok(event) => map_one(event, &state),
            Err(err) => {
                let error = UpstreamError::from(err);
                vec![Event::Failed {
                    retryable: matches!(error.status, Some(429 | 503)),
                    message: error.message,
                }]
            }
        };
        futures::stream::iter(events)
    })
}

#[derive(Default)]
struct ServerModelSlot {
    model: std::sync::Mutex<Option<String>>,
}

fn map_one(event: ResponseEvent, slot: &ServerModelSlot) -> Vec<Event> {
    match event {
        ResponseEvent::ServerModel(model) => {
            if let Ok(mut guard) = slot.model.lock() {
                *guard = Some(model);
            }
            vec![]
        }
        ResponseEvent::Created => {
            let model = slot.model.lock().ok().and_then(|g| g.clone());
            vec![Event::Started { model }]
        }
        ResponseEvent::OutputTextDelta(text) => vec![Event::TextDelta { text }],
        // Summary and content are two channels of the same reasoning. The
        // consumer wants both as thinking text — the distinction would be detail
        // without a taker here.
        ResponseEvent::ReasoningSummaryDelta { delta, .. }
        | ResponseEvent::ReasoningContentDelta { delta, .. } => {
            vec![Event::ThinkingDelta { text: delta }]
        }
        ResponseEvent::OutputItemDone(ref item) => match item {
            ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => vec![Event::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }],
            // Pass on as opaque JSON, do not rebuild from fields: the
            // `encrypted_content` is verified server-side.
            ResponseItem::Reasoning { summary, .. } => {
                let texts = summary
                    .iter()
                    .map(|entry| match entry {
                        ReasoningItemReasoningSummary::SummaryText { text } => text.clone(),
                    })
                    .collect();
                match serde_json::to_value(item) {
                    Ok(value) => vec![Event::Reasoning {
                        item: value,
                        summary: texts,
                    }],
                    // If the item could not be serialised, a silent loss would
                    // be worse than a message: the consumer would otherwise be
                    // unable to continue the turn correctly.
                    Err(err) => vec![Event::Failed {
                        message: format!("reasoning item not serialisable: {err}"),
                        retryable: false,
                    }],
                }
            }
            // Message items only repeat what already arrived as deltas.
            _ => vec![],
        },
        ResponseEvent::RateLimits(snapshot) => vec![Event::RateLimits {
            plan: snapshot.plan_type.map(|p| format!("{p:?}")),
            primary: snapshot.primary.map(map_window),
            secondary: snapshot.secondary.map(map_window),
        }],
        ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        } => vec![Event::Done {
            response_id: Some(response_id),
            // `end_turn: None` means "the provider says nothing about it". We
            // treat that as a regular ending, because an abort would arrive as an
            // error.
            stop_reason: match end_turn {
                Some(false) => "aborted".to_string(),
                _ => "end_turn".to_string(),
            },
            usage: token_usage.map(|u| Usage {
                input_tokens: Some(u.input_tokens),
                output_tokens: Some(u.output_tokens),
                cached_input_tokens: Some(u.cached_input_tokens),
                cache_write_input_tokens: Some(u.cache_write_input_tokens),
                reasoning_output_tokens: Some(u.reasoning_output_tokens),
                total_tokens: Some(u.total_tokens),
            }),
        }],
        // Everything else (ModelsEtag, OutputItemAdded, SafetyBuffering, ...) has
        // no taker in this vocabulary.
        _ => vec![],
    }
}

fn map_window(window: codex_protocol::protocol::RateLimitWindow) -> RateLimitWindow {
    RateLimitWindow {
        used_percent: Some(window.used_percent),
        window_minutes: window.window_minutes,
        resets_at: window.resets_at,
    }
}

// --- Raw mode (CLI only) ---------------------------------------------------

/// POST bypassing the `ResponsesClient`, unparsed SSE text.
///
/// Reserved for the CLI: for protocol questions where the raw format is exactly
/// what matters. The daemon does not need this.
pub async fn raw_stream(
    manager: Arc<AuthManager>,
    req: &StreamRequest,
    body: Value,
    mut on_meta: impl FnMut(&str),
) -> Result<impl Stream<Item = Result<bytes::Bytes, String>>> {
    let provider = provider();
    let auth = ManagedChatGptAuth { manager };

    let mut request = provider.build_request(Method::POST, "responses");
    request.headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    if let Some(session_id) = &req.session_id {
        request.headers.extend(codex_api::build_session_headers(
            Some(session_id.clone()),
            None,
        ));
    }
    request.body = Some(RequestBody::Json(body));

    let request = match auth.apply_auth(request).await {
        Ok(request) => request,
        Err(err) => bail!("auth failed: {err}"),
    };

    on_meta(&format!("POST {}", request.url));
    for (name, value) in request.headers.iter() {
        on_meta(&format!("{name}: {}", redact(name.as_str(), value)));
    }

    let response = match transport().stream(request).await {
        Ok(response) => response,
        Err(err) => bail!("transport error: {err:?}"),
    };

    on_meta(&format!("--- status: {} ---", response.status));
    for (name, value) in response.headers.iter() {
        on_meta(&format!("{name}: {}", value.to_str().unwrap_or("<binary>")));
    }

    Ok(response
        .bytes
        .map(|chunk| chunk.map_err(|err| format!("{err:?}"))))
}

/// Never print header values carrying credentials in full.
fn redact(name: &str, value: &HeaderValue) -> String {
    let text = value.to_str().unwrap_or("<binary>");
    if name.eq_ignore_ascii_case("authorization") {
        let shown = text.chars().take(20).collect::<String>();
        format!("{shown}... ({} characters)", text.len())
    } else {
        text.to_string()
    }
}
