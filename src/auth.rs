//! Auth setup: own CODEX_HOME, official OAuth flow, token access.
//!
//! Everything here calls only public API of `codex-login`. No `auth.json` is
//! written or parsed by hand and no token endpoint is reimplemented — that is
//! exactly the line drawn in KONTEXT-HARNESS.md 8.1.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use codex_config::types::AuthCredentialsStoreMode;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::CLIENT_ID;
use codex_login::CodexAuth;
use codex_login::ServerOptions;
use codex_login::load_auth_dot_json;
use codex_login::logout_with_revoke;
use codex_login::request_device_code;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_login::token_data::parse_jwt_expiration;

/// Our own credential directory, deliberately separate from `~/.codex`.
///
/// This must neither read nor overwrite an existing Codex installation: a broken
/// login here must not take the real client down with it, and it should stay
/// visible which token came from which flow.
pub fn home() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_WRAPPER_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex-api-wrapper"))
}

/// Credentials land in a file inside our CODEX_HOME, not in the OS keyring.
///
/// That is the inspectable variant: the token can be looked at, and deleting the
/// directory leaves nothing behind in a keyring.
const STORE_MODE: AuthCredentialsStoreMode = AuthCredentialsStoreMode::File;

fn route_config() -> AuthRouteConfig {
    // `ReqwestDefault` keeps the standard behaviour (proxy from the
    // environment). The system/PAC resolution the real CLI can optionally do is
    // one more source of failure we do not need.
    AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    ))
}

fn keyring_kind() -> AuthKeyringBackendKind {
    AuthKeyringBackendKind::default()
}

/// Runs the official ChatGPT OAuth flow (PKCE, local callback server).
///
/// `run_login_server` binds a fixed port because the `redirect_uri` is
/// registered with OpenAI. On a remote machine that port has to be forwarded to
/// wherever the browser runs — which is why the URL is always printed.
pub async fn login() -> Result<()> {
    let codex_home = home()?;
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("creating CODEX_HOME: {}", codex_home.display()))?;

    // Discard previous credentials, otherwise a stale refresh token gets mixed
    // into the new flow. Failure is not fatal here (e.g. nothing to delete).
    if let Err(err) =
        logout_with_revoke(&codex_home, STORE_MODE, keyring_kind(), &route_config()).await
    {
        eprintln!("Note: existing credentials were not cleanly removed: {err}");
    }

    let mut opts = ServerOptions::new(
        codex_home.clone(),
        CLIENT_ID.to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        STORE_MODE,
        keyring_kind(),
        route_config(),
    );
    // On a headless or remote host there is no browser worth opening. The URL is
    // printed instead.
    opts.open_browser = std::env::var("CODEX_WRAPPER_NO_BROWSER").is_err();

    let server = run_login_server(opts)?;

    eprintln!(
        "Callback server listening on 127.0.0.1:{}",
        server.actual_port
    );
    eprintln!("Login URL (open in a browser):\n\n{}\n", server.auth_url);
    eprintln!("Waiting for the callback ...");

    server.block_until_done().await?;
    eprintln!(
        "Login complete. Credentials in {}",
        codex_home.display()
    );
    Ok(())
}

/// Device code flow: sign-in without a local browser and without a callback port.
///
/// For containers and remote machines this is the right way — there is no
/// `redirect_uri` that would have to point at `localhost:1455`. The user opens a
/// URL on any device and types in a code.
///
/// `probe_only` requests a code and stops there. That makes it possible to check
/// whether the flow is enabled for this account at all without forcing a
/// sign-in.
pub async fn login_device(probe_only: bool) -> Result<()> {
    let codex_home = home()?;
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("creating CODEX_HOME: {}", codex_home.display()))?;

    let opts = ServerOptions::new(
        codex_home.clone(),
        CLIENT_ID.to_string(),
        None,
        STORE_MODE,
        keyring_kind(),
        route_config(),
    );

    if probe_only {
        let code = request_device_code(&opts).await?;
        println!("Device code flow is available.");
        println!("  URL : {}", code.verification_url);
        println!("  Code: {}", code.user_code);
        println!("(probe only — nothing was signed in)");
        return Ok(());
    }

    run_device_code_login(opts).await?;
    eprintln!(
        "Login complete. Credentials in {}",
        codex_home.display()
    );
    Ok(())
}

pub async fn logout() -> Result<()> {
    let codex_home = home()?;
    logout_with_revoke(&codex_home, STORE_MODE, keyring_kind(), &route_config()).await?;
    eprintln!("Signed out, tokens revoked.");
    Ok(())
}

/// Builds the official `AuthManager` against our CODEX_HOME.
///
/// `enable_codex_api_key_env = false`: this must exercise the subscription path
/// only. A stray `CODEX_API_KEY`/`OPENAI_API_KEY` would silently skew results,
/// because measurements would then run against the platform API instead of the
/// subscription backend.
pub async fn auth_manager() -> Result<Arc<AuthManager>> {
    let codex_home = home()?;
    let manager = AuthManager::new(
        codex_home,
        /*enable_codex_api_key_env*/ false,
        STORE_MODE,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        keyring_kind(),
        route_config(),
    )
    .await;
    Ok(Arc::new(manager))
}

/// Fetches the current auth, refreshing an expired access token on the way.
pub async fn current_auth(manager: &AuthManager) -> Result<CodexAuth> {
    manager
        .auth()
        .await
        .context("not signed in — run `codex-api-wrapper login` first")
}

/// Operational readiness — the basis of the readiness probe.
///
/// Checks three things, in this order:
///
/// 1. **Signed in?** After a first rollout usually not yet.
/// 2. **Refresh permanently failed?** `auth()` swallows refresh errors and
///    returns the cached auth (see DEPLOY.md §1) — from the outside that state is
///    otherwise invisible. `refresh_failure_for_auth` is the only place it
///    surfaces.
/// 3. **Access token still valid?** Catches the case where the refresh was never
///    attempted, e.g. because the process only started after expiry.
pub async fn readiness(manager: &AuthManager) -> crate::wire::ReadyStatus {
    let not_ready = |reason, detail, secs| crate::wire::ReadyStatus {
        ready: false,
        reason,
        detail,
        access_token_expires_in_seconds: secs,
    };

    let Some(auth) = manager.auth().await else {
        return not_ready("not_authenticated", None, None);
    };
    if !auth.is_chatgpt_auth() {
        return not_ready("not_authenticated", None, None);
    }

    let expires_in = auth
        .get_token_data()
        .ok()
        .and_then(|tokens| parse_jwt_expiration(&tokens.access_token).ok().flatten())
        .map(|at| (at - chrono::Utc::now()).num_seconds());

    if let Some(failure) = manager.refresh_failure_for_auth(&auth) {
        return not_ready("refresh_failed", Some(failure.message), expires_in);
    }
    if expires_in.is_some_and(|secs| secs <= 0) {
        return not_ready("token_expired", None, expires_in);
    }

    crate::wire::ReadyStatus {
        ready: true,
        reason: "ok",
        detail: None,
        access_token_expires_in_seconds: expires_in,
    }
}

pub async fn status(manager: &AuthManager) -> Result<crate::wire::AuthStatus> {
    let Some(auth) = manager.auth().await else {
        return Ok(crate::wire::AuthStatus {
            authenticated: false,
            account_id: None,
            email: None,
            plan: None,
            chatgpt_user_id: None,
            workspace_account: false,
            fedramp: false,
            access_token_expires_at: None,
            access_token_expires_in_seconds: None,
            last_refresh: None,
            last_refresh_age_seconds: None,
        });
    };

    let now = chrono::Utc::now();

    // The expiry lives in the access token's JWT.
    let expires_at = auth
        .get_token_data()
        .ok()
        .and_then(|tokens| parse_jwt_expiration(&tokens.access_token).ok().flatten());

    // `last_refresh` hangs off AuthDotJson, not TokenData, and reaching it
    // through CodexAuth is private upstream. So read from disk once — cheap
    // enough for a status endpoint.
    let last_refresh = load_auth_dot_json(&home()?, STORE_MODE, keyring_kind())
        .ok()
        .flatten()
        .and_then(|auth_json| auth_json.last_refresh);

    Ok(crate::wire::AuthStatus {
        authenticated: auth.is_chatgpt_auth(),
        account_id: auth.get_account_id(),
        email: auth.get_account_email(),
        plan: auth.account_plan_type().map(|p| format!("{p:?}")),
        chatgpt_user_id: auth.get_chatgpt_user_id(),
        workspace_account: auth.is_workspace_account(),
        fedramp: auth.is_fedramp_account(),
        access_token_expires_at: expires_at.map(|at| at.to_rfc3339()),
        access_token_expires_in_seconds: expires_at.map(|at| (at - now).num_seconds()),
        last_refresh: last_refresh.map(|at| at.to_rfc3339()),
        last_refresh_age_seconds: last_refresh.map(|at| (now - at).num_seconds()),
    })
}

pub async fn whoami() -> Result<()> {
    let manager = auth_manager().await?;
    let auth = current_auth(&manager).await?;

    println!("CODEX_HOME      : {}", home()?.display());
    println!("Auth mode       : {:?}", auth.auth_mode());
    println!("ChatGPT auth    : {}", auth.is_chatgpt_auth());
    println!("Codex backend   : {}", auth.uses_codex_backend());
    println!("E-mail          : {}", opt(auth.get_account_email()));
    println!("Account ID      : {}", opt(auth.get_account_id()));
    println!("ChatGPT user ID : {}", opt(auth.get_chatgpt_user_id()));
    println!(
        "Plan            : {}",
        opt(auth.account_plan_type().map(|p| format!("{p:?}")))
    );
    println!("Workspace acct  : {}", auth.is_workspace_account());
    println!("FedRAMP         : {}", auth.is_fedramp_account());

    match auth.get_token() {
        // Never print the whole token — it would end up in terminal scrollback
        // and in every log that records this.
        Ok(token) => println!(
            "Access token    : {} ... ({} characters)",
            &token[..token.len().min(12)],
            token.len()
        ),
        Err(err) => println!("Access token    : not available ({err})"),
    }
    Ok(())
}

fn opt(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_string())
}
