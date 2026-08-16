//! `docs/proxy-behavior.md` §4.2 — choosing a transport, and staying with it.

use super::Transport;
use super::http::HttpTransport;
use super::websocket::Upload;
use super::websocket::WebSocketTransport;
use super::websocket::plan_upload;
use crate::error::ProxyError;
use codex_cc_proxy_core::responses::ResponsesRequest;
use codex_cc_proxy_core::session::Baseline;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// A WebSocket attempt that did not produce a turn, and whether it happened on
/// a connection carried over from an earlier one.
struct WebSocketFailure {
    error: ProxyError,
    reused: bool,
}

/// What one turn was actually sent as, for the caller to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    Full,
    Delta,
}

/// The transport binding for one session.
///
/// WebSocket is primary and HTTP is its fallback, but neither is a degraded
/// version of the other: the backend closes WebSocket connections under policy
/// conditions often enough that HTTP is a normal operating mode.
pub struct Conduit {
    websocket: Option<Arc<WebSocketTransport>>,
    http: Arc<HttpTransport>,
    /// §4.1 — one connection, cached and reused. Opened lazily, and handed back
    /// after each turn that ended cleanly.
    connection: Arc<tokio::sync::Mutex<Option<crate::upstream::pool::PooledConnection>>>,
    /// Once a session falls back it stays fallen back for the rest of its life.
    ///
    /// Retrying the WebSocket every turn spends a failed handshake per turn to
    /// re-learn what the first one established, and does it on the latency path
    /// of every request.
    latched_to_http: AtomicBool,
}

impl Conduit {
    pub fn new(http: Arc<HttpTransport>, websocket: Option<Arc<WebSocketTransport>>) -> Self {
        Self {
            websocket,
            http,
            connection: Arc::new(tokio::sync::Mutex::new(None)),
            latched_to_http: AtomicBool::new(false),
        }
    }

    pub fn is_latched_to_http(&self) -> bool {
        self.latched_to_http.load(Ordering::SeqCst)
    }

    /// Send one turn.
    ///
    /// Returns the event stream and what the request was sent as, so the caller
    /// can advance its baseline correctly — a delta advances it by the new
    /// items, a full send replaces it.
    pub async fn send(
        &self,
        request: &ResponsesRequest,
        baseline: &Baseline,
        previous_request: Option<&ResponsesRequest>,
        previous_response_id: Option<&str>,
    ) -> Result<(super::EventStream, Sent), ProxyError> {
        if let Some(websocket) = &self.websocket
            && !self.is_latched_to_http()
        {
            let upload = plan_upload(baseline, request, previous_request, previous_response_id);

            let (payload, previous, sent) = match &upload {
                Upload::Delta {
                    items,
                    previous_response_id,
                } => {
                    let mut delta = request.clone();
                    delta.input = items.to_vec();
                    (delta, Some(previous_response_id.clone()), Sent::Delta)
                }
                Upload::Full => (request.clone(), None, Sent::Full),
            };

            tracing::debug!(
                upload = ?sent,
                items = payload.input.len(),
                previous = ?previous,
                "uploading"
            );

            let attempt = self
                .send_over_websocket(websocket, &payload, previous)
                .await;

            let attempt = match attempt {
                Ok(events) => return Ok((events, sent)),
                // A pooled socket the backend closed while it was idle is not
                // evidence that this session cannot use the WebSocket. It is
                // retried once on a fresh connection — as a *full* send, because
                // `previous_response_id` names a response the closed socket held
                // and the new one knows nothing about. Replaying the delta
                // against it would attach the turn to a conversation that does
                // not exist there, which is exactly the silent corruption a
                // full send exists to avoid.
                Err(failure) if failure.reused => {
                    tracing::debug!(error = %failure.error, "the pooled connection had expired");
                    match self.send_over_websocket(websocket, request, None).await {
                        Ok(events) => return Ok((events, Sent::Full)),
                        Err(failure) => failure,
                    }
                }
                Err(failure) => failure,
            };

            // A policy close or a refused handshake is not a failed turn. The
            // turn proceeds over HTTP, and this session does not try the
            // WebSocket again.
            tracing::info!(error = %attempt.error, "falling back to HTTP for this session");
            self.latched_to_http.store(true, Ordering::SeqCst);
        }

        // HTTP is stateless: it always carries the whole conversation.
        let events = self.http.stream(request).await?;
        Ok((events, Sent::Full))
    }

    /// Send over the pooled connection, opening one if there is none.
    ///
    /// The failure says whether it happened on a connection this session had
    /// been holding, because that distinguishes "the WebSocket does not work
    /// here" from "the socket went stale between turns" — and only the first is
    /// a reason to latch (§4.2).
    async fn send_over_websocket(
        &self,
        websocket: &WebSocketTransport,
        request: &ResponsesRequest,
        previous_response_id: Option<String>,
    ) -> Result<super::EventStream, WebSocketFailure> {
        let pooled = self.connection.lock().await.take();
        let reused = pooled.is_some();
        let failed = |error: ProxyError| WebSocketFailure { error, reused };

        let mut connection = match pooled {
            Some(connection) => connection,
            None => websocket.connect().await.map_err(failed)?,
        };

        connection
            .send(request, previous_response_id, true)
            .await
            .map_err(failed)?;

        // The first event is read before the connection is handed over.
        //
        // A policy close accepts the handshake and *then* closes, so a socket
        // that sent successfully is not yet a socket that works. Handing back
        // an empty stream would render as a turn where the model said nothing
        // — a silent failure standing exactly where the fallback belongs.
        let first = match connection.next_event().await {
            Some(Err(error)) if reused => return Err(failed(error)),
            Some(first) => first,
            None => {
                return Err(failed(ProxyError::overloaded(
                    "the websocket closed before sending anything".to_owned(),
                )));
            }
        };

        Ok(super::pool::pump(
            connection,
            first,
            Arc::clone(&self.connection),
        ))
    }

    /// §4.1 — open the connection before the turn that will use it.
    ///
    /// A prewarm produces no response. Its only purpose is that the request
    /// which follows reuses both the connection and the prior response id, so
    /// the first turn of a session does not pay for the handshake.
    pub async fn prewarm(&self, request: &ResponsesRequest) {
        let Some(websocket) = &self.websocket else {
            return;
        };
        if self.is_latched_to_http() {
            return;
        }
        if self.connection.lock().await.is_some() {
            return;
        }

        match websocket.connect().await {
            Ok(mut connection) => {
                if connection.send(request, None, false).await.is_ok() {
                    // Re-checked under the lock: a turn may have started
                    // between the check above and here, and overwriting its
                    // connection would drop an open socket mid-conversation.
                    let mut slot = self.connection.lock().await;
                    if slot.is_none() {
                        *slot = Some(connection);
                    }
                }
            }
            Err(error) => {
                // A prewarm that fails costs nothing and says nothing: the real
                // request will try again and latch if it must.
                tracing::debug!(%error, "prewarm did not open a connection");
            }
        }
    }

    /// Whether a connection is currently pooled.
    pub async fn has_pooled_connection(&self) -> bool {
        self.connection.lock().await.is_some()
    }
}
