//! `docs/proxy-behavior.md` §4.1 — one connection, reused across a session.
//!
//! Reuse removes per-turn TCP and TLS setup, which is significant in an agent
//! loop issuing many sequential requests. It is also what makes a prewarm worth
//! anything: opening a connection the next request will not use saves nothing.

use super::EventStream;
use super::websocket::ResponseCreate;
use crate::error::ProxyError;
use codex_cc_proxy_core::responses::ResponsesRequest;
use futures::SinkExt;
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A connection that outlives the turn that opened it.
pub struct PooledConnection {
    socket: Socket,
}

impl PooledConnection {
    pub fn new(socket: Socket) -> Self {
        Self { socket }
    }

    /// Send one frame.
    pub async fn send(
        &mut self,
        request: &ResponsesRequest,
        previous_response_id: Option<String>,
        generate: bool,
    ) -> Result<(), ProxyError> {
        let mut frame = ResponseCreate::new(request);
        if let Some(previous) = previous_response_id {
            frame = frame.incremental(previous);
        }
        if !generate {
            frame = frame.prewarm();
        }

        let payload = serde_json::to_string(&frame).map_err(|error| {
            ProxyError::invalid_request(format!("could not serialize the request: {error}"))
        })?;

        // Always text. A binary frame carries no signal that its contents are
        // compressed, so the backend cannot parse it and refuses the request —
        // and the reference client rejects binary frames outright. WebSocket
        // compression is `permessage-deflate`, negotiated in the upgrade.
        self.socket
            .send(Message::Text(payload.into()))
            .await
            .map_err(|error| {
                ProxyError::overloaded(format!("could not send over the websocket: {error}"))
            })
    }

    /// The next event payload, or `None` when the turn or the connection ends.
    pub async fn next_event(&mut self) -> Option<Result<String, ProxyError>> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => return Some(Ok(text.to_string())),
                // Pings are answered by the library; anything else that is not
                // text carries no events.
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    return Some(Err(ProxyError::overloaded(format!(
                        "the websocket failed mid-turn: {error}"
                    ))));
                }
            }
        }
    }
}

/// Whether this event ends the turn.
///
/// The connection stays open past it, which is the whole point — but the
/// caller's stream must end, or the client waits forever for a turn that has
/// already finished.
pub fn ends_turn(payload: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|event| {
            event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "response.completed" | "response.failed" | "response.incomplete" | "error"
            )
        })
}

/// Pump one turn's events out of a connection, then hand the connection back.
///
/// The connection is returned to the pool only when the turn ended cleanly. One
/// that failed mid-turn is dropped: reusing a socket whose state is unknown
/// risks attaching the next turn to a conversation the backend has already
/// abandoned, and that failure is silent.
pub fn pump(
    mut connection: PooledConnection,
    first: Result<String, ProxyError>,
    slot: std::sync::Arc<tokio::sync::Mutex<Option<PooledConnection>>>,
) -> EventStream {
    let (sender, receiver) = futures::channel::mpsc::unbounded();

    let first_ends_turn = first
        .as_ref()
        .map(|payload| ends_turn(payload))
        .unwrap_or(false);
    let first_failed = first.is_err();
    let _ = sender.unbounded_send(first);

    tokio::spawn(async move {
        let mut healthy = false;

        if first_failed {
            return;
        }
        if first_ends_turn {
            park(slot, connection).await;
            return;
        }

        while let Some(event) = connection.next_event().await {
            let finished = match &event {
                Ok(payload) => ends_turn(payload),
                Err(_) => false,
            };
            let failed = event.is_err();

            if sender.unbounded_send(event).is_err() {
                // The client went away. Dropping the connection propagates the
                // cancellation upstream rather than generating into nothing
                // (§5.3).
                return;
            }

            if failed {
                return;
            }
            if finished {
                healthy = true;
                break;
            }
        }

        if healthy {
            park(slot, connection).await;
        }
    });

    receiver.boxed()
}

/// Hand a connection back, unless the session already holds one.
///
/// Two turns can overlap — a prewarm and the turn that overtook it, or two
/// concurrent requests — and each opens its own socket when the slot is empty.
/// Overwriting is how the loser's socket gets dropped while it is still open,
/// so the loser closes instead.
async fn park(
    slot: std::sync::Arc<tokio::sync::Mutex<Option<PooledConnection>>>,
    connection: PooledConnection,
) {
    let mut slot = slot.lock().await;
    if slot.is_none() {
        *slot = Some(connection);
    }
}
