//! Reactive authentication recovery and backend-observed auth health.
//!
//! `AuthManager::auth()` refreshes proactively from the JWT expiry. A backend can
//! reject an otherwise locally valid token earlier, though (for example after an
//! account plan change). This module adds the part that normally lives in
//! `codex-core`: bounded recovery after an actual HTTP 401.

use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;

use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::UnauthorizedRecovery;

use crate::client::UpstreamError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AuthFingerprint(u64);

fn fingerprint(auth: &CodexAuth) -> Option<AuthFingerprint> {
    let token = auth.get_token().ok()?;
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    Some(AuthFingerprint(hasher.finish()))
}

/// Records which exact credential was attached to one backend attempt.
///
/// The fingerprint, rather than "whatever token is current when the response
/// arrives", prevents a late 401 from an old concurrent request from poisoning a
/// newer successful login.
#[derive(Clone, Default)]
pub(crate) struct AuthTracker(Arc<Mutex<Option<AuthFingerprint>>>);

impl AuthTracker {
    pub(crate) fn record(&self, auth: &CodexAuth) {
        if let Ok(mut used) = self.0.lock() {
            *used = fingerprint(auth);
        }
    }

    fn get(&self) -> Option<AuthFingerprint> {
        self.0.lock().ok().and_then(|used| *used)
    }
}

#[derive(Clone, Debug)]
struct RejectedAuth {
    fingerprint: AuthFingerprint,
    detail: String,
}

/// Backend-observed state which `AuthManager` cannot infer from JWT claims.
#[derive(Default)]
pub(crate) struct AuthHealth {
    rejected: Mutex<Option<RejectedAuth>>,
}

impl AuthHealth {
    fn current_fingerprint(manager: &AuthManager) -> Option<AuthFingerprint> {
        manager.auth_cached().as_ref().and_then(fingerprint)
    }

    fn mark_unauthorized(
        &self,
        manager: &AuthManager,
        used: Option<AuthFingerprint>,
        detail: impl Into<String>,
    ) {
        let Some(used) = used else {
            return;
        };
        // Ignore a response made with credentials which another request has
        // already replaced in the shared AuthManager.
        if Self::current_fingerprint(manager) != Some(used) {
            return;
        }
        if let Ok(mut rejected) = self.rejected.lock() {
            *rejected = Some(RejectedAuth {
                fingerprint: used,
                detail: detail.into(),
            });
        }
    }

    fn mark_success(&self, used: Option<AuthFingerprint>) {
        let Some(used) = used else {
            return;
        };
        if let Ok(mut rejected) = self.rejected.lock()
            && rejected
                .as_ref()
                .is_some_and(|rejected| rejected.fingerprint == used)
        {
            *rejected = None;
        }
    }

    /// Returns a rejection only while it still belongs to the current token.
    pub(crate) fn rejection_for(&self, auth: &CodexAuth) -> Option<String> {
        let current = fingerprint(auth)?;
        self.rejected.lock().ok().and_then(|rejected| {
            rejected
                .as_ref()
                .filter(|rejected| rejected.fingerprint == current)
                .map(|rejected| rejected.detail.clone())
        })
    }
}

/// Runs one logical backend operation with the official bounded 401 recovery.
///
/// `UnauthorizedRecovery` performs at most two steps for managed ChatGPT auth:
/// reload credentials from disk, then refresh them at the OAuth authority. Each
/// successful step permits exactly one replay. Other HTTP statuses never enter
/// this path.
pub(crate) async fn run_with_unauthorized_recovery<T, F, Fut>(
    manager: &Arc<AuthManager>,
    health: &AuthHealth,
    attempt: F,
) -> Result<T, UpstreamError>
where
    F: FnMut(AuthTracker) -> Fut,
    Fut: Future<Output = Result<T, UpstreamError>>,
{
    run_with_recovery(manager, health, manager.unauthorized_recovery(), attempt).await
}

trait Recovery {
    fn has_next(&self) -> bool;
    fn step_name(&self) -> &'static str;
    fn next(&mut self) -> impl Future<Output = Result<(), String>> + Send;
}

impl Recovery for UnauthorizedRecovery {
    fn has_next(&self) -> bool {
        self.has_next()
    }

    fn step_name(&self) -> &'static str {
        self.step_name()
    }

    async fn next(&mut self) -> Result<(), String> {
        UnauthorizedRecovery::next(self)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

async fn run_with_recovery<T, F, Fut, R>(
    manager: &Arc<AuthManager>,
    health: &AuthHealth,
    mut recovery: R,
    mut attempt: F,
) -> Result<T, UpstreamError>
where
    F: FnMut(AuthTracker) -> Fut,
    Fut: Future<Output = Result<T, UpstreamError>>,
    R: Recovery,
{
    loop {
        let tracker = AuthTracker::default();
        let result = attempt(tracker.clone()).await;
        let used = tracker.get();

        match result {
            Ok(value) => {
                health.mark_success(used);
                return Ok(value);
            }
            Err(error) if error.status == Some(401) && recovery.has_next() => {
                let step = recovery.step_name();
                eprintln!("upstream returned 401; auth recovery step={step}");
                if let Err(recovery_error) = recovery.next().await {
                    health.mark_unauthorized(
                        manager,
                        used,
                        format!("authentication recovery step {step} failed: {recovery_error}"),
                    );
                    return Err(UpstreamError {
                        status: error.status,
                        message: format!(
                            "{} (authentication recovery step {step} failed: {recovery_error})",
                            error.message
                        ),
                        body: error.body,
                    });
                }
                eprintln!("auth recovery step={step} completed; retrying request");
            }
            Err(error) if error.status == Some(401) => {
                health.mark_unauthorized(
                    manager,
                    used,
                    "upstream rejected credentials after authentication recovery",
                );
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
#[path = "auth_recovery_tests.rs"]
mod tests;
