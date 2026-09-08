use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
#[cfg(not(target_arch = "wasm32"))]
use firebase_core::platform::http::{HttpClient, HttpMethod, HttpRequest, HttpResponse};
#[cfg(not(target_arch = "wasm32"))]
use futures::future::BoxFuture;
#[cfg(not(target_arch = "wasm32"))]
use futures::FutureExt;
#[cfg(not(target_arch = "wasm32"))]
use serde_json::Map;
use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use url::Url;

use crate::error::DatabaseResult;
#[cfg(not(target_arch = "wasm32"))]
use crate::error::{internal_error, invalid_argument, permission_denied, DatabaseError};
use crate::server_value::{contains_server_value, extract_data_ref, resolve_server_values};
use firebase_core::app::FirebaseApp;
#[cfg(not(target_arch = "wasm32"))]
use firebase_core::logger::Logger;
#[cfg(not(target_arch = "wasm32"))]
use firebase_core::platform::credentials::AppCredentials;
#[cfg(not(target_arch = "wasm32"))]
type TokenFetcher = Arc<dyn Fn() -> BoxFuture<'static, DatabaseResult<Option<String>>> + Send + Sync>;

#[cfg_attr(
    all(feature = "wasm-web", target_arch = "wasm32"),
    async_trait(?Send)
)]
#[cfg_attr(not(all(feature = "wasm-web", target_arch = "wasm32")), async_trait)]
pub(crate) trait DatabaseBackend: Send + Sync {
    /// Writes `value` and returns what the server actually stored, with any
    /// [`ServerValue`](crate::server_timestamp) placeholders resolved.
    async fn set(&self, path: &[String], value: Value) -> DatabaseResult<Value>;
    /// Applies a multi-path update and returns the stored value for each path.
    async fn update(
        &self,
        base_path: &[String],
        updates: Vec<(Vec<String>, Value)>,
    ) -> DatabaseResult<Vec<(Vec<String>, Value)>>;
    async fn delete(&self, path: &[String]) -> DatabaseResult<()>;
    async fn get(&self, path: &[String], query: &[(String, String)]) -> DatabaseResult<Value>;
    /// Reads a value together with the version tag needed for a compare-and-set write.
    async fn read_for_update(&self, path: &[String]) -> DatabaseResult<VersionedValue>;
    /// Writes `value` only when the stored data still matches `version`, mirroring the REST
    /// `if-match` / ETag protocol the Realtime Database exposes for transactions.
    async fn compare_and_set(
        &self,
        path: &[String],
        value: Value,
        version: Option<String>,
    ) -> DatabaseResult<CasOutcome>;
}

/// A value plus the opaque version tag that identifies it (an ETag for the REST backend).
#[derive(Clone, Debug)]
pub(crate) struct VersionedValue {
    pub value: Value,
    pub version: Option<String>,
}

/// Outcome of a [`DatabaseBackend::compare_and_set`] attempt.
#[derive(Clone, Debug)]
pub(crate) enum CasOutcome {
    /// The write went through; carries the stored value.
    Committed(Value),
    /// Somebody else wrote first; carries the data as it is now.
    Conflict(VersionedValue),
}

/// Database URLs installed by [`connect_database_emulator`](crate::connect_database_emulator),
/// keyed by app name. Both the REST backend and the realtime transport resolve their endpoint
/// through [`database_url_for`], so pointing an app at an emulator moves every channel at once.
static URL_OVERRIDES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Points `app_name` at `url` until it is overridden again.
pub(crate) fn set_database_url_override(app_name: &str, url: String) {
    URL_OVERRIDES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(app_name.to_string(), url);
}

/// The database URL an app should talk to: an emulator override when one is installed, otherwise
/// the `databaseURL` from the app's options.
pub(crate) fn database_url_for(app: &FirebaseApp) -> Option<String> {
    if let Some(url) = URL_OVERRIDES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(app.name())
    {
        return Some(url.clone());
    }
    app.options().database_url
}

pub(crate) fn select_backend(app: &FirebaseApp) -> Arc<dyn DatabaseBackend> {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(url) = database_url_for(app) {
        let credentials = AppCredentials::for_app(app);
        let auth_credentials = credentials.clone();
        let auth_fetcher: TokenFetcher = Arc::new(move || {
            let credentials = auth_credentials.clone();
            async move {
                credentials
                    .auth_token()
                    .await
                    .map_err(|err| internal_error(format!("failed to obtain auth token: {err}")))
            }
            .boxed()
        });

        let app_check_credentials = credentials;
        let app_check_fetcher: TokenFetcher = Arc::new(move || {
            let credentials = app_check_credentials.clone();
            async move {
                credentials
                    .app_check_token()
                    .await
                    .map_err(|err| internal_error(format!("failed to obtain App Check token: {err}")))
            }
            .boxed()
        });

        match RestBackend::new(url, auth_fetcher, app_check_fetcher) {
            Ok(backend) => return Arc::new(backend),
            Err(err) => {
                LOGGER.warn(format!("Falling back to in-memory Realtime Database backend: {err}"));
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    if let Some(_url) = database_url_for(app) {
        // REST backend not yet supported on wasm; fall back to in-memory.
    }
    Arc::new(InMemoryBackend::default())
}

struct InMemoryBackend {
    data: Mutex<Value>,
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self {
            data: Mutex::new(Value::Object(Default::default())),
        }
    }
}

#[cfg_attr(
    all(feature = "wasm-web", target_arch = "wasm32"),
    async_trait(?Send)
)]
#[cfg_attr(not(all(feature = "wasm-web", target_arch = "wasm32")), async_trait)]
impl DatabaseBackend for InMemoryBackend {
    async fn set(&self, path: &[String], value: Value) -> DatabaseResult<Value> {
        let mut data = self.data.lock().unwrap();
        let resolved = resolve_locally(&data, path, value)?;
        set_at_path(&mut data, path, resolved.clone());
        Ok(resolved)
    }

    async fn update(
        &self,
        _base_path: &[String],
        updates: Vec<(Vec<String>, Value)>,
    ) -> DatabaseResult<Vec<(Vec<String>, Value)>> {
        let mut data = self.data.lock().unwrap();
        let mut stored = Vec::with_capacity(updates.len());
        for (path, value) in updates {
            let resolved = resolve_locally(&data, &path, value)?;
            set_at_path(&mut data, &path, resolved.clone());
            stored.push((path, resolved));
        }
        Ok(stored)
    }

    async fn delete(&self, path: &[String]) -> DatabaseResult<()> {
        let mut data = self.data.lock().unwrap();
        delete_at_path(&mut data, path);
        Ok(())
    }

    async fn get(&self, path: &[String], _query: &[(String, String)]) -> DatabaseResult<Value> {
        let data = self.data.lock().unwrap();
        Ok(get_at_path(&data, path).cloned().unwrap_or(Value::Null))
    }

    async fn read_for_update(&self, path: &[String]) -> DatabaseResult<VersionedValue> {
        let data = self.data.lock().unwrap();
        let value = get_at_path(&data, path).cloned().unwrap_or(Value::Null);
        let version = Some(version_tag(&value));
        Ok(VersionedValue { value, version })
    }

    async fn compare_and_set(
        &self,
        path: &[String],
        value: Value,
        version: Option<String>,
    ) -> DatabaseResult<CasOutcome> {
        let mut data = self.data.lock().unwrap();
        let current = get_at_path(&data, path).cloned().unwrap_or(Value::Null);
        if let Some(expected) = version {
            if expected != version_tag(&current) {
                return Ok(CasOutcome::Conflict(VersionedValue {
                    version: Some(version_tag(&current)),
                    value: current,
                }));
            }
        }
        let resolved = resolve_locally(&data, path, value)?;
        if resolved.is_null() {
            delete_at_path(&mut data, path);
        } else {
            set_at_path(&mut data, path, resolved.clone());
        }
        Ok(CasOutcome::Committed(resolved))
    }
}

/// Resolves `.sv` placeholders against the local snapshot; the in-memory backend has no server to
/// do it for us.
fn resolve_locally(root: &Value, path: &[String], value: Value) -> DatabaseResult<Value> {
    if !contains_server_value(&value) {
        return Ok(value);
    }
    let current = get_at_path(root, path).cloned().unwrap_or(Value::Null);
    resolve_server_values(value, Some(extract_data_ref(&current)))
}

/// Stable tag for a value, standing in for the REST backend's ETag.
fn version_tag(value: &Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    value.to_string().hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[cfg(not(target_arch = "wasm32"))]
struct RestBackend {
    client: HttpClient,
    base_url: Url,
    base_query: Vec<(String, String)>,
    auth_token_fetcher: TokenFetcher,
    app_check_token_fetcher: TokenFetcher,
}

#[cfg(not(target_arch = "wasm32"))]
impl RestBackend {
    fn new(
        raw_url: String,
        auth_token_fetcher: TokenFetcher,
        app_check_token_fetcher: TokenFetcher,
    ) -> DatabaseResult<Self> {
        let mut url =
            Url::parse(&raw_url).map_err(|err| invalid_argument(format!("Invalid database_url '{raw_url}': {err}")))?;

        // Ensure the base URL ends with a slash so joins behave predictably.
        if !url.path().ends_with('/') {
            let mut path = url.path().trim_end_matches('/').to_owned();
            path.push('/');
            url.set_path(&path);
        }

        let base_query: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);

        let client = HttpClient::new();

        Ok(Self {
            client,
            base_url: url,
            base_query,
            auth_token_fetcher,
            app_check_token_fetcher,
        })
    }

    fn url_for_path(&self, path: &[String], query: &[(String, String)]) -> DatabaseResult<Url> {
        let relative = if path.is_empty() {
            ".json".to_string()
        } else {
            format!("{}.json", path.join("/"))
        };
        let mut url = self
            .base_url
            .join(&relative)
            .map_err(|err| internal_error(format!("Failed to compose database URL: {err}")))?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (key, value) in self.base_query.iter().chain(query.iter()) {
                pairs.append_pair(key, value);
            }
        }

        Ok(url)
    }

    fn handle_http_error(&self, status: u16, body: Option<String>) -> DatabaseError {
        let message = body.as_deref().and_then(extract_error_message);

        match status {
            400 | 422 => invalid_argument(message.clone().unwrap_or_else(|| "Invalid data payload".to_string())),
            401 | 403 => permission_denied(message.clone().unwrap_or_else(|| "Permission denied".to_string())),
            _ => internal_error(format!(
                "Database request failed with status {}{}",
                status,
                message.map(|b| format!(": {b}")).unwrap_or_else(String::new)
            )),
        }
    }

    async fn send(&self, request: HttpRequest) -> DatabaseResult<HttpResponse> {
        self.client
            .send(request)
            .await
            .map_err(|err| internal_error(format!("Database request failed: {err}")))
    }

    async fn send_request(
        &self,
        method: HttpMethod,
        path: &[String],
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> DatabaseResult<HttpResponse> {
        let augmented_query = self.query_with_tokens(query).await?;
        let url = self.url_for_path(path, &augmented_query)?;
        let mut request = HttpRequest::new(method, url);
        if let Some(payload) = body {
            request = request
                .json(payload)
                .map_err(|err| internal_error(format!("Failed to encode database payload: {err}")))?;
        }

        self.send(request).await
    }

    fn ensure_success(&self, response: HttpResponse) -> DatabaseResult<HttpResponse> {
        if response.is_success() {
            Ok(response)
        } else {
            Err(self.handle_http_error(response.status(), Some(response.text())))
        }
    }

    async fn query_with_tokens(&self, query: &[(String, String)]) -> DatabaseResult<Vec<(String, String)>> {
        let mut params: Vec<(String, String)> = query.to_vec();

        if !params.iter().any(|(key, _)| key == "auth") {
            if let Some(token) = fetch_token(&self.auth_token_fetcher).await? {
                params.push(("auth".to_string(), token));
            }
        }

        if !params.iter().any(|(key, _)| key == "ac") {
            if let Some(token) = fetch_token(&self.app_check_token_fetcher).await? {
                params.push(("ac".to_string(), token));
            }
        }

        Ok(params)
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_token(fetcher: &TokenFetcher) -> DatabaseResult<Option<String>> {
    (fetcher.as_ref())().await
}

#[cfg_attr(
    all(feature = "wasm-web", target_arch = "wasm32"),
    async_trait(?Send)
)]
#[cfg_attr(not(all(feature = "wasm-web", target_arch = "wasm32")), async_trait)]
#[cfg(not(target_arch = "wasm32"))]
impl DatabaseBackend for RestBackend {
    async fn set(&self, path: &[String], value: Value) -> DatabaseResult<Value> {
        // The server resolves `.sv` placeholders and echoes the stored value back when we do not
        // ask for a silent response, so a write with server values still costs a single request.
        let resolves_server_values = contains_server_value(&value);
        let mut params = Vec::with_capacity(1);
        if !resolves_server_values {
            params.push(("print".to_string(), "silent".to_string()));
        }
        let response = self.send_request(HttpMethod::Put, path, &params, Some(&value)).await?;
        let response = self.ensure_success(response)?;
        if !resolves_server_values {
            return Ok(value);
        }
        response
            .json()
            .map_err(|err| internal_error(format!("Failed to decode database response: {err}")))
    }

    async fn update(
        &self,
        base_path: &[String],
        updates: Vec<(Vec<String>, Value)>,
    ) -> DatabaseResult<Vec<(Vec<String>, Value)>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }

        let mut payload = Map::with_capacity(updates.len());
        let mut keys = Vec::with_capacity(updates.len());
        let mut resolves_server_values = false;
        for (absolute_path, value) in updates {
            if !path_starts_with(&absolute_path, base_path) {
                return Err(internal_error("Database update contained a path outside the reference"));
            }
            let relative = &absolute_path[base_path.len()..];
            if relative.is_empty() {
                return Err(invalid_argument(
                    "Database update path cannot be empty relative to the reference",
                ));
            }
            resolves_server_values |= contains_server_value(&value);
            let key = relative.join("/");
            keys.push((key.clone(), absolute_path, value.clone()));
            payload.insert(key, value);
        }

        let body = Value::Object(payload);
        let mut params = Vec::with_capacity(1);
        if !resolves_server_values {
            params.push(("print".to_string(), "silent".to_string()));
        }
        let response = self
            .send_request(HttpMethod::Patch, base_path, &params, Some(&body))
            .await?;
        let response = self.ensure_success(response)?;

        let stored: Option<Map<String, Value>> = if resolves_server_values {
            response.json().ok()
        } else {
            None
        };

        Ok(keys
            .into_iter()
            .map(|(key, absolute_path, value)| {
                let stored_value = stored.as_ref().and_then(|map| map.get(&key).cloned()).unwrap_or(value);
                (absolute_path, stored_value)
            })
            .collect())
    }

    async fn delete(&self, path: &[String]) -> DatabaseResult<()> {
        let mut params = Vec::with_capacity(1);
        params.push(("print".to_string(), "silent".to_string()));
        let response = self.send_request(HttpMethod::Delete, path, &params, None).await?;
        if response.status() == 404 {
            return Ok(());
        }
        self.ensure_success(response).map(|_| ())
    }

    async fn get(&self, path: &[String], query: &[(String, String)]) -> DatabaseResult<Value> {
        let mut params = Vec::with_capacity(query.len() + 1);
        if !query.iter().any(|(key, _)| key == "format") {
            params.push(("format".to_string(), "export".to_string()));
        }
        params.extend_from_slice(query);

        let response = self.send_request(HttpMethod::Get, path, &params, None).await?;

        if response.status() == 404 {
            return Ok(Value::Null);
        }

        let response = self.ensure_success(response)?;

        response
            .json()
            .map_err(|err| internal_error(format!("Failed to decode database response: {err}")))
    }

    async fn read_for_update(&self, path: &[String]) -> DatabaseResult<VersionedValue> {
        let params = vec![("format".to_string(), "export".to_string())];
        let augmented_query = self.query_with_tokens(&params).await?;
        let url = self.url_for_path(path, &augmented_query)?;
        let response = self
            .send(HttpRequest::get(url).header(FIREBASE_ETAG_HEADER, "true"))
            .await?;

        if response.status() == 404 {
            return Ok(VersionedValue {
                value: Value::Null,
                version: None,
            });
        }

        let version = etag_of(&response);
        let response = self.ensure_success(response)?;
        let value = response
            .json()
            .map_err(|err| internal_error(format!("Failed to decode database response: {err}")))?;
        Ok(VersionedValue { value, version })
    }

    async fn compare_and_set(
        &self,
        path: &[String],
        value: Value,
        version: Option<String>,
    ) -> DatabaseResult<CasOutcome> {
        let augmented_query = self.query_with_tokens(&[]).await?;
        let url = self.url_for_path(path, &augmented_query)?;
        let mut request = HttpRequest::put(url)
            .json(&value)
            .map_err(|err| internal_error(format!("Failed to encode database payload: {err}")))?;
        if let Some(version) = version.as_deref() {
            request = request.header(IF_MATCH_HEADER, version);
        }
        let response = self.send(request).await?;

        if response.status() == 412 {
            // The body carries the data as it is now, so a retry needs no extra read.
            let version = etag_of(&response);
            let value = response.json().unwrap_or(Value::Null);
            return Ok(CasOutcome::Conflict(VersionedValue { value, version }));
        }

        let response = self.ensure_success(response)?;
        let stored = response.json().unwrap_or(value);
        Ok(CasOutcome::Committed(stored))
    }
}

/// Header that asks the Realtime Database REST API to return an ETag for the read value.
#[cfg(not(target_arch = "wasm32"))]
const FIREBASE_ETAG_HEADER: &str = "X-Firebase-ETag";
#[cfg(not(target_arch = "wasm32"))]
const IF_MATCH_HEADER: &str = "if-match";

#[cfg(not(target_arch = "wasm32"))]
fn etag_of(response: &HttpResponse) -> Option<String> {
    response.header("etag").map(|value| value.to_string())
}

fn set_at_path(root: &mut Value, path: &[String], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }

    let mut current = root;
    for segment in &path[..path.len() - 1] {
        if !current.is_object() {
            *current = Value::Object(Default::default());
        }
        let obj = current.as_object_mut().unwrap();
        current = obj.entry(segment).or_insert(Value::Object(Default::default()));
    }

    if !current.is_object() {
        *current = Value::Object(Default::default());
    }
    current
        .as_object_mut()
        .unwrap()
        .insert(path.last().unwrap().clone(), value);
}

fn get_at_path<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(root);
    }
    let mut current = root;
    for segment in path {
        match current {
            Value::Object(obj) => match obj.get(segment) {
                Some(value) => current = value,
                None => return None,
            },
            _ => return None,
        }
    }
    Some(current)
}

#[cfg(not(target_arch = "wasm32"))]
fn path_starts_with(path: &[String], prefix: &[String]) -> bool {
    if prefix.len() > path.len() {
        return false;
    }
    path.iter().zip(prefix.iter()).all(|(left, right)| left == right)
}

fn delete_at_path(root: &mut Value, path: &[String]) {
    if path.is_empty() {
        *root = Value::Null;
        return;
    }

    let mut current = root;
    for segment in &path[..path.len() - 1] {
        match current {
            Value::Object(obj) => match obj.get_mut(segment) {
                Some(next) => {
                    current = next;
                }
                None => return,
            },
            _ => return,
        }
    }

    if let Value::Object(obj) = current {
        obj.remove(path.last().unwrap());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_error_message(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(raw) {
        if let Some(Value::String(message)) = obj.get("error") {
            return Some(message.clone());
        }
    }

    Some(raw.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
static LOGGER: LazyLock<Logger> = LazyLock::new(|| Logger::new("@firebase/database"));

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use futures::FutureExt;
    use httpmock::prelude::*;
    use serde_json::json;

    fn static_token(value: &'static str) -> TokenFetcher {
        Arc::new(move || async move { Ok(Some(value.to_string())) }.boxed())
    }

    fn empty_token() -> TokenFetcher {
        Arc::new(|| async { Ok(None) }.boxed())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_attaches_tokens_to_requests() {
        let server = MockServer::start();

        let get_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/items.json")
                .query_param("auth", "id-token")
                .query_param("ac", "app-check")
                .query_param("format", "export");
            then.status(200).body("null");
        });

        let backend = RestBackend::new(server.url("/"), static_token("id-token"), static_token("app-check"))
            .expect("rest backend");

        backend.get(&["items".to_string()], &[]).await.unwrap();

        get_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_lets_the_server_resolve_server_values() {
        let server = MockServer::start();

        // No `print=silent`: the response body carries the value the server stored.
        let put_mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/stamped.json")
                .json_body(json!({"at": {".sv": "timestamp"}}));
            then.status(200).body(r#"{"at":1700000000000}"#);
        });

        let backend = RestBackend::new(server.url("/"), empty_token(), empty_token()).unwrap();
        let stored = backend
            .set(&["stamped".to_string()], json!({"at": {".sv": "timestamp"}}))
            .await
            .unwrap();

        put_mock.assert();
        assert_eq!(stored, json!({"at": 1_700_000_000_000_u64}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_reads_and_writes_with_etags() {
        let server = MockServer::start();

        let read_mock = server.mock(|when, then| {
            when.method(GET).path("/counter.json").header("X-Firebase-ETag", "true");
            then.status(200).header("ETag", "tag-1").body("7");
        });
        let write_mock = server.mock(|when, then| {
            when.method(PUT).path("/counter.json").header("if-match", "tag-1");
            then.status(200).body("8");
        });

        let backend = RestBackend::new(server.url("/"), empty_token(), empty_token()).unwrap();
        let current = backend.read_for_update(&["counter".to_string()]).await.unwrap();
        assert_eq!(current.value, json!(7));
        assert_eq!(current.version.as_deref(), Some("tag-1"));

        let outcome = backend
            .compare_and_set(&["counter".to_string()], json!(8), current.version)
            .await
            .unwrap();
        assert!(matches!(outcome, CasOutcome::Committed(value) if value == json!(8)));

        read_mock.assert();
        write_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_reports_a_lost_race_as_a_conflict() {
        let server = MockServer::start();

        // 412 carries the data as it is now, so the caller can retry without another read.
        let conflict_mock = server.mock(|when, then| {
            when.method(PUT).path("/counter.json").header("if-match", "stale");
            then.status(412).header("ETag", "tag-2").body("9");
        });

        let backend = RestBackend::new(server.url("/"), empty_token(), empty_token()).unwrap();
        let outcome = backend
            .compare_and_set(&["counter".to_string()], json!(8), Some("stale".to_string()))
            .await
            .unwrap();

        conflict_mock.assert();
        match outcome {
            CasOutcome::Conflict(latest) => {
                assert_eq!(latest.value, json!(9));
                assert_eq!(latest.version.as_deref(), Some("tag-2"));
            }
            CasOutcome::Committed(value) => panic!("expected a conflict, committed {value}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_backend_resolves_server_values_and_detects_conflicts() {
        let backend = InMemoryBackend::default();
        let path = vec!["counter".to_string()];

        backend.set(&path, json!(5)).await.unwrap();
        let stored = backend.set(&path, json!({".sv": {"increment": 3}})).await.unwrap();
        assert_eq!(stored, json!(8.0));

        let current = backend.read_for_update(&path).await.unwrap();
        let outcome = backend.compare_and_set(&path, json!(9), current.version).await.unwrap();
        assert!(matches!(outcome, CasOutcome::Committed(_)));

        let outcome = backend
            .compare_and_set(&path, json!(10), Some("not-the-current-tag".to_string()))
            .await
            .unwrap();
        assert!(matches!(outcome, CasOutcome::Conflict(_)), "a stale tag must not write");
        assert_eq!(backend.get(&path, &[]).await.unwrap(), json!(9));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_skips_missing_tokens() {
        let server = MockServer::start();

        let put_mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/data.json")
                .query_param("print", "silent")
                .json_body(json!({"value": true}));
            then.status(200).body("null");
        });

        let backend = RestBackend::new(server.url("/"), empty_token(), empty_token()).unwrap();

        backend
            .set(&["data".to_string()], json!({"value": true}))
            .await
            .unwrap();

        put_mock.assert();
    }
}
