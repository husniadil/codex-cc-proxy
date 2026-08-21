//! `docs/proxy-behavior.md` §5 — response translation.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos_core::anthropic::Frame;
use proxenos_core::translate::ResponseOptions;
use proxenos_core::translate::ResponseTranslator;
use serde_json::Value;
use serde_json::json;

/// Run a stream of upstream events through the translator and return the frames
/// it emits, as the JSON the client would receive.
fn run(events: &[Value]) -> Vec<Value> {
    let mut translator = ResponseTranslator::new(ResponseOptions {
        message_id: "msg_test".to_owned(),
        model: "claude-sonnet-5".to_owned(),
        estimated_input_tokens: 100,
    });

    let mut frames: Vec<Frame> = Vec::new();
    for event in events {
        frames.extend(translator.push(&event.to_string()));
    }
    frames.extend(translator.finish());

    frames
        .iter()
        .map(|frame| serde_json::to_value(frame).unwrap())
        .collect()
}

/// The `type` of each frame, which is the sequence the client's state machine
/// follows.
fn shape(frames: &[Value]) -> Vec<&str> {
    frames
        .iter()
        .filter_map(|frame| frame["type"].as_str())
        .collect()
}

/// §5.1 — the smallest complete turn.
#[test]
fn a_text_response_produces_a_complete_frame_sequence() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "Hel" }),
        json!({ "type": "response.output_text.delta", "delta": "lo" }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 120,
                    "output_tokens": 5,
                    "input_tokens_details": { "cached_tokens": 20 },
                },
            },
        }),
    ]);

    assert_eq!(
        shape(&frames),
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    assert_eq!(
        frames[1]["content_block"],
        json!({ "type": "text", "text": "" })
    );
    assert_eq!(
        frames[2]["delta"],
        json!({ "type": "text_delta", "text": "Hel" })
    );
}

/// §6.2 — `message_start` carries an estimate, because the client renders that
/// value live and a zero collapses the context meter at the start of every
/// turn.
#[test]
fn message_start_carries_the_estimate_and_message_delta_replaces_it() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "hi" }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 120,
                    "output_tokens": 5,
                    "input_tokens_details": { "cached_tokens": 20 },
                },
            },
        }),
    ]);

    assert_eq!(frames[0]["message"]["usage"]["input_tokens"], json!(100));

    // §6.1 — upstream `input_tokens` includes cached tokens; Anthropic's
    // excludes them. 120 - 20 = 100, coincidentally the estimate.
    let usage = &frames
        .iter()
        .find(|frame| frame["type"] == "message_delta")
        .unwrap()["usage"];
    assert_eq!(usage["input_tokens"], json!(100));
    assert_eq!(usage["cache_read_input_tokens"], json!(20));
    assert_eq!(usage["output_tokens"], json!(5));
    // No upstream write event exists to report.
    assert_eq!(usage["cache_creation_input_tokens"], json!(0));
}

/// §6.1 — a cached count exceeding the input count clamps rather than
/// underflowing. An unsigned subtraction here would wrap to an enormous
/// number and the client would render a context meter far past full.
#[test]
fn a_cached_count_larger_than_the_input_count_clamps_to_zero() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "hi" }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 1,
                    "input_tokens_details": { "cached_tokens": 99 },
                },
            },
        }),
    ]);

    let message_delta = frames
        .iter()
        .find(|frame| frame["type"] == "message_delta")
        .unwrap();
    assert_eq!(message_delta["usage"]["input_tokens"], json!(0));
}

/// §5.1 — Anthropic permits one open content block at a time, so reasoning and
/// text cannot interleave. The reasoning block closes before the text opens.
#[test]
fn reasoning_and_text_occupy_separate_blocks() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.reasoning_summary_text.delta", "delta": "thinking" }),
        json!({ "type": "response.output_text.delta", "delta": "answer" }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    assert_eq!(
        shape(&frames),
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    assert_eq!(
        frames[1]["content_block"],
        json!({ "type": "thinking", "thinking": "" })
    );
    assert_eq!(
        frames[2]["delta"],
        json!({ "type": "thinking_delta", "thinking": "thinking" })
    );
    assert_eq!(frames[4]["content_block"]["type"], json!("text"));
    assert_eq!(frames[4]["index"], json!(1));
}

/// §5.1 — a `tool_use` block cannot open until the function name is known,
/// because an Anthropic client cannot patch a block header after it is emitted.
#[test]
fn a_tool_call_block_opens_only_once_its_name_is_known() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "call_id": "call_1", "name": "Read" },
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "call_1",
            "delta": "{\"path\":",
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "call_1",
            "delta": "\"/etc/hosts\"}",
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{\"path\":\"/etc/hosts\"}",
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    assert_eq!(
        shape(&frames),
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    assert_eq!(
        frames[1]["content_block"],
        json!({ "type": "tool_use", "id": "call_1", "name": "Read", "input": {} })
    );
    assert_eq!(
        frames[2]["delta"],
        json!({ "type": "input_json_delta", "partial_json": "{\"path\":" })
    );
}

/// §5.1 — a turn that produced a call stops with `tool_use`.
#[test]
fn a_turn_with_a_call_stops_for_tool_use() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{}",
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    let message_delta = frames
        .iter()
        .find(|f| f["type"] == "message_delta")
        .unwrap();
    assert_eq!(message_delta["delta"]["stop_reason"], json!("tool_use"));
}

/// The backend does not stream function arguments in every configuration. When
/// only the completed item arrives, its arguments are emitted as one delta —
/// otherwise the call reaches the client with no input at all and the tool runs
/// on nothing.
#[test]
fn arguments_arriving_only_on_the_done_item_are_still_emitted() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Grep",
                "arguments": "{\"pattern\":\"x\"}",
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    assert_eq!(
        frames[2]["delta"],
        json!({ "type": "input_json_delta", "partial_json": "{\"pattern\":\"x\"}" })
    );
}

/// Arguments already streamed are not repeated by the completed item. Emitting
/// both leaves the client parsing the same JSON twice, which fails.
#[test]
fn streamed_arguments_are_not_repeated_by_the_done_item() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "call_id": "call_1", "name": "Grep" },
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "call_1",
            "delta": "{\"pattern\":\"x\"}",
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Grep",
                "arguments": "{\"pattern\":\"x\"}",
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    let deltas = frames
        .iter()
        .filter(|frame| frame["type"] == "content_block_delta")
        .count();
    assert_eq!(deltas, 1);
}

/// §5.1 — an incomplete response stops for `max_tokens`.
#[test]
fn an_incomplete_response_stops_for_max_tokens() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "partial" }),
        json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_1",
                "incomplete_details": { "reason": "max_output_tokens" },
            },
        }),
    ]);

    let message_delta = frames
        .iter()
        .find(|f| f["type"] == "message_delta")
        .unwrap();
    assert_eq!(message_delta["delta"]["stop_reason"], json!("max_tokens"));
    assert_eq!(shape(&frames).last(), Some(&"message_stop"));
}

/// §5.0 — a payload that is not JSON is ignored rather than treated as an
/// error. Keep-alives and sentinels arrive this way.
#[test]
fn unparseable_payloads_are_ignored() {
    let mut translator = ResponseTranslator::new(ResponseOptions {
        message_id: "msg_test".to_owned(),
        model: "m".to_owned(),
        estimated_input_tokens: 1,
    });

    assert!(translator.push("[DONE]").is_empty());
    assert!(translator.push("not json at all").is_empty());
}

/// An event type this proxy does not model is ignored. A backend that adds an
/// event must not break a client that has not learned it yet.
#[test]
fn unknown_event_types_are_ignored() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.in_progress", "response": { "id": "resp_1" } }),
        json!({ "type": "response.content_part.added", "part": { "type": "output_text" } }),
        json!({ "type": "response.some.future.event", "delta": "x" }),
        json!({ "type": "response.output_text.delta", "delta": "hi" }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    assert_eq!(
        shape(&frames),
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

/// §5.4 — a stream that completes having produced no content still forms a
/// valid message. The client must not be left waiting on a turn that never
/// closes.
#[test]
fn an_empty_stream_still_closes_the_message() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    assert_eq!(
        shape(&frames),
        vec!["message_start", "message_delta", "message_stop"]
    );
}

/// A stream that ends without `response.completed` is still closed off. Leaving
/// the message open hangs the client on a turn the backend has abandoned.
#[test]
fn a_truncated_stream_is_closed_at_the_end() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "cut off" }),
    ]);

    assert_eq!(
        shape(&frames),
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

/// §5.1 — a failure becomes an `error` frame carrying a type the client's own
/// retry logic understands.
#[test]
fn a_failed_response_becomes_an_error_frame() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.failed",
            "response": {
                "id": "resp_1",
                "error": { "code": "rate_limit_exceeded", "message": "slow down" },
            },
        }),
    ]);

    let error = frames.iter().find(|f| f["type"] == "error").unwrap();
    assert_eq!(error["error"]["type"], json!("rate_limit_error"));
    assert_eq!(error["error"]["message"], json!("slow down"));
}

/// §5.1 — a capacity condition surfaces as retryable so the client backs off on
/// its own. The proxy does not build a second retry loop on top of that.
#[rstest::rstest]
#[case("server_is_overloaded", "overloaded_error")]
#[case("slow_down", "overloaded_error")]
#[case("rate_limit_exceeded", "rate_limit_error")]
#[case("context_length_exceeded", "invalid_request_error")]
#[case("insufficient_quota", "rate_limit_error")]
#[case("invalid_prompt", "invalid_request_error")]
#[case("something_unrecognized", "api_error")]
fn upstream_error_codes_map_to_the_client_vocabulary(#[case] code: &str, #[case] expected: &str) {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.failed",
            "response": { "id": "resp_1", "error": { "code": code, "message": "m" } },
        }),
    ]);

    let error = frames.iter().find(|f| f["type"] == "error").unwrap();
    assert_eq!(error["error"]["type"], json!(expected));
}

/// A top-level error frame carries the same weight as one nested in a failed
/// response. The WebSocket transport delivers errors this way.
#[test]
fn a_top_level_error_event_becomes_an_error_frame() {
    let frames = run(&[json!({
        "type": "error",
        "status": 429,
        "error": { "type": "usage_limit_reached", "message": "limit reached" },
    })]);

    let error = frames.iter().find(|f| f["type"] == "error").unwrap();
    assert_eq!(error["error"]["type"], json!("rate_limit_error"));
}

/// §5.2 — a server-side search is reconstructed into the structured blocks the
/// client reads. Passing the model's prose through as the result leaves the
/// client's extraction empty, so the structured form is required rather than
/// preferred.
#[test]
fn a_web_search_is_reconstructed_into_structured_blocks() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "status": "completed",
                "action": { "type": "search", "query": "rust sse parsing" },
            },
        }),
        json!({ "type": "response.output_text.delta", "delta": "Per the docs" }),
        json!({
            "type": "response.output_text.annotation.added",
            "annotation": {
                "type": "url_citation",
                "url": "https://example.invalid/a",
                "title": "A Guide",
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    let server_tool_use = frames
        .iter()
        .find(|frame| frame["content_block"]["type"] == "server_tool_use")
        .expect("a search should produce a server_tool_use block");
    assert_eq!(
        server_tool_use["content_block"]["name"],
        json!("web_search")
    );
    assert_eq!(
        server_tool_use["content_block"]["input"],
        json!({ "query": "rust sse parsing" })
    );

    let result = frames
        .iter()
        .find(|frame| frame["content_block"]["type"] == "web_search_tool_result")
        .expect("a search should produce a result block");
    assert_eq!(
        result["content_block"]["tool_use_id"],
        server_tool_use["content_block"]["id"]
    );
    assert_eq!(
        result["content_block"]["content"],
        json!([{
            "type": "web_search_result",
            "url": "https://example.invalid/a",
            "title": "A Guide",
        }])
    );
}

/// Citations also arrive attached to a completed message item rather than as
/// their own events. Both carry the same annotation shape.
#[test]
fn citations_on_a_completed_message_item_are_collected() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "action": { "type": "search", "query": "q" },
            },
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "answer",
                    "annotations": [
                        {
                            "type": "url_citation",
                            "url": "https://example.invalid/b",
                            "title": "B",
                        },
                    ],
                }],
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    let result = frames
        .iter()
        .find(|frame| frame["content_block"]["type"] == "web_search_tool_result")
        .unwrap();
    assert_eq!(
        result["content_block"]["content"][0]["url"],
        json!("https://example.invalid/b")
    );
}

/// The same source cited repeatedly is one result. A client rendering the list
/// verbatim would otherwise show the same page several times.
#[test]
fn repeated_citations_of_one_source_collapse() {
    let citation = json!({
        "type": "response.output_text.annotation.added",
        "annotation": {
            "type": "url_citation",
            "url": "https://example.invalid/a",
            "title": "A",
        },
    });

    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "action": { "type": "search", "query": "q" },
            },
        }),
        citation.clone(),
        citation,
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    let result = frames
        .iter()
        .find(|frame| frame["content_block"]["type"] == "web_search_tool_result")
        .unwrap();
    assert_eq!(
        result["content_block"]["content"].as_array().map(Vec::len),
        Some(1)
    );
}

/// A page the model opened is a source even when nothing cited it. Without this
/// a search that fetched pages but produced no citations reaches the client as
/// an empty result, which reads as "nothing found".
#[test]
fn opened_pages_count_as_sources_when_no_citation_arrives() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "action": { "type": "search", "query": "q" },
            },
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_2",
                "action": { "type": "open_page", "url": "https://example.invalid/opened" },
            },
        }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    let result = frames
        .iter()
        .find(|frame| frame["content_block"]["type"] == "web_search_tool_result")
        .unwrap();
    assert_eq!(
        result["content_block"]["content"][0]["url"],
        json!("https://example.invalid/opened")
    );
}

/// A turn with no search produces no search blocks at all.
#[test]
fn a_turn_without_a_search_produces_no_search_blocks() {
    let frames = run(&[
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        json!({ "type": "response.output_text.delta", "delta": "hi" }),
        json!({ "type": "response.completed", "response": { "id": "resp_1" } }),
    ]);

    assert!(
        !frames
            .iter()
            .any(|frame| frame["content_block"]["type"] == "server_tool_use")
    );
}

/// §3.3 — reasoning items are retained, not rendered.
///
/// Requests ask for encrypted reasoning, so responses carry items the model
/// expects to see again next turn. They cannot survive a round trip through the
/// client — thinking blocks are dropped on the request path, and the client
/// would not return encrypted upstream reasoning even if they were not — so the
/// session keeps them.
#[test]
fn reasoning_items_are_retained_for_the_next_turn() {
    let mut translator = ResponseTranslator::new(ResponseOptions {
        message_id: "msg".to_owned(),
        model: "m".to_owned(),
        estimated_input_tokens: 1,
    });

    let events = [
        json!({ "type": "response.created", "response": { "id": "r" } }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": "considering" }],
                "encrypted_content": "OPAQUE-BLOB",
            },
        }),
        json!({ "type": "response.output_text.delta", "delta": "answer" }),
        json!({ "type": "response.completed", "response": { "id": "r" } }),
    ];

    let mut frames = Vec::new();
    for event in &events {
        frames.extend(translator.push(&event.to_string()));
    }
    frames.extend(translator.finish());

    let retained = translator.retained_reasoning();
    assert_eq!(retained.len(), 1);

    let rendered = serde_json::to_value(&retained[0]).unwrap();
    assert_eq!(rendered["type"], json!("reasoning"));
    assert_eq!(rendered["encrypted_content"], json!("OPAQUE-BLOB"));

    // It is additive and upstream-only: nothing synthesized here reaches the
    // client as model output.
    let client = frames
        .iter()
        .map(|frame| serde_json::to_string(frame).unwrap_or_default())
        .collect::<String>();
    assert!(
        !client.contains("OPAQUE-BLOB"),
        "the encrypted blob was surfaced to the client"
    );
}

/// A turn with no reasoning retains nothing.
#[test]
fn a_turn_without_reasoning_retains_nothing() {
    let mut translator = ResponseTranslator::new(ResponseOptions {
        message_id: "msg".to_owned(),
        model: "m".to_owned(),
        estimated_input_tokens: 1,
    });

    translator.push(&json!({ "type": "response.created", "response": { "id": "r" } }).to_string());
    translator.push(&json!({ "type": "response.output_text.delta", "delta": "hi" }).to_string());

    assert!(translator.retained_reasoning().is_empty());
}
