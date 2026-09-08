use std::error::Error;
use std::fmt::{Display, Formatter};

/// Error codes raised by Cloud Storage operations.
///
/// Mirrors `StorageErrorCode` in `packages/storage/src/implementation/error.ts`; `as_str`
/// returns the same `storage/...` strings the JS SDK exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageErrorCode {
    /// `storage/unknown`: the backend answered with an unexpected status; see `status` and
    /// `server_response`.
    Unknown,
    /// `storage/object-not-found`: no object exists at the reference (HTTP 404).
    ObjectNotFound,
    /// `storage/bucket-not-found`: the bucket does not exist.
    BucketNotFound,
    /// `storage/project-not-found`.
    ProjectNotFound,
    /// `storage/quota-exceeded` (HTTP 402).
    QuotaExceeded,
    /// `storage/unauthenticated`: the request carried no valid user credentials (HTTP 401).
    Unauthenticated,
    /// `storage/unauthorized`: security rules denied the operation (HTTP 403).
    Unauthorized,
    /// `storage/unauthorized-app`: the App Check token was rejected (HTTP 401).
    UnauthorizedApp,
    /// `storage/retry-limit-exceeded`: retries or the operation timeout were exhausted.
    RetryLimitExceeded,
    /// `storage/invalid-checksum`.
    InvalidChecksum,
    /// `storage/canceled`.
    Canceled,
    InvalidUrl,
    InvalidDefaultBucket,
    NoDefaultBucket,
    InvalidArgument,
    AppDeleted,
    InvalidRootOperation,
    /// `storage/server-file-wrong-size`: the uploaded size does not match the server's.
    ServerFileWrongSize,
    /// `storage/invalid-format`: a string could not be decoded in the requested format.
    InvalidFormat,
    InternalError,
    UnsupportedEnvironment,
    NoDownloadUrl,
}

impl StorageErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            StorageErrorCode::Unknown => "storage/unknown",
            StorageErrorCode::ObjectNotFound => "storage/object-not-found",
            StorageErrorCode::BucketNotFound => "storage/bucket-not-found",
            StorageErrorCode::ProjectNotFound => "storage/project-not-found",
            StorageErrorCode::QuotaExceeded => "storage/quota-exceeded",
            StorageErrorCode::Unauthenticated => "storage/unauthenticated",
            StorageErrorCode::Unauthorized => "storage/unauthorized",
            StorageErrorCode::UnauthorizedApp => "storage/unauthorized-app",
            StorageErrorCode::RetryLimitExceeded => "storage/retry-limit-exceeded",
            StorageErrorCode::InvalidChecksum => "storage/invalid-checksum",
            StorageErrorCode::Canceled => "storage/canceled",
            StorageErrorCode::ServerFileWrongSize => "storage/server-file-wrong-size",
            StorageErrorCode::InvalidFormat => "storage/invalid-format",
            StorageErrorCode::InvalidUrl => "storage/invalid-url",
            StorageErrorCode::InvalidDefaultBucket => "storage/invalid-default-bucket",
            StorageErrorCode::NoDefaultBucket => "storage/no-default-bucket",
            StorageErrorCode::InvalidArgument => "storage/invalid-argument",
            StorageErrorCode::AppDeleted => "storage/app-deleted",
            StorageErrorCode::InvalidRootOperation => "storage/invalid-root-operation",
            StorageErrorCode::InternalError => "storage/internal-error",
            StorageErrorCode::UnsupportedEnvironment => "storage/unsupported-environment",
            StorageErrorCode::NoDownloadUrl => "storage/no-download-url",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StorageError {
    pub code: StorageErrorCode,
    message: String,
    pub status: Option<u16>,
    pub server_response: Option<String>,
}

impl StorageError {
    pub fn new(code: StorageErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            status: None,
            server_response: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_server_response(mut self, response: impl Into<String>) -> Self {
        self.server_response = Some(response.into());
        self
    }

    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
}

impl Display for StorageError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(server) = &self.server_response {
            write!(f, "{} ({}): {}", self.message, self.code_str(), server)
        } else {
            write!(f, "{} ({})", self.message, self.code_str())
        }
    }
}

impl Error for StorageError {}

pub type StorageResult<T> = Result<T, StorageError>;

pub fn unknown_error() -> StorageError {
    StorageError::new(
        StorageErrorCode::Unknown,
        "An unknown error occurred; check the error payload for details.",
    )
}

pub fn invalid_url(url: &str) -> StorageError {
    StorageError::new(StorageErrorCode::InvalidUrl, format!("Invalid storage URL: {url}"))
}

pub fn invalid_default_bucket(bucket: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::InvalidDefaultBucket,
        format!("Invalid default bucket: {bucket}"),
    )
}

pub fn no_default_bucket() -> StorageError {
    StorageError::new(
        StorageErrorCode::NoDefaultBucket,
        "No default storage bucket configured on this Firebase app.",
    )
}

pub fn invalid_argument(message: impl Into<String>) -> StorageError {
    StorageError::new(StorageErrorCode::InvalidArgument, message)
}

pub fn app_deleted() -> StorageError {
    StorageError::new(
        StorageErrorCode::AppDeleted,
        "The Firebase app associated with this Storage instance was deleted.",
    )
}

pub fn invalid_root_operation(operation: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::InvalidRootOperation,
        format!("'{operation}' cannot be performed on the storage root reference."),
    )
}

pub fn unsupported_environment(message: impl Into<String>) -> StorageError {
    StorageError::new(StorageErrorCode::UnsupportedEnvironment, message)
}

pub fn internal_error(message: impl Into<String>) -> StorageError {
    StorageError::new(StorageErrorCode::InternalError, message)
}

pub fn no_download_url() -> StorageError {
    StorageError::new(
        StorageErrorCode::NoDownloadUrl,
        "The requested object does not expose a download URL.",
    )
}

pub fn object_not_found(path: &str) -> StorageError {
    StorageError::new(StorageErrorCode::ObjectNotFound, format!("Object '{path}' does not exist."))
}

pub fn bucket_not_found(bucket: &str) -> StorageError {
    StorageError::new(StorageErrorCode::BucketNotFound, format!("Bucket '{bucket}' does not exist."))
}

pub fn project_not_found(project: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::ProjectNotFound,
        format!("Project '{project}' does not exist."),
    )
}

pub fn quota_exceeded(bucket: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::QuotaExceeded,
        format!(
            "Quota for bucket '{bucket}' exceeded, please view quota on \
             https://firebase.google.com/pricing/."
        ),
    )
}

pub fn unauthenticated() -> StorageError {
    StorageError::new(
        StorageErrorCode::Unauthenticated,
        "User is not authenticated, please authenticate using Firebase Authentication and try again.",
    )
}

pub fn unauthorized_app() -> StorageError {
    StorageError::new(
        StorageErrorCode::UnauthorizedApp,
        "This app does not have permission to access Firebase Storage on this project.",
    )
}

pub fn unauthorized(path: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::Unauthorized,
        format!("User does not have permission to access '{path}'."),
    )
}

pub fn retry_limit_exceeded() -> StorageError {
    StorageError::new(
        StorageErrorCode::RetryLimitExceeded,
        "Max retry time for operation exceeded, please try again.",
    )
}

pub fn invalid_checksum(path: &str, checksum: &str, calculated: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::InvalidChecksum,
        format!("Uploaded/downloaded object '{path}' has checksum '{checksum}' which does not match '{calculated}'. Please retry the upload/download."),
    )
}

pub fn canceled() -> StorageError {
    StorageError::new(StorageErrorCode::Canceled, "User canceled the upload/download.")
}

pub fn server_file_wrong_size() -> StorageError {
    StorageError::new(
        StorageErrorCode::ServerFileWrongSize,
        "Server recorded incorrect upload file size, please retry the upload.",
    )
}

pub fn invalid_format(format: &str, message: &str) -> StorageError {
    StorageError::new(
        StorageErrorCode::InvalidFormat,
        format!("String does not match format '{format}': {message}"),
    )
}
