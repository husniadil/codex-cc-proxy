//! `docs/proxy-behavior.md` §6.2 — the two points that need an estimate.
//!
//! Nothing here is authoritative. Where upstream reports a figure that figure
//! is used unchanged (§6.1); this exists only for `count_tokens`, which is a
//! pre-flight call with no upstream counterpart, and for `message_start`, which
//! must carry a number before upstream has produced one.

use codex_cc_proxy_core::anthropic::MessagesRequest;

/// Characters per token, before calibration.
///
/// A ratio, not a tokenizer. The figure that matters includes framing this
/// proxy does not model identically — the instructions blob, serialized tool
/// schemas, per-item overhead — so a byte-exact tokenizer over structurally
/// different input is authoritatively wrong, which is worse than approximate
/// and self-correcting (§6.3).
const CHARS_PER_TOKEN: f64 = 3.6;

/// Rough per-item framing cost, in tokens.
const ITEM_OVERHEAD: u64 = 4;

/// Estimate the input size of a request.
pub fn estimate_input_tokens(request: &MessagesRequest) -> u64 {
    let mut characters = 0usize;
    let mut items = 0u64;

    if let Some(system) = &request.system {
        characters = characters.saturating_add(system.to_text().len());
        items = items.saturating_add(1);
    }

    for message in &request.messages {
        items = items.saturating_add(1);
        characters = characters.saturating_add(content_size(message));
    }

    for tool in &request.tools {
        // A withheld tool costs nothing, which is the point of withholding it.
        if tool.defer_loading {
            continue;
        }
        items = items.saturating_add(1);
        characters = characters.saturating_add(tool.name.len());
        characters = characters.saturating_add(tool.description.as_deref().unwrap_or("").len());
        characters = characters.saturating_add(
            tool.input_schema
                .as_ref()
                .map(|schema| schema.to_string().len())
                .unwrap_or_default(),
        );
    }

    let from_text = (characters as f64 / CHARS_PER_TOKEN).ceil() as u64;
    from_text.saturating_add(items.saturating_mul(ITEM_OVERHEAD))
}

/// Attachment payloads are excluded from the character count. A base64 image is
/// megabytes of text that costs a fixed, much smaller number of tokens, and
/// counting its characters would overstate the input by orders of magnitude —
/// which the client renders as a context meter pinned at full.
fn content_size(message: &codex_cc_proxy_core::anthropic::Message) -> usize {
    use codex_cc_proxy_core::anthropic::ContentBlock;

    message
        .content
        .blocks()
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolUse { name, input, .. } => {
                name.len().saturating_add(input.to_string().len())
            }
            ContentBlock::ToolResult { content, .. } => content
                .as_ref()
                .map(|content| {
                    content
                        .blocks()
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Text { text } => text.len(),
                            _ => 0,
                        })
                        .sum()
                })
                .unwrap_or(0),
            ContentBlock::Image { .. } | ContentBlock::Document { .. } => IMAGE_TOKENS_AS_CHARS,
            _ => 0,
        })
        .sum()
}

/// A stand-in cost for one attachment, expressed in characters so it flows
/// through the same ratio. Deliberately coarse: the true cost depends on
/// dimensions this proxy never sees.
const IMAGE_TOKENS_AS_CHARS: usize = 3_000;
