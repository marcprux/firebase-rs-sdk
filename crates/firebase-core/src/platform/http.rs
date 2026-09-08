//! The one HTTP client the products share.
//!
//! Every Firebase backend is reached the same way — a JSON request with a few headers, a status
//! code to check, a body to decode, and a retry when the server says it is having a bad minute.
//! Each product used to write that itself, twice: once with `reqwest` for native and once against
//! `web_sys::fetch` for the browser, with its own header handling, its own error mapping and its
//! own idea of what to retry. This module is that code, written once.
//!
//! `reqwest` covers both targets (it speaks `fetch` when compiled to wasm), so there is a single
//! implementation here rather than a pair behind `cfg`. The one thing it does not carry across is
//! the request timeout, which is applied through [`platform::runtime::with_timeout`] instead:
//! `tokio` natively, a browser timer in wasm.
//!
//! [`platform::runtime::with_timeout`]: crate::platform::runtime::with_timeout

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::platform::runtime;
use crate::util::backoff::{calculate_backoff_with, BackoffConfig};

/// The HTTP verbs the Firebase backends use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What went wrong before a status code could be read.
///
/// A response the server actually sent is never an error here, however unhappy its status: that is
/// [`HttpResponse`], and each product decides what its own status codes mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpErrorKind {
    /// The request did not finish inside its timeout.
    Timeout,
    /// The request never reached the server, or the connection died mid-response.
    Network,
    /// The request could not be built or serialised — a bad URL, an invalid header, a payload that
    /// is not JSON. A caller bug, not a transport failure.
    InvalidRequest,
    /// The response body was not what the caller asked to decode.
    Decode,
}

/// A transport failure.
#[derive(Clone, Debug)]
pub struct HttpError {
    kind: HttpErrorKind,
    message: String,
}

impl HttpError {
    pub fn new(kind: HttpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> HttpErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_timeout(&self) -> bool {
        self.kind == HttpErrorKind::Timeout
    }

    pub fn is_network(&self) -> bool {
        self.kind == HttpErrorKind::Network
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpError {}

/// One outgoing request.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Get, url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Post, url)
    }

    pub fn put(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Put, url)
    }

    pub fn patch(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Patch, url)
    }

    pub fn delete(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Delete, url)
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds several headers at once, in the order given.
    pub fn headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.headers
            .extend(headers.into_iter().map(|(name, value)| (name.into(), value.into())));
        self
    }

    /// Serialises `payload` as the body and sets `Content-Type: application/json`.
    pub fn json<T: Serialize>(mut self, payload: &T) -> Result<Self, HttpError> {
        let body = serde_json::to_vec(payload)
            .map_err(|err| HttpError::new(HttpErrorKind::InvalidRequest, format!("failed to encode request: {err}")))?;
        self.body = Some(body);
        Ok(self.header(CONTENT_TYPE, JSON_CONTENT_TYPE))
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// One response, read in full.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16, headers: HashMap<String, String>, body: Vec<u8>) -> Self {
        Self { status, headers, body }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn is_server_error(&self) -> bool {
        (500..600).contains(&self.status)
    }

    /// A response header, looked up case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Every response header, keyed by its lower-cased name.
    pub fn headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    /// Takes the response apart into its status, headers and body.
    pub fn into_parts(self) -> (u16, HashMap<String, String>, Vec<u8>) {
        (self.status, self.headers, self.body)
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// The body as text, with invalid UTF-8 replaced rather than rejected: this is used for error
    /// messages, where refusing to show a malformed body helps nobody.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Decodes the body as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HttpError> {
        serde_json::from_slice(&self.body)
            .map_err(|err| HttpError::new(HttpErrorKind::Decode, format!("invalid response body: {err}")))
    }
}

/// When to send a request again.
///
/// Retries cover transport failures and, by default, 5xx responses — never a 4xx, which will fail
/// the same way however many times it is sent. Products that know better about their own backend
/// (FCM also retries 408 and 429) narrow or widen that with [`retry_when`](Self::retry_when).
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_retries: u32,
    /// How long to wait between attempts; `None` retries immediately, which is what a single
    /// "the server hiccupped" retry wants.
    pub backoff: Option<BackoffConfig>,
    /// Whether a response with this status is worth sending again.
    pub retry_status: fn(u16) -> bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

fn is_server_error(status: u16) -> bool {
    (500..600).contains(&status)
}

impl RetryPolicy {
    /// Send the request once and report whatever comes back.
    pub const fn none() -> Self {
        Self {
            max_retries: 0,
            backoff: None,
            retry_status: is_server_error,
        }
    }

    /// One immediate retry, for endpoints where a 5xx is usually a blip (Installations does this).
    pub const fn retry_once() -> Self {
        Self {
            max_retries: 1,
            backoff: None,
            retry_status: is_server_error,
        }
    }

    /// Several retries with the SDK's standard jittered exponential backoff.
    pub fn exponential(max_retries: u32) -> Self {
        Self {
            max_retries,
            backoff: Some(BackoffConfig::default()),
            retry_status: is_server_error,
        }
    }

    /// Uses a different first interval or growth factor.
    pub fn with_backoff(mut self, config: BackoffConfig) -> Self {
        self.backoff = Some(config);
        self
    }

    /// Replaces the rule deciding which statuses are worth retrying.
    pub fn retry_when(mut self, retry_status: fn(u16) -> bool) -> Self {
        self.retry_status = retry_status;
        self
    }
}

const CONTENT_TYPE: &str = "content-type";
const JSON_CONTENT_TYPE: &str = "application/json";

/// The shared HTTP client.
///
/// Cheap to clone — clones share one connection pool — so a product can keep one per service
/// instance.
#[derive(Clone, Debug)]
pub struct HttpClient {
    inner: reqwest::Client,
    retry: RetryPolicy,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    /// A client with no retries.
    ///
    /// A client owns a connection pool, and that pool belongs to the async runtime it was first
    /// used on — a client built under one `tokio` runtime and used under another fails with
    /// "dispatch task is gone". So there is deliberately no process-wide client here: a service
    /// builds one and keeps it, which ties the pool to that service's lifetime.
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
            retry: RetryPolicy::none(),
        }
    }

    /// A client that identifies itself with `user_agent`.
    ///
    /// Browsers forbid a page from setting `User-Agent`, so this is a no-op in wasm builds.
    pub fn with_user_agent(user_agent: &str) -> Result<Self, HttpError> {
        Ok(Self {
            inner: build_client(user_agent)?,
            retry: RetryPolicy::none(),
        })
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    /// Sends a request, retrying as the policy allows, and reads the response in full.
    pub async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut attempt = 0;
        loop {
            let outcome = self.send_once(&request).await;

            let retryable = match &outcome {
                Ok(response) => (self.retry.retry_status)(response.status()),
                // A malformed request fails identically every time; only transport trouble is
                // worth another attempt.
                Err(error) => error.kind != HttpErrorKind::InvalidRequest,
            };

            if !retryable || attempt >= self.retry.max_retries {
                return outcome;
            }

            if let Some(backoff) = self.retry.backoff {
                runtime::sleep(Duration::from_millis(calculate_backoff_with(attempt, backoff))).await;
            }
            attempt += 1;
        }
    }

    async fn send_once(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut builder = self
            .inner
            .request(method_for(request.method), &request.url)
            .headers(header_map(&request.headers)?);

        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        let send = builder.send();
        let response = match request.timeout {
            // reqwest has no timeout on wasm, so the timeout is applied here for both targets.
            Some(timeout) => runtime::with_timeout(send, timeout)
                .await
                .map_err(|_| {
                    HttpError::new(
                        HttpErrorKind::Timeout,
                        format!("{} {} timed out after {:?}", request.method, request.url, timeout),
                    )
                })?
                .map_err(map_reqwest_error)?,
            None => send.await.map_err(map_reqwest_error)?,
        };

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = response.bytes().await.map_err(map_reqwest_error)?.to_vec();

        Ok(HttpResponse::new(status, headers, body))
    }
}

fn method_for(method: HttpMethod) -> reqwest::Method {
    match method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Patch => reqwest::Method::PATCH,
        HttpMethod::Delete => reqwest::Method::DELETE,
    }
}

fn header_map(headers: &[(String, String)]) -> Result<reqwest::header::HeaderMap, HttpError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    let mut map = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|err| HttpError::new(HttpErrorKind::InvalidRequest, format!("invalid header `{name}`: {err}")))?;
        let value = HeaderValue::from_str(value).map_err(|err| {
            HttpError::new(
                HttpErrorKind::InvalidRequest,
                format!("invalid value for header `{name}`: {err}"),
            )
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

fn map_reqwest_error(err: reqwest::Error) -> HttpError {
    if err.is_timeout() {
        return HttpError::new(HttpErrorKind::Timeout, format!("request timed out: {err}"));
    }
    if err.is_decode() {
        return HttpError::new(HttpErrorKind::Decode, format!("failed to read response: {err}"));
    }
    if err.is_builder() {
        return HttpError::new(HttpErrorKind::InvalidRequest, format!("malformed request: {err}"));
    }
    HttpError::new(HttpErrorKind::Network, format!("request failed: {err}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn build_client(user_agent: &str) -> Result<reqwest::Client, HttpError> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .build()
        .map_err(|err| HttpError::new(HttpErrorKind::InvalidRequest, format!("failed to build HTTP client: {err}")))
}

#[cfg(target_arch = "wasm32")]
fn build_client(_user_agent: &str) -> Result<reqwest::Client, HttpError> {
    // `User-Agent` is a forbidden header name in a browser: the request carries the browser's own.
    Ok(reqwest::Client::new())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde::Deserialize;

    #[derive(Deserialize, PartialEq, Debug)]
    struct Payload {
        name: String,
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sends_a_json_request_and_decodes_the_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/things")
                .header("x-goog-api-key", "key")
                .json_body(serde_json::json!({ "id": 7 }));
            then.status(200).json_body(serde_json::json!({ "name": "thing" }));
        });

        let response = HttpClient::new()
            .send(
                HttpRequest::post(server.url("/things"))
                    .header("x-goog-api-key", "key")
                    .json(&serde_json::json!({ "id": 7 }))
                    .unwrap(),
            )
            .await
            .expect("request");

        mock.assert();
        assert!(response.is_success());
        assert_eq!(
            response.json::<Payload>().unwrap(),
            Payload {
                name: "thing".to_string()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejection_is_a_response_not_an_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/nope");
            then.status(403).body("denied");
        });

        let response = HttpClient::new()
            .send(HttpRequest::get(server.url("/nope")))
            .await
            .expect("request");

        assert_eq!(response.status(), 403);
        assert!(!response.is_success());
        assert_eq!(response.text(), "denied");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retries_a_server_error_and_keeps_the_last_response() {
        let server = MockServer::start();
        let failing = server.mock(|when, then| {
            when.method(GET).path("/flaky");
            then.status(503);
        });

        let response = HttpClient::new()
            .with_retry(RetryPolicy::retry_once())
            .send(HttpRequest::get(server.url("/flaky")))
            .await
            .expect("request");

        assert_eq!(response.status(), 503);
        assert_eq!(failing.hits(), 2, "the request should have been sent twice");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn does_not_retry_a_client_error() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/gone");
            then.status(404);
        });

        HttpClient::new()
            .with_retry(RetryPolicy::exponential(3))
            .send(HttpRequest::get(server.url("/gone")))
            .await
            .expect("request");

        assert_eq!(mock.hits(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_a_timeout_as_a_timeout() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/slow");
            then.status(200).delay(Duration::from_millis(600));
        });

        let error = HttpClient::new()
            .send(HttpRequest::get(server.url("/slow")).timeout(Duration::from_millis(50)))
            .await
            .expect_err("should time out");

        assert_eq!(error.kind(), HttpErrorKind::Timeout);
        assert!(error.is_timeout());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_host_is_a_network_error() {
        // Port 0 is never listening.
        let error = HttpClient::new()
            .send(HttpRequest::get("http://127.0.0.1:1/nothing"))
            .await
            .expect_err("should fail");

        assert_eq!(error.kind(), HttpErrorKind::Network);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_header_is_not_retried() {
        let error = HttpClient::new()
            .with_retry(RetryPolicy::exponential(5))
            .send(HttpRequest::get("http://127.0.0.1:1/nothing").header("bad name", "value"))
            .await
            .expect_err("should fail");

        assert_eq!(error.kind(), HttpErrorKind::InvalidRequest);
    }
}
