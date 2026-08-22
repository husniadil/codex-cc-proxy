//! The single body a non-streaming request is answered with.
//!
//! This proxy has one thing to build a response out of: the frame sequence
//! §5.1 produces. A caller that did not ask for a stream is answered by folding
//! that sequence shut — blocks closed, deltas concatenated — rather than by
//! being handed an event stream it never agreed to parse.
//!
//! The fold is pure and total over what the frames carry. It invents nothing:
//! where a value cannot be reconstructed, the turn fails in the error shape of
//! `docs/api.md` §1.1 rather than being completed with something plausible.

use super::AssistantLiteral;
use super::BlockStart;
use super::Delta;
use super::ErrorBody;
use super::ErrorKind;
use super::Frame;
use super::MessageLiteral;
use super::StopReason;
use super::Usage;
use super::WebSearchResult;
use serde::Deserialize;
use serde::Serialize;

/// One finished block of a response.
///
/// Distinct from `OutputBlock`, which is what a client sends: this is what a
/// completed turn returns, and the two vocabularies do not coincide. The streaming surface splits each of these into
/// a start, its deltas, and a stop; here they are whole.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Vec<WebSearchResult>,
    },
}

/// The body of a non-streaming `POST /v1/messages` answer.
///
/// The field set is the one the real endpoint returns, measured in
/// `fixtures/surface/plain-generation.json`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MessageBody {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: MessageLiteral,
    pub role: AssistantLiteral,
    pub model: String,
    pub content: Vec<OutputBlock>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

/// A block that has been started and not yet closed.
enum Open {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        arguments: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        arguments: String,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Vec<WebSearchResult>,
    },
}

fn api_error(message: &str) -> ErrorBody {
    ErrorBody {
        kind: ErrorKind::ApiError,
        message: message.to_owned(),
    }
}

/// Arguments arrive as a fragment and have to leave as a value. An empty
/// fragment means the block carried its input whole.
fn arguments(input: serde_json::Value, arguments: &str) -> Result<serde_json::Value, ErrorBody> {
    if arguments.is_empty() {
        return Ok(input);
    }
    serde_json::from_str(arguments).map_err(|error| {
        api_error(&format!(
            "the tool call's arguments did not parse as JSON: {error}"
        ))
    })
}

impl Open {
    fn close(self) -> Result<OutputBlock, ErrorBody> {
        Ok(match self {
            Self::Text(text) => OutputBlock::Text { text },
            Self::Thinking(thinking) => OutputBlock::Thinking { thinking },
            Self::ToolUse {
                id,
                name,
                input,
                arguments: fragment,
            } => OutputBlock::ToolUse {
                id,
                name,
                input: arguments(input, &fragment)?,
            },
            Self::ServerToolUse {
                id,
                name,
                input,
                arguments: fragment,
            } => OutputBlock::ServerToolUse {
                id,
                name,
                input: arguments(input, &fragment)?,
            },
            Self::WebSearchToolResult {
                tool_use_id,
                content,
            } => OutputBlock::WebSearchToolResult {
                tool_use_id,
                content,
            },
        })
    }
}

impl From<BlockStart> for Open {
    fn from(start: BlockStart) -> Self {
        match start {
            BlockStart::Text { text } => Self::Text(text),
            BlockStart::Thinking { thinking } => Self::Thinking(thinking),
            BlockStart::ToolUse { id, name, input } => Self::ToolUse {
                id,
                name,
                input,
                arguments: String::new(),
            },
            BlockStart::ServerToolUse { id, name, input } => Self::ServerToolUse {
                id,
                name,
                input,
                arguments: String::new(),
            },
            BlockStart::WebSearchToolResult {
                tool_use_id,
                content,
            } => Self::WebSearchToolResult {
                tool_use_id,
                content,
            },
        }
    }
}

/// Fold a completed frame sequence into the body it describes.
///
/// The usage reported is the one `message_delta` carries, never the estimate in
/// `message_start`: §6.1 makes the completed turn's figures authoritative, and
/// a body that quoted the estimate would state a guess as a count.
pub fn aggregate(frames: &[Frame]) -> Result<MessageBody, ErrorBody> {
    let mut body: Option<MessageBody> = None;
    let mut open: Option<Open> = None;
    let mut content: Vec<OutputBlock> = Vec::new();

    for frame in frames {
        match frame {
            Frame::Error { error } => return Err(error.clone()),
            Frame::MessageStart { message } => {
                body = Some(MessageBody {
                    id: message.id.clone(),
                    kind: message.kind,
                    role: message.role,
                    model: message.model.clone(),
                    content: Vec::new(),
                    stop_reason: message.stop_reason,
                    stop_sequence: message.stop_sequence.clone(),
                    usage: message.usage,
                });
            }
            Frame::ContentBlockStart { content_block, .. } => {
                // Anthropic permits one open block at a time (§5.1), so a start
                // arriving over an unclosed block closes it rather than losing
                // it.
                if let Some(previous) = open.take() {
                    content.push(previous.close()?);
                }
                open = Some(Open::from(content_block.clone()));
            }
            Frame::ContentBlockDelta { delta, .. } => match (&mut open, delta) {
                (Some(Open::Text(text)), Delta::TextDelta { text: fragment }) => {
                    text.push_str(fragment);
                }
                (Some(Open::Thinking(thinking)), Delta::ThinkingDelta { thinking: fragment }) => {
                    thinking.push_str(fragment);
                }
                (
                    Some(Open::ToolUse { arguments, .. } | Open::ServerToolUse { arguments, .. }),
                    Delta::InputJsonDelta { partial_json },
                ) => {
                    arguments.push_str(partial_json);
                }
                // A signature delta has nowhere to go in the non-streaming
                // shape, and a delta against no open block is not content.
                _ => {}
            },
            Frame::ContentBlockStop { .. } => {
                if let Some(previous) = open.take() {
                    content.push(previous.close()?);
                }
            }
            Frame::MessageDelta { delta, usage } => {
                if let Some(body) = body.as_mut() {
                    body.stop_reason = delta.stop_reason;
                    body.stop_sequence = delta.stop_sequence.clone();
                    body.usage = *usage;
                }
            }
            Frame::MessageStop | Frame::Ping => {}
        }
    }

    // A stream that ended with a block still open is truncated, not empty. What
    // it did produce is kept rather than discarded.
    if let Some(previous) = open.take() {
        content.push(previous.close()?);
    }

    let mut body =
        body.ok_or_else(|| api_error("the response never began: no message was started"))?;
    body.content = content;
    Ok(body)
}
