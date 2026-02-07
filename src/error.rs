use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("authentication error: {0}")]
    Auth(#[from] AuthError),

    #[error("api error: {0}")]
    Api(#[from] ApiError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(String),

    #[error("request timed out after {0:?}")]
    Timeout(Duration),
}

impl Error {
    /// Returns a user-friendly suggestion for how to resolve this error
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            Error::Auth(AuthError::TokenExpired) => Some("Run 'agcp login' to re-authenticate"),
            Error::Auth(AuthError::RefreshFailed(_)) => Some("Run 'agcp login' to re-authenticate"),
            Error::Auth(AuthError::OAuthFailed(_)) => {
                Some("Check your internet connection and try again")
            }
            Error::Api(ApiError::QuotaExhausted { .. }) => {
                Some("Wait for quota to reset or try a different model")
            }
            Error::Api(ApiError::CapacityExhausted) => {
                Some("Model is overloaded, try again in a few minutes")
            }
            Error::Api(ApiError::RateLimited { .. }) => Some("Too many requests, slow down"),
            Error::Timeout(_) => Some("Check your internet connection or try again"),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("access token expired")]
    TokenExpired,

    #[error("token refresh failed: {0}")]
    RefreshFailed(String),

    #[error("OAuth flow failed: {0}")]
    OAuthFailed(String),
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("rate limited - retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },

    #[error("You have exhausted your capacity on {model}. Quota will reset after {reset_time}.")]
    QuotaExhausted { model: String, reset_time: String },

    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    #[error("server error ({status}): {message}")]
    ServerError { status: u16, message: String },

    #[error("model capacity exhausted - try again later")]
    CapacityExhausted,

    #[error("request body too large: {size} bytes (max: {max} bytes)")]
    RequestTooLarge { size: usize, max: usize },
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_suggestion_token_expired() {
        let err = Error::Auth(AuthError::TokenExpired);
        assert!(err.suggestion().is_some());
        assert!(err.suggestion().unwrap().contains("login"));
    }

    #[test]
    fn test_error_suggestion_refresh_failed() {
        let err = Error::Auth(AuthError::RefreshFailed("test".to_string()));
        assert!(err.suggestion().is_some());
        assert!(err.suggestion().unwrap().contains("login"));
    }

    #[test]
    fn test_error_suggestion_quota_exhausted() {
        let err = Error::Api(ApiError::QuotaExhausted {
            model: "claude-opus-4-5".to_string(),
            reset_time: "1h".to_string(),
        });
        assert!(err.suggestion().is_some());
        assert!(err.suggestion().unwrap().contains("quota"));
    }

    #[test]
    fn test_error_suggestion_rate_limited() {
        let err = Error::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(60),
        });
        assert!(err.suggestion().is_some());
        assert!(err.suggestion().unwrap().contains("slow"));
    }

    #[test]
    fn test_error_suggestion_capacity_exhausted() {
        let err = Error::Api(ApiError::CapacityExhausted);
        assert!(err.suggestion().is_some());
        assert!(err.suggestion().unwrap().contains("overloaded"));
    }

    #[test]
    fn test_error_no_suggestion() {
        let err = Error::Http("connection failed".to_string());
        assert!(err.suggestion().is_none());
    }

    #[test]
    fn test_api_error_display() {
        let err = ApiError::InvalidRequest {
            message: "bad input".to_string(),
        };
        let display = format!("{}", err);
        assert!(display.contains("bad input"));
    }

    #[test]
    fn test_auth_error_display() {
        let err = AuthError::OAuthFailed("network error".to_string());
        let display = format!("{}", err);
        assert!(display.contains("network error"));
    }
}
