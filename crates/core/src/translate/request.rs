//! `docs/proxy-behavior.md` §2 — Messages request to Responses request.

use crate::anthropic::ContentBlock;
use crate::anthropic::Message;
use crate::anthropic::MessagesRequest;
use crate::anthropic::Role;
use crate::anthropic::SystemPrompt;
use crate::anthropic::Tool;
use crate::anthropic::ToolChoice;
use crate::responses::ContentPart;
use crate::responses::FunctionLiteral;
use crate::responses::InputItem;
use crate::responses::ItemRole;
use crate::responses::ResponsesRequest;
use crate::responses::ToolChoice as ResponsesToolChoice;
use crate::responses::ToolChoiceMode;
use crate::responses::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;

/// Everything the translation needs that the inbound request does not carry.
#[derive(Debug, Clone, Default)]
pub struct TranslateOptions {
    /// The upstream model id the requested tier maps to. `None` means the
    /// inbound id passes through unchanged.
    pub model: Option<String>,
    /// Tools this session has seen discovered through a tool-search result.
    /// The client does not clear `defer_loading` once a tool is discovered, so
    /// this set is the only signal that a deferred tool is live (§2.5).
    pub discovered_tools: BTreeSet<String>,
}

/// Translate one Messages request into one Responses request.
pub fn translate_request(
    request: &MessagesRequest,
    options: &TranslateOptions,
) -> ResponsesRequest {
    let instructions = request.system.as_ref().map(SystemPrompt::to_text);

    ResponsesRequest {
        model: options
            .model
            .clone()
            .unwrap_or_else(|| request.model.clone()),
        instructions,
        input: request
            .messages
            .iter()
            .filter_map(translate_message)
            .collect(),
        tools: translate_tools(&request.tools, options),
        tool_choice: request.tool_choice.as_ref().map(translate_tool_choice),
    }
}

fn translate_tools(tools: &[Tool], options: &TranslateOptions) -> Vec<ToolSpec> {
    tools
        .iter()
        .filter(|tool| forward_tool(tool, options))
        .map(translate_tool)
        .collect()
}

/// §2.5 — a tool is forwarded unless it is still undiscovered.
///
/// The backend has its own deferred-loading mechanism, and `defer_loading`
/// could be passed through to it. It is not: discovery here is driven by the
/// client, and a second discovery path the client cannot observe would let the
/// model load a tool whose results never reach the client.
fn forward_tool(tool: &Tool, options: &TranslateOptions) -> bool {
    !tool.defer_loading || options.discovered_tools.contains(&tool.name)
}

fn translate_tool(tool: &Tool) -> ToolSpec {
    if tool.is_web_search() {
        return ToolSpec::WebSearch {
            external_web_access: true,
            indexed_web_access: true,
        };
    }

    ToolSpec::Function {
        name: tool.name.clone(),
        description: tool.description.clone(),
        strict: false,
        parameters: normalize_schema(tool.input_schema.clone()),
    }
}

/// An object schema with no `properties` gains an empty one.
fn normalize_schema(schema: Option<Value>) -> Value {
    let mut schema = schema.unwrap_or_else(|| json!({ "type": "object" }));
    if let Some(object) = schema.as_object_mut()
        && !object.contains_key("properties")
    {
        object.insert("properties".to_owned(), json!({}));
    }
    schema
}

fn translate_tool_choice(choice: &ToolChoice) -> ResponsesToolChoice {
    match choice {
        ToolChoice::Any => ResponsesToolChoice::Mode(ToolChoiceMode::Required),
        ToolChoice::Tool { name } => ResponsesToolChoice::Function {
            r#type: FunctionLiteral::Function,
            name: name.clone(),
        },
        // Including `none`: withholding the tools entirely is what `none`
        // would mean upstream, and the client sends it on turns where the tool
        // list still has to be visible.
        ToolChoice::Auto | ToolChoice::None | ToolChoice::Unknown => {
            ResponsesToolChoice::Mode(ToolChoiceMode::Auto)
        }
    }
}

/// `None` when translation leaves the message with no content at all — an
/// assistant turn that carried only thinking, for instance. An empty message
/// item is not the same thing as no message, and the backend need not see one.
fn translate_message(message: &Message) -> Option<InputItem> {
    let role = match message.role {
        Role::User => ItemRole::User,
        Role::Assistant => ItemRole::Assistant,
    };

    let content: Vec<ContentPart> = message
        .content
        .blocks()
        .iter()
        .filter_map(|block| translate_block(block, message.role))
        .collect();

    if content.is_empty() {
        return None;
    }

    Some(InputItem::Message { role, content })
}

fn translate_block(block: &ContentBlock, role: Role) -> Option<ContentPart> {
    match block {
        ContentBlock::Text { text } => Some(match role {
            Role::User => ContentPart::InputText { text: text.clone() },
            Role::Assistant => ContentPart::OutputText { text: text.clone() },
        }),
        ContentBlock::Thinking | ContentBlock::RedactedThinking | ContentBlock::Unknown => None,
    }
}
