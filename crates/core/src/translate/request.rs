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
use crate::responses::Effort;
use crate::responses::FunctionLiteral;
use crate::responses::InputItem;
use crate::responses::ItemRole;
use crate::responses::Reasoning;
use crate::responses::ResponsesRequest;
use crate::responses::Summary;
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
    /// Stable for the life of a conversation (§3.1). Cache hit rate depends on
    /// it directly.
    pub prompt_cache_key: Option<String>,
    /// §2.1 — operator text placed before the client's system prompt.
    ///
    /// The prompt the client sends is written for a different model and opens
    /// by saying so. Nothing else in the request tells the model what it
    /// actually is, and nothing in the client can be made to. This is where
    /// that is said, and it leads because an identity stated after a prompt
    /// that already asserted a different one reads as a correction rather than
    /// a fact.
    ///
    /// It must be stable for the life of a conversation. Text that varies per
    /// turn changes `instructions`, and a delta requires every non-input field
    /// to be unchanged (§4.3).
    pub instructions_lead: Option<String>,
    /// §2.1 — operator text placed after the client's system prompt.
    ///
    /// Last, and that is the point: an instruction meant to take precedence
    /// over the prompt above it has to come after it. Same stability
    /// requirement as the lead.
    pub instructions_trailer: Option<String>,
    /// §2.7 — the efforts this model accepts, from the catalog.
    ///
    /// Empty means unknown, and unknown means leave the request alone. A
    /// ceiling only bounds the top; it cannot keep a request for an effort
    /// *below* what a model offers off it, and no current model accepts `none`
    /// or `minimal`. Such a request fails for a reason the client could not
    /// have anticipated, having only asked for a tier.
    pub supported_efforts: Vec<Effort>,
    /// An operator-set ceiling on reasoning effort.
    ///
    /// The client does not know what a turn costs the operator, so it cannot
    /// choose this. A ceiling rather than a fixed value: a request asking for
    /// less than the ceiling still gets less, because lowering a request's own
    /// choice is not something an operator asked for by capping the maximum.
    pub effort_ceiling: Option<Effort>,
}

/// The one value asked for in `include`. Without it, responses carry no
/// reasoning that can be re-sent on the next turn (§3.3).
const INCLUDE_ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// Translate one Messages request into one Responses request.
pub fn translate_request(
    request: &MessagesRequest,
    options: &TranslateOptions,
) -> ResponsesRequest {
    let instructions = build_instructions(request, options);

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
        reasoning: Reasoning {
            effort: effort_for(request, options),
            summary: Some(Summary::Auto),
        },
        include: vec![INCLUDE_ENCRYPTED_REASONING.to_owned()],
        parallel_tool_calls: true,
        store: false,
        stream: true,
        prompt_cache_key: options.prompt_cache_key.clone(),
    }
}

/// §2.7 — the effort this request asks for, under whatever ceiling the operator
/// set.
///
/// With a ceiling and no request effort, the ceiling applies: an operator who
/// capped effort meant it for the traffic that expresses no preference too, and
/// that is most of it.
fn effort_for(request: &MessagesRequest, options: &TranslateOptions) -> Option<Effort> {
    let requested = request
        .output_config
        .as_ref()
        .and_then(|config| config.effort.as_deref())
        .and_then(Effort::parse);

    let chosen = match (requested, options.effort_ceiling) {
        (Some(requested), Some(ceiling)) => Some(requested.min(ceiling)),
        (Some(requested), None) => Some(requested),
        (None, ceiling) => ceiling,
    }?;

    Some(nearest_supported(chosen, &options.supported_efforts))
}

/// The closest effort the model will actually accept.
///
/// The highest supported level at or below what was asked for; failing that —
/// the request was below everything on offer — the lowest supported level,
/// because asking for less than a model can do is a request for its cheapest
/// setting, not for one it would refuse.
///
/// An empty list is "unknown", not "nothing supported", and leaves the choice
/// exactly as it was.
fn nearest_supported(chosen: Effort, supported: &[Effort]) -> Effort {
    if supported.is_empty() {
        return chosen;
    }
    supported
        .iter()
        .filter(|effort| **effort <= chosen)
        .max()
        .or_else(|| supported.iter().min())
        .copied()
        .unwrap_or(chosen)
}

/// §2.1 — the system prompt, and nothing else.
///
/// A message whose role is neither user nor assistant is *not* folded in here.
/// It cannot be an input item under its own role — the backend rejects system
/// and developer roles there — so it is carried as a `user` item instead.
/// Folding it here looked equivalent and was not: the client attaches per-turn
/// content that way, so `instructions` changed every turn, and a delta requires
/// every non-input field to be unchanged.
fn build_instructions(request: &MessagesRequest, options: &TranslateOptions) -> Option<String> {
    let system = request
        .system
        .as_ref()
        .map(SystemPrompt::to_text)
        .filter(|text| !text.is_empty());

    // Empty parts are dropped rather than joined, so an absent system prompt
    // leaves no blank run between the operator's own two pieces.
    let parts: Vec<&str> = [
        options.instructions_lead.as_deref(),
        system.as_deref(),
        options.instructions_trailer.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect();

    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// §2.5 — the tool names a request reports as discovered.
///
/// Discovery is observable exactly once, in the `tool_reference` blocks of a
/// search result. Nothing later in the conversation repeats it, and the client
/// never clears `defer_loading`, so a caller that does not record this loses
/// the only evidence that a tool became callable.
pub fn discovered_tool_names(request: &MessagesRequest) -> BTreeSet<String> {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.blocks())
        .flat_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => {
                content.map(|content| content.blocks()).unwrap_or_default()
            }
            _ => Vec::new(),
        })
        .filter_map(|block| match block {
            ContentBlock::ToolReference { name } => Some(name),
            _ => None,
        })
        .collect()
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
        // §2.1 — carried as a user item rather than folded into
        // `instructions`.
        //
        // The backend rejects system and developer roles inside `input`, but
        // nothing requires the *content* to leave the conversation. Folding it
        // into `instructions` looked equivalent and is not: the client sends
        // per-turn content this way, so instructions changed on every turn —
        // which invalidates the cached prefix and forces a full upload each
        // time, because a delta requires every non-input field to be unchanged.
        Role::Other => ItemRole::User,
    };

    let mut items = Vec::new();
    let mut parts: Vec<ContentPart> = Vec::new();

    for block in message.content.blocks() {
        match block {
            ContentBlock::Text { text } => parts.push(match role {
                ItemRole::User => ContentPart::InputText { text },
                ItemRole::Assistant => ContentPart::OutputText { text },
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
