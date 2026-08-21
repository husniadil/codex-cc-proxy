//! `docs/proxy-behavior.md` §4.2 — HTTP with SSE.

use super::EventStream;
use super::Transport;
use crate::error::ProxyError;
use axum::http::StatusCode;
use futures::StreamExt;
use futures::stream;
use proxenos_core::responses::ResponsesRequest;
use proxenos_core::sse::SseDecoder;
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
    /// §4.4 — compress the body, and say so.
    compression: bool,
    /// Asked for a token per request rather than handed one at construction.
    /// A token captured once goes stale the moment the session refreshes, and
    /// the failure is a 401 halfway through a working conversation.
    ///
    /// `None` for a replay server, which wants no credentials at all.
    credentials: Option<Arc<dyn crate::auth::authorize::Authorizer>>,
    /// Which family of credential this endpoint takes. A transport built
    /// without being told takes a subscription's, which is what every endpoint
    /// in this project was until there was a second kind.
    kind: crate::auth::authorize::Kind,
}

impl HttpTransport {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            compression: false,
            credentials: None,
            kind: crate::auth::authorize::Kind::Subscription,
        }
    }

    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compression = compression;
        self
    }

    /// Which credential kind this endpoint expects.
    pub fn for_endpoint(mut self, kind: crate::auth::authorize::Kind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_credentials(
        mut self,
        credentials: Arc<dyn crate::auth::authorize::Authorizer>,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn stream(
        &self,
        request: &ResponsesRequest,
        session_id: Option<&str>,
    ) -> Result<EventStream, ProxyError> {
        let body = serde_json::to_string(request).map_err(|error| {
            ProxyError::invalid_request(format!("could not serialize the request: {error}"))
        })?;

        let mut builder = self
            .client
            .post(&self.endpoint)
            .header(axum::http::header::ACCEPT, "text/event-stream")
            .header(axum::http::header::USER_AGENT, USER_AGENT)
            .header(axum::http::header::CONTENT_TYPE, "application/json");

        // §2.8 — the prompt cache scope. This matters most here: every HTTP
        // turn is a full send with no `previous_response_id` chain, so without
        // it nothing is cached at all. Measured on one four-turn conversation,
        // uncached input per turn fell from ~4,480 tokens to ~640. The body's
        // `prompt_cache_key` did nothing on its own.
        if let Some(session_id) = session_id {
            builder = builder.header("session_id", session_id);
        }

        // The header is the whole mechanism. Compressed bytes without it are
        // just bytes the backend cannot parse.
        // §4.4 — zstd on a request body is measured against the subscription
        // backend and nowhere else. An endpoint that does not decompress it
        // parses the bytes as JSON and rejects them, which was observed live
        // as a unicode decode error naming neither compression nor the
        // endpoint. Compression saves bandwidth; it is not worth an error that
        // points at nothing.
        let compressible =
            self.compression && matches!(self.kind, crate::auth::authorize::Kind::Subscription);
        builder = if compressible && super::compression::worth_compressing(&body) {
            builder
                .header(axum::http::header::CONTENT_ENCODING, "zstd")
                .body(super::compression::zstd(&body)?)
        } else {
            builder.body(body)
        };

        // The credential decides what identifies this request, including the
        // originator: it belongs to the subscription dialect and means nothing
        // to an endpoint taking a key. With no credential at all — the replay
        // paths, and nothing else — the subscription dialect is what this
        // transport has always spoken.
        match &self.credentials {
            Some(credentials) => {
                for (name, value) in credentials
                    .authorize(None)
                    .await?
                    .for_endpoint(self.kind)?
                    .headers
                {
                    builder = builder.header(name, value);
                }
            }
            None => builder = builder.header("originator", ORIGINATOR),
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
