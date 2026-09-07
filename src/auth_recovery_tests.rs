use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_config::types::AuthCredentialsStoreMode;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::CodexAuth;
use codex_login::save_auth;
use codex_protocol::auth::AuthMode;
use serde_json::json;

use super::*;

type RecoverySteps = Arc<Mutex<VecDeque<(&'static str, Result<(), String>)>>>;

#[derive(Clone)]
struct ScriptedRecovery {
    steps: RecoverySteps,
}

impl ScriptedRecovery {
    fn successful(steps: &[&'static str]) -> Self {
        Self {
            steps: Arc::new(Mutex::new(
                steps.iter().copied().map(|step| (step, Ok(()))).collect(),
            )),
        }
    }
}

impl Recovery for ScriptedRecovery {
    fn has_next(&self) -> bool {
        self.steps.lock().unwrap().front().is_some()
    }

    fn step_name(&self) -> &'static str {
        self.steps
            .lock()
            .unwrap()
            .front()
            .map(|(name, _)| *name)
            .unwrap_or("done")
    }

    async fn next(&mut self, _rejected: Option<AuthFingerprint>) -> Result<(), String> {
        self.steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted recovery step")
            .1
    }
}

fn manager(token: &str) -> Arc<AuthManager> {
    AuthManager::from_auth_for_testing(CodexAuth::from_api_key(token))
}

fn upstream(status: u16) -> UpstreamError {
    UpstreamError {
        status: Some(status),
        message: format!("HTTP {status}"),
        body: Some(json!({ "status": status })),
    }
}

fn record(tracker: &AuthTracker, auth: &CodexAuth) {
    tracker.record(auth);
}

fn save_api_key(codex_home: &Path, token: &str) {
    save_auth(
        codex_home,
        &AuthDotJson {
            auth_mode: Some(AuthMode::ApiKey),
            openai_api_key: Some(token.to_string()),
            tokens: None,
            last_refresh: None,
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
        },
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .unwrap();
}

async fn file_manager(codex_home: &Path) -> Arc<AuthManager> {
    Arc::new(
        AuthManager::new(
            codex_home.to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            AuthCredentialsStoreMode::File,
            /*forced_chatgpt_workspace_id*/ None,
            /*chatgpt_base_url*/ None,
            AuthKeyringBackendKind::default(),
            AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            )),
        )
        .await,
    )
}

#[tokio::test]
async fn success_does_not_run_recovery() {
    let manager = manager("token-a");
    let auth = manager.auth_cached().unwrap();
    let health = AuthHealth::default();
    let recovery = ScriptedRecovery::successful(&["reload", "refresh_token"]);
    let remaining = recovery.steps.clone();
    let attempts = Arc::new(Mutex::new(0));
    let seen = attempts.clone();

    let result = run_with_recovery(&manager, &health, recovery, move |tracker| {
        record(&tracker, &auth);
        *seen.lock().unwrap() += 1;
        std::future::ready(Ok("ok"))
    })
    .await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(*attempts.lock().unwrap(), 1);
    assert_eq!(remaining.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn reload_can_recover_the_first_unauthorized() {
    let manager = manager("token-a");
    let auth = manager.auth_cached().unwrap();
    let health = AuthHealth::default();
    let recovery = ScriptedRecovery::successful(&["reload", "refresh_token"]);
    let remaining = recovery.steps.clone();
    let attempts = Arc::new(Mutex::new(0));
    let seen = attempts.clone();

    let result = run_with_recovery(&manager, &health, recovery, move |tracker| {
        record(&tracker, &auth);
        let mut count = seen.lock().unwrap();
        *count += 1;
        std::future::ready(if *count == 1 {
            Err(upstream(401))
        } else {
            Ok(())
        })
    })
    .await;

    result.unwrap();
    assert_eq!(*attempts.lock().unwrap(), 2);
    assert_eq!(remaining.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refresh_recovers_a_backend_expired_token() {
    let manager = manager("token-a");
    let auth = manager.auth_cached().unwrap();
    let health = AuthHealth::default();
    let recovery = ScriptedRecovery::successful(&["reload", "refresh_token"]);
    let remaining = recovery.steps.clone();
    let attempts = Arc::new(Mutex::new(0));
    let seen = attempts.clone();

    let result = run_with_recovery(&manager, &health, recovery, move |tracker| {
        record(&tracker, &auth);
        let mut count = seen.lock().unwrap();
        *count += 1;
        std::future::ready(if *count < 3 {
            Err(upstream(401))
        } else {
            Ok(())
        })
    })
    .await;

    result.unwrap();
    assert_eq!(*attempts.lock().unwrap(), 3);
    assert!(remaining.lock().unwrap().is_empty());
}

#[tokio::test]
async fn recovery_is_bounded_and_latches_the_final_unauthorized() {
    let manager = manager("token-a");
    let auth = manager.auth_cached().unwrap();
    let health = AuthHealth::default();
    let recovery = ScriptedRecovery::successful(&["reload", "refresh_token"]);
    let attempts = Arc::new(Mutex::new(0));
    let seen = attempts.clone();

    let error = run_with_recovery(&manager, &health, recovery, move |tracker| {
        record(&tracker, &auth);
        *seen.lock().unwrap() += 1;
        std::future::ready(Err::<(), _>(upstream(401)))
    })
    .await
    .unwrap_err();

    assert_eq!(error.status, Some(401));
    assert_eq!(*attempts.lock().unwrap(), 3);
    assert_eq!(
        health
            .rejection_for(&manager.auth_cached().unwrap())
            .as_deref(),
        Some("upstream rejected credentials after authentication recovery")
    );
}

#[tokio::test]
async fn non_401_errors_are_not_retried() {
    for status in [403, 429, 500] {
        let manager = manager("token-a");
        let auth = manager.auth_cached().unwrap();
        let health = AuthHealth::default();
        let recovery = ScriptedRecovery::successful(&["reload", "refresh_token"]);
        let remaining = recovery.steps.clone();
        let attempts = Arc::new(Mutex::new(0));
        let seen = attempts.clone();

        let error = run_with_recovery(&manager, &health, recovery, move |tracker| {
            record(&tracker, &auth);
            *seen.lock().unwrap() += 1;
            std::future::ready(Err::<(), _>(upstream(status)))
        })
        .await
        .unwrap_err();

        assert_eq!(error.status, Some(status));
        assert_eq!(*attempts.lock().unwrap(), 1);
        assert_eq!(remaining.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn recovery_failure_stops_replays_and_is_reported() {
    let manager = manager("token-a");
    let auth = manager.auth_cached().unwrap();
    let health = AuthHealth::default();
    let recovery = ScriptedRecovery {
        steps: Arc::new(Mutex::new(VecDeque::from([(
            "reload",
            Err("disk unavailable".to_string()),
        )]))),
    };
    let attempts = Arc::new(Mutex::new(0));
    let seen = attempts.clone();

    let error = run_with_recovery(&manager, &health, recovery, move |tracker| {
        record(&tracker, &auth);
        *seen.lock().unwrap() += 1;
        std::future::ready(Err::<(), _>(upstream(401)))
    })
    .await
    .unwrap_err();

    assert_eq!(*attempts.lock().unwrap(), 1);
    assert!(error.message.contains("reload failed: disk unavailable"));
}

#[test]
fn stale_failures_do_not_poison_new_credentials_or_clear_new_failures() {
    let old = CodexAuth::from_api_key("old-token");
    let new = CodexAuth::from_api_key("new-token");
    let manager = AuthManager::from_auth_for_testing(new.clone());
    let health = AuthHealth::default();

    health.mark_unauthorized(
        &manager,
        fingerprint(&old),
        "late rejection from old request",
    );
    assert_eq!(health.rejection_for(&new), None);

    health.mark_unauthorized(&manager, fingerprint(&new), "new token rejected");
    health.mark_success(fingerprint(&old));
    assert_eq!(
        health.rejection_for(&new).as_deref(),
        Some("new token rejected")
    );

    health.mark_success(fingerprint(&new));
    assert_eq!(health.rejection_for(&new), None);
}

#[tokio::test]
async fn final_backend_rejection_makes_readiness_fail_until_success() {
    let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let manager = AuthManager::from_auth_for_testing(auth.clone());
    let health = AuthHealth::default();

    health.mark_unauthorized(&manager, fingerprint(&auth), "new token rejected");
    let rejected = crate::auth::readiness_with_auth(&manager, &health, Some(auth.clone()));
    assert!(!rejected.ready);
    assert_eq!(rejected.reason, "upstream_unauthorized");
    assert_eq!(rejected.detail.as_deref(), Some("new token rejected"));

    health.mark_success(fingerprint(&auth));
    let recovered = crate::auth::readiness_with_auth(&manager, &health, Some(auth));
    assert!(recovered.ready);
    assert_eq!(recovered.reason, "ok");
}

#[tokio::test]
async fn refresh_waiter_reloads_and_skips_refresh_after_another_process_updates_auth() {
    let codex_home = std::env::temp_dir().join(format!(
        "codex-wrapper-auth-recovery-lock-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&codex_home).unwrap();
    save_api_key(&codex_home, "old-token");
    let manager = file_manager(&codex_home).await;
    let rejected = fingerprint(&manager.auth_cached().unwrap());
    let recovery = CoordinatedRecovery::new(manager.clone(), codex_home.clone());

    let held = crate::auth::acquire_auth_lock(&codex_home).await.unwrap();
    let mut waiter = tokio::spawn(async move { recovery.refresh_if_unchanged(rejected).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut waiter)
            .await
            .is_err(),
        "the recovery did not wait for the auth lock"
    );

    save_api_key(&codex_home, "new-token");
    drop(held);

    assert!(
        !waiter.await.unwrap().unwrap(),
        "a second refresh was attempted"
    );
    assert_eq!(
        manager.auth_cached().unwrap().get_token().unwrap(),
        "new-token"
    );
    std::fs::remove_dir_all(codex_home).unwrap();
}
