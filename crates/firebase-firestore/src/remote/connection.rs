use std::time::Duration;

use firebase_core::platform::http::{HttpClient, HttpMethod, HttpRequest};
use serde_json::Value as JsonValue;

use crate::error::{internal_error, FirestoreResult};
use crate::model::DatabaseId;

use super::rpc_error::map_http_error;

const FIRESTORE_API_HOST: &str = "https://firestore.googleapis.com";
const FIRESTORE_API_VERSION: &str = "v1";

#[derive(Clone, Debug)]
pub struct Connection {
    client: HttpClient,
    base_url: String,
}

#[derive(Clone, Debug)]
pub struct ConnectionBuilder {
    database_id: DatabaseId,
    client: Option<HttpClient>,
    emulator_host: Option<String>,
}

#[derive(Default, Clone, Debug)]
pub struct RequestContext {
    pub auth_token: Option<String>,
    pub app_check_token: Option<String>,
    pub heartbeat_header: Option<String>,
    pub request_timeout: Option<Duration>,
}

impl ConnectionBuilder {
    pub fn new(database_id: DatabaseId) -> Self {
        Self {
            database_id,
            client: None,
            emulator_host: std::env::var("FIRESTORE_EMULATOR_HOST").ok(),
        }
    }

    pub fn with_client(mut self, client: HttpClient) -> Self {
        self.client = Some(client);
        self
    }

    pub fn with_emulator_host(mut self, host: impl Into<String>) -> Self {
        self.emulator_host = Some(host.into());
        self
    }

    pub fn build(self) -> FirestoreResult<Connection> {
        let client = self.client.unwrap_or_default();
        let base_url = build_base_url(&self.database_id, self.emulator_host.as_deref());
        Ok(Connection { client, base_url })
    }
}

impl Connection {
    pub fn builder(database_id: DatabaseId) -> ConnectionBuilder {
        ConnectionBuilder::new(database_id)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn invoke_json(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<JsonValue>,
        context: &RequestContext,
    ) -> FirestoreResult<JsonValue> {
        self.invoke_json_owned(method, path.to_owned(), body, context.clone())
            .await
    }

    pub async fn invoke_json_optional(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<JsonValue>,
        context: &RequestContext,
    ) -> FirestoreResult<Option<JsonValue>> {
        self.invoke_json_optional_owned(method, path.to_owned(), body, context.clone())
            .await
    }

    async fn invoke_json_owned(
        &self,
        method: HttpMethod,
        path: String,
        body: Option<JsonValue>,
        context: RequestContext,
    ) -> FirestoreResult<JsonValue> {
        let (status, text) = self.invoke(method, &path, body, &context).await?;
        if (200..300).contains(&status) {
            if text.is_empty() {
                Ok(JsonValue::Null)
            } else {
                serde_json::from_str(&text).map_err(|err| internal_error(err.to_string()))
            }
        } else {
            Err(map_http_error(status, &text))
        }
    }

    async fn invoke_json_optional_owned(
        &self,
        method: HttpMethod,
        path: String,
        body: Option<JsonValue>,
        context: RequestContext,
    ) -> FirestoreResult<Option<JsonValue>> {
        let (status, text) = self.invoke(method, &path, body, &context).await?;
        if (200..300).contains(&status) {
            if text.is_empty() {
                Ok(Some(JsonValue::Null))
            } else {
                serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|err| internal_error(err.to_string()))
            }
        } else if status == 404 {
            Ok(None)
        } else {
            Err(map_http_error(status, &text))
        }
    }

    /// Sends one request and reads its body, leaving the status for the caller to interpret:
    /// `getDocument` treats a 404 as "no such document", everything else as an error.
    async fn invoke(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<JsonValue>,
        context: &RequestContext,
    ) -> FirestoreResult<(u16, String)> {
        let request = self.build_request(method, path, body, context)?;
        let response = self
            .client
            .send(request)
            .await
            .map_err(|err| internal_error(err.to_string()))?;
        Ok((response.status(), response.text()))
    }

    fn build_request(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<JsonValue>,
        context: &RequestContext,
    ) -> FirestoreResult<HttpRequest> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let mut request = HttpRequest::new(method, url).header("Content-Type", "application/json");

        if let Some(body) = body {
            request = request.json(&body).map_err(|err| internal_error(err.to_string()))?;
        }
        if let Some(timeout) = context.request_timeout {
            request = request.timeout(timeout);
        }
        if let Some(token) = context.auth_token.as_deref() {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(app_check) = context.app_check_token.as_deref() {
            request = request.header("X-Firebase-AppCheck", app_check);
        }
        if let Some(header) = context.heartbeat_header.as_deref() {
            request = request.header("X-Firebase-Client", header);
        }
        Ok(request)
    }
}

fn build_base_url(database_id: &DatabaseId, emulator_host: Option<&str>) -> String {
    match emulator_host {
        Some(host) => format!(
            "http://{host}/{api_version}/projects/{}/databases/{}",
            database_id.project_id(),
            database_id.database(),
            api_version = FIRESTORE_API_VERSION
        ),
        None => format!(
            "{host}/{api_version}/projects/{}/databases/{}",
            database_id.project_id(),
            database_id.database(),
            host = FIRESTORE_API_HOST,
            api_version = FIRESTORE_API_VERSION
        ),
    }
}
