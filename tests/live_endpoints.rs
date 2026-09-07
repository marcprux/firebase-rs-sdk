//! Live integration tests that exercise the real Firebase backends.
//!
//! These tests are `#[ignore]`d so that a plain `cargo test` stays offline. Run them with:
//!
//! ```bash
//! cargo test --test live_endpoints -- --ignored --nocapture
//! ```
//!
//! # Credentials
//!
//! Credentials are never committed. They are resolved in this order (later sources override
//! earlier ones):
//!
//! 1. A `google-services.json` file (the Android client config downloaded from the Firebase
//!    console). Location: `$FIREBASE_GOOGLE_SERVICES_FILE`, else `./google-services.json` at the
//!    crate root. Alternatively the raw JSON can be provided in `$FIREBASE_GOOGLE_SERVICES_JSON`,
//!    which is how GitHub Actions injects the secret.
//! 2. A `.env.firebase` dot file at the crate root with `KEY=VALUE` lines.
//! 3. Environment variables:
//!    - `FIREBASE_API_KEY` (required)
//!    - `FIREBASE_PROJECT_ID` (required)
//!    - `FIREBASE_APP_ID` (required)
//!    - `FIREBASE_PROJECT_NUMBER` (optional, messaging sender id)
//!    - `FIREBASE_STORAGE_BUCKET` (optional)
//!    - `FIREBASE_DATABASE_URL` (optional)
//!    - `FIREBASE_TEST_CALLABLE` (optional, name of a deployed callable function to invoke)
//!
//! When no credentials are found the tests print a notice and pass, unless
//! `FIREBASE_LIVE_TESTS_REQUIRED=1` is set (CI does this on the main branch) in which case they
//! fail loudly.
//!
//! # Emulators
//!
//! When the standard emulator variables are set (`firebase emulators:exec` exports the first
//! three; `scripts/emulator_test.sh` exports the last one), the matching service is routed to
//! the Firebase Local Emulator Suite instead of the online project, and no credentials are
//! needed at all: a `demo-*` project id and a fake API key are synthesized.
//!
//! - `FIREBASE_AUTH_EMULATOR_HOST`
//! - `FIRESTORE_EMULATOR_HOST` (read by the crate itself)
//! - `FIREBASE_STORAGE_EMULATOR_HOST`
//! - `FIREBASE_FUNCTIONS_EMULATOR_HOST`
//!
//! Installations and Remote Config have no emulator; their tests need online credentials and
//! skip otherwise. Run everything with `scripts/emulator_test.sh`.
//!
//! # Project provisioning
//!
//! Firebase projects don't have every product enabled. When a backend reports that a product is
//! not enabled (Auth never initialised, Firestore API disabled, no Storage bucket, …), the
//! affected test prints a `SKIP:` line explaining what to enable in the console and passes. The
//! `live_project_probe` test prints a one-screen summary of what the current credentials can
//! reach, which is the first thing to run when a test is unexpectedly skipped.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use firebase_rs_sdk::app::{delete_app, initialize_app, FirebaseApp, FirebaseAppSettings, FirebaseOptions};
use firebase_rs_sdk::auth::{auth_for_app, register_auth_component, AuthError, AuthErrorCode, User};
use firebase_rs_sdk::firestore::{
    get_firestore, FieldPath, FilterOperator, Firestore, FirestoreClient, FirestoreErrorCode, FirestoreValue, ValueKind,
};
use firebase_rs_sdk::functions::error::FunctionsErrorCode;
use firebase_rs_sdk::functions::{get_functions, register_functions_component};
use firebase_rs_sdk::installations::{delete_installations, get_installations};
use firebase_rs_sdk::remote_config::{get_remote_config, FetchStatus, RemoteConfigValueSource};
use firebase_rs_sdk::storage::{get_storage_for_app, StorageErrorCode, StringFormat};

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// Everything the tests need to know about the target Firebase project.
#[derive(Clone, Debug, Default)]
struct LiveConfig {
    api_key: String,
    project_id: String,
    app_id: String,
    project_number: Option<String>,
    storage_bucket: Option<String>,
    database_url: Option<String>,
    test_callable: Option<String>,
    /// True when real project credentials were found (as opposed to a synthesized emulator
    /// configuration).
    online: bool,
    emulators: EmulatorHosts,
}

/// `host:port` of each running emulator, taken from the standard environment variables.
#[derive(Clone, Debug, Default)]
struct EmulatorHosts {
    auth: Option<String>,
    firestore: Option<String>,
    storage: Option<String>,
    functions: Option<String>,
}

impl EmulatorHosts {
    fn from_env() -> Self {
        Self {
            auth: read_env("FIREBASE_AUTH_EMULATOR_HOST"),
            firestore: read_env("FIRESTORE_EMULATOR_HOST"),
            storage: read_env("FIREBASE_STORAGE_EMULATOR_HOST"),
            functions: read_env("FIREBASE_FUNCTIONS_EMULATOR_HOST"),
        }
    }

    fn any(&self) -> bool {
        self.auth.is_some() || self.firestore.is_some() || self.storage.is_some() || self.functions.is_some()
    }
}

/// Splits `host:port` into its parts, defaulting the port when absent.
fn split_host_port(value: &str, default_port: u16) -> (String, u16) {
    match value.rsplit_once(':') {
        Some((host, port)) => (
            host.trim_matches(|c| c == '[' || c == ']').to_string(),
            port.parse().unwrap_or(default_port),
        ),
        None => (value.to_string(), default_port),
    }
}

impl LiveConfig {
    fn firebase_options(&self) -> FirebaseOptions {
        FirebaseOptions {
            api_key: Some(self.api_key.clone()),
            project_id: Some(self.project_id.clone()),
            app_id: Some(self.app_id.clone()),
            messaging_sender_id: self.project_number.clone(),
            storage_bucket: self.storage_bucket.clone(),
            database_url: self.database_url.clone(),
            auth_domain: Some(format!("{}.firebaseapp.com", self.project_id)),
            ..Default::default()
        }
    }
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Parses `KEY=VALUE` lines (with `#` comments and optional surrounding quotes) into the map.
/// Values already present in the process environment win over dot-file values.
fn load_dotfile(path: &Path, out: &mut HashMap<String, String>) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        if !value.is_empty() {
            out.entry(key.to_string()).or_insert_with(|| value.to_string());
        }
    }
}

/// Extracts the client settings from a `google-services.json` document.
fn load_google_services(json: &str, out: &mut HashMap<String, String>) {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(json) else {
        eprintln!("live tests: google-services.json is not valid JSON; ignoring");
        return;
    };
    fn put(out: &mut HashMap<String, String>, key: &str, value: Option<&str>) {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            out.entry(key.to_string()).or_insert_with(|| value.to_string());
        }
    }
    let info = &doc["project_info"];
    put(out, "FIREBASE_PROJECT_ID", info["project_id"].as_str());
    put(out, "FIREBASE_PROJECT_NUMBER", info["project_number"].as_str());
    put(out, "FIREBASE_STORAGE_BUCKET", info["storage_bucket"].as_str());
    put(out, "FIREBASE_DATABASE_URL", info["firebase_url"].as_str());

    // Prefer the client matching FIREBASE_APP_ID if given, otherwise the first client.
    let wanted_app = out.get("FIREBASE_APP_ID").cloned();
    let clients = doc["client"].as_array().cloned().unwrap_or_default();
    let client = clients
        .iter()
        .find(|c| {
            wanted_app
                .as_deref()
                .is_some_and(|w| c["client_info"]["mobilesdk_app_id"].as_str() == Some(w))
        })
        .or_else(|| clients.first());
    if let Some(client) = client {
        put(out, "FIREBASE_APP_ID", client["client_info"]["mobilesdk_app_id"].as_str());
        put(out, "FIREBASE_API_KEY", client["api_key"][0]["current_key"].as_str());
    }
}

/// Resolves the live configuration from the sources documented at the top of this file.
fn live_config() -> Option<LiveConfig> {
    let mut values: HashMap<String, String> = HashMap::new();
    for key in [
        "FIREBASE_API_KEY",
        "FIREBASE_PROJECT_ID",
        "FIREBASE_APP_ID",
        "FIREBASE_PROJECT_NUMBER",
        "FIREBASE_STORAGE_BUCKET",
        "FIREBASE_DATABASE_URL",
        "FIREBASE_TEST_CALLABLE",
    ] {
        if let Some(value) = read_env(key) {
            values.insert(key.to_string(), value);
        }
    }

    load_dotfile(&crate_root().join(".env.firebase"), &mut values);

    if let Some(json) = read_env("FIREBASE_GOOGLE_SERVICES_JSON") {
        load_google_services(&json, &mut values);
    } else {
        let path = read_env("FIREBASE_GOOGLE_SERVICES_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate_root().join("google-services.json"));
        if let Ok(json) = std::fs::read_to_string(&path) {
            load_google_services(&json, &mut values);
        }
    }

    let online = ["FIREBASE_API_KEY", "FIREBASE_PROJECT_ID", "FIREBASE_APP_ID"]
        .iter()
        .all(|key| values.contains_key(*key));
    if !online {
        return None;
    }
    Some(LiveConfig {
        api_key: values["FIREBASE_API_KEY"].clone(),
        project_id: values["FIREBASE_PROJECT_ID"].clone(),
        app_id: values["FIREBASE_APP_ID"].clone(),
        project_number: values.get("FIREBASE_PROJECT_NUMBER").cloned(),
        storage_bucket: values.get("FIREBASE_STORAGE_BUCKET").cloned(),
        database_url: values.get("FIREBASE_DATABASE_URL").cloned(),
        test_callable: values.get("FIREBASE_TEST_CALLABLE").cloned(),
        online: true,
        emulators: EmulatorHosts::default(),
    })
}

/// Configuration for the Local Emulator Suite, when any emulator host variable is set.
/// Emulators accept any API key and any `demo-*` project id without credentials, and the
/// project id must match the one the emulators were started with.
fn emulator_config() -> Option<LiveConfig> {
    let emulators = EmulatorHosts::from_env();
    if !emulators.any() {
        return None;
    }
    let project_id = read_env("FIREBASE_EMULATOR_PROJECT_ID")
        .or_else(|| read_env("GCLOUD_PROJECT"))
        .unwrap_or_else(|| "demo-firebase-rs-sdk".to_string());
    Some(LiveConfig {
        api_key: "demo-api-key".to_string(),
        app_id: "1:000000000000:web:demo".to_string(),
        project_number: Some("000000000000".to_string()),
        storage_bucket: Some(format!("{project_id}.appspot.com")),
        database_url: None,
        test_callable: None,
        project_id,
        online: false,
        emulators,
    })
}

/// Like [`require_config`] but for services without an emulator: skips unless real
/// credentials are configured.
fn require_online_config(test: &str) -> Option<LiveConfig> {
    init_process();
    match live_config() {
        Some(config) => Some(config),
        None => {
            let required = read_env("FIREBASE_LIVE_TESTS_REQUIRED").is_some_and(|v| v == "1" || v == "true");
            let message = format!(
                "SKIP: {test}: this service has no emulator; provide online credentials (see CONTRIBUTING.md) to run it."
            );
            if required {
                panic!("{message}");
            }
            eprintln!("{message}");
            None
        }
    }
}

/// Returns the configuration, or `None` after printing why the test is being skipped.
/// Panics instead when `FIREBASE_LIVE_TESTS_REQUIRED=1`.
fn require_config(test: &str) -> Option<LiveConfig> {
    init_process();
    // Emulators take precedence: the migrated tests run offline whenever the suite is up.
    match emulator_config().or_else(live_config) {
        Some(config) => Some(config),
        None => {
            let message = format!(
                "SKIP: {test}: no Firebase credentials or emulators found. Run scripts/emulator_test.sh, or \
                 provide google-services.json, .env.firebase, or FIREBASE_API_KEY/FIREBASE_PROJECT_ID/\
                 FIREBASE_APP_ID (see tests/live_endpoints.rs)."
            );
            if read_env("FIREBASE_LIVE_TESTS_REQUIRED").is_some_and(|v| v == "1" || v == "true") {
                panic!("{message}");
            }
            eprintln!("{message}");
            None
        }
    }
}

/// One-time process setup: keep the installations cache out of the working tree.
fn init_process() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if std::env::var_os("FIREBASE_INSTALLATIONS_CACHE_DIR").is_none() {
            let dir = std::env::temp_dir().join(format!("firebase-rs-sdk-live-tests-{}", std::process::id()));
            std::env::set_var("FIREBASE_INSTALLATIONS_CACHE_DIR", &dir);
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{millis:x}{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

async fn live_app(config: &LiveConfig, label: &str) -> FirebaseApp {
    let settings = FirebaseAppSettings {
        name: Some(format!("live-{label}-{}", nonce())),
        automatic_data_collection_enabled: Some(false),
    };
    initialize_app(config.firebase_options(), Some(settings))
        .await
        .expect("initialize_app should succeed with valid options")
}

/// Resolves the Auth service for `app`, routed to the Auth emulator when one is configured.
fn auth_for(config: &LiveConfig, app: &FirebaseApp) -> Arc<firebase_rs_sdk::auth::Auth> {
    register_auth_component();
    let auth = auth_for_app(app.clone()).expect("auth service");
    if let Some(host) = &config.emulators.auth {
        auth.connect_emulator(&format!("http://{host}"));
    }
    auth
}

/// Resolves the Storage service for `app`, routed to the Storage emulator when configured.
async fn storage_for(config: &LiveConfig, app: &FirebaseApp) -> Arc<firebase_rs_sdk::storage::FirebaseStorageImpl> {
    let storage = get_storage_for_app(Some(app.clone()), None)
        .await
        .expect("storage service");
    if let Some(host) = &config.emulators.storage {
        let (host, port) = split_host_port(host, 9199);
        firebase_rs_sdk::storage::connect_storage_emulator(&storage, &host, port, None).expect("connect emulator");
    }
    storage
}

/// Resolves the Functions service for `app`, routed to the Functions emulator when configured.
async fn functions_for(config: &LiveConfig, app: &FirebaseApp) -> Arc<firebase_rs_sdk::functions::Functions> {
    register_functions_component();
    let functions = get_functions(Some(app.clone()), None).await.expect("functions service");
    if let Some(host) = &config.emulators.functions {
        let (host, port) = split_host_port(host, 5001);
        functions.connect_emulator(&host, port);
    }
    functions
}

/// Classifies backend errors that mean "this product isn't provisioned on the project" rather
/// than "the SDK is broken". Returns a human-readable remediation when the test should skip.
fn provisioning_skip_reason(error_text: &str) -> Option<String> {
    let text = error_text.to_ascii_uppercase();
    let hits: &[(&str, &str)] = &[
        (
            "CONFIGURATION_NOT_FOUND",
            "Firebase Authentication has not been initialised for this project. Open the console, go to \
             Build > Authentication and click Get started, then enable the Anonymous and Email/Password \
             providers.",
        ),
        (
            "ADMIN_ONLY_OPERATION",
            "Anonymous sign-in is disabled. Enable the Anonymous provider under Authentication > Sign-in method.",
        ),
        (
            "OPERATION_NOT_ALLOWED",
            "The sign-in provider is disabled. Enable it under Authentication > Sign-in method.",
        ),
        (
            "SERVICE_DISABLED",
            "The product's Google API is disabled for this project. Enable it from the console link in the error.",
        ),
        (
            "DOES NOT EXIST FOR PROJECT",
            "The Firestore API is enabled but no database has been created. In the console open Build > Firestore \
             Database > Create database (the default database id must be `(default)`).",
        ),
        (
            "HAS NOT BEEN USED IN PROJECT",
            "The product's Google API is disabled for this project. Enable it from the console link in the error.",
        ),
    ];
    hits.iter()
        .find(|(needle, _)| text.contains(needle))
        .map(|(needle, fix)| format!("backend reported {needle}. {fix}"))
}

fn skip(test: &str, reason: &str, error: &str) {
    eprintln!("SKIP: {test}: {reason}\n      raw error: {error}");
}

fn field_string(data: &BTreeMap<String, FirestoreValue>, key: &str) -> Option<String> {
    data.get(key).and_then(|v| match v.kind() {
        ValueKind::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn field_integer(data: &BTreeMap<String, FirestoreValue>, key: &str) -> Option<i64> {
    data.get(key).and_then(|v| match v.kind() {
        ValueKind::Integer(i) => Some(*i),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// Probe: what can these credentials reach? (diagnostic, always passes when credentials exist)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn live_project_probe() {
    let Some(config) = require_online_config("live_project_probe") else {
        return;
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client");

    let mut rows: Vec<(&str, String)> = Vec::new();

    // Installations: create a throwaway registration and delete it again.
    let fid = "cAAAAAAAAAAAAAAAAAAAAA"; // any 22-char base64url string works for a probe
    let url = format!(
        "https://firebaseinstallations.googleapis.com/v1/projects/{}/installations",
        config.project_id
    );
    let body = serde_json::json!({
        "fid": fid, "appId": config.app_id, "authVersion": "FIS_v2", "sdkVersion": "w:0.6.4"
    });
    let response = client
        .post(&url)
        .header("x-goog-api-key", &config.api_key)
        .json(&body)
        .send()
        .await;
    rows.push(("installations", summarize(response).await));

    // Identity Toolkit: anonymous sign-up (fails with CONFIGURATION_NOT_FOUND if Auth is unset).
    let url = format!(
        "https://identitytoolkit.googleapis.com/v1/accounts:signUp?key={}",
        config.api_key
    );
    let response = client
        .post(&url)
        .json(&serde_json::json!({"returnSecureToken": true}))
        .send()
        .await;
    let (summary, id_token) = summarize_with_token(response).await;
    if let Some(token) = id_token {
        // Best effort clean-up of the anonymous account we just created. The token itself is
        // never printed: CI logs are no place for credentials, however short-lived.
        let url = format!(
            "https://identitytoolkit.googleapis.com/v1/accounts:delete?key={}",
            config.api_key
        );
        let _ = client
            .post(&url)
            .json(&serde_json::json!({"idToken": token}))
            .send()
            .await;
    }
    rows.push(("auth (signUp)", summary));

    // Firestore REST: list a collection that is unlikely to exist.
    let url = format!(
        "https://firestore.googleapis.com/v1/projects/{}/databases/(default)/documents/rust_sdk_live_tests?pageSize=1&key={}",
        config.project_id, config.api_key
    );
    rows.push(("firestore", summarize(client.get(&url).send().await).await));

    // Realtime Database REST.
    if let Some(db) = &config.database_url {
        let url = format!("{}/.json?shallow=true", db.trim_end_matches('/'));
        rows.push(("database", summarize(client.get(&url).send().await).await));
    } else {
        rows.push(("database", "no FIREBASE_DATABASE_URL configured".into()));
    }

    // Storage: list root of the default bucket.
    if let Some(bucket) = &config.storage_bucket {
        let url = format!("https://firebasestorage.googleapis.com/v0/b/{bucket}/o?maxResults=1");
        rows.push(("storage", summarize(client.get(&url).send().await).await));
    } else {
        rows.push(("storage", "no FIREBASE_STORAGE_BUCKET configured".into()));
    }

    // Remote Config fetch requires an installation token, so probe with a bogus one; the
    // endpoint answers 401/403 for bad tokens and 200 when everything is wired up.
    let url = format!(
        "https://firebaseremoteconfig.googleapis.com/v1/projects/{}/namespaces/firebase:fetch?key={}",
        config.project_id, config.api_key
    );
    let body = serde_json::json!({
        "sdk_version": "w:0.6.4", "app_instance_id": fid, "app_instance_id_token": "probe", "app_id": config.app_id
    });
    rows.push(("remote_config", summarize(client.post(&url).json(&body).send().await).await));

    // Callable functions: hit the regional host with a function that does not exist.
    let url = format!("https://us-central1-{}.cloudfunctions.net/rustSdkProbe", config.project_id);
    rows.push((
        "functions host",
        summarize(client.post(&url).json(&serde_json::json!({"data": {}})).send().await).await,
    ));

    eprintln!(
        "\nLive endpoint probe for project '{}' (app {}):",
        config.project_id, config.app_id
    );
    eprintln!("  mode: {}", if config.online { "online project" } else { "emulators" });
    if config.emulators.any() {
        eprintln!("  emulators: {:?}", config.emulators);
    }
    for (service, result) in &rows {
        eprintln!("  {service:<16} {result}");
    }
    eprintln!();

    // The probe is diagnostic; the only hard assertion is that we could reach Google at all.
    assert!(
        rows.iter().any(|(_, r)| r.starts_with(char::is_numeric)),
        "no Firebase endpoint answered; check network connectivity"
    );
}

/// Renders an HTTP outcome as `STATUS short-message` for the probe table, never leaking secrets.
async fn summarize(response: Result<reqwest::Response, reqwest::Error>) -> String {
    summarize_with_token(response).await.0
}

/// Like [`summarize`], additionally returning an `idToken` from the body (for clean-up) instead
/// of rendering it.
async fn summarize_with_token(response: Result<reqwest::Response, reqwest::Error>) -> (String, Option<String>) {
    match response {
        Err(err) => (format!("transport error: {err}"), None),
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();
            let id_token = json.as_ref().and_then(|j| j["idToken"].as_str()).map(str::to_string);
            let detail = json
                .as_ref()
                .and_then(|j| {
                    j["error"]["message"]
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| j["error"]["status"].as_str().map(str::to_string))
                        .or_else(|| j["state"].as_str().map(|s| format!("state={s}")))
                        .or_else(|| j["fid"].as_str().map(|_| "registered".to_string()))
                        .or_else(|| j["idToken"].as_str().map(|_| "signed in (token redacted)".to_string()))
                })
                .unwrap_or_else(|| {
                    let head: String = text.chars().take(60).collect();
                    head.replace('\n', " ")
                });
            let detail: String = detail.chars().take(160).collect();
            (format!("{status} {detail}"), id_token)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Installations
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn installations_registers_fid_and_issues_token() {
    let Some(config) = require_online_config("installations_registers_fid_and_issues_token") else {
        return;
    };
    let app = live_app(&config, "fis").await;
    let installations = get_installations(Some(app.clone())).expect("installations service");

    let fid = installations
        .get_id()
        .await
        .expect("get_id should register with the FIS backend");
    assert_eq!(fid.len(), 22, "FID must be 22 base64url characters, got {fid:?}");
    assert!(
        fid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "FID must be base64url, got {fid:?}"
    );
    let first = fid.chars().next().unwrap();
    assert!(
        "cdef".contains(first),
        "FID must start with the 0b0111 nibble prefix (c/d/e/f), got {fid:?}"
    );

    let same_fid = installations.get_id().await.expect("second get_id");
    assert_eq!(fid, same_fid, "FID must be stable across calls");

    let token = installations
        .get_token(false)
        .await
        .expect("get_token should return the registration token");
    assert!(!token.token.is_empty());
    assert!(token.expires_at > SystemTime::now(), "token expiry must be in the future");

    let refreshed = installations
        .get_token(true)
        .await
        .expect("forced refresh should hit authTokens:generate");
    assert!(!refreshed.token.is_empty());
    assert!(refreshed.expires_at > SystemTime::now());

    delete_installations(&installations)
        .await
        .expect("delete_installations should call the DELETE endpoint");
    delete_app(&app).await.expect("delete_app");
}

// ---------------------------------------------------------------------------------------------
// Remote Config
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn remote_config_fetches_and_activates_live_template() {
    let Some(config) = require_online_config("remote_config_fetches_and_activates_live_template") else {
        return;
    };
    let app = live_app(&config, "rc").await;
    let remote_config = get_remote_config(Some(app.clone()))
        .await
        .expect("remote config service");

    remote_config.set_defaults(HashMap::from([
        ("rust_sdk_live_default".to_string(), "from-defaults".to_string()),
        ("rust_sdk_live_flag".to_string(), "true".to_string()),
    ]));

    // fetch_and_activate returns true only when a template with a new ETag was activated. A project
    // without a published template answers NO_TEMPLATE (HTTP 200, no ETag), which must activate
    // nothing and leave the defaults reporting the `default` source, exactly as the JS SDK does.
    let activated = remote_config
        .fetch_and_activate()
        .await
        .expect("fetch_and_activate should succeed against the Remote Config backend");
    eprintln!("remote config: activated fresh values = {activated}");

    assert_eq!(
        remote_config.last_fetch_status(),
        FetchStatus::Success,
        "a successful round-trip must record FetchStatus::Success"
    );
    assert_eq!(remote_config.get_string("rust_sdk_live_default"), "from-defaults");
    assert!(remote_config.get_boolean("rust_sdk_live_flag"));
    assert_eq!(remote_config.get_string("rust_sdk_live_missing_key"), "");
    assert_eq!(
        remote_config.get_value("rust_sdk_live_missing_key").source(),
        RemoteConfigValueSource::Static
    );

    // Our default keys are not part of any real template, so they must never be reported as remote.
    for key in ["rust_sdk_live_default", "rust_sdk_live_flag"] {
        assert_eq!(
            remote_config.get_value(key).source(),
            RemoteConfigValueSource::Default,
            "default-only key {key} must keep the `default` source after activation"
        );
    }

    let all = remote_config.get_all();
    eprintln!("remote config: {} parameters visible after activation", all.len());
    for (key, value) in &all {
        eprintln!("  {key} = {:?} ({:?})", value.as_string(), value.source());
    }
    if !activated {
        assert!(
            all.values()
                .all(|value| value.source() == RemoteConfigValueSource::Default),
            "nothing was activated, so every visible value must come from the defaults"
        );
        assert!(remote_config.active_template_version().is_none());
    } else {
        assert!(
            all.values()
                .any(|value| value.source() == RemoteConfigValueSource::Remote),
            "activation reported a change but no remote values are visible"
        );
    }

    // Activating again without a new fetch must be a no-op.
    assert!(!remote_config.activate().await.expect("activate"));

    delete_app(&app).await.expect("delete_app");
}

// ---------------------------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_anonymous_sign_in_round_trip() {
    let test = "auth_anonymous_sign_in_round_trip";
    let Some(config) = require_config(test) else {
        return;
    };
    let app = live_app(&config, "auth-anon").await;
    let auth = auth_for(&config, &app);

    // Record every auth-state notification as Some(uid) / None.
    let events: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let unsubscribe = auth.on_auth_state_changed(move |user: &Option<Arc<User>>| {
        sink.lock().unwrap().push(user.as_ref().map(|u| u.uid().to_string()));
    });
    assert_eq!(
        *events.lock().unwrap(),
        vec![None],
        "listener must be primed with the signed-out state"
    );

    let credential = match auth.sign_in_anonymously().await {
        Ok(credential) => credential,
        Err(err) => {
            let text = err.to_string();
            if let Some(reason) = provisioning_skip_reason(&text) {
                skip(test, &reason, &text);
                delete_app(&app).await.ok();
                return;
            }
            panic!("anonymous sign-in failed: {text}");
        }
    };

    let user = credential.user.clone();
    assert!(!user.uid().is_empty(), "uid must be populated");
    assert!(user.is_anonymous(), "user must be flagged anonymous");
    assert!(auth.current_user().is_some(), "current_user must be set after sign-in");

    let token = auth
        .get_token(false)
        .await
        .expect("get_token")
        .expect("an ID token must be cached");
    assert_eq!(token.split('.').count(), 3, "ID token must be a JWT");

    let refreshed = auth
        .get_token(true)
        .await
        .expect("forced refresh should hit securetoken.googleapis.com")
        .expect("refreshed token");
    assert_eq!(refreshed.split('.').count(), 3);

    // The same refresh must be reachable from the user object, as in the JS SDK. Secure Token
    // mints byte-identical JWTs within one second (same `iat`), so wait before forcing again.
    let before = user.cached_id_token().expect("cached token");
    std::thread::sleep(Duration::from_millis(1100));
    let via_user = user.get_id_token(true).await.expect("User::get_id_token(true)");
    assert_eq!(via_user.split('.').count(), 3);
    assert_ne!(via_user, before, "a forced refresh must mint a new token");
    assert_eq!(user.cached_id_token().as_deref(), Some(via_user.as_str()));
    assert_eq!(
        user.get_id_token(false).await.expect("cached"),
        via_user,
        "a valid token is served from the cache"
    );

    let uid = user.uid().to_string();
    assert_eq!(
        *events.lock().unwrap(),
        vec![None, Some(uid.clone())],
        "token refreshes must not fire auth-state notifications"
    );

    auth.delete_user()
        .await
        .expect("delete_user should remove the anonymous account");
    assert!(auth.current_user().is_none(), "current_user must be cleared after deletion");
    assert_eq!(
        *events.lock().unwrap(),
        vec![None, Some(uid.clone()), None],
        "deletion must report sign-out"
    );

    unsubscribe();
    let second = auth.sign_in_anonymously().await.expect("second anonymous sign-in");
    assert_ne!(second.user.uid(), uid);
    assert_eq!(events.lock().unwrap().len(), 3, "unsubscribed listener must stay silent");
    auth.delete_user().await.expect("cleanup second anonymous user");

    delete_app(&app).await.expect("delete_app");
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_email_password_create_sign_in_and_delete() {
    let test = "auth_email_password_create_sign_in_and_delete";
    let Some(config) = require_config(test) else {
        return;
    };
    let app = live_app(&config, "auth-email").await;
    let auth = auth_for(&config, &app);

    let email = format!("rust-sdk-live-{}@example.com", nonce());
    let password = format!("Pw-{}-{}", nonce(), "correct-horse");

    let created = match auth.create_user_with_email_and_password(&email, &password).await {
        Ok(credential) => credential,
        Err(err) => {
            let text = err.to_string();
            if let Some(reason) = provisioning_skip_reason(&text) {
                skip(test, &reason, &text);
                delete_app(&app).await.ok();
                return;
            }
            panic!("create_user_with_email_and_password failed: {text}");
        }
    };
    assert_eq!(created.user.info().email.as_deref(), Some(email.as_str()));
    let uid = created.user.uid().to_string();

    auth.sign_out();
    assert!(auth.current_user().is_none());

    // Wrong password must surface a server error, not a success.
    let wrong = auth.sign_in_with_email_and_password(&email, "definitely-wrong").await;
    match wrong {
        Ok(_) => panic!("sign-in with a wrong password must fail"),
        Err(err) => {
            // Projects with email enumeration protection answer INVALID_LOGIN_CREDENTIALS
            // (`auth/invalid-credential`); older projects answer INVALID_PASSWORD
            // (`auth/wrong-password`). Both must surface as typed server errors.
            let code = err.code().cloned();
            assert!(
                matches!(code, Some(AuthErrorCode::WrongPassword | AuthErrorCode::InvalidCredential)),
                "expected a typed credential error, got {} ({})",
                auth_error_variant(&err),
                err
            );
            eprintln!("wrong-password error: {err}");
        }
    }

    let signed_in = auth
        .sign_in_with_email_and_password(&email, &password)
        .await
        .expect("sign-in with the correct password");
    assert_eq!(signed_in.user.uid(), uid, "the same account must be returned");

    auth.delete_user().await.expect("delete_user");
    let after_delete = auth
        .sign_in_with_email_and_password(&email, &password)
        .await
        .expect_err("deleted account must no longer sign in");
    assert!(
        matches!(
            after_delete.code(),
            Some(AuthErrorCode::UserNotFound | AuthErrorCode::InvalidCredential)
        ),
        "expected user-not-found or invalid-credential, got {after_delete}"
    );
    delete_app(&app).await.expect("delete_app");
}

fn auth_error_variant(err: &AuthError) -> &'static str {
    match err {
        AuthError::Firebase(_) => "Firebase",
        AuthError::App(_) => "App",
        AuthError::Network(_) => "Network",
        AuthError::InvalidCredential(_) => "InvalidCredential",
        AuthError::NotImplemented(_) => "NotImplemented",
        AuthError::MultiFactorRequired(_) => "MultiFactorRequired",
        AuthError::MultiFactor(_) => "MultiFactor",
        AuthError::Server(_) => "Server",
    }
}

// ---------------------------------------------------------------------------------------------
// Firestore
// ---------------------------------------------------------------------------------------------

/// Everything a live Firestore test needs: an anonymous user (so the security rules that require
/// `request.auth != null` pass) and an authenticated client.
struct LiveFirestore {
    app: FirebaseApp,
    auth: Arc<firebase_rs_sdk::auth::Auth>,
    client: FirestoreClient,
    firestore: Firestore,
}

impl LiveFirestore {
    async fn connect(config: &LiveConfig, label: &str) -> Self {
        let app = live_app(config, label).await;
        let firestore = Firestore::from_arc(get_firestore(Some(app.clone())).await.expect("firestore service"));
        let auth = auth_for(config, &app);
        let client = if auth.sign_in_anonymously().await.is_ok() {
            FirestoreClient::with_http_datastore_authenticated(firestore.clone(), auth.token_provider(), None)
        } else {
            eprintln!("firestore: anonymous auth unavailable, continuing unauthenticated");
            FirestoreClient::with_http_datastore(firestore.clone())
        }
        .expect("firestore client");
        Self {
            app,
            auth,
            client,
            firestore,
        }
    }

    /// Prints a `SKIP:` line and returns `true` when `err` means the project is not provisioned
    /// for this test (API disabled, no database, or rules that deny the scratch collection).
    fn skip_if_unprovisioned(&self, test: &str, err: &firebase_rs_sdk::firestore::FirestoreError) -> bool {
        let text = err.to_string();
        if let Some(reason) = provisioning_skip_reason(&text) {
            skip(test, &reason, &text);
            return true;
        }
        if matches!(
            err.code,
            FirestoreErrorCode::PermissionDenied | FirestoreErrorCode::Unauthenticated
        ) {
            skip(
                test,
                "Firestore security rules deny writes to `rust_sdk_live_tests`. Allow read/write on that \
                 collection for authenticated users (the test signs in anonymously when the provider is \
                 enabled).",
                &text,
            );
            return true;
        }
        false
    }

    async fn teardown(self) {
        cleanup_auth(&self.auth).await;
        delete_app(&self.app).await.ok();
    }
}

const LIVE_COLLECTION: &str = "rust_sdk_live_tests";

fn integer_field(snapshot: &firebase_rs_sdk::firestore::DocumentSnapshot, field: &str) -> i64 {
    snapshot.data().and_then(|d| field_integer(d, field)).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_document_crud_and_query() {
    let test = "firestore_document_crud_and_query";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs").await;
    let client = &live.client;

    let marker = nonce();
    let mut data = BTreeMap::new();
    data.insert("marker".to_string(), FirestoreValue::from_string(marker.clone()));
    data.insert("count".to_string(), FirestoreValue::from_integer(1));
    data.insert("active".to_string(), FirestoreValue::from_bool(true));

    let added = match client.add_doc(LIVE_COLLECTION, data).await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if live.skip_if_unprovisioned(test, &err) {
                live.teardown().await;
                return;
            }
            panic!("add_doc failed: {err}");
        }
    };
    let doc_path = format!("{LIVE_COLLECTION}/{}", added.id());

    let fetched = client.get_doc(&doc_path).await.expect("get_doc");
    assert!(fetched.exists(), "document must exist after add_doc");
    let fetched_data = fetched.data().expect("data");
    assert_eq!(field_string(fetched_data, "marker").as_deref(), Some(marker.as_str()));
    assert_eq!(field_integer(fetched_data, "count"), Some(1));

    let mut update = BTreeMap::new();
    update.insert("count".to_string(), FirestoreValue::from_integer(2));
    client.update_doc(&doc_path, update).await.expect("update_doc");
    let updated = client.get_doc(&doc_path).await.expect("get_doc after update");
    assert_eq!(field_integer(updated.data().expect("data"), "count"), Some(2));

    let query = live
        .firestore
        .collection(LIVE_COLLECTION)
        .expect("collection")
        .query()
        .where_field(
            FieldPath::from_dot_separated("marker").expect("field path"),
            FilterOperator::Equal,
            FirestoreValue::from_string(marker.clone()),
        )
        .expect("where");
    let results = client.get_docs(&query).await.expect("get_docs");
    assert_eq!(results.documents().len(), 1, "query by marker must return exactly our document");
    assert_eq!(results.documents()[0].id(), added.id());

    // A batch commit reports the backend's update time for each write.
    let doc_ref = live.firestore.doc(&doc_path).expect("doc ref");
    let mut batch = client.batch();
    let mut patch = BTreeMap::new();
    patch.insert("count".to_string(), FirestoreValue::from_integer(3));
    batch.update(&doc_ref, patch).expect("batch update");
    let commit = batch.commit_with_results().await.expect("commit_with_results");
    assert_eq!(commit.write_results.len(), 1);
    assert!(commit.write_results[0].update_time.is_some(), "backend must report updateTime");
    assert!(commit.commit_time.is_some(), "backend must report commitTime");

    client.delete_doc(&doc_path).await.expect("delete_doc");
    let gone = client.get_doc(&doc_path).await.expect("get_doc after delete");
    assert!(!gone.exists(), "document must not exist after delete");

    live.teardown().await;
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_transaction_read_modify_write() {
    let test = "firestore_transaction_read_modify_write";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-txn").await;
    let client = &live.client;
    let path = format!("{LIVE_COLLECTION}/txn-{}", nonce());
    let counter = live.firestore.doc(&path).expect("doc ref");

    let mut seed = BTreeMap::new();
    seed.insert("total".to_string(), FirestoreValue::from_integer(10));
    if let Err(err) = client.set_doc(&path, seed, None).await {
        if live.skip_if_unprovisioned(test, &err) {
            live.teardown().await;
            return;
        }
        panic!("seed set_doc failed: {err}");
    }

    let attempts = Arc::new(Mutex::new(0usize));
    let returned = client
        .run_transaction(|txn| {
            let counter = counter.clone();
            let attempts = Arc::clone(&attempts);
            async move {
                *attempts.lock().unwrap() += 1;
                let snapshot = txn.get(&counter).await?;
                assert!(snapshot.exists(), "seeded document must be visible inside the transaction");
                let next = integer_field(&snapshot, "total") + 1;
                let mut data = BTreeMap::new();
                data.insert("total".to_string(), FirestoreValue::from_integer(next));
                txn.set(&counter, data, None)?;
                Ok(next)
            }
        })
        .await
        .expect("run_transaction");
    assert_eq!(returned, 11);
    assert_eq!(*attempts.lock().unwrap(), 1, "an uncontended transaction commits first time");

    let stored = client.get_doc(&path).await.expect("get_doc");
    assert_eq!(integer_field(&stored, "total"), 11);

    // A read-only transaction with no writes commits cleanly and returns the closure value.
    let seen = client
        .run_transaction(|txn| {
            let counter = counter.clone();
            async move { Ok(integer_field(&txn.get(&counter).await?, "total")) }
        })
        .await
        .expect("read-only transaction");
    assert_eq!(seen, 11);

    client.delete_doc(&path).await.expect("cleanup");
    live.teardown().await;
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_concurrent_transactions_serialize() {
    let test = "firestore_concurrent_transactions_serialize";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-txn-race").await;
    let client = &live.client;
    let path = format!("{LIVE_COLLECTION}/race-{}", nonce());
    let counter = live.firestore.doc(&path).expect("doc ref");

    let mut seed = BTreeMap::new();
    seed.insert("total".to_string(), FirestoreValue::from_integer(0));
    if let Err(err) = client.set_doc(&path, seed, None).await {
        if live.skip_if_unprovisioned(test, &err) {
            live.teardown().await;
            return;
        }
        panic!("seed set_doc failed: {err}");
    }

    // Two transactions race on the same document. Firestore must serialise them: whichever
    // commits second either waited for the first or was aborted and re-run, so the final
    // total is exactly 2 with no lost update.
    let attempts = Arc::new(Mutex::new(0usize));
    let increment = |label: &'static str| {
        let counter = counter.clone();
        let attempts = Arc::clone(&attempts);
        async move {
            client
                .run_transaction(move |txn| {
                    let counter = counter.clone();
                    let attempts = Arc::clone(&attempts);
                    async move {
                        *attempts.lock().unwrap() += 1;
                        let current = integer_field(&txn.get(&counter).await?, "total");
                        let mut data = BTreeMap::new();
                        data.insert("total".to_string(), FirestoreValue::from_integer(current + 1));
                        data.insert("last_writer".to_string(), FirestoreValue::from_string(label));
                        txn.set(&counter, data, None)?;
                        Ok(current + 1)
                    }
                })
                .await
        }
    };
    let (first, second) = tokio::join!(increment("first"), increment("second"));
    let first = first.expect("first transaction");
    let second = second.expect("second transaction");
    let mut results = vec![first, second];
    results.sort_unstable();
    assert_eq!(results, vec![1, 2], "each transaction must observe the other's increment");

    let stored = client.get_doc(&path).await.expect("get_doc");
    assert_eq!(integer_field(&stored, "total"), 2, "no lost update");
    eprintln!(
        "concurrent transactions: {} closure invocations for 2 commits",
        *attempts.lock().unwrap()
    );

    client.delete_doc(&path).await.expect("cleanup");
    live.teardown().await;
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_transaction_rolls_back_on_closure_error() {
    let test = "firestore_transaction_rolls_back_on_closure_error";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-txn-rollback").await;
    let client = &live.client;
    let path = format!("{LIVE_COLLECTION}/rollback-{}", nonce());
    let doc_ref = live.firestore.doc(&path).expect("doc ref");

    // Probe provisioning with a real write first so the skip logic stays uniform.
    let mut probe = BTreeMap::new();
    probe.insert("probe".to_string(), FirestoreValue::from_bool(true));
    if let Err(err) = client.set_doc(&path, probe, None).await {
        if live.skip_if_unprovisioned(test, &err) {
            live.teardown().await;
            return;
        }
        panic!("probe set_doc failed: {err}");
    }
    client.delete_doc(&path).await.expect("probe cleanup");

    let calls = Arc::new(Mutex::new(0usize));
    let err = client
        .run_transaction(|txn| {
            let doc_ref = doc_ref.clone();
            let calls = Arc::clone(&calls);
            async move {
                *calls.lock().unwrap() += 1;
                let mut data = BTreeMap::new();
                data.insert("should_not_persist".to_string(), FirestoreValue::from_bool(true));
                txn.set(&doc_ref, data, None)?;
                Err::<(), _>(firebase_rs_sdk::firestore::invalid_argument("business rule violated"))
            }
        })
        .await
        .expect_err("closure error must propagate");
    assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);
    assert_eq!(*calls.lock().unwrap(), 1, "invalid-argument is not retried");

    let after = client.get_doc(&path).await.expect("get_doc");
    assert!(!after.exists(), "staged write must have been rolled back");

    live.teardown().await;
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_transaction_verifies_documents_it_only_read() {
    let test = "firestore_transaction_verifies_documents_it_only_read";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-txn-verify").await;
    let client = &live.client;
    let tag = nonce();
    let source_path = format!("{LIVE_COLLECTION}/verify-src-{tag}");
    let target_path = format!("{LIVE_COLLECTION}/verify-dst-{tag}");
    let source = live.firestore.doc(&source_path).expect("doc ref");
    let target = live.firestore.doc(&target_path).expect("doc ref");

    let mut seed = BTreeMap::new();
    seed.insert("total".to_string(), FirestoreValue::from_integer(100));
    if let Err(err) = client.set_doc(&source_path, seed, None).await {
        if live.skip_if_unprovisioned(test, &err) {
            live.teardown().await;
            return;
        }
        panic!("seed set_doc failed: {err}");
    }

    // The closure reads `source` and only writes `target`. On the first attempt another writer
    // changes `source` between the read and the commit; the commit must fail on the `verify`
    // precondition and the closure must run again, now observing the new value.
    let attempts = Arc::new(Mutex::new(0usize));
    let interfering = client.clone();
    let copied = client
        .run_transaction(|txn| {
            let (source, target) = (source.clone(), target.clone());
            let attempts = Arc::clone(&attempts);
            let interfering = interfering.clone();
            let source_path = source_path.clone();
            async move {
                let attempt = {
                    let mut guard = attempts.lock().unwrap();
                    *guard += 1;
                    *guard
                };
                let snapshot = txn.get(&source).await?;
                assert!(snapshot.update_time().is_some(), "live reads must report updateTime");
                let total = integer_field(&snapshot, "total");
                if attempt == 1 {
                    let mut bump = BTreeMap::new();
                    bump.insert("total".to_string(), FirestoreValue::from_integer(total + 1));
                    interfering.update_doc(&source_path, bump).await?;
                }
                let mut data = BTreeMap::new();
                data.insert("copied_total".to_string(), FirestoreValue::from_integer(total));
                txn.set(&target, data, None)?;
                Ok(total)
            }
        })
        .await
        .expect("transaction with verify retry");

    assert_eq!(*attempts.lock().unwrap(), 2, "the stale read must force exactly one retry");
    assert_eq!(copied, 101, "the retry must observe the interfering write");
    let stored = client.get_doc(&target_path).await.expect("get target");
    assert_eq!(integer_field(&stored, "copied_total"), 101);

    // Updating a document that was read as missing is rejected client-side, as in the JS SDK.
    let ghost = live
        .firestore
        .doc(&format!("{LIVE_COLLECTION}/ghost-{tag}"))
        .expect("doc ref");
    let err = client
        .run_transaction(|txn| {
            let ghost = ghost.clone();
            async move {
                let _ = txn.get(&ghost).await?;
                let mut data = BTreeMap::new();
                data.insert("x".to_string(), FirestoreValue::from_integer(1));
                txn.update(&ghost, data)?;
                Ok(())
            }
        })
        .await
        .expect_err("update of a missing document must fail");
    assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);

    client.delete_doc(&source_path).await.expect("cleanup source");
    client.delete_doc(&target_path).await.expect("cleanup target");
    live.teardown().await;
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
enum Climate {
    Temperate,
    Tropical { humidity: u8 },
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct CityRecord {
    marker: String,
    name: String,
    population: i64,
    area_km2: f64,
    capital: bool,
    tags: Vec<String>,
    mayor: Option<String>,
    founded: firebase_rs_sdk::firestore::Timestamp,
    location: firebase_rs_sdk::firestore::GeoPoint,
    flag: firebase_rs_sdk::firestore::BytesValue,
    climate: Climate,
    stats: BTreeMap<String, i64>,
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_serde_round_trip_query_and_transaction() {
    let test = "firestore_serde_round_trip_query_and_transaction";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-serde").await;
    let client = &live.client;
    let marker = format!("serde-{}", nonce());
    let path = format!("{LIVE_COLLECTION}/{marker}");

    let city = CityRecord {
        marker: marker.clone(),
        name: "Amsterdam".into(),
        population: 921_402,
        area_km2: 219.3,
        capital: true,
        tags: vec!["canals".into(), "bikes".into()],
        mayor: None,
        founded: firebase_rs_sdk::firestore::Timestamp::new(-21_366_115_200, 500_000),
        location: firebase_rs_sdk::firestore::GeoPoint::new(52.37, 4.9).expect("geo point"),
        flag: firebase_rs_sdk::firestore::BytesValue::new(vec![0xde, 0xad, 0xbe, 0xef]),
        climate: Climate::Tropical { humidity: 87 },
        stats: BTreeMap::from([("bridges".to_string(), 1281), ("museums".to_string(), 75)]),
    };

    // Typed set + get: every Firestore-specific type must survive the REST encoding.
    if let Err(err) = client.set_doc_as(&path, &city, None).await {
        if live.skip_if_unprovisioned(test, &err) {
            live.teardown().await;
            return;
        }
        panic!("set_doc_as failed: {err}");
    }
    let stored: CityRecord = client
        .get_doc_as(&path)
        .await
        .expect("get_doc_as")
        .expect("document exists");
    assert_eq!(stored, city);

    // Typed query results.
    let query = live
        .firestore
        .collection(LIVE_COLLECTION)
        .expect("collection")
        .query()
        .where_field(
            FieldPath::from_dot_separated("marker").expect("field path"),
            FilterOperator::Equal,
            FirestoreValue::from_string(marker.clone()),
        )
        .expect("where");
    let cities: Vec<CityRecord> = client.get_docs_as(&query).await.expect("get_docs_as");
    assert_eq!(cities, vec![city.clone()]);

    // Typed converter on a reference, the JS `withConverter` path.
    let converted = live
        .firestore
        .doc(&path)
        .expect("doc")
        .with_converter(firebase_rs_sdk::firestore::SerdeConverter::<CityRecord>::new());
    let typed = client.get_doc_with_converter(&converted).await.expect("typed get");
    assert_eq!(typed.data().expect("decode").as_ref(), Some(&city));

    // Typed transaction: read as a struct, write back a modified struct.
    let doc_ref = live.firestore.doc(&path).expect("doc");
    let grown = client
        .run_transaction(|txn| {
            let doc_ref = doc_ref.clone();
            async move {
                let mut current: CityRecord = txn.get_as(&doc_ref).await?.expect("exists");
                current.population += 1;
                current.mayor = Some("Femke".into());
                txn.set_as(&doc_ref, &current, None)?;
                Ok(current.population)
            }
        })
        .await
        .expect("typed transaction");
    assert_eq!(grown, 921_403);
    let after: CityRecord = client.get_doc_as(&path).await.unwrap().unwrap();
    assert_eq!(after.population, 921_403);
    assert_eq!(after.mayor.as_deref(), Some("Femke"));

    // Serde-encoded sentinel inside a struct update through a batch.
    #[derive(serde::Serialize)]
    struct Touch {
        population: FirestoreValue,
        tags: FirestoreValue,
    }
    let mut batch = client.batch();
    batch
        .update_as(
            &doc_ref,
            &Touch {
                population: FirestoreValue::numeric_increment(FirestoreValue::from_integer(7)),
                tags: FirestoreValue::array_union(vec![FirestoreValue::from_string("tulips")]),
            },
        )
        .expect("batch update_as");
    batch.commit().await.expect("batch commit");
    let touched: CityRecord = client.get_doc_as(&path).await.unwrap().unwrap();
    assert_eq!(touched.population, 921_410);
    assert_eq!(touched.tags, vec!["canals", "bikes", "tulips"]);

    client.delete_doc(&path).await.expect("cleanup");
    live.teardown().await;
}

async fn cleanup_auth(auth: &std::sync::Arc<firebase_rs_sdk::auth::Auth>) {
    if auth.current_user().is_some() {
        if let Err(err) = auth.delete_user().await {
            eprintln!("warning: failed to delete temporary anonymous user: {err}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cloud Storage
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn storage_error_codes_match_the_js_sdk() {
    let test = "storage_error_codes_match_the_js_sdk";
    let Some(config) = require_config(test) else {
        return;
    };
    if config.emulators.storage.is_none() {
        skip(
            test,
            "needs the Storage emulator (scripts/emulator_test.sh); online buckets are not provisioned",
            "n/a",
        );
        return;
    }
    let app = live_app(&config, "storage-errors").await;
    let auth = auth_for(&config, &app);
    auth.sign_in_anonymously().await.expect("anonymous sign-in");
    let storage = storage_for(&config, &app).await;
    let root = storage.root_reference().expect("root reference");

    // 404 on an object the rules allow us to read -> object-not-found.
    let missing = root.child(&format!("rust_sdk_live_tests/missing-{}.txt", nonce()));
    let err = missing.get_metadata().await.expect_err("missing object");
    assert_eq!(err.code, StorageErrorCode::ObjectNotFound, "got {err}");
    assert_eq!(err.status, Some(404));
    let err = missing.get_bytes(None).await.expect_err("missing object bytes");
    assert_eq!(err.code, StorageErrorCode::ObjectNotFound, "got {err}");
    let err = missing.get_download_url().await.expect_err("missing object url");
    assert_eq!(err.code, StorageErrorCode::ObjectNotFound, "got {err}");
    let err = missing.delete_object().await.expect_err("missing object delete");
    assert_eq!(err.code, StorageErrorCode::ObjectNotFound, "got {err}");

    // Rules deny writes outside `rust_sdk_live_tests/` -> unauthorized (403), with the raw body.
    let forbidden = root.child(&format!("forbidden/{}.txt", nonce()));
    let err = forbidden
        .upload_string("nope", StringFormat::Raw, None)
        .await
        .expect_err("rules must deny");
    assert_eq!(err.code, StorageErrorCode::Unauthorized, "got {err}");
    assert_eq!(err.status, Some(403));
    assert!(err.server_response.is_some(), "server body must be preserved");
    assert!(err.to_string().contains("forbidden/"), "message names the path: {err}");

    // No user at all: the emulator answers 403 as well (rules see request.auth == null).
    cleanup_auth(&auth).await;
    auth.sign_out();
    let anonymous_read = root.child(&format!("rust_sdk_live_tests/whatever-{}.txt", nonce()));
    let err = anonymous_read
        .get_metadata()
        .await
        .expect_err("signed-out read must fail");
    assert!(
        matches!(err.code, StorageErrorCode::Unauthorized | StorageErrorCode::Unauthenticated),
        "got {err}"
    );

    delete_app(&app).await.expect("delete_app");
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn storage_upload_download_and_delete() {
    let test = "storage_upload_download_and_delete";
    let Some(config) = require_config(test) else {
        return;
    };
    if config.storage_bucket.is_none() {
        skip(test, "no FIREBASE_STORAGE_BUCKET configured", "n/a");
        return;
    }
    let app = live_app(&config, "storage").await;
    // Sign in anonymously so rules of the form `request.auth != null` pass; Storage picks the
    // token up through the app's `auth-internal` component.
    let auth = auth_for(&config, &app);
    if let Err(err) = auth.sign_in_anonymously().await {
        eprintln!("storage: anonymous auth unavailable ({err}), continuing unauthenticated");
    }
    let storage = storage_for(&config, &app).await;

    let object_name = format!("rust_sdk_live_tests/{}.txt", nonce());
    let reference = storage.root_reference().expect("root reference").child(&object_name);
    let payload = format!("Hello from firebase-rs-sdk live tests ({})", nonce());

    let metadata = match reference.upload_string(&payload, StringFormat::Raw, None).await {
        Ok(metadata) => metadata,
        Err(err) => {
            let text = err.to_string();
            // The SDK maps every non-2xx to `internal-error`, so the HTTP status is the only
            // reliable signal for "bucket missing / rules deny" (404 / 401 / 403).
            // Against the emulator nothing can be unprovisioned: any failure is a real failure.
            let not_provisioned = config.emulators.storage.is_none()
                && (provisioning_skip_reason(&text).is_some()
                    || matches!(
                        err.code,
                        StorageErrorCode::Unauthenticated
                            | StorageErrorCode::Unauthorized
                            | StorageErrorCode::ObjectNotFound
                            | StorageErrorCode::BucketNotFound
                    )
                    || (err.code == StorageErrorCode::Unknown
                        && matches!(err.status, Some(401) | Some(403) | Some(404))));
            if not_provisioned {
                skip(
                    test,
                    "Cloud Storage is not usable with these credentials. Enable Storage in the console (Build > \
                     Storage > Get started) and allow writes to `rust_sdk_live_tests/` in the rules.",
                    &text,
                );
                cleanup_auth(&auth).await;
                delete_app(&app).await.ok();
                return;
            }
            panic!("upload_string failed: {text}");
        }
    };
    assert_eq!(metadata.name.as_deref(), Some(object_name.as_str()));

    let fetched = reference.get_metadata().await.expect("get_metadata");
    assert_eq!(fetched.name.as_deref(), Some(object_name.as_str()));

    let bytes = reference.get_bytes(None).await.expect("get_bytes");
    assert_eq!(String::from_utf8(bytes).expect("utf-8"), payload);

    let url = reference.get_download_url().await.expect("get_download_url");
    let expected_scheme = if config.emulators.storage.is_some() {
        "http://"
    } else {
        "https://"
    };
    assert!(url.starts_with(expected_scheme), "download URL must be absolute, got {url}");
    assert!(
        url.contains("alt=media") && url.contains("token="),
        "download URL must carry a token: {url}"
    );

    reference.delete_object().await.expect("delete_object");
    let after_delete = reference
        .get_metadata()
        .await
        .expect_err("object must be gone after delete");
    assert_eq!(after_delete.code, StorageErrorCode::ObjectNotFound, "got {after_delete}");
    assert_eq!(after_delete.status, Some(404));
    assert_eq!(after_delete.code_str(), "storage/object-not-found");

    // The download URL must use `encodeURIComponent` semantics: `-`, `.` and `_` stay literal.
    assert!(
        !url.contains("%2D") && !url.contains("%2E") && !url.contains("%5F"),
        "over-encoded URL: {url}"
    );

    cleanup_auth(&auth).await;
    delete_app(&app).await.expect("delete_app");
}

// ---------------------------------------------------------------------------------------------
// Cloud Functions (callable protocol)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn functions_callable_protocol_against_live_host() {
    let test = "functions_callable_protocol_against_live_host";
    let Some(config) = require_config(test) else {
        return;
    };
    let app = live_app(&config, "fn").await;
    let functions = functions_for(&config, &app).await;

    // A function that does not exist must map the host's 404 to `not-found`, which proves the
    // request reached the callable host and that error decoding works.
    let missing = functions
        .https_callable::<serde_json::Value, serde_json::Value>("rustSdkLiveTestsDoesNotExist")
        .expect("callable reference");
    let err = missing
        .call_async(&serde_json::json!({ "ping": true }))
        .await
        .expect_err("calling a missing function must fail");
    assert_eq!(err.code, FunctionsErrorCode::NotFound, "unexpected error: {}", err.message());

    if config.emulators.functions.is_some() {
        // The emulator serves the fixtures in `firebase-emulator/functions/index.js`. Sign in so the callable can
        // report the caller's uid, proving the ID token travels in the request.
        let auth = auth_for(&config, &app);
        let uid = auth
            .sign_in_anonymously()
            .await
            .expect("anonymous sign-in")
            .user
            .uid()
            .to_string();

        let hello = functions
            .https_callable::<serde_json::Value, serde_json::Value>("helloWorld")
            .expect("callable reference");
        let response = hello
            .call_async(&serde_json::json!({ "message": "firebase-rs-sdk" }))
            .await
            .expect("helloWorld");
        assert_eq!(response["message"], "Hello, firebase-rs-sdk!");
        assert_eq!(response["uid"], uid, "the callable must see the signed-in user");
        assert_eq!(response["echo"]["message"], "firebase-rs-sdk");

        let failing = functions
            .https_callable::<serde_json::Value, serde_json::Value>("alwaysFails")
            .expect("callable reference");
        let err = failing
            .call_async(&serde_json::json!({}))
            .await
            .expect_err("alwaysFails must fail");
        assert_eq!(
            err.code,
            FunctionsErrorCode::FailedPrecondition,
            "unexpected error: {}",
            err.message()
        );
        assert_eq!(err.message(), "This callable always fails");
        assert_eq!(err.details().and_then(|d| d["reason"].as_str()), Some("test-fixture"));

        cleanup_auth(&auth).await;
    } else if let Some(name) = &config.test_callable {
        let callable = functions
            .https_callable::<serde_json::Value, serde_json::Value>(name)
            .expect("callable reference");
        match callable
            .call_async(&serde_json::json!({ "message": "hello from firebase-rs-sdk" }))
            .await
        {
            Ok(response) => eprintln!("callable {name} responded: {response}"),
            Err(err) if err.code == FunctionsErrorCode::NotFound => skip(
                test,
                &format!(
                    "FIREBASE_TEST_CALLABLE names `{name}` but no such function is deployed in us-central1. \
                     Deploy it or unset the variable / delete the secret."
                ),
                err.message(),
            ),
            Err(err) => panic!("callable {name} failed: {err}"),
        }
    } else {
        eprintln!(
            "functions: run scripts/emulator_test.sh or set FIREBASE_TEST_CALLABLE=<name> to exercise a callable"
        );
    }
    delete_app(&app).await.expect("delete_app");
}
