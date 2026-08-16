//! `docs/proxy-behavior.md` §4.2 — HTTP with SSE.

use super::EventStream;
use super::Transport;
use crate::error::ProxyError;
use axum::http::StatusCode;
use codex_cc_proxy_core::responses::ResponsesRequest;
use codex_cc_proxy_core::sse::SseDecoder;
use futures::StreamExt;
use futures::stream;
use std::sync::Arc;

/// The identity this proxy presents upstream.
///
/// One originator, always, with no alternate to fall back to. A fallback
/// identity is state that has to be tracked, it invalidates the prompt cache
/// when it changes, and it turns one clear failure into two unclear ones
/// (§2.8).
pub const ORIGINATOR: &str = "codex_cli_rs";

/// The user agent that goes with it.
pub const USER_AGENT: &str = concat!("codex_cli_rs/", env!("CARGO_PKG_VERSION"));

pub struct HttpTransport {
    client: reqwest::Client,
    endpoint: String,
    /// Asked for a token per request rather than handed one at construction.
    /// A token captured once goes stale the moment the session refreshes, and
    /// the failure is a 401 halfway through a working conversation.
    ///
    /// `None` for a replay server, which wants no credentials at all.
    credentials: Option<Arc<crate::auth::tokens::TokenSource>>,
}

impl HttpTransport {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            credentials: None,
        }
    }

    pub fn with_credentials(mut self, credentials: Arc<crate::auth::tokens::TokenSource>) -> Self {
        self.credentials = Some(credentials);
        self
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn stream(&self, request: &ResponsesRequest) -> Result<EventStream, ProxyError> {
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header(axum::http::header::ACCEPT, "text/event-stream")
            .header("originator", ORIGINATOR)
            .header(axum::http::header::USER_AGENT, USER_AGENT)
            .json(request);

        if let Some(credentials) = &self.credentials {
            builder = builder.bearer_auth(credentials.access_token().await?);
            if let Some(account) = credentials.account_id() {
                builder = builder.header("chatgpt-account-id", account);
            }
        }

        let response = builder.send().await.map_err(|error| {
            // Nothing was sent, so this is retryable: the client's own backoff
            // is the right place to handle a connection that did not open.
            ProxyError::overloaded(format!("upstream request failed: {error}"))
        })?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = response.text().await.unwrap_or_default();

            let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            // A challenge response — a non-JSON body on a 403 — is reported
            // with its excerpt intact, because the excerpt is the only
            // diagnostic available (§2.8).
            return Err(ProxyError::from_upstream_status(status, excerpt(&body))
                .with_retry_after(retry_after));
        }

        let mut decoder = SseDecoder::default();
        let byte_stream = response.bytes_stream();

        let events = byte_stream
            .flat_map(move |chunk| match chunk {
                Ok(bytes) => {
                    let payloads: Vec<Result<String, ProxyError>> =
                        decoder.push(&bytes).map(Ok).collect();
                    stream::iter(payloads)
                }
                Err(error) => stream::iter(vec![Err(ProxyError::overloaded(format!(
                    "upstream stream failed: {error}"
                )))]),
            })
            .boxed();

        Ok(events)
    }
}

/// Upstream bodies can be large. Enough to diagnose, not enough to fill a log.
fn excerpt(body: &str) -> String {
    const LIMIT: usize = 500;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "upstream returned no body".to_owned();
    }
    match trimmed.char_indices().nth(LIMIT) {
        Some((index, _)) => format!("{}…", &trimmed[..index]),
        None => trimmed.to_owned(),
    }
}
