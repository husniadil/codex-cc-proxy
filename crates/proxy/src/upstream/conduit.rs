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

            match self
                .send_over_websocket(websocket, &payload, previous)
                .await
            {
                Ok(events) => return Ok((events, sent)),
                Err(error) => {
                    // A policy close or a refused handshake is not a failed
                    // turn. The turn proceeds over HTTP, and this session does
                    // not try the WebSocket again.
                    tracing::info!(%error, "falling back to HTTP for this session");
                    self.latched_to_http.store(true, Ordering::SeqCst);
                }
            }
        }

        // HTTP is stateless: it always carries the whole conversation.
        let events = self.http.stream(request).await?;
        Ok((events, Sent::Full))
    }

    /// Send over the pooled connection, opening one if there is none.
    async fn send_over_websocket(
        &self,
        websocket: &WebSocketTransport,
        request: &ResponsesRequest,
        previous_response_id: Option<String>,
    ) -> Result<super::EventStream, ProxyError> {
        let pooled = self.connection.lock().await.take();

        let mut connection = match pooled {
            Some(connection) => connection,
            None => websocket.connect().await?,
        };

        // A send that fails on a reused connection is retried once on a fresh
        // one. The backend closes idle connections, and a turn must not be lost
        // to a socket that expired between requests.
        if connection
            .send(request, previous_response_id.clone(), true)
            .await
            .is_err()
        {
            connection = websocket.connect().await?;
            connection.send(request, previous_response_id, true).await?;
        }

        // The first event is read before the connection is handed over.
        //
        // A policy close accepts the handshake and *then* closes, so a socket
        // that sent successfully is not yet a socket that works. Handing back
        // an empty stream would render as a turn where the model said nothing
        // — a silent failure standing exactly where the fallback belongs.
        let Some(first) = connection.next_event().await else {
            return Err(ProxyError::overloaded(
                "the websocket closed before sending anything".to_owned(),
            ));
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
                    *self.connection.lock().await = Some(connection);
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
