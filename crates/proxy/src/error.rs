//! `docs/api.md` §1.1 — the error vocabulary.
//!
//! Every failure leaves as an Anthropic-shaped body with a type the client's
//! own retry logic understands. Transient conditions surface as retryable and
//! terminal ones as terminal, so the client's backoff drives retries and this
//! proxy does not build a second loop on top of it.

use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use proxenos_core::anthropic::ErrorBody;
use proxenos_core::anthropic::ErrorKind;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ProxyError {
    pub kind: ErrorKind,
    pub message: String,
    /// The status to report. Upstream rejections keep their own status, since
    /// the client distinguishes them.
    pub status: StatusCode,
    /// Forwarded verbatim when upstream supplies it.
    pub retry_after: Option<String>,
}

impl ProxyError {
    pub fn new(kind: ErrorKind, status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            kind,
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::InvalidRequestError,
            StatusCode::BAD_REQUEST,
            message,
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFoundError, StatusCode::NOT_FOUND, message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::AuthenticationError,
            StatusCode::UNAUTHORIZED,
            message,
        )
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::RateLimitError,
            StatusCode::TOO_MANY_REQUESTS,
            message,
        )
    }

    /// 529 rather than 503. The client recognizes it as this API's overload
    /// signal and backs off accordingly.
    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::OverloadedError,
            StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            message,
        )
    }

    pub fn upstream(status: StatusCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ApiError, status, message)
    }

    pub fn with_retry_after(mut self, retry_after: Option<String>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            kind: self.kind,
            message: self.message.clone(),
        }
    }

    /// Map an upstream HTTP status onto the vocabulary.
    ///
    /// A credential failure upstream is *this* proxy's authentication problem
    /// to report, since the client holds no credentials of its own and cannot
    /// act on a 401 by re-authenticating.
    pub fn from_upstream_status(status: StatusCode, message: impl Into<String>) -> Self {
        let message = message.into();
        match status {
            StatusCode::TOO_MANY_REQUESTS => Self::rate_limited(message),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Self::authentication(message),
            // The backend judged the request itself invalid. Saying so is more
            // use than a generic upstream error: both are terminal, but only
            // one tells the reader where to look.
            StatusCode::BAD_REQUEST => Self::invalid_request(message),
            status if status.is_server_error() => Self::overloaded(message),
            status => Self::upstream(status, message),
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let payload = serde_json::json!({
            "type": "error",
            "error": self.body(),
        });

        let mut headers = HeaderMap::new();
        if let Some(retry_after) = self
            .retry_after
            .as_deref()
            .and_then(|value| value.parse().ok())
        {
            headers.insert(axum::http::header::RETRY_AFTER, retry_after);
        }

        (self.status, headers, axum::Json(payload)).into_response()
    }
}
