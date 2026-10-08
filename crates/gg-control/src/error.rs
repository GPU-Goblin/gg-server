//! API error type. External responses never reveal SQL, paths or secrets.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Error body returned to clients.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Human-readable message (no internal details).
    pub message: String,
}

/// API errors.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// 401.
    #[error("unauthorized")]
    Unauthorized,
    /// 403.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// 404.
    #[error("not found: {0}")]
    NotFound(String),
    /// 409.
    #[error("conflict: {0}")]
    Conflict(String),
    /// 413.
    #[error("payload too large")]
    TooLarge,
    /// 426: the client is older than this server accepts (it must be updated, not retried).
    #[error("upgrade required: {0}")]
    UpgradeRequired(String),
    /// 422 (validation of a well-formed request).
    #[error("unprocessable: {0}")]
    Unprocessable(String),
    /// 500 (details logged, not returned).
    #[error("internal error: {0}")]
    Internal(String),
    /// 503: what the request needs is not available right now (a lab node that does not answer);
    /// the same request can be tried again later.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// 503: the database aborted the transaction to resolve a deadlock or a serialization
    /// conflict. Nothing was written; the same request can be replayed as is.
    #[error("retryable: {0}")]
    Retryable(String),
}

impl ApiError {
    /// True for a transaction aborted by the database that can be replayed as is.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ApiError::Retryable(_))
    }
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "authentication required".into(),
            ),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m.clone()),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m.clone()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            ApiError::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "payload too large".into(),
            ),
            ApiError::UpgradeRequired(m) => {
                (StatusCode::UPGRADE_REQUIRED, "upgrade_required", m.clone())
            }
            ApiError::Unprocessable(m) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "unprocessable", m.clone())
            }
            ApiError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error".into(),
            ),
            ApiError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable", m.clone()),
            ApiError::Retryable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "retry",
                "concurrent update, retry the same request".into(),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(detail) = &self {
            tracing::error!(detail = %detail, "internal error");
        }
        let (status, code, message) = self.parts();
        (status, Json(ErrorBody { code, message })).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::from(gg_storage::StorageError::Sql(e))
    }
}

impl From<gg_storage::StorageError> for ApiError {
    fn from(e: gg_storage::StorageError) -> Self {
        match e {
            gg_storage::StorageError::NotFound(what) => ApiError::NotFound(what.to_owned()),
            gg_storage::StorageError::HashMismatch { .. } => {
                ApiError::Unprocessable("hash mismatch".into())
            }
            gg_storage::StorageError::SizeMismatch { .. } => {
                ApiError::Unprocessable("size mismatch".into())
            }
            gg_storage::StorageError::TooLarge(..) => ApiError::TooLarge,
            // 40P01 deadlock_detected, 40001 serialization_failure: the transaction was rolled
            // back and can be replayed.
            gg_storage::StorageError::Sql(sqlx::Error::Database(db))
                if matches!(db.code().as_deref(), Some("40P01" | "40001")) =>
            {
                ApiError::Retryable(db.message().to_owned())
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<gg_ledger::LedgerError> for ApiError {
    fn from(e: gg_ledger::LedgerError) -> Self {
        match e {
            gg_ledger::LedgerError::Insufficient {
                available,
                required,
            } => ApiError::Conflict(format!(
                "insufficient budget: available {available}, required {required}"
            )),
            gg_ledger::LedgerError::Storage(s) => s.into(),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<gg_protocol::ProtocolError> for ApiError {
    fn from(e: gg_protocol::ProtocolError) -> Self {
        match e {
            gg_protocol::ProtocolError::InvalidSignature => {
                ApiError::Forbidden("invalid signature".into())
            }
            gg_protocol::ProtocolError::Validation(m) => ApiError::Unprocessable(m),
            gg_protocol::ProtocolError::Decode(m) => ApiError::BadRequest(format!("decode: {m}")),
            gg_protocol::ProtocolError::KeyMaterial(m) => ApiError::BadRequest(m),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<gg_compute::ComputeError> for ApiError {
    fn from(e: gg_compute::ComputeError) -> Self {
        match e {
            gg_compute::ComputeError::Io(io) => ApiError::Internal(io.to_string()),
            other => ApiError::Unprocessable(other.to_string()),
        }
    }
}

impl From<gg_core::TransitionError> for ApiError {
    fn from(e: gg_core::TransitionError) -> Self {
        ApiError::Conflict(e.to_string())
    }
}
