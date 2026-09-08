use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use firebase_core::platform::http::HttpMethod as Method;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde_json::{Map, Value};

use crate::error::{
    bucket_not_found, canceled, internal_error, object_not_found, quota_exceeded, unauthenticated, unauthorized,
    unauthorized_app, StorageError, StorageResult,
};
use crate::list::{build_list_options, ListOptions};
use crate::location::Location;
use crate::metadata::serde::ObjectMetadata;
use crate::request::info::ErrorHandler;
use crate::request::transport::ResponsePayload;
use crate::service::FirebaseStorageImpl;
use crate::util::encode_uri_component;
use crate::{SetMetadataRequest, UploadMetadata};

use super::{RequestBody, RequestInfo, ResponseHandler};

/// Maps status codes common to every Storage request onto typed errors, keeping the HTTP status
/// and raw body on the result. Mirrors `sharedErrorHandler` in
/// `packages/storage/src/implementation/requests.ts`.
pub fn shared_error_handler(location: &Location) -> ErrorHandler {
    let bucket = location.bucket().to_string();
    let path = location.path().to_string();
    Arc::new(move |payload: ResponsePayload, base: StorageError| {
        let status = payload.status;
        let body = String::from_utf8_lossy(&payload.body).to_string();
        let mapped = match status {
            // This exact message string is the only consistent part of the server's error
            // response that identifies it as an App Check error.
            401 if body.contains("Firebase App Check token is invalid") => unauthorized_app(),
            401 => unauthenticated(),
            402 => quota_exceeded(&bucket),
            403 => unauthorized(&path),
            _ => return base,
        };
        mapped.with_status(status).with_server_response(body)
    })
}

/// Like [`shared_error_handler`], additionally mapping 404 to `object-not-found`. Mirrors
/// `objectErrorHandler` in the JS SDK.
pub fn object_error_handler(location: &Location) -> ErrorHandler {
    let shared = shared_error_handler(location);
    let path = location.path().to_string();
    Arc::new(move |payload: ResponsePayload, base: StorageError| {
        if payload.status == 404 {
            return object_not_found(&path)
                .with_status(404)
                .with_server_response(String::from_utf8_lossy(&payload.body).to_string());
        }
        shared(payload, base)
    })
}

/// Like [`shared_error_handler`], additionally mapping 404 to `bucket-not-found`, which is what
/// a listing of a missing (or unprovisioned) bucket returns.
pub fn bucket_error_handler(location: &Location) -> ErrorHandler {
    let shared = shared_error_handler(location);
    let bucket = location.bucket().to_string();
    Arc::new(move |payload: ResponsePayload, base: StorageError| {
        if payload.status == 404 {
            return bucket_not_found(&bucket)
                .with_status(404)
                .with_server_response(String::from_utf8_lossy(&payload.body).to_string());
        }
        shared(payload, base)
    })
}

pub fn get_metadata_request(storage: &FirebaseStorageImpl, location: &Location) -> RequestInfo<Value> {
    let base_url = format!("{}/v0{}", storage.host(), location.full_server_url());
    let timeout = Duration::from_millis(storage.max_operation_retry_time());

    let handler: ResponseHandler<Value> = Arc::new(|payload| {
        serde_json::from_slice(&payload.body).map_err(|err| internal_error(format!("failed to parse metadata: {err}")))
    });

    RequestInfo::new(base_url, Method::Get, timeout, handler)
        .with_query_param("alt", "json")
        .with_headers(default_json_headers())
        .with_error_handler(object_error_handler(location))
}

pub fn update_metadata_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    metadata: SetMetadataRequest,
) -> RequestInfo<Value> {
    let base_url = format!("{}/v0{}", storage.host(), location.full_server_url());
    let timeout = Duration::from_millis(storage.max_operation_retry_time());

    let handler: ResponseHandler<Value> = Arc::new(|payload| {
        serde_json::from_slice(&payload.body).map_err(|err| internal_error(format!("failed to parse metadata: {err}")))
    });

    RequestInfo::new(base_url, Method::Patch, timeout, handler)
        .with_query_param("alt", "json")
        .with_headers(default_json_headers())
        .with_body(RequestBody::Text(
            serde_json::to_string(&metadata).expect("metadata serialization should never fail"),
        ))
        .with_error_handler(object_error_handler(location))
}

pub fn list_request(storage: &FirebaseStorageImpl, location: &Location, options: &ListOptions) -> RequestInfo<Value> {
    let base_url = format!("{}/v0{}", storage.host(), location.bucket_only_server_url());
    let timeout = Duration::from_millis(storage.max_operation_retry_time());
    let handler: ResponseHandler<Value> = Arc::new(|payload| {
        serde_json::from_slice(&payload.body)
            .map_err(|err| internal_error(format!("failed to parse list response: {err}")))
    });

    let mut request = RequestInfo::new(base_url, Method::Get, timeout, handler)
        .with_query_param("alt", "json")
        .with_headers(default_json_headers())
        .with_error_handler(bucket_error_handler(location));

    for (key, value) in build_list_options(location, options) {
        request = request.with_query_param(key, value);
    }

    request
}

pub fn download_bytes_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    max_download_size_bytes: Option<u64>,
) -> RequestInfo<Vec<u8>> {
    let base_url = format!("{}/v0{}", storage.host(), location.full_server_url());
    let timeout = Duration::from_millis(storage.max_operation_retry_time());

    let handler: ResponseHandler<Vec<u8>> = Arc::new(|payload| Ok(payload.body));

    let mut request =
        RequestInfo::new(base_url, Method::Get, timeout, handler).with_error_handler(object_error_handler(location));
    request.query_params.insert("alt".to_string(), "media".to_string());

    if let Some(limit) = max_download_size_bytes {
        request.headers.insert("Range".to_string(), format!("bytes=0-{limit}"));
        request.success_codes = vec![200, 206];
    }

    request
}

pub fn download_url_request(storage: &FirebaseStorageImpl, location: &Location) -> RequestInfo<Option<String>> {
    let url_part = location.full_server_url();
    let base_url = format!("{}/v0{}", storage.host(), url_part.clone());
    let timeout = Duration::from_millis(storage.max_operation_retry_time());

    let download_base = format!("{}://{}/v0{}", storage.protocol(), storage.host(), url_part);

    let handler: ResponseHandler<Option<String>> = Arc::new(move |payload| {
        let value: Value = serde_json::from_slice(&payload.body)
            .map_err(|err| internal_error(format!("failed to parse download metadata: {err}")))?;

        if let Some(tokens) = value
            .get("downloadTokens")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if let Some(token) = tokens.split(',').find(|segment| !segment.is_empty()) {
                let encoded_token = encode_uri_component(token);
                return Ok(Some(format!("{download_base}?alt=media&token={encoded_token}")));
            }
        }

        Ok(None)
    });

    let mut request =
        RequestInfo::new(base_url, Method::Get, timeout, handler).with_error_handler(object_error_handler(location));
    request.headers = default_json_headers();
    request
}

pub fn delete_object_request(storage: &FirebaseStorageImpl, location: &Location) -> RequestInfo<()> {
    let base_url = format!("{}/v0{}", storage.host(), location.full_server_url());
    let timeout = Duration::from_millis(storage.max_operation_retry_time());

    let handler: ResponseHandler<()> = Arc::new(|_| Ok(()));

    let mut request =
        RequestInfo::new(base_url, Method::Delete, timeout, handler).with_error_handler(object_error_handler(location));
    request.success_codes = vec![200, 204];
    request
}

pub const RESUMABLE_UPLOAD_CHUNK_SIZE: usize = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct ResumableUploadStatus {
    pub current: u64,
    pub total: u64,
    pub finalized: bool,
    pub metadata: Option<ObjectMetadata>,
}

impl ResumableUploadStatus {
    pub fn new(current: u64, total: u64, finalized: bool, metadata: Option<ObjectMetadata>) -> Self {
        Self {
            current,
            total,
            finalized,
            metadata,
        }
    }
}

pub fn multipart_upload_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    data: Vec<u8>,
    metadata: Option<UploadMetadata>,
) -> RequestInfo<ObjectMetadata> {
    let base_url = format!("{}/v0{}", storage.host(), location.bucket_only_server_url());
    let timeout = Duration::from_millis(storage.max_upload_retry_time());

    let total_size = data.len() as u64;
    let (resource, content_type) = build_upload_resource(location, metadata.clone(), total_size);
    let resource_json = serde_json::to_string(&resource).expect("upload metadata serialization should never fail");

    let boundary = generate_boundary();
    let mut body = Vec::with_capacity(resource_json.len() + data.len() + boundary.len() * 4 + 200);
    push_multipart_segment(
        &mut body,
        &boundary,
        "Content-Type: application/json; charset=utf-8",
        resource_json.as_bytes(),
    );
    push_multipart_segment(&mut body, &boundary, &format!("Content-Type: {content_type}"), &data);
    finalize_multipart(&mut body, &boundary);

    let handler: ResponseHandler<ObjectMetadata> = Arc::new(|payload| {
        let value: Value = serde_json::from_slice(&payload.body)
            .map_err(|err| internal_error(format!("failed to parse upload metadata: {err}")))?;
        Ok(ObjectMetadata::from_value(value))
    });

    let mut request = RequestInfo::new(base_url, Method::Post, timeout, handler)
        .with_headers(default_json_headers())
        .with_body(RequestBody::Bytes(body))
        .with_query_param("uploadType", "multipart")
        .with_query_param("name", location.path())
        .with_error_handler(shared_error_handler(location));

    request
        .headers
        .insert("Content-Type".to_string(), format!("multipart/related; boundary={boundary}"));
    request
        .headers
        .insert("X-Goog-Upload-Protocol".to_string(), "multipart".to_string());

    request
}

/// Reads and validates the `X-Goog-Upload-Status` header shared by every resumable response.
///
/// The Web SDK only accepts `active` and `final`; a session that was cancelled (either by this
/// client or because the server dropped it) is reported as `storage/canceled` so callers can tell
/// the two apart.
fn resumable_upload_status(payload: &ResponsePayload) -> StorageResult<String> {
    let status = header_value(&payload.headers, "X-Goog-Upload-Status")
        .ok_or_else(|| internal_error("missing resumable upload status header"))?;
    if status.eq_ignore_ascii_case("cancelled") || status.eq_ignore_ascii_case("canceled") {
        return Err(canceled());
    }
    if !matches!(status.to_ascii_lowercase().as_str(), "active" | "final") {
        return Err(internal_error(format!("unexpected resumable upload status: {status}")));
    }
    Ok(status.to_string())
}

pub fn create_resumable_upload_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    metadata: Option<UploadMetadata>,
    total_size: u64,
) -> RequestInfo<String> {
    let base_url = format!("{}/v0{}", storage.host(), location.bucket_only_server_url());
    let timeout = Duration::from_millis(storage.max_upload_retry_time());

    let (resource, content_type) = build_upload_resource(location, metadata.clone(), total_size);
    let resource_json = serde_json::to_string(&resource).expect("upload metadata serialization should never fail");

    let handler: ResponseHandler<String> = Arc::new(|payload| {
        resumable_upload_status(&payload)?;

        let upload_url = header_value(&payload.headers, "X-Goog-Upload-URL")
            .ok_or_else(|| internal_error("missing resumable upload url"))?;
        Ok(upload_url.to_string())
    });

    let mut request = RequestInfo::new(base_url, Method::Post, timeout, handler)
        .with_query_param("uploadType", "resumable")
        .with_query_param("name", location.path())
        .with_headers(default_json_headers())
        .with_body(RequestBody::Text(resource_json))
        .with_error_handler(shared_error_handler(location));

    request
        .headers
        .insert("X-Goog-Upload-Protocol".to_string(), "resumable".to_string());
    request
        .headers
        .insert("X-Goog-Upload-Command".to_string(), "start".to_string());
    request
        .headers
        .insert("X-Goog-Upload-Header-Content-Length".to_string(), total_size.to_string());
    request
        .headers
        .insert("X-Goog-Upload-Header-Content-Type".to_string(), content_type);

    request
}

pub fn get_resumable_upload_status_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    upload_url: &str,
    total_size: u64,
) -> RequestInfo<ResumableUploadStatus> {
    let timeout = Duration::from_millis(storage.max_upload_retry_time());
    let handler: ResponseHandler<ResumableUploadStatus> = Arc::new(move |payload| {
        let status = resumable_upload_status(&payload)?;
        let received = header_value(&payload.headers, "X-Goog-Upload-Size-Received")
            .ok_or_else(|| internal_error("missing upload size header"))?;
        let current = received
            .parse::<u64>()
            .map_err(|_| internal_error("invalid upload size header"))?;

        Ok(ResumableUploadStatus::new(
            current,
            total_size,
            status.eq_ignore_ascii_case("final"),
            None,
        ))
    });

    let mut request =
        RequestInfo::new(upload_url, Method::Post, timeout, handler).with_error_handler(shared_error_handler(location));
    request
        .headers
        .insert("X-Goog-Upload-Command".to_string(), "query".to_string());
    request
        .headers
        .insert("X-Goog-Upload-Protocol".to_string(), "resumable".to_string());
    request
}

pub fn continue_resumable_upload_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    upload_url: &str,
    start_offset: u64,
    total_size: u64,
    chunk: Vec<u8>,
    finalize: bool,
) -> RequestInfo<ResumableUploadStatus> {
    let timeout = Duration::from_millis(storage.max_upload_retry_time());
    let bytes_to_upload = chunk.len() as u64;
    let empty_chunk = chunk.is_empty();

    let handler: ResponseHandler<ResumableUploadStatus> = Arc::new(move |payload| {
        let status = resumable_upload_status(&payload)?;

        let new_current = (start_offset + bytes_to_upload).min(total_size);

        let metadata = if status.eq_ignore_ascii_case("final") {
            if payload.body.is_empty() {
                return Err(internal_error("final resumable response missing metadata payload"));
            }
            let value: Value = serde_json::from_slice(&payload.body)
                .map_err(|err| internal_error(format!("failed to parse upload metadata: {err}")))?;
            Some(ObjectMetadata::from_value(value))
        } else {
            None
        };

        Ok(ResumableUploadStatus::new(
            new_current,
            total_size,
            status.eq_ignore_ascii_case("final"),
            metadata,
        ))
    });

    let mut request = RequestInfo::new(upload_url, Method::Post, timeout, handler)
        .with_body(RequestBody::Bytes(chunk))
        .with_error_handler(shared_error_handler(location));

    let mut command = String::from("upload");
    if finalize && empty_chunk {
        command = "finalize".to_string();
    } else if finalize {
        command.push_str(", finalize");
    }

    request
        .headers
        .insert("X-Goog-Upload-Protocol".to_string(), "resumable".to_string());
    request.headers.insert("X-Goog-Upload-Command".to_string(), command);
    request
        .headers
        .insert("X-Goog-Upload-Offset".to_string(), start_offset.to_string());
    request
        .headers
        .insert("Content-Type".to_string(), "application/octet-stream".to_string());

    request.success_codes = vec![200, 201, 308];
    request
        .additional_retry_codes
        .extend_from_slice(&[308_u16, 500, 502, 503, 504]);

    request
}

/// Cancels a resumable upload session so the server discards the partially uploaded object.
///
/// The Web SDK simply aborts the in-flight `XMLHttpRequest`; on native targets there is no request
/// to abort between chunks, so the session is cancelled explicitly with the `cancel` upload
/// command. The call is best effort: a session that already finalised answers 400 and a session the
/// server has forgotten answers 404.
pub fn cancel_resumable_upload_request(
    storage: &FirebaseStorageImpl,
    location: &Location,
    upload_url: &str,
) -> RequestInfo<()> {
    let timeout = Duration::from_millis(storage.max_upload_retry_time());
    let handler: ResponseHandler<()> = Arc::new(|_payload| Ok(()));

    let mut request =
        RequestInfo::new(upload_url, Method::Post, timeout, handler).with_error_handler(shared_error_handler(location));
    request
        .headers
        .insert("X-Goog-Upload-Command".to_string(), "cancel".to_string());
    request
        .headers
        .insert("X-Goog-Upload-Protocol".to_string(), "resumable".to_string());
    request
}

fn default_json_headers() -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers
}

fn generate_boundary() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect()
}

fn push_multipart_segment(body: &mut Vec<u8>, boundary: &str, header: &str, data: &[u8]) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(b"\r\n\r\n");
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n");
}

fn finalize_multipart(body: &mut Vec<u8>, boundary: &str) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--");
}

fn build_upload_resource(location: &Location, metadata: Option<UploadMetadata>, total_size: u64) -> (Value, String) {
    let mut map = Map::new();
    map.insert("name".to_string(), Value::String(location.path().to_string()));
    map.insert("fullPath".to_string(), Value::String(location.path().to_string()));
    map.insert("size".to_string(), Value::String(total_size.to_string()));

    let mut content_type = String::from("application/octet-stream");

    if let Some(meta) = metadata {
        if let Some(value) = meta.cache_control {
            map.insert("cacheControl".to_string(), Value::String(value));
        }
        if let Some(value) = meta.content_disposition {
            map.insert("contentDisposition".to_string(), Value::String(value));
        }
        if let Some(value) = meta.content_encoding {
            map.insert("contentEncoding".to_string(), Value::String(value));
        }
        if let Some(value) = meta.content_language {
            map.insert("contentLanguage".to_string(), Value::String(value));
        }
        if let Some(value) = meta.content_type {
            content_type = value.clone();
            map.insert("contentType".to_string(), Value::String(value));
        }
        if let Some(custom) = meta.custom_metadata {
            let mut custom_map = Map::new();
            for (k, v) in custom {
                custom_map.insert(k, Value::String(v));
            }
            map.insert("metadata".to_string(), Value::Object(custom_map));
        }
        if let Some(value) = meta.md5_hash {
            map.insert("md5Hash".to_string(), Value::String(value));
        }
        if let Some(value) = meta.crc32c {
            map.insert("crc32c".to_string(), Value::String(value));
        }
    }

    if !map.contains_key("contentType") {
        map.insert("contentType".to_string(), Value::String(content_type.clone()));
    }

    (Value::Object(map), content_type)
}

fn header_value<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    if let Some(value) = headers.get(name) {
        return Some(value);
    }
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod error_handler_tests {
    use super::*;
    use crate::error::{unknown_error, StorageErrorCode};

    fn payload(status: u16, body: &str) -> ResponsePayload {
        ResponsePayload {
            status,
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn base(status: u16, body: &str) -> StorageError {
        unknown_error().with_status(status).with_server_response(body)
    }

    #[test]
    fn shared_handler_maps_auth_quota_and_permission_statuses() {
        let location = Location::new("my-bucket", "photos/cat.png");
        let handler = shared_error_handler(&location);

        let err = handler(payload(401, "{}"), base(401, "{}"));
        assert_eq!(err.code, StorageErrorCode::Unauthenticated);
        assert_eq!(err.status, Some(401));

        let err = handler(
            payload(401, r#"{"error":{"message":"Firebase App Check token is invalid."}}"#),
            base(401, ""),
        );
        assert_eq!(err.code, StorageErrorCode::UnauthorizedApp);

        let err = handler(payload(402, ""), base(402, ""));
        assert_eq!(err.code, StorageErrorCode::QuotaExceeded);
        assert!(err.to_string().contains("my-bucket"));

        let err = handler(
            payload(403, r#"{"error":{"code":403,"message":"Permission denied."}}"#),
            base(403, ""),
        );
        assert_eq!(err.code, StorageErrorCode::Unauthorized);
        assert!(err.to_string().contains("photos/cat.png"));
        assert_eq!(
            err.server_response.as_deref(),
            Some(r#"{"error":{"code":403,"message":"Permission denied."}}"#)
        );

        // Anything else keeps the transport's base error (status + body preserved).
        let err = handler(payload(500, "boom"), base(500, "boom"));
        assert_eq!(err.code, StorageErrorCode::Unknown);
        assert_eq!(err.status, Some(500));
        assert_eq!(err.server_response.as_deref(), Some("boom"));
    }

    #[test]
    fn object_handler_maps_404_to_object_not_found() {
        let location = Location::new("my-bucket", "photos/cat.png");
        let handler = object_error_handler(&location);
        let err = handler(payload(404, "Not Found"), base(404, "Not Found"));
        assert_eq!(err.code, StorageErrorCode::ObjectNotFound);
        assert_eq!(err.status, Some(404));
        assert_eq!(err.code_str(), "storage/object-not-found");
        assert!(err.to_string().contains("photos/cat.png"));
        // Shared mappings still apply.
        assert_eq!(handler(payload(403, ""), base(403, "")).code, StorageErrorCode::Unauthorized);
    }

    #[test]
    fn bucket_handler_maps_404_to_bucket_not_found() {
        let location = Location::new("missing-bucket", "");
        let handler = bucket_error_handler(&location);
        let err = handler(payload(404, ""), base(404, ""));
        assert_eq!(err.code, StorageErrorCode::BucketNotFound);
        assert!(err.to_string().contains("missing-bucket"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::serde::{SetMetadataRequest, UploadMetadata};
    use crate::request::{RequestBody, ResponsePayload};
    use firebase_core::app::initialize_app;
    use firebase_core::app::{FirebaseAppSettings, FirebaseOptions};

    fn unique_settings() -> FirebaseAppSettings {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("storage-request-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    async fn build_storage() -> FirebaseStorageImpl {
        let options = FirebaseOptions {
            storage_bucket: Some("my-bucket".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        FirebaseStorageImpl::new(app, None, None).unwrap()
    }

    #[tokio::test]
    async fn every_request_carries_an_error_handler() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/cat.png");
        assert!(get_metadata_request(&storage, &location).error_handler.is_some());
        assert!(download_bytes_request(&storage, &location, None)
            .error_handler
            .is_some());
        assert!(download_url_request(&storage, &location).error_handler.is_some());
        assert!(delete_object_request(&storage, &location).error_handler.is_some());
        assert!(list_request(&storage, &location, &ListOptions::default())
            .error_handler
            .is_some());
        assert!(multipart_upload_request(&storage, &location, vec![1], None)
            .error_handler
            .is_some());
    }

    #[tokio::test]
    async fn builds_a_resumable_cancel_request() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/cat.png");
        let request = cancel_resumable_upload_request(&storage, &location, "https://upload.example/session/1");
        assert_eq!(request.url, "https://upload.example/session/1");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.headers.get("X-Goog-Upload-Command").map(String::as_str), Some("cancel"));
        assert_eq!(
            request.headers.get("X-Goog-Upload-Protocol").map(String::as_str),
            Some("resumable")
        );
        assert!(request.error_handler.is_some());
    }

    #[tokio::test]
    async fn a_cancelled_resumable_session_reports_storage_canceled() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/cat.png");
        let request = get_resumable_upload_status_request(&storage, &location, "https://upload.example/session/1", 10);

        let mut headers = HashMap::new();
        headers.insert("x-goog-upload-status".to_string(), "cancelled".to_string());
        let payload = ResponsePayload {
            status: 200,
            headers,
            body: Vec::new(),
        };
        let err = (request.response_handler)(payload).expect_err("a cancelled session must not parse");
        assert_eq!(err.code_str(), "storage/canceled");
    }

    #[tokio::test]
    async fn resumable_status_requires_a_known_status_header() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/cat.png");
        let request = get_resumable_upload_status_request(&storage, &location, "https://upload.example/session/1", 10);

        let mut headers = HashMap::new();
        headers.insert("x-goog-upload-status".to_string(), "active".to_string());
        headers.insert("x-goog-upload-size-received".to_string(), "4".to_string());
        let payload = ResponsePayload {
            status: 200,
            headers,
            body: Vec::new(),
        };
        let status = (request.response_handler)(payload).expect("active session");
        assert_eq!(status.current, 4);
        assert_eq!(status.total, 10);
        assert!(!status.finalized);
    }

    #[tokio::test]
    async fn list_requests_carry_pagination_parameters() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos");
        let options = ListOptions {
            max_results: Some(2),
            page_token: Some("photos/cat.png".into()),
        };
        let request = list_request(&storage, &location, &options);
        assert_eq!(request.query_params.get("prefix").map(String::as_str), Some("photos/"));
        assert_eq!(request.query_params.get("delimiter").map(String::as_str), Some("/"));
        assert_eq!(request.query_params.get("maxResults").map(String::as_str), Some("2"));
        assert_eq!(
            request.query_params.get("pageToken").map(String::as_str),
            Some("photos/cat.png")
        );
    }

    #[tokio::test]
    async fn builds_get_metadata_request() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/cat.png");
        let request = get_metadata_request(&storage, &location);
        assert_eq!(request.url, "firebasestorage.googleapis.com/v0/b/my-bucket/o/photos%2Fcat.png");
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.query_params.get("alt"), Some(&"json".to_string()));
    }

    #[tokio::test]
    async fn builds_update_metadata_request() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "docs/file.txt");
        let mut metadata = SetMetadataRequest::default();
        metadata.content_type = Some("text/plain".into());
        let request = update_metadata_request(&storage, &location, metadata);
        assert_eq!(request.method, Method::Patch);
        assert_eq!(request.url, "firebasestorage.googleapis.com/v0/b/my-bucket/o/docs%2Ffile.txt");
        assert_eq!(request.query_params.get("alt"), Some(&"json".to_string()));
        match &request.body {
            RequestBody::Text(body) => {
                assert!(body.contains("\"contentType\":\"text/plain\""));
            }
            _ => panic!("expected text body"),
        }
    }

    #[tokio::test]
    async fn builds_list_request() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "");
        let mut options = ListOptions::default();
        options.max_results = Some(25);
        options.page_token = Some("token123".into());
        let request = list_request(&storage, &location, &options);
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.query_params.get("delimiter"), Some(&"/".to_string()));
        assert_eq!(request.query_params.get("prefix"), Some(&"".to_string()));
        assert_eq!(request.query_params.get("maxResults"), Some(&"25".to_string()));
        assert_eq!(request.query_params.get("pageToken"), Some(&"token123".to_string()));
    }

    #[tokio::test]
    async fn download_bytes_request_sets_range_header() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "docs/file.txt");
        let request = download_bytes_request(&storage, &location, Some(1024));
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.query_params.get("alt"), Some(&"media".to_string()));
        assert_eq!(request.headers.get("Range"), Some(&"bytes=0-1024".to_string()));
        assert_eq!(request.success_codes, vec![200, 206]);
    }

    #[tokio::test]
    async fn download_url_request_builds_signed_url() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/cat.jpg");
        let request = download_url_request(&storage, &location);

        let payload = ResponsePayload {
            status: 200,
            headers: HashMap::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "downloadTokens": "token123"
            }))
            .unwrap(),
        };

        let handler = request.response_handler.clone();
        let url = handler(payload).unwrap().unwrap();
        assert!(url.contains("token=token123"));
        assert!(url.starts_with("https://"));
        assert!(url.contains("/v0/b/my-bucket"));
    }

    #[tokio::test]
    async fn delete_object_request_accepts_empty_response() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "docs/file.txt");
        let request = delete_object_request(&storage, &location);
        assert_eq!(request.method, Method::Delete);
        assert!(request.success_codes.contains(&204));
    }

    #[tokio::test]
    async fn multipart_upload_request_sets_protocol_and_body() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "photos/dog.jpg");
        let mut metadata = UploadMetadata::new();
        metadata.content_type = Some("image/jpeg".into());
        metadata.md5_hash = Some("abc123".into());
        metadata.insert_custom_metadata("role", "cover");
        let bytes = vec![1_u8, 2, 3, 4, 5];

        let request = multipart_upload_request(&storage, &location, bytes.clone(), Some(metadata));
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.query_params.get("uploadType"), Some(&"multipart".to_string()));
        assert_eq!(request.query_params.get("name"), Some(&"photos/dog.jpg".to_string()));
        let content_type = request.headers.get("Content-Type").unwrap();
        assert!(content_type.starts_with("multipart/related; boundary="));
        assert_eq!(request.headers.get("X-Goog-Upload-Protocol"), Some(&"multipart".to_string()));

        match &request.body {
            RequestBody::Bytes(body) => {
                assert!(body.windows(bytes.len()).any(|window| window == bytes.as_slice()));
            }
            other => panic!("unexpected request body: {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_resumable_upload_request_extracts_upload_url() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "videos/clip.mp4");
        let mut metadata = UploadMetadata::new();
        metadata.content_type = Some("video/mp4".into());
        metadata.crc32c = Some("deadbeef".into());
        let request = create_resumable_upload_request(&storage, &location, Some(metadata), 2048);

        assert_eq!(request.query_params.get("uploadType"), Some(&"resumable".to_string()));

        let mut headers = HashMap::new();
        headers.insert("X-Goog-Upload-Status".to_string(), "active".to_string());
        headers.insert(
            "X-Goog-Upload-URL".to_string(),
            "https://example.com/upload/session".to_string(),
        );
        let payload = ResponsePayload {
            status: 200,
            headers,
            body: Vec::new(),
        };

        let handler = request.response_handler.clone();
        let url = handler(payload).unwrap();
        assert_eq!(url, "https://example.com/upload/session");
    }

    #[tokio::test]
    async fn get_resumable_upload_status_reads_headers() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "videos/clip.mp4");
        let request =
            get_resumable_upload_status_request(&storage, &location, "https://example.com/upload/session", 4096);

        let mut headers = HashMap::new();
        headers.insert("X-Goog-Upload-Status".to_string(), "active".to_string());
        headers.insert("X-Goog-Upload-Size-Received".to_string(), "1024".to_string());
        let payload = ResponsePayload {
            status: 200,
            headers,
            body: Vec::new(),
        };

        let handler = request.response_handler.clone();
        let status = handler(payload).unwrap();
        assert_eq!(status.current, 1024);
        assert_eq!(status.total, 4096);
        assert!(!status.finalized);
    }

    #[tokio::test]
    async fn continue_resumable_upload_handles_final_response() {
        let storage = build_storage().await;
        let location = Location::new("my-bucket", "videos/clip.mp4");
        let chunk = vec![0_u8, 1, 2, 3];
        let request = continue_resumable_upload_request(
            &storage,
            &location,
            "https://example.com/upload/session",
            0,
            4,
            chunk,
            true,
        );

        let mut headers = HashMap::new();
        headers.insert("X-Goog-Upload-Status".to_string(), "final".to_string());
        let payload = ResponsePayload {
            status: 200,
            headers,
            body: serde_json::to_vec(&serde_json::json!({
                "name": "videos/clip.mp4",
                "bucket": "my-bucket"
            }))
            .unwrap(),
        };

        let handler = request.response_handler.clone();
        let status = handler(payload).unwrap();
        assert!(status.finalized);
        assert_eq!(status.current, 4);
        assert!(status.metadata.is_some());
    }
}
