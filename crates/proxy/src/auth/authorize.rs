//! `docs/proxy-behavior.md` §8 — turning a stored credential into headers.
//!
//! Two kinds of credential reach two different endpoints, and the difference
//! between them is a header set. Resolving that in one place is what keeps a
//! subscription header off a key request: every path that authenticates asks
//! here rather than assembling its own.

use super::store::AccountStore;
use super::store::Credential;
use super::tokens::TokenSource;
use crate::error::ProxyError;
use std::sync::Arc;

/// Which family of endpoint a credential belongs to.
///
/// Not a label on the credential so much as a statement about where it may be
/// spent. A key sent to a subscription endpoint is refused there, and the
/// refusal names an expired token, which is not what happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Subscription,
    Key,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Key => "key",
        }
    }
}

/// One credential, resolved into what goes on the wire.
#[derive(Clone, Debug)]
pub struct Authorization {
    pub kind: Kind,
    /// Every header this credential requires, including the one identifying
    /// the client where the endpoint expects it. `Debug` is derived, so this
    /// carries a bearer token: it is never logged, and the two types it is
    /// built from redact themselves for that reason.
    pub headers: Vec<(String, String)>,
}

impl Authorization {
    /// Put these headers on a request.
    pub fn apply(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }

    /// This credential, if it belongs where it is about to be spent.
    ///
    /// A refusal rather than a fallback: a key sent to a subscription endpoint
    /// comes back as a message about an invalid token, which sends whoever
    /// reads it looking for the wrong problem. Both halves are named, because
    /// either one could be the half that is wrong.
    pub fn for_endpoint(self, expected: Kind) -> Result<Self, ProxyError> {
        if self.kind == expected {
            return Ok(self);
        }
        Err(ProxyError::invalid_request(format!(
            "the account serving turns holds a {} credential, and this endpoint takes a {} one; \
             select an account of the other kind, or point the {} endpoint somewhere it belongs",
            self.kind.as_str(),
            expected.as_str(),
            expected.as_str()
        )))
    }
}

#[async_trait::async_trait]
pub trait Authorizer: Send + Sync {
    /// The headers for one upstream request.
    ///
    /// Resolved per request rather than captured: a token taken once goes
    /// stale at the next refresh, and the account serving turns can change
    /// between one request and the next.
    async fn authorize(&self) -> Result<Authorization, ProxyError>;
}

/// Which kind the account serving turns holds, for a caller that has to pick
/// an endpoint before it has a request to authorize.
///
/// A store that cannot be read answers `Subscription`, which is what this
/// project was before there was a second kind. The mistake surfaces at the
/// next request as a refusal naming both halves rather than as a turn sent
/// somewhere it does not belong.
pub fn selected_kind(store: &Arc<dyn AccountStore>) -> Kind {
    match store.credential() {
        Ok(Some(Credential::Key(_))) => Kind::Key,
        _ => Kind::Subscription,
    }
}

/// The credential of whichever account is serving turns.
///
/// The store is read on every request, so a switch or a login reaches the next
/// request without anything being rebuilt. A grant goes through the token
/// source, which is where refresh and refusal live; a key is spent as it is,
/// because there is nothing to refresh and nothing to expire.
pub struct AccountAuthorizer {
    store: Arc<dyn AccountStore>,
    grants: Arc<TokenSource>,
}

impl AccountAuthorizer {
    pub fn new(store: Arc<dyn AccountStore>, grants: Arc<TokenSource>) -> Self {
        Self { store, grants }
    }
}

#[async_trait::async_trait]
impl Authorizer for AccountAuthorizer {
    async fn authorize(&self) -> Result<Authorization, ProxyError> {
        match self.store.credential()? {
            Some(Credential::Grant(_)) => self.grants.authorize().await,
            Some(Credential::Key(key)) => Ok(Authorization {
                kind: Kind::Key,
                // One header. A key identifies nothing but itself: no account
                // to name, and no client to identify to an endpoint that does
                // not ask.
                headers: vec![(
                    axum::http::header::AUTHORIZATION.to_string(),
                    format!("Bearer {}", key.value()),
                )],
            }),
            None => Err(ProxyError::authentication("not authenticated; run `login`")),
        }
    }
}

#[async_trait::async_trait]
impl Authorizer for TokenSource {
    async fn authorize(&self) -> Result<Authorization, ProxyError> {
        let mut headers = vec![
            (
                axum::http::header::AUTHORIZATION.to_string(),
                format!("Bearer {}", self.access_token().await?),
            ),
            // §2.8 — required on every subscription path, and its absence is a
            // bare 400 that names nothing.
            (
                "originator".to_owned(),
                crate::upstream::http::ORIGINATOR.to_owned(),
            ),
        ];
        if let Some(account) = self.account_id() {
            headers.push(("chatgpt-account-id".to_owned(), account));
        }

        Ok(Authorization {
            kind: Kind::Subscription,
            headers,
        })
    }
}
