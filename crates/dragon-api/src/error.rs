//! One error type, rendered as JSON.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug)]
pub enum ApiError {
    /// The request was malformed or asks for something that cannot be.
    BadRequest(String),
    /// No credentials, or credentials that mean nothing.
    Unauthorized(String),
    /// Valid credentials, insufficient privilege.
    Forbidden(String),
    NotFound(String),
    /// The name is taken.
    Conflict(String),
    /// Too many attempts in too short a time.
    TooManyRequests(String),
    /// The Realm is busy — a lock or statement timed out. Retryable.
    Busy(String),
    /// Anything we did not anticipate. Details are logged, not returned.
    Internal(anyhow::Error),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, String) {
        match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m.clone()),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            Self::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Self::TooManyRequests(m) => (StatusCode::TOO_MANY_REQUESTS, m.clone()),
            Self::Busy(m) => (StatusCode::SERVICE_UNAVAILABLE, m.clone()),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong in the Realm.".to_string(),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Internal failures are logged in full and reported vaguely: a database
        // error should never hand a connection string back to a caller.
        if let Self::Internal(error) = &self {
            tracing::error!(?error, "request failed");
        }
        let (status, message) = self.parts();
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        let error = error.into();
        // A contended or slow turn is a temporary condition the caller can
        // retry, not a bug. Reporting it as 500 would hide a capacity problem
        // among genuine faults and give clients no reason to back off.
        if is_timeout(&error) {
            return Self::Busy(
                "The Realm is busy right now. Try again in a moment.".into(),
            );
        }
        Self::Internal(error)
    }
}

/// Whether an error is Postgres refusing to wait any longer.
///
/// `55P03` is `lock_not_available` (our `lock_timeout` fired waiting on the
/// game row) and `57014` is `query_canceled` (`statement_timeout`).
fn is_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .and_then(|db| db.as_database_error())
            .and_then(|db| db.code())
            .is_some_and(|code| code == "55P03" || code == "57014")
    })
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_maps_to_its_status() {
        let cases = [
            (ApiError::BadRequest("no".into()), StatusCode::BAD_REQUEST),
            (ApiError::Unauthorized("who".into()), StatusCode::UNAUTHORIZED),
            (ApiError::Forbidden("nope".into()), StatusCode::FORBIDDEN),
            (ApiError::NotFound("gone".into()), StatusCode::NOT_FOUND),
            (ApiError::Conflict("taken".into()), StatusCode::CONFLICT),
            (ApiError::TooManyRequests("slow".into()), StatusCode::TOO_MANY_REQUESTS),
        ];
        for (error, expected) in cases {
            assert_eq!(error.parts().0, expected);
        }
    }

    #[test]
    fn internal_errors_never_leak_their_detail() {
        let error = ApiError::Internal(anyhow::anyhow!(
            "postgres://user:hunter2@db.example/dragon refused the connection"
        ));
        let (status, message) = error.parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("postgres"), "{message}");
    }

    #[test]
    fn client_errors_keep_their_message() {
        let (_, message) = ApiError::BadRequest("that quest id is not hex".into()).parts();
        assert_eq!(message, "that quest id is not hex");
    }

    #[test]
    fn a_contended_turn_is_reported_as_retryable_not_as_a_fault() {
        // 55P03 is lock_not_available; a caller that queued on the game row and
        // gave up should be told to come back, not handed a 500 that reads as
        // a bug and gives no reason to back off.
        for code in ["55P03", "57014"] {
            let (status, message) = ApiError::Busy(format!("busy {code}")).parts();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(message.contains("busy"));
        }
    }

    #[test]
    fn arbitrary_failures_convert_into_internal_errors() {
        let parsed: Result<i32, _> = "not a number".parse::<i32>();
        let error: ApiError = parsed.unwrap_err().into();
        assert!(matches!(error, ApiError::Internal(_)));
    }
}
