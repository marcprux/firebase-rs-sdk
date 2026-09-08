//! The FCM registration endpoints.
//!
//! One client for both targets: the transport underneath is
//! [`firebase_core::platform::http`], which speaks `fetch` in the browser and `reqwest` natively,
//! and carries the retry policy FCM asks for (408, 429 and the 5xx family, backed off from five
//! seconds). This crate used to carry a hand-written copy of that for each target.

use firebase_core::platform::http::{HttpClient, HttpRequest, HttpResponse, RetryPolicy};
use firebase_core::util::backoff::BackoffConfig;
use url::Url;

use super::{
    build_body, build_headers, is_retriable_status, map_subscribe_response, map_update_response,
    FcmRegistrationRequest, FcmResponse, FcmUpdateRequest, FCM_API_URL,
};
use crate::constants::{FCM_MAX_RETRIES, FCM_RETRY_BASE_DELAY_MS};
use crate::error::{internal_error, token_unsubscribe_failed, MessagingResult};

#[derive(Clone, Debug)]
pub struct FcmClient {
    http: HttpClient,
    base_url: Url,
}

impl FcmClient {
    #[allow(dead_code)]
    pub fn new() -> MessagingResult<Self> {
        let base = endpoint_from_env().unwrap_or_else(|| FCM_API_URL.to_string());
        Self::with_base_url(&base)
    }

    pub fn with_base_url(base_url: &str) -> MessagingResult<Self> {
        let url =
            Url::parse(base_url).map_err(|err| internal_error(format!("Invalid FCM endpoint '{base_url}': {err}")))?;
        let retry = RetryPolicy::exponential(FCM_MAX_RETRIES)
            .with_backoff(BackoffConfig {
                interval_millis: FCM_RETRY_BASE_DELAY_MS,
                ..BackoffConfig::default()
            })
            .retry_when(is_retriable_status);
        let http = HttpClient::with_user_agent(&format!("firebase-rs-sdk/{}", env!("CARGO_PKG_VERSION")))
            .map_err(|err| internal_error(format!("Failed to build HTTP client: {err}")))?
            .with_retry(retry);
        Ok(Self { http, base_url: url })
    }

    pub async fn register_token(&self, request: &FcmRegistrationRequest<'_>) -> MessagingResult<String> {
        let http_request = HttpRequest::post(self.registration_endpoint(request.project_id)?)
            .headers(build_headers(request.api_key, request.installation_auth_token)?)
            .json(&build_body(&request.subscription))
            .map_err(|err| internal_error(err.to_string()))?;

        let response = self.send(http_request).await?;
        map_subscribe_response(parse_response(&response)?)
    }

    pub async fn update_token(&self, request: &FcmUpdateRequest<'_>) -> MessagingResult<String> {
        let url = self.registration_instance_endpoint(request.registration.project_id, request.registration_token)?;
        let http_request = HttpRequest::patch(url)
            .headers(build_headers(
                request.registration.api_key,
                request.registration.installation_auth_token,
            )?)
            .json(&build_body(&request.registration.subscription))
            .map_err(|err| internal_error(err.to_string()))?;

        let response = self.send(http_request).await?;
        map_update_response(parse_response(&response)?)
    }

    pub async fn delete_token(
        &self,
        project_id: &str,
        api_key: &str,
        installation_auth: &str,
        registration_token: &str,
    ) -> MessagingResult<()> {
        let http_request = HttpRequest::delete(self.registration_instance_endpoint(project_id, registration_token)?)
            .headers(build_headers(api_key, installation_auth)?);

        let response = self.send(http_request).await?;
        let status = response.status();
        let parsed = parse_response(&response)?;

        if let Some(error) = parsed.error {
            return Err(token_unsubscribe_failed(error.message));
        }
        if response.is_success() {
            Ok(())
        } else {
            Err(token_unsubscribe_failed(format!("FCM delete failed with status {status}")))
        }
    }

    async fn send(&self, request: HttpRequest) -> MessagingResult<HttpResponse> {
        self.http
            .send(request)
            .await
            .map_err(|err| internal_error(format!("FCM request failed: {err}")))
    }

    fn registration_endpoint(&self, project_id: &str) -> MessagingResult<Url> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| internal_error("FCM endpoint is not base"))?;
            segments.extend(["projects", project_id, "registrations"]);
        }
        Ok(url)
    }

    fn registration_instance_endpoint(&self, project_id: &str, registration_token: &str) -> MessagingResult<Url> {
        let mut url = self.registration_endpoint(project_id)?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| internal_error("FCM endpoint is not base"))?;
            segments.push(registration_token);
        }
        Ok(url)
    }
}

fn parse_response(response: &HttpResponse) -> MessagingResult<FcmResponse> {
    response
        .json::<FcmResponse>()
        .map_err(|err| internal_error(format!("Failed to parse FCM response (status {}): {err}", response.status())))
}

/// Points the client at an emulator or a test server.
fn endpoint_from_env() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var("FIREBASE_MESSAGING_FCM_ENDPOINT").ok()
    }
}
