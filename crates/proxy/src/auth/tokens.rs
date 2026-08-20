//! `docs/proxy-behavior.md` §8 — keeping an access token usable.

use super::store::CredentialStore;
use super::store::Credentials;
use crate::error::ProxyError;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::Mutex;
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
    /// replaces the old. The superseded token was measured to keep working, so
    /// this is not about avoiding a broken grant — it is that the new one
    /// carries the current lifetime, and a family left to age out is one that
    /// eventually cannot be renewed at all.
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    /// Seconds of remaining life, relative to receipt. Used only when the
    /// access token itself carries no readable claim.
    #[serde(default)]
    expires_in: Option<u64>,
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
    /// The refresh token the backend refused, if it has refused one.
    ///
    /// A refusal is about that token, and holding it as a bare flag makes it a
    /// fact about the process instead: the message a refusal produces tells
    /// the operator to log in again, and the grant they produce lands in the
    /// store this reads on every turn — where a flag would go on refusing it
    /// until the daemon restarts. Holding the token itself keeps both halves:
    /// a new grant is tried, and the refused one is never retried.
    refused: Mutex<Option<String>>,
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
            refused: Mutex::new(None),
            refreshes: AtomicU32::new(0),
        }
    }

    /// How many refresh requests reached the network.
    pub fn refresh_count(&self) -> u32 {
        self.refreshes.load(Ordering::SeqCst)
    }

    /// Whether the grant currently stored is one the backend has refused.
    ///
    /// Answered against the store rather than from a flag, so switching
    /// accounts or authorizing a new one is enough to end it, and switching
    /// back to a refused grant is enough to bring it back.
    pub fn is_dead(&self) -> bool {
        match self.store.load() {
            Ok(Some(credentials)) => self.is_refused(&credentials.refresh_token),
            _ => false,
        }
    }

    fn is_refused(&self, refresh_token: &str) -> bool {
        self.refused
            .lock()
            .ok()
            .and_then(|refused| refused.clone())
            .is_some_and(|refused| refused == refresh_token)
    }

    /// The account this grant belongs to, sent upstream as a header.
    pub fn account_id(&self) -> Option<String> {
        self.store.load().ok().flatten().and_then(|c| c.account_id)
    }

    /// A usable access token, refreshing first if it is due.
    pub async fn access_token(&self) -> Result<String, ProxyError> {
        let credentials = self
            .store
            .load()?
            .ok_or_else(|| ProxyError::authentication("not authenticated; run `login`"))?;

        if self.is_refused(&credentials.refresh_token) {
            return Err(ProxyError::authentication(
                "the stored grant was refused and will not be retried; run `login` again",
            ));
        }

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
        if self.is_refused(&credentials.refresh_token) {
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
                if let Ok(mut refused) = self.refused.lock() {
                    *refused = Some(credentials.refresh_token.clone());
                }
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
            // The claim inside the access token first: it is what the backend
            // validates against, so it is the expiry that decides. The
            // response's own `expires_in` is the fallback for a token this
            // proxy cannot read, and it matters — an absent expiry is treated
            // as expired, so falling through to nothing would refresh on every
            // single request.
            //
            // Measured, not assumed: a live refresh returns `expires_in`, and
            // it agreed with the token's `exp` to within a second.
            expires_at: super::jwt::expiry(&access_token).or_else(|| {
                parsed
                    .expires_in
                    .map(|lifetime| self.clock.now_unix().saturating_add(lifetime))
            }),
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
