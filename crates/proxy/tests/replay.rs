//! A local server that replays recorded upstream exchanges.
//!
//! It speaks real HTTP and real SSE over loopback, so the transport under test
//! is the one that ships rather than a stub standing in for it. No test in this
//! suite reaches the network.

#![allow(dead_code)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use futures::SinkExt;
use futures::StreamExt;
use futures::stream;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

/// What the replay server should do with the next request.
#[derive(Clone)]
pub enum Behavior {
    /// Emit these event payloads as SSE, then close.
    Events(Vec<Value>),
    /// Emit raw bytes verbatim, for framing cases a well-formed stream cannot
    /// express — an event split across several `data:` lines, for instance.
    Raw(String),
    /// Emit one event, then stall. The server records whether it ever got to
    /// send the rest, which is how cancellation is observed.
    Stall { sent_everything: Arc<Mutex<bool>> },
    /// Answer with a status and body instead of a stream.
    Failure {
        status: u16,
        body: String,
        retry_after: Option<String>,
    },
}

#[derive(Clone)]
pub struct ReplayServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
}

impl ReplayServer {
    /// Start a server on an ephemeral loopback port.
    pub async fn start(behavior: Behavior) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState {
            behavior: Arc::new(behavior),
            requests: Arc::clone(&requests),
            headers: Arc::clone(&headers),
        };

        let app = Router::new()
            .route("/responses", post(handle))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding loopback should succeed");
        let addr: SocketAddr = listener.local_addr().expect("a bound port");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{addr}/responses"),
            requests,
            headers,
        }
    }

    /// Every request's headers, lowercased, in order.
    pub fn headers(&self) -> Vec<std::collections::BTreeMap<String, String>> {
        self.headers
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }

    /// Every request body the server received, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct ServerState {
    behavior: Arc<Behavior>,
    requests: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
}

async fn handle(
    State(state): State<ServerState>,
    request_headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    if let Ok(mut headers) = state.headers.lock() {
        headers.push(
            request_headers
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
                })
                .collect(),
        );
    }

    if let Ok(parsed) = serde_json::from_str::<Value>(&body)
        && let Ok(mut requests) = state.requests.lock()
    {
        requests.push(parsed);
    }

    match state.behavior.as_ref() {
        Behavior::Events(events) => {
            let mut out = String::new();
            for event in events {
                let kind = event["type"].as_str().unwrap_or("message");
                out.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
            }
            sse(out)
        }
        Behavior::Raw(raw) => sse(raw.clone()),
        Behavior::Stall { sent_everything } => {
            let sent_everything = Arc::clone(sent_everything);
            let stream = stream::once(async move {
                Ok::<_, std::io::Error>(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n"
                        .to_owned(),
                )
            })
            .chain(stream::once(async move {
                // Short enough that a request which was NOT cancelled reaches
                // this point well within the assertion's wait. That is what
                // makes the cancellation test able to fail.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Ok(mut flag) = sent_everything.lock() {
                    *flag = true;
                }
                Ok(
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n"
                        .to_owned(),
                )
            }));

            let mut response = Response::new(Body::from_stream(stream));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
            response
        }
        Behavior::Failure {
            status,
            body,
            retry_after,
        } => {
            let status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body.clone()).into_response();
            if let Some(retry_after) = retry_after
                && let Ok(value) = retry_after.parse()
            {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

fn sse(payload: String) -> Response {
    let mut response = Response::new(Body::from(payload));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response
}

// ---------------------------------------------------------------------------
// A WebSocket replay server, so the delta path can be observed end to end.
//
// The HTTP transport is stateless and always sends the whole conversation, so a
// test that only speaks HTTP cannot see an upload plan at all — which is how an
// always-empty delta survived a full suite.
// ---------------------------------------------------------------------------

pub struct WsReplay {
    pub url: String,
    frames: Arc<Mutex<Vec<Value>>>,
}

impl WsReplay {
    pub async fn start(events: Vec<Value>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&frames);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let events = events.clone();
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = socket.next().await {
                        let body = match message {
                            tokio_tungstenite::tungstenite::Message::Text(text) => text.to_string(),
                            tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                                zstd::decode_all(bytes.as_ref())
                                    .ok()
                                    .and_then(|raw| String::from_utf8(raw).ok())
                                    .unwrap_or_default()
                            }
                            tokio_tungstenite::tungstenite::Message::Close(_) => break,
                            _ => continue,
                        };

                        if let Ok(frame) = serde_json::from_str::<Value>(&body)
                            && let Ok(mut frames) = sink.lock()
                        {
                            frames.push(frame);
                        }

                        for event in &events {
                            let _ = socket
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    event.to_string().into(),
                                ))
                                .await;
                        }
                    }
                });
            }
        });

        Self {
            url: format!("ws://{addr}"),
            frames,
        }
    }

    /// Every `response.create` frame the server received, in order.
    pub fn frames(&self) -> Vec<Value> {
        self.frames
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }

    /// Wait until at least `count` frames have arrived.
    pub async fn wait_for(&self, count: usize) -> Vec<Value> {
        for _ in 0..300 {
            let frames = self.frames();
            if frames.len() >= count {
                return frames;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "only {} frames arrived, wanted {count}",
            self.frames().len()
        );
    }
}
