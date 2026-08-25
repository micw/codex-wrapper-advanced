//! Local REST API. One process, arbitrarily many concurrent requests.
//!
//! Why HTTP rather than a line protocol over stdio: multiplexing and
//! cancellation come for free here — one connection per request, aborting means
//! dropping the connection. Over stdio both would have to be built by hand. And
//! SSE is the format arriving from upstream anyway.
//!
//! # Paths
//!
//! The layout follows one rule: **one prefix, one exposure decision.** A reverse
//! proxy should get by with a single rule per surface, and whatever does not sit
//! under a released prefix stays inside automatically.
//!
//! | Prefix | Content | exposable |
//! |---|---|---|
//! | `/v1/*` | OpenAI-compatible (`models`; chat/responses to follow) | yes |
//! | `/wire/v1/*` | our own vocabulary (`wire::Event`) | yes |
//! | `/health`, `/ready` | operations, probes | **no** |
//!
//! `/v1` deliberately sits at the root rather than under `/api/openai/v1`: some
//! clients take a host and append `/v1/chat/completions` themselves. `/wire/v1`
//! is versioned from the start, because `wire::Event` is still moving.
//!
//! ```nginx
//! location /v1/      { proxy_pass http://127.0.0.1:8080; }
//! location /wire/v1/ { proxy_pass http://127.0.0.1:8080; }
//! location /         { return 404; }   # /health, /ready stay inside
//! ```
//!
//! # Access control
//!
//! See [`crate::listen`]. In short: on a unix socket the file permissions are the
//! access control, on TCP they are named API keys — and TCP without keys refuses
//! to start.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::extract::Query;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Sse;
use axum::response::sse::Event as SseEvent;
use axum::routing::get;
use axum::routing::post;
use codex_login::AuthManager;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::client::Client;
use crate::listen::ApiKeys;
use crate::listen::Listen;
use crate::wire::ServerInfo;
use crate::wire::StreamRequest;

#[derive(Clone)]
struct AppState {
    client: Client,
    manager: Arc<AuthManager>,
    keys: ApiKeys,
}

pub struct ServeConfig {
    pub listen: Listen,
    pub keys: ApiKeys,
    pub server_info: Option<PathBuf>,
    /// How often a sign-in URL is logged while the service cannot work.
    /// `None` disables it.
    pub login_reminder: Option<std::time::Duration>,
}

pub async fn run(config: ServeConfig) -> Result<()> {
    let ServeConfig {
        listen,
        keys,
        server_info,
        login_reminder,
    } = config;

    // Before anything else: an unsafe combination must not even start listening.
    crate::listen::validate(&listen, &keys)?;

    let manager = auth::auth_manager().await?;

    // Runs alongside the server: while not signed in, a sign-in URL goes to the
    // log at regular intervals, and the login completes as soon as somebody
    // confirms the code. Replaces a login endpoint along with its attack surface
    // — see auth::login_reminder.
    if let Some(interval) = login_reminder {
        tokio::spawn(auth::login_reminder(manager.clone(), interval));
    }

    let state = AppState {
        client: Client::new(manager.clone()),
        manager,
        keys,
    };

    let wire_api = Router::new()
        .route("/auth", get(auth_status))
        .route("/models", get(models))
        .route("/responses", post(responses));

    // OpenAI-compatible. `/v1/responses` to follow.
    let openai_api = Router::new()
        .route("/models", get(openai_models))
        .route("/chat/completions", post(openai_chat_completions));

    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .nest("/wire/v1", wire_api)
        .nest("/v1", openai_api)
        .with_state(state);

    match listen {
        Listen::Unix(path) => serve_unix(app, &path, server_info.as_deref()).await,
        Listen::Tcp(addr) => serve_tcp(app, addr, server_info.as_deref()).await,
    }
}

async fn serve_tcp(
    app: Router,
    addr: std::net::SocketAddr,
    server_info: Option<&Path>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {addr} failed"))?;
    // Do not report `addr`: with port 0 the OS picks one.
    let bound = listener.local_addr()?;

    announce(&Listen::Tcp(bound), server_info)?;
    axum::serve(listener, app).await.context("HTTP server")?;
    Ok(())
}

async fn serve_unix(app: Router, path: &Path, server_info: Option<&Path>) -> Result<()> {
    // A socket survives a hard abort as a file and then blocks the next start.
    // Only remove it when there really is a socket there — otherwise a typo in
    // the path deletes a real file.
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() {
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket: {}", path.display()))?;
        } else {
            anyhow::bail!(
                "{} exists and is not a socket — check the path",
                path.display()
            );
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket directory: {}", parent.display()))?;
    }

    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("binding to {} failed", path.display()))?;

    // The permissions are the access control here — so set them tightly, and do
    // it after binding (the file does not exist before).
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;

    announce(&Listen::Unix(path.to_path_buf()), server_info)?;
    axum::serve(listener, app).await.context("HTTP server")?;
    Ok(())
}

/// Tells the parent process where we listen.
///
/// Carries **no secret** — access now depends on the socket's file permissions or
/// on configured keys. The file stays useful when TCP is run with port 0.
fn announce(listen: &Listen, server_info: Option<&Path>) -> Result<()> {
    let info = ServerInfo {
        listen: listen.to_string(),
        pid: std::process::id(),
    };
    let encoded = serde_json::to_string(&info)?;

    if let Some(path) = server_info {
        std::fs::write(path, &encoded)
            .with_context(|| format!("writing server-info: {}", path.display()))?;
    }

    // Always to stdout as well, so a parent process can do without the file.
    // One line, flushed immediately — the caller is waiting for it.
    println!("{encoded}");
    use std::io::Write as _;
    std::io::stdout().flush()?;

    eprintln!("codex-api-wrapper serve: {listen}");
    Ok(())
}

// --- Handlers --------------------------------------------------------------

/// Readiness — can the daemon work?
///
/// `200` if yes, `503` if no. No key required, because a Kubernetes probe cannot
/// send one; the body therefore carries operational state only, no identity. Do
/// not expose.
///
/// Catches the case `/health` cannot see: `auth()` swallows refresh errors and
/// keeps reporting a valid sign-in while the refresh has long since failed for
/// good (DEPLOY.md §1).
async fn ready(State(state): State<AppState>) -> axum::response::Response {
    let status = auth::readiness(&state.manager).await;
    let code = if status.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(status)).into_response()
}

/// Liveness. Do not expose.
///
/// **Always** answers `200` while the process is alive — that is the point. Tie
/// liveness to the sign-in state and Kubernetes will kill the container in a
/// loop before anybody can exec in and sign in.
///
/// `authenticated` is a hint for the eye only. The readiness probe is `/ready`'s
/// job: `authenticated` stays `true` even when the refresh fails permanently.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let authenticated = state
        .manager
        .auth()
        .await
        .map(|auth| auth.is_chatgpt_auth())
        .unwrap_or(false);
    Json(json!({ "status": "ok", "authenticated": authenticated }))
}

async fn auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> axum::response::Response {
    if state.keys.authenticate(&headers).is_none() {
        return unauthorized();
    }
    match auth::status(&state.manager).await {
        Ok(status) => Json(status).into_response(),
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &err.to_string(),
            "server_error",
            Some("internal_error"),
        ),
    }
}

#[derive(Deserialize)]
struct ModelsQuery {
    #[serde(default = "default_client_version")]
    client_version: String,
}

fn default_client_version() -> String {
    crate::DEFAULT_CLIENT_VERSION.to_string()
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelsQuery>,
) -> axum::response::Response {
    if state.keys.authenticate(&headers).is_none() {
        return unauthorized();
    }
    match state.client.models(&query.client_version).await {
        Ok(models) => Json(json!({ "models": models })).into_response(),
        Err(err) => upstream_error(&err),
    }
}

#[derive(Deserialize)]
struct OpenAiModelsQuery {
    /// Also list models with `visibility: hide` — on the subscription that is
    /// `codex-auto-review`. Off by default so a model picker does not get
    /// cluttered with internals.
    #[serde(default)]
    include_hidden: bool,
}

/// `GET /v1/models` — OpenAI shape.
///
/// A slim projection rather than a pass-through; the reasoning is in
/// [`crate::openai::models_response`]. Callers who need the backend's raw data
/// use `/wire/v1/models`.
async fn openai_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OpenAiModelsQuery>,
) -> axum::response::Response {
    if state.keys.authenticate(&headers).is_none() {
        return unauthorized();
    }
    match state.client.models(crate::DEFAULT_CLIENT_VERSION).await {
        Ok(models) => Json(crate::openai::models_response(
            &models,
            query.include_hidden,
        ))
        .into_response(),
        Err(err) => upstream_error(&err),
    }
}

/// One turn as SSE.
///
/// Every event is a JSON object in `data:`; the kind sits in the `type` field.
/// No `event:` field — otherwise the consumer would have to read two places.
///
/// If the client aborts the connection the stream is dropped and the upstream
/// request ends with it. That is why there is no `cancel` command.
async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StreamRequest>,
) -> axum::response::Response {
    let Some(caller) = state.keys.authenticate(&headers) else {
        return unauthorized();
    };

    let stream = match state.client.stream(request).await {
        Ok(stream) => stream,
        // Errors *before* the first event come back as an HTTP status, not as an
        // SSE event: a status-200 stream containing nothing but an error event
        // would be harder for the caller to handle than a 400.
        Err(err) => {
            eprintln!("[{caller}] request rejected: {err}");
            return upstream_error(&err);
        }
    };

    let sse = stream.map(|event| {
        let payload = serde_json::to_string(&event)
            .unwrap_or_else(|err| format!(r#"{{"type":"failed","message":"{err}"}}"#));
        Ok::<_, std::convert::Infallible>(SseEvent::default().data(payload))
    });

    Sse::new(sse).into_response()
}

/// `POST /v1/chat/completions` — OpenAI shape, streaming only.
///
/// Translation lives in [`crate::openai_chat`]. Non-streaming requests are
/// answered with SSE as well: every Chat-Completions client handles a stream,
/// while the reverse is not true, and a second accumulation path would double
/// the mapping surface for no measured consumer.
async fn openai_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Result<Json<crate::openai_chat::ChatRequest>, JsonRejection>,
) -> axum::response::Response {
    let Some(caller) = state.keys.authenticate(&headers) else {
        return unauthorized();
    };
    let request = match request {
        Ok(Json(request)) => request,
        Err(rejection) => {
            return error(
                StatusCode::BAD_REQUEST,
                &rejection.body_text(),
                "invalid_request_error",
                None,
            );
        }
    };

    let include_usage = request
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    let streaming = request.stream.unwrap_or(false);

    let wire = match crate::openai_chat::to_wire(&request) {
        Ok(wire) => wire,
        Err(message) => {
            return error(
                StatusCode::BAD_REQUEST,
                &message,
                "invalid_request_error",
                None,
            );
        }
    };

    let stream = match state.client.stream(wire).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("[{caller}] chat request rejected: {err}");
            return upstream_error(&err);
        }
    };

    // Stable id for the whole turn; `created` stays 0 for the same reason as in
    // `models_response` — a moving timestamp would make caches differ.
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let model = request.model;

    if !streaming {
        let mut state = crate::openai_chat::ChatResponseState::default();
        let mut events = stream;
        while let Some(event) = events.next().await {
            state.apply(&event, &id, &model, false);
            if state.failed.is_some() {
                break;
            }
        }
        if let Some((message, retryable)) = state.failed {
            return upstream_chat_error(&message, retryable);
        }
        return Json(state.response(&id, &model)).into_response();
    }

    let mut state = crate::openai_chat::ChatResponseState::default();
    let sse = stream.flat_map(move |event| {
        let lines = state.apply(&event, &id, &model, include_usage);
        futures::stream::iter(
            lines
                .into_iter()
                .map(|line| Ok::<_, std::convert::Infallible>(SseEvent::default().data(line))),
        )
    });

    Sse::new(sse).into_response()
}

fn upstream_chat_error(message: &str, retryable: bool) -> axum::response::Response {
    let status = if retryable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    };
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "api_error",
                "code": "upstream_error"
            }
        })),
    )
        .into_response()
}

/// Passes the upstream's status through.
///
/// A 400 from the backend stays a 400, with its own wording. Only where there was
/// no answer at all (transport, auth) does it become a 502 — then the daemon
/// really is the broken link.
fn upstream_error(err: &crate::client::UpstreamError) -> axum::response::Response {
    let status = err
        .status
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    (
        status,
        Json(json!({
            "error": {
                "message": err.message,
                "type": "api_error",
                "param": null,
                "code": "upstream_error",
                "upstream_status": err.status,
            }
        })),
    )
        .into_response()
}

fn unauthorized() -> axum::response::Response {
    error(
        StatusCode::UNAUTHORIZED,
        "invalid or missing API key",
        "invalid_request_error",
        Some("invalid_api_key"),
    )
}

fn error(
    status: StatusCode,
    message: &str,
    error_type: &str,
    code: Option<&str>,
) -> axum::response::Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type,
                "param": null,
                "code": code,
            }
        })),
    )
        .into_response()
}
