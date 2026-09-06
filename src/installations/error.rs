use std::fmt::{Display, Formatter};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallationsErrorCode {
    InvalidArgument,
    Internal,
    RequestFailed,
}

impl InstallationsErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            InstallationsErrorCode::InvalidArgument => "installations/invalid-argument",
            InstallationsErrorCode::Internal => "installations/internal",
            InstallationsErrorCode::RequestFailed => "installations/request-failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstallationsError {
    pub code: InstallationsErrorCode,
    message: String,
    server_code: Option<u16>,
}

impl InstallationsError {
    pub fn new(code: InstallationsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            server_code: None,
        }
    }

    /// Attaches the HTTP status code returned by the Installations backend.
    ///
    /// Mirrors `customData.serverCode` on the JS `FirebaseError` so callers (and the SDK's own
    /// token refresh logic) can distinguish "installation not found" from other failures.
    pub fn with_server_code(mut self, status: u16) -> Self {
        self.server_code = Some(status);
        self
    }

    /// HTTP status code returned by the backend, when the error originated from a response.
    pub fn server_code(&self) -> Option<u16> {
        self.server_code
    }

    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
}

impl Display for InstallationsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code_str())
    }
}

impl std::error::Error for InstallationsError {}

pub type InstallationsResult<T> = Result<T, InstallationsError>;

pub fn invalid_argument(message: impl Into<String>) -> InstallationsError {
    InstallationsError::new(InstallationsErrorCode::InvalidArgument, message)
}

pub fn internal_error(message: impl Into<String>) -> InstallationsError {
    InstallationsError::new(InstallationsErrorCode::Internal, message)
}

pub fn request_failed(message: impl Into<String>) -> InstallationsError {
    InstallationsError::new(InstallationsErrorCode::RequestFailed, message)
}
