use crate::protocol::ControlErrorBody;

#[derive(Debug, thiserror::Error)]
pub enum ZcodeDesktopError {
    #[error("ZCode Desktop is not running")]
    DesktopUnavailable,
    #[error("ZCode Desktop host tap is not listening")]
    TapUnavailable,
    #[error("control protocol mismatch: {0}")]
    ProtocolMismatch(String),
    #[error("ZCode artifact does not match the qualified pin")]
    ArtifactMismatch { expected: String, actual: String },
    #[error("ZCode session not found: {0}")]
    SessionNotFound(String),
    #[error("Vellum thread is already bound to a different session")]
    ThreadAlreadyBound,
    #[error("duplicate in-flight turn: {0}")]
    DuplicateTurn(String),
    #[error("turn not found: {0}")]
    TurnNotFound(String),
    #[error("turn timed out: {0}")]
    Timeout(String),
    #[error("control channel closed")]
    Closed,
    #[error("CAPTCHA is waiting in ZCode Desktop")]
    CaptchaWaiting,
    #[error("{code}: {message}")]
    Remote { code: String, message: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("binding store: {0}")]
    Binding(String),
}

impl ZcodeDesktopError {
    pub fn from_body(body: ControlErrorBody) -> Self {
        match body.code.as_str() {
            "PROTOCOL_MISMATCH" => Self::ProtocolMismatch(body.message),
            "ARTIFACT_MISMATCH" => Self::ArtifactMismatch {
                expected: body
                    .data
                    .as_ref()
                    .and_then(|value| value.get("expected"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                actual: body.message,
            },
            "SESSION_NOT_FOUND" => Self::SessionNotFound(body.message),
            "THREAD_ALREADY_BOUND" => Self::ThreadAlreadyBound,
            "DUPLICATE_TURN" => Self::DuplicateTurn(body.message),
            "TURN_NOT_FOUND" => Self::TurnNotFound(body.message),
            "TIMEOUT" => Self::Timeout(body.message),
            "CAPTCHA_WAITING" => Self::CaptchaWaiting,
            "DESKTOP_UNAVAILABLE" => Self::DesktopUnavailable,
            "TAP_UNAVAILABLE" => Self::TapUnavailable,
            _ => Self::Remote {
                code: body.code,
                message: body.message,
            },
        }
    }
}
