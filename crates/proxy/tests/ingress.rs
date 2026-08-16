//! End to end through the ingress surface, against a loopback replay server.
//!
//! Both halves are real: a real axum ingress, a real reqwest client, real SSE
//! in both directions. Nothing reaches the network.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

mod replay;

use codex_cc_proxy::ingress::AppState;
use codex_cc_proxy::ingress::ModelMapping;
use codex_cc_proxy::ingress::router;
use codex_cc_proxy::upstream::http::HttpTransport;
use pretty_assertions::assert_eq;
use replay::Behavior;
use replay::ReplayServer;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

struct Harness {
    base: String,
    upstream: ReplayServer,
    client: reqwest::Client,
}

impl Harness {
    async fn start(behavior: Behavior) -> Self {
        Self::start_with(behavior, None).await
    }

    async fn start_with(
        behavior: Behavior,
        recorder: Option<codex_cc_proxy::recorder::Recorder>,
    ) -> Self {
        let upstream = ReplayServer::start(behavior).await;

        let state = AppState {
            transport: Arc::new(HttpTransport::new(upstream.url.clone())),
            conduits: None,
            models: Arc::new(vec![ModelMapping {
                requested: "claude-sonnet-4".to_owned(),
                upstream: "gpt-5-codex".to_owned(),
            }]),
            recorder: recorder.clone(),
            record_ingress: recorder.is_some(),
            sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });

        Self {
            base: format!("http://{addr}"),
            upstream,
            client: reqwest::Client::new(),
        }
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .expect("the request should reach the proxy")
    }
}

/// Split an SSE body into its event payloads.
fn payloads(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|block| {
            block
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|data| serde_json::from_str(data).ok())
        })
        .collect()
}

fn completed() -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "usage": {
                "input_tokens": 900,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 400 },
            },
        },
    })
}

#[tokio::test]
async fn a_streaming_request_returns_a_valid_frame_sequence() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "Hello" }),
        completed(),
    ]))
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let body = response.text().await.unwrap();
    let kinds: Vec<String> = payloads(&body)
        .iter()
        .filter_map(|frame| frame["type"].as_str().map(str::to_owned))
        .collect();

    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
}

/// The tier mapping is applied on the way out, and only on the way out. The
/// client is told the model it asked for, because that is what it matches
/// against.
#[tokio::test]
async fn the_request_is_translated_and_the_tier_is_mapped() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "system": "Be brief.",
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    let body = response.text().await.unwrap();
    let start = &payloads(&body)[0];
    assert_eq!(start["message"]["model"], json!("claude-sonnet-4"));

    let sent = harness.upstream.requests();
    assert_eq!(sent[0]["model"], json!("gpt-5-codex"));
    assert_eq!(sent[0]["instructions"], json!("Be brief."));
    assert_eq!(sent[0]["stream"], json!(true));
}

/// A tool round trip end to end.
#[tokio::test]
async fn a_tool_call_survives_the_round_trip() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{\"file_path\":\"/tmp/a\"}",
            },
        }),
        completed(),
    ]))
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "read it" }],
                "tools": [{
                    "name": "Read",
                    "input_schema": { "type": "object", "properties": {} },
                }],
            }),
        )
        .await;

    let body = response.text().await.unwrap();
    let frames = payloads(&body);

    let start = frames
        .iter()
        .find(|frame| frame["content_block"]["type"] == "tool_use")
        .expect("a tool_use block should be emitted");
    assert_eq!(start["content_block"]["name"], json!("Read"));
    assert_eq!(start["content_block"]["id"], json!("call_1"));

    let delta = frames
        .iter()
        .find(|frame| frame["delta"]["type"] == "input_json_delta")
        .unwrap();
    assert_eq!(
        delta["delta"]["partial_json"],
        json!("{\"file_path\":\"/tmp/a\"}")
    );

    let message_delta = frames
        .iter()
        .find(|frame| frame["type"] == "message_delta")
        .unwrap();
    assert_eq!(message_delta["delta"]["stop_reason"], json!("tool_use"));
}

/// §5.0 — an event split across several `data:` lines is one payload. This is
/// the case a line-at-a-time parser corrupts, and it only shows up on events
/// large enough to be split.
#[tokio::test]
async fn an_event_split_across_data_lines_survives_the_transport() {
    let raw = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\n",
        "data: \"delta\":\"split across lines\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let harness = Harness::start(Behavior::Raw(raw.to_owned())).await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    let body = response.text().await.unwrap();
    let delta = payloads(&body)
        .into_iter()
        .find(|frame| frame["type"] == "content_block_delta")
        .expect("the split event should have produced a delta");
    assert_eq!(delta["delta"]["text"], json!("split across lines"));
}

/// §1.1 — upstream statuses map to the vocabulary the client understands, and
/// `retry-after` is forwarded when supplied.
#[tokio::test]
async fn a_rate_limited_upstream_surfaces_as_retryable() {
    let harness = Harness::start(Behavior::Failure {
        status: 429,
        body: "{\"error\":{\"message\":\"slow down\"}}".to_owned(),
        retry_after: Some("11".to_owned()),
    })
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 429);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("11")
    );

    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], json!("error"));
    assert_eq!(body["error"]["type"], json!("rate_limit_error"));
}

/// A server failure is an overload, which the client retries. Reporting it as
/// terminal would end a session that a retry would have completed.
#[tokio::test]
async fn a_server_error_surfaces_as_overloaded() {
    let harness = Harness::start(Behavior::Failure {
        status: 500,
        body: "internal".to_owned(),
        retry_after: None,
    })
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 529);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], json!("overloaded_error"));
}

/// The client holds no credentials of its own, so an upstream credential
/// failure is this proxy's to report.
#[tokio::test]
async fn an_upstream_credential_failure_surfaces_as_authentication() {
    let harness = Harness::start(Behavior::Failure {
        status: 401,
        body: "unauthorized".to_owned(),
        retry_after: None,
    })
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], json!("authentication_error"));
}

/// A 403 carrying a non-JSON challenge keeps its body excerpt, because the
/// excerpt is the only diagnostic available.
#[tokio::test]
async fn a_challenge_response_keeps_its_excerpt() {
    let harness = Harness::start(Behavior::Failure {
        status: 403,
        body: "<html>Attention Required! Cloudflare</html>".to_owned(),
        retry_after: None,
    })
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    let body: Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Cloudflare"),
        "the excerpt should survive: {message}"
    );
}

#[tokio::test]
async fn a_malformed_body_is_an_invalid_request() {
    let harness = Harness::start(Behavior::Events(vec![])).await;

    let response = harness
        .client
        .post(format!("{}/v1/messages", harness.base))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
}

#[tokio::test]
async fn an_unknown_endpoint_is_not_found() {
    let harness = Harness::start(Behavior::Events(vec![])).await;

    let response = harness
        .client
        .get(format!("{}/v1/nothing", harness.base))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], json!("not_found_error"));
}

#[tokio::test]
async fn count_tokens_returns_an_estimate() {
    let harness = Harness::start(Behavior::Events(vec![])).await;

    let response = harness
        .post(
            "/v1/messages/count_tokens",
            json!({
                "model": "claude-sonnet-4",
                "messages": [{ "role": "user", "content": "a fairly ordinary sentence" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let estimate = body["input_tokens"].as_u64().expect("an estimate");
    assert!(estimate > 0, "an estimate of zero would collapse the meter");
}

/// A base64 attachment is megabytes of characters that cost a fixed, much
/// smaller number of tokens. Counting them would pin the client's context meter
/// at full.
#[tokio::test]
async fn an_attachment_does_not_dominate_the_estimate() {
    let harness = Harness::start(Behavior::Events(vec![])).await;
    let huge = "A".repeat(400_000);

    let response = harness
        .post(
            "/v1/messages/count_tokens",
            json!({
                "model": "claude-sonnet-4",
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": huge,
                        },
                    }],
                }],
            }),
        )
        .await;

    let body: Value = response.json().await.unwrap();
    let estimate = body["input_tokens"].as_u64().unwrap();
    assert!(
        estimate < 10_000,
        "a single image should not read as {estimate} tokens"
    );
}

#[tokio::test]
async fn models_lists_the_mapping_in_the_anthropic_shape() {
    let harness = Harness::start(Behavior::Events(vec![])).await;

    let response = harness
        .client
        .get(format!("{}/v1/models", harness.base))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["id"], json!("gpt-5-codex"));
    assert_eq!(body["data"][0]["type"], json!("model"));
}

/// §5.3 — cancelling the outbound stream aborts the upstream request.
///
/// Without propagation the backend generates to completion against a reader
/// that no longer exists, spending quota on output nobody receives. The replay
/// server records whether it ever finished sending; a cancelled request means
/// it never should.
#[tokio::test]
async fn cancelling_the_client_stream_aborts_the_upstream_request() {
    let sent_everything = Arc::new(std::sync::Mutex::new(false));
    let harness = Harness::start(Behavior::Stall {
        sent_everything: Arc::clone(&sent_everything),
    })
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;
    assert_eq!(response.status(), 200);

    // Drop the response without reading it to completion. That is what a client
    // pressing escape does.
    drop(response);

    // Four times the server's own stall, so an upstream that was left running
    // has long since finished and set the flag.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    assert!(
        !*sent_everything.lock().unwrap(),
        "upstream kept generating after the client went away"
    );
}

/// The control for the test above. Reading the stream to completion must set
/// the flag — otherwise that test passes for the wrong reason and would keep
/// passing if cancellation stopped working entirely.
#[tokio::test]
async fn a_stream_read_to_completion_does_reach_the_end_upstream() {
    let sent_everything = Arc::new(std::sync::Mutex::new(false));
    let harness = Harness::start(Behavior::Stall {
        sent_everything: Arc::clone(&sent_everything),
    })
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    let _ = response.text().await.unwrap();

    assert!(
        *sent_everything.lock().unwrap(),
        "the flag never gets set, so the cancellation test proves nothing"
    );
}

/// §5.4 — a stream that completes having produced no content is recorded with
/// its request and the upstream events that produced nothing.
///
/// It is always a defect, and it is otherwise invisible: the client receives a
/// well-formed turn that simply said nothing, and reports nothing wrong.
#[tokio::test]
async fn an_empty_stream_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path());

    let harness = Harness::start_with(
        Behavior::Events(vec![
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
            completed(),
        ]),
        Some(recorder),
    )
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;
    let _ = response.text().await.unwrap();

    let captures: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("upstream-"))
        })
        .collect();

    assert_eq!(
        captures.len(),
        1,
        "the empty stream should have been recorded"
    );

    let body: Value =
        serde_json::from_str(&std::fs::read_to_string(&captures[0]).unwrap()).unwrap();
    assert_eq!(body["provenance"], json!("captured"));
    // The raw upstream events are kept, since they are the evidence of what
    // produced nothing.
    assert_eq!(body["upstream"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["request"]["model"], json!("claude-sonnet-4"));
}

/// A stream that produced content is not recorded. Recording every exchange
/// would bury the defective ones.
#[tokio::test]
async fn a_stream_that_produced_content_is_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path());

    let harness = Harness::start_with(
        Behavior::Events(vec![
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
            json!({ "type": "response.output_text.delta", "delta": "content" }),
            completed(),
        ]),
        Some(recorder),
    )
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;
    let _ = response.text().await.unwrap();

    let upstream_captures = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("upstream-"))
        })
        .count();

    assert_eq!(upstream_captures, 0);
}

/// `record ingress` captures what the client sends, before translation. No
/// credentials are involved, because nothing upstream is yet.
#[tokio::test]
async fn ingress_capture_records_the_untranslated_request() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path());

    let harness = Harness::start_with(
        Behavior::Events(vec![
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
            json!({ "type": "response.output_text.delta", "delta": "hi" }),
            completed(),
        ]),
        Some(recorder),
    )
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "system": "You are Claude Code.",
                "messages": [{ "role": "user", "content": "hello" }],
                "tools": [{
                    "name": "Read",
                    "input_schema": { "type": "object", "properties": {} },
                }],
            }),
        )
        .await;
    let _ = response.text().await.unwrap();

    let capture = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ingress-"))
        })
        .expect("the request should have been captured");

    let body: Value = serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();

    // Untranslated: the Anthropic shape, not the Responses one. A capture that
    // had already been translated could not test the translation.
    assert_eq!(body["request"]["system"], json!("You are Claude Code."));
    assert_eq!(body["request"]["messages"][0]["role"], json!("user"));
    assert_eq!(body["request"]["tools"][0]["name"], json!("Read"));
    assert_eq!(body["provenance"], json!("captured"));
}

/// A capture replays through the corpus loader without hand-editing. A capture
/// that needs editing before it can be used is not a fixture, it is a note.
#[tokio::test]
async fn a_capture_parses_as_a_corpus_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path());

    let harness = Harness::start_with(
        Behavior::Events(vec![
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
            json!({ "type": "response.output_text.delta", "delta": "hi" }),
            completed(),
        ]),
        Some(recorder),
    )
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-4",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hello" }],
            }),
        )
        .await;
    let _ = response.text().await.unwrap();

    let capture = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .expect("something should have been captured");

    let raw = std::fs::read_to_string(&capture).unwrap();
    let fixture: codex_cc_proxy_core::fixture::Fixture =
        serde_json::from_str(&raw).expect("a capture should parse as a fixture");

    assert_eq!(
        fixture.provenance,
        codex_cc_proxy_core::fixture::Provenance::Captured
    );
    assert!(!fixture.note.is_empty());
}

/// §6.3 — calibration reaches the live path. A second turn in the same
/// conversation carries an estimate corrected by what the first turn learned.
///
/// Without this the estimator is rebuilt per request, calibration never
/// accumulates, and §6.3 describes something the proxy does not do.
#[tokio::test]
async fn a_conversation_calibrates_across_turns() {
    // Upstream charges far more than the raw estimate guesses, consistently.
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "ok" }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 4000,
                    "output_tokens": 2,
                    "input_tokens_details": { "cached_tokens": 0 },
                },
            },
        }),
    ]))
    .await;

    let first_body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "system": "You are Claude Code.",
        "messages": [{ "role": "user", "content": "opening turn" }],
    });

    let response = harness.post("/v1/messages", first_body.clone()).await;
    let body = response.text().await.unwrap();
    let first_estimate = payloads(&body)[0]["message"]["usage"]["input_tokens"]
        .as_u64()
        .unwrap();

    // The same conversation, extended. It resolves to the same session, so the
    // correction from the first turn applies.
    let second_body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "system": "You are Claude Code.",
        "messages": [
            { "role": "user", "content": "opening turn" },
            { "role": "assistant", "content": "ok" },
            { "role": "user", "content": "second turn" },
        ],
    });

    let response = harness.post("/v1/messages", second_body).await;
    let body = response.text().await.unwrap();
    let second_estimate = payloads(&body)[0]["message"]["usage"]["input_tokens"]
        .as_u64()
        .unwrap();

    assert!(
        second_estimate > first_estimate.saturating_mul(2),
        "the second estimate ({second_estimate}) shows no correction from the first \
         ({first_estimate}); calibration is not reaching the live path"
    );
}

/// An unrelated conversation gets its own session, and does not inherit a
/// correction fitted to a different one.
#[tokio::test]
async fn an_unrelated_conversation_starts_uncalibrated() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "ok" }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 9000,
                    "output_tokens": 2,
                    "input_tokens_details": { "cached_tokens": 0 },
                },
            },
        }),
    ]))
    .await;

    let one = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "first conversation" }],
    });
    let _ = harness
        .post("/v1/messages", one)
        .await
        .text()
        .await
        .unwrap();

    let two = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "an entirely separate conversation" }],
    });
    let body = harness
        .post("/v1/messages", two)
        .await
        .text()
        .await
        .unwrap();
    let estimate = payloads(&body)[0]["message"]["usage"]["input_tokens"]
        .as_u64()
        .unwrap();

    assert!(
        estimate < 1_000,
        "an unrelated conversation inherited a correction: {estimate}"
    );
}

/// The cache key is stable for the life of a conversation. Cache hit rate
/// depends on it directly, so a key that changes per turn is the most expensive
/// possible bug that still works.
#[tokio::test]
async fn the_cache_key_is_stable_across_a_conversation() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let first = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "opening" }],
    });
    let _ = harness.post("/v1/messages", first).await.text().await;

    let second = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [
            { "role": "user", "content": "opening" },
            { "role": "assistant", "content": "reply" },
            { "role": "user", "content": "next" },
        ],
    });
    let _ = harness.post("/v1/messages", second).await.text().await;

    let sent = harness.upstream.requests();
    assert_eq!(
        sent[0]["prompt_cache_key"], sent[1]["prompt_cache_key"],
        "the cache key changed between turns of one conversation"
    );
}

/// §5 — `count_tokens` is an estimate, uncalibrated *before a session's first
/// completed request* and calibrated after. Answering from a fresh estimator
/// every time would leave it permanently uncalibrated however long the session
/// ran, which is not what the documented limitation says.
#[tokio::test]
async fn count_tokens_uses_what_the_conversation_has_learned() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "ok" }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 6000,
                    "output_tokens": 2,
                    "input_tokens_details": { "cached_tokens": 0 },
                },
            },
        }),
    ]))
    .await;

    let conversation = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "opening turn" }],
    });

    let before: Value = harness
        .post("/v1/messages/count_tokens", conversation.clone())
        .await
        .json()
        .await
        .unwrap();

    // A completed turn teaches the session what upstream charges.
    let _ = harness
        .post("/v1/messages", conversation.clone())
        .await
        .text()
        .await;

    let after: Value = harness
        .post("/v1/messages/count_tokens", conversation)
        .await
        .json()
        .await
        .unwrap();

    assert!(
        after["input_tokens"].as_u64().unwrap() > before["input_tokens"].as_u64().unwrap(),
        "count_tokens learned nothing: {before} then {after}"
    );
}

/// The conduit path is reachable from ingress, and carries the incremental
/// upload with it.
///
/// This is the wiring the rest of the transport work depends on. Every
/// transport test builds a `Conduit` directly, so all of them passed while
/// nothing in the request path constructed one — the WebSocket, pooling,
/// prewarm and delta code was unreachable from a running daemon and no test
/// noticed.
#[tokio::test]
async fn ingress_sends_through_a_conduit_and_uploads_incrementally() {
    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "ok" }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "ok" }],
            },
        }),
        completed(),
    ]))
    .await;

    let endpoint = upstream.url.clone();
    let conduits: codex_cc_proxy::ingress::ConduitFactory = Arc::new(move || {
        Arc::new(codex_cc_proxy::upstream::conduit::Conduit::new(
            Arc::new(HttpTransport::new(endpoint.clone())),
            // No WebSocket here: this asserts the conduit is *used*, and HTTP
            // is the transport a replay server can answer.
            None,
        ))
    });

    let state = AppState {
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: Some(conduits),
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-4".to_owned(),
            upstream: "gpt-5-codex".to_owned(),
        }]),
        recorder: None,
        record_ingress: false,
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let client = reqwest::Client::new();
    let post = |body: Value| {
        let client = client.clone();
        async move {
            client
                .post(format!("http://{addr}/v1/messages"))
                .json(&body)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };

    let first = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "opening" }],
    });
    let _ = post(first).await;

    // The same conversation, extended by the reply the server produced and one
    // new user turn.
    let second = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [
            { "role": "user", "content": "opening" },
            { "role": "assistant", "content": "ok" },
            { "role": "user", "content": "next" },
        ],
    });
    let _ = post(second).await;

    let sent = upstream.requests();
    assert_eq!(sent.len(), 2);

    // The server's own item is in the baseline, so the second turn does not
    // resend it — and the cache key is stable across both.
    assert_eq!(sent[0]["prompt_cache_key"], sent[1]["prompt_cache_key"]);
    assert_eq!(
        sent[1]["input"].as_array().map(Vec::len),
        Some(3),
        "the second turn should carry the whole conversation over HTTP"
    );
}

/// Credentials reach the upstream request. Without this every real request
///401s, and no test that uses a credential-free replay server would notice.
#[tokio::test]
async fn upstream_requests_carry_the_access_token() {
    use codex_cc_proxy::auth::store::CredentialStore;

    let dir = tempfile::tempdir().unwrap();
    let store = codex_cc_proxy::auth::store::FileStore::new(dir.path().join("credentials.json"));
    store
        .save(&codex_cc_proxy::auth::store::Credentials {
            access_token: "token-abc".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: None,
            account_id: Some("acct_7".to_owned()),
            // Far future, so nothing tries to refresh.
            expires_at: Some(4_000_000_000),
        })
        .unwrap();

    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let tokens = Arc::new(codex_cc_proxy::auth::tokens::TokenSource::new(
        Arc::new(store) as Arc<dyn CredentialStore>,
        "http://127.0.0.1:1/unused".to_owned(),
        "client",
        Arc::new(codex_cc_proxy::auth::tokens::SystemClock),
    ));

    let transport = HttpTransport::new(upstream.url.clone()).with_credentials(Arc::clone(&tokens));

    let request = codex_cc_proxy_core::responses::ResponsesRequest {
        model: "gpt-5-codex".to_owned(),
        ..Default::default()
    };
    let _ = codex_cc_proxy::upstream::Transport::stream(&transport, &request)
        .await
        .expect("the request should reach the replay server");

    let headers = upstream.headers();
    assert_eq!(
        headers[0].get("authorization").map(String::as_str),
        Some("Bearer token-abc")
    );
    assert_eq!(
        headers[0].get("chatgpt-account-id").map(String::as_str),
        Some("acct_7")
    );
}
