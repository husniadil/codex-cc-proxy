//! The Anthropic Messages API surface.

mod aggregate;
mod stream;

pub use aggregate::*;
pub use stream::*;

use serde::Deserialize;
use serde::Serialize;

/// An inbound `POST /v1/messages` body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Whether the caller asked to be answered with an event stream. Absent
    /// means it did not: the Messages endpoint's default is one JSON body, and
    /// `docs/api.md` §1 holds this proxy to that (§5.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

impl MessagesRequest {
    /// Whether this turn is answered with an event stream.
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// A tool declaration. Function tools carry `input_schema`; the server-side
/// search tool carries a `type` and no schema at all.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    /// Set by the client on tools it has not yet discovered. It is never
    /// cleared, so it means "was undiscovered when the session began", not "is
    /// undiscovered now" — see `docs/proxy-behavior.md` §2.5.
    #[serde(default)]
    pub defer_loading: bool,
}

impl Tool {
    /// Whether this declares the server-side search tool rather than a
    /// function.
    pub fn is_web_search(&self) -> bool {
        self.r#type
            .as_deref()
            .is_some_and(|kind| kind.starts_with("web_search"))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool {
        name: String,
    },
    #[serde(other)]
    Unknown,
}

/// `system` arrives either as a bare string or as a list of text blocks. The
/// block form is what the client sends whenever it attaches `cache_control` to
/// part of the prompt.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemBlock {
    #[serde(default)]
    pub text: String,
}

impl SystemPrompt {
    /// The prompt as a single string, which is the only form `instructions`
    /// takes.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|block| block.text.as_str())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

/// Where an attachment's bytes come from.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Base64 {
        media_type: String,
        data: String,
    },
    Url {
        url: String,
    },
    #[serde(other)]
    Unknown,
}

impl Source {
    /// The source as a single URL. Base64 payloads become data URLs; URL
    /// sources pass through and are not prefetched.
    pub fn to_url(&self) -> Option<String> {
        match self {
            Self::Base64 { media_type, data } => Some(format!("data:{media_type};base64,{data}")),
            Self::Url { url } => Some(url.clone()),
            Self::Unknown => None,
        }
    }

    /// The declared media type, where the source states one.
    pub fn media_type(&self) -> Option<&str> {
        match self {
            Self::Base64 { media_type, .. } => Some(media_type.as_str()),
            Self::Url { .. } | Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Anything else. The backend rejects system and developer roles inside
    /// `input`, so these fold into `instructions` — see
    /// `docs/proxy-behavior.md` §2.1.
    #[serde(other)]
    Other,
}

/// Message content is either a bare string or a list of blocks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl Content {
    /// The content as blocks, normalizing the bare-string form.
    pub fn blocks(&self) -> Vec<ContentBlock> {
        match self {
            Self::Text(text) => vec![ContentBlock::Text { text: text.clone() }],
            Self::Blocks(blocks) => blocks.clone(),
        }
    }
}

/// A content block. Unknown block types are captured rather than rejected: a
/// client newer than this proxy must not fail to translate, it must translate
/// what it can.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<Content>,
    },
    Image {
        source: Source,
    },
    Document {
        source: Source,
    },
    /// Names a tool that became available through a tool search. Appears only
    /// inside a `tool_result` — see `docs/proxy-behavior.md` §2.5.
    ToolReference {
        name: String,
    },
    /// No Responses equivalent exists; dropped on the request path.
    Thinking,
    /// No Responses equivalent exists; dropped on the request path.
    RedactedThinking,
    #[serde(other)]
    Unknown,
}
