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
pub type ConduitFactory = Arc<dyn Fn() -> Arc<crate::upstream::conduit::Conduit> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    /// §2.7 — an operator's ceiling on reasoning effort.
    pub effort_ceiling: Option<codex_cc_proxy_core::responses::Effort>,
    /// Used when no factory is supplied — a single stateless transport, which
    /// is what the probes and most tests want.
    pub transport: Arc<dyn Transport>,
    /// Present in the running daemon. Its absence is what makes a test able to
    /// drive one fixed transport.
    pub conduits: Option<ConduitFactory>,
    /// Tier name to upstream model id.
    pub models: Arc<Vec<ModelMapping>>,
    /// Where captures are written. Always present: §5.4 records every empty
    /// stream regardless of whether capture was asked for, because an empty
    /// stream is always a defect and is otherwise invisible.
    pub recorder: Option<crate::recorder::Recorder>,
    /// Whether to capture every request, not only the defective ones.
    pub record_ingress: bool,
    /// Per-conversation state: calibration, discovered tools, and the baseline
    /// the incremental path will use.
    pub sessions: Arc<crate::session::SessionStore>,
}

#[derive(Debug, Clone)]
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

    let upstream_model = state
        .models
        .iter()
        .find(|mapping| mapping.requested == request.model)
        .map(|mapping| mapping.upstream.clone());

    // Translated once with no session knowledge, purely to derive the item
    // sequence this conversation is identified by (§3.1).
    let probe = translate_request(&request, &TranslateOptions::default());
    let session = state.sessions.resolve(&probe.input);
    session.record_discovered(discovered_tool_names(&request));

    let options = TranslateOptions {
        model: upstream_model,
        discovered_tools: session.discovered(),
        prompt_cache_key: Some(session.cache_key.clone()),
        effort_ceiling: state.effort_ceiling,
    };
    let mut translated = translate_request(&request, &options);

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

    // The baseline advances before the reply arrives. What was sent is what the
    // next turn must extend, and recording it later would leave a window in
    // which a concurrent request sees an empty baseline and matches anything.
    //
    // It must not be what the delta is measured against: advancing first and
    // then diffing compares this turn's input with itself, which is always
    // empty. An empty delta is not a small delta — the backend answers from the
    // previous response and the turn silently repeats itself.
    session.advance(&translated.input, &[]);

    // Ingress capture happens here, before anything is sent. It needs no
    // credentials because nothing upstream is involved yet.
    if state.record_ingress
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

    let (previous_request, previous_response_id) = session.previous();

    let events = match &state.conduits {
        Some(factory) => {
            let factory = Arc::clone(factory);
            let conduit = session.conduit(move || factory()).await;
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
        None => match state.transport.stream(&translated).await {
            Ok(events) => events,
            // Nothing has been written yet, so this can still be a status.
            Err(error) => return error.into_response(),
        },
    };

    session.remember_request(&translated);

    // §6.2 — the estimate carried in `message_start`, corrected by everything
    // this conversation has already learned.
    let estimate = session.estimator.estimate(&request);

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
        translator,
        empty_stream_watch,
        Calibration {
            session: Arc::clone(&session),
            estimate,
        },
        Arc::clone(&session),
        translated.input,
    )
}

/// Turn upstream events into Anthropic frames on the wire.
///
/// Dropping this response cancels the upstream request with it: the stream is
/// owned by the response body, so a client that disconnects drops the whole
/// chain rather than leaving the backend generating into nothing (§5.3).
fn sse_response(
    events: crate::upstream::EventStream,
    translator: ResponseTranslator,
    empty_stream_watch: Option<(crate::recorder::Recorder, Value)>,
    calibration: Calibration,
    session: Arc<crate::session::Session>,
    sent_input: Vec<codex_cc_proxy_core::responses::InputItem>,
) -> Response {
    let state = StreamState {
        translator,
        done: false,
        seen: Vec::new(),
        produced_content: false,
        watch: empty_stream_watch,
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
                state.record_if_empty();
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
    fn record_if_empty(&mut self) {
        if self.produced_content {
            return;
        }
        let Some((recorder, request)) = self.watch.take() else {
            return;
        };

        recorder.record(
            crate::recorder::Mode::Upstream,
            &request,
            std::mem::take(&mut self.seen),
            "An empty stream: the upstream events below produced no content at all. \
             Always a defect, and invisible without this record — the client sees a \
             well-formed turn that simply said nothing.",
        );
    }
}

fn render(frames: &[codex_cc_proxy_core::anthropic::Frame]) -> String {
    frames.iter().map(encode_frame).collect()
}
