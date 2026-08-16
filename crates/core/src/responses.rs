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
#[derive(Debug, Clone, Deserialize, Serialize)]
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
        output: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText { text: String },
    OutputText { text: String },
}
