//! The Anthropic Messages API surface, as the client sends it.

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
    /// No Responses equivalent exists; dropped on the request path.
    Thinking,
    /// No Responses equivalent exists; dropped on the request path.
    RedactedThinking,
    #[serde(other)]
    Unknown,
}
