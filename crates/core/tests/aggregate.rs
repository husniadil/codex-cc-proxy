//! Folding the streamed frames back into the one body a non-streaming request
//! is answered with.
//!
//! The Messages endpoint answers a request that did not ask for a stream with a
//! single JSON message. This proxy has only an event stream to build it from,
//! so the body is that stream folded shut: blocks closed, deltas concatenated,
//! and the authoritative usage taken from `message_delta` rather than from the
//! estimate `message_start` carries.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use proxenos_core::anthropic::AssistantLiteral;
use proxenos_core::anthropic::BlockStart;
use proxenos_core::anthropic::Delta;
use proxenos_core::anthropic::ErrorBody;
use proxenos_core::anthropic::ErrorKind;
use proxenos_core::anthropic::Frame;
use proxenos_core::anthropic::MessageDelta;
use proxenos_core::anthropic::MessageLiteral;
use proxenos_core::anthropic::MessageStart;
use proxenos_core::anthropic::OutputBlock;
use proxenos_core::anthropic::StopReason;
use proxenos_core::anthropic::Usage;
use proxenos_core::anthropic::aggregate;
use serde_json::json;

fn start() -> Frame {
    Frame::MessageStart {
        message: MessageStart {
            id: "msg_1".to_owned(),
            kind: MessageLiteral::Message,
            role: AssistantLiteral::Assistant,
            model: "claude-sonnet-5".to_owned(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            // The estimate of §6.2, which the completed turn supersedes.
            usage: Usage {
                input_tokens: 999,
                ..Usage::default()
            },
        },
    }
}

fn stop(reason: StopReason, usage: Usage) -> Vec<Frame> {
    vec![
        Frame::MessageDelta {
            delta: MessageDelta {
                stop_reason: Some(reason),
                stop_sequence: None,
            },
            usage,
        },
        Frame::MessageStop,
    ]
}

/// Text arrives in fragments and leaves as one string.
#[test]
fn text_deltas_are_concatenated_into_one_block() {
    let mut frames = vec![
        start(),
        Frame::ContentBlockStart {
            index: 0,
            content_block: BlockStart::Text {
                text: String::new(),
            },
        },
        Frame::ContentBlockDelta {
            index: 0,
            delta: Delta::TextDelta {
                text: "7VQ".to_owned(),
            },
        },
        Frame::ContentBlockDelta {
            index: 0,
            delta: Delta::TextDelta {
                text: "K2M".to_owned(),
            },
        },
        Frame::ContentBlockStop { index: 0 },
    ];
    frames.extend(stop(
        StopReason::EndTurn,
        Usage {
            input_tokens: 22,
            output_tokens: 10,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 3,
        },
    ));

    let body = aggregate(&frames).expect("the frames describe a complete turn");

    assert_eq!(body.id, "msg_1");
    assert_eq!(body.model, "claude-sonnet-5");
    assert_eq!(body.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(
        body.content,
        vec![OutputBlock::Text {
            text: "7VQK2M".to_owned()
        }]
    );
    // The completed turn's figures, not `message_start`'s estimate.
    assert_eq!(body.usage.input_tokens, 22);
    assert_eq!(body.usage.output_tokens, 10);
    assert_eq!(body.usage.cache_read_input_tokens, 3);
}

/// A tool call's arguments stream as a JSON fragment and land as a value: a
/// client reading `input` gets an object, never the text that spelled it.
#[test]
fn tool_arguments_land_as_a_value_rather_than_a_fragment() {
    let mut frames = vec![
        start(),
        Frame::ContentBlockStart {
            index: 0,
            content_block: BlockStart::ToolUse {
                id: "toolu_1".to_owned(),
                name: "Read".to_owned(),
                input: json!({}),
            },
        },
        Frame::ContentBlockDelta {
            index: 0,
            delta: Delta::InputJsonDelta {
                partial_json: "{\"file_path\":".to_owned(),
            },
        },
        Frame::ContentBlockDelta {
            index: 0,
            delta: Delta::InputJsonDelta {
                partial_json: "\"/a\"}".to_owned(),
            },
        },
        Frame::ContentBlockStop { index: 0 },
    ];
    frames.extend(stop(StopReason::ToolUse, Usage::default()));

    let body = aggregate(&frames).expect("the frames describe a complete turn");

    assert_eq!(
        body.content,
        vec![OutputBlock::ToolUse {
            id: "toolu_1".to_owned(),
            name: "Read".to_owned(),
            input: json!({ "file_path": "/a" }),
        }]
    );
}

/// Arguments that do not parse cannot be answered with a plausible object.
/// Nothing here invents one: the turn fails in the error shape §1.1 defines.
#[test]
fn unparseable_tool_arguments_are_an_error_rather_than_an_invented_value() {
    let frames = vec![
        start(),
        Frame::ContentBlockStart {
            index: 0,
            content_block: BlockStart::ToolUse {
                id: "toolu_1".to_owned(),
                name: "Read".to_owned(),
                input: json!({}),
            },
        },
        Frame::ContentBlockDelta {
            index: 0,
            delta: Delta::InputJsonDelta {
                partial_json: "{\"file_path\":".to_owned(),
            },
        },
        Frame::ContentBlockStop { index: 0 },
    ];

    let error = aggregate(&frames).expect_err("truncated arguments cannot be aggregated");
    assert_eq!(error.kind, ErrorKind::ApiError);
}

/// An error frame is the answer, and it is the answer the caller gets.
#[test]
fn an_error_frame_becomes_the_error() {
    let frames = vec![
        start(),
        Frame::Error {
            error: ErrorBody {
                kind: ErrorKind::OverloadedError,
                message: "busy".to_owned(),
            },
        },
    ];

    let error = aggregate(&frames).expect_err("an error frame is not a message");
    assert_eq!(error.kind, ErrorKind::OverloadedError);
    assert_eq!(error.message, "busy");
}

/// A stream that never opened a message has nothing to fold.
#[test]
fn a_stream_without_a_message_start_is_an_error() {
    let error = aggregate(&[]).expect_err("no message was ever started");
    assert_eq!(error.kind, ErrorKind::ApiError);
}
