//! One error type for the whole service, and how it becomes an HTTP response.
//!
//! Every failure a handler can return is an [`ApiError`]. Most come from the
//! database: inillucent reports each failure with a [`Status`], and the status
//! decides the HTTP code. A constraint the schema declares, such as a unique
//! email or a due date that is not a real day, comes back as `constraint` and
//! becomes `409 Conflict` with the engine's own message, so the Rust code does
//! not repeat a rule the schema already enforces.
//!
//! Every error body has the same shape:
//!
//! ```json
//! { "error": "conflict", "message": "UNIQUE constraint failed: person.email" }
//! ```

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use inillucent::Status;
use serde_json::json;

/// A failure, with the HTTP status it is answered with.
#[derive(Debug)]
pub struct ApiError {
    /// The HTTP status.
    pub status: StatusCode,
    /// A short machine readable name, such as `not_found`.
    pub code: &'static str,
    /// A sentence a person can read.
    pub message: String,
}

impl ApiError {
    /// A request that names a row that does not exist.
    ///
    /// @param what - what was not found, such as `todo 12`
    pub fn not_found(what: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::NOT_FOUND, code: "not_found", message: format!("{} does not exist", what.into()) }
    }

    /// A request the service refuses before it reaches the database.
    ///
    /// @param message - what is wrong with the request
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::BAD_REQUEST, code: "bad_request", message: message.into() }
    }

    /// A request that is well formed but conflicts with the data, such as a
    /// todo made its own subtask.
    ///
    /// @param message - what the conflict is
    pub fn conflict(message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::CONFLICT, code: "conflict", message: message.into() }
    }
}

impl From<inillucent::Error> for ApiError {
    /// Maps an engine failure to an HTTP status by its [`Status`].
    ///
    /// `unsupported` gets its own code, `501`, because it means this build of
    /// the engine has not implemented a construct the SQL uses. That is a
    /// different fix from a bug in the SQL, and a caller should be able to
    /// tell the two apart.
    ///
    /// @param error - the engine's failure
    fn from(error: inillucent::Error) -> ApiError {
        let (status, code) = match error.status {
            Status::Constraint => (StatusCode::CONFLICT, "conflict"),
            Status::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
            Status::Unsupported => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "database"),
        };
        ApiError { status, code, message: error.message }
    }
}

impl IntoResponse for ApiError {
    /// Writes the error as a JSON body with its status.
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            eprintln!("{} {}: {}", self.status.as_u16(), self.code, self.message);
        }
        (self.status, Json(json!({ "error": self.code, "message": self.message }))).into_response()
    }
}

/// What every store method and handler returns.
pub type ApiResult<T> = Result<T, ApiError>;
