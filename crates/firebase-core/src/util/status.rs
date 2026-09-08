//! The canonical Google API status codes, and the error envelope the backends return.
//!
//! Every Firebase backend answers a failure the same way: an HTTP status, and a body of the shape
//! `{"error": {"code": 409, "message": "…", "status": "ABORTED"}}` whose `status` is the canonical
//! code from `google.rpc.Code`. That code is the more precise of the two — `ABORTED` and
//! `FAILED_PRECONDITION` both arrive as a 409 or a 400 — so it wins when present.
//!
//! Functions, Firestore and Installations each carried their own copy of this taxonomy, its HTTP
//! mapping and its envelope parser. Products still own their public error types; what they share
//! is the reading of what the server said.

use serde::Deserialize;

use crate::platform::http::HttpResponse;

/// A canonical status code, as `google.rpc.Code` defines it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusCode {
    Ok,
    Cancelled,
    Unknown,
    InvalidArgument,
    DeadlineExceeded,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
    Unauthenticated,
}

impl StatusCode {
    /// The name the backend sends, e.g. `NOT_FOUND`.
    pub fn wire_name(&self) -> &'static str {
        match self {
            StatusCode::Ok => "OK",
            StatusCode::Cancelled => "CANCELLED",
            StatusCode::Unknown => "UNKNOWN",
            StatusCode::InvalidArgument => "INVALID_ARGUMENT",
            StatusCode::DeadlineExceeded => "DEADLINE_EXCEEDED",
            StatusCode::NotFound => "NOT_FOUND",
            StatusCode::AlreadyExists => "ALREADY_EXISTS",
            StatusCode::PermissionDenied => "PERMISSION_DENIED",
            StatusCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
            StatusCode::FailedPrecondition => "FAILED_PRECONDITION",
            StatusCode::Aborted => "ABORTED",
            StatusCode::OutOfRange => "OUT_OF_RANGE",
            StatusCode::Unimplemented => "UNIMPLEMENTED",
            StatusCode::Internal => "INTERNAL",
            StatusCode::Unavailable => "UNAVAILABLE",
            StatusCode::DataLoss => "DATA_LOSS",
            StatusCode::Unauthenticated => "UNAUTHENTICATED",
        }
    }

    /// The name Firebase error codes use, e.g. `not-found` in `functions/not-found`.
    pub fn slug(&self) -> &'static str {
        match self {
            StatusCode::Ok => "ok",
            StatusCode::Cancelled => "cancelled",
            StatusCode::Unknown => "unknown",
            StatusCode::InvalidArgument => "invalid-argument",
            StatusCode::DeadlineExceeded => "deadline-exceeded",
            StatusCode::NotFound => "not-found",
            StatusCode::AlreadyExists => "already-exists",
            StatusCode::PermissionDenied => "permission-denied",
            StatusCode::ResourceExhausted => "resource-exhausted",
            StatusCode::FailedPrecondition => "failed-precondition",
            StatusCode::Aborted => "aborted",
            StatusCode::OutOfRange => "out-of-range",
            StatusCode::Unimplemented => "unimplemented",
            StatusCode::Internal => "internal",
            StatusCode::Unavailable => "unavailable",
            StatusCode::DataLoss => "data-loss",
            StatusCode::Unauthenticated => "unauthenticated",
        }
    }

    /// Reads the `status` field of an error envelope.
    pub fn from_wire_name(name: &str) -> Option<Self> {
        Some(match name {
            "OK" => StatusCode::Ok,
            "CANCELLED" => StatusCode::Cancelled,
            "UNKNOWN" => StatusCode::Unknown,
            "INVALID_ARGUMENT" => StatusCode::InvalidArgument,
            "DEADLINE_EXCEEDED" => StatusCode::DeadlineExceeded,
            "NOT_FOUND" => StatusCode::NotFound,
            "ALREADY_EXISTS" => StatusCode::AlreadyExists,
            "PERMISSION_DENIED" => StatusCode::PermissionDenied,
            "RESOURCE_EXHAUSTED" => StatusCode::ResourceExhausted,
            "FAILED_PRECONDITION" => StatusCode::FailedPrecondition,
            "ABORTED" => StatusCode::Aborted,
            "OUT_OF_RANGE" => StatusCode::OutOfRange,
            "UNIMPLEMENTED" => StatusCode::Unimplemented,
            "INTERNAL" => StatusCode::Internal,
            "UNAVAILABLE" => StatusCode::Unavailable,
            "DATA_LOSS" => StatusCode::DataLoss,
            "UNAUTHENTICATED" => StatusCode::Unauthenticated,
            _ => return None,
        })
    }

    /// The status an HTTP code maps to, as the Google API guidelines define it.
    pub fn from_http_status(status: u16) -> Self {
        if (200..300).contains(&status) {
            return StatusCode::Ok;
        }

        match status {
            400 => StatusCode::InvalidArgument,
            401 => StatusCode::Unauthenticated,
            403 => StatusCode::PermissionDenied,
            404 => StatusCode::NotFound,
            408 => StatusCode::DeadlineExceeded,
            409 => StatusCode::Aborted,
            412 => StatusCode::FailedPrecondition,
            413 => StatusCode::OutOfRange,
            415 => StatusCode::InvalidArgument,
            429 => StatusCode::ResourceExhausted,
            499 => StatusCode::Cancelled,
            500 => StatusCode::Internal,
            501 => StatusCode::Unimplemented,
            502 => StatusCode::Unavailable,
            503 => StatusCode::Unavailable,
            504 => StatusCode::DeadlineExceeded,
            _ => StatusCode::Unknown,
        }
    }
}

/// What a Firebase backend said when it refused a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleApiError {
    /// The canonical code: the envelope's `status` when it carried one, else the HTTP status.
    pub status: StatusCode,
    /// The backend's message, or a description of the HTTP status when the body carried none.
    pub message: String,
    /// The HTTP status the response arrived with.
    pub http_status: u16,
    /// The envelope's `status` string exactly as it arrived, including codes this SDK does not
    /// know: a new backend status must not silently become `UNKNOWN` in a log.
    pub raw_status: Option<String>,
}

impl GoogleApiError {
    /// Reads a rejected response.
    pub fn from_response(response: &HttpResponse) -> Self {
        Self::from_parts(response.status(), &response.text())
    }

    /// Reads a rejected response whose body has already been read.
    pub fn from_parts(http_status: u16, body: &str) -> Self {
        let payload = parse_envelope(body);
        let raw_status = payload.as_ref().and_then(|error| error.status.clone());
        let status = raw_status
            .as_deref()
            .and_then(StatusCode::from_wire_name)
            .unwrap_or_else(|| StatusCode::from_http_status(http_status));
        let message = payload
            .and_then(|error| error.message)
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| format!("HTTP {http_status}"));

        Self {
            status,
            message,
            http_status,
            raw_status,
        }
    }
}

impl std::fmt::Display for GoogleApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status.wire_name())
    }
}

impl std::error::Error for GoogleApiError {}

#[derive(Debug, Deserialize)]
struct Envelope {
    error: Option<ErrorBody>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

fn parse_envelope(body: &str) -> Option<ErrorBody> {
    // Streaming RPCs (Firestore's `runQuery`, `batchGet`, `runAggregationQuery`) wrap their error
    // in a one-element JSON array; unary RPCs return the bare object. The shape has to be checked
    // first: serde would otherwise read a struct out of an array positionally.
    if body.trim_start().starts_with('[') {
        return serde_json::from_str::<Vec<Envelope>>(body)
            .ok()
            .and_then(|entries| entries.into_iter().find_map(|entry| entry.error));
    }
    serde_json::from_str::<Envelope>(body)
        .ok()
        .and_then(|parsed| parsed.error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_status_beats_the_http_status() {
        // 409 alone would be ABORTED; the payload says otherwise and the payload is right.
        let error = GoogleApiError::from_parts(
            409,
            r#"{"error":{"code":409,"message":"already there","status":"ALREADY_EXISTS"}}"#,
        );

        assert_eq!(error.status, StatusCode::AlreadyExists);
        assert_eq!(error.message, "already there");
        assert_eq!(error.http_status, 409);
        assert_eq!(error.raw_status.as_deref(), Some("ALREADY_EXISTS"));
    }

    #[test]
    fn falls_back_to_the_http_status_without_a_body() {
        let error = GoogleApiError::from_parts(503, "");

        assert_eq!(error.status, StatusCode::Unavailable);
        assert_eq!(error.message, "HTTP 503");
        assert_eq!(error.raw_status, None);
    }

    #[test]
    fn reads_the_error_out_of_a_streamed_array() {
        let error = GoogleApiError::from_parts(
            400,
            r#"[{"error":{"code":400,"message":"bad query","status":"INVALID_ARGUMENT"}}]"#,
        );

        assert_eq!(error.status, StatusCode::InvalidArgument);
        assert_eq!(error.message, "bad query");
    }

    #[test]
    fn an_unknown_status_keeps_its_name() {
        let error = GoogleApiError::from_parts(400, r#"{"error":{"message":"?","status":"BRAND_NEW_CODE"}}"#);

        assert_eq!(error.status, StatusCode::InvalidArgument, "falls back to the HTTP status");
        assert_eq!(error.raw_status.as_deref(), Some("BRAND_NEW_CODE"));
    }

    #[test]
    fn http_statuses_map_the_way_the_api_guidelines_say() {
        assert_eq!(StatusCode::from_http_status(200), StatusCode::Ok);
        assert_eq!(StatusCode::from_http_status(401), StatusCode::Unauthenticated);
        assert_eq!(StatusCode::from_http_status(429), StatusCode::ResourceExhausted);
        assert_eq!(StatusCode::from_http_status(504), StatusCode::DeadlineExceeded);
        assert_eq!(StatusCode::from_http_status(418), StatusCode::Unknown);
    }

    #[test]
    fn names_round_trip() {
        for status in [
            StatusCode::Ok,
            StatusCode::Cancelled,
            StatusCode::Unknown,
            StatusCode::InvalidArgument,
            StatusCode::DeadlineExceeded,
            StatusCode::NotFound,
            StatusCode::AlreadyExists,
            StatusCode::PermissionDenied,
            StatusCode::ResourceExhausted,
            StatusCode::FailedPrecondition,
            StatusCode::Aborted,
            StatusCode::OutOfRange,
            StatusCode::Unimplemented,
            StatusCode::Internal,
            StatusCode::Unavailable,
            StatusCode::DataLoss,
            StatusCode::Unauthenticated,
        ] {
            assert_eq!(StatusCode::from_wire_name(status.wire_name()), Some(status));
            assert!(!status.slug().is_empty());
        }
    }
}
