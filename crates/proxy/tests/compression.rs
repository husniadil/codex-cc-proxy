//! `docs/proxy-behavior.md` §4.4 — compression on the socket.
//!
//! The backend negotiates `permessage-deflate` on the upgrade, and roughly two
//! thirds of every frame in both directions is compressible. The downstream
//! half is the larger one: the backend echoes the whole request back in
//! `response.created`, `response.in_progress`, and `response.completed`, so a
//! turn's events run about three times the size of the request that caused
//! them, and nothing else in this proxy has any lever on that.
//!
//! Every test here runs against a local server. Nothing contacts the network.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use futures::StreamExt;
use proxenos::upstream::websocket::WebSocketTransport;
use proxenos_core::responses::ResponsesRequest;
use std::sync::Arc;
use std::sync::Mutex;

/// A WebSocket server that negotiates `permessage-deflate`, records what it was
/// asked, and replies with what it was told to.
///
/// It reports the extensions each client offered, which is how a test can tell
/// "connected" from "connected with compression" — those are indistinguishable
/// from the outside, and only one of them is the point.
struct DeflateServer {
    url: String,
    offered: Arc<Mutex<Vec<String>>>,
    received: Arc<Mutex<Vec<String>>>,
    /// Every upgrade's headers, lowercased. The upgrade is a request like any
    /// other and carries the same identity (§2.8); nothing else checks that.
    headers: Arc<Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
}

impl DeflateServer {
    async fn start(events: Vec<String>) -> Self {
        use yawc::WebSocket;

        let offered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let headers: Arc<Mutex<Vec<std::collections::BTreeMap<String, String>>>> =
            Arc::new(Mutex::new(Vec::new()));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let offered_for_server = Arc::clone(&offered);
        let received_for_server = Arc::clone(&received);
        let headers_for_server = Arc::clone(&headers);

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let events = events.clone();
                let offered = Arc::clone(&offered_for_server);
                let received = Arc::clone(&received_for_server);
                let headers = Arc::clone(&headers_for_server);

                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |mut request| {
                        let events = events.clone();
                        let offered = Arc::clone(&offered);
                        let received = Arc::clone(&received);
                        let headers = Arc::clone(&headers);
                        async move {
                            if let Ok(mut headers) = headers.lock() {
                                headers.push(
                                    request
                                        .headers()
                                        .iter()
                                        .filter_map(|(name, value)| {
                                            Some((
                                                name.as_str().to_ascii_lowercase(),
                                                value.to_str().ok()?.to_owned(),
                                            ))
                                        })
                                        .collect(),
                                );
                            }

                            if let Some(extensions) = request
                                .headers()
                                .get("sec-websocket-extensions")
                                .and_then(|value| value.to_str().ok())
                                && let Ok(mut offered) = offered.lock()
                            {
                                offered.push(extensions.to_owned());
                            }

                            // Compression is off in `Options::default()`, so a
                            // server that took the default would never select
                            // the extension and the test would pass for the
                            // wrong reason.
                            let (response, upgrade) = WebSocket::upgrade_with_options(
                                &mut request,
                                yawc::Options::default().with_balanced_compression(),
                            )?;

                            tokio::spawn(async move {
                                let Ok(mut socket) = upgrade.await else {
                                    return;
                                };
                                while let Some(frame) = socket.next().await {
                                    let (opcode, payload): (yawc::frame::OpCode, bytes::Bytes) =
                                        frame.into();
                                    if opcode != yawc::frame::OpCode::Text {
                                        continue;
                                    }
                                    let Ok(text) = String::from_utf8(payload.to_vec()) else {
                                        continue;
                                    };
                                    if let Ok(mut received) = received.lock() {
                                        received.push(text);
                                    }
                                    for event in &events {
                                        use futures::SinkExt;
                                        let _ = socket
                                            .send(yawc::frame::Frame::text(event.clone()))
                                            .await;
                                    }
                                }
                            });

                            Ok::<_, yawc::WebSocketError>(response)
                        }
                    });

                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .with_upgrades()
                        .await;
                });
            }
        });

        Self {
            url: format!("ws://{addr}"),
            offered,
            received,
            headers,
        }
    }

    fn headers(&self) -> Vec<std::collections::BTreeMap<String, String>> {
        self.headers
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }

    fn offered(&self) -> Vec<String> {
        self.offered
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }

    fn received(&self) -> Vec<String> {
        self.received
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }
}

fn request(marker: &str) -> ResponsesRequest {
    ResponsesRequest {
        model: "gpt-5.6-luna".to_owned(),
        instructions: Some(marker.to_owned()),
        ..ResponsesRequest::default()
    }
}

/// The client offers `permessage-deflate` on the upgrade.
///
/// Without the offer nothing is negotiated and every frame travels whole. The
/// server cannot ask for it — under RFC 7692 the client offers and the server
/// selects — so an unasked-for extension is simply never used.
#[tokio::test]
async fn the_upgrade_offers_permessage_deflate() {
    let server = DeflateServer::start(vec![
        r#"{"type":"response.completed","response":{"id":"r1"}}"#.to_owned(),
    ])
    .await;

    let transport = WebSocketTransport::new(&server.url).with_compression(true);
    let connection = transport
        .open(&request("hello"), None, None)
        .await
        .expect("the connection should open");
    let _ = connection.into_events().collect::<Vec<_>>().await;

    let offered = server.offered();
    assert!(
        offered
            .iter()
            .any(|value| value.contains("permessage-deflate")),
        "the upgrade should offer compression: {offered:?}"
    );
}

/// A compressed connection still carries the payload verbatim.
///
/// The marker is a code the server could not produce on its own, so a test that
/// passes is a test where the bytes genuinely made the round trip. A frame that
/// arrived empty, truncated, or inflated with the wrong window would fail here
/// rather than read as success.
#[tokio::test]
async fn a_compressed_request_arrives_intact() {
    const MARKER: &str = "zq7f3-VERBATIM-8bk2x";

    let server = DeflateServer::start(vec![
        r#"{"type":"response.completed","response":{"id":"r1"}}"#.to_owned(),
    ])
    .await;

    let transport = WebSocketTransport::new(&server.url).with_compression(true);
    let connection = transport
        .open(&request(MARKER), None, None)
        .await
        .expect("the connection should open");
    let _ = connection.into_events().collect::<Vec<_>>().await;

    let received = server.received();
    assert_eq!(received.len(), 1, "{received:?}");
    assert!(
        received[0].contains(MARKER),
        "the payload should survive compression verbatim: {}",
        received[0]
    );
}

/// Events come back through a compressed connection unchanged.
///
/// Downstream is where two thirds of the traffic is, so a client that
/// compresses only what it sends has taken the smaller half of the win. The
/// code here is one the client could not have guessed.
#[tokio::test]
async fn compressed_events_arrive_intact() {
    const CODE: &str = "r4m8p-EVENT-w9tz2";

    let server = DeflateServer::start(vec![
        format!(r#"{{"type":"response.output_text.delta","delta":"{CODE}"}}"#),
        r#"{"type":"response.completed","response":{"id":"r1"}}"#.to_owned(),
    ])
    .await;

    let transport = WebSocketTransport::new(&server.url).with_compression(true);
    let connection = transport
        .open(&request("hello"), None, None)
        .await
        .expect("the connection should open");

    let events: Vec<String> = connection
        .into_events()
        .filter_map(|event| async move { event.ok() })
        .collect()
        .await;

    assert!(
        events.iter().any(|event| event.contains(CODE)),
        "the code should survive the round trip: {events:?}"
    );
}

/// Compression turned off connects without offering it, and still works.
///
/// Not a degraded mode: a server that does not select the extension is a normal
/// operating condition, and the turn has to proceed either way.
#[tokio::test]
async fn compression_can_be_declined_and_the_turn_still_runs() {
    const MARKER: &str = "n3vq6-PLAIN-k1sd7";

    let server = DeflateServer::start(vec![
        r#"{"type":"response.completed","response":{"id":"r1"}}"#.to_owned(),
    ])
    .await;

    let transport = WebSocketTransport::new(&server.url).with_compression(false);
    let connection = transport
        .open(&request(MARKER), None, None)
        .await
        .expect("the connection should open");
    let _ = connection.into_events().collect::<Vec<_>>().await;

    let offered = server.offered();
    assert!(
        !offered
            .iter()
            .any(|value| value.contains("permessage-deflate")),
        "compression was declined, so it should not be offered: {offered:?}"
    );

    let received = server.received();
    assert!(
        received[0].contains(MARKER),
        "the turn should run regardless: {}",
        received[0]
    );
}

// ---------------------------------------------------------------------------
// The upgrade's identity headers. Nothing covered these before, which is how
// two of them went missing without a test noticing.
// ---------------------------------------------------------------------------

/// `docs/proxy-behavior.md` §2.8 — the upgrade carries the same identity as
/// every other upstream request.
///
/// `originator` and `user-agent` were absent here while the HTTP transport and
/// the catalog fetch both sent them. The catalog endpoint rejects a request
/// without them with a bare 400 that names nothing, so an endpoint that starts
/// enforcing them would look like an unexplained connection failure.
#[tokio::test]
async fn the_upgrade_carries_the_identity_headers() {
    let server = DeflateServer::start(vec![
        r#"{"type":"response.completed","response":{"id":"r1"}}"#.to_owned(),
    ])
    .await;

    let transport = WebSocketTransport::new(&server.url);
    let connection = transport
        .open(&request("hello"), None, None)
        .await
        .expect("the connection should open");
    let _ = connection.into_events().collect::<Vec<_>>().await;

    let headers = server.headers();
    let seen = headers.first().expect("one upgrade should have arrived");

    assert_eq!(
        seen.get("openai-beta").map(String::as_str),
        Some(proxenos::upstream::websocket::BETA_HEADER)
    );
    assert_eq!(
        seen.get("originator").map(String::as_str),
        Some(proxenos::upstream::http::ORIGINATOR)
    );
    assert_eq!(
        seen.get("user-agent").map(String::as_str),
        Some(proxenos::upstream::http::USER_AGENT)
    );
}
