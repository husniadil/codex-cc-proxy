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

            match websocket.open(&payload, previous).await {
                Ok(connection) => return Ok((connection.into_events(), sent)),
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
}
