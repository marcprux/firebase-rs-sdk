//! Turning a rejected Firestore REST call into a `FirestoreError`.
//!
//! The reading of the response — the canonical status, the message, the array wrapper the
//! streaming RPCs use — is [`firebase_core::util::status`]; what stays here is Firestore's own
//! opinion about which of its error codes each status becomes.

use firebase_core::util::status::{GoogleApiError, StatusCode};

use crate::error::{
    aborted, already_exists, deadline_exceeded, failed_precondition, internal_error, invalid_argument, not_found,
    permission_denied, resource_exhausted, unauthenticated, unavailable, FirestoreError,
};

pub fn map_http_error(status: u16, body: &str) -> FirestoreError {
    if (200..300).contains(&status) {
        return internal_error(format!("Received HTTP {status} while handling error"));
    }

    let error = GoogleApiError::from_parts(status, body);
    let message = error.message;

    match error.status {
        StatusCode::InvalidArgument | StatusCode::OutOfRange => invalid_argument(message),
        StatusCode::FailedPrecondition => failed_precondition(message),
        StatusCode::Aborted => aborted(message),
        StatusCode::Unauthenticated => unauthenticated(message),
        StatusCode::PermissionDenied => permission_denied(message),
        StatusCode::NotFound => not_found(message),
        StatusCode::AlreadyExists => already_exists(message),
        StatusCode::ResourceExhausted => resource_exhausted(message),
        StatusCode::Unavailable => unavailable(message),
        StatusCode::DeadlineExceeded => deadline_exceeded(message),
        // A status the taxonomy does not cover is read from the HTTP status alone: a 4xx is the
        // caller's fault, a 5xx is not.
        StatusCode::Unknown if error.raw_status.is_none() && (400..500).contains(&status) => invalid_argument(message),
        // Firestore has no code of its own for the rest, and a caller cannot act on the difference.
        StatusCode::Cancelled
        | StatusCode::DataLoss
        | StatusCode::Unknown
        | StatusCode::Internal
        | StatusCode::Unimplemented
        | StatusCode::Ok => internal_error(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FirestoreErrorCode;

    #[test]
    fn payload_status_takes_precedence_over_http_status() {
        let body = r#"{"error":{"code":409,"message":"the transaction was aborted","status":"ABORTED"}}"#;
        let err = map_http_error(409, body);
        assert_eq!(err.code, FirestoreErrorCode::Aborted);
        assert!(err.to_string().contains("aborted"));

        let body = r#"{"error":{"code":400,"message":"no entity to update","status":"FAILED_PRECONDITION"}}"#;
        assert_eq!(map_http_error(400, body).code, FirestoreErrorCode::FailedPrecondition);

        let body = r#"{"error":{"code":409,"message":"exists","status":"ALREADY_EXISTS"}}"#;
        assert_eq!(map_http_error(409, body).code, FirestoreErrorCode::AlreadyExists);

        let body = r#"{"error":{"code":400,"message":"bad","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(map_http_error(400, body).code, FirestoreErrorCode::InvalidArgument);
    }

    #[test]
    fn streaming_rpc_errors_arrive_wrapped_in_an_array() {
        let body = r#"[{"error":{"code":400,"message":"The query requires an index. You can create it here: https://console.firebase.google.com/x","status":"FAILED_PRECONDITION"}}]"#;
        let err = map_http_error(400, body);
        assert_eq!(err.code, FirestoreErrorCode::FailedPrecondition);
        assert!(err.to_string().contains("requires an index"));
        assert!(err.to_string().contains("console.firebase.google.com"));
    }

    #[test]
    fn http_status_is_used_without_payload() {
        assert_eq!(map_http_error(409, "").code, FirestoreErrorCode::Aborted);
        assert_eq!(map_http_error(404, "nope").code, FirestoreErrorCode::NotFound);
        assert_eq!(map_http_error(503, "").code, FirestoreErrorCode::Unavailable);
        assert_eq!(map_http_error(403, "").code, FirestoreErrorCode::PermissionDenied);
    }
}
