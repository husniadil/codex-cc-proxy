//! `docs/proxy-behavior.md` §4 — transports.
//!
//! A transport takes a translated request and yields the upstream event
//! payloads it produces. WebSocket and HTTP are interchangeable here: neither
//! is a degraded version of the other, because the backend closes WebSocket
//! connections under policy conditions often enough that HTTP is a normal
//! operating mode.

pub mod compression;
pub mod conduit;
pub mod http;
pub mod pool;
pub mod websocket;

use crate::error::ProxyError;
use futures::stream::BoxStream;
use proxenos_core::responses::ResponsesRequest;

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
    ///
    /// `session_id` scopes the prompt cache and must be stable for the life of
    /// a conversation. It is carried per request rather than per transport
    /// because one transport serves every session (§2.7).
    ///
    /// `account` is the account this request is made as, which a pinned tier
    /// names (§7.1). It travels with the request for the same reason: one
    /// transport serves every tier, and two tiers on one conversation can
    /// belong to two different accounts.
    async fn stream(
        &self,
        request: &ResponsesRequest,
        session_id: Option<&str>,
        account: Option<&str>,
    ) -> Result<EventStream, ProxyError>;
}
