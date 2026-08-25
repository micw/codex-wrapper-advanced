//! Local REST API. One process, arbitrarily many concurrent requests.
//!
//! Why HTTP rather than a line protocol over stdio: multiplexing and
//! cancellation come for free here — one connection per request, aborting means
//! dropping the connection. Over stdio both would have to be built by hand. And
//! SSE is the format arriving from upstream anyway.
//!
//! # Access control
//!
//! A port, unlike a pipe, has no built-in access control. That bears directly on
//! the account clause in KONTEXT-HARNESS.md §8.2: a local wrapper for oneself is
//! not sharing, an open port for colleagues is — regardless of transport. Hence:
//!
//! * Bind to `127.0.0.1` only, never `0.0.0.0`.
//! * An ephemeral port, so nothing waits on a well-known number.
//! * A bearer token, minted randomly at startup and handed over only through
//!   `server-info`. Without a valid token the answer is 401.
//!
//! The token does not protect against anyone who can read the process list or the
//! `server-info` file — it stops an arbitrary other process on the machine from
//! simply using the port.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Sse;
use axum::response::sse::Event as SseEvent;
use axum::routing::get;
use axum::routing::post;
use codex_login::AuthManager;
use futures::StreamExt;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::client::Client;
use crate::wire::ServerInfo;
use crate::wire::StreamRequest;

#[derive(Clone)]
struct AppState {
    client: Client,
    manager: Arc<AuthManager>,
    token: String,
}

/// 256 bits from the OS CSPRNG, hex encoded.
fn mint_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    // Constant-time comparison is overkill here (local socket, 256-bit token)
    // but costs nothing.
    presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Where to bind.
///
/// Loopback by default. Inside a container that means "reachable only within this
/// network namespace" — exactly right for a Kubernetes **sidecar**, because
/// containers of one pod share localhost. Only when the daemon has to be reachable
/// from another namespace is `--bind` needed; the token is then the sole remaining
/// barrier. See DEPLOY.md.
pub struct BindConfig {
    pub address: IpAddr,
    pub port: u16,
}

impl Default for BindConfig {
    fn default() -> Self {
        Self {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            // 0 = the operating system picks a free port.
            port: 0,
        }
    }
}

pub async fn run(server_info: Option<PathBuf>, bind: BindConfig) -> Result<()> {
    let manager = auth::auth_manager().await?;
    let token = mint_token();
    let state = AppState {
        client: Client::new(manager.clone()),
        manager,
        token: token.clone(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/auth", get(auth_status))
        .route("/models", get(models))
        .route("/responses", post(responses))
        .with_state(state);

    if !bind.address.is_loopback() {
        eprintln!(
            "WARNING: listening on {} - the daemon is reachable outside this machine \
             reachable. The bearer token is the only barrier. Anyone exposing the endpoint \
             to other people shares their ChatGPT account \
             with them.",
            bind.address
        );
    }
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((bind.address, bind.port)))
        .await
        .with_context(|| format!("binding to {}:{} failed", bind.address, bind.port))?;
    let port = listener.local_addr()?.port();

    let info = ServerInfo {
        port,
        pid: std::process::id(),
        token,
    };
    let encoded = serde_json::to_string(&info)?;

    if let Some(path) = &server_info {
        std::fs::write(path, &encoded)
            .with_context(|| format!("writing server-info: {}", path.display()))?;
        // Owner-only: the file carries the token.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }

    // Always to stdout as well, so a parent process can do without the file.
    // One line, flushed immediately — the caller is waiting for it.
    println!("{encoded}");
    use std::io::Write as _;
    std::io::stdout().flush()?;

    eprintln!("codex-api-wrapper serve: http://{}:{port}", bind.address);
    axum::serve(listener, app).await.context("HTTP server")?;
    Ok(())
}

// --- Handlers --------------------------------------------------------------

/// Readiness — can the daemon work?
///
/// `200` if yes, `503` if no. No token required, because a Kubernetes probe
/// cannot send one; the body therefore carries operational state only, no
/// identity.
///
/// This probe catches the case `/health` cannot see: `auth()` swallows refresh
/// errors and keeps reporting a valid sign-in while the refresh has long since
/// failed for good (DEPLOY.md §1).
async fn ready(State(state): State<AppState>) -> axum::response::Response {
    let status = auth::readiness(&state.manager).await;
    let code = if status.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(status)).into_response()
}

/// Lebenszeichen, ohne Token.
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
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    match auth::status(&state.manager).await {
        Ok(status) => Json(status).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
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
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    match state.client.models(&query.client_version).await {
        Ok(models) => Json(json!({ "models": models })).into_response(),
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
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }

    let stream = match state.client.stream(request).await {
        Ok(stream) => stream,
        // Errors *before* the first event come back as an HTTP status, not as an
        // SSE event: a status-200 stream containing nothing but an error event
        // would be harder for the caller to handle than a 400.
        Err(err) => return upstream_error(&err),
    };

    let sse = stream.map(|event| {
        let payload = serde_json::to_string(&event)
            .unwrap_or_else(|err| format!(r#"{{"type":"failed","message":"{err}"}}"#));
        Ok::<_, std::convert::Infallible>(SseEvent::default().data(payload))
    });

    Sse::new(sse).into_response()
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
        Json(json!({ "error": err.message, "upstream_status": err.status })),
    )
        .into_response()
}

fn unauthorized() -> axum::response::Response {
    error(StatusCode::UNAUTHORIZED, "invalid or missing API key")
}

fn error(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(json!({ "error": message }))).into_response()
}
