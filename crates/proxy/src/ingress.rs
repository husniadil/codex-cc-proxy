//! `docs/api.md` §1 — the Anthropic Messages surface.
//!
//! The daemon binds loopback and performs no authentication: every caller
//! reaching the socket is already a local process running as the user.

use crate::error::ProxyError;
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

#[derive(Clone)]
pub struct AppState {
    pub transport: Arc<dyn Transport>,
    /// Tier name to upstream model id.
    pub models: Arc<Vec<ModelMapping>>,
    /// Where captures are written. Always present: §5.4 records every empty
    /// stream regardless of whether capture was asked for, because an empty
    /// stream is always a defect and is otherwise invisible.
    pub recorder: Option<crate::recorder::Recorder>,
    /// Whether to capture every request, not only the defective ones.
    pub record_ingress: bool,
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
async fn count_tokens(
    body: Result<Json<MessagesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return ProxyError::invalid_request(rejection.body_text()).into_response();
        }
    };

    let estimate = crate::estimate::estimate_input_tokens(&request);
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

    let options = TranslateOptions {
        model: upstream_model,
        discovered_tools: discovered_tool_names(&request),
        prompt_cache_key: None,
    };
    let translated = translate_request(&request, &options);

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

    let events = match state.transport.stream(&translated).await {
        Ok(events) => events,
        // Nothing has been written yet, so this can still be a status.
        Err(error) => return error.into_response(),
    };

    let translator = ResponseTranslator::new(ResponseOptions {
        message_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
        // The client matches this against the model it asked for, not against
        // the upstream id it was mapped to.
        model: request.model.clone(),
        estimated_input_tokens: crate::estimate::estimate_input_tokens(&request),
    });

    let empty_stream_watch = state
        .recorder
        .clone()
        .zip(serde_json::to_value(&request).ok());

    sse_response(events, translator, empty_stream_watch)
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
) -> Response {
    let state = StreamState {
        translator,
        done: false,
        seen: Vec::new(),
        produced_content: false,
        watch: empty_stream_watch,
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

struct StreamState {
    translator: ResponseTranslator,
    done: bool,
    seen: Vec<Value>,
    produced_content: bool,
    watch: Option<(crate::recorder::Recorder, Value)>,
}

impl StreamState {
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
