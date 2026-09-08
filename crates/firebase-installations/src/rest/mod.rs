//! The Installations REST endpoints.
//!
//! One client for both targets: the transport underneath is
//! [`firebase_core::platform::http`], which speaks `fetch` in the browser and `reqwest` natively.
//! This crate used to carry a hand-written copy of each.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use firebase_core::platform::http::{HttpClient, HttpRequest, HttpResponse, RetryPolicy};
use firebase_core::util::status::GoogleApiError;
use url::Url;

use crate::config::AppConfig;
use crate::error::{
    internal_error, invalid_argument, request_failed as request_failed_err, InstallationsError, InstallationsResult,
};
use crate::types::InstallationToken;

pub const INSTALLATIONS_API_URL: &str = "https://firebaseinstallations.googleapis.com/v1";
const INTERNAL_AUTH_VERSION: &str = "FIS_v2";
const SDK_VERSION: &str = concat!("w:", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredInstallation {
    pub fid: String,
    pub refresh_token: String,
    pub auth_token: InstallationToken,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateInstallationRequest<'a> {
    fid: &'a str,
    auth_version: &'static str,
    app_id: &'a str,
    sdk_version: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateInstallationResponse {
    refresh_token: String,
    auth_token: GenerateAuthTokenResponse,
    fid: Option<String>,
}

#[derive(Serialize)]
struct GenerateAuthTokenRequest<'a> {
    installation: GenerateAuthTokenInstallation<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateAuthTokenInstallation<'a> {
    app_id: &'a str,
    sdk_version: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateAuthTokenResponse {
    token: String,
    expires_in: String,
}

fn convert_auth_token(response: GenerateAuthTokenResponse) -> InstallationsResult<InstallationToken> {
    let expires_at = SystemTime::now() + parse_expires_in(&response.expires_in)?;
    Ok(InstallationToken {
        token: response.token,
        expires_at,
    })
}

fn parse_expires_in(raw: &str) -> InstallationsResult<Duration> {
    let stripped = raw
        .strip_suffix('s')
        .ok_or_else(|| invalid_argument(format!("Invalid expiresIn format: {}", raw)))?;
    let seconds: u64 = stripped
        .parse()
        .map_err(|err| invalid_argument(format!("Invalid expiresIn value '{}': {}", raw, err)))?;
    Ok(Duration::from_secs(seconds))
}

/// Talks to the Installations backend.
#[derive(Clone, Debug)]
pub struct RestClient {
    http: HttpClient,
    base_url: Url,
}

impl RestClient {
    pub fn new() -> InstallationsResult<Self> {
        let base_url = base_url_from_env().unwrap_or_else(|| INSTALLATIONS_API_URL.to_string());
        Self::with_base_url(&base_url)
    }

    pub fn with_base_url(base_url: &str) -> InstallationsResult<Self> {
        let base_url = Url::parse(base_url)
            .map_err(|err| invalid_argument(format!("Invalid installations endpoint '{}': {}", base_url, err)))?;

        // The backend answers a 5xx often enough that one immediate retry is worth it; anything
        // else it says is final.
        let http = HttpClient::with_user_agent(&format!("firebase-rs-sdk/{}", env!("CARGO_PKG_VERSION")))
            .map_err(|err| internal_error(format!("Failed to build HTTP client: {}", err)))?
            .with_retry(RetryPolicy::retry_once());

        Ok(Self { http, base_url })
    }

    pub async fn register_installation(
        &self,
        config: &AppConfig,
        fid: &str,
    ) -> InstallationsResult<RegisteredInstallation> {
        let request = HttpRequest::post(self.installations_endpoint(config, None)?)
            .headers(base_headers(&config.api_key))
            .json(&CreateInstallationRequest {
                fid,
                auth_version: INTERNAL_AUTH_VERSION,
                app_id: &config.app_id,
                sdk_version: SDK_VERSION,
            })
            .map_err(|err| internal_error(err.to_string()))?;

        let response = self.send(request, "Create Installation").await?;
        if !response.is_success() {
            return Err(request_failed(&response, "Create Installation"));
        }

        let parsed: CreateInstallationResponse = response
            .json()
            .map_err(|err| internal_error(format!("Invalid installation response: {}", err)))?;
        Ok(RegisteredInstallation {
            fid: parsed.fid.unwrap_or_else(|| fid.to_owned()),
            refresh_token: parsed.refresh_token,
            auth_token: convert_auth_token(parsed.auth_token)?,
        })
    }

    pub async fn generate_auth_token(
        &self,
        config: &AppConfig,
        fid: &str,
        refresh_token: &str,
    ) -> InstallationsResult<InstallationToken> {
        let mut url = self.installations_endpoint(config, Some(fid))?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| internal_error("Installations endpoint is not base"))?;
            segments.push("authTokens:generate");
        }

        let request = HttpRequest::post(url)
            .headers(base_headers(&config.api_key))
            .header("authorization", format!("{} {}", INTERNAL_AUTH_VERSION, refresh_token))
            .json(&GenerateAuthTokenRequest {
                installation: GenerateAuthTokenInstallation {
                    app_id: &config.app_id,
                    sdk_version: SDK_VERSION,
                },
            })
            .map_err(|err| internal_error(err.to_string()))?;

        let response = self.send(request, "Generate Auth Token").await?;
        if !response.is_success() {
            return Err(request_failed(&response, "Generate Auth Token"));
        }

        let parsed: GenerateAuthTokenResponse = response
            .json()
            .map_err(|err| internal_error(format!("Invalid auth token response: {}", err)))?;
        convert_auth_token(parsed)
    }

    pub async fn delete_installation(
        &self,
        config: &AppConfig,
        fid: &str,
        refresh_token: &str,
    ) -> InstallationsResult<()> {
        let request = HttpRequest::delete(self.installations_endpoint(config, Some(fid))?)
            .headers(base_headers(&config.api_key))
            .header("authorization", format!("{} {}", INTERNAL_AUTH_VERSION, refresh_token));

        let response = self.send(request, "Delete Installation").await?;
        if response.is_success() {
            Ok(())
        } else {
            Err(request_failed(&response, "Delete Installation"))
        }
    }

    async fn send(&self, request: HttpRequest, request_name: &str) -> InstallationsResult<HttpResponse> {
        self.http
            .send(request)
            .await
            .map_err(|err| internal_error(format!("Network error during {}: {}", request_name, err)))
    }

    fn installations_endpoint(&self, config: &AppConfig, fid: Option<&str>) -> InstallationsResult<Url> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| internal_error("Installations endpoint is not base"))?;
            segments.extend(["projects", config.project_id.as_str(), "installations"]);
            if let Some(fid) = fid {
                segments.push(fid);
            }
        }
        Ok(url)
    }
}

fn base_headers(api_key: &str) -> Vec<(String, String)> {
    vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("x-goog-api-key".to_string(), api_key.to_string()),
    ]
}

/// Turns a rejection into an error carrying the backend's own words, and its status: the token
/// refresh logic keys off `server_code` to tell "this installation is gone" from "try again".
fn request_failed(response: &HttpResponse, request_name: &str) -> InstallationsError {
    let status = response.status();
    let error = GoogleApiError::from_response(response);
    let reported_status = error.raw_status.as_deref().unwrap_or(error.status.wire_name());

    request_failed_err(format!(
        "{} request failed with error \"{} {}: {}\"",
        request_name, status, reported_status, error.message
    ))
    .with_server_code(status)
}

/// Points the client at an emulator or a test server.
fn base_url_from_env() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var("FIREBASE_INSTALLATIONS_API_URL").ok()
    }
}

#[cfg(test)]
mod tests;
