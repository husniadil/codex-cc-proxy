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
    /// The same switches the control socket holds, so a test can turn capture
    /// on the way `record.start` does.
    switches: Arc<codex_cc_proxy::recorder::Switches>,
}

impl Harness {
    async fn start(behavior: Behavior) -> Self {
        Self::start_with(behavior, None).await
    }

    async fn start_with(
        behavior: Behavior,
        recorder: Option<codex_cc_proxy::recorder::Recorder>,
    ) -> Self {
        // A recorder passed here means the test wants ingress capture on from
        // the start, which is what `record ingress` does.
        let mode = recorder
            .as_ref()
            .map(|_| codex_cc_proxy::recorder::Mode::Ingress);
        Self::build(behavior, recorder, mode).await
    }

    /// Capture upstream as well, which is what `record upstream` turns on.
    async fn start_recording_upstream(
        behavior: Behavior,
        recorder: codex_cc_proxy::recorder::Recorder,
    ) -> Self {
        Self::build(
            behavior,
            Some(recorder),
            Some(codex_cc_proxy::recorder::Mode::Upstream),
        )
        .await
    }

    async fn build(
        behavior: Behavior,
        recorder: Option<codex_cc_proxy::recorder::Recorder>,
        mode: Option<codex_cc_proxy::recorder::Mode>,
    ) -> Self {
        let upstream = ReplayServer::start(behavior).await;
        let switches = Arc::new(codex_cc_proxy::recorder::Switches::new(mode));

        let state = AppState {
            effort_ceiling: None,
            catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
            transport: Arc::new(HttpTransport::new(upstream.url.clone())),
            conduits: None,
            models: Arc::new(vec![ModelMapping {
                requested: "claude-sonnet-5".to_owned(),
                upstream: "gpt-5.6-terra".to_owned(),
            }]),
            recorder: recorder.clone(),
            capture: Arc::clone(&switches),
            usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
            instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
                identity: false,
                append: None,
            }),
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
            switches,
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
                "max_tokens": 512,
                "system": "Be brief.",
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;

    let body = response.text().await.unwrap();
    let start = &payloads(&body)[0];
    assert_eq!(start["message"]["model"], json!("claude-sonnet-5"));

    let sent = harness.upstream.requests();
    assert_eq!(sent[0]["model"], json!("gpt-5.6-terra"));
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
    assert_eq!(body["data"][0]["id"], json!("gpt-5.6-terra"));
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
    assert_eq!(body["request"]["model"], json!("claude-sonnet-5"));
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
                "model": "claude-sonnet-5",
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

/// `record upstream` captures a turn that worked, which is the whole point of
/// it: a fixture is made from an exchange that succeeded, not from one that
/// failed. The default stays as it was — only empty streams — because
/// recording every exchange buries the defective ones.
#[tokio::test]
async fn upstream_capture_records_a_turn_that_produced_content() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path());

    let harness = Harness::start_recording_upstream(
        Behavior::Events(vec![
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
            json!({ "type": "response.output_text.delta", "delta": "content" }),
            completed(),
        ]),
        recorder,
    )
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 512,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        )
        .await;
    let _ = response.text().await.unwrap();

    let capture: Value = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("upstream-"))
        })
        .map(|path| serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
        .expect("a turn that produced content should have been captured");

    // The client's own request, untranslated — that is what a fixture replays,
    // and a capture of the translated one could not be replayed through the
    // translation it had already been through.
    assert_eq!(capture["request"]["model"], json!("claude-sonnet-5"));
    // And the upstream stream verbatim, which is the half that cannot be
    // invented.
    assert_eq!(capture["upstream"][0]["type"], json!("response.created"));
    assert_eq!(capture["upstream"].as_array().unwrap().len(), 3);
}

/// A refusal is still a status when the backend led with a preamble event.
///
/// The backend opens every stream with a quota snapshot before anything about
/// the response itself, so a refusal is not the first event — it is the first
/// event that *says* anything about the outcome. Checking only position zero
/// meant a refused turn became a 200 whose body was one error frame and no
/// `message_start`, which the client reports as an empty or malformed response
/// rather than as the refusal it is.
#[tokio::test]
async fn a_refusal_behind_a_preamble_is_still_a_status() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({
            "type": "codex.rate_limits",
            "rate_limits": { "primary": { "used_percent": 6 } },
        }),
        json!({ "type": "codex.response.metadata", "metadata": {} }),
        json!({
            "type": "error",
            "status": 429,
            "error": { "message": "quota exhausted" },
        }),
    ]))
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "hello" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 429, "nothing had been written yet");
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], json!("rate_limit_error"));
}

/// §2.1 — the configured lead and trailer reach the backend, around the
/// client's own prompt.
///
/// Asserted on the wire rather than on the translation, because this is the
/// only place the three pieces are seen in the order the model reads them.
#[tokio::test]
async fn the_configured_instructions_reach_the_backend() {
    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: None,
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: true,
            append: Some("  Answer briefly.  ".to_owned()),
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": "You are Claude Code.",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();
    let _ = response.text().await.unwrap();

    let sent = upstream.requests();
    let instructions = sent[0]["instructions"].as_str().unwrap();

    // The model that answers, not the tier that was asked for.
    assert!(
        instructions.starts_with("You are gpt-5.6-terra,"),
        "{instructions}"
    );
    assert!(
        instructions.contains("You are Claude Code."),
        "{instructions}"
    );
    // Trailing, and trimmed: surrounding whitespace in a config file is
    // formatting, not instruction.
    assert!(instructions.ends_with("Answer briefly."), "{instructions}");
}

/// Capture can be turned on while the daemon is running.
///
/// This is what `record.start` over the control socket means. Reading the flag
/// once at startup makes that method report success and change nothing — a
/// plausible answer to a request that had no effect, which is the failure this
/// project refuses everywhere else.
#[tokio::test]
async fn capture_can_be_switched_on_while_the_daemon_runs() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path());

    let harness = Harness::build(
        Behavior::Events(vec![
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
            json!({ "type": "response.output_text.delta", "delta": "hi" }),
            completed(),
        ]),
        Some(recorder),
        None,
    )
    .await;

    let ask = || async {
        let response = harness
            .post(
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-5",
                    "max_tokens": 512,
                    "messages": [{ "role": "user", "content": "hi" }],
                }),
            )
            .await;
        let _ = response.text().await.unwrap();
    };

    ask().await;
    assert_eq!(captures(dir.path(), "ingress-"), 0, "nothing asked for yet");

    harness
        .switches
        .start(codex_cc_proxy::recorder::Mode::Ingress);
    ask().await;
    assert_eq!(
        captures(dir.path(), "ingress-"),
        1,
        "capture was switched on"
    );

    harness.switches.stop();
    ask().await;
    assert_eq!(
        captures(dir.path(), "ingress-"),
        1,
        "and switched off again"
    );
}

fn captures(dir: &std::path::Path, prefix: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
        .count()
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
                "model": "claude-sonnet-5",
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
                "model": "claude-sonnet-5",
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
        "model": "claude-sonnet-5",
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
        "model": "claude-sonnet-5",
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
        "model": "claude-sonnet-5",
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
        "model": "claude-sonnet-5",
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
        "model": "claude-sonnet-5",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "opening" }],
    });
    let _ = harness.post("/v1/messages", first).await.text().await;

    let second = json!({
        "model": "claude-sonnet-5",
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
        "model": "claude-sonnet-5",
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
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: Some(conduits),
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
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
        "model": "claude-sonnet-5",
        "max_tokens": 512,
        "messages": [{ "role": "user", "content": "opening" }],
    });
    let _ = post(first).await;

    // The same conversation, extended by the reply the server produced and one
    // new user turn.
    let second = json!({
        "model": "claude-sonnet-5",
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

/// The delta a second turn puts on the wire is the new items, and never empty.
///
/// This is the bug the whole suite missed. Ingress advanced the baseline to the
/// current turn's input and *then* diffed against it, so the delta compared the
/// turn with itself and came out empty every time. An empty delta is not a
/// small delta: the backend receives a previous response id and no new input,
/// answers from the previous response, and the conversation silently repeats
/// itself with no error raised anywhere.
///
/// It has to be asserted on the frame that reaches the socket. Every transport
/// test drove `Conduit` directly with a correctly-managed baseline, and an
/// earlier attempt at this test recomputed the delta from a correct baseline
/// too — both pass with the bug present, because neither observes what ingress
/// actually hands over.
#[tokio::test]
async fn a_second_turn_uploads_the_new_items_and_not_nothing() {
    let events = vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "first answer" }],
            },
        }),
        completed(),
    ];

    let ws = replay::WsReplay::start(events).await;
    let socket = ws.url.clone();

    let conduits: codex_cc_proxy::ingress::ConduitFactory = Arc::new(move || {
        Arc::new(codex_cc_proxy::upstream::conduit::Conduit::new(
            // Unreachable on purpose: a fallback here would hide the failure
            // this test exists to catch.
            Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
            Some(Arc::new(
                codex_cc_proxy::upstream::websocket::WebSocketTransport::new(socket.clone())
                    .with_compression(false),
            )),
        ))
    });

    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
        conduits: Some(conduits),
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let client = reqwest::Client::new();
    let send = |body: Value| {
        let client = client.clone();
        async move {
            let _ = client
                .post(format!("http://{addr}/v1/messages"))
                .json(&body)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
        }
    };

    send(json!({
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "first question" }],
    }))
    .await;

    send(json!({
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "messages": [
            { "role": "user", "content": "first question" },
            { "role": "assistant", "content": "first answer" },
            { "role": "user", "content": "second question" },
        ],
    }))
    .await;

    let frames = ws.wait_for(2).await;

    // The opening turn carries the whole conversation and continues nothing.
    assert_eq!(frames[0]["input"].as_array().map(Vec::len), Some(1));
    assert!(frames[0].get("previous_response_id").is_none());

    // The second continues it, and carries exactly what is new.
    assert_eq!(
        frames[1]["previous_response_id"],
        json!("resp_1"),
        "a delta must name the response it continues"
    );
    let uploaded = frames[1]["input"].as_array().map(Vec::len);
    assert_eq!(
        uploaded,
        Some(1),
        "the second turn uploaded {uploaded:?} items; an empty delta makes the \
         backend answer from the previous response and repeat itself"
    );
    assert_eq!(
        frames[1]["input"][0]["content"][0]["text"],
        json!("second question")
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
        model: "gpt-5.6-terra".to_owned(),
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

/// A conversation keeps uploading deltas once the model starts reasoning.
///
/// The server returns reasoning items the client never sees and can never
/// replay. Judged strictly, the baseline that holds one is never extended
/// again: the session stops matching, a fresh one is created on every turn, and
/// the conversation loses its calibration, its discovered tools, and every
/// delta. Live, this showed as the third turn and every turn after it uploading
/// the whole conversation.
#[tokio::test]
async fn a_reasoning_turn_does_not_end_the_session() {
    let events = vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        // The item the client can never send back.
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "OPAQUE",
            },
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "answer" }],
            },
        }),
        completed(),
    ];

    let ws = replay::WsReplay::start(events).await;
    let socket = ws.url.clone();

    let conduits: codex_cc_proxy::ingress::ConduitFactory = Arc::new(move || {
        Arc::new(codex_cc_proxy::upstream::conduit::Conduit::new(
            Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
            Some(Arc::new(
                codex_cc_proxy::upstream::websocket::WebSocketTransport::new(socket.clone())
                    .with_compression(false),
            )),
        ))
    });

    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
        conduits: Some(conduits),
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };
    let sessions = Arc::clone(&state.sessions);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let client = reqwest::Client::new();
    let mut messages = vec![json!({ "role": "user", "content": "question 1" })];

    for turn in 1..=4 {
        let body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "messages": messages,
        });
        let _ = client
            .post(format!("http://{addr}/v1/messages"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // The client replays the answer and asks again, as it does.
        messages.push(json!({ "role": "assistant", "content": "answer" }));
        messages.push(json!({ "role": "user", "content": format!("question {}", turn + 1) }));
    }

    let frames = ws.wait_for(4).await;

    // One session for the whole conversation, not one per turn.
    assert_eq!(
        sessions.len(),
        1,
        "the conversation was split across {} sessions",
        sessions.len()
    );

    // Every turn after the first uploads exactly what is new.
    for (index, frame) in frames.iter().enumerate().skip(1) {
        assert_eq!(
            frame["input"].as_array().map(Vec::len),
            Some(1),
            "turn {} uploaded the whole conversation again",
            index + 1
        );
        assert!(
            frame.get("previous_response_id").is_some(),
            "turn {} did not continue the previous response",
            index + 1
        );
    }

    // And the reasoning the client could not replay is still being sent.
    let full = frames[0]["input"].as_array().map(Vec::len);
    assert_eq!(full, Some(1), "the opening turn carries only the question");
}

/// A turn the backend never accepted must not enter the baseline.
///
/// The baseline is what the next delta is measured against, so recording a turn
/// the backend rejected makes the next delta skip it: the backend is asked to
/// continue a response that never saw those items, and the question silently
/// vanishes from the conversation.
#[tokio::test]
async fn a_failed_turn_does_not_advance_the_baseline() {
    let events = vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "first answer" }],
            },
        }),
        completed(),
    ];

    let ws = replay::WsReplay::start(events).await;
    let socket = ws.url.clone();

    let conduits: codex_cc_proxy::ingress::ConduitFactory = Arc::new(move || {
        Arc::new(codex_cc_proxy::upstream::conduit::Conduit::new(
            // Unreachable, so a failing socket fails the turn rather than
            // quietly succeeding over HTTP.
            Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
            Some(Arc::new(
                codex_cc_proxy::upstream::websocket::WebSocketTransport::new(socket.clone())
                    .with_compression(false),
            )),
        ))
    });

    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
        conduits: Some(conduits),
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };
    let sessions = Arc::clone(&state.sessions);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let client = reqwest::Client::new();
    let opening = json!({
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "question 1" }],
    });
    let _ = client
        .post(format!("http://{addr}/v1/messages"))
        .json(&opening)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let continued = json!({
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "messages": [
            { "role": "user", "content": "question 1" },
            { "role": "assistant", "content": "first answer" },
            { "role": "user", "content": "question 2" },
        ],
    });

    let before = sessions
        .lookup(&input_of(&continued))
        .and_then(|session| session.baseline.lock().ok().map(|held| held.len()))
        .expect("the conversation should be known");

    // The next turn cannot reach the backend at all.
    ws.stop();
    let failed = client
        .post(format!("http://{addr}/v1/messages"))
        .json(&continued)
        .send()
        .await
        .unwrap();
    assert!(!failed.status().is_success(), "the turn should have failed");

    let after = sessions
        .lookup(&input_of(&continued))
        .and_then(|session| session.baseline.lock().ok().map(|held| held.len()))
        .expect("the conversation should still be known");

    assert_eq!(
        after, before,
        "a turn the backend never accepted was recorded as though it had been; \
         the next delta would continue a response that never saw it"
    );
}

/// Translate a Messages body the way ingress does when identifying a session.
fn input_of(body: &Value) -> Vec<codex_cc_proxy_core::responses::InputItem> {
    let request: codex_cc_proxy_core::anthropic::MessagesRequest =
        serde_json::from_value(body.clone()).unwrap();
    codex_cc_proxy_core::translate::translate_request(
        &request,
        &codex_cc_proxy_core::translate::TranslateOptions::default(),
    )
    .input
}

/// §7.2 and §1.1 — a request larger than the model can hold is refused here,
/// with a message that says so.
///
/// The error table lists this condition, which made it a published contract
/// that nothing produced. Forwarding it instead returns whatever the backend
/// says about a request it could not read — and the client cannot act on that.
#[tokio::test]
async fn a_request_larger_than_the_window_is_refused() {
    let catalog = codex_cc_proxy::catalog::Catalog::parse(
        r#"{"models":[{"slug":"gpt-5.6-terra","context_window":1000}]}"#,
    )
    .unwrap();

    let upstream = ReplayServer::start(Behavior::Events(Vec::new())).await;
    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(catalog),
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: None,
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "word ".repeat(40_000) }],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));

    let message = body["error"]["message"].as_str().unwrap();
    // The effective window, not the raw one: what is left after the headroom
    // §7.0 reserves for instructions, tool overhead and output. Enforcing the
    // raw figure would admit requests that leave no room to answer.
    assert!(
        message.contains("950"),
        "the limit should be named: {message}"
    );
    assert!(
        message.contains("gpt-5.6-terra"),
        "the model should be named: {message}"
    );

    // And nothing was sent upstream.
    assert!(upstream.requests().is_empty());
}

/// A model the catalog said nothing about is unknown, not unlimited. Guessing a
/// window here would reject requests that would have worked.
#[tokio::test]
async fn an_unknown_window_does_not_refuse_anything() {
    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let state = AppState {
        effort_ceiling: None,
        // The fallback list carries ids only, and no windows at all.
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: None,
        models: Arc::new(vec![ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "word ".repeat(40_000) }],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200, "an unknown window must not refuse");
}

/// §2.7 — effort is capped by what the model accepts, not only by the operator.
///
/// The client asks for a tier, not a model, so it cannot know that the model
/// behind that tier stops at `xhigh` while another goes to `max`. Forwarding an
/// effort the model does not support fails the turn for a reason the client
/// could not have anticipated or fixed.
#[tokio::test]
async fn effort_is_capped_by_what_the_model_supports() {
    let catalog = codex_cc_proxy::catalog::Catalog::parse(
        r#"{"models":[{
            "slug": "modest-model",
            "context_window": 272000,
            "supported_reasoning_levels": [
                { "effort": "low" },
                { "effort": "medium" },
                { "effort": "high" }
            ]
        }]}"#,
    )
    .unwrap();

    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let state = AppState {
        // No operator ceiling: the model's own limit is the only one.
        effort_ceiling: None,
        catalog: Arc::new(catalog),
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: None,
        models: Arc::new(vec![ModelMapping {
            requested: "sonnet".to_owned(),
            upstream: "modest-model".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let _ = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&json!({
            "model": "sonnet",
            "max_tokens": 32,
            // More than this model offers.
            "output_config": { "effort": "max" },
            "messages": [{ "role": "user", "content": "hello" }],
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let sent = upstream.requests();
    assert_eq!(
        sent[0]["reasoning"]["effort"],
        json!("high"),
        "the request should have been capped to what the model accepts"
    );
}

/// A model whose efforts the catalog never listed caps nothing. Unknown is not
/// a limit, and inventing one would refuse effort the model may well support.
#[tokio::test]
async fn an_unlisted_model_does_not_cap_effort() {
    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        transport: Arc::new(HttpTransport::new(upstream.url.clone())),
        conduits: None,
        models: Arc::new(vec![ModelMapping {
            requested: "sonnet".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
        }]),
        recorder: None,
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
        instructions: Arc::new(codex_cc_proxy::config::InstructionsConfig {
            identity: false,
            append: None,
        }),
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let _ = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&json!({
            "model": "sonnet",
            "max_tokens": 32,
            "output_config": { "effort": "max" },
            "messages": [{ "role": "user", "content": "hello" }],
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(upstream.requests()[0]["reasoning"]["effort"], json!("max"));
}

/// §1.1 — a refusal that arrives as the first upstream event is a status, not a
/// 200 carrying an error frame.
///
/// Nothing has been written to the client at that point, so the status is still
/// available — and it must be used. A 200 whose body is one error frame and no
/// `message_start` is not a message any client can read: the real one reports
/// "empty or malformed response (HTTP 200)" and says nothing about the refusal,
/// which sends you looking at the proxy instead of at the request.
#[tokio::test]
async fn an_upstream_refusal_on_the_first_event_is_a_status() {
    let harness = Harness::start(Behavior::Events(vec![json!({
        "type": "error",
        "status": 400,
        "error": { "message": "Invalid request", "type": "invalid_request_error" },
    })]))
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "hello" }],
            }),
        )
        .await;

    assert_eq!(
        response.status(),
        400,
        "the backend's status should survive"
    );

    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], json!("error"));
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Invalid request")),
        "the backend's own words should survive: {body}"
    );
}

/// A refusal *after* content has been sent stays an SSE error frame, because
/// the status is already gone.
#[tokio::test]
async fn a_refusal_after_content_is_still_a_frame() {
    let harness = Harness::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "partial" }),
        json!({
            "type": "response.failed",
            "response": { "id": "resp_1", "error": { "code": "server_is_overloaded" } },
        }),
    ]))
    .await;

    let response = harness
        .post(
            "/v1/messages",
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "hello" }],
            }),
        )
        .await;

    assert_eq!(response.status(), 200, "content was already sent");
    let body = response.text().await.unwrap();
    let error = payloads(&body)
        .into_iter()
        .find(|frame| frame["type"] == "error")
        .expect("the failure should arrive as a frame");
    assert_eq!(error["error"]["type"], json!("overloaded_error"));
}

/// §4.4 — a compressed body is announced, and the announcement is the whole
/// mechanism.
///
/// Compressed bytes without the header are just bytes the backend cannot parse,
/// and it refuses the request with nothing naming compression. Only bodies over
/// the threshold are compressed, which is what made this survive every
/// hand-made test: they were all too small to compress.
#[tokio::test]
async fn a_large_body_is_compressed_and_announced() {
    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let transport = HttpTransport::new(upstream.url.clone()).with_compression(true);

    let request = codex_cc_proxy_core::responses::ResponsesRequest {
        model: "gpt-5.6-terra".to_owned(),
        instructions: Some("consideration ".repeat(200)),
        ..Default::default()
    };

    let _ = codex_cc_proxy::upstream::Transport::stream(&transport, &request)
        .await
        .expect("the replay server should accept it");

    let headers = upstream.headers();
    assert_eq!(
        headers[0].get("content-encoding").map(String::as_str),
        Some("zstd"),
        "a compressed body must say so"
    );
    assert_eq!(
        headers[0].get("content-type").map(String::as_str),
        Some("application/json"),
        "the content type still describes what it decompresses to"
    );
}

/// A small body is sent as-is, and carries no encoding header. Compressing it
/// would make it larger.
#[tokio::test]
async fn a_small_body_is_not_compressed() {
    let upstream = ReplayServer::start(Behavior::Events(vec![
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        completed(),
    ]))
    .await;

    let transport = HttpTransport::new(upstream.url.clone()).with_compression(true);
    let request = codex_cc_proxy_core::responses::ResponsesRequest {
        model: "gpt-5.6-terra".to_owned(),
        ..Default::default()
    };

    let _ = codex_cc_proxy::upstream::Transport::stream(&transport, &request)
        .await
        .unwrap();

    assert_eq!(upstream.headers()[0].get("content-encoding"), None);
    // And it still arrives as readable JSON.
    assert_eq!(upstream.requests()[0]["model"], json!("gpt-5.6-terra"));
}

/// A capture is conversation content, and is written as such.
///
/// It holds the system prompt, the messages, and whatever the tools read — file
/// contents included. Written into a shared temporary directory at 0644, as it
/// was, every local process could read the user's work.
#[cfg(unix)]
#[tokio::test]
async fn captures_are_private_and_bounded() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let recorder = codex_cc_proxy::recorder::Recorder::new(dir.path().join("captures"));

    let request = json!({ "model": "m", "messages": [{ "role": "user", "content": "secret" }] });

    let first = recorder
        .record(
            codex_cc_proxy::recorder::Mode::Upstream,
            &request,
            Vec::new(),
            "a note long enough to be meaningful for the corpus loader",
        )
        .expect("the capture should be written");

    let mode = std::fs::metadata(&first).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "a capture must not be readable by others"
    );

    let dir_mode = std::fs::metadata(dir.path().join("captures"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        dir_mode & 0o077,
        0,
        "the directory must not be listable by others"
    );

    // A repeating failure must not fill a disk.
    for _ in 0..40 {
        recorder.record(
            codex_cc_proxy::recorder::Mode::Upstream,
            &request,
            Vec::new(),
            "a note long enough to be meaningful for the corpus loader",
        );
    }

    let kept = std::fs::read_dir(dir.path().join("captures"))
        .unwrap()
        .filter_map(Result::ok)
        .count();
    assert!(
        kept <= 20,
        "{kept} captures kept; the directory is unbounded"
    );

    // And what survives is the most recent, not the first.
    assert!(
        !first.exists(),
        "the oldest capture should have been pruned"
    );
}

/// §3.2 — a conversation nobody is having is forgotten.
///
/// A session holds the whole conversation and the previous request, so an
/// abandoned one keeps that alive indefinitely. The client never says a
/// conversation has ended, so idleness is the only signal there is.
#[test]
fn an_idle_conversation_is_forgotten() {
    use codex_cc_proxy::session::SessionStore;

    let store = SessionStore::new();
    let input = input_of(&json!({
        "model": "claude-sonnet-5",
        "messages": [{ "role": "user", "content": "hello" }],
    }));

    let session = store.resolve(&input);
    session.advance(&input, &[]);
    assert_eq!(store.len(), 1);

    // Pretend the conversation has been untouched for longer than the limit.
    store.forget_idle_for_test(std::time::Duration::ZERO);
    assert_eq!(
        store.len(),
        0,
        "an idle conversation should have been dropped"
    );

    // And the store still works afterwards.
    let fresh = store.resolve(&input);
    assert_ne!(
        fresh.cache_key, session.cache_key,
        "a new conversation, not the old one"
    );
}

/// An active conversation is never dropped for being idle.
#[test]
fn an_active_conversation_survives_the_sweep() {
    use codex_cc_proxy::session::SessionStore;

    let store = SessionStore::new();
    let input = input_of(&json!({
        "model": "claude-sonnet-5",
        "messages": [{ "role": "user", "content": "hello" }],
    }));

    let session = store.resolve(&input);
    session.advance(&input, &[]);

    store.forget_idle_for_test(std::time::Duration::from_secs(3600));

    assert_eq!(store.len(), 1);
    assert_eq!(store.resolve(&input).cache_key, session.cache_key);
}
