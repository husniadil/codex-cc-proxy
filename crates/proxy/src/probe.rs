//! `docs/proxy-behavior.md` §9.3 — capability probes.
//!
//! A probe must turn on content the model could not infer. A model handed no
//! file at all describes one confidently from its name, and that output is
//! indistinguishable from success — so every probe here checks for a random
//! code that exists nowhere except in the exchange under test.
//!
//! **What these prove.** Run against replayed fixtures they establish that the
//! proxy does its half: that the bytes reach the upstream request in the shape
//! the backend expects, and that what the backend said reaches the client
//! intact. They do not establish that the backend does its half. That needs a
//! live subscription and is roadmap §L, and the matrix says so on its face.

use proxenos_core::fixture::Capability;
use serde_json::Value;

/// One thing that must be true of the upstream request, or of the frames the
/// client receives.
#[derive(Debug, Clone)]
pub enum Check {
    /// The unguessable marker must appear at this JSON pointer in the request
    /// the backend received.
    RequestContains { pointer: String, marker: String },
    /// The marker must appear anywhere in the request. Used where the exact
    /// position is not the point.
    RequestMentions { marker: String },
    /// The marker must reach the client in a content delta.
    ClientReceives { marker: String },
    /// A frame of this type must be emitted.
    FrameEmitted { frame_type: String },
    /// A content block of this type must be emitted.
    BlockEmitted { block_type: String },
    /// A number that must be present and above zero in a client frame. Zero
    /// and absent are both failures, and they fail differently from a wrong
    /// value: a zero here renders.
    PositiveInFrame { frame_type: String, pointer: String },
}

/// Which surface a probe exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// A streaming turn through `/v1/messages`.
    Messages,
    /// Pre-flight sizing through `/v1/messages/count_tokens`. It never reaches
    /// the backend, which is the whole point of probing it separately.
    CountTokens,
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub name: &'static str,
    pub capability: Capability,
    pub surface: Surface,
    /// The fixture this replays.
    pub fixture: &'static str,
    /// What it means for this to have worked, wherever it ran.
    pub checks: Vec<Check>,
    /// Checks that only mean something against a fixture.
    ///
    /// A corpus can assert the exact URL a search returned because the corpus
    /// wrote it. A live backend returns whatever it returns, and applying that
    /// check to it would fail a working capability — which teaches whoever
    /// reads the matrix to discount failures, the one habit it must not build.
    pub replay_only: Vec<Check>,
    /// Why this probe exists, in terms of what breaks silently without it.
    pub rationale: &'static str,
}

/// Every probe, in the order `doctor` reports them.
pub fn all() -> Vec<Probe> {
    vec![
        Probe {
            name: "read-image",
            surface: Surface::Messages,
            capability: Capability::ReadImage,
            fixture: "read-image",
            replay_only: Vec::new(),
            rationale: "Without the bytes the model describes the file from its \
                        name, in hedged wording that reads as success.",
            checks: vec![
                // The image must travel inside the output of the call that
                // produced it, base64 intact. The marker is a slice from deep
                // inside the encoded PNG: the code itself is rendered as
                // pixels, so nothing spells it in the bytes.
                Check::RequestContains {
                    pointer: "/input/2/output/1/image_url".to_owned(),
                    marker: "+FenH7x+dQXRB/+z55/wkvkp/zDUr24A".to_owned(),
                },
                Check::ClientReceives {
                    marker: "P7K4XR".to_owned(),
                },
            ],
        },
        Probe {
            name: "read-document",
            surface: Surface::Messages,
            capability: Capability::ReadDocument,
            fixture: "read-document",
            replay_only: Vec::new(),
            rationale: "The same failure as an image, for the file type with no \
                        upstream precedent at all.",
            checks: vec![
                Check::RequestContains {
                    pointer: "/input/3/content/0/file_data".to_owned(),
                    marker: "VjJNOVFa".to_owned(),
                },
                Check::ClientReceives {
                    marker: "V2M9QZ".to_owned(),
                },
            ],
        },
        Probe {
            name: "web-search",
            surface: Surface::Messages,
            capability: Capability::WebSearch,
            fixture: "web-search",
            rationale: "Translated as a function tool the search runs and returns \
                        nothing, which the client reports as no results.",
            checks: vec![
                Check::RequestContains {
                    pointer: "/tools/0/type".to_owned(),
                    marker: "web_search".to_owned(),
                },
                Check::BlockEmitted {
                    block_type: "server_tool_use".to_owned(),
                },
                Check::BlockEmitted {
                    block_type: "web_search_tool_result".to_owned(),
                },
            ],
            // The client extracts url and title from those blocks and nothing
            // else — but the URL is one the corpus invented, so it can only be
            // asserted against the corpus.
            replay_only: vec![Check::ClientReceives {
                marker: "https://example.invalid/sse-spec".to_owned(),
            }],
        },
        Probe {
            name: "web-fetch",
            surface: Surface::Messages,
            capability: Capability::WebFetch,
            fixture: "web-fetch",
            replay_only: Vec::new(),
            rationale: "WebFetch summarizes on the haiku tier, so an unmapped \
                        haiku breaks it in a way that looks unrelated.",
            checks: vec![
                Check::RequestMentions {
                    marker: "L9WQ2T".to_owned(),
                },
                Check::ClientReceives {
                    marker: "L9WQ2T".to_owned(),
                },
            ],
        },
        Probe {
            name: "tool-search",
            surface: Surface::Messages,
            capability: Capability::ToolSearch,
            fixture: "tool-search",
            replay_only: Vec::new(),
            rationale: "A discovered tool that is still withheld stays \
                        permanently uncallable.",
            checks: vec![
                // The discovered tool is forwarded despite still being marked
                // deferred, and the undiscovered one is not.
                Check::RequestMentions {
                    marker: "SendMessage".to_owned(),
                },
                Check::RequestContains {
                    pointer: "/input/2/output".to_owned(),
                    marker: "available_tools".to_owned(),
                },
                Check::BlockEmitted {
                    block_type: "tool_use".to_owned(),
                },
            ],
        },
        Probe {
            name: "tool-calling",
            surface: Surface::Messages,
            capability: Capability::ToolCalling,
            fixture: "tool-calling",
            replay_only: Vec::new(),
            rationale: "A call whose arguments never arrive runs the tool on \
                        nothing.",
            checks: vec![
                // Index 2, not 1: the assistant's prose is its own item, so
                // the first call follows it rather than replacing it.
                Check::RequestContains {
                    pointer: "/input/2/arguments".to_owned(),
                    marker: "/tmp/a".to_owned(),
                },
                Check::FrameEmitted {
                    frame_type: "message_stop".to_owned(),
                },
            ],
        },
        Probe {
            name: "context-meter",
            surface: Surface::Messages,
            capability: Capability::ContextMeter,
            fixture: "context-meter",
            replay_only: Vec::new(),
            rationale: "A zero in message_start collapses the meter at the start \
                        of every turn.",
            checks: vec![
                Check::FrameEmitted {
                    frame_type: "message_start".to_owned(),
                },
                // The value, not just the frame. The client renders this live,
                // so a zero here collapses the meter even though every frame
                // was emitted correctly.
                Check::PositiveInFrame {
                    frame_type: "message_start".to_owned(),
                    pointer: "/message/usage/input_tokens".to_owned(),
                },
                // And the true count replaces it rather than adding to it.
                Check::PositiveInFrame {
                    frame_type: "message_delta".to_owned(),
                    pointer: "/usage/input_tokens".to_owned(),
                },
            ],
        },
        Probe {
            name: "count-tokens",
            surface: Surface::CountTokens,
            capability: Capability::CountTokens,
            fixture: "count-tokens",
            replay_only: Vec::new(),
            rationale: "An absent or zero estimate leaves the client sizing a \
                        request it cannot size, before anything has been sent.",
            checks: vec![Check::PositiveInFrame {
                // The response is a single object rather than a stream, and is
                // presented to the checks as one frame.
                frame_type: "count_tokens".to_owned(),
                pointer: "/input_tokens".to_owned(),
            }],
        },
    ]
}

/// How a probe ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed(String),
    /// The probe could not run. Distinct from failure, and reported as such: a
    /// probe that could not run has established nothing, and calling that a
    /// pass is the same lie the probes exist to prevent.
    Skipped(String),
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub capability: Capability,
    pub status: Status,
}

/// Evaluate one probe against what was actually sent and received.
///
/// Every check, including the ones that only mean something against a corpus.
pub fn evaluate(probe: &Probe, upstream_request: &Value, client_frames: &[Value]) -> Status {
    check_all(
        probe.checks.iter().chain(probe.replay_only.iter()),
        upstream_request,
        client_frames,
    )
}

/// The same, minus the checks a live backend cannot be held to.
///
/// A live run proves more than a replayed one and asserts less, and those are
/// the same fact: what the corpus knows in advance is exactly what the backend
/// is free to answer differently.
pub fn evaluate_live(probe: &Probe, upstream_request: &Value, client_frames: &[Value]) -> Status {
    check_all(probe.checks.iter(), upstream_request, client_frames)
}

fn check_all<'a>(
    checks: impl Iterator<Item = &'a Check>,
    upstream_request: &Value,
    client_frames: &[Value],
) -> Status {
    // Two views of what the client got, because markers arrive in two forms.
    //
    // A model spells its reply a token at a time, so a code lands split across
    // several deltas and is contiguous only once the text is reassembled. A URL
    // inside a search result is a whole field of a frame and appears in neither
    // half of a delta. Scanning only the raw frames failed every attachment
    // probe against a backend that had read the attachment and said so.
    let rendered = client_frames
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("");
    let spoken = spoken_text(client_frames);

    for check in checks {
        match check {
            Check::RequestContains { pointer, marker } => {
                let found = upstream_request
                    .pointer(pointer)
                    .map(ToString::to_string)
                    .unwrap_or_default();
                if !found.contains(marker) {
                    return Status::Failed(format!(
                        "the upstream request has nothing containing `{marker}` at {pointer}"
                    ));
                }
            }
            Check::RequestMentions { marker } => {
                if !upstream_request.to_string().contains(marker) {
                    return Status::Failed(format!(
                        "the upstream request never mentions `{marker}`"
                    ));
                }
            }
            Check::ClientReceives { marker } => {
                if !rendered.contains(marker) && !spoken.contains(marker) {
                    return Status::Failed(format!("`{marker}` never reached the client"));
                }
            }
            Check::FrameEmitted { frame_type } => {
                if !client_frames
                    .iter()
                    .any(|frame| frame.get("type").and_then(Value::as_str) == Some(frame_type))
                {
                    return Status::Failed(format!("no `{frame_type}` frame was emitted"));
                }
            }
            Check::PositiveInFrame {
                frame_type,
                pointer,
            } => {
                let value = client_frames
                    .iter()
                    .find(|frame| frame.get("type").and_then(Value::as_str) == Some(frame_type))
                    .and_then(|frame| frame.pointer(pointer))
                    .and_then(Value::as_u64);

                match value {
                    Some(count) if count > 0 => {}
                    Some(_) => {
                        return Status::Failed(format!(
                            "{frame_type}{pointer} is zero, which the client renders"
                        ));
                    }
                    None => {
                        return Status::Failed(format!("{frame_type}{pointer} is missing"));
                    }
                }
            }
            Check::BlockEmitted { block_type } => {
                if !client_frames.iter().any(|frame| {
                    frame.pointer("/content_block/type").and_then(Value::as_str) == Some(block_type)
                }) {
                    return Status::Failed(format!("no `{block_type}` block was emitted"));
                }
            }
        }
    }

    Status::Passed
}

/// The reply as the user would read it: every text delta, in order, joined.
///
/// Deliberately only the deltas. Reassembling every string in every frame would
/// let a marker echoed back in the request satisfy a check about what the model
/// said, which is the opposite of what these probes are for.
fn spoken_text(frames: &[Value]) -> String {
    frames
        .iter()
        .filter_map(|frame| frame.pointer("/delta/text").and_then(Value::as_str))
        .collect()
}

/// Render the capability matrix.
///
/// The header states what the run was against. A matrix built from replayed
/// fixtures that reads like one built from a live backend is exactly the
/// plausible-looking output §9.3 exists to prevent.
pub fn matrix(outcomes: &[Outcome], against: &str) -> String {
    let mut lines = vec![format!("Capability matrix — {against}"), String::new()];

    for outcome in outcomes {
        let (mark, detail) = match &outcome.status {
            Status::Passed => ("pass", String::new()),
            Status::Failed(reason) => ("FAIL", format!("  {reason}")),
            Status::Skipped(reason) => ("skip", format!("  {reason}")),
        };
        lines.push(format!("  {mark:<5} {:<16}{detail}", outcome.name));
    }

    let failures = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, Status::Failed(_)))
        .count();
    let skipped = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, Status::Skipped(_)))
        .count();

    lines.push(String::new());
    lines.push(format!(
        "{} passed, {failures} failed, {skipped} skipped",
        outcomes
            .len()
            .saturating_sub(failures)
            .saturating_sub(skipped)
    ));

    lines.join("\n")
}
