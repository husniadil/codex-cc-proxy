//! The OpenAI Responses API surface, as the backend accepts it.

use serde::Deserialize;
use serde::Serialize;

/// An outbound Responses request.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    /// The system prompt. It never appears as an input item — the backend
    /// rejects system and developer roles inside `input`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Reasoning,
    /// Asks for reasoning that can be carried into the next turn (§3.3).
    pub include: Vec<String>,
    pub parallel_tool_calls: bool,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Reasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    /// Parse an inbound effort. `None` for anything unrecognized — the
    /// backend's own default is a better guess than one invented here.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Summary {
    Auto,
}

/// A tool as the backend declares it: internally tagged, so a function tool is
/// one flat object rather than a nested wrapper.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSpec {
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Always false. See the note in the request-translation tests.
        strict: bool,
        parameters: serde_json::Value,
    },
    /// The server-side search tool. Both access flags are stated rather than
    /// left to a default.
    WebSearch {
        external_web_access: bool,
        indexed_web_access: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(ToolChoiceMode),
    Function {
        r#type: FunctionLiteral,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoiceMode {
    Auto,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionLiteral {
    Function,
}

/// One item of conversation input.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: ItemRole,
        content: Vec<ContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        /// The call's input, serialized. The backend takes a string here, not
        /// an object.
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: CallOutput,
    },
    /// Reasoning the model produced, carried back so the next turn can see it.
    ///
    /// This cannot survive a round trip through the client: Anthropic
    /// `thinking` blocks are dropped on the request path, and the client would
    /// not return encrypted upstream reasoning even if they were not. The
    /// session retains these and re-injects them (§3.3).
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

/// A tool result. It collapses to a bare string when it is a single piece of
/// text, and stays an array otherwise — which is what lets an image reach the
/// model inside the output of the call that produced it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CallOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl CallOutput {
    /// Build the narrowest form the parts allow.
    pub fn from_parts(parts: Vec<ContentPart>) -> Self {
        match parts.as_slice() {
            [ContentPart::InputText { text }] => Self::Text(text.clone()),
            _ => Self::Parts(parts),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText {
        text: String,
    },
    InputImage {
        /// A data URL or an ordinary URL. Not an object.
        image_url: String,
    },
    /// A document. Unlike every other part here, this shape is not exercised by
    /// the upstream client — see `docs/proxy-behavior.md` §2.3 and roadmap §L.
    InputFile {
        filename: String,
        file_data: String,
    },
    OutputText {
        text: String,
    },
}
