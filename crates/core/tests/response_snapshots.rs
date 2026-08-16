//! Full frame sequences for each kind of stream, as the client receives them.
//!
//! The assertions elsewhere check one rule at a time. These check the whole
//! emission, so a change that fixes one rule and disturbs another shows up as a
//! diff rather than passing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy_core::translate::ResponseOptions;
use codex_cc_proxy_core::translate::ResponseTranslator;
use serde_json::Value;
use serde_json::json;

fn frames(events: &[Value]) -> String {
    let mut translator = ResponseTranslator::new(ResponseOptions {
        message_id: "msg_snapshot".to_owned(),
        model: "claude-sonnet-4".to_owned(),
        estimated_input_tokens: 100,
    });

    let mut out = Vec::new();
    for event in events {
        out.extend(translator.push(&event.to_string()));
    }
    out.extend(translator.finish());

    out.iter()
        .map(|frame| serde_json::to_string(frame).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn completed() -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "usage": {
                "input_tokens": 500,
                "output_tokens": 12,
                "input_tokens_details": { "cached_tokens": 400 },
                "output_tokens_details": { "reasoning_tokens": 8 },
            },
        },
    })
}

fn created() -> Value {
    json!({ "type": "response.created", "response": { "id": "resp_1" } })
}

#[test]
fn text_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({ "type": "response.output_text.delta", "delta": "The answer " }),
        json!({ "type": "response.output_text.delta", "delta": "is 42." }),
        completed(),
    ]));
}

#[test]
fn reasoning_then_text_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({ "type": "response.reasoning_summary_text.delta", "delta": "Considering." }),
        json!({ "type": "response.output_text.delta", "delta": "Done." }),
        completed(),
    ]));
}

#[test]
fn tool_call_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "call_id": "call_1", "name": "Read" },
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "call_1",
            "delta": "{\"path\":\"/tmp/a\"}",
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{\"path\":\"/tmp/a\"}",
            },
        }),
        completed(),
    ]));
}

/// Two calls in one turn. The client runs them together, so both must arrive as
/// separate blocks with distinct indices.
#[test]
fn parallel_tool_calls_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{\"path\":\"/tmp/a\"}",
            },
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_2",
                "name": "Grep",
                "arguments": "{\"pattern\":\"x\"}",
            },
        }),
        completed(),
    ]));
}

#[test]
fn web_search_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "status": "completed",
                "action": { "type": "search", "query": "sse framing" },
            },
        }),
        json!({ "type": "response.output_text.delta", "delta": "Sources say so." }),
        json!({
            "type": "response.output_text.annotation.added",
            "annotation": {
                "type": "url_citation",
                "url": "https://example.invalid/spec",
                "title": "The Specification",
            },
        }),
        completed(),
    ]));
}

#[test]
fn incomplete_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({ "type": "response.output_text.delta", "delta": "It was cut " }),
        json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_1",
                "incomplete_details": { "reason": "max_output_tokens" },
            },
        }),
    ]));
}

#[test]
fn error_stream() {
    insta::assert_snapshot!(frames(&[
        created(),
        json!({
            "type": "response.failed",
            "response": {
                "id": "resp_1",
                "error": {
                    "code": "server_is_overloaded",
                    "message": "The server is overloaded.",
                },
            },
        }),
    ]));
}
