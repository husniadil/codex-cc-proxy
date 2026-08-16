//! The Anthropic Messages SSE surface, as the client expects to receive it.

use serde::Deserialize;
use serde::Serialize;

/// One outbound SSE frame. The `type` is both the serialized discriminator and
/// the SSE `event:` name, which is why it is recoverable separately.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    MessageStart {
        message: MessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: BlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDelta,
        usage: Usage,
    },
    MessageStop,
    /// Keeps a slow stream alive through intermediaries that would otherwise
    /// time it out.
    Ping,
    Error {
        error: ErrorBody,
    },
}

impl Frame {
    /// The SSE event name for this frame.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error { .. } => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MessageStart {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: MessageLiteral,
    pub role: AssistantLiteral,
    pub model: String,
    pub content: Vec<()>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageLiteral {
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssistantLiteral {
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockStart {
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
    /// A tool the server ran on the model's behalf — web search (§5.2).
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

/// The client extracts `url` and `title` from these and nothing else (§5.2).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WebSearchResult {
    #[serde(rename = "type")]
    pub kind: WebSearchResultLiteral,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchResultLiteral {
    WebSearchResult,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    /// Tool arguments arrive as a JSON fragment, not a value.
    InputJsonDelta {
        partial_json: String,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MessageDelta {
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
}

/// Anthropic token accounting. `input_tokens` excludes cached tokens, which the
/// upstream figure includes — see `docs/proxy-behavior.md` §6.1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Always zero: no upstream write event exists to report (§6.1).
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ErrorBody {
    #[serde(rename = "type")]
    pub kind: ErrorKind,
    pub message: String,
}

/// The error vocabulary of `docs/api.md` §1.1. Each maps to a condition Claude
/// Code's own retry logic already understands, which is why the proxy does not
/// build a second retry loop on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidRequestError,
    AuthenticationError,
    NotFoundError,
    RateLimitError,
    ApiError,
    OverloadedError,
}

impl ErrorKind {
    /// Whether the client should retry on its own.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimitError | Self::OverloadedError)
    }
}
