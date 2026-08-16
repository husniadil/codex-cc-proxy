//! `docs/proxy-behavior.md` §5 — Responses events to Anthropic frames.
//!
//! Upstream events are read permissively: a flat struct of optional fields
//! dispatched on the `type` string, with anything unrecognized ignored. A
//! backend that adds an event must not break a client that has not learned it
//! yet, and a strict enum would turn every addition into a failed turn.

use crate::anthropic::AssistantLiteral;
use crate::anthropic::BlockStart;
use crate::anthropic::Delta;
use crate::anthropic::ErrorBody;
use crate::anthropic::ErrorKind;
use crate::anthropic::Frame;
use crate::anthropic::MessageDelta;
use crate::anthropic::MessageLiteral;
use crate::anthropic::MessageStart;
use crate::anthropic::StopReason;
use crate::anthropic::Usage;
use crate::anthropic::WebSearchResult;
use crate::anthropic::WebSearchResultLiteral;
use serde_json::Value;

/// What the translator needs that the stream does not carry.
#[derive(Debug, Clone)]
pub struct ResponseOptions {
    /// The id given to this turn's message.
    pub message_id: String,
    /// The model id to report back. This is the tier's name as the client knows
    /// it, not the upstream id it was mapped to — the client matches it against
    /// what it asked for.
    pub model: String,
    /// The estimate carried in `message_start` (§6.2). The client renders this
    /// live, so a zero collapses the context meter at the start of every turn.
    pub estimated_input_tokens: u64,
}

/// The block currently open. Anthropic permits exactly one at a time.
#[derive(Debug, PartialEq, Eq)]
enum OpenBlock {
    None,
    Text,
    Thinking,
    /// A call whose header has been emitted. Holds the upstream item id so
    /// argument deltas can be matched to it.
    ToolUse {
        item_id: String,
        /// Whether any argument fragment has been forwarded. The completed item
        /// repeats the full arguments, and emitting both leaves the client
        /// parsing the same JSON twice.
        streamed_arguments: bool,
    },
}

/// Translates one upstream stream into one Anthropic message.
#[derive(Debug)]
pub struct ResponseTranslator {
    options: ResponseOptions,
    started: bool,
    finished: bool,
    open: OpenBlock,
    next_index: usize,
    saw_tool_call: bool,
    stop_reason: StopReason,
    usage: Usage,
    /// §5.2 — the searches this turn ran, and the sources it drew on. Both are
    /// emitted together when the message closes, because the citations that
    /// name the sources arrive while the answer is being written, after the
    /// search itself has completed.
    searches: Vec<Search>,
    sources: Vec<WebSearchResult>,
    /// §3.3 — reasoning items the server returned, for the session to re-inject
    /// on the next request.
    reasoning: Vec<crate::responses::InputItem>,
}

#[derive(Debug)]
struct Search {
    id: String,
    query: String,
}

impl ResponseTranslator {
    pub fn new(options: ResponseOptions) -> Self {
        Self {
            usage: Usage {
                input_tokens: options.estimated_input_tokens,
                ..Usage::default()
            },
            options,
            started: false,
            finished: false,
            open: OpenBlock::None,
            next_index: 0,
            saw_tool_call: false,
            stop_reason: StopReason::EndTurn,
            searches: Vec::new(),
            sources: Vec::new(),
            reasoning: Vec::new(),
        }
    }

    /// Consume one event payload, yielding the frames it produces.
    ///
    /// A payload that is not JSON is ignored rather than treated as an error:
    /// keep-alives and stream sentinels arrive that way (§5.0).
    pub fn push(&mut self, payload: &str) -> Vec<Frame> {
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };

        let mut frames = Vec::new();
        self.handle(kind, &event, &mut frames);
        frames
    }

    /// §3.3 — the reasoning items this turn produced.
    ///
    /// They cannot survive a round trip through the client, so a caller that
    /// does not retain them begins every turn with the model's prior reasoning
    /// discarded.
    pub fn retained_reasoning(&self) -> &[crate::responses::InputItem] {
        &self.reasoning
    }

    /// Close off a stream that ended without completing.
    ///
    /// Leaving the message open hangs the client on a turn the backend has
    /// abandoned, which is indistinguishable from a model still thinking.
    pub fn finish(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        if self.started && !self.finished {
            self.close_message(&mut frames);
        }
        frames
    }

    fn handle(&mut self, kind: &str, event: &Value, frames: &mut Vec<Frame>) {
        match kind {
            "response.created" => self.start_message(frames),
            "response.output_text.delta" => {
                if let Some(text) = text_field(event, "delta") {
                    self.open_block(BlockKind::Text, frames);
                    self.push_delta(Delta::TextDelta { text }, frames);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(thinking) = text_field(event, "delta") {
                    self.open_block(BlockKind::Thinking, frames);
                    self.push_delta(Delta::ThinkingDelta { thinking }, frames);
                }
            }
            "response.output_text.annotation.added" => {
                self.collect_citation(event.get("annotation"));
            }
            "response.output_item.added" => self.item_added(event, frames),
            "response.function_call_arguments.delta" => self.arguments_delta(event, frames),
            "response.output_item.done" => self.item_done(event, frames),
            "response.completed" => {
                self.start_message(frames);
                if let Some(usage) = event.pointer("/response/usage") {
                    self.usage = translate_usage(usage);
                }
                self.close_message(frames);
            }
            "response.incomplete" => {
                self.start_message(frames);
                self.stop_reason = StopReason::MaxTokens;
                self.close_message(frames);
            }
            "response.failed" => {
                let error = event.pointer("/response/error");
                self.emit_error(error, frames);
            }
            "error" => {
                self.emit_error(event.get("error"), frames);
            }
            // Everything else, including events this proxy has no use for and
            // events it has never seen.
            _ => {}
        }
    }

    fn start_message(&mut self, frames: &mut Vec<Frame>) {
        if self.started {
            return;
        }
        self.started = true;
        frames.push(Frame::MessageStart {
            message: MessageStart {
                id: self.options.message_id.clone(),
                kind: MessageLiteral::Message,
                role: AssistantLiteral::Assistant,
                model: self.options.model.clone(),
                content: Vec::new(),
                stop_reason: None,
                stop_sequence: None,
                usage: self.usage,
            },
        });
    }

    fn close_message(&mut self, frames: &mut Vec<Frame>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.close_block(frames);
        self.emit_searches(frames);

        if self.saw_tool_call && self.stop_reason == StopReason::EndTurn {
            self.stop_reason = StopReason::ToolUse;
        }

        frames.push(Frame::MessageDelta {
            delta: MessageDelta {
                stop_reason: Some(self.stop_reason),
                stop_sequence: None,
            },
            // §6.2 — cumulative final usage, which replaces the estimate rather
            // than adding to it.
            usage: self.usage,
        });
        frames.push(Frame::MessageStop);
    }

    fn item_added(&mut self, event: &Value, frames: &mut Vec<Frame>) {
        let Some(item) = event.get("item") else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let Some(name) = text_field(item, "name") else {
            // §5.1 — without a name there is no header to emit, and an
            // Anthropic client cannot patch one after the fact. The block waits
            // for the completed item.
            return;
        };

        self.start_message(frames);
        self.open_tool_use(item, name, frames);
    }

    fn arguments_delta(&mut self, event: &Value, frames: &mut Vec<Frame>) {
        let Some(partial_json) = text_field(event, "delta") else {
            return;
        };
        let OpenBlock::ToolUse { item_id, .. } = &self.open else {
            return;
        };

        // Only forward fragments belonging to the block that is open.
        let target = text_field(event, "item_id").or_else(|| text_field(event, "call_id"));
        if target.is_some_and(|target| &target != item_id) {
            return;
        }

        if let OpenBlock::ToolUse {
            streamed_arguments, ..
        } = &mut self.open
        {
            *streamed_arguments = true;
        }
        self.push_delta(Delta::InputJsonDelta { partial_json }, frames);
    }

    fn item_done(&mut self, event: &Value, frames: &mut Vec<Frame>) {
        let Some(item) = event.get("item") else {
            return;
        };

        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {}
            Some("web_search_call") => {
                self.search_done(item);
                return;
            }
            Some("message") => {
                self.collect_message_citations(item);
                return;
            }
            Some("reasoning") => {
                // §3.3 — retained, not rendered. Nothing synthesized here is
                // ever surfaced to the client as model output; it exists only
                // to be re-sent upstream on the next turn.
                if let Ok(retained) = serde_json::from_value(item.clone()) {
                    self.reasoning.push(retained);
                }
                return;
            }
            _ => return,
        }
        let Some(name) = text_field(item, "name") else {
            return;
        };

        self.start_message(frames);

        let call_id = call_id_of(item);
        let already_open = matches!(
            &self.open,
            OpenBlock::ToolUse { item_id, .. } if item_id == &call_id
        );

        if !already_open {
            self.open_tool_use(item, name, frames);
        }

        // The completed item repeats the full arguments. Forward them only if
        // nothing was streamed, otherwise the client parses the same JSON twice.
        let streamed = matches!(
            &self.open,
            OpenBlock::ToolUse {
                streamed_arguments: true,
                ..
            }
        );
        if !streamed
            && let Some(arguments) = text_field(item, "arguments")
            && !arguments.is_empty()
        {
            self.push_delta(
                Delta::InputJsonDelta {
                    partial_json: arguments,
                },
                frames,
            );
        }

        self.close_block(frames);
    }

    fn open_tool_use(&mut self, item: &Value, name: String, frames: &mut Vec<Frame>) {
        self.close_block(frames);
        let call_id = call_id_of(item);
        self.saw_tool_call = true;

        frames.push(Frame::ContentBlockStart {
            index: self.next_index,
            content_block: BlockStart::ToolUse {
                id: call_id.clone(),
                name,
                input: serde_json::json!({}),
            },
        });
        self.open = OpenBlock::ToolUse {
            item_id: call_id,
            streamed_arguments: false,
        };
    }

    /// §5.2 — a completed search. A `search` action names the query; an
    /// `open_page` action names a source the model actually read, which is the
    /// only evidence of a source when no citation follows.
    fn search_done(&mut self, item: &Value) {
        let action = item.get("action");
        let kind = action
            .and_then(|action| action.get("type"))
            .and_then(Value::as_str);

        match kind {
            Some("open_page") | Some("find_in_page") => {
                if let Some(url) = action.and_then(|action| text_field(action, "url")) {
                    self.add_source(url.clone(), url);
                }
            }
            _ => {
                let query = action
                    .and_then(|action| text_field(action, "query"))
                    .or_else(|| {
                        action
                            .and_then(|action| action.get("queries"))
                            .and_then(Value::as_array)
                            .and_then(|queries| queries.first())
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                self.searches.push(Search {
                    id: call_id_of(item),
                    query,
                });
            }
        }
    }

    fn collect_message_citations(&mut self, item: &Value) {
        let parts = item
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        for annotation in parts
            .iter()
            .filter_map(|part| part.get("annotations"))
            .filter_map(Value::as_array)
            .flatten()
        {
            self.collect_citation(Some(annotation));
        }
    }

    fn collect_citation(&mut self, annotation: Option<&Value>) {
        let Some(annotation) = annotation else { return };
        if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
            return;
        }
        let Some(url) = text_field(annotation, "url") else {
            return;
        };
        let title = text_field(annotation, "title").unwrap_or_else(|| url.clone());
        self.add_source(url, title);
    }

    /// The same source cited repeatedly is one result. A client renders this
    /// list verbatim, so duplicates show as the same page several times.
    fn add_source(&mut self, url: String, title: String) {
        if self.sources.iter().any(|source| source.url == url) {
            return;
        }
        self.sources.push(WebSearchResult {
            kind: WebSearchResultLiteral::WebSearchResult,
            url,
            title,
        });
    }

    /// §5.2 — emit each search and the sources it produced.
    ///
    /// These close the message rather than appearing where the search ran,
    /// because the citations naming the sources arrive while the answer is
    /// written — after the search itself has completed.
    fn emit_searches(&mut self, frames: &mut Vec<Frame>) {
        if self.searches.is_empty() {
            return;
        }
        self.close_block(frames);

        let sources = std::mem::take(&mut self.sources);
        for search in std::mem::take(&mut self.searches) {
            frames.push(Frame::ContentBlockStart {
                index: self.next_index,
                content_block: BlockStart::ServerToolUse {
                    id: search.id.clone(),
                    name: "web_search".to_owned(),
                    input: serde_json::json!({ "query": search.query }),
                },
            });
            frames.push(Frame::ContentBlockStop {
                index: self.next_index,
            });
            self.next_index = self.next_index.saturating_add(1);

            frames.push(Frame::ContentBlockStart {
                index: self.next_index,
                content_block: BlockStart::WebSearchToolResult {
                    tool_use_id: search.id,
                    content: sources.clone(),
                },
            });
            frames.push(Frame::ContentBlockStop {
                index: self.next_index,
            });
            self.next_index = self.next_index.saturating_add(1);
        }
    }

    fn open_block(&mut self, kind: BlockKind, frames: &mut Vec<Frame>) {
        let already_open = matches!(
            (&self.open, kind),
            (OpenBlock::Text, BlockKind::Text) | (OpenBlock::Thinking, BlockKind::Thinking)
        );
        if already_open {
            return;
        }

        self.start_message(frames);
        self.close_block(frames);

        let content_block = match kind {
            BlockKind::Text => BlockStart::Text {
                text: String::new(),
            },
            BlockKind::Thinking => BlockStart::Thinking {
                thinking: String::new(),
            },
        };
        frames.push(Frame::ContentBlockStart {
            index: self.next_index,
            content_block,
        });
        self.open = match kind {
            BlockKind::Text => OpenBlock::Text,
            BlockKind::Thinking => OpenBlock::Thinking,
        };
    }

    fn close_block(&mut self, frames: &mut Vec<Frame>) {
        if self.open == OpenBlock::None {
            return;
        }
        frames.push(Frame::ContentBlockStop {
            index: self.next_index,
        });
        self.next_index = self.next_index.saturating_add(1);
        self.open = OpenBlock::None;
    }

    fn push_delta(&mut self, delta: Delta, frames: &mut Vec<Frame>) {
        frames.push(Frame::ContentBlockDelta {
            index: self.next_index,
            delta,
        });
    }

    fn emit_error(&mut self, error: Option<&Value>, frames: &mut Vec<Frame>) {
        let code = error
            .and_then(|error| text_field(error, "code").or_else(|| text_field(error, "type")))
            .unwrap_or_default();
        let message = error
            .and_then(|error| text_field(error, "message"))
            .unwrap_or_else(|| "upstream request failed".to_owned());

        self.finished = true;
        frames.push(Frame::Error {
            error: ErrorBody {
                kind: classify(&code),
                message,
            },
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

/// §1.1 — upstream failure codes, mapped to the vocabulary the client's own
/// retry logic understands. Transient conditions must surface as retryable and
/// terminal ones as terminal; anything unrecognized is reported rather than
/// guessed at.
fn classify(code: &str) -> ErrorKind {
    match code {
        "server_is_overloaded" | "slow_down" => ErrorKind::OverloadedError,
        "rate_limit_exceeded" | "usage_limit_reached" | "insufficient_quota" => {
            ErrorKind::RateLimitError
        }
        "context_length_exceeded" | "invalid_prompt" | "bio_policy" => {
            ErrorKind::InvalidRequestError
        }
        _ => ErrorKind::ApiError,
    }
}

/// §6.1 — upstream `input_tokens` includes cached tokens and Anthropic's
/// excludes them, so the cached count is subtracted exactly once. The
/// subtraction clamps: an unsigned wrap here would render a context meter far
/// past full.
fn translate_usage(usage: &Value) -> Usage {
    let input = number(usage, "input_tokens");
    let cached = usage
        .get("input_tokens_details")
        .map(|details| number(details, "cached_tokens"))
        .unwrap_or_default();

    Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: number(usage, "output_tokens"),
        // No upstream write event exists to report (§6.1).
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    }
}

fn number(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or_default()
}

fn text_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

/// A call is identified by `call_id`, falling back to `id`. The two differ
/// upstream, and the client keys its tool results on whichever it was given.
fn call_id_of(item: &Value) -> String {
    text_field(item, "call_id")
        .or_else(|| text_field(item, "id"))
        .unwrap_or_default()
}
