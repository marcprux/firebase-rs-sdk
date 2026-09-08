use crate::error::{internal_error, retry_limit_exceeded, unknown_error, StorageError, StorageResult};
use crate::util::is_url;
#[cfg(not(target_arch = "wasm32"))]
use bytes::Bytes;
use firebase_core::platform::http::{
    HttpClient as SharedHttpClient, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
};
use firebase_core::platform::runtime;
#[cfg(not(target_arch = "wasm32"))]
use firebase_core::platform::runtime::TimeoutError;
#[cfg(not(target_arch = "wasm32"))]
use futures::stream::TryStreamExt;
#[cfg(not(target_arch = "wasm32"))]
use reqwest::{Client, Response};
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Error as IoError, ErrorKind};
#[cfg(not(target_arch = "wasm32"))]
use std::pin::Pin;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use tokio_util::io::StreamReader;
use url::Url;

use super::backoff::{BackoffConfig, BackoffState};
use super::info::{RequestBody, RequestInfo};

#[derive(Clone, Debug)]
pub struct ResponsePayload {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl From<HttpResponse> for ResponsePayload {
    fn from(response: HttpResponse) -> Self {
        let (status, headers, body) = response.into_parts();
        Self { status, headers, body }
    }
}

#[cfg(not(target_arch = "wasm32"))]
type DynByteStream = Pin<Box<dyn futures::stream::Stream<Item = Result<Bytes, IoError>> + Send>>;

#[cfg(not(target_arch = "wasm32"))]
pub type StorageByteStream = StreamReader<DynByteStream, Bytes>;

#[cfg(not(target_arch = "wasm32"))]
pub struct StreamingResponse {
    pub status: reqwest::StatusCode,
    pub headers: HashMap<String, String>,
    pub reader: StorageByteStream,
}

#[derive(Debug)]
pub enum RequestError {
    Network(String),
    Timeout,
    Fatal(StorageError),
}

/// Storage's request runner.
///
/// The requests themselves go through the shared client in `firebase-core`; what lives here is
/// what Storage does on top of it and the shared client deliberately does not: a total-time retry
/// budget (`maxOperationRetryTime`), per-request success codes, and the streaming download the
/// shared client cannot serve because it reads a response in full.
#[derive(Clone)]
pub struct HttpClient {
    client: SharedHttpClient,
    #[cfg(not(target_arch = "wasm32"))]
    streaming_client: Client,
    is_using_emulator: bool,
    backoff: BackoffConfig,
}

impl HttpClient {
    pub fn new(is_using_emulator: bool, backoff: BackoffConfig) -> StorageResult<Self> {
        Ok(Self {
            client: SharedHttpClient::new(),
            #[cfg(not(target_arch = "wasm32"))]
            streaming_client: Client::builder()
                .build()
                .map_err(|err| internal_error(format!("failed to build HTTP client: {err}")))?,
            is_using_emulator,
            backoff,
        })
    }

    pub async fn execute<O>(&self, info: RequestInfo<O>) -> StorageResult<O> {
        let mut backoff = BackoffState::new(self.backoff.clone());

        loop {
            if !backoff.has_time_remaining() {
                return Err(retry_limit_exceeded());
            }

            let delay = backoff.next_delay();
            if delay > Duration::from_millis(0) {
                runtime::sleep(delay).await;
            }

            let result = self.try_once(&info).await;

            match result {
                Ok(payload) => {
                    if info.success_codes.contains(&payload.status) {
                        return (info.response_handler)(payload);
                    }

                    if should_retry(payload.status, &info) && backoff.can_retry() {
                        continue;
                    }

                    return Err(map_failure(payload, &info));
                }
                Err(RequestError::Fatal(err)) => return Err(err),
                Err(RequestError::Timeout) => {
                    return Err(retry_limit_exceeded());
                }
                Err(RequestError::Network(reason)) => {
                    if backoff.can_retry() {
                        continue;
                    }
                    return Err(
                        retry_limit_exceeded().with_server_response(format!("network failure after retries: {reason}"))
                    );
                }
            }
        }
    }

    async fn try_once<O>(&self, info: &RequestInfo<O>) -> Result<ResponsePayload, RequestError> {
        let request = self.build_request(info).map_err(RequestError::Fatal)?;
        let response = self.client.send(request).await.map_err(map_transport_error)?;
        Ok(ResponsePayload::from(response))
    }

    /// The request for one attempt, with the query string and body Storage builds by hand.
    fn build_request<O>(&self, info: &RequestInfo<O>) -> StorageResult<HttpRequest> {
        let mut url = self.prepare_url(&info.url)?;
        if !info.query_params.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &info.query_params {
                pairs.append_pair(k, v);
            }
        }
        drop(url.query_pairs_mut());

        let mut request = HttpRequest::new(info.method, url).timeout(info.timeout);
        for (header, value) in &info.headers {
            request = request.header(header, value);
        }

        match &info.body {
            RequestBody::Bytes(bytes) if !bytes.is_empty() => request = request.body(bytes.clone()),
            RequestBody::Text(text) if !text.is_empty() => request = request.body(text.clone().into_bytes()),
            _ => {}
        }

        Ok(request)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn execute_streaming<O>(&self, info: RequestInfo<O>) -> StorageResult<StreamingResponse> {
        let mut backoff = BackoffState::new(self.backoff.clone());

        loop {
            if !backoff.has_time_remaining() {
                return Err(retry_limit_exceeded());
            }

            let delay = backoff.next_delay();
            if delay > Duration::from_millis(0) {
                runtime::sleep(delay).await;
            }

            match self.try_stream_once(&info).await {
                Ok(response) => return Ok(response),
                Err(RequestError::Fatal(err)) => return Err(err),
                Err(RequestError::Timeout) => {
                    return Err(retry_limit_exceeded());
                }
                Err(RequestError::Network(reason)) => {
                    if backoff.can_retry() {
                        continue;
                    }
                    return Err(
                        retry_limit_exceeded().with_server_response(format!("network failure after retries: {reason}"))
                    );
                }
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn try_stream_once<O>(&self, info: &RequestInfo<O>) -> Result<StreamingResponse, RequestError> {
        let mut url = self.prepare_url(&info.url).map_err(RequestError::Fatal)?;
        if !info.query_params.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &info.query_params {
                pairs.append_pair(k, v);
            }
        }

        let mut request_builder = self
            .streaming_client
            .request(reqwest_method(info.method), url)
            .timeout(info.timeout);

        for (header, value) in &info.headers {
            request_builder = request_builder.header(header, value);
        }

        match &info.body {
            RequestBody::Bytes(bytes) => {
                if !bytes.is_empty() {
                    request_builder = request_builder.body(bytes.clone());
                }
            }
            RequestBody::Text(text) => {
                if !text.is_empty() {
                    request_builder = request_builder.body(text.clone());
                }
            }
            RequestBody::Empty => {}
        }

        let response = send_with_timeout(request_builder, info.timeout).await?;
        let status = response.status();

        if !info.success_codes.contains(&status.as_u16()) {
            let mut headers = HashMap::new();
            for (key, value) in response.headers().iter() {
                if let Ok(val) = value.to_str() {
                    headers.insert(key.as_str().to_owned(), val.to_owned());
                }
            }
            let body = response
                .bytes()
                .await
                .map_err(|err| RequestError::Fatal(internal_error(format!("failed to read response body: {err}"))))?
                .to_vec();
            let payload = ResponsePayload {
                status: status.as_u16(),
                headers,
                body,
            };
            return Err(RequestError::Fatal(map_failure(payload, info)));
        }

        let mut headers = HashMap::new();
        for (key, value) in response.headers().iter() {
            if let Ok(val) = value.to_str() {
                headers.insert(key.as_str().to_owned(), val.to_owned());
            }
        }

        let stream = response
            .bytes_stream()
            .map_err(|err| IoError::new(ErrorKind::Other, err));
        let stream: DynByteStream = Box::pin(stream);
        let reader = StreamReader::new(stream);

        Ok(StreamingResponse {
            status,
            headers,
            reader,
        })
    }

    fn prepare_url(&self, raw: &str) -> StorageResult<Url> {
        if is_url(raw) {
            Url::parse(raw).map_err(|err| internal_error(format!("invalid storage URL: {err}")))
        } else {
            let scheme = if self.is_using_emulator { "http" } else { "https" };
            let formatted = format!("{scheme}://{raw}");
            Url::parse(&formatted).map_err(|err| internal_error(format!("invalid storage URL: {err}")))
        }
    }
}

/// Maps a shared-transport failure onto Storage's retry decision: only a request Storage built
/// wrongly is fatal, everything else is worth another attempt inside the budget.
fn map_transport_error(err: HttpError) -> RequestError {
    match err.kind() {
        HttpErrorKind::Timeout => RequestError::Timeout,
        HttpErrorKind::InvalidRequest => RequestError::Fatal(internal_error(err.to_string())),
        _ => RequestError::Network(err.to_string()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn reqwest_method(method: firebase_core::platform::http::HttpMethod) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).expect("a known HTTP method")
}

#[cfg(not(target_arch = "wasm32"))]
async fn send_with_timeout(builder: reqwest::RequestBuilder, timeout: Duration) -> Result<Response, RequestError> {
    #[cfg(not(target_arch = "wasm32"))]
    let send_future = builder.timeout(timeout).send();
    #[cfg(target_arch = "wasm32")]
    let send_future = builder.send();

    match runtime::with_timeout(send_future, timeout).await {
        Ok(result) => result.map_err(map_reqwest_error),
        Err(TimeoutError) => Err(RequestError::Timeout),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn map_reqwest_error(err: reqwest::Error) -> RequestError {
    if err.is_timeout() {
        RequestError::Timeout
    } else {
        RequestError::Network(err.to_string())
    }
}

fn should_retry<O>(status: u16, info: &RequestInfo<O>) -> bool {
    crate::util::is_retry_status_code(status, &info.additional_retry_codes)
}

fn map_failure<O>(payload: ResponsePayload, info: &RequestInfo<O>) -> StorageError {
    // Same default as the JS SDK: an unexpected status is `storage/unknown`, with the HTTP
    // status and raw body attached; request-specific handlers refine it below.
    let base_error = unknown_error()
        .with_status(payload.status)
        .with_server_response(String::from_utf8_lossy(&payload.body).to_string());

    if let Some(handler) = &info.error_handler {
        handler(payload, base_error)
    } else {
        base_error
    }
}
