//! `docs/proxy-behavior.md` §4 and §9.4 — transports and the incremental path.
//!
//! Incremental upload is the one subsystem whose bugs corrupt conversations
//! instead of failing loudly. Identical conversation state across both
//! transports is the criterion that catches it: if WebSocket and HTTP disagree
//! on a single item, the delta logic is wrong.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use codex_cc_proxy::upstream::compression::worth_compressing;
use codex_cc_proxy::upstream::compression::zstd as compress;
use codex_cc_proxy::upstream::conduit::Conduit;
use codex_cc_proxy::upstream::conduit::Sent;
use codex_cc_proxy::upstream::http::HttpTransport;
use codex_cc_proxy::upstream::websocket::Upload;
use codex_cc_proxy::upstream::websocket::WebSocketTransport;
use codex_cc_proxy::upstream::websocket::plan_upload;
use codex_cc_proxy_core::responses::InputItem;
use codex_cc_proxy_core::responses::ResponsesRequest;
use codex_cc_proxy_core::session::Baseline;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;

fn items(values: Value) -> Vec<InputItem> {
    serde_json::from_value(values).expect("items should deserialize")
}

fn message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }],
    })
}

fn request(input: Vec<InputItem>) -> ResponsesRequest {
    ResponsesRequest {
        model: "gpt-5.6-terra".to_owned(),
        instructions: Some("Be brief.".to_owned()),
        input,
        stream: true,
        ..ResponsesRequest::default()
    }
}

// ---------------------------------------------------------------------------
// §9.4 — the invariants, at the transport layer.
// ---------------------------------------------------------------------------

/// A valid delta contains exactly the new items, and names what it continues.
#[test]
fn a_delta_carries_only_the_new_items() {
    let mut baseline = Baseline::new();
    let first = items(json!([message("one")]));
    baseline.advance(&first, &[]);

    let second = request(items(json!([message("one"), message("two")])));
    let previous = request(first);

    match plan_upload(&baseline, &second, Some(&previous), Some("resp_1")) {
        Upload::Delta {
            items,
            previous_response_id,
        } => {
            assert_eq!(items.len(), 1);
            assert_eq!(previous_response_id, "resp_1");
        }
        Upload::Full => panic!("this should have been a delta"),
    }
}

/// Any change to a non-input field forces a full send. A different tool list is
/// a different request, and attaching new items to the wrong context is exactly
/// the silent corruption this rule prevents.
#[test]
fn a_changed_non_input_field_forces_a_full_send() {
    let mut baseline = Baseline::new();
    let first = items(json!([message("one")]));
    baseline.advance(&first, &[]);

    let previous = request(first);
    let mut second = request(items(json!([message("one"), message("two")])));
    second.instructions = Some("Be verbose instead.".to_owned());

    assert_eq!(
        plan_upload(&baseline, &second, Some(&previous), Some("resp_1")),
        Upload::Full
    );
}

/// The comparison covers fields added later. It strips `input` and compares
/// what remains, rather than listing fields by hand — a hand-written list stops
/// being exhaustive the moment the struct grows, and the failure that produces
/// is a wrong delta.
#[test]
fn the_field_comparison_covers_fields_it_was_not_written_for() {
    let mut baseline = Baseline::new();
    let first = items(json!([message("one")]));
    baseline.advance(&first, &[]);

    let previous = request(first);
    let mut second = request(items(json!([message("one"), message("two")])));
    second.prompt_cache_key = Some("a different session".to_owned());

    assert_eq!(
        plan_upload(&baseline, &second, Some(&previous), Some("resp_1")),
        Upload::Full
    );
}

/// A non-extending input forces a full send.
#[test]
fn an_edited_history_forces_a_full_send() {
    let mut baseline = Baseline::new();
    baseline.advance(&items(json!([message("one"), message("two")])), &[]);

    let previous = request(items(json!([message("one"), message("two")])));
    let second = request(items(json!([message("one"), message("EDITED")])));

    assert_eq!(
        plan_upload(&baseline, &second, Some(&previous), Some("resp_1")),
        Upload::Full
    );
}

/// With no previous response there is nothing to continue, so nothing is sent
/// incrementally.
#[test]
fn a_delta_needs_a_response_to_continue() {
    let mut baseline = Baseline::new();
    let first = items(json!([message("one")]));
    baseline.advance(&first, &[]);

    let previous = request(first);
    let second = request(items(json!([message("one"), message("two")])));

    assert_eq!(
        plan_upload(&baseline, &second, Some(&previous), None),
        Upload::Full
    );
}

/// Server-returned items are part of the baseline and are never resent.
#[test]
fn server_returned_items_are_never_resent() {
    let mut baseline = Baseline::new();
    let sent = items(json!([message("ask")]));
    let returned = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Read",
        "arguments": "{}",
    }]));
    baseline.advance(&sent, &returned);

    let previous = request(sent);
    let next = request(items(json!([
        message("ask"),
        { "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}" },
        { "type": "function_call_output", "call_id": "call_1", "output": "contents" },
    ])));

    match plan_upload(&baseline, &next, Some(&previous), Some("resp_1")) {
        Upload::Delta { items, .. } => {
            assert_eq!(items.len(), 1, "only the tool output is new");
        }
        Upload::Full => panic!("the server's own item should not have forced a full send"),
    }
}

// ---------------------------------------------------------------------------
// §4.4 — compression.
// ---------------------------------------------------------------------------

#[test]
fn a_large_payload_compresses_smaller() {
    let payload = json!({ "input": vec!["a repetitive sentence"; 400] }).to_string();

    assert!(worth_compressing(&payload));
    let compressed = compress(&payload).expect("compression should succeed");

    assert!(
        compressed.len() < payload.len() / 2,
        "{} bytes from {}",
        compressed.len(),
        payload.len()
    );
}

/// Small payloads grow when compressed once the frame is counted. Sending them
/// uncompressed is the correct outcome, not a failed optimization.
#[test]
fn a_small_payload_is_not_worth_compressing() {
    assert!(!worth_compressing(r#"{"input":[]}"#));
}

#[test]
fn compression_round_trips() {
    let payload = json!({ "text": "x".repeat(4_000) }).to_string();
    let compressed = compress(&payload).unwrap();
    let restored = zstd::decode_all(compressed.as_slice()).unwrap();

    assert_eq!(String::from_utf8(restored).unwrap(), payload);
}

// ---------------------------------------------------------------------------
// A WebSocket server on loopback, replaying recorded events.
// ---------------------------------------------------------------------------

struct WsServer {
    url: String,
    received: Arc<Mutex<Vec<String>>>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

/// What the server does when a client connects.
#[derive(Clone, Copy)]
enum WsBehavior {
    /// Accept, replay the events, close.
    Replay,
    /// Close immediately with a policy code, as the backend does on some
    /// accounts. Fallback is not a rare path.
    PolicyClose,
    /// Serve one turn, then close — the idle connection the backend reaps
    /// between turns. The next turn finds a socket that is already gone.
    CloseAfterTurn,
}

impl WsServer {
    async fn start(events: Vec<Value>, behavior: WsBehavior) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&connections);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let events = events.clone();
                let sink = Arc::clone(&sink);
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    if matches!(behavior, WsBehavior::PolicyClose) {
                        let _ = socket
                            .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
                                reason: "policy".into(),
                            }))
                            .await;
                        return;
                    }

                    // Several turns arrive on one connection: §4.1 caches it per
                    // session rather than per request, so a server that closed
                    // after one frame would make reuse untestable.
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

                        let prewarm = serde_json::from_str::<Value>(&body)
                            .ok()
                            .and_then(|frame| frame.get("generate").and_then(Value::as_bool))
                            == Some(false);

                        if let Ok(mut received) = sink.lock() {
                            received.push(body);
                        }

                        // A prewarm produces nothing at all.
                        if prewarm {
                            continue;
                        }

                        for event in &events {
                            let _ = socket
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    event.to_string().into(),
                                ))
                                .await;
                        }

                        if matches!(behavior, WsBehavior::CloseAfterTurn) {
                            let _ = socket.close(None).await;
                            break;
                        }
                    }
                });
            }
        });

        Self {
            url: format!("ws://{addr}"),
            received,
            connections,
        }
    }

    /// How many sockets were accepted. One per session is the claim §4.1 makes.
    fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn received(&self) -> Vec<String> {
        self.received
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }

    /// Wait for the server to have recorded a request. Asserting immediately
    /// races the server's own read.
    async fn wait_for_request(&self) -> String {
        for _ in 0..200 {
            if let Some(first) = self.received().first() {
                return first.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the server never received a request");
    }
}

fn stream_events() -> Vec<Value> {
    vec![
        json!({ "type": "response.created", "response": { "id": "resp_ws" } }),
        json!({ "type": "response.output_text.delta", "delta": "over the socket" }),
        json!({ "type": "response.completed", "response": { "id": "resp_ws" } }),
    ]
}

#[tokio::test]
async fn the_websocket_transport_carries_a_turn() {
    let server = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let transport = WebSocketTransport::new(server.url.clone());

    let connection = transport
        .open(&request(items(json!([message("hello")]))), None)
        .await
        .expect("the socket should open");

    let payloads: Vec<String> = connection
        .into_events()
        .filter_map(|event| async move { event.ok() })
        .collect()
        .await;

    assert_eq!(payloads.len(), 3);
    assert!(payloads[1].contains("over the socket"));

    // The outbound frame names what it is.
    let sent: Value = serde_json::from_str(&server.wait_for_request().await).unwrap();
    assert_eq!(sent["type"], json!("response.create"));
    assert_eq!(sent["model"], json!("gpt-5.6-terra"));
}

/// A delta names the response it continues. Without that the backend has no way
/// to know what the items extend.
#[tokio::test]
async fn a_delta_names_the_response_it_continues() {
    let server = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let transport = WebSocketTransport::new(server.url.clone());

    let _ = transport
        .open(
            &request(items(json!([message("only the new turn")]))),
            Some("resp_previous".to_owned()),
        )
        .await
        .unwrap();

    let sent: Value = serde_json::from_str(&server.wait_for_request().await).unwrap();
    assert_eq!(sent["previous_response_id"], json!("resp_previous"));
}

/// A full send carries no previous response id. Sending one alongside the whole
/// conversation would duplicate everything before it.
#[tokio::test]
async fn a_full_send_carries_no_previous_response_id() {
    let server = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let transport = WebSocketTransport::new(server.url.clone());

    let _ = transport
        .open(&request(items(json!([message("everything")]))), None)
        .await
        .unwrap();

    let sent: Value = serde_json::from_str(&server.wait_for_request().await).unwrap();
    assert!(sent.get("previous_response_id").is_none());
}

/// §4.2 — a policy close falls back to HTTP without losing the turn.
#[tokio::test]
async fn a_policy_close_falls_back_without_losing_the_turn() {
    let ws = WsServer::start(Vec::new(), WsBehavior::PolicyClose).await;

    // The HTTP side answers normally.
    let http_events = stream_events();
    let http = axum::Router::new().route(
        "/responses",
        axum::routing::post(move || {
            let body = http_events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, http).await;
    });

    let conduit = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(WebSocketTransport::new(ws.url.clone()))),
    );

    let (events, sent) = conduit
        .send(
            &request(items(json!([message("hi")]))),
            &Baseline::new(),
            None,
            None,
        )
        .await
        .expect("the turn should complete over HTTP");

    let payloads: Vec<String> = events
        .filter_map(|event| async move { event.ok() })
        .collect()
        .await;

    assert_eq!(payloads.len(), 3, "the turn was lost");
    assert_eq!(sent, Sent::Full);
    assert!(conduit.is_latched_to_http());
}

/// Once latched, a session does not try the WebSocket again. Retrying every
/// turn spends a failed handshake per turn on the latency path to re-learn what
/// the first one established.
#[tokio::test]
async fn a_latched_session_stops_trying_the_websocket() {
    let ws = WsServer::start(Vec::new(), WsBehavior::PolicyClose).await;
    let http_events = stream_events();
    let http = axum::Router::new().route(
        "/responses",
        axum::routing::post(move || {
            let body = http_events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, http).await;
    });

    let conduit = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(WebSocketTransport::new(ws.url.clone()))),
    );

    for _ in 0..3 {
        let _ = conduit
            .send(
                &request(items(json!([message("hi")]))),
                &Baseline::new(),
                None,
                None,
            )
            .await
            .unwrap();
    }

    assert!(conduit.is_latched_to_http());
    assert_eq!(
        ws.received().len(),
        0,
        "the websocket was contacted after latching"
    );
}

// ---------------------------------------------------------------------------
// The criterion that matters: identical conversation state across transports.
// ---------------------------------------------------------------------------

/// Reconstruct the conversation as the *backend* would hold it, from what it
/// was actually sent.
///
/// This is the whole point. Comparing what the proxy believes it sent proves
/// nothing — both transports compute that the same way. What differs is the
/// wire: HTTP carries the whole conversation every turn, while a delta carries
/// only new items and asks the backend to append them to a response it already
/// has. Rebuilding the backend's view from the payloads is the only comparison
/// that can catch a delta which dropped or duplicated a turn, because that
/// failure produces no error at all.
fn backend_view(payloads: &[String], replies: &[Value]) -> Vec<Value> {
    let mut view: Vec<Value> = Vec::new();

    for (turn, payload) in payloads.iter().enumerate() {
        let Ok(frame) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        let input = frame
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if frame.get("previous_response_id").is_some() {
            // A delta appends to what the backend already had.
            view.extend(input);
        } else {
            // A full send replaces it. The client has replayed every earlier
            // reply, so they are already in there.
            view = input;
        }

        // Then the backend adds its own output. On the delta path this is the
        // only way those items are ever in its view, which is exactly why §4.3
        // forbids resending them.
        if let Some(reply) = replies.get(turn) {
            view.push(reply.clone());
        }
    }

    view
}

/// The reply the fake backend produces on turn `n`, in the item form the client
/// replays it as.
fn reply_item(turn: usize) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": format!("reply {turn}") }],
    })
}

/// Drive a long session over a conduit and return the conversation state it
/// leaves behind, rendered as JSON.
async fn drive(conduit: &Conduit, turns: usize) -> (String, Vec<Sent>) {
    let mut baseline = Baseline::new();
    let mut previous_request: Option<ResponsesRequest> = None;
    let mut previous_response_id: Option<String> = None;
    let mut conversation: Vec<InputItem> = Vec::new();
    let mut how_sent = Vec::new();

    for turn in 0..turns {
        // The client replays the whole conversation every turn, as it does.
        conversation.extend(items(json!([message(&format!("user turn {turn}"))])));
        let request = request(conversation.clone());

        let (events, sent) = conduit
            .send(
                &request,
                &baseline,
                previous_request.as_ref(),
                previous_response_id.as_deref(),
            )
            .await
            .expect("the turn should complete");

        // Drain, and note what the server added.
        let payloads: Vec<String> = events
            .filter_map(|event| async move { event.ok() })
            .collect()
            .await;
        for payload in &payloads {
            if let Ok(event) = serde_json::from_str::<Value>(payload)
                && let Some(id) = event.pointer("/response/id").and_then(Value::as_str)
            {
                previous_response_id = Some(id.to_owned());
            }
        }

        // The server's reply becomes part of the conversation, exactly as the
        // client will replay it next turn.
        let returned = items(json!([{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": format!("reply {turn}") }],
        }]));
        conversation.extend(returned.clone());

        baseline.advance(&request.input, &returned);
        previous_request = Some(request);
        how_sent.push(sent);
    }

    (
        serde_json::to_string_pretty(baseline.items()).unwrap_or_default(),
        how_sent,
    )
}

fn http_only(addr: std::net::SocketAddr) -> Conduit {
    Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        None,
    )
}

/// An HTTP replay server that also records the bodies it was sent, so the
/// backend's view can be rebuilt from them.
async fn start_recording_http() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let events = stream_events();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&bodies);

    let app = axum::Router::new().route(
        "/responses",
        axum::routing::post(move |body: String| {
            if let Ok(mut bodies) = sink.lock() {
                bodies.push(body);
            }
            let body = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, bodies)
}

async fn start_http() -> std::net::SocketAddr {
    let events = stream_events();
    let app = axum::Router::new().route(
        "/responses",
        axum::routing::post(move || {
            let body = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// **The acceptance criterion.** A long session produces byte-identical
/// conversation state over both transports.
#[tokio::test]
async fn a_long_session_leaves_identical_state_over_both_transports() {
    let http_addr = start_recording_http().await;
    let ws = WsServer::start(stream_events(), WsBehavior::Replay).await;

    let over_http = Conduit::new(
        Arc::new(HttpTransport::new(format!(
            "http://{}/responses",
            http_addr.0
        ))),
        None,
    );
    let over_websocket = Conduit::new(
        Arc::new(HttpTransport::new(format!(
            "http://{}/responses",
            http_addr.0
        ))),
        Some(Arc::new(
            WebSocketTransport::new(ws.url.clone()).with_compression(false),
        )),
    );

    let (_, http_sends) = drive(&over_http, 20).await;
    let http_payloads = http_addr.1.lock().unwrap().clone();

    let (_, websocket_sends) = drive(&over_websocket, 20).await;
    let websocket_payloads = ws.received();

    // Rebuilt from what each transport actually put on the wire.
    let replies: Vec<Value> = (0..20).map(reply_item).collect();
    let from_http = backend_view(&http_payloads, &replies);
    let from_websocket = backend_view(&websocket_payloads, &replies);

    assert_eq!(
        serde_json::to_string_pretty(&from_http).unwrap(),
        serde_json::to_string_pretty(&from_websocket).unwrap(),
        "the backend would hold different conversations depending on the transport"
    );
    assert!(!from_http.is_empty(), "the reconstruction found nothing");

    // And the two genuinely took different paths, or the comparison proves
    // nothing: HTTP is stateless and always sends everything, while the
    // WebSocket sends deltas once it has a response to continue.
    assert!(
        http_sends.iter().all(|sent| *sent == Sent::Full),
        "HTTP should always send the whole conversation"
    );
    assert!(
        websocket_sends
            .iter()
            .filter(|sent| **sent == Sent::Delta)
            .count()
            >= 15,
        "the websocket path never sent a delta, so this proves nothing: {websocket_sends:?}"
    );
}

/// A session that falls back mid-way still ends in the same state. This is the
/// case where the two paths interleave, and where a delta computed against a
/// baseline the other transport advanced would corrupt the conversation.
#[tokio::test]
async fn a_session_that_falls_back_midway_ends_in_the_same_state() {
    let addr = start_http().await;
    let ws = WsServer::start(Vec::new(), WsBehavior::PolicyClose).await;

    let mixed = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(WebSocketTransport::new(ws.url.clone()))),
    );
    let pure_http = http_only(addr);

    let (mixed_state, _) = drive(&mixed, 12).await;
    let (http_state, _) = drive(&pure_http, 12).await;

    assert_eq!(mixed_state, http_state);
    assert!(mixed.is_latched_to_http());
}

/// The delta actually shrinks what is sent. Without this the incremental path
/// is machinery that changes nothing.
#[tokio::test]
async fn a_delta_sends_less_than_the_whole_conversation() {
    let ws = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let transport = WebSocketTransport::new(ws.url.clone()).with_compression(false);

    // A long conversation, sent whole.
    let mut long = Vec::new();
    for turn in 0..40 {
        long.extend(items(json!([message(&format!("turn {turn}"))])));
    }
    let _ = transport.open(&request(long.clone()), None).await.unwrap();
    let full = ws.wait_for_request().await.len();

    // The same conversation continued by one item.
    let ws2 = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let transport2 = WebSocketTransport::new(ws2.url.clone()).with_compression(false);
    let one_more = items(json!([message("just the new turn")]));
    let _ = transport2
        .open(&request(one_more), Some("resp_previous".to_owned()))
        .await
        .unwrap();
    let delta = ws2.wait_for_request().await.len();

    assert!(
        delta * 4 < full,
        "the delta ({delta} bytes) is not meaningfully smaller than the full send ({full} bytes)"
    );
}

/// §4.1 — one connection, cached per session and reused.
///
/// Reuse removes per-turn TCP and TLS setup, which is significant in an agent
/// loop issuing many sequential requests. A server that only ever sees one
/// connection for many turns is what that claim means.
#[tokio::test]
async fn one_connection_serves_a_whole_session() {
    let ws = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let addr = start_http().await;
    let conduit = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(
            WebSocketTransport::new(ws.url.clone()).with_compression(false),
        )),
    );

    let (_, sends) = drive(&conduit, 8).await;

    assert_eq!(ws.connections(), 1, "a connection was opened per turn");
    assert_eq!(ws.received().len(), 8, "not every turn reached the socket");
    assert!(
        conduit.has_pooled_connection().await,
        "the connection was not kept"
    );
    assert!(!conduit.is_latched_to_http());

    // And reuse is what makes the delta path reachable at all: a delta
    // continues a response the connection already knows about.
    assert_eq!(sends.iter().filter(|sent| **sent == Sent::Delta).count(), 7);
}

/// §4.1 — a prewarm opens the connection before the turn that will use it, and
/// produces no response of its own.
#[tokio::test]
async fn a_prewarm_opens_the_connection_without_producing_a_turn() {
    let ws = WsServer::start(stream_events(), WsBehavior::Replay).await;
    let addr = start_http().await;
    let conduit = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(
            WebSocketTransport::new(ws.url.clone()).with_compression(false),
        )),
    );

    conduit
        .prewarm(&request(items(json!([message("warm")]))))
        .await;

    assert!(conduit.has_pooled_connection().await);
    let warmed: Value = serde_json::from_str(&ws.wait_for_request().await).unwrap();
    assert_eq!(
        warmed["generate"],
        json!(false),
        "a prewarm must not generate"
    );

    // The turn that follows reuses it rather than opening another.
    let (events, _) = conduit
        .send(
            &request(items(json!([message("real")]))),
            &Baseline::new(),
            None,
            None,
        )
        .await
        .unwrap();
    let payloads: Vec<String> = events
        .filter_map(|event| async move { event.ok() })
        .collect()
        .await;

    assert_eq!(payloads.len(), 3);
    assert_eq!(
        ws.connections(),
        1,
        "the prewarmed connection was not reused"
    );
}

/// A prewarm on a session already latched to HTTP does nothing. Opening a
/// socket that will never be used spends a handshake to no end.
#[tokio::test]
async fn a_latched_session_does_not_prewarm() {
    let ws = WsServer::start(Vec::new(), WsBehavior::PolicyClose).await;
    let addr = start_http().await;
    let conduit = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(WebSocketTransport::new(ws.url.clone()))),
    );

    let _ = conduit
        .send(
            &request(items(json!([message("hi")]))),
            &Baseline::new(),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(conduit.is_latched_to_http());

    let before = ws.connections();
    conduit
        .prewarm(&request(items(json!([message("warm")]))))
        .await;

    assert_eq!(
        ws.connections(),
        before,
        "a latched session tried to prewarm"
    );
    assert!(!conduit.has_pooled_connection().await);
}

/// §4.1 — a pooled connection the backend reaped between turns costs a
/// handshake, not the session's transport and not the conversation.
///
/// Two things must hold, and the second is the dangerous one. The session must
/// not latch to HTTP for the rest of its life over one stale socket; and the
/// retry must go out as a *full* send, because `previous_response_id` names a
/// response the closed socket held. Replaying the delta against a connection
/// that never saw it uploads a fragment as though it were the conversation —
/// the silent corruption a full send exists to avoid.
#[tokio::test]
async fn a_stale_pooled_connection_retries_as_a_full_send() {
    let ws = WsServer::start(stream_events(), WsBehavior::CloseAfterTurn).await;
    let addr = start_http().await;
    let conduit = Conduit::new(
        Arc::new(HttpTransport::new(format!("http://{addr}/responses"))),
        Some(Arc::new(
            WebSocketTransport::new(ws.url.clone()).with_compression(false),
        )),
    );

    let (state, sends) = drive(&conduit, 2).await;

    assert!(
        !conduit.is_latched_to_http(),
        "one expired socket latched the whole session to HTTP"
    );
    assert_eq!(
        ws.connections(),
        2,
        "the second turn did not reopen the connection"
    );
    assert_eq!(
        sends,
        vec![Sent::Full, Sent::Full],
        "the retry was recorded as a delta"
    );

    for frame in ws.received() {
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert!(
            frame.get("previous_response_id").is_none_or(Value::is_null),
            "a delta was replayed on a connection that never saw the response it names: {frame}"
        );
    }

    // And the conversation upstream ends up where HTTP would have put it.
    let http_addr = start_http().await;
    let (http_state, _) = drive(&http_only(http_addr), 2).await;
    assert_eq!(state, http_state);
}
