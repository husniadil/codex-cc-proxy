//! `docs/proxy-behavior.md` §10.3 — capability probes.
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

impl Check {
    /// Whether this reads the request the backend was sent.
    ///
    /// The split matters on one arm only: a live relay forwards bytes this
    /// process cannot watch, so the request half has nothing to read and a
    /// check applied to a `Null` there would pass without establishing
    /// anything.
    pub fn reads_the_request(&self) -> bool {
        matches!(
            self,
            Self::RequestContains { .. } | Self::RequestMentions { .. }
        )
    }
}

/// Which surface a probe exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// A streaming turn through `/v1/messages`.
    Messages,
    /// Pre-flight sizing through `/v1/messages/count_tokens`. It never reaches
    /// the backend, which is the whole point of probing it separately.
    CountTokens,
    /// A turn forwarded rather than translated (§9). Nothing on this path is
    /// parsed, so what it establishes is that the bytes were not touched.
    Relay,
    /// The environment a launch renders (`docs/api.md` §2.2). Like
    /// `CountTokens` it is answered entirely by the proxy and reaches no
    /// backend on any mode.
    Environment,
}

impl Surface {
    /// Whether a live run's turn on this surface is answered by a backend.
    ///
    /// Two surfaces are served entirely by the proxy, and the live header's
    /// claim that the backend answered and was billed is false of both. A row
    /// silently exempt from the header above it is the plausible output §10.3
    /// exists to prevent.
    pub fn reaches_the_backend(self) -> bool {
        !matches!(self, Self::CountTokens | Self::Environment)
    }
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub name: &'static str,
    pub capability: Capability,
    pub surface: Surface,
    /// The fixture this replays. Empty for a probe that replays nothing —
    /// `Surface::Environment` asks the proxy to render, and there is no
    /// exchange to read.
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
        Probe {
            name: "relay",
            surface: Surface::Relay,
            capability: Capability::Relay,
            fixture: "relay",
            replay_only: Vec::new(),
            rationale: "A relayed turn that is re-encoded on the way loses every \
                        field this proxy does not model, and loses it silently: \
                        the request still succeeds and the answer still reads \
                        like one.",
            checks: vec![
                // Inside a field the proxy has no type for, so nothing that
                // parsed and rebuilt the body could carry it here.
                Check::RequestContains {
                    pointer: "/an_unmodelled_field/marker".to_owned(),
                    marker: "N8QP4W".to_owned(),
                },
                // And the answering bytes came back as they were sent.
                Check::ClientReceives {
                    marker: "T5ZJ9C".to_owned(),
                },
            ],
        },
        Probe {
            name: "env-contract",
            surface: Surface::Environment,
            capability: Capability::EnvContract,
            fixture: "",
            replay_only: Vec::new(),
            rationale: "Without ENABLE_TOOL_SEARCH the client disables deferred \
                        tool loading on a custom base URL, and without \
                        CLAUDE_CODE_DISABLE_1M_CONTEXT it invents a million-token \
                        window for an id it cannot recognize. Both present as a \
                        broken-looking client over a green matrix.",
            // Asserted by `check_environment` against two rendered
            // environments rather than by the check list, which reads one
            // exchange. The contract is a difference between two mappings, and
            // a check that could only see one of them would state half of it.
            checks: Vec::new(),
        },
    ]
}

/// The two variables that must survive every change to the launch surface.
pub const DEFERRAL_OVERRIDE: &str = "ENABLE_TOOL_SEARCH";
pub const WINDOW_FLAG: &str = "CLAUDE_CODE_DISABLE_1M_CONTEXT";

/// `docs/api.md` §2.2 — the launch contract, asserted on what was rendered.
///
/// Two environments, because half the contract is an absence: the window flag
/// belongs on a mapping where a tier translates and must not be emitted where
/// every tier is relayed (§7.2). Asserting the first alone would pass a build
/// that emitted it unconditionally, which strips an entitlement the account may
/// hold from ids the client recognizes itself.
///
/// On the rendered variables rather than on the configuration behind them. What
/// the client reads is the environment; a probe of the switch would stay green
/// over a launch that emitted nothing.
pub fn check_environment(
    translating: &[(String, String)],
    all_relay: &[(String, String)],
) -> Status {
    let has = |rendered: &[(String, String)], name: &str| {
        rendered.iter().any(|(variable, _)| variable == name)
    };

    for (mapping, rendered) in [("translating", translating), ("all-relay", all_relay)] {
        if !has(rendered, DEFERRAL_OVERRIDE) {
            return Status::Failed(format!(
                "a {mapping} launch renders no {DEFERRAL_OVERRIDE}, so the client \
                 disables deferred tool loading on this base URL"
            ));
        }
    }

    if !has(translating, WINDOW_FLAG) {
        return Status::Failed(format!(
            "a translating launch renders no {WINDOW_FLAG}, so the client appends \
             `[1m]` to an id it cannot recognize and assumes a window four times \
             the model's"
        ));
    }

    if has(all_relay, WINDOW_FLAG) {
        return Status::Failed(format!(
            "an all-relay launch renders {WINDOW_FLAG}, which strips \
             `context-1m-2025-08-07` from ids the client recognizes itself (§7.2)"
        ));
    }

    Status::Passed
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
    /// Which surface the probe drove. The matrix reads it to mark the row a
    /// live run did not actually bill for.
    pub surface: Surface,
    /// The probe's own rationale, carried so a failure can print it without
    /// the renderer having to look the probe up again.
    pub rationale: &'static str,
    pub status: Status,
    /// What this run could not establish, where a passing row would otherwise
    /// claim more than it measured. Printed beside the row.
    pub note: Option<String>,
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

/// The checks that read the answer, and only those.
///
/// A live relay forwards the client's bytes straight through, so there is no
/// point inside this process where they can be observed. Running the
/// request-half checks over the `Null` that stands in for them would report a
/// pass for a half nothing looked at, which is the plausible output §10.3
/// exists to prevent. What is left out is named on the row instead.
pub fn evaluate_answer_only(probe: &Probe, client_frames: &[Value]) -> Status {
    check_all(
        probe
            .checks
            .iter()
            .filter(|check| !check.reads_the_request()),
        &Value::Null,
        client_frames,
    )
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

/// What a matrix was produced by.
///
/// One value rather than a phrase plus a flag: the header, the row marks, and
/// the coverage line all turn on whether the backend answered, and two
/// encodings of that would eventually disagree.
pub enum Evidence {
    /// Answered from recordings. Carries the corpus in the words the header
    /// prints, because a directory can hold a recording made minutes ago while
    /// the embedded copy is whatever the binary was built from.
    Replay { corpus: String },
    /// Answered by the real backend, as this account. `None` is the account
    /// serving turns.
    ///
    /// `relay` is the account the §9 arm spent, which is a different one by
    /// construction: a relayed turn is authorized as an account on the second
    /// provider, named rather than serving. `None` where that arm did not run.
    Live {
        account: Option<String>,
        relay: Option<String>,
    },
}

/// One probe run, in the terms the matrix has to state.
pub struct Run {
    pub evidence: Evidence,
}

/// The replay header. A matrix built from recordings that reads like one built
/// from a backend is exactly the plausible output §10.3 exists to prevent.
pub const AGAINST_REPLAY: &str = "replayed fixtures (the backend was not contacted)";

/// The live counterpart. It spends quota, and the matrix says so.
pub const AGAINST_LIVE: &str = "a live backend (the backend answered, and was billed)";

/// What a `count-tokens` row is marked with under `--live`.
///
/// The header's claim is true of every other row and false of this one, and a
/// row silently exempt from the header above it is the kind of plausible
/// output the probes exist to prevent.
pub const NEVER_REACHES_THE_BACKEND: &str =
    "(answered by the proxy; this surface never reaches the backend)";

impl Evidence {
    /// The header line, verbatim.
    fn describe(&self) -> String {
        match self {
            Self::Replay { corpus } => format!("{AGAINST_REPLAY} — {corpus}"),
            Self::Live { .. } => AGAINST_LIVE.to_owned(),
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// What a run established about one path.
///
/// A path nothing ran on and a path whose every probe failed are different
/// facts, and the line does not say the same thing about them.
#[derive(PartialEq)]
enum Reach {
    /// At least one probe on the path passed.
    Exercised,
    /// Probes on the path ran and every one of them failed.
    Nothing,
    /// Nothing on the path ran, or everything on it was skipped.
    Absent,
}

fn reach(outcomes: &[Outcome], on_relay: bool) -> Reach {
    let rows = outcomes
        .iter()
        .filter(|outcome| (outcome.surface == Surface::Relay) == on_relay);
    let mut failed = false;
    for outcome in rows {
        match outcome.status {
            Status::Passed => return Reach::Exercised,
            Status::Failed(_) => failed = true,
            Status::Skipped(_) => {}
        }
    }
    if failed {
        Reach::Nothing
    } else {
        Reach::Absent
    }
}

/// One line naming what the run exercised, and what it did not.
///
/// A green matrix says nothing about the WebSocket transport or the relay, and
/// a reader with nothing to tell them otherwise reads green as coverage of the
/// whole proxy. Both absences are named here rather than inferred. Both halves
/// are derived from the outcomes: a path with no passing row is not exercised,
/// and its account is not named as spent.
fn coverage(outcomes: &[Outcome], run: &Run) -> String {
    let relay = match (&run.evidence, reach(outcomes, true)) {
        (_, Reach::Absent) => "the relay path (§9) was not exercised".to_owned(),
        (_, Reach::Nothing) => {
            "the relay path (§9) established nothing (every probe on it failed)".to_owned()
        }
        (
            Evidence::Live {
                relay: Some(name), ..
            },
            Reach::Exercised,
        ) => {
            format!("the relay path (§9) answered live as `{name}`")
        }
        (_, Reach::Exercised) => "the relay path (§9) was replayed".to_owned(),
    };

    let translation = match reach(outcomes, false) {
        Reach::Absent => "the translation path was not exercised".to_owned(),
        Reach::Nothing => {
            "the translation path established nothing (every probe on it failed)".to_owned()
        }
        Reach::Exercised => match &run.evidence {
            Evidence::Replay { corpus } => {
                format!("the translation path, answered from {corpus}")
            }
            Evidence::Live { account, .. } => {
                let whose = match account {
                    Some(account) => format!("as `{account}`"),
                    None => "as the account serving turns".to_owned(),
                };
                format!("the translation path over the HTTP transport, {whose}")
            }
        },
    };

    match &run.evidence {
        Evidence::Replay { .. } => format!(
            "Exercised: {translation}; {relay}. \
             Not exercised: the WebSocket transport, and no account was contacted."
        ),
        Evidence::Live { .. } => format!(
            "Exercised: {translation}; {relay}. \
             Not exercised: the WebSocket transport."
        ),
    }
}

/// Render the capability matrix.
///
/// The header states what the run was against, a failure states why the probe
/// exists at all, and the line underneath states what the run did not touch.
pub fn matrix(outcomes: &[Outcome], run: &Run) -> String {
    let mut lines = vec![
        format!("Capability matrix — {}", run.evidence.describe()),
        String::new(),
    ];

    for outcome in outcomes {
        let (mark, detail) = match &outcome.status {
            Status::Passed => ("pass", String::new()),
            Status::Failed(reason) => ("FAIL", format!("  {reason}")),
            Status::Skipped(reason) => ("skip", format!("  {reason}")),
        };
        // §10.3 — the one surface the live header does not describe.
        let marked = if run.evidence.is_live() && !outcome.surface.reaches_the_backend() {
            format!("{detail}  {NEVER_REACHES_THE_BACKEND}")
        } else {
            detail
        };
        // What the row did not establish, where it did not establish all of it.
        let marked = match &outcome.note {
            Some(note) => format!("{marked}  ({note})"),
            None => marked,
        };
        lines.push(format!("  {mark:<5} {:<16}{marked}", outcome.name));

        // Only where it is needed. A rationale on every row is eight
        // paragraphs of prose over a matrix nobody would then read.
        if matches!(outcome.status, Status::Failed(_)) {
            lines.push(format!("        {}", outcome.rationale));
        }
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
    lines.push(coverage(outcomes, run));
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
