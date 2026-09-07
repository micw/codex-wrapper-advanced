use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codex_login::AuthDotJson;
use codex_login::AuthManager;
use codex_login::save_auth;
use codex_protocol::auth::AuthMode;

use super::*;

fn temp_home() -> PathBuf {
    std::env::temp_dir().join(format!("codex-wrapper-auth-lock-{}", uuid::Uuid::new_v4()))
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
        STORE_MODE,
        keyring_kind(),
    )
    .unwrap();
}

async fn file_manager(codex_home: &Path) -> Arc<AuthManager> {
    Arc::new(
        AuthManager::new(
            codex_home.to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            STORE_MODE,
            /*forced_chatgpt_workspace_id*/ None,
            /*chatgpt_base_url*/ None,
            keyring_kind(),
            route_config(),
        )
        .await,
    )
}

#[tokio::test]
async fn waiter_reloads_auth_written_by_previous_lock_holder() {
    let codex_home = temp_home();
    std::fs::create_dir_all(&codex_home).unwrap();
    save_api_key(&codex_home, "old-token");
    let waiting_manager = file_manager(&codex_home).await;
    assert_eq!(
        waiting_manager.auth_cached().unwrap().get_token().unwrap(),
        "old-token"
    );

    let held = acquire_auth_lock(&codex_home).await.unwrap();
    let waiter_home = codex_home.clone();
    let waiter_manager = waiting_manager.clone();
    let mut waiter = tokio::spawn(async move {
        coordinated_auth_at(&waiter_manager, &waiter_home)
            .await
            .unwrap()
            .unwrap()
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut waiter)
            .await
            .is_err(),
        "the second manager did not wait for the auth lock"
    );

    save_api_key(&codex_home, "new-token");
    drop(held);

    let auth = waiter.await.unwrap();
    assert_eq!(auth.get_token().unwrap(), "new-token");
    std::fs::remove_dir_all(codex_home).unwrap();
}
