//! Request path against `chatgpt.com/backend-api/codex`.
//!
//! Translates `codex_api::ResponseEvent` into the neutral vocabulary of
//! [`crate::wire`]. Knowledge of the Codex types lives only here; everything
//! above (daemon, CLI) sees `wire::Event` exclusively.

use std::sync::Arc;
use std::sync::Mutex;
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
use codex_http_client::HttpClientFactory;
use codex_http_client::HttpTransport;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::Request;
use codex_http_client::RequestBody;
use codex_http_client::ReqwestTransport;
use codex_http_client::Response;
use codex_http_client::StreamResponse;
use codex_http_client::TransportError;
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

use crate::auth_recovery::AuthHealth;
use crate::auth_recovery::AuthTracker;
use crate::auth_recovery::run_with_unauthorized_recovery;
use crate::limits::TurnLimits;
use crate::wire::Event;
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
    tracker: AuthTracker,
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
            self.tracker.record(&auth);
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

/// Keeps a copy of the response headers on their way past.
///
/// `codex-api` consumes them inside `spawn_response_stream` and only forwards
/// what its own types model — which drops `x-codex-active-limit` and
/// `x-codex-plan-type` entirely, and hard-codes `plan_type: None`
/// (`rate_limits.rs:98`). Without those two, a turn's quota groups cannot be
/// resolved (see [`crate::limits`]).
///
/// The seam is the library's own: `HttpTransport` is public and
/// `ResponsesClient` generic over it, so this needs no fork and no rebuilt SSE
/// decoding. If the trait changes, the build breaks — which is the good kind of
/// dependency, unlike data that quietly turns into something else.
struct HeaderTap<T> {
    inner: T,
    seen: Arc<Mutex<Option<HeaderMap>>>,
}

impl<T: HttpTransport> HttpTransport for HeaderTap<T> {
    async fn execute(&self, req: Request) -> Result<Response, TransportError> {
        self.inner.execute(req).await
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        let response = self.inner.stream(req).await?;
        // Locked only after the await — a guard held across it would make the
        // future non-`Send` and the trait bound would reject it.
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(response.headers.clone());
        }
        Ok(response)
    }
}

// --- Cache key -------------------------------------------------------------

/// Derives a prompt cache key from the **invariant head** of a conversation.
///
/// Measured against the subscription backend: the cache is machine-local, and
/// the key is what routes a request to the machine holding the prefix. Without
/// one, a request lands somewhere in the pool and only hits by luck — 7/30 in a
/// controlled run, and less than that in real traffic where other prefixes sit
/// in between. With a key that stays the same across the turns of a
/// conversation: 28/30.
///
/// What goes in, and why exactly this:
///
/// * `model` — two models never share a cache entry.
/// * `instructions` — the system prompt, the largest stable block.
/// * `tools` — part of the prefix, and stable for the life of a conversation.
/// * **the first input item** — what separates two conversations that share a
///   system prompt and a tool set. That is the common case for one agent
///   holding many conversations.
///
/// What stays out: everything that can change between the turns of one
/// conversation without breaking the token prefix. `effort` in particular — a
/// caller raising it mid-conversation would otherwise lose the cache, although
/// the prefix itself is untouched.
///
/// Two conversations whose heads are byte-identical share a key. That is
/// harmless: measured, three different conversations under a single key all sat
/// at 98 % — the key routes, the prefix decides the hit.
///
/// The counter-case is a head that is *cut off* mid-conversation (compaction, a
/// sliding window). The key changes then — but so does the token prefix, so
/// there was nothing left to hit anyway.
pub fn cache_key(req: &StreamRequest) -> String {
    // FNV-1a, deliberately not a crate and not `DefaultHasher`: this value has to
    // stay identical across restarts of the daemon, otherwise a conversation
    // loses its cache when the service is restarted. `DefaultHasher` gives no
    // guarantee across Rust versions; a hash written out here does.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // Separator, so ("ab", "c") and ("a", "bc") do not collapse into one.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    };

    eat(req.model.as_bytes());
    eat(req.instructions.as_deref().unwrap_or("").as_bytes());
    eat(req
        .tools
        .as_ref()
        .map(|tools| tools.to_string())
        .unwrap_or_default()
        .as_bytes());
    eat(req
        .input
        .first()
        .map(|item| item.to_string())
        .unwrap_or_default()
        .as_bytes());

    // The prefix makes the value recognisable in a backend-side log as coming
    // from here, without carrying anything about the content.
    format!("wrap-{hash:016x}")
}

/// The key actually sent: the caller's, or the derived one.
///
/// A caller who names a key knows more than we can derive — an OpenAI client
/// sending `prompt_cache_key` or `user` has the conversation identity at hand.
pub fn effective_cache_key(req: &StreamRequest) -> String {
    req.session_id.clone().unwrap_or_else(|| cache_key(req))
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
    // The summary is asked for unconditionally, the effort only when the caller
    // named one. Measured (MESSUNGEN.md §3): the backend accepts `summary`
    // without `effort`, the input tokens stay identical and the reasoning tokens
    // fall either way — the turn thinks regardless, the summary only puts it into
    // words. Tying the two together would leave a caller who sets no effort
    // watching a silent pause followed by an answer out of nowhere.
    let mut reasoning = json!({ "summary": "auto" });
    if let Some(effort) = &req.effort {
        reasoning["effort"] = json!(effort);
    }
    map.insert("reasoning".to_string(), reasoning);
    // Body field and `session-id` header carry the same value, as the official
    // client does. Measured: each works on its own, and with both set to
    // different values the entry is reachable under either — they are not
    // separate namespaces. Setting both spares us the question of precedence.
    map.insert(
        "prompt_cache_key".to_string(),
        json!(effective_cache_key(req)),
    );
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
    auth_health: Arc<AuthHealth>,
}

impl Client {
    pub fn new(manager: Arc<AuthManager>) -> Self {
        Self {
            manager,
            auth_health: Arc::new(AuthHealth::default()),
        }
    }

    pub(crate) fn with_auth_health(
        manager: Arc<AuthManager>,
        auth_health: Arc<AuthHealth>,
    ) -> Self {
        Self {
            manager,
            auth_health,
        }
    }

    /// Starts a turn and yields the events as a stream.
    ///
    /// Dropping the stream aborts the request — that is the cancellation HTTP
    /// brings along for free.
    pub async fn stream(
        &self,
        mut req: StreamRequest,
    ) -> Result<impl Stream<Item = Event> + use<>, UpstreamError> {
        // `gpt-5.6-sol:long` is the same model with a different budget in the
        // caller's head. Stripped here rather than per surface, so the cache key
        // and the body both see the name the backend knows — and both variants
        // therefore share a cache entry, which is right: the requests are
        // identical.
        req.model = crate::models::wire_model(&req.model).to_string();
        let body = build_body(&req);
        let mut headers = HeaderMap::new();
        headers.extend(codex_api::build_session_headers(
            Some(effective_cache_key(&req)),
            None,
        ));

        let (upstream, quota) =
            run_with_unauthorized_recovery(&self.manager, &self.auth_health, |tracker| {
                let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth {
                    manager: self.manager.clone(),
                    tracker,
                });
                let seen = Arc::new(Mutex::new(None));
                let client = ResponsesClient::new(
                    HeaderTap {
                        inner: transport(),
                        seen: seen.clone(),
                    },
                    provider(),
                    auth,
                );
                let body = body.clone();
                let headers = headers.clone();
                async move {
                    let upstream = client
                        .stream(body, headers, Compression::None, /*turn_state*/ None)
                        .await
                        .map_err(UpstreamError::from)?;

                    // The tap has run by now: the transport returns before the
                    // stream is spawned. Build one resolved quota event from the
                    // headers rather than codex-api's lossy snapshots.
                    let quota: Vec<Event> = seen
                        .lock()
                        .ok()
                        .and_then(|mut seen| seen.take())
                        .map(|headers| Event::RateLimits(TurnLimits::from_headers(&headers)))
                        .into_iter()
                        .collect();
                    Ok((upstream, quota))
                }
            })
            .await?;

        Ok(futures::stream::iter(quota).chain(map_events(upstream)))
    }

    /// The subscription's model list.
    pub async fn models(&self, client_version: &str) -> Result<Vec<Value>, UpstreamError> {
        let models = run_with_unauthorized_recovery(&self.manager, &self.auth_health, |tracker| {
            let provider = provider();
            let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth {
                manager: self.manager.clone(),
                tracker,
            });
            let client = codex_api::ModelsClient::new(transport(), provider.clone(), auth);
            let url =
                codex_api::ModelsClient::<ReqwestTransport>::request_url(&provider, client_version);
            async move {
                let (models, _etag) = client
                    .list_models(url, HeaderMap::new())
                    .await
                    .map_err(UpstreamError::from)?;
                Ok(models)
            }
        })
        .await?;

        // Via serde_json so the daemon passes the upstream structure through
        // unchanged. What the backend declares should not be filtered here — the
        // consumer makes that choice.
        Ok(models
            .into_iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect())
    }

    /// Reads the subscription usage and rate-limit snapshot.
    pub async fn usage(&self) -> Result<Value, UpstreamError> {
        run_with_unauthorized_recovery(&self.manager, &self.auth_health, |tracker| async move {
            let auth = self
                .manager
                .auth()
                .await
                .ok_or_else(|| UpstreamError::local("not signed in"))?;
            tracker.record(&auth);
            let mut headers = HeaderMap::new();
            headers.extend(default_client::default_headers());
            apply_auth_headers(&auth, &mut headers);
            let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
            let http = default_client::create_client_with_chatgpt_cookies(&factory);
            let client = ReqwestTransport::from_http_client(http);
            let mut request = Request::new(
                Method::GET,
                "https://chatgpt.com/backend-api/wham/usage".to_string(),
            );
            request.headers = headers;
            let response = client
                .execute(request)
                .await
                .map_err(|err| UpstreamError::local(format!("usage request failed: {err}")))?;
            let status = response.status;
            let body = String::from_utf8_lossy(&response.body).to_string();
            let value = serde_json::from_str(&body).map_err(|err| UpstreamError {
                status: Some(status.as_u16()),
                message: format!("invalid usage response: {err}"),
                body: None,
            })?;
            if !status.is_success() {
                return Err(UpstreamError {
                    status: Some(status.as_u16()),
                    message: body,
                    body: Some(value),
                });
            }
            Ok(value)
        })
        .await
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
        // Only from the second part onward. The backend announces part 0 as
        // well, and a boundary before the first block would open the thinking
        // with an empty paragraph.
        ResponseEvent::ReasoningSummaryPartAdded { summary_index } if summary_index > 0 => {
            vec![Event::ThinkingBreak]
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
        // Dropped on purpose: one per header family, no group identity, and
        // `plan_type` always `None` on this path. `Client::stream` builds a single
        // resolved event from the headers instead.
        ResponseEvent::RateLimits(_) => vec![],
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
    let auth = ManagedChatGptAuth {
        manager,
        tracker: AuthTracker::default(),
    };

    let mut request = provider.build_request(Method::POST, "responses");
    request.headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    request.headers.extend(codex_api::build_session_headers(
        Some(effective_cache_key(req)),
        None,
    ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(input: Vec<Value>) -> StreamRequest {
        StreamRequest {
            model: "gpt-5.6-sol".into(),
            input,
            instructions: Some("You are a code reviewer.".into()),
            tools: Some(json!([{ "type": "function", "name": "read_file" }])),
            effort: None,
            tool_choice: None,
            parallel_tool_calls: None,
            store: Some(false),
            session_id: None,
        }
    }

    fn user(text: &str) -> Value {
        json!({ "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": text }] })
    }

    fn assistant(text: &str) -> Value {
        json!({ "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": text }] })
    }

    /// The point of the whole exercise: the history grows, the key does not
    /// move. A key that changed per turn measured 0 hits out of 4.
    #[test]
    fn key_survives_a_growing_conversation() {
        let turn1 = request(vec![user("Explain the parser.")]);
        let turn2 = request(vec![
            user("Explain the parser."),
            assistant("It is a recursive descent parser."),
            user("And the lexer?"),
        ]);
        let turn3 = request(vec![
            user("Explain the parser."),
            assistant("It is a recursive descent parser."),
            user("And the lexer?"),
            assistant("Hand-written."),
            user("Any tests?"),
        ]);
        assert_eq!(cache_key(&turn1), cache_key(&turn2));
        assert_eq!(cache_key(&turn2), cache_key(&turn3));
    }

    /// Two conversations that share a system prompt and a tool set must not
    /// share a key just because of that — that is the everyday case for one
    /// agent holding many conversations.
    #[test]
    fn different_first_message_gives_a_different_key() {
        assert_ne!(
            cache_key(&request(vec![user("Explain the parser.")])),
            cache_key(&request(vec![user("Explain the lexer.")])),
        );
    }

    /// Everything the token prefix is made of has to move the key.
    #[test]
    fn head_fields_all_move_the_key() {
        let base = request(vec![user("Explain the parser.")]);
        let key = cache_key(&base);

        let mut other_model = request(vec![user("Explain the parser.")]);
        other_model.model = "gpt-5.5".into();
        assert_ne!(key, cache_key(&other_model));

        let mut other_instructions = request(vec![user("Explain the parser.")]);
        other_instructions.instructions = Some("You are a poet.".into());
        assert_ne!(key, cache_key(&other_instructions));

        let mut other_tools = request(vec![user("Explain the parser.")]);
        other_tools.tools = Some(json!([{ "type": "function", "name": "write_file" }]));
        assert_ne!(key, cache_key(&other_tools));
    }

    /// `effort` may change mid-conversation without touching the token prefix.
    /// Folding it into the key would throw the cache away for nothing.
    #[test]
    fn effort_does_not_move_the_key() {
        let base = request(vec![user("Explain the parser.")]);
        let mut raised = request(vec![user("Explain the parser.")]);
        raised.effort = Some("high".into());
        assert_eq!(cache_key(&base), cache_key(&raised));
    }

    /// The separator between the parts: without it, moving a byte across a
    /// field boundary would produce the same key.
    #[test]
    fn field_boundaries_are_separated() {
        let mut a = request(vec![user("x")]);
        a.instructions = Some("ab".into());
        a.tools = Some(json!("c"));
        let mut b = request(vec![user("x")]);
        b.instructions = Some("a".into());
        b.tools = Some(json!("bc"));
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    /// A caller who names a key knows more than we can derive.
    #[test]
    fn callers_key_wins() {
        let mut req = request(vec![user("Explain the parser.")]);
        req.session_id = Some("chat-42".into());
        assert_eq!(effective_cache_key(&req), "chat-42");
        req.session_id = None;
        assert_eq!(effective_cache_key(&req), cache_key(&req));
    }

    /// Both places carry the key, and the same value — the header is set in
    /// `stream`, the body field here.
    #[test]
    fn body_carries_the_cache_key() {
        let req = request(vec![user("Explain the parser.")]);
        let body = build_body(&req);
        assert_eq!(body["prompt_cache_key"], json!(effective_cache_key(&req)));
        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| key.starts_with("wrap-"))
        );
    }

    #[test]
    fn native_image_input_reaches_the_upstream_unchanged() {
        let image = json!({
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "describe" },
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,iVBORw0KGgo="
                }
            ]
        });
        let req = request(vec![image.clone()]);
        let body = build_body(&req);
        assert_eq!(body["input"][0], image);
    }

    /// Deterministic across processes: the value is written out here rather than
    /// taken from `DefaultHasher`, so a restart does not cost a conversation its
    /// cache. Pinned to a literal so a refactor cannot silently change it.
    #[test]
    fn key_is_stable_across_runs() {
        let mut req = request(vec![user("Explain the parser.")]);
        req.tools = None;
        req.instructions = None;
        assert_eq!(cache_key(&req), "wrap-d9d7a1e7bd87c778");
    }
}
