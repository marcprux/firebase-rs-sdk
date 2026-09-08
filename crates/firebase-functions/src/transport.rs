use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value as JsonValue;

use firebase_core::platform::http::{HttpClient, HttpError, HttpErrorKind, HttpRequest};

use crate::error::{error_for_http_response, internal_error, FunctionsError, FunctionsErrorCode, FunctionsResult};

#[derive(Clone, Debug)]
pub struct CallableRequest {
    pub url: String,
    pub payload: JsonValue,
    pub timeout: Duration,
    pub headers: HashMap<String, String>,
}

impl CallableRequest {
    pub fn new(url: impl Into<String>, payload: JsonValue, timeout: Duration) -> Self {
        Self {
            url: url.into(),
            payload,
            timeout,
            headers: HashMap::new(),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait CallableTransport: Send + Sync {
    async fn invoke(&self, request: CallableRequest) -> FunctionsResult<JsonValue>;
}

pub async fn invoke_callable_async(request: CallableRequest) -> FunctionsResult<JsonValue> {
    callable_transport().invoke(request).await
}

/// The client a callable request goes out on.
///
/// Built per request rather than kept in a static: a pooled client belongs to the runtime it was
/// first used on, and callables are reached from whichever runtime the application is running.
fn client() -> HttpClient {
    HttpClient::new()
}

/// Raw response body of a streaming callable.
#[cfg(not(target_arch = "wasm32"))]
pub type StreamingBody =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;

/// Opens a streaming callable request and returns its body once the response headers confirm
/// success; a non-2xx status is mapped to a `FunctionsError` exactly like a unary call.
#[cfg(not(target_arch = "wasm32"))]
pub async fn open_callable_stream(request: CallableRequest) -> FunctionsResult<StreamingBody> {
    native::open_stream(request).await
}

/// Sends a callable request through the shared HTTP client.
///
/// The unary path is the same code on both targets: `firebase_core::platform::http` speaks
/// `fetch` in the browser and `reqwest` natively, including the request timeout. Only streaming
/// stays native-specific, because it needs the response body before the request completes.
pub fn callable_transport() -> &'static dyn CallableTransport {
    &SharedCallableTransport
}

struct SharedCallableTransport;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl CallableTransport for SharedCallableTransport {
    async fn invoke(&self, request: CallableRequest) -> FunctionsResult<JsonValue> {
        let CallableRequest {
            url,
            payload,
            timeout,
            headers,
        } = request;

        let http_request = HttpRequest::post(url)
            .headers(headers)
            .json(&payload)
            .map_err(map_http_error)?
            .timeout(timeout);

        let response = client().send(http_request).await.map_err(map_http_error)?;

        let status = response.status();
        let body = response.body();
        let (payload, parse_error) = if body.is_empty() {
            (None, None)
        } else {
            match serde_json::from_slice::<JsonValue>(body) {
                Ok(value) => (Some(value), None),
                Err(err) => (None, Some(err)),
            }
        };

        if let Some(error) = error_for_http_response(status, payload.as_ref()) {
            return Err(error);
        }

        if let Some(err) = parse_error {
            return Err(internal_error(format!("Response is not valid JSON object: {err}")));
        }

        if status == 204 {
            return Err(internal_error("Callable response is missing data payload (HTTP 204)"));
        }

        Ok(payload.unwrap_or(JsonValue::Null))
    }
}

/// Maps a transport failure onto the callable error taxonomy, which is what a caller catches.
fn map_http_error(err: HttpError) -> FunctionsError {
    let code = match err.kind() {
        HttpErrorKind::Timeout => FunctionsErrorCode::DeadlineExceeded,
        HttpErrorKind::Network => FunctionsErrorCode::Unavailable,
        HttpErrorKind::InvalidRequest => FunctionsErrorCode::InvalidArgument,
        HttpErrorKind::Decode => FunctionsErrorCode::Internal,
    };
    FunctionsError::new(code, format!("callable request failed: {err}"))
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    //! Streaming callables, the one path the shared HTTP client cannot serve: it reads a response
    //! in full, and a stream has to be consumed while the request is still open.
    use super::{CallableRequest, JsonValue};
    use crate::error::{
        error_for_http_response, internal_error, invalid_argument, FunctionsError, FunctionsErrorCode, FunctionsResult,
    };
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use reqwest::Client;
    use std::collections::HashMap;
    use std::sync::LazyLock;

    fn client() -> &'static Client {
        static CLIENT: LazyLock<Client> =
            LazyLock::new(|| Client::builder().build().expect("Failed to construct reqwest client"));
        &CLIENT
    }

    fn build_headers(headers: &HashMap<String, String>) -> FunctionsResult<HeaderMap> {
        let mut map = HeaderMap::new();
        for (key, value) in headers {
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|err| invalid_argument(format!("invalid header name `{key}`: {err}")))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|err| invalid_argument(format!("invalid header value for `{key}`: {err}")))?;
            map.insert(name, header_value);
        }
        Ok(map)
    }

    fn map_stream_error(err: reqwest::Error) -> FunctionsError {
        let code = if err.is_timeout() {
            FunctionsErrorCode::DeadlineExceeded
        } else if err.is_connect() {
            FunctionsErrorCode::Unavailable
        } else if err.is_decode() {
            FunctionsErrorCode::Internal
        } else {
            FunctionsErrorCode::Unknown
        };
        FunctionsError::new(code, format!("callable stream request failed: {err}"))
    }

    pub(super) async fn open_stream(request: CallableRequest) -> FunctionsResult<super::StreamingBody> {
        let CallableRequest {
            url,
            payload,
            timeout,
            headers,
        } = request;
        let header_map = build_headers(&headers)?;
        let client = client().clone();
        // Only the connection and response headers are bounded by the timeout; the stream may
        // legitimately outlive it (JS applies no timeout to streams at all).
        let response = tokio::time::timeout(timeout, client.post(url).headers(header_map).json(&payload).send())
            .await
            .map_err(|_| {
                FunctionsError::new(
                    FunctionsErrorCode::DeadlineExceeded,
                    "callable stream request timed out before the response started",
                )
            })?
            .map_err(map_stream_error)?;
        if !response.status().is_success() {
            let status = response.status();
            let bytes = response.bytes().await.unwrap_or_default();
            let body = serde_json::from_slice::<JsonValue>(&bytes).ok();
            return Err(error_for_http_response(status.as_u16(), body.as_ref())
                .unwrap_or_else(|| internal_error(format!("callable stream failed with status {status}"))));
        }
        Ok(Box::pin(response.bytes_stream()))
    }
}
