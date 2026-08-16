//! `docs/proxy-behavior.md` §8 — keeping an access token usable.

use super::store::CredentialStore;
use super::store::Credentials;
use crate::error::ProxyError;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

/// How far ahead of expiry to refresh.
const REFRESH_MARGIN_SECONDS: u64 = 300;

/// A clock, so expiry can be tested without waiting for it.
pub trait Clock: Send + Sync {
    fn now_unix(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default()
    }
}

/// Every field is optional. The endpoint returns only what changed, so a
/// response carrying no `refresh_token` means the old one is still current
/// rather than that it was withdrawn.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    /// Refresh-token families rotate, so a response carrying a new one
    /// replaces the old. Keeping the old one after rotation invalidates the
    /// grant on its next use.
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

/// The refresh body. Three fields, and `scope` is not one of them.
#[derive(Debug, serde::Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Debug, Deserialize)]
struct TokenError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

pub struct TokenSource {
    store: Arc<dyn CredentialStore>,
    client: reqwest::Client,
    endpoint: String,
    client_id: String,
    clock: Arc<dyn Clock>,
    /// Serializes refresh. A caller that waits here re-checks the stored
    /// credentials on the way in, so concurrent callers collapse to one
    /// upstream call rather than each issuing their own.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Once a grant is refused, it stays refused. Retrying an invalid grant in
    /// a loop is how a client gets its account rate-limited for nothing.
    dead: AtomicBool,
    /// Test-visible count of refresh requests actually sent upstream.
    refreshes: AtomicU32,
}

impl TokenSource {
    pub fn new(
        store: Arc<dyn CredentialStore>,
        endpoint: impl Into<String>,
        client_id: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            client_id: client_id.into(),
            clock,
            refresh_lock: tokio::sync::Mutex::new(()),
            dead: AtomicBool::new(false),
            refreshes: AtomicU32::new(0),
        }
    }

    /// How many refresh requests reached the network.
    pub fn refresh_count(&self) -> u32 {
        self.refreshes.load(Ordering::SeqCst)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// A usable access token, refreshing first if it is due.
    pub async fn access_token(&self) -> Result<String, ProxyError> {
        if self.is_dead() {
            return Err(ProxyError::authentication(
                "the stored grant was refused and will not be retried; run `login` again",
            ));
        }

        let credentials = self
            .store
            .load()?
            .ok_or_else(|| ProxyError::authentication("not authenticated; run `login`"))?;

        if !credentials.needs_refresh(self.clock.now_unix(), REFRESH_MARGIN_SECONDS) {
            return Ok(credentials.access_token);
        }

        self.refresh().await
    }

    /// Exchange the refresh token for a new access token.
    async fn refresh(&self) -> Result<String, ProxyError> {
        let _guard = self.refresh_lock.lock().await;

        // Whoever held the lock may already have done the work. Re-reading is
        // what makes this single-flight: the second caller finds a fresh token
        // and issues no request of its own.
        let credentials = self
            .store
            .load()?
            .ok_or_else(|| ProxyError::authentication("not authenticated; run `login`"))?;
        if !credentials.needs_refresh(self.clock.now_unix(), REFRESH_MARGIN_SECONDS) {
            return Ok(credentials.access_token);
        }
        if self.is_dead() {
            return Err(ProxyError::authentication(
                "the stored grant was refused and will not be retried; run `login` again",
            ));
        }

        self.refreshes.fetch_add(1, Ordering::SeqCst);

        // §8 — `grant_type`, `refresh_token`, and `client_id`, and **never
        // `scope`**. Including it causes the authorization server to re-scope
        // the grant and invalidate sibling refresh-token families, which shows
        // up as another tool being logged out for no visible reason.
        //
        // JSON, not form encoding. The authorization code exchange is
        // form-encoded and this is not; they differ, and sending the wrong one
        // is rejected.
        let body = RefreshRequest {
            client_id: &self.client_id,
            grant_type: "refresh_token",
            refresh_token: &credentials.refresh_token,
        };

        let response = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                // The request never landed, so the grant is not implicated.
                ProxyError::overloaded(format!("could not reach the authorization server: {error}"))
            })?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            let parsed: TokenError = serde_json::from_str(&body).unwrap_or(TokenError {
                error: None,
                error_description: None,
            });
            let code = parsed.error.unwrap_or_default();

            // A refused grant is terminal. Retrying it cannot succeed, and a
            // retry loop against an authorization server is how an account
            // ends up rate-limited for nothing.
            if is_dead_grant(&code, status.as_u16()) {
                self.dead.store(true, Ordering::SeqCst);
                return Err(ProxyError::authentication(format!(
                    "the stored grant is no longer valid ({code}); run `login` again"
                )));
            }

            // Anything else may be transient, so the grant is left alone.
            return Err(ProxyError::overloaded(format!(
                "refresh failed: {}",
                parsed.error_description.unwrap_or(code)
            )));
        }

        let parsed: TokenResponse = serde_json::from_str(&body).map_err(|error| {
            ProxyError::authentication(format!("unreadable token response: {error}"))
        })?;

        let access_token = parsed
            .access_token
            .unwrap_or(credentials.access_token.clone());
        let id_token = parsed.id_token.or(credentials.id_token);

        let updated = Credentials {
            // The response carries no expiry field. It is a claim inside the
            // access token itself, so it is read from there rather than
            // invented — an absent expiry means every request refreshes.
            expires_at: super::jwt::expiry(&access_token),
            account_id: super::jwt::account_id(id_token.as_deref()).or(credentials.account_id),
            access_token: access_token.clone(),
            // Rotation: a response carrying a new refresh token replaces the
            // old one. Keeping the old one invalidates the grant on next use.
            refresh_token: parsed
                .refresh_token
                .unwrap_or(credentials.refresh_token.clone()),
            id_token,
        };

        self.store.save(&updated)?;
        Ok(access_token)
    }
}

/// Whether a refusal means the grant itself is gone.
///
/// The three specific codes are the ones this authorization server uses to say
/// so. `invalid_grant` is the standard spelling and is accepted alongside them.
/// Everything else — including a plain 400 — is treated as transient: marking a
/// grant dead on a recoverable failure forces a re-login that a retry would
/// have made unnecessary.
fn is_dead_grant(code: &str, status: u16) -> bool {
    status == 401
        || matches!(
            code,
            "refresh_token_expired"
                | "refresh_token_reused"
                | "refresh_token_invalidated"
                | "invalid_grant"
        )
}
