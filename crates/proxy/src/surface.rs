//! Capturing the real Messages surface as conformance fixtures.
//!
//! This proxy's product is an Anthropic Messages surface, and until this module
//! existed nothing here had ever seen one. The corpus is built from the
//! upstream provider's protocol definitions and from captures of the *client*
//! side, so every conformance claim — the SSE event vocabulary, the error
//! envelope, the sizing response — was derived from documentation rather than
//! measured against the endpoint the client thinks it is talking to.
//!
//! Neither existing capture mode answers it. `record upstream` is wired into
//! the translating path, where the events are the other provider's; a relayed
//! turn (§9) streams back untouched with nothing recording it. So this is a
//! third mode, and unlike the other two it makes the calls itself rather than
//! waiting for a client to make them: what is wanted is a handful of known
//! shapes, not whatever a session happens to send.
//!
//! It goes out through `Relay`, the same code that carries a relayed turn, so
//! what is captured is what the shipping path would receive. It spends quota —
//! one exchange per plan, and the list is deliberately short.
//!
//! **A capture is written to disk and committed.** The credential that fetched
//! it is supplied by this proxy, so the response header set is its own doing;
//! it is scrubbed by name before anything is serialized.

use crate::error::ProxyError;
use crate::upstream::relay::Relay;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

/// One captured exchange with the real Messages endpoint.
///
/// A streaming answer is held as `events`, one entry per SSE payload, and a
/// non-streaming one as `body`. Never both: which of the two arrived is itself
/// part of what the fixture records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// Stable identifier, matching the file stem.
    pub name: String,
    /// Always `captured` here. Kept as a field rather than implied, because a
    /// reader of the corpus must never have to guess.
    pub provenance: String,
    pub note: String,
    /// The path the exchange was made against, not the host: the endpoint is
    /// the operator's configuration and is not this fixture's business.
    pub endpoint: String,
    pub request: Value,
    pub status: u16,
    /// Response headers, scrubbed by name.
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// Response header names whose values must not reach a file.
///
/// The first four are credentials. `set-cookie` is a session. The organization
/// and workspace ids are neither, and are scrubbed anyway: they identify whose
/// account paid for the capture, and a fixture is committed. Both were observed
/// on a real answer, which is why they are named here rather than guessed at.
const WITHHELD: [&str; 7] = [
    "authorization",
    "x-api-key",
    "cookie",
    "proxy-authorization",
    "set-cookie",
    "anthropic-organization-id",
    "anthropic-workspace-id",
];

/// The recordable form of a response header set. The name survives where the
/// value cannot — that a header was sent at all is the datum a conformance
/// question asks about.
pub fn scrubbed(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_str().to_owned();
            let value = if WITHHELD.contains(&name.as_str()) {
                "(withheld)".to_owned()
            } else {
                value
                    .to_str()
                    .unwrap_or("(a value that is not valid UTF-8)")
                    .to_owned()
            };
            (name, value)
        })
        .collect()
}

/// Split an SSE body into its payloads, in order.
///
/// The `event:` line is dropped: it repeats the payload's own `type` and a
/// vocabulary comparison reads the payload. `[DONE]` is dropped too — it is a
/// framing terminator, and recorded as an event it would show up in every
/// comparison as a name the proxy fails to emit.
pub fn sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|payload| !payload.is_empty() && *payload != "[DONE]")
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect()
}

/// One exchange to make, and why it is worth a turn's quota.
pub struct Plan {
    pub name: &'static str,
    pub note: &'static str,
    /// Sizing has its own path and is answered locally by this proxy (§5), so
    /// only a direct call can say what the real one returns.
    pub sizing: bool,
    pub request: fn() -> Value,
}

/// The exchanges worth capturing, and no more. Every entry spends quota.
///
/// Each generating request carries a code that exists nowhere else, so a
/// capture proves a round trip rather than a plausible answer (non-negotiable
/// #4): a reply that speaks the code could not have been produced by anything
/// that did not receive it.
pub const PLANS: [Plan; 5] = [
    Plan {
        name: "plain-generation",
        note: "A non-streaming turn against the real Messages endpoint. This is where the \
               response body's own key set comes from — the shape a non-streaming answer \
               has, measured rather than read off documentation. The code 7VQK2M is \
               spoken back, so the body is an answer to this request and not a shape \
               something plausible produced.",
        sizing: false,
        request: || {
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32,
                "messages": [{
                    "role": "user",
                    "content": "Reply with exactly this and nothing else: 7VQK2M"
                }]
            })
        },
    },
    Plan {
        name: "streaming-tool-call",
        note: "A streaming turn that forces a tool call. This is the fixture the SSE event \
               vocabulary is measured against: the event names the real endpoint emits, \
               and the field set of each one, including the input_json_delta path a tool \
               call takes. The code 4HXR9T is passed through the tool arguments, so the \
               stream is an answer to this request.",
        sizing: false,
        request: || {
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 128,
                "stream": true,
                "tools": [{
                    "name": "record_code",
                    "description": "Record a verification code.",
                    "input_schema": {
                        "type": "object",
                        "properties": { "code": { "type": "string" } },
                        "required": ["code"]
                    }
                }],
                "tool_choice": { "type": "tool", "name": "record_code" },
                "messages": [{
                    "role": "user",
                    "content": "Record the code 4HXR9T."
                }]
            })
        },
    },
    Plan {
        name: "streaming-text",
        note: "A streaming turn with no tools, which is the ordinary case and the one the \
               translate path emits most of. Without it the corpus can measure the tool \
               call's block and delta shapes but not the text ones, and text is what \
               nearly every frame a client renders carries. The code 3PMD8L is spoken in \
               the deltas, so the stream answered this request.",
        sizing: false,
        request: || {
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32,
                "stream": true,
                "messages": [{
                    "role": "user",
                    "content": "Reply with exactly this and nothing else: 3PMD8L"
                }]
            })
        },
    },
    Plan {
        name: "error-envelope",
        note: "A refusal, captured for its envelope. Every failure this proxy emits claims \
               to leave in the client's own error shape with a type its retry logic \
               understands, and that claim was never measured against a real refusal. The \
               model id is deliberately one no account serves.",
        sizing: false,
        request: || {
            serde_json::json!({
                "model": "claude-not-a-model-00000000",
                "max_tokens": 16,
                "messages": [{ "role": "user", "content": "unreachable" }]
            })
        },
    },
    Plan {
        name: "count-tokens",
        note: "The sizing endpoint's real response. This proxy answers sizing locally from \
               its estimator (docs/api.md §5) and never relays it, so no turn through the \
               daemon can ever show what the real one returns — only a direct call can.",
        sizing: true,
        request: || {
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "messages": [{ "role": "user", "content": "How many tokens is this?" }]
            })
        },
    },
];

/// Headers a client sends on a Messages call. The credential is not among them:
/// the relay replaces whatever it is handed with the stored account's own.
fn request_headers() -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("anthropic-version"),
        axum::http::HeaderValue::from_static("2023-06-01"),
    );
    headers
}

/// Make one planned exchange and hold what came back.
async fn capture_one(relay: &Relay, account: &str, plan: &Plan) -> Result<Capture, ProxyError> {
    let request = (plan.request)();
    let body = axum::body::Bytes::from(request.to_string());

    let response = relay
        .forward(account, &request_headers(), None, body)
        .await?;

    let status = response.status().as_u16();
    let headers = scrubbed(response.headers());

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|error| ProxyError::overloaded(format!("reading the answer failed: {error}")))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    // Which arrived is decided by what the bytes are, not by what was asked
    // for. A streaming request that was refused answers with a JSON envelope,
    // and recording that as an empty event list would lose the envelope, which
    // is the whole reason the refusal is captured.
    let events = sse_events(&text);
    let (events, body) = if events.is_empty() {
        (Vec::new(), serde_json::from_str::<Value>(&text).ok())
    } else {
        (events, None)
    };

    Ok(Capture {
        name: plan.name.to_owned(),
        provenance: "captured".to_owned(),
        note: plan.note.to_owned(),
        endpoint: if plan.sizing {
            "/v1/messages/count_tokens".to_owned()
        } else {
            "/v1/messages".to_owned()
        },
        request,
        status,
        headers,
        events,
        body,
    })
}

/// Make every planned exchange and write each as a fixture. Returns the files
/// written, in plan order.
///
/// `only` narrows the run to one plan. A capture already made is quota already
/// spent, and adding a sixth shape to the corpus is not a reason to pay for the
/// five that are already on disk.
pub async fn capture_all(
    messages: &Relay,
    sizing: &Relay,
    account: &str,
    directory: &Path,
) -> Result<Vec<PathBuf>, ProxyError> {
    capture_some(messages, sizing, account, directory, None).await
}

pub async fn capture_some(
    messages: &Relay,
    sizing: &Relay,
    account: &str,
    directory: &Path,
    only: Option<&str>,
) -> Result<Vec<PathBuf>, ProxyError> {
    std::fs::create_dir_all(directory).map_err(|error| {
        ProxyError::invalid_request(format!("could not create {}: {error}", directory.display()))
    })?;

    if let Some(only) = only
        && !PLANS.iter().any(|plan| plan.name == only)
    {
        return Err(ProxyError::invalid_request(format!(
            "no planned exchange is named `{only}`"
        )));
    }

    let mut written = Vec::new();
    for plan in PLANS
        .iter()
        .filter(|plan| only.is_none_or(|only| only == plan.name))
    {
        let relay = if plan.sizing { sizing } else { messages };
        let capture = capture_one(relay, account, plan).await?;

        let path = directory.join(format!("{}.json", plan.name));
        let rendered = serde_json::to_string_pretty(&capture).map_err(|error| {
            ProxyError::invalid_request(format!("could not render the capture: {error}"))
        })?;
        std::fs::write(&path, format!("{rendered}\n")).map_err(|error| {
            ProxyError::invalid_request(format!("could not write {}: {error}", path.display()))
        })?;
        written.push(path);
    }
    Ok(written)
}
