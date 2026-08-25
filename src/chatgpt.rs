//! Request path against the subscription backend `chatgpt.com/backend-api/codex`.
//!
//! Zwei Modi, absichtlich beide vorhanden:
//!
//! * `decoded` — via `codex_api::ResponsesClient`. That is the same code the real
//!   CLI runs: SSE parsing, idle timeout, rate limit headers. It shows what Codex
//!   itself would see.
//! * `raw` — straight through the transport, unparsed SSE text. For the questions
//!   where the raw format is exactly what matters (KONTEXT-HARNESS.md 10): does
//!   `apply_patch` come back as `custom` or `function`, what does a 400 actually
//!   say, which headers does the backend send.
//!
//! The request body is built as free-form JSON rather than through
//! `ResponsesApiRequest`. For an exploration tool that is the point:
//! `instructions` and `tools` have to be freely variable to measure 8.3.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use codex_api::AuthError;
use codex_api::AuthHeadersFuture;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider;
use codex_api::ResponsesClient;
use codex_api::RetryConfig;
use codex_api::SharedAuthProvider;
use codex_http_client::HttpTransport;
use codex_http_client::RequestBody;
use codex_http_client::ReqwestTransport;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use serde_json::json;

/// Base URL of the subscription backend. `ResponsesClient` appends `responses`.
///
/// Deliberately hard-wired: this tests exactly that endpoint. The platform API
/// (`api.openai.com`) would need an API key, and the question would be a
/// different one.
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

// --- Auth provider ---------------------------------------------------------

/// Connects the `AuthManager` to `codex_api`'s `AuthProvider` trait.
///
/// Unlike `model_provider::BearerAuthProvider` (a snapshot taken at construction
/// time) this variant fetches the token afresh on *every* request via
/// `AuthManager::auth()`, which renews an expired access token along the way. For
/// something long-running that is the relevant difference.
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
/// An exploration tool should surface the first error, not paper over it. 429 and
/// 5xx in particular are the measurement here, not the nuisance.
fn provider() -> Provider {
    Provider {
        name: "chatgpt".to_string(),
        base_url: CHATGPT_BASE_URL.to_string(),
        query_params: None,
        // Supplies the user agent and the `originator: codex_cli_rs` header from
        // upstream. Both identify the client to the backend.
        headers: default_client::default_headers(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(500),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        // opus:high on the Claude wrapper took up to ~165 s to the first token
        // (KONTEXT-HARNESS.md 4.1). Set generously, so that a timeout here really
        // is a timeout.
        stream_idle_timeout: Duration::from_secs(300),
    }
}

fn transport() -> ReqwestTransport {
    ReqwestTransport::from_http_client(default_client::create_client())
}

// --- Request body ----------------------------------------------------------

pub struct AskOptions {
    pub prompt: String,
    pub model: String,
    pub effort: Option<String>,
    pub instructions: Option<String>,
    pub tools: Option<Value>,
    pub store: bool,
    pub session_id: String,
    pub dump_dir: Option<String>,
}

/// Builds the Responses body.
///
/// `instructions` is only set when something was supplied — the default is
/// deliberately *empty*. That makes the very first call the test from 8.3: does
/// the subscription backend accept a request without the official Codex
/// instructions, or does it answer `400 {"detail":"Instructions are not valid"}`?
pub fn build_body(opts: &AskOptions) -> Value {
    let mut body = json!({
        "model": opts.model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": opts.prompt }]
        }],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "store": opts.store,
        "stream": true,
        // Without `store` the client has to hand reasoning items back itself,
        // and that needs the encrypted content.
        "include": ["reasoning.encrypted_content"],
    });

    let map = body.as_object_mut().expect("object");

    if let Some(instructions) = &opts.instructions {
        map.insert("instructions".to_string(), json!(instructions));
    }
    // Empty array rather than `null`: "this client has no tools" is a different
    // statement from "the field is missing".
    map.insert("tools".to_string(), opts.tools.clone().unwrap_or_else(|| json!([])));

    if let Some(effort) = &opts.effort {
        map.insert(
            "reasoning".to_string(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }
    body
}

fn dump(dir: &str, name: &str, content: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating dump directory: {dir}"))?;
    let path = Path::new(dir).join(name);
    std::fs::write(&path, content).with_context(|| format!("writing: {}", path.display()))?;
    eprintln!("  -> {}", path.display());
    Ok(())
}

// --- Execution -------------------------------------------------------------

/// Decoded run via `ResponsesClient` — the path the real CLI takes.
pub async fn ask_decoded(manager: Arc<AuthManager>, opts: AskOptions) -> Result<()> {
    let body = build_body(&opts);
    if let Some(dir) = &opts.dump_dir {
        dump(dir, "request.json", &serde_json::to_string_pretty(&body)?)?;
    }

    let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth { manager });
    let client = ResponsesClient::new(transport(), provider(), auth);

    let stream = client
        .stream(
            body,
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await;

    let mut stream = match stream {
        Ok(stream) => stream,
        // The failure case is the interesting one here: a 400 carrying
        // "Instructions are not valid" would be the answer to 8.3.
        Err(err) => bail!("request rejected: {err:?}"),
    };

    if let Some(request_id) = &stream.upstream_request_id {
        eprintln!("x-request-id: {request_id}");
    }

    let mut transcript = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => {
                let line = format!("{event:?}");
                transcript.push_str(&line);
                transcript.push('\n');
                print_event(&event);
            }
            Err(err) => {
                let line = format!("ERROR {err:?}");
                transcript.push_str(&line);
                transcript.push('\n');
                eprintln!("\n[stream error] {err:?}");
                break;
            }
        }
    }
    println!();

    if let Some(dir) = &opts.dump_dir {
        dump(dir, "events.log", &transcript)?;
    }
    Ok(())
}

/// Compact rendering. Deltas run on, everything else gets a marked line — so it
/// stays visible what the backend sends besides text.
fn print_event(event: &codex_api::ResponseEvent) {
    use codex_api::ResponseEvent as E;
    match event {
        E::OutputTextDelta(delta) => print!("{delta}"),
        E::ReasoningSummaryDelta { delta, .. } => eprint!("\x1b[2m{delta}\x1b[0m"),
        E::ReasoningContentDelta { delta, .. } => eprint!("\x1b[2m{delta}\x1b[0m"),
        E::Completed { token_usage, .. } => {
            eprintln!("\n[completed] usage: {token_usage:?}");
        }
        E::RateLimits(snapshot) => eprintln!("\n[rate-limits] {snapshot:?}"),
        E::ServerModel(model) => eprintln!("\n[server-model] {model}"),
        other => eprintln!("\n[{}]", short_name(other)),
    }
}

fn short_name(event: &codex_api::ResponseEvent) -> String {
    format!("{event:?}")
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .trim_end_matches('(')
        .to_string()
}

/// Raw run: POST through the transport, bypassing `ResponsesClient`, SSE
/// unprocessed.
///
/// Shows status, every response header and the body line by line. That is the view
/// needed for protocol questions and the one the decoded path abstracts away.
pub async fn ask_raw(manager: Arc<AuthManager>, opts: AskOptions) -> Result<()> {
    let body = build_body(&opts);
    if let Some(dir) = &opts.dump_dir {
        dump(dir, "request.json", &serde_json::to_string_pretty(&body)?)?;
    }

    let provider = provider();
    let auth = ManagedChatGptAuth { manager };

    let mut request = provider.build_request(Method::POST, "responses");
    request.headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    request
        .headers
        .extend(codex_api::build_session_headers(
            Some(opts.session_id.clone()),
            None,
        ));
    request.body = Some(RequestBody::Json(body));

    let request = auth
        .apply_auth(request)
        .await
        .map_err(|err| anyhow::anyhow!("auth failed: {err}"))?;

    eprintln!("POST {}", request.url);
    eprintln!("--- request headers ---");
    for (name, value) in request.headers.iter() {
        eprintln!("{name}: {}", redact(name.as_str(), value));
    }

    let response = transport()
        .stream(request)
        .await
        .map_err(|err| anyhow::anyhow!("transport error: {err:?}"))?;

    eprintln!("--- status: {} ---", response.status);
    eprintln!("--- response headers ---");
    let mut header_dump = String::new();
    for (name, value) in response.headers.iter() {
        let line = format!("{name}: {}", value.to_str().unwrap_or("<binary>"));
        eprintln!("{line}");
        header_dump.push_str(&line);
        header_dump.push('\n');
    }
    eprintln!("--- body ---");

    let mut raw = String::new();
    let mut bytes = response.bytes;
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|err| anyhow::anyhow!("stream error: {err:?}"))?;
        let text = String::from_utf8_lossy(&chunk);
        print!("{text}");
        raw.push_str(&text);
    }
    println!();

    if let Some(dir) = &opts.dump_dir {
        dump(dir, "response-headers.txt", &header_dump)?;
        dump(dir, "response-raw.sse", &raw)?;
    }
    Ok(())
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

// --- Modelle ---------------------------------------------------------------

/// Queries `/models` — which models does the subscription allow.
///
/// Answers the `/v1/models` point from the feature audit (KONTEXT-HARNESS.md 6)
/// along the way: this is where the backend states context size and capabilities
/// itself.
pub async fn models(manager: Arc<AuthManager>, client_version: &str) -> Result<()> {
    let provider = provider();
    let auth: SharedAuthProvider = Arc::new(ManagedChatGptAuth { manager });
    let client = codex_api::ModelsClient::new(transport(), provider.clone(), auth);
    let url = codex_api::ModelsClient::<ReqwestTransport>::request_url(&provider, client_version);

    eprintln!("GET {url}");
    let (models, etag) = client
        .list_models(url, HeaderMap::new())
        .await
        .map_err(|err| anyhow::anyhow!("query failed: {err:?}"))?;

    if let Some(etag) = etag {
        eprintln!("etag: {etag}");
    }
    println!("{}", serde_json::to_string_pretty(&models)?);
    Ok(())
}

/// Reads tool definitions from a JSON file.
///
/// Expects an array. This is exactly how the test runs whether our own tools get
/// through instead of the built-in ones — the point where the Claude wrapper has
/// `--tools ""` (KONTEXT-HARNESS.md 1.1).
pub fn load_tools(path: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading: {path}"))?;
    let value: Value = serde_json::from_str(&text).with_context(|| format!("parsing JSON: {path}"))?;
    if !value.is_array() {
        bail!("{path}: expected a JSON array of tool definitions");
    }
    Ok(value)
}

/// Loads one of the official prompt files from the Codex checkout.
///
/// Handy for the counter-test to empty `instructions`: first without, then with
/// the verbatim prompt the CLI sends.
pub fn load_instructions(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading: {path}"))
}
