//! API errors and their HTTP status codes, per the lightwallet spec: 400 for
//! malformed requests, 403 for unknown or unauthorised accounts, 501 when
//! account creation is off, 503 when the daemon or server is unavailable.
//! Bodies never echo request data or internal detail.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{error, warn};

#[derive(Clone, Debug, thiserror::Error)]
pub enum ApiError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("forbidden")]
    Forbidden,
    #[error("not implemented")]
    NotImplemented,
    #[error("too many requests")]
    RateLimited { retry_after: u64 },
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("daemon rejected the transaction: {0}")]
    TxRejected(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            ApiError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::TxRejected(_) | ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<lws_store::Error> for ApiError {
    fn from(e: lws_store::Error) -> Self {
        if e.is_overload() {
            ApiError::Unavailable("database pool exhausted".into())
        } else {
            ApiError::Internal(e.to_string())
        }
    }
}

impl From<lws_daemon::Error> for ApiError {
    fn from(e: lws_daemon::Error) -> Self {
        match e {
            lws_daemon::Error::Unavailable(m) => ApiError::Unavailable(m),
            other => ApiError::Unavailable(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            error!(error = %self, "request failed");
        } else {
            warn!(error = %self, status = status.as_u16(), "request refused");
        }
        let mut response = (status, status.canonical_reason().unwrap_or("")).into_response();
        if let ApiError::RateLimited { retry_after } = self {
            if let Ok(v) = HeaderValue::from_str(&retry_after.max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        response
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
