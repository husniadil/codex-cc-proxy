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

    sse_response(events, translator)
}

/// Turn upstream events into Anthropic frames on the wire.
///
/// Dropping this response cancels the upstream request with it: the stream is
/// owned by the response body, so a client that disconnects drops the whole
/// chain rather than leaving the backend generating into nothing (§5.3).
fn sse_response(events: crate::upstream::EventStream, translator: ResponseTranslator) -> Response {
    let state = (translator, false);

    let body = stream::unfold(
        (events, state),
        |(mut events, (mut translator, done))| async move {
            if done {
                return None;
            }

            match events.next().await {
                Some(Ok(payload)) => {
                    let frames = translator.push(&payload);
                    let chunk = render(&frames);
                    Some((
                        Ok::<_, std::io::Error>(chunk),
                        (events, (translator, false)),
                    ))
                }
                Some(Err(error)) => {
                    // The status is already sent, so a mid-stream failure is an
                    // error frame rather than a status change (§1.1).
                    let frame = codex_cc_proxy_core::anthropic::Frame::Error {
                        error: error.body(),
                    };
                    let chunk = encode_frame(&frame);
                    Some((Ok(chunk), (events, (translator, true))))
                }
                None => {
                    let frames = translator.finish();
                    if frames.is_empty() {
                        return None;
                    }
                    let chunk = render(&frames);
                    Some((Ok(chunk), (events, (translator, true))))
                }
            }
        },
    );

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

fn render(frames: &[codex_cc_proxy_core::anthropic::Frame]) -> String {
    frames.iter().map(encode_frame).collect()
}
