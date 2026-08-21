//! `record surface` — capturing the real Messages surface as fixtures.
//!
//! The proxy's whole product is an Anthropic Messages surface, and every claim
//! about its conformance was derived from documentation and from captures of
//! the *client* side. Nothing in the corpus was ever the real backend's own
//! answer, because no verb produced one: upstream capture is wired into the
//! translating path only, and a relayed turn (§9) streams back untouched with
//! nothing recording it.
//!
//! These assert the capture format offline, against a stub that answers the
//! shapes the real endpoint answers. The live verb is the same code path — a
//! test harness that reimplements the runner proves the harness works.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use axum::http::HeaderMap;
use proxenos::auth::store::AccountStore;
use proxenos::auth::store::CredentialStore;
use proxenos::auth::store::FileStore;
use proxenos::auth::store::Provider;
use proxenos::surface;
use proxenos::upstream::relay::Relay;
use serde_json::Value;
use std::sync::Arc;

/// A streaming answer in the shape the real endpoint streams: named events,
/// one JSON payload each, terminated by a blank line.
const STREAM: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

#[test]
fn a_stream_becomes_one_entry_per_event_in_order() {
    let events = surface::sse_events(STREAM);

    let names: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["message_start", "content_block_delta", "message_stop"]
    );
}

/// The terminator is a framing artefact, not an event. Recorded as one it would
/// appear in every vocabulary comparison as an event name the proxy fails to
/// emit.
#[test]
fn the_stream_terminator_is_not_recorded_as_an_event() {
    let events = surface::sse_events("data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n\n");
    assert_eq!(events.len(), 1);
}

/// A capture is written to disk. A credential in a file that is not the
/// credential store is a leak however it got there — and on this path the
/// proxy supplied the credential itself, so the header set is its own doing.
#[test]
fn credential_and_identifying_headers_never_reach_a_capture() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer sk-ant-oat-not-a-real-token"),
        ("x-api-key", "sk-ant-also-not-real"),
        ("set-cookie", "session=secret"),
        ("anthropic-organization-id", "org_12345"),
        ("anthropic-workspace-id", "wrkspc_67890"),
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("content-type", "application/json"),
    ] {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }

    let scrubbed = surface::scrubbed(&headers);
    let rendered = serde_json::to_string(&scrubbed).unwrap();

    assert!(!rendered.contains("sk-ant"), "a token reached a capture");
    assert!(!rendered.contains("secret"), "a cookie reached a capture");
    assert!(
        !rendered.contains("org_12345"),
        "an org id reached a capture"
    );
    assert!(
        !rendered.contains("wrkspc_67890"),
        "a workspace id reached a capture"
    );
    // The name survives where the value cannot: that the header was sent at
    // all is the datum a conformance question asks about.
    assert!(rendered.contains("authorization"));
    assert!(
        rendered.contains("allowed"),
        "quota headers are not secrets"
    );
}

/// A stand-in for the real endpoint: streams for a streaming request, answers
/// JSON otherwise, refuses an unknown model the way the real one refuses, and
/// answers `count_tokens` on its own path.
async fn endpoint() -> String {
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(|body: String| async move {
                let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                if request["model"].as_str().is_some_and(|model| model.starts_with("claude-not-a-model")) {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        "{\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"model: not found\"}}".to_owned(),
                    );
                }
                if request["stream"] == Value::Bool(true) {
                    return (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        STREAM.to_owned(),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "{\"type\":\"message\",\"id\":\"msg_y\",\"content\":[]}".to_owned(),
                )
            }),
        )
        .route(
            "/v1/messages/count_tokens",
            axum::routing::post(|_body: String| async move {
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "{\"input_tokens\":14}".to_owned(),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1/messages")
}

/// The whole assembly, not the pieces: the plan list, the relay, the parse, and
/// the write. Three features here have died in the wiring while every unit test
/// passed, so this drives the shipping runner.
#[tokio::test]
async fn the_runner_writes_one_fixture_per_planned_exchange() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(home.path().join("credentials.json")));
    store
        .add_key("personal", "sk-ant-oat01-stub", Provider::Anthropic)
        .expect("the stub key should store");

    let authorizer: Arc<dyn proxenos::auth::authorize::Authorizer> =
        Arc::new(proxenos::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&store) as Arc<dyn proxenos::auth::store::AccountStore>,
            Arc::new(proxenos::auth::tokens::TokenSource::new(
                Arc::clone(&store) as Arc<dyn CredentialStore>,
                String::new(),
                "client-abc",
                Arc::new(proxenos::auth::tokens::SystemClock),
            )),
        ));

    let base = endpoint().await;
    let relay = Relay::new(
        &base,
        Arc::clone(&store) as Arc<dyn proxenos::auth::store::AccountStore>,
        Arc::clone(&authorizer),
    );
    let counter = Relay::new(
        format!("{base}/count_tokens"),
        Arc::clone(&store) as Arc<dyn proxenos::auth::store::AccountStore>,
        Arc::clone(&authorizer),
    );

    let out = home.path().join("surface");
    let written = surface::capture_all(&relay, &counter, "personal", &out)
        .await
        .expect("the stub should answer every planned exchange");

    assert_eq!(written.len(), surface::PLANS.len());

    let by_name: std::collections::BTreeMap<String, surface::Capture> = written
        .iter()
        .map(|path| {
            let raw = std::fs::read_to_string(path).unwrap();
            let capture: surface::Capture = serde_json::from_str(&raw).unwrap();
            (capture.name.clone(), capture)
        })
        .collect();

    // A plain generation is a body, not a stream.
    let plain = &by_name["plain-generation"];
    assert_eq!(plain.status, 200);
    assert!(plain.events.is_empty());
    assert_eq!(plain.body.as_ref().unwrap()["type"], "message");

    // A streaming turn is events, in order, and no body.
    let streamed = &by_name["streaming-tool-call"];
    assert_eq!(streamed.events[0]["type"], "message_start");
    assert!(streamed.body.is_none());

    // A refusal is captured with its status and its envelope intact — that
    // envelope is the thing the conformance check measures against.
    let refused = &by_name["error-envelope"];
    assert_eq!(refused.status, 404);
    assert_eq!(
        refused.body.as_ref().unwrap()["error"]["type"],
        "not_found_error"
    );

    // Sizing has its own endpoint, and it is answered locally by the proxy
    // (§5), so nothing but a direct call can say what the real one returns.
    let sized = &by_name["count-tokens"];
    assert_eq!(sized.endpoint, "/v1/messages/count_tokens");
    assert_eq!(sized.body.as_ref().unwrap()["input_tokens"], 14);

    // Provenance is part of a fixture, and this one was observed.
    assert!(written.iter().all(|path| {
        let raw = std::fs::read_to_string(path).unwrap();
        raw.contains("\"provenance\": \"captured\"")
    }));

    // The ordinary streaming case is planned too. Without it the corpus can
    // measure a tool call's blocks and not a text one's, and text is what
    // nearly every frame a client renders carries.
    let text = &by_name["streaming-text"];
    assert_eq!(text.events[0]["type"], "message_start");

    // No capture carries the token that fetched it.
    for path in &written {
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(
            !raw.contains("sk-ant"),
            "{} carries a token",
            path.display()
        );
    }
}

/// A capture on disk is quota already spent. Adding a shape to the corpus must
/// not re-buy the ones that are already there.
#[tokio::test]
async fn one_named_exchange_can_be_captured_without_paying_for_the_rest() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(home.path().join("credentials.json")));
    store
        .add_key("personal", "sk-ant-oat01-stub", Provider::Anthropic)
        .expect("the stub key should store");

    let authorizer: Arc<dyn proxenos::auth::authorize::Authorizer> =
        Arc::new(proxenos::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&store) as Arc<dyn proxenos::auth::store::AccountStore>,
            Arc::new(proxenos::auth::tokens::TokenSource::new(
                Arc::clone(&store) as Arc<dyn CredentialStore>,
                String::new(),
                "client-abc",
                Arc::new(proxenos::auth::tokens::SystemClock),
            )),
        ));

    let base = endpoint().await;
    let relay = Relay::new(
        &base,
        Arc::clone(&store) as Arc<dyn proxenos::auth::store::AccountStore>,
        Arc::clone(&authorizer),
    );

    let out = home.path().join("surface");
    let written = surface::capture_some(&relay, &relay, "personal", &out, Some("streaming-text"))
        .await
        .expect("the stub should answer the named exchange");

    assert_eq!(written.len(), 1);
    assert!(written[0].ends_with("streaming-text.json"));

    // A name no plan carries is refused rather than quietly capturing nothing:
    // a run that spends no quota and reports success is the same shape as a
    // typo.
    let refused =
        surface::capture_some(&relay, &relay, "personal", &out, Some("no-such-plan")).await;
    assert!(refused.is_err());
}
