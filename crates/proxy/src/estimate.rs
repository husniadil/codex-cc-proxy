//! `docs/proxy-behavior.md` §6.2 and §6.3 — estimating, and correcting the
//! estimate.
//!
//! Nothing here is authoritative. Where upstream reports a figure that figure
//! is used unchanged (§6.1); this exists only for `count_tokens`, which is a
//! pre-flight call with no upstream counterpart, and for `message_start`, which
//! must carry a number before upstream has produced one.

use codex_cc_proxy_core::anthropic::ContentBlock;
use codex_cc_proxy_core::anthropic::Message;
use codex_cc_proxy_core::anthropic::MessagesRequest;
use std::sync::Mutex;

/// Rough per-item framing cost, in tokens.
const ITEM_OVERHEAD: u64 = 4;

/// A stand-in cost for one attachment, expressed in characters so it flows
/// through the same ratio as everything else. Deliberately coarse: the true
/// cost depends on dimensions this proxy never sees.
const ATTACHMENT_AS_CHARS: usize = 3_000;

/// How a request's size is guessed before upstream says.
pub trait Estimator: Send + Sync {
    /// Tokens the request is expected to cost.
    fn estimate(&self, request: &MessagesRequest) -> u64;

    /// Correct against a real count for a request that was also estimated.
    ///
    /// Called once per completed response. An estimator with nothing to learn
    /// may ignore it.
    fn observe(&self, _estimated: u64, _actual: u64) {}

    fn name(&self) -> &'static str;
}

/// The structural size of a request, in characters and items.
///
/// Both estimators measure the same thing and differ only in how they turn it
/// into tokens, which is what makes comparing them meaningful.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    pub characters: usize,
    pub items: u64,
}

pub fn shape_of(request: &MessagesRequest) -> Shape {
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

    Shape { characters, items }
}

/// Attachment payloads are excluded from the character count. A base64 image is
/// megabytes of text that costs a fixed, much smaller number of tokens, and
/// counting its characters would overstate the input by orders of magnitude —
/// which the client renders as a context meter pinned at full.
fn content_size(message: &Message) -> usize {
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
                            ContentBlock::Image { .. } | ContentBlock::Document { .. } => {
                                ATTACHMENT_AS_CHARS
                            }
                            _ => 0,
                        })
                        .sum()
                })
                .unwrap_or(0),
            ContentBlock::Image { .. } | ContentBlock::Document { .. } => ATTACHMENT_AS_CHARS,
            _ => 0,
        })
        .sum()
}

/// Characters per token, before any correction.
const CHARS_PER_TOKEN: f64 = 3.6;

/// An estimate that corrects itself against upstream.
///
/// §6.3 — the upstream count includes framing this proxy does not model
/// identically: the instructions blob, serialized tool schemas, per-item
/// overhead. Calibration absorbs all of it without needing to know what any of
/// it is.
///
/// It fits a line rather than a multiplier. Part of the unmodelled cost scales
/// with the conversation and part of it does not — the instructions wrapper is
/// charged once however long the session runs. A single ratio cannot represent
/// both, and trying makes it converge from whichever regime it saw first: an
/// early short request pulls it high, and it then decays for the rest of the
/// session while every estimate reads over.
pub struct CalibratedEstimator {
    fit: Mutex<Fit>,
}

/// Incremental least squares over (raw estimate, true count).
#[derive(Debug, Default, Clone, Copy)]
struct Fit {
    n: f64,
    sum_x: f64,
    sum_y: f64,
    sum_xy: f64,
    sum_xx: f64,
}

impl Fit {
    /// Scale and offset, where enough varied observations exist to determine
    /// them. Two points on the same x say nothing about slope.
    fn line(&self) -> Option<(f64, f64)> {
        if self.n < 2.0 {
            return None;
        }
        let denominator = self.n * self.sum_xx - self.sum_x * self.sum_x;
        // All observations at the same size. The slope is unknowable, so it is
        // not guessed.
        if denominator.abs() < 1e-6 {
            return None;
        }

        let scale = (self.n * self.sum_xy - self.sum_x * self.sum_y) / denominator;
        let offset = (self.sum_y - scale * self.sum_x) / self.n;

        // A non-positive slope is not a conversation getting cheaper as it
        // grows; it is noise, and applying it would make longer sessions
        // estimate lower.
        (scale > 0.0).then_some((scale, offset))
    }

    /// The fallback while the line is undetermined: the mean ratio.
    fn ratio(&self) -> f64 {
        if self.n < 1.0 || self.sum_x <= 0.0 {
            return 1.0;
        }
        self.sum_y / self.sum_x
    }
}

impl Default for CalibratedEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl CalibratedEstimator {
    pub fn new() -> Self {
        Self {
            fit: Mutex::new(Fit::default()),
        }
    }

    /// The effective multiplier, for reporting. One while uncalibrated.
    pub fn ratio(&self) -> f64 {
        self.fit.lock().map(|fit| fit.ratio()).unwrap_or(1.0)
    }

    pub fn observations(&self) -> u32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        self.fit.lock().map(|fit| fit.n as u32).unwrap_or(0)
    }

    fn raw(shape: Shape) -> u64 {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let from_text = (shape.characters as f64 / CHARS_PER_TOKEN).ceil() as u64;
        from_text.saturating_add(shape.items.saturating_mul(ITEM_OVERHEAD))
    }
}

impl Estimator for CalibratedEstimator {
    fn estimate(&self, request: &MessagesRequest) -> u64 {
        #[allow(clippy::cast_precision_loss)]
        let raw = Self::raw(shape_of(request)) as f64;

        let Ok(fit) = self.fit.lock() else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            return raw as u64;
        };

        let corrected = match fit.line() {
            Some((scale, offset)) => raw * scale + offset,
            None => raw * fit.ratio(),
        };

        // Never zero and never negative: the client renders this live, and a
        // zero collapses the context meter at the start of the turn (§6.2).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let corrected = corrected.max(1.0).round() as u64;
        corrected
    }

    /// Fold in one true count.
    ///
    /// The observation is recorded against the *raw* estimate rather than the
    /// corrected one. Fitting against a figure the fit itself produced would
    /// make the correction chase its own output.
    fn observe(&self, estimated: u64, actual: u64) {
        if estimated == 0 || actual == 0 {
            return;
        }

        let Ok(mut fit) = self.fit.lock() else {
            return;
        };

        // Recover the raw estimate the correction was applied to, so every
        // observation is on the same axis regardless of when it was taken.
        #[allow(clippy::cast_precision_loss)]
        let corrected = estimated as f64;
        let raw = match fit.line() {
            Some((scale, offset)) if scale > 0.0 => (corrected - offset) / scale,
            _ => corrected / fit.ratio(),
        };
        if !raw.is_finite() || raw <= 0.0 {
            return;
        }

        #[allow(clippy::cast_precision_loss)]
        let y = actual as f64;

        fit.n += 1.0;
        fit.sum_x += raw;
        fit.sum_y += y;
        fit.sum_xy += raw * y;
        fit.sum_xx += raw * raw;
    }

    fn name(&self) -> &'static str {
        "calibrated"
    }
}

/// A tokenizer-backed estimator, for comparison.
///
/// It counts the text exactly and the framing not at all, which is the whole
/// question §6.3 poses: exactness over structurally different input is not
/// accuracy.
#[cfg(feature = "tokenizer")]
pub struct TokenizerEstimator {
    encoder: tiktoken_rs::CoreBPE,
}

#[cfg(feature = "tokenizer")]
impl TokenizerEstimator {
    pub fn new() -> Result<Self, crate::error::ProxyError> {
        tiktoken_rs::o200k_base()
            .map(|encoder| Self { encoder })
            .map_err(|error| {
                crate::error::ProxyError::invalid_request(format!(
                    "could not load the tokenizer: {error}"
                ))
            })
    }
}

#[cfg(feature = "tokenizer")]
impl Estimator for TokenizerEstimator {
    fn estimate(&self, request: &MessagesRequest) -> u64 {
        let mut total = 0u64;

        if let Some(system) = &request.system {
            total = total.saturating_add(self.count(&system.to_text()));
        }
        for message in &request.messages {
            total = total.saturating_add(self.count(&message_text(message)));
            total = total.saturating_add(ITEM_OVERHEAD);
        }
        for tool in &request.tools {
            if tool.defer_loading {
                continue;
            }
            let rendered = format!(
                "{}{}{}",
                tool.name,
                tool.description.as_deref().unwrap_or(""),
                tool.input_schema
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            );
            total = total.saturating_add(self.count(&rendered));
            total = total.saturating_add(ITEM_OVERHEAD);
        }

        total
    }

    fn name(&self) -> &'static str {
        "tokenizer"
    }
}

#[cfg(feature = "tokenizer")]
impl TokenizerEstimator {
    fn count(&self, text: &str) -> u64 {
        self.encoder.encode_with_special_tokens(text).len() as u64
    }
}

#[cfg(feature = "tokenizer")]
fn message_text(message: &Message) -> String {
    message
        .content
        .blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::ToolUse { name, input, .. } => Some(format!("{name}{input}")),
            ContentBlock::ToolResult { content, .. } => content.as_ref().map(|content| {
                content
                    .blocks()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            }),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The estimate for one request, using the default estimator.
pub fn estimate_input_tokens(request: &MessagesRequest) -> u64 {
    CalibratedEstimator::new().estimate(request)
}
