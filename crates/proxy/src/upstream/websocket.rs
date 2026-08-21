//! `docs/proxy-behavior.md` §4.1, §4.3, §4.4 — WebSocket, incremental input,
//! compression.

use super::EventStream;
use crate::error::ProxyError;
use futures::StreamExt;
use proxenos_core::responses::InputItem;
use proxenos_core::responses::ResponsesRequest;
use serde::Serialize;

/// The beta opt-in the WebSocket endpoint requires.
pub const BETA_HEADER: &str = "responses_websockets=2026-02-06";

/// The largest single event this transport will accept, and the buffer behind
/// it.
///
/// Far above the library's 1 MiB default, because one event legitimately
/// carries an entire conversation: the backend echoes the whole request back in
/// `response.created`, `response.in_progress`, and `response.completed`. A cap
/// sized for ordinary messages would sever long conversations, and would do it
/// silently in the middle of a turn.
const MAX_FRAME: usize = 64 * 1024 * 1024;
const MAX_BUFFER: usize = 128 * 1024 * 1024;

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

pub struct WebSocketTransport {
    endpoint: String,
    /// Asked for a token per connection, for the same reason as the HTTP
    /// transport: a captured token goes stale when the session refreshes.
    credentials: Option<std::sync::Arc<dyn crate::auth::authorize::Authorizer>>,
    /// Whether to offer `permessage-deflate` on the upgrade.
    ///
    /// The client offers and the server selects (RFC 7692), so an unoffered
    /// extension is simply never used. A server that declines is a normal
    /// connection, not a degraded one.
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
        credentials: std::sync::Arc<dyn crate::auth::authorize::Authorizer>,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Offer `permessage-deflate` on the upgrade, or decline to.
    ///
    /// Worth roughly two thirds of every frame in both directions, and the
    /// larger half of that is inbound: the backend echoes the whole request
    /// back three times per turn, so a turn's events run about three times the
    /// size of the request that caused them.
    pub fn with_compression(mut self, compression: bool) -> Self {
        self.compression = compression;
        self
    }

    /// The connection options, including whether compression is offered.
    ///
    /// The read limits are raised well above the library default of 1 MiB. A
    /// single event can carry the entire conversation — `response.created`,
    /// `response.in_progress`, and `response.completed` each echo the whole
    /// request — so the default would sever long conversations partway through,
    /// and it would do so at exactly the point where compression matters most.
    fn options(&self) -> yawc::Options {
        let options = yawc::Options::default().with_limits(MAX_FRAME, MAX_BUFFER);
        if self.compression {
            options.with_balanced_compression()
        } else {
            options.without_compression()
        }
    }

    /// Open a connection, without sending anything.
    ///
    /// Separate from `open` because a pooled connection outlives the turn that
    /// created it (§4.1), so opening and sending are no longer the same act.
    pub async fn connect(
        &self,
        session_id: Option<&str>,
    ) -> Result<super::pool::PooledConnection, ProxyError> {
        Ok(super::pool::PooledConnection::new(
            self.dial(session_id).await?,
        ))
    }

    /// Open the socket, with the identity headers and the negotiated options.
    async fn dial(&self, session_id: Option<&str>) -> Result<yawc::TcpWebSocket, ProxyError> {
        let url: url::Url = self.endpoint.parse().map_err(|error| {
            ProxyError::invalid_request(format!("`{}` is not a usable url: {error}", self.endpoint))
        })?;

        // What was asked for, since what was agreed cannot be read back: the
        // library exposes no accessor for the negotiated extensions. This is
        // the offer, not the outcome — a server that declines still connects,
        // and nothing here can tell the two apart.
        tracing::debug!(
            compression = self.compression,
            "opening a websocket; compression is offered, not guaranteed"
        );

        yawc::WebSocket::connect(url)
            .with_options(self.options())
            .with_request(self.request_builder(session_id).await?)
            .await
            .map_err(|error| {
                // A connection that never opened is not a failed turn: the
                // caller falls back to HTTP and the turn proceeds (§4.2).
                ProxyError::overloaded(format!("the websocket did not open: {error}"))
            })
    }

    /// The identity headers §2.8 requires, on the upgrade request.
    ///
    /// The upgrade is an HTTP request like any other and carries the same
    /// identity. `originator` and `user-agent` were absent here for a while
    /// while both the HTTP transport and the catalog fetch sent them — the
    /// socket happened not to enforce them, and the catalog endpoint rejects
    /// their absence with a bare 400 that names nothing. One originator,
    /// always, on every path.
    ///
    /// The token is fetched per connection rather than captured: one taken at
    /// construction goes stale the moment the session refreshes.
    async fn request_builder(
        &self,
        session_id: Option<&str>,
    ) -> Result<yawc::HttpRequestBuilder, ProxyError> {
        let mut builder = yawc::HttpRequestBuilder::new()
            .header("openai-beta", BETA_HEADER)
            .header("originator", super::http::ORIGINATOR)
            .header(
                axum::http::header::USER_AGENT.as_str(),
                super::http::USER_AGENT,
            );

        // §2.8 — the cache scope, on the upgrade: the socket carries many turns
        // of one conversation, so it belongs to the connection rather than to
        // each frame.
        //
        // It buys nothing measurable here — the incremental path already chains
        // turns with `previous_response_id`, and that caches on its own. It is
        // sent for consistency with the HTTP path, where it is the whole
        // difference, and because a session that falls back mid-life must not
        // change cache scope when it does.
        if let Some(session_id) = session_id {
            builder = builder.header("session_id", session_id);
        }

        // The socket belongs to the subscription backend, so a credential of
        // any other kind is refused here rather than upgraded and rejected
        // with something that names neither half.
        if let Some(credentials) = &self.credentials {
            let authorization = credentials
                .authorize(None)
                .await?
                .for_endpoint(crate::auth::authorize::Kind::Subscription)?;
            for (name, value) in authorization.headers {
                // `originator` is already on the builder above: this dialect
                // sends it on every path, credential or not.
                if name.eq_ignore_ascii_case("originator") {
                    continue;
                }
                builder = builder.header(&name, value);
            }
        }

        Ok(builder)
    }

    /// Open a connection and send one request.
    ///
    /// Built on the pooled connection so there is one frame reader rather than
    /// two. The difference between this and the pooled path is only what
    /// happens to the socket afterwards.
    pub async fn open(
        &self,
        request: &ResponsesRequest,
        previous_response_id: Option<String>,
        session_id: Option<&str>,
    ) -> Result<Connection, ProxyError> {
        let mut connection = super::pool::PooledConnection::new(self.dial(session_id).await?);
        connection.send(request, previous_response_id, true).await?;

        // The first event is read here rather than left to the caller.
        //
        // A policy close accepts the handshake and *then* closes, so a
        // connection that opened is not yet a connection that works. Returning
        // it unread would hand back an empty stream, which the translator would
        // faithfully render as a turn where the model said nothing — a silent
        // failure in place of a fallback.
        let Some(first) = connection.next_event().await else {
            return Err(ProxyError::overloaded(
                "the websocket closed before sending anything".to_owned(),
            ));
        };

        let ended = first
            .as_ref()
            .map(|payload| super::pool::ends_turn(payload))
            .unwrap_or(false);
        let failed = first.is_err();

        let rest = if ended || failed {
            // The turn is already over. A connection that stays open is not a
            // turn that continues, and waiting on it hangs the client forever.
            futures::stream::empty().boxed()
        } else {
            // Stops *after* the terminating event, not before it: the
            // terminator is part of the turn, and a translator that never sees
            // it never closes the message.
            //
            // The check happens before the next poll, not after the previous
            // one. A combinator that decides by inspecting the item it just
            // received has to receive one more item to stop — and on a
            // connection that stays open past the turn, that item never comes
            // and the client waits forever.
            futures::stream::unfold(
                (connection, false),
                |(mut connection, finished)| async move {
                    if finished {
                        return None;
                    }
                    let event = connection.next_event().await?;
                    // An error ends the stream after being emitted, matching
                    // what the pooled path does with a failed turn. Continuing
                    // to poll a socket that has already failed either yields
                    // nothing or yields more of the same failure, and the
                    // caller has what it needs to fall back.
                    let ends = event
                        .as_ref()
                        .map(|payload| super::pool::ends_turn(payload))
                        .unwrap_or(true);
                    Some((event, (connection, ends)))
                },
            )
            .boxed()
        };

        // The terminating event is part of the turn, so it is emitted before
        // the stream ends.
        let events = futures::stream::once(async move { first })
            .chain(rest)
            .boxed();

        Ok(Connection { events })
    }
}

/// Compute what to send, given what the session has already sent.
///
/// **Falling back is always safe; a wrong delta is not.** Any ambiguity
/// resolves toward the full send: a full send costs bandwidth, a wrong delta
/// corrupts the conversation and does not fail visibly (§4.3).
pub fn plan_upload<'a>(
    baseline: &proxenos_core::session::Baseline,
    request: &'a ResponsesRequest,
    previous_request: Option<&ResponsesRequest>,
    previous_response_id: Option<&str>,
) -> Upload<'a> {
    // A delta is only meaningful as a continuation of a specific response.
    let Some(response_id) = previous_response_id else {
        tracing::debug!("full: nothing to continue");
        return Upload::Full;
    };

    // Every non-input field must be unchanged. A different tool list or a
    // different model is a different request, and sending only the new items
    // would attach them to the wrong context.
    let Some(previous) = previous_request else {
        tracing::debug!("full: no previous request");
        return Upload::Full;
    };
    if !non_input_fields_match(previous, request) {
        tracing::debug!("full: a non-input field changed");
        return Upload::Full;
    }

    match baseline.plan(&request.input) {
        // An empty delta is not a small delta. The backend receives a previous
        // response id and no new input, and answers from that response — so a
        // client retrying an unchanged conversation would be handed the
        // previous turn again rather than a fresh one. There is nothing to send
        // incrementally, so everything is sent.
        proxenos_core::session::Plan::Delta([]) => Upload::Full,
        proxenos_core::session::Plan::Delta(items) => Upload::Delta {
            items,
            previous_response_id: response_id.to_owned(),
        },
        proxenos_core::session::Plan::Full => {
            tracing::debug!("full: the input did not continue the baseline");
            Upload::Full
        }
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
