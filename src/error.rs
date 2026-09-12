use thiserror::Error;

pub type Result<T> = std::result::Result<T, BridgeError>;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("login: {0}")]
    Login(String),
    #[error("verification code invalid")]
    VerificationCode,
    #[error("proxy: {0}")]
    Proxy(String),
    #[error("storage: {0}")]
    Storage(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl BridgeError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            BridgeError::Unauthorized => StatusCode::UNAUTHORIZED,
            BridgeError::Login(_) | BridgeError::VerificationCode => StatusCode::UNAUTHORIZED,
            BridgeError::Proxy(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            BridgeError::Unauthorized => "unauthorized",
            BridgeError::Login(_) => "login_failed",
            BridgeError::VerificationCode => "verification_code",
            BridgeError::Proxy(_) => "proxy_error",
            BridgeError::Storage(_) => "storage_error",
            _ => "internal",
        }
    }
}
