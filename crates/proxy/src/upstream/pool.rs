//! `docs/proxy-behavior.md` §4.1 — one connection, reused across a session.
//!
//! Reuse removes per-turn TCP and TLS setup, which is significant in an agent
//! loop issuing many sequential requests. It is also what makes a prewarm worth
//! anything: opening a connection the next request will not use saves nothing.

use super::EventStream;
use super::websocket::ResponseCreate;
use crate::error::ProxyError;
use futures::SinkExt;
use futures::StreamExt;
use proxenos_core::responses::ResponsesRequest;
use yawc::frame::Frame;
use yawc::frame::OpCode;

type Socket = yawc::TcpWebSocket;

/// A connection that outlives the turn that opened it.
pub struct PooledConnection {
    socket: Socket,
    /// The last response id an event on this connection named. A delta may
    /// only continue a response through the connection that produced it
    /// (§4.3); this is what that check reads.
    seen_response_id: Option<String>,
    /// The account whose credential opened this socket (§7.1).
    ///
    /// A connection authenticates once, at the upgrade, and then carries every
    /// turn sent over it. So a socket outlives the turn that opened it *as one
    /// account*: handing it a turn a tier pinned somewhere else would spend the
    /// opener's quota, succeed, and say nothing. This is what the reuse check
    /// reads.
    account: Option<String>,
}

impl PooledConnection {
    pub fn new(socket: Socket, account: Option<&str>) -> Self {
        Self {
            socket,
            seen_response_id: None,
            account: account.map(str::to_owned),
        }
    }

    /// Whether this connection has seen the response a delta would continue.
    pub fn saw(&self, response_id: &str) -> bool {
        self.seen_response_id.as_deref() == Some(response_id)
    }

    /// Whether this connection was opened as the account a turn belongs to.
    pub fn opened_as(&self, account: Option<&str>) -> bool {
        self.account.as_deref() == account
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

        // Always a text frame. A binary frame carries no signal that its
        // contents are compressed, so the backend cannot parse it and refuses
        // the request — and the reference client rejects binary frames
        // outright. Compression here is `permessage-deflate`, applied by the
        // library to this same text frame and marked in the frame header rather
        // than in the payload.
        self.socket
            .send(Frame::text(payload))
            .await
            .map_err(|error| {
                ProxyError::overloaded(format!("could not send over the websocket: {error}"))
            })
    }

    /// The next event payload, or `None` when the turn or the connection ends.
    pub async fn next_event(&mut self) -> Option<Result<String, ProxyError>> {
        loop {
            let frame = match self.socket.next_frame().await {
                Ok(frame) => frame,
                // A closed connection is the end of the stream, not a failure
                // in it. Reporting it as one would turn every ordinary shutdown
                // into an error the caller has to reason about.
                Err(yawc::WebSocketError::ConnectionClosed) => return None,
                Err(error) => {
                    return Some(Err(ProxyError::overloaded(format!(
                        "the websocket failed mid-turn: {error}"
                    ))));
                }
            };

            let (opcode, payload) = <(OpCode, bytes::Bytes)>::from(frame);
            match opcode {
                OpCode::Text => match String::from_utf8(payload.to_vec()) {
                    Ok(text) => return Some(Ok(text)),
                    Err(error) => {
                        return Some(Err(ProxyError::overloaded(format!(
                            "the websocket sent a text frame that is not utf-8: {error}"
                        ))));
                    }
                },
                OpCode::Close => return None,
                // Pings are answered by the library; nothing else carries
                // events.
                _ => continue,
            }
        }
    }
}

/// The response id an event names, if any.
fn response_id(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .pointer("/response/id")?
        .as_str()
        .map(str::to_owned)
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
    if let Ok(payload) = &first
        && let Some(id) = response_id(payload)
    {
        connection.seen_response_id = Some(id);
    }
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
            if let Ok(payload) = &event
                && let Some(id) = response_id(payload)
            {
                connection.seen_response_id = Some(id);
            }

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
