//! `docs/api.md` §1 — the Anthropic Messages surface.
//!
//! The daemon binds loopback and performs no authentication: every caller
//! reaching the socket is already a local process running as the user.

use crate::error::ProxyError;
use crate::estimate::Estimator;
use crate::upstream::Transport;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use codex_cc_proxy_core::anthropic::MessagesRequest;
use codex_cc_proxy_core::sse::encode_frame;
use codex_cc_proxy_core::translate::ResponseOptions;
use codex_cc_proxy_core::translate::ResponseTranslator;
use codex_cc_proxy_core::translate::TranslateOptions;
use codex_cc_proxy_core::translate::discovered_tool_names;
use codex_cc_proxy_core::translate::translate_request;
use futures::StreamExt;
use futures::stream;
use serde_json::Value;
use std::sync::Arc;

/// How a session's transport binding is built.
///
/// A factory rather than a transport, because the binding is per conversation:
/// latching, the pooled connection, and the previous response id all belong to
/// one conversation and must not be shared between two.
/// Builds the conduit for one conversation. Takes the session id because the
/// conduit carries it on every turn as the prompt cache scope (§2.7).
pub type ConduitFactory =
    Arc<dyn Fn(String) -> Arc<crate::upstream::conduit::Conduit> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    /// §2.7 — the tier mapping and the operator's ceiling on reasoning effort,
    /// read together. Shared with the control socket, which can move both on a
    /// running daemon; a reader takes a snapshot, so a turn keeps the policy it
    /// started with.
    pub policy: Arc<crate::policy::Policy>,
    /// §7.2 — what the mapped models can actually hold.
    pub catalog: Arc<crate::catalog::Catalog>,
    /// Used when no factory is supplied — a single stateless transport, which
    /// is what the probes and most tests want.
    pub transport: Arc<dyn Transport>,
    /// Present in the running daemon. Its absence is what makes a test able to
    /// drive one fixed transport.
    pub conduits: Option<ConduitFactory>,
    /// Where captures are written. Always present: §5.4 records every empty
    /// stream regardless of whether capture was asked for, because an empty
    /// stream is always a defect and is otherwise invisible.
    pub recorder: Option<crate::recorder::Recorder>,
    /// Which captures are on, shared with the control socket so `record.start`
    /// changes what a running daemon does rather than reporting that it did.
    pub capture: Arc<crate::recorder::Switches>,
    /// The latest quota snapshot the backend volunteered, for whoever asks
    /// between turns.
    pub usage: Arc<crate::usage::UsageStore>,
    /// §2.1 — what the proxy puts around the client's system prompt.
    pub instructions: Arc<crate::config::InstructionsConfig>,
    /// Per-conversation state: calibration, discovered tools, and the baseline
    /// the incremental path will use.
    pub sessions: Arc<crate::session::SessionStore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMapping {
    pub requested: String,
    pub upstream: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(models))
        .fallback(not_found)
        .with_state(state)
}

async fn not_found() -> Response {
    ProxyError::not_found("unknown endpoint").into_response()
}

/// The mapped models, in the Anthropic list shape.
async fn models(State(state): State<AppState>) -> Response {
    let data: Vec<Value> = state
        .policy
        .get()
        .models
        .iter()
        .map(|mapping| {
            serde_json::json!({
                "id": mapping.upstream,
                "display_name": mapping.upstream,
                "type": "model",
            })
        })
        .collect();

    Json(serde_json::json!({ "data": data })).into_response()
}

/// Pre-flight sizing. Returns an estimate, and says so in `docs/api.md` §5.
///
/// It is answered by the conversation's own estimator where the conversation is
/// known, so a session that has learned what upstream charges answers with that
/// knowledge. A fresh estimator per call would leave `count_tokens` permanently
/// uncalibrated no matter how long the session had run — which is not what §5
/// says, and not what a caller sizing a request would expect.
///
/// Sizing is read-only: a conversation the store does not know is answered from
/// a fresh estimator rather than entered into it. An entry made here would never
/// advance its baseline, an empty baseline extends into anything (§3.1), and at
/// capacity it would evict a conversation that is actually running.
async fn count_tokens(
    State(state): State<AppState>,
    body: Result<Json<MessagesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return ProxyError::invalid_request(rejection.body_text()).into_response();
        }
    };

    let probe = translate_request(&request, &TranslateOptions::default());
    let estimate = match state.sessions.lookup(&probe.input) {
        Some(session) => session.estimator.estimate(&request),
        None => crate::estimate::estimate_input_tokens(&request),
    };

    Json(serde_json::json!({ "input_tokens": estimate })).into_response()
}

async fn messages(
    State(state): State<AppState>,
    body: Result<Json<MessagesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return ProxyError::invalid_request(rejection.body_text()).into_response();
        }
    };

    // One snapshot for the whole turn. Taken before anything is translated, so
    // a mapping set mid-turn cannot move the model this request is already
    // being prepared for.
    let policy = state.policy.get();

    let upstream_model = policy
        .models
        .iter()
        .find(|mapping| mapping.requested == request.model)
        .map(|mapping| mapping.upstream.clone());

    // Translated once with no session knowledge, purely to derive the item
    // sequence this conversation is identified by (§3.1).
    let probe = translate_request(&request, &TranslateOptions::default());
    let session = state.sessions.resolve(&probe.input);
    session.record_discovered(discovered_tool_names(&request));

    // §2.7 — the ceiling is the operator's, capped again by what this model
    // will actually accept.
    //
    // The client asks for a tier, not a model, so it cannot know that the model
    // behind it stops at `xhigh` while another goes to `max`. Forwarding an
    // effort the model does not support fails the turn for a reason the client
    // could not have anticipated or fixed.
    let catalog_entry = policy
        .models
        .iter()
        .find(|mapping| mapping.requested == request.model)
        .map(|mapping| mapping.upstream.as_str())
        .or(Some(request.model.as_str()))
        .and_then(|model| state.catalog.get(model));

    let supported_efforts = catalog_entry
        .map(crate::catalog::Model::supported_efforts)
        .unwrap_or_default();

    let model_ceiling = policy
        .models
        .iter()
        .find(|mapping| mapping.requested == request.model)
        .map(|mapping| mapping.upstream.as_str())
        .or(Some(request.model.as_str()))
        .and_then(|model| state.catalog.get(model))
        .and_then(crate::catalog::Model::highest_effort);

    let effort_ceiling = match (policy.effort_ceiling, model_ceiling) {
        (Some(operator), Some(model)) => Some(operator.min(model)),
        (Some(operator), None) => Some(operator),
        (None, model) => model,
    };

    // Derived from the model that will answer, not the tier that was asked
    // for: the tier is a name the client chose and the model is what is
    // actually reading the prompt.
    let answering = upstream_model
        .clone()
        .unwrap_or_else(|| request.model.clone());

    let options = TranslateOptions {
        supported_efforts,
        instructions_lead: state.instructions.lead(&answering),
        instructions_budget: state.instructions.budget().map(str::to_owned),
        instructions_trailer: state.instructions.trailer(),
        model: upstream_model,
        discovered_tools: session.discovered(),
        prompt_cache_key: Some(session.cache_key.clone()),
        effort_ceiling,
    };
    let mut translated = translate_request(&request, &options);

    // Which tier a turn arrived on is otherwise invisible, and it is the only
    // way to see that a secondary conversation — the client's own summarization
    // and search-refinement calls — really did route to the cheap tier rather
    // than the one the user is watching.
    tracing::debug!(
        requested = %request.model,
        upstream = %translated.model,
        "routing a turn"
    );

    // The id the *client* asked for, not the one it maps to: a status line reads
    // the client's own id, so that is the only one it can be recognized by.
    state.usage.record_model(&request.model);

    // What the conversation contained *before* this turn. The delta is computed
    // against this, and it has to be taken before the baseline moves.
    let baseline_before_turn = session
        .baseline
        .lock()
        .map(|baseline| baseline.clone())
        .unwrap_or_default();

    // §3.3 — put back what the client could not replay.
    //
    // The server returns reasoning items the client never receives and could
    // never send again. Left out, the conversation the backend sees loses the
    // model's own reasoning every turn — and the replay stops matching the
    // baseline at that position, so every later turn is a full send.
    if let Some(reconciled) = baseline_before_turn.reconcile(&translated.input) {
        translated.input = reconciled.input;
    }

    // A brand-new session claims its conversation immediately, so a concurrent
    // request cannot match its empty baseline and join a conversation it has
    // nothing to do with. A session that has already completed a turn is left
    // alone until this one completes too — see `seed_if_unconfirmed`.
    session.seed_if_unconfirmed(&translated.input);

    // Ingress capture happens here, before anything is sent. It needs no
    // credentials because nothing upstream is involved yet.
    if state.capture.ingress()
        && let Some(recorder) = &state.recorder
        && let Ok(raw) = serde_json::to_value(&request)
    {
        recorder.record(
            crate::recorder::Mode::Ingress,
            &raw,
            Vec::new(),
            "Captured from a live client before translation. No credentials were \
             involved: this is what the client sent, not what the backend replied.",
        );
    }

    // §6.2 — the estimate carried in `message_start`, corrected by everything
    // this conversation has already learned.
    let estimate = session.estimator.estimate(&request);

    // §7.2 — refuse a request the model cannot hold, before it is sent.
    //
    // Checking after the send would spend the request to learn what the
    // catalog already said, and return an opaque upstream rejection instead of
    // a sentence naming the limit.
    //
    // Only where the window is known. A model the catalog said nothing about is
    // unknown, not unlimited, and guessing one would refuse requests that would
    // have worked.
    if let Some(window) = state
        .catalog
        .get(&translated.model)
        .and_then(crate::catalog::Model::effective_window)
        && estimate > window
    {
        return ProxyError::invalid_request(format!(
            "this request is about {estimate} tokens, and `{}` holds about {window}. \
             Shorten the conversation or start a new one.",
            translated.model
        ))
        .into_response();
    }

    let (previous_request, previous_response_id) = session.previous();

    let events = match &state.conduits {
        Some(factory) => {
            let factory = Arc::clone(factory);
            let session_id = session.cache_key.clone();
            let conduit = session.conduit(move || factory(session_id)).await;
            match conduit
                .send(
                    &translated,
                    &baseline_before_turn,
                    previous_request.as_ref(),
                    previous_response_id.as_deref(),
                )
                .await
            {
                Ok((events, _sent)) => events,
                Err(error) => return error.into_response(),
            }
        }
        None => match state
            .transport
            .stream(&translated, Some(&session.cache_key))
            .await
        {
            Ok(events) => events,
            // Nothing has been written yet, so this can still be a status.
            Err(error) => return error.into_response(),
        },
    };

    session.remember_request(&translated);

    // An upstream refusal arriving before the response begins is not a
    // mid-stream failure. Nothing has been written to the client yet, so it can
    // still be a status — and it must be: a 200 whose body is one error frame
    // and no `message_start` is not a message the client can read, and it
    // reports it as an empty or malformed response rather than as the refusal
    // it is.
    //
    // Not just the first event. The backend opens a stream with a quota
    // snapshot and its own metadata before saying anything about the response,
    // so the refusal is the first event that *speaks to the outcome* rather
    // than the first event on the wire.
    let mut rate_limit_headers: Vec<(&'static str, String)> = Vec::new();
    let (preamble, events) = peek_preamble(events).await;
    for payload in preamble.iter().flatten() {
        if let Some(error) = upstream_refusal(payload) {
            return error.into_response();
        }
    }

    // The same preamble carries what quota is left, which is why it is read
    // here rather than during the stream: response headers are gone by then.
    if let Some(snapshot) = preamble
        .iter()
        .flatten()
        .find_map(|payload| crate::usage::Snapshot::parse(payload))
    {
        state.usage.record(&snapshot);
        rate_limit_headers = snapshot.headers();
    }

    let events = futures::stream::iter(preamble).chain(events).boxed();

    let translator = ResponseTranslator::new(ResponseOptions {
        message_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
        // The client matches this against the model it asked for, not against
        // the upstream id it was mapped to.
        model: request.model.clone(),
        estimated_input_tokens: estimate,
    });

    let empty_stream_watch = state
        .recorder
        .clone()
        .zip(serde_json::to_value(&request).ok());

    sse_response(
        events,
        rate_limit_headers,
        translator,
        empty_stream_watch,
        Calibration {
            session: Arc::clone(&session),
            estimate,
        },
        Arc::clone(&session),
        translated.input,
        state.capture.upstream(),
    )
}

/// Turn upstream events into Anthropic frames on the wire.
///
/// Dropping this response cancels the upstream request with it: the stream is
/// owned by the response body, so a client that disconnects drops the whole
/// chain rather than leaving the backend generating into nothing (§5.3).
fn sse_response(
    events: crate::upstream::EventStream,
    rate_limit_headers: Vec<(&'static str, String)>,
    translator: ResponseTranslator,
    empty_stream_watch: Option<(crate::recorder::Recorder, Value)>,
    calibration: Calibration,
    session: Arc<crate::session::Session>,
    sent_input: Vec<codex_cc_proxy_core::responses::InputItem>,
    record_upstream: bool,
) -> Response {
    let state = StreamState {
        translator,
        done: false,
        seen: Vec::new(),
        produced_content: false,
        watch: empty_stream_watch,
        record_upstream,
        calibration,
        session,
        sent_input,
    };

    let body = stream::unfold((events, state), |(mut events, mut state)| async move {
        if state.done {
            return None;
        }

        match events.next().await {
            Some(Ok(payload)) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&payload) {
                    state.seen.push(parsed);
                }
                let frames = state.translator.push(&payload);
                if frames.iter().any(|frame| {
                    matches!(
                        frame,
                        codex_cc_proxy_core::anthropic::Frame::ContentBlockDelta { .. }
                    )
                }) {
                    state.produced_content = true;
                }
                let chunk = render(&frames);
                Some((Ok::<_, std::io::Error>(chunk), (events, state)))
            }
            Some(Err(error)) => {
                // The status is already sent, so a mid-stream failure is an
                // error frame rather than a status change (§1.1).
                let frame = codex_cc_proxy_core::anthropic::Frame::Error {
                    error: error.body(),
                };
                let chunk = encode_frame(&frame);
                state.done = true;
                Some((Ok(chunk), (events, state)))
            }
            None => {
                let frames = state.translator.finish();
                state.done = true;
                state.calibrate();
                state.close_turn();
                state.record_upstream_exchange();
                if frames.is_empty() {
                    return None;
                }
                let chunk = render(&frames);
                Some((Ok(chunk), (events, state)))
            }
        }
    });

    let mut response = Response::new(Body::from_stream(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));

    // §3 — what quota is left, in the headers this client reads it from. Set
    // here because headers are gone once the body starts, which is why the
    // snapshot had to be taken from the stream's preamble rather than during
    // it.
    for (name, value) in rate_limit_headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    *response.status_mut() = StatusCode::OK;
    response
}

/// What this turn needs in order to correct its own estimate once upstream
/// reports the truth (§6.3).
struct Calibration {
    session: Arc<crate::session::Session>,
    estimate: u64,
}

struct StreamState {
    translator: ResponseTranslator,
    done: bool,
    seen: Vec<Value>,
    produced_content: bool,
    watch: Option<(crate::recorder::Recorder, Value)>,
    /// Capture the exchange whether or not it was defective.
    record_upstream: bool,
    calibration: Calibration,
    session: Arc<crate::session::Session>,
    /// What this turn put on the wire, which together with what the server adds
    /// becomes the baseline the next turn must extend (§4.3).
    sent_input: Vec<codex_cc_proxy_core::responses::InputItem>,
}

impl StreamState {
    /// §6.3 — fold this turn's true input count back into the session.
    ///
    /// The count is taken from the upstream event rather than from the frames
    /// emitted, because the frames carry the Anthropic conversion and the fit
    /// is against what upstream actually charged.
    fn calibrate(&self) {
        let Some(usage) = self
            .seen
            .iter()
            .rev()
            .find_map(|event| event.pointer("/response/usage"))
        else {
            return;
        };

        let input = usage.get("input_tokens").and_then(Value::as_u64);
        let Some(actual) = input else { return };
        if actual == 0 {
            return;
        }

        // Logged so the fit can be checked against real counts rather than a
        // modelled one — roadmap §L.
        tracing::debug!(
            estimated = self.calibration.estimate,
            actual,
            "input tokens"
        );

        self.calibration
            .session
            .estimator
            .observe(self.calibration.estimate, actual);
    }

    /// §3.3 and §4.3 — record what the server added, and what it called the
    /// response.
    ///
    /// The baseline is only correct once the server's own items are in it.
    /// Without them the next turn's delta would resend what the backend already
    /// has, or worse, be computed against a conversation neither side holds.
    fn close_turn(&self) {
        let mut returned: Vec<codex_cc_proxy_core::responses::InputItem> = Vec::new();

        for event in &self.seen {
            if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                self.session.remember_response(id.to_owned());
            }
            if event.get("type").and_then(Value::as_str) == Some("response.output_item.done")
                && let Some(item) = event.get("item")
                && let Ok(parsed) = serde_json::from_value::<
                    codex_cc_proxy_core::responses::InputItem,
                >(item.clone())
            {
                returned.push(parsed);
            }
        }

        self.session.advance(&self.sent_input, &returned);
    }

    /// §5.4 — a stream that completes having produced no content frames is
    /// recorded with its request and the upstream events that produced nothing.
    ///
    /// It is always a defect, and it is otherwise invisible: the client sees a
    /// well-formed empty turn and reports nothing wrong.
    ///
    /// Under `record upstream` every exchange is recorded instead, defective or
    /// not — a fixture is made from a turn that worked.
    fn record_upstream_exchange(&mut self) {
        if self.produced_content && !self.record_upstream {
            return;
        }
        let Some((recorder, request)) = self.watch.take() else {
            return;
        };

        let note = if self.produced_content {
            "Captured from a live exchange: the client's request, and the upstream \
             stream that answered it. Both halves are needed to replay it as a \
             fixture — the request cannot be inferred from the stream."
        } else {
            "An empty stream: the upstream events below produced no content at all. \
             Always a defect, and invisible without this record — the client sees a \
             well-formed turn that simply said nothing."
        };

        recorder.record(
            crate::recorder::Mode::Upstream,
            &request,
            std::mem::take(&mut self.seen),
            note,
        );
    }
}

/// Read the opening events without consuming the rest.
///
/// Bounded, because this runs before a single byte reaches the client: reading
/// until the response starts would let a slow backend hold the status open
/// indefinitely. Four is past the preamble the backend actually sends and stops
/// well short of any response body.
const PREAMBLE: usize = 4;

async fn peek_preamble(
    mut events: crate::upstream::EventStream,
) -> (
    Vec<Result<String, ProxyError>>,
    crate::upstream::EventStream,
) {
    let mut seen = Vec::new();
    while seen.len() < PREAMBLE {
        let Some(event) = events.next().await else {
            break;
        };
        let ends_preamble = event
            .as_ref()
            .ok()
            .is_some_and(|payload| !is_preamble_event(payload));
        seen.push(event);
        if ends_preamble {
            break;
        }
    }
    (seen, events)
}

/// Whether an event precedes the response rather than being part of it.
///
/// Everything the backend namespaces to itself: the quota snapshot and the
/// response metadata. A `response.*` event is the response starting.
fn is_preamble_event(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| kind.starts_with("codex."))
        })
        .unwrap_or(false)
}

/// An upstream event that is a refusal rather than content.
///
/// The status the backend gave is carried through rather than replaced, so a
/// 400 stays a 400 and the client's retry logic sees what actually happened.
fn upstream_refusal(payload: &str) -> Option<ProxyError> {
    let event: Value = serde_json::from_str(payload).ok()?;
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }

    let message = event
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("the backend refused the request")
        .to_owned();

    let status = event
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);

    Some(ProxyError::from_upstream_status(status, message))
}

fn render(frames: &[codex_cc_proxy_core::anthropic::Frame]) -> String {
    frames.iter().map(encode_frame).collect()
}
