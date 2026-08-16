//! `docs/proxy-behavior.md` §4.1, §4.3, §4.4 — WebSocket, incremental input,
//! compression.

use super::EventStream;
use crate::error::ProxyError;
use codex_cc_proxy_core::responses::InputItem;
use codex_cc_proxy_core::responses::ResponsesRequest;
use futures::SinkExt;
use futures::StreamExt;
use serde::Serialize;
use tokio_tungstenite::tungstenite::Message;

/// The beta opt-in the WebSocket endpoint requires.
pub const BETA_HEADER: &str = "responses_websockets=2026-02-06";

/// The outbound frame.
///
/// Unlike the events coming back, which arrive bare, requests carry an envelope
/// naming what they are.
#[derive(Debug, Serialize)]
pub struct ResponseCreate<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(flatten)]
    pub request: &'a ResponsesRequest,
    /// Set only by the incremental path. Its presence is what makes `input` a
    /// delta rather than the whole conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// `false` opens the connection without producing a turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate: Option<bool>,
}

impl<'a> ResponseCreate<'a> {
    pub fn new(request: &'a ResponsesRequest) -> Self {
        Self {
            kind: "response.create",
            request,
            previous_response_id: None,
            generate: None,
        }
    }

    /// A delta continuing a previous response.
    pub fn incremental(mut self, previous_response_id: String) -> Self {
        self.previous_response_id = Some(previous_response_id);
        self
    }

    /// A prewarm: open the connection, produce nothing.
    pub fn prewarm(mut self) -> Self {
        self.generate = Some(false);
        self
    }
}

/// §4.4 — payloads may be zstd-compressed.
///
/// This compounds with the incremental path: that removes most turns' bulk, and
/// compression reduces what remains on the turns where a full send is
/// unavoidable.
pub fn compress(payload: &str) -> Result<Vec<u8>, ProxyError> {
    zstd::encode_all(payload.as_bytes(), 3)
        .map_err(|error| ProxyError::overloaded(format!("could not compress the request: {error}")))
}

/// Whether compressing this payload is worth doing.
///
/// Small payloads compress to more than they started as, once the frame header
/// is counted. Sending those uncompressed is not an optimization that failed;
/// it is the correct outcome.
pub fn worth_compressing(payload: &str) -> bool {
    payload.len() > 1024
}

pub struct WebSocketTransport {
    endpoint: String,
    /// Asked for a token per connection, for the same reason as the HTTP
    /// transport: a captured token goes stale when the session refreshes.
    credentials: Option<std::sync::Arc<crate::auth::tokens::TokenSource>>,
    compression: bool,
}

/// One opened connection, carrying the events of a single turn.
pub struct Connection {
    events: EventStream,
}

impl Connection {
    pub fn into_events(self) -> EventStream {
        self.events
    }
}

impl WebSocketTransport {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            credentials: None,
            compression: true,
        }
    }

    pub fn with_credentials(
        mut self,
        credentials: std::sync::Arc<crate::auth::tokens::TokenSource>,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }

    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compression = compression;
        self
    }

    /// Open a connection, without sending anything.
    ///
    /// Separate from `open` because a pooled connection outlives the turn that
    /// created it (§4.1), so opening and sending are no longer the same act.
    pub async fn connect(&self) -> Result<super::pool::PooledConnection, ProxyError> {
        let handshake = self.handshake().await?;
        let (stream, _) = tokio_tungstenite::connect_async(handshake)
            .await
            .map_err(|error| {
                ProxyError::overloaded(format!("the websocket did not open: {error}"))
            })?;
        Ok(super::pool::PooledConnection::new(stream, self.compression))
    }

    async fn handshake(
        &self,
    ) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, ProxyError> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let mut handshake = self
            .endpoint
            .as_str()
            .into_client_request()
            .map_err(|error| {
                ProxyError::invalid_request(format!("could not build the handshake: {error}"))
            })?;

        let headers = handshake.headers_mut();
        headers.insert(
            "openai-beta",
            BETA_HEADER.parse().map_err(|_| {
                ProxyError::invalid_request("the beta header is not a valid header value")
            })?,
        );
        if let Some(credentials) = &self.credentials {
            let token = credentials.access_token().await?;
            if let Ok(value) = format!("Bearer {token}").parse() {
                headers.insert(axum::http::header::AUTHORIZATION, value);
            }
            if let Some(account) = credentials.account_id()
                && let Ok(value) = account.parse()
            {
                headers.insert("chatgpt-account-id", value);
            }
        }

        Ok(handshake)
    }

    /// Open a connection and send one request.
    pub async fn open(
        &self,
        request: &ResponsesRequest,
        previous_response_id: Option<String>,
    ) -> Result<Connection, ProxyError> {
        let handshake = self.handshake().await?;

        let (stream, _) = tokio_tungstenite::connect_async(handshake)
            .await
            .map_err(|error| {
                // A connection that never opened is not a failed turn: the
                // caller falls back to HTTP and the turn proceeds (§4.2).
                ProxyError::overloaded(format!("the websocket did not open: {error}"))
            })?;

        let (mut writer, reader) = stream.split();

        let mut frame = ResponseCreate::new(request);
        if let Some(previous) = previous_response_id {
            frame = frame.incremental(previous);
        }
        let payload = serde_json::to_string(&frame).map_err(|error| {
            ProxyError::invalid_request(format!("could not serialize the request: {error}"))
        })?;

        let message = if self.compression && worth_compressing(&payload) {
            Message::Binary(compress(&payload)?.into())
        } else {
            Message::Text(payload.into())
        };

        writer.send(message).await.map_err(|error| {
            ProxyError::overloaded(format!("could not send over the websocket: {error}"))
        })?;

        let mut events = reader
            .filter_map(|message| async move {
                match message {
                    Ok(Message::Text(text)) => Some(Ok(text.to_string())),
                    // A close is the end of the stream, not an error in it. The
                    // translator closes the message off (§5.1).
                    Ok(Message::Close(_)) => None,
                    Ok(_) => None,
                    Err(error) => Some(Err(ProxyError::overloaded(format!(
                        "the websocket failed mid-turn: {error}"
                    )))),
                }
            })
            .boxed();

        // The first event is read here rather than left to the caller.
        //
        // A policy close accepts the handshake and *then* closes, so a
        // connection that opened is not yet a connection that works. Returning
        // it unread would hand back an empty stream, which the translator would
        // faithfully render as a turn where the model said nothing — a silent
        // failure in place of a fallback.
        let first = events.next().await;

        let events = match first {
            Some(first) => {
                let ended = first
                    .as_ref()
                    .map(|payload| super::pool::ends_turn(payload))
                    .unwrap_or(false);
                let rest = if ended {
                    // The turn is already over. A connection that stays open is
                    // not a turn that continues, and waiting on it hangs the
                    // client forever.
                    futures::stream::empty().boxed()
                } else {
                    // Stops *after* the terminating event, not before it: the
                    // terminator is part of the turn, and a translator that
                    // never sees it never closes the message.
                    //
                    // The check happens before the next poll, not after the
                    // previous one. A combinator that decides by inspecting the
                    // item it just received has to receive one more item to
                    // stop — and on a connection that stays open past the turn,
                    // that item never comes and the client waits forever.
                    futures::stream::unfold((events, false), |(mut events, finished)| async move {
                        if finished {
                            return None;
                        }
                        let event = events.next().await?;
                        let ends = event
                            .as_ref()
                            .map(|payload| super::pool::ends_turn(payload))
                            .unwrap_or(false);
                        Some((event, (events, ends)))
                    })
                    .boxed()
                };

                // The terminating event is part of the turn, so it is emitted
                // before the stream ends.
                futures::stream::once(async move { first })
                    .chain(rest)
                    .boxed()
            }
            None => {
                return Err(ProxyError::overloaded(
                    "the websocket closed before sending anything".to_owned(),
                ));
            }
        };

        Ok(Connection { events })
    }
}

/// Compute what to send, given what the session has already sent.
///
/// **Falling back is always safe; a wrong delta is not.** Any ambiguity
/// resolves toward the full send: a full send costs bandwidth, a wrong delta
/// corrupts the conversation and does not fail visibly (§4.3).
pub fn plan_upload<'a>(
    baseline: &codex_cc_proxy_core::session::Baseline,
    request: &'a ResponsesRequest,
    previous_request: Option<&ResponsesRequest>,
    previous_response_id: Option<&str>,
) -> Upload<'a> {
    // A delta is only meaningful as a continuation of a specific response.
    let Some(response_id) = previous_response_id else {
        return Upload::Full;
    };

    // Every non-input field must be unchanged. A different tool list or a
    // different model is a different request, and sending only the new items
    // would attach them to the wrong context.
    let Some(previous) = previous_request else {
        return Upload::Full;
    };
    if !non_input_fields_match(previous, request) {
        return Upload::Full;
    }

    match baseline.plan(&request.input) {
        // An empty delta is not a small delta. The backend receives a previous
        // response id and no new input, and answers from that response — so a
        // client retrying an unchanged conversation would be handed the
        // previous turn again rather than a fresh one. There is nothing to send
        // incrementally, so everything is sent.
        codex_cc_proxy_core::session::Plan::Delta([]) => Upload::Full,
        codex_cc_proxy_core::session::Plan::Delta(items) => Upload::Delta {
            items,
            previous_response_id: response_id.to_owned(),
        },
        codex_cc_proxy_core::session::Plan::Full => Upload::Full,
    }
}

#[derive(Debug, PartialEq)]
pub enum Upload<'a> {
    Full,
    Delta {
        items: &'a [InputItem],
        previous_response_id: String,
    },
}

/// Everything except `input`.
///
/// Compared by serializing the request with its input emptied, so a field added
/// later is included automatically. A hand-written comparison is a list that
/// silently stops being exhaustive the moment the struct grows — and the
/// failure it produces is a wrong delta, which does not announce itself.
fn non_input_fields_match(left: &ResponsesRequest, right: &ResponsesRequest) -> bool {
    let strip = |request: &ResponsesRequest| {
        let mut copy = request.clone();
        copy.input = Vec::new();
        serde_json::to_value(copy).unwrap_or(serde_json::Value::Null)
    };
    strip(left) == strip(right)
}
