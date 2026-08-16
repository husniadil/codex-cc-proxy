//! `docs/proxy-behavior.md` §2 — Messages request to Responses request.

use crate::anthropic::Content;
use crate::anthropic::ContentBlock;
use crate::anthropic::Message;
use crate::anthropic::MessagesRequest;
use crate::anthropic::Role;
use crate::anthropic::Source;
use crate::anthropic::SystemPrompt;
use crate::anthropic::Tool;
use crate::anthropic::ToolChoice;
use crate::responses::CallOutput;
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
            .flat_map(translate_message)
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

/// One message can produce several items: calls and their outputs are items in
/// their own right, not parts of a message, so a turn mixing prose with a call
/// splits in order.
///
/// A message left with no content at all — an assistant turn that carried only
/// thinking — produces nothing. An empty message item is not the same thing as
/// no message, and the backend need not see one.
fn translate_message(message: &Message) -> Vec<InputItem> {
    let role = match message.role {
        Role::User => ItemRole::User,
        Role::Assistant => ItemRole::Assistant,
    };

    let mut items = Vec::new();
    let mut parts: Vec<ContentPart> = Vec::new();

    for block in message.content.blocks() {
        match block {
            ContentBlock::Text { text } => parts.push(match message.role {
                Role::User => ContentPart::InputText { text },
                Role::Assistant => ContentPart::OutputText { text },
            }),
            ContentBlock::ToolUse { id, name, input } => {
                flush(&mut items, role, &mut parts);
                items.push(InputItem::FunctionCall {
                    call_id: id,
                    name,
                    arguments: serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned()),
                });
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                flush(&mut items, role, &mut parts);
                let (output, trailing) = tool_result_output(content.as_ref());
                items.push(InputItem::FunctionCallOutput {
                    call_id: tool_use_id,
                    output,
                });
                items.extend(trailing);
            }
            // Attachments are dropped in assistant messages: assistant content
            // is `output_text` only.
            ContentBlock::Image { source } if message.role == Role::User => {
                if let Some(part) = image_part(&source) {
                    parts.push(part);
                }
            }
            ContentBlock::Document { source } if message.role == Role::User => {
                if let Some(part) = document_part(&source) {
                    parts.push(part);
                }
            }
            ContentBlock::Image { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::Thinking
            | ContentBlock::RedactedThinking
            | ContentBlock::ToolReference { .. }
            | ContentBlock::Unknown => {}
        }
    }

    flush(&mut items, role, &mut parts);
    items
}

fn flush(items: &mut Vec<InputItem>, role: ItemRole, parts: &mut Vec<ContentPart>) {
    if parts.is_empty() {
        return;
    }
    items.push(InputItem::Message {
        role,
        content: std::mem::take(parts),
    });
}

fn image_part(source: &Source) -> Option<ContentPart> {
    source
        .to_url()
        .map(|image_url| ContentPart::InputImage { image_url })
}

/// The filename is synthesized from the media type. Nothing in a `document`
/// block carries the original name, and the extension is the only part the
/// backend is likely to read.
fn document_part(source: &Source) -> Option<ContentPart> {
    let file_data = source.to_url()?;
    let extension = match source.media_type() {
        Some("application/pdf") | None => "pdf",
        Some("text/plain") => "txt",
        Some("text/markdown") => "md",
        Some(_) => "bin",
    };
    Some(ContentPart::InputFile {
        filename: format!("attachment.{extension}"),
        file_data,
    })
}

/// §2.3 — the output of a tool call, plus any items that must follow it.
///
/// Images travel inside the output itself, which keeps them attached to the
/// call that produced them. Documents have no representation there, so each is
/// re-emitted as a user message placed immediately after the output.
///
/// §2.5 — a tool-search result carries only `tool_reference` blocks. Reporting
/// the discovered names keeps the output non-empty and tells the model what it
/// may now call; an empty output leaves it unable to act on a search it just
/// ran.
fn tool_result_output(content: Option<&Content>) -> (CallOutput, Vec<InputItem>) {
    let Some(content) = content else {
        return (CallOutput::Text(String::new()), Vec::new());
    };

    let blocks = content.blocks();

    let discovered: Vec<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolReference { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    if !discovered.is_empty() {
        let output = json!({ "available_tools": discovered }).to_string();
        return (CallOutput::Text(output), Vec::new());
    }

    let mut parts = Vec::new();
    let mut documents = Vec::new();

    for block in &blocks {
        match block {
            ContentBlock::Text { text } => {
                parts.push(ContentPart::InputText { text: text.clone() })
            }
            ContentBlock::Image { source } => parts.extend(image_part(source)),
            ContentBlock::Document { source } => documents.extend(document_part(source)),
            _ => {}
        }
    }

    let trailing = if documents.is_empty() {
        Vec::new()
    } else {
        vec![InputItem::Message {
            role: ItemRole::User,
            content: documents,
        }]
    };

    (CallOutput::from_parts(parts), trailing)
}
