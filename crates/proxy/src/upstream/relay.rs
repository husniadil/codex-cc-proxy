//! `docs/proxy-behavior.md` §9 — the Messages relay.
//!
//! The second provider speaks the surface this proxy already exposes, so a
//! turn belonging to it is forwarded rather than translated. Nothing here
//! parses, maps, or re-encodes the payload: the bytes the client sent are the
//! bytes the backend receives, and the bytes it answers with are the bytes the
//! client reads.
//!
//! That is a rule rather than an observation. Translation would round-trip the
//! body through this proxy's own types, and every field they do not model —
//! today's and next release's — would be dropped somewhere no test looks. The
//! only thing that changes on the way is the header set, which §9.2 states
//! exactly.

use crate::auth::authorize::Authorizer;
use crate::auth::authorize::Kind;
use crate::auth::store::AccountStore;
use crate::auth::store::Provider;
use crate::error::ProxyError;
use axum::body::Body;
use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::response::Response;
use std::sync::Arc;

/// Headers that describe this hop rather than the message.
///
/// Forwarding one makes a claim about a connection the backend is not on. The
/// length and the transfer coding belong to whoever writes the request, which
/// is the HTTP client, and `accept-encoding` negotiates a coding this proxy
/// does not decode — asking for one would leave it relaying bytes the client
/// never agreed to.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "accept-encoding",
];

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

/// Where a turn for the second provider goes, and what it goes as.
pub struct Relay {
    client: reqwest::Client,
    endpoint: String,
    /// Read per request rather than captured, so a login or a switch reaches
    /// the next turn without anything being rebuilt.
    store: Arc<dyn AccountStore>,
    credentials: Arc<dyn Authorizer>,
}

impl Relay {
    pub fn new(
        endpoint: impl Into<String>,
        store: Arc<dyn AccountStore>,
        credentials: Arc<dyn Authorizer>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            store,
            credentials,
        }
    }

    /// The account this mapping names, if its turns belong on this path.
    ///
    /// `None` for the account — the mapping pinned nobody — resolves to
    /// whichever account is serving turns. An unpinned tier has always meant
    /// that, and a key selected for this provider would otherwise send every
    /// turn to the other provider's endpoint.
    ///
    /// The answer is the account's *name* either way, because the relay
    /// authenticates by name: a credential read by name is the same credential
    /// whether or not it happens to be selected, and reading it by name is
    /// what lets a refusal say which account it was.
    ///
    /// A name the store does not hold answers `None`, which sends the turn
    /// down the translating path — where the same name is already refused by
    /// the store, with a message naming it. Refusing here as well would give
    /// one mistake two different errors.
    fn serves(&self, account: Option<&str>) -> Result<Option<String>, ProxyError> {
        Ok(self
            .store
            .accounts()?
            .into_iter()
            .find(|stored| match account {
                Some(account) => stored.name == account,
                None => stored.selected,
            })
            .filter(|stored| stored.provider == Provider::Anthropic.as_str())
            .map(|stored| stored.name))
    }

    /// Which account serves this model id, where one does.
    ///
    /// Routing is by model id because the body carries an id: this path never
    /// rewrites the model, so the client was handed the final one at launch.
    /// Two accounts claiming the same id leaves nothing to tell them apart,
    /// and that is refused rather than picked — picking spends a subscription
    /// nobody pointed at the turn, and says nothing.
    ///
    /// The refusal is scoped to ids this path actually claims. Two tiers of
    /// the first provider sharing an upstream model is ordinary and decides
    /// nothing, so it stays what it has always been.
    pub fn account_for(
        &self,
        model: &str,
        mappings: &[crate::ingress::ModelMapping],
    ) -> Result<Option<String>, ProxyError> {
        let mut claimants: Vec<Option<&str>> = Vec::new();
        for mapping in mappings.iter().filter(|mapping| mapping.upstream == model) {
            let account = mapping.account.as_deref();
            if !claimants.contains(&account) {
                claimants.push(account);
            }
        }

        let mut relaying = Vec::new();
        for account in &claimants {
            if let Some(name) = self.serves(*account)? {
                relaying.push(name);
            }
        }

        let Some(first) = relaying.first() else {
            return Ok(None);
        };

        if claimants.len() > 1 {
            let named = claimants
                .iter()
                .map(|account| match account {
                    Some(account) => format!("`{account}`"),
                    None => "the account serving turns".to_owned(),
                })
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(ProxyError::invalid_request(format!(
                "`{model}` is claimed by {named}, and a request carries a model id \
                 rather than a tier name — so there is nothing left to say which of \
                 them the turn belongs to. Give each account its own model id."
            )));
        }

        Ok(Some(first.clone()))
    }

    /// Forward one turn and stream the answer back.
    ///
    /// The status, the body, and every response header that is not this hop's
    /// pass through as they arrive. A refusal is already in the client's error
    /// shape, so rewrapping it would restate a message the backend wrote — and
    /// a rewrap that loses the type takes the client's own retry logic with it.
    pub async fn forward(
        &self,
        account: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<Response, ProxyError> {
        let authorization = self
            .credentials
            .authorize(Some(account))
            .await?
            .for_endpoint(Kind::Key)?;

        let mut request = self.client.post(&self.endpoint);
        for (name, value) in headers {
            // The client's own bearer is a placeholder — `ANTHROPIC_AUTH_TOKEN`
            // has to be set for the client's sake and its value is ignored
            // (§8). It is replaced rather than forwarded. So is any key the
            // client carries: a turn authenticated as whatever the caller held
            // is a turn this proxy did not route.
            if is_hop_by_hop(name)
                || name == axum::http::header::AUTHORIZATION
                || name == "x-api-key"
            {
                continue;
            }
            request = request.header(name, value);
        }
        let response = authorization
            .apply(request)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                // Nothing was sent, so this is retryable, and the client's own
                // backoff is the right place for a connection that did not
                // open (§4.2).
                ProxyError::overloaded(format!("upstream request failed: {error}"))
            })?;

        let mut relayed = Response::builder().status(response.status());
        for (name, value) in response.headers() {
            if is_hop_by_hop(name) {
                continue;
            }
            relayed = relayed.header(name, value);
        }

        relayed
            .body(Body::from_stream(futures::TryStreamExt::map_err(
                response.bytes_stream(),
                std::io::Error::other,
            )))
            .map_err(|error| {
                ProxyError::overloaded(format!("could not relay the response: {error}"))
            })
    }
}
