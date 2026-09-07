use reqwest::StatusCode;
use serde::Deserialize;

use crate::firestore::error::{
    aborted, already_exists, deadline_exceeded, failed_precondition, internal_error, invalid_argument, not_found,
    permission_denied, resource_exhausted, unauthenticated, unavailable, FirestoreError,
};

#[derive(Debug, Deserialize)]
struct GoogleErrorBody {
    error: Option<GoogleError>,
}

#[derive(Debug, Deserialize)]
struct GoogleError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

pub fn map_http_error(status: StatusCode, body: &str) -> FirestoreError {
    let message =
        extract_message(body).unwrap_or_else(|| status.canonical_reason().unwrap_or("HTTP error").to_string());
    // The canonical gRPC status in the payload is more precise than the HTTP status: ABORTED and
    // FAILED_PRECONDITION both arrive as 409/400 and must stay distinguishable for transactions.
    if let Some(payload) = extract_error_payload(body) {
        if let Some(status_string) = payload.status.as_deref() {
            return map_status_code(status_string, payload.message.as_deref().unwrap_or(&message));
        }
    }
    match status {
        StatusCode::BAD_REQUEST => invalid_argument(message),
        StatusCode::UNAUTHORIZED => unauthenticated(message),
        StatusCode::FORBIDDEN => permission_denied(message),
        StatusCode::NOT_FOUND => not_found(message),
        StatusCode::TOO_MANY_REQUESTS => resource_exhausted(message),
        StatusCode::SERVICE_UNAVAILABLE => unavailable(message),
        StatusCode::GATEWAY_TIMEOUT => deadline_exceeded(message),
        StatusCode::REQUEST_TIMEOUT => deadline_exceeded(message),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => invalid_argument(message),
        StatusCode::PRECONDITION_FAILED => failed_precondition(message),
        StatusCode::INTERNAL_SERVER_ERROR => internal_error(message),
        StatusCode::BAD_GATEWAY => unavailable(message),
        StatusCode::OK => internal_error("Received HTTP 200 while handling error"),
        other => map_status_from_payload(other, &message, body),
    }
}

fn map_status_from_payload(status: StatusCode, fallback_message: &str, body: &str) -> FirestoreError {
    if let Some(payload) = extract_error_payload(body) {
        if let Some(status_string) = payload.status.as_deref() {
            return map_status_code(status_string, payload.message.as_deref().unwrap_or(fallback_message));
        }
    }

    match status {
        StatusCode::CONFLICT => aborted(fallback_message.to_string()),
        StatusCode::GATEWAY_TIMEOUT => deadline_exceeded(fallback_message.to_string()),
        StatusCode::REQUEST_TIMEOUT => deadline_exceeded(fallback_message.to_string()),
        StatusCode::PAYLOAD_TOO_LARGE => invalid_argument(fallback_message.to_string()),
        status if status.is_client_error() => invalid_argument(fallback_message.to_string()),
        status if status.is_server_error() => internal_error(fallback_message.to_string()),
        _ => internal_error(fallback_message.to_string()),
    }
}

fn map_status_code(status: &str, message: &str) -> FirestoreError {
    match status {
        "INVALID_ARGUMENT" => invalid_argument(message.to_string()),
        "FAILED_PRECONDITION" => failed_precondition(message.to_string()),
        "ABORTED" => aborted(message.to_string()),
        "OUT_OF_RANGE" => invalid_argument(message.to_string()),
        "UNAUTHENTICATED" => unauthenticated(message.to_string()),
        "PERMISSION_DENIED" => permission_denied(message.to_string()),
        "NOT_FOUND" => not_found(message.to_string()),
        "ALREADY_EXISTS" => already_exists(message.to_string()),
        "RESOURCE_EXHAUSTED" => resource_exhausted(message.to_string()),
        "CANCELLED" => internal_error(message.to_string()),
        "DATA_LOSS" => internal_error(message.to_string()),
        "UNKNOWN" => internal_error(message.to_string()),
        "INTERNAL" => internal_error(message.to_string()),
        "UNAVAILABLE" => unavailable(message.to_string()),
        "DEADLINE_EXCEEDED" => deadline_exceeded(message.to_string()),
        other => internal_error(format!("Unhandled Firestore error status: {other}")),
    }
}

fn extract_message(body: &str) -> Option<String> {
    extract_error_payload(body)
        .and_then(|payload| payload.message)
        .filter(|message| !message.is_empty())
}

fn extract_error_payload(body: &str) -> Option<GoogleError> {
    serde_json::from_str::<GoogleErrorBody>(body)
        .ok()
        .and_then(|parsed| parsed.error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firestore::error::FirestoreErrorCode;

    #[test]
    fn payload_status_takes_precedence_over_http_status() {
        let body = r#"{"error":{"code":409,"message":"the transaction was aborted","status":"ABORTED"}}"#;
        let err = map_http_error(StatusCode::CONFLICT, body);
        assert_eq!(err.code, FirestoreErrorCode::Aborted);
        assert!(err.to_string().contains("aborted"));

        let body = r#"{"error":{"code":400,"message":"no entity to update","status":"FAILED_PRECONDITION"}}"#;
        assert_eq!(
            map_http_error(StatusCode::BAD_REQUEST, body).code,
            FirestoreErrorCode::FailedPrecondition
        );

        let body = r#"{"error":{"code":409,"message":"exists","status":"ALREADY_EXISTS"}}"#;
        assert_eq!(
            map_http_error(StatusCode::CONFLICT, body).code,
            FirestoreErrorCode::AlreadyExists
        );

        let body = r#"{"error":{"code":400,"message":"bad","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(
            map_http_error(StatusCode::BAD_REQUEST, body).code,
            FirestoreErrorCode::InvalidArgument
        );
    }

    #[test]
    fn http_status_is_used_without_payload() {
        assert_eq!(map_http_error(StatusCode::CONFLICT, "").code, FirestoreErrorCode::Aborted);
        assert_eq!(map_http_error(StatusCode::NOT_FOUND, "nope").code, FirestoreErrorCode::NotFound);
        assert_eq!(
            map_http_error(StatusCode::SERVICE_UNAVAILABLE, "").code,
            FirestoreErrorCode::Unavailable
        );
        assert_eq!(
            map_http_error(StatusCode::FORBIDDEN, "").code,
            FirestoreErrorCode::PermissionDenied
        );
    }
}
