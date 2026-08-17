//! The wire shape of the frames Claude Code receives.
//!
//! These assert the Anthropic Messages contract, which is not ours to change.
//! A field renamed here is a client that stops rendering.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy_core::anthropic::AssistantLiteral;
use codex_cc_proxy_core::anthropic::BlockStart;
use codex_cc_proxy_core::anthropic::Delta;
use codex_cc_proxy_core::anthropic::Frame;
use codex_cc_proxy_core::anthropic::MessageDelta;
use codex_cc_proxy_core::anthropic::MessageLiteral;
use codex_cc_proxy_core::anthropic::MessageStart;
use codex_cc_proxy_core::anthropic::StopReason;
use codex_cc_proxy_core::anthropic::Usage;
use codex_cc_proxy_core::sse::encode_frame;
use pretty_assertions::assert_eq;
use serde_json::json;

fn value(frame: &Frame) -> serde_json::Value {
    serde_json::to_value(frame).unwrap()
}

#[test]
fn message_start_carries_usage_the_client_renders_live() {
    let frame = Frame::MessageStart {
        message: MessageStart {
            id: "msg_01".to_owned(),
            kind: MessageLiteral::Message,
            role: AssistantLiteral::Assistant,
            model: "gpt-5.5".to_owned(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: Usage {
                input_tokens: 1200,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 300,
            },
        },
    };

    assert_eq!(
        value(&frame),
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "model": "gpt-5.5",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 1200,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 300,
                },
            },
        })
    );
}

#[test]
fn a_tool_use_block_opens_with_its_name() {
    let frame = Frame::ContentBlockStart {
        index: 1,
        content_block: BlockStart::ToolUse {
            id: "toolu_01".to_owned(),
            name: "Read".to_owned(),
            input: json!({}),
        },
    };

    assert_eq!(
        value(&frame),
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "tool_use",
                "id": "toolu_01",
                "name": "Read",
                "input": {},
            },
        })
    );
}

#[test]
fn tool_arguments_stream_as_json_fragments() {
    let frame = Frame::ContentBlockDelta {
        index: 1,
        delta: Delta::InputJsonDelta {
            partial_json: "{\"pa".to_owned(),
        },
    };

    assert_eq!(
        value(&frame),
        json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": "{\"pa" },
        })
    );
}

#[test]
fn message_delta_carries_the_stop_reason_and_final_usage() {
    let frame = Frame::MessageDelta {
        delta: MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            stop_sequence: None,
        },
        usage: Usage {
            input_tokens: 1200,
            output_tokens: 42,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 300,
        },
    };

    assert_eq!(
        value(&frame),
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use", "stop_sequence": null },
            "usage": {
                "input_tokens": 1200,
                "output_tokens": 42,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 300,
            },
        })
    );
}

#[test]
fn message_stop_carries_nothing_but_its_type() {
    assert_eq!(
        value(&Frame::MessageStop),
        json!({ "type": "message_stop" })
    );
}

/// Every frame is written with its `event:` name as well as its payload. A
/// client dispatching on the event name sees nothing without it.
#[test]
fn a_frame_encodes_with_its_event_name() {
    let encoded = encode_frame(&Frame::MessageStop);
    assert_eq!(
        encoded,
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
}

/// A payload never contains a bare newline — JSON encoding escapes them — so a
/// frame is always one `data:` line and round-trips through any decoder.
#[test]
fn an_encoded_frame_is_a_single_data_line() {
    let frame = Frame::ContentBlockDelta {
        index: 0,
        delta: Delta::TextDelta {
            text: "first\nsecond".to_owned(),
        },
    };

    let encoded = encode_frame(&frame);
    assert_eq!(encoded.matches("data: ").count(), 1);
    assert!(encoded.ends_with("\n\n"));
}
