//! `docs/proxy-behavior.md` §4 — transports.
//!
//! A transport takes a translated request and yields the upstream event
//! payloads it produces. WebSocket and HTTP are interchangeable here: neither
//! is a degraded version of the other, because the backend closes WebSocket
//! connections under policy conditions often enough that HTTP is a normal
//! operating mode.

pub mod conduit;
pub mod http;
pub mod websocket;

use crate::error::ProxyError;
use codex_cc_proxy_core::responses::ResponsesRequest;
use futures::stream::BoxStream;

/// One upstream exchange: the event payloads, in order, exactly as §5.0 leaves
/// them.
pub type EventStream = BoxStream<'static, Result<String, ProxyError>>;

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Open a stream for one request.
    ///
    /// Failures that happen before any event is emitted surface here; failures
    /// after that arrive as an error item in the stream, because the client has
    /// already received a status and a mid-stream failure cannot change it.
    async fn stream(&self, request: &ResponsesRequest) -> Result<EventStream, ProxyError>;
}
