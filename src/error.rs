use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A problem the caller can act on. The message is safe to show to shoppers and assistants.
    #[error("{0}")]
    User(String),
    #[error("{0}")]
    NotFound(String),
    #[error("This link does not give access to that shop.")]
    Forbidden,
    #[error("Too many requests. Please try again in a little while.")]
    RateLimited,
    #[error(transparent)]
    PayPal(#[from] crate::paypal::PayPalError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

pub fn user_error(message: impl Into<String>) -> AppError {
    AppError::User(message.into())
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::User(message) => (StatusCode::BAD_REQUEST, message.clone()),
            AppError::NotFound(message) => (StatusCode::NOT_FOUND, message.clone()),
            AppError::Forbidden => (StatusCode::FORBIDDEN, self.to_string()),
            AppError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, self.to_string()),
            AppError::PayPal(error) => {
                tracing::error!(?error, "paypal request failed");
                (StatusCode::BAD_GATEWAY, "PayPal could not complete that request.".to_owned())
            }
            AppError::Internal(error) => {
                tracing::error!(?error, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Please try again.".to_owned())
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
