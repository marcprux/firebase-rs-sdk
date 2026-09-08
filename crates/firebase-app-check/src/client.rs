use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};

use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::json;

use crate::errors::{AppCheckError, AppCheckResult};
use crate::types::AppCheckToken;
use crate::util::parse_protobuf_duration;
use firebase_core::app::{FirebaseApp, HeartbeatService};

const BASE_ENDPOINT: &str = "https://content-firebaseappcheck.googleapis.com/v1";
const EXCHANGE_RECAPTCHA_V3_METHOD: &str = "exchangeRecaptchaV3Token";
const EXCHANGE_RECAPTCHA_ENTERPRISE_METHOD: &str = "exchangeRecaptchaEnterpriseToken";
const EXCHANGE_DEBUG_TOKEN_METHOD: &str = "exchangeDebugToken";

/// Redirects the exchange endpoint, used by tests to point at a local server.
static BASE_ENDPOINT_OVERRIDE: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

fn base_endpoint() -> String {
    BASE_ENDPOINT_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .unwrap_or_else(|| BASE_ENDPOINT.to_string())
}

type ExchangeFuture = Pin<Box<dyn Future<Output = AppCheckResult<AppCheckToken>> + Send + 'static>>;

type ExchangeHandler = Arc<dyn Fn(ExchangeRequest, Option<Arc<dyn HeartbeatService>>) -> ExchangeFuture + Send + Sync>;

static EXCHANGE_OVERRIDE: LazyLock<Mutex<Option<ExchangeHandler>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Debug)]
pub struct ExchangeRequest {
    pub url: String,
    pub body: serde_json::Value,
}

#[derive(Deserialize)]
struct AppCheckResponse {
    token: String,
    ttl: String,
}

pub async fn exchange_token(
    request: ExchangeRequest,
    heartbeat: Option<Arc<dyn HeartbeatService>>,
) -> AppCheckResult<AppCheckToken> {
    let handler = EXCHANGE_OVERRIDE.lock().unwrap().clone();
    if let Some(handler) = handler {
        return handler(request, heartbeat).await;
    }

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    if let Some(service) = heartbeat {
        if let Some(header) = service
            .heartbeats_header()
            .await
            .map_err(|err| AppCheckError::FetchNetworkError {
                message: err.to_string(),
            })?
        {
            headers.insert(
                "X-Firebase-Client",
                HeaderValue::from_str(&header).map_err(|err| AppCheckError::FetchNetworkError {
                    message: format!("invalid heartbeat header: {err}"),
                })?,
            );
        }
    }

    let client = reqwest::Client::new();
    let response = client
        .post(&request.url)
        .headers(headers)
        .json(&request.body)
        .send()
        .await
        .map_err(|err| AppCheckError::FetchNetworkError {
            message: err.to_string(),
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(AppCheckError::FetchStatusError {
            http_status: status.as_u16(),
        });
    }

    let body: AppCheckResponse = response.json().await.map_err(|err| AppCheckError::FetchParseError {
        message: err.to_string(),
    })?;

    let ttl = parse_protobuf_duration(&body.ttl)?;
    AppCheckToken::with_ttl(body.token, ttl)
}

/// Builds the request that trades a debug token registered in the Firebase console for a real
/// App Check token, mirroring `exchangeDebugToken` in the JS SDK.
///
/// This is the only attestation flow that works outside a browser, which makes it what tests and
/// command-line tools use.
pub fn get_exchange_debug_token_request(app: &FirebaseApp, debug_token: String) -> AppCheckResult<ExchangeRequest> {
    build_exchange_request(app, EXCHANGE_DEBUG_TOKEN_METHOD, "debug_token", debug_token)
}

pub fn get_exchange_recaptcha_v3_request(
    app: &FirebaseApp,
    recaptcha_token: String,
) -> AppCheckResult<ExchangeRequest> {
    build_exchange_request(app, EXCHANGE_RECAPTCHA_V3_METHOD, "recaptcha_v3_token", recaptcha_token)
}

pub fn get_exchange_recaptcha_enterprise_request(
    app: &FirebaseApp,
    recaptcha_token: String,
) -> AppCheckResult<ExchangeRequest> {
    build_exchange_request(
        app,
        EXCHANGE_RECAPTCHA_ENTERPRISE_METHOD,
        "recaptcha_enterprise_token",
        recaptcha_token,
    )
}

fn build_exchange_request(
    app: &FirebaseApp,
    method: &str,
    field: &str,
    token: String,
) -> AppCheckResult<ExchangeRequest> {
    let options = app.options();
    let project_id = options.project_id.ok_or_else(|| AppCheckError::InvalidConfiguration {
        message: "Firebase options must include project_id for App Check".into(),
    })?;
    let app_id = options.app_id.ok_or_else(|| AppCheckError::InvalidConfiguration {
        message: "Firebase options must include app_id for App Check".into(),
    })?;
    let api_key = options.api_key.ok_or_else(|| AppCheckError::InvalidConfiguration {
        message: "Firebase options must include api_key for App Check".into(),
    })?;

    let url = format!("{}/projects/{project_id}/apps/{app_id}:{method}?key={api_key}", base_endpoint());
    let body = json!({ field: token });

    Ok(ExchangeRequest { url, body })
}

#[cfg(all(any(test, feature = "test-support"), not(target_arch = "wasm32")))]
pub fn set_base_endpoint_for_test(endpoint: Option<String>) {
    *BASE_ENDPOINT_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = endpoint;
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_exchange_override<F, Fut>(override_fn: F)
where
    F: Fn(ExchangeRequest, Option<Arc<dyn HeartbeatService>>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AppCheckResult<AppCheckToken>> + Send + 'static,
{
    let handler: ExchangeHandler = Arc::new(move |request, heartbeat| Box::pin(override_fn(request, heartbeat)));
    *EXCHANGE_OVERRIDE.lock().unwrap() = Some(handler);
}

#[cfg(any(test, feature = "test-support"))]
pub fn clear_exchange_override() {
    *EXCHANGE_OVERRIDE.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use firebase_core::app::{FirebaseApp, FirebaseAppConfig, FirebaseOptions};
    use firebase_core::component::ComponentContainer;

    fn test_app(name: &str) -> FirebaseApp {
        FirebaseApp::new(
            FirebaseOptions {
                api_key: Some("api-key".into()),
                app_id: Some("1:1:web:1".into()),
                project_id: Some("demo-project".into()),
                ..Default::default()
            },
            FirebaseAppConfig::new(name, false),
            ComponentContainer::new(name),
        )
    }

    // httpmock and the multi-threaded runtime are native-only dev dependencies.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn exchanges_a_debug_token_over_http() {
        use httpmock::prelude::*;

        // The endpoint override is process-wide, so these tests take the App Check test guard.
        let _guard = crate::test_guard();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/projects/demo-project/apps/1:1:web:1:exchangeDebugToken")
                .query_param("key", "api-key")
                .json_body(serde_json::json!({ "debug_token": "debug-secret" }));
            then.status(200)
                .json_body(serde_json::json!({ "token": "app-check-token", "ttl": "1800s" }));
        });

        set_base_endpoint_for_test(Some(format!("{}/v1", server.base_url())));
        let app = test_app("app-check-debug-exchange");
        let request = get_exchange_debug_token_request(&app, "debug-secret".into()).expect("request");
        let token = exchange_token(request, None).await.expect("exchange");
        set_base_endpoint_for_test(None);

        mock.assert();
        assert_eq!(token.token, "app-check-token");
        // The backend reports the lifetime as a protobuf duration; it must survive the round trip.
        assert!(
            token.expire_time > token.issued_at,
            "the token must carry the expiry derived from the ttl"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn reports_the_http_status_when_the_exchange_is_rejected() {
        use httpmock::prelude::*;

        let _guard = crate::test_guard();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path_contains("exchangeDebugToken");
            then.status(403).json_body(serde_json::json!({
                "error": { "message": "App attestation failed" }
            }));
        });

        set_base_endpoint_for_test(Some(format!("{}/v1", server.base_url())));
        let app = test_app("app-check-debug-denied");
        let request = get_exchange_debug_token_request(&app, "wrong".into()).expect("request");
        let result = exchange_token(request, None).await;
        set_base_endpoint_for_test(None);

        mock.assert();
        // The status is what the providers use to decide how long to back off.
        assert!(
            matches!(result, Err(AppCheckError::FetchStatusError { http_status: 403 })),
            "unexpected result: {result:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builds_the_debug_token_request() {
        let app = test_app("app-check-debug-request");
        let request = get_exchange_debug_token_request(&app, "debug-secret".into()).expect("request");

        assert!(request.url.contains(":exchangeDebugToken"));
        assert!(request.url.contains("/projects/demo-project/apps/1:1:web:1"));
        assert!(request.url.contains("key=api-key"));
        assert_eq!(request.body["debug_token"], "debug-secret");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_missing_project_id() {
        let app = FirebaseApp::new(
            FirebaseOptions {
                api_key: Some("key".into()),
                app_id: Some("app".into()),
                ..Default::default()
            },
            FirebaseAppConfig::new("test", false),
            ComponentContainer::new("test"),
        );

        let result = build_exchange_request(&app, "method", "field", "token".into());
        assert!(matches!(result, Err(AppCheckError::InvalidConfiguration { .. })));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parses_response() {
        let app = FirebaseApp::new(
            FirebaseOptions {
                api_key: Some("key".into()),
                app_id: Some("app".into()),
                project_id: Some("project".into()),
                ..Default::default()
            },
            FirebaseAppConfig::new("test", false),
            ComponentContainer::new("test"),
        );

        let request = get_exchange_recaptcha_v3_request(&app, "captcha".into()).unwrap();
        assert!(request.url.contains("exchangeRecaptchaV3Token"));
        assert_eq!(request.body["recaptcha_v3_token"], "captcha");
    }
}
