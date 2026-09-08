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
use firebase_rs_sdk::auth::{
    auth_for_app, initialize_auth, register_auth_component, AuthError, AuthErrorCode, AuthPersistence, FilePersistence,
    User,
};
use serde_json::{json, Value};

use firebase_rs_sdk::database::error::DatabaseErrorCode;
use firebase_rs_sdk::database::{connect_database_emulator, get_database};
use firebase_rs_sdk::firestore::{
    get_firestore, FieldPath, FilterOperator, Firestore, FirestoreClient, FirestoreErrorCode, FirestoreValue,
    OrderDirection, ValueKind,
};
use firebase_rs_sdk::functions::error::FunctionsErrorCode;
use firebase_rs_sdk::functions::{get_functions, register_functions_component};
use firebase_rs_sdk::installations::{delete_installations, get_installations};
use firebase_rs_sdk::remote_config::{get_remote_config, FetchStatus, RemoteConfigValueSource};
use firebase_rs_sdk::storage::{
    get_storage_for_app, ListOptions, SettableMetadata, StorageErrorCode, StringFormat, UploadTaskState,
};

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
    database: Option<String>,
    storage: Option<String>,
    functions: Option<String>,
}

impl EmulatorHosts {
    fn from_env() -> Self {
        Self {
            auth: read_env("FIREBASE_AUTH_EMULATOR_HOST"),
            firestore: read_env("FIRESTORE_EMULATOR_HOST"),
            database: read_env("FIREBASE_DATABASE_EMULATOR_HOST"),
            storage: read_env("FIREBASE_STORAGE_EMULATOR_HOST"),
            functions: read_env("FIREBASE_FUNCTIONS_EMULATOR_HOST"),
        }
    }

    fn any(&self) -> bool {
        self.auth.is_some()
            || self.firestore.is_some()
            || self.database.is_some()
            || self.storage.is_some()
            || self.functions.is_some()
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

/// Polls `condition` for up to five seconds, panicking with `message` if it never becomes true.
async fn wait_for<F>(mut condition: F, message: &str)
where
    F: FnMut() -> bool,
{
    for _ in 0..50 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting: {message}");
}

/// Resolves the Realtime Database for `app`, routed to its emulator when one is configured.
async fn database_for(config: &LiveConfig, app: &FirebaseApp) -> Arc<firebase_rs_sdk::database::Database> {
    let database = get_database(Some(app.clone())).await.expect("database service");
    if let Some(host) = &config.emulators.database {
        let (host, port) = split_host_port(host, 9000);
        connect_database_emulator(&database, &host, port).expect("connect database emulator");
    }
    database
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
            "REQUIRES AN INDEX",
            "This query needs a composite index on the online project (the emulator does not enforce indexes). \
             Create it with the console link in the error, then rerun.",
        ),
        (
            "REQUIRES MULTIPLE INDEXES",
            "This query needs composite indexes on the online project (the emulator does not enforce indexes). \
             Create them with the console link in the error, then rerun.",
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
        _ => "Other",
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

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_composite_filters_delete_field_and_dotted_names() {
    let test = "firestore_composite_filters_delete_field_and_dotted_names";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-filters").await;
    let client = &live.client;
    let marker = format!("filters-{}", nonce());
    let field = |name: &str| FieldPath::from_dot_separated(name).expect("field path");

    let cities = [
        ("Lima", "PE", 10_000_000),
        ("Cusco", "PE", 430_000),
        ("Quito", "EC", 2_800_000),
        ("Loja", "EC", 200_000),
    ];
    for (name, country, population) in cities {
        let mut data = BTreeMap::new();
        data.insert("marker".to_string(), FirestoreValue::from_string(marker.clone()));
        data.insert("name".to_string(), FirestoreValue::from_string(name));
        data.insert("country".to_string(), FirestoreValue::from_string(country));
        data.insert("population".to_string(), FirestoreValue::from_integer(population));
        if let Err(err) = client
            .set_doc(&format!("{LIVE_COLLECTION}/{marker}-{name}"), data, None)
            .await
        {
            if live.skip_if_unprovisioned(test, &err) {
                live.teardown().await;
                return;
            }
            panic!("seed failed: {err}");
        }
    }
    let base = live.firestore.collection(LIVE_COLLECTION).expect("collection").query();
    let mine = base
        .where_field(
            field("marker"),
            FilterOperator::Equal,
            FirestoreValue::from_string(marker.clone()),
        )
        .expect("marker filter");
    let names = |snapshot: &firebase_rs_sdk::firestore::QuerySnapshot| -> Vec<String> {
        let mut names: Vec<String> = snapshot
            .documents()
            .iter()
            .filter_map(|d| d.data().and_then(|m| field_string(m, "name")))
            .collect();
        names.sort();
        names
    };

    // OR over one field.
    let query = mine
        .where_filter(firebase_rs_sdk::firestore::or(vec![
            firebase_rs_sdk::firestore::where_filter(
                field("name"),
                FilterOperator::Equal,
                FirestoreValue::from_string("Lima"),
            ),
            firebase_rs_sdk::firestore::where_filter(
                field("name"),
                FilterOperator::Equal,
                FirestoreValue::from_string("Quito"),
            ),
        ]))
        .expect("or filter");
    let or_result = match client.get_docs(&query).await {
        Ok(snapshot) => snapshot,
        Err(err) if live.skip_if_unprovisioned(test, &err) => {
            cleanup_marker(client, &mine).await;
            live.teardown().await;
            return;
        }
        Err(err) => panic!("or query failed: {err}"),
    };
    assert_eq!(names(&or_result), vec!["Lima", "Quito"]);

    // AND of an equality and an OR that mixes fields and operators.
    let query = mine
        .where_filter(firebase_rs_sdk::firestore::and(vec![
            firebase_rs_sdk::firestore::where_filter(
                field("country"),
                FilterOperator::Equal,
                FirestoreValue::from_string("EC"),
            ),
            firebase_rs_sdk::firestore::or(vec![
                firebase_rs_sdk::firestore::where_filter(
                    field("population"),
                    FilterOperator::GreaterThan,
                    FirestoreValue::from_integer(1_000_000),
                ),
                firebase_rs_sdk::firestore::where_filter(
                    field("name"),
                    FilterOperator::Equal,
                    FirestoreValue::from_string("Loja"),
                ),
            ]),
        ]))
        .expect("and/or filter");
    let and_or_result = match client.get_docs(&query).await {
        Ok(snapshot) => snapshot,
        Err(err) if live.skip_if_unprovisioned(test, &err) => {
            cleanup_marker(client, &mine).await;
            live.teardown().await;
            return;
        }
        Err(err) => panic!("and/or query failed: {err}"),
    };
    assert_eq!(names(&and_or_result), vec!["Loja", "Quito"]);

    // deleteField through update, then through a merge set; plain set must refuse it.
    let lima = format!("{LIVE_COLLECTION}/{marker}-Lima");
    let mut patch = BTreeMap::new();
    patch.insert("population".to_string(), FirestoreValue::delete_field());
    client.update_doc(&lima, patch).await.expect("update with delete_field");
    let after = client.get_doc(&lima).await.unwrap();
    assert!(after.data().unwrap().get("population").is_none(), "population must be deleted");
    assert_eq!(
        field_string(after.data().unwrap(), "name").as_deref(),
        Some("Lima"),
        "other fields survive"
    );
    let mut merge = BTreeMap::new();
    merge.insert("country".to_string(), FirestoreValue::delete_field());
    client
        .set_doc(&lima, merge.clone(), Some(firebase_rs_sdk::firestore::SetOptions::merge_all()))
        .await
        .expect("merge set with delete_field");
    assert!(client
        .get_doc(&lima)
        .await
        .unwrap()
        .data()
        .unwrap()
        .get("country")
        .is_none());
    let err = client
        .set_doc(&lima, merge, None)
        .await
        .expect_err("plain set rejects delete_field");
    assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);

    // A field literally named `a.b` next to a nested `a.b`: string paths address the nested one,
    // `FieldPath::new` addresses the literal one, and both filter correctly.
    let dotted = format!("{LIVE_COLLECTION}/{marker}-dotted");
    let mut data = BTreeMap::new();
    data.insert("marker".to_string(), FirestoreValue::from_string(marker.clone()));
    data.insert("a.b".to_string(), FirestoreValue::from_string("literal"));
    let mut nested = BTreeMap::new();
    nested.insert("b".to_string(), FirestoreValue::from_string("nested"));
    data.insert("a".to_string(), FirestoreValue::from_map(nested));
    client.set_doc(&dotted, data, None).await.expect("dotted seed");
    let mut patch = BTreeMap::new();
    patch.insert("a.b".to_string(), FirestoreValue::from_string("literal-updated"));
    let mut nested = BTreeMap::new();
    nested.insert("b".to_string(), FirestoreValue::from_string("nested-updated"));
    patch.insert("a".to_string(), FirestoreValue::from_map(nested));
    client.update_doc(&dotted, patch).await.expect("dotted update");
    let doc = client.get_doc(&dotted).await.unwrap();
    assert_eq!(
        doc.get(FieldPath::new(["a.b"]).unwrap())
            .unwrap()
            .map(|v| v.kind().clone()),
        Some(ValueKind::String("literal-updated".into()))
    );
    assert_eq!(
        doc.get(field("a.b")).unwrap().map(|v| v.kind().clone()),
        Some(ValueKind::String("nested-updated".into()))
    );
    let by_literal = mine
        .where_field(
            FieldPath::new(["a.b"]).unwrap(),
            FilterOperator::Equal,
            FirestoreValue::from_string("literal-updated"),
        )
        .unwrap();
    assert_eq!(client.get_docs(&by_literal).await.unwrap().len(), 1);
    let by_nested = mine
        .where_field(
            field("a.b"),
            FilterOperator::Equal,
            FirestoreValue::from_string("nested-updated"),
        )
        .unwrap();
    assert_eq!(client.get_docs(&by_nested).await.unwrap().len(), 1);

    cleanup_marker(client, &mine).await;
    live.teardown().await;
}

/// Deletes every document matched by `query` (a marker-scoped query in these tests).
async fn cleanup_marker(client: &FirestoreClient, query: &firebase_rs_sdk::firestore::Query) {
    if let Ok(snapshot) = client.get_docs(query).await {
        for doc in snapshot.documents() {
            let _ = client.delete_doc(&format!("{LIVE_COLLECTION}/{}", doc.id())).await;
        }
    }
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn firestore_snapshot_cursor_pagination() {
    let test = "firestore_snapshot_cursor_pagination";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-pages").await;
    let client = &live.client;
    let marker = format!("pages-{}", nonce());
    let field = |name: &str| FieldPath::from_dot_separated(name).expect("field path");

    // Five documents; `group` has ties so the implicit `__name__` tiebreak matters.
    for index in 0..5i64 {
        let mut data = BTreeMap::new();
        data.insert("marker".to_string(), FirestoreValue::from_string(marker.clone()));
        data.insert("index".to_string(), FirestoreValue::from_integer(index));
        data.insert(
            "group".to_string(),
            FirestoreValue::from_string(if index < 3 { "a" } else { "b" }),
        );
        if let Err(err) = client
            .set_doc(&format!("{LIVE_COLLECTION}/{marker}-{index}"), data, None)
            .await
        {
            if live.skip_if_unprovisioned(test, &err) {
                live.teardown().await;
                return;
            }
            panic!("seed failed: {err}");
        }
    }
    let mine = live
        .firestore
        .collection(LIVE_COLLECTION)
        .expect("collection")
        .query()
        .where_field(
            field("marker"),
            FilterOperator::Equal,
            FirestoreValue::from_string(marker.clone()),
        )
        .expect("marker filter");
    let indexes = |snapshot: &firebase_rs_sdk::firestore::QuerySnapshot| -> Vec<i64> {
        snapshot
            .documents()
            .iter()
            .filter_map(|d| d.data().and_then(|m| field_integer(m, "index")))
            .collect()
    };

    // Page through by `index`, two at a time, using the last document of each page as the cursor.
    let ordered = mine
        .order_by(field("index"), OrderDirection::Ascending)
        .unwrap()
        .limit(2)
        .unwrap();
    let mut pages = Vec::new();
    let mut cursor: Option<firebase_rs_sdk::firestore::DocumentSnapshot> = None;
    loop {
        let query = match &cursor {
            Some(last) => ordered.start_after_snapshot(last).unwrap(),
            None => ordered.clone(),
        };
        let page = match client.get_docs(&query).await {
            Ok(page) => page,
            Err(err) if live.skip_if_unprovisioned(test, &err) => {
                cleanup_marker(client, &mine).await;
                live.teardown().await;
                return;
            }
            Err(err) => panic!("page failed: {err}"),
        };
        if page.is_empty() {
            break;
        }
        pages.push(indexes(&page));
        cursor = page.documents().last().cloned();
    }
    assert_eq!(pages, vec![vec![0, 1], vec![2, 3], vec![4]]);

    // Ties on `group`: the cursor carries the document name, so paging never repeats or skips.
    let by_group = mine
        .order_by(field("group"), OrderDirection::Ascending)
        .unwrap()
        .limit(2)
        .unwrap();
    let first = client.get_docs(&by_group).await.unwrap();
    let second = client
        .get_docs(
            &by_group
                .start_after_snapshot(first.documents().last().unwrap())
                .unwrap(),
        )
        .await
        .unwrap();
    let third = client
        .get_docs(
            &by_group
                .start_after_snapshot(second.documents().last().unwrap())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut seen: Vec<i64> = [indexes(&first), indexes(&second), indexes(&third)].concat();
    assert_eq!(seen.len(), 5, "pages: {seen:?}");
    seen.sort();
    assert_eq!(seen, vec![0, 1, 2, 3, 4]);

    // end_before / start_at variants.
    let all = client
        .get_docs(&mine.order_by(field("index"), OrderDirection::Ascending).unwrap())
        .await
        .unwrap();
    let third_doc = &all.documents()[2];
    let before = client
        .get_docs(
            &mine
                .order_by(field("index"), OrderDirection::Ascending)
                .unwrap()
                .end_before_snapshot(third_doc)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(indexes(&before), vec![0, 1]);
    let from = client
        .get_docs(
            &mine
                .order_by(field("index"), OrderDirection::Ascending)
                .unwrap()
                .start_at_snapshot(third_doc)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(indexes(&from), vec![2, 3, 4]);

    cleanup_marker(client, &mine).await;
    live.teardown().await;
}

async fn cleanup_auth(auth: &std::sync::Arc<firebase_rs_sdk::auth::Auth>) {
    if auth.current_user().is_some() {
        if let Err(err) = auth.delete_user().await {
            eprintln!("warning: failed to delete temporary anonymous user: {err}");
        }
    }
}

/// Watches a query over the Firestore `Listen` gRPC stream and checks that writes made through the
/// REST path come back as document changes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn firestore_query_on_snapshot_streams_changes() {
    let test = "firestore_query_on_snapshot_streams_changes";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-listen").await;
    let client = &live.client;
    let marker = format!("listen-{}", nonce());
    let field = |name: &str| FieldPath::from_dot_separated(name).expect("field path");

    let query = live
        .firestore
        .collection(LIVE_COLLECTION)
        .expect("collection")
        .query()
        .where_field(
            field("marker"),
            FilterOperator::Equal,
            FirestoreValue::from_string(marker.clone()),
        )
        .expect("marker filter")
        .order_by(field("index"), OrderDirection::Ascending)
        .expect("order by index");

    // Every snapshot is recorded as (document ids, doc changes, from_cache).
    type Snapshots = Arc<std::sync::Mutex<Vec<(Vec<String>, Vec<(String, String)>, bool)>>>;
    let snapshots: Snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let errors: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let snapshot_recorder = Arc::clone(&snapshots);
    let error_recorder = Arc::clone(&errors);

    let registration = client
        .on_snapshot(&query, move |result| match result {
            Ok(snapshot) => {
                let ids = snapshot
                    .documents()
                    .iter()
                    .map(|doc| doc.id().to_string())
                    .collect::<Vec<_>>();
                let changes = snapshot
                    .doc_changes()
                    .iter()
                    .map(|change| (format!("{:?}", change.change_type()), change.doc().id().to_string()))
                    .collect::<Vec<_>>();
                snapshot_recorder
                    .lock()
                    .expect("lock")
                    .push((ids, changes, snapshot.from_cache()));
            }
            Err(error) => error_recorder.lock().expect("lock").push(error.to_string()),
        })
        .expect("attach listener");

    let snapshot_count = || snapshots.lock().expect("lock").len();
    let fail_on_error = || {
        let errors = errors.lock().expect("lock");
        assert!(errors.is_empty(), "listener reported errors: {errors:?}");
    };

    // The first snapshot arrives once the target is in sync, even though nothing matches yet.
    // A backend that needs a composite index for this query rejects the target instead; that is a
    // provisioning gap, not an SDK bug, so report it the way the other query tests do.
    wait_for(
        || snapshot_count() >= 1 || !errors.lock().expect("lock").is_empty(),
        "the initial snapshot must arrive",
    )
    .await;
    if let Some(error) = errors.lock().expect("lock").first().cloned() {
        if let Some(reason) = provisioning_skip_reason(&error) {
            skip(test, &reason, &error);
            registration.remove();
            live.teardown().await;
            return;
        }
        panic!("listener failed: {error}");
    }
    fail_on_error();
    {
        let recorded = snapshots.lock().expect("lock");
        let (ids, changes, from_cache) = &recorded[0];
        assert!(ids.is_empty(), "the query matches nothing yet, got {ids:?}");
        assert!(changes.is_empty());
        assert!(!from_cache, "a synced listener is not serving from cache");
    }

    let write = |suffix: &str, index: i64| {
        let mut data = BTreeMap::new();
        data.insert("marker".to_string(), FirestoreValue::from_string(marker.clone()));
        data.insert("index".to_string(), FirestoreValue::from_integer(index));
        let path = format!("{LIVE_COLLECTION}/{marker}-{suffix}");
        async move { client.set_doc(&path, data, None).await }
    };

    if let Err(err) = write("a", 1).await {
        if live.skip_if_unprovisioned(test, &err) {
            registration.remove();
            live.teardown().await;
            return;
        }
        panic!("write failed: {err}");
    }
    wait_for(|| snapshot_count() >= 2, "a new document must reach the listener").await;
    {
        let recorded = snapshots.lock().expect("lock");
        let (ids, changes, _) = recorded.last().expect("snapshot");
        assert_eq!(ids, &vec![format!("{marker}-a")]);
        assert_eq!(changes, &vec![("Added".to_string(), format!("{marker}-a"))]);
    }

    write("b", 2).await.expect("write b");
    wait_for(|| snapshot_count() >= 3, "the second document must reach the listener").await;
    {
        let recorded = snapshots.lock().expect("lock");
        let (ids, changes, _) = recorded.last().expect("snapshot");
        assert_eq!(
            ids,
            &vec![format!("{marker}-a"), format!("{marker}-b")],
            "documents are ordered by the query's orderBy"
        );
        assert_eq!(changes, &vec![("Added".to_string(), format!("{marker}-b"))]);
    }

    // Reordering: `a` moves behind `b`, which the JS SDK reports as a modification.
    write("a", 3).await.expect("rewrite a");
    wait_for(|| snapshot_count() >= 4, "an update must reach the listener").await;
    {
        let recorded = snapshots.lock().expect("lock");
        let (ids, changes, _) = recorded.last().expect("snapshot");
        assert_eq!(ids, &vec![format!("{marker}-b"), format!("{marker}-a")]);
        assert!(
            changes
                .iter()
                .any(|(kind, id)| kind == "Modified" && id == &format!("{marker}-a")),
            "expected a Modified change, got {changes:?}"
        );
    }

    client
        .delete_doc(&format!("{LIVE_COLLECTION}/{marker}-b"))
        .await
        .expect("delete b");
    wait_for(|| snapshot_count() >= 5, "a delete must reach the listener").await;
    {
        let recorded = snapshots.lock().expect("lock");
        let (ids, changes, _) = recorded.last().expect("snapshot");
        assert_eq!(ids, &vec![format!("{marker}-a")]);
        assert_eq!(changes, &vec![("Removed".to_string(), format!("{marker}-b"))]);
    }

    // Documents that do not match the query never reach this listener.
    let mut unrelated = BTreeMap::new();
    unrelated.insert("marker".to_string(), FirestoreValue::from_string(format!("{marker}-other")));
    unrelated.insert("index".to_string(), FirestoreValue::from_integer(9));
    client
        .set_doc(&format!("{LIVE_COLLECTION}/{marker}-unrelated"), unrelated, None)
        .await
        .expect("unrelated write");

    // Detaching stops the stream.
    registration.remove();
    let seen_before_detach = snapshot_count();
    write("c", 4).await.expect("write after detach");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        snapshot_count(),
        seen_before_detach,
        "a removed listener must not receive further snapshots"
    );
    fail_on_error();

    cleanup_marker(client, &query).await;
    let _ = client
        .delete_doc(&format!("{LIVE_COLLECTION}/{marker}-unrelated"))
        .await;
    let _ = client.delete_doc(&format!("{LIVE_COLLECTION}/{marker}-c")).await;
    live.teardown().await;
}

/// Watches a single document and a location the rules deny.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn firestore_document_on_snapshot_and_permission_errors() {
    let test = "firestore_document_on_snapshot_and_permission_errors";
    let Some(config) = require_config(test) else {
        return;
    };
    let live = LiveFirestore::connect(&config, "fs-doc-listen").await;
    let client = &live.client;
    let marker = format!("doc-listen-{}", nonce());
    let path = format!("{LIVE_COLLECTION}/{marker}");
    let reference = live.firestore.doc(&path).expect("document reference");

    type DocSnapshots = Arc<std::sync::Mutex<Vec<(bool, Option<i64>)>>>;
    let snapshots: DocSnapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&snapshots);
    let registration = client
        .on_document_snapshot(&reference, move |result| {
            if let Ok(snapshot) = result {
                let value = snapshot.data().and_then(|data| field_integer(data, "index"));
                recorder.lock().expect("lock").push((snapshot.exists(), value));
            }
        })
        .expect("attach document listener");

    let count = || snapshots.lock().expect("lock").len();
    wait_for(|| count() >= 1, "the initial document snapshot must arrive").await;
    assert_eq!(
        snapshots.lock().expect("lock")[0],
        (false, None),
        "a missing document is reported as not existing"
    );

    let mut data = BTreeMap::new();
    data.insert("marker".to_string(), FirestoreValue::from_string(marker.clone()));
    data.insert("index".to_string(), FirestoreValue::from_integer(1));
    if let Err(err) = client.set_doc(&path, data.clone(), None).await {
        if live.skip_if_unprovisioned(test, &err) {
            registration.remove();
            live.teardown().await;
            return;
        }
        panic!("write failed: {err}");
    }
    wait_for(|| count() >= 2, "the created document must reach the listener").await;
    assert_eq!(snapshots.lock().expect("lock")[1], (true, Some(1)));

    data.insert("index".to_string(), FirestoreValue::from_integer(2));
    client.set_doc(&path, data, None).await.expect("update");
    wait_for(|| count() >= 3, "the updated document must reach the listener").await;
    assert_eq!(snapshots.lock().expect("lock")[2], (true, Some(2)));

    client.delete_doc(&path).await.expect("delete");
    wait_for(|| count() >= 4, "the delete must reach the listener").await;
    assert_eq!(snapshots.lock().expect("lock")[3], (false, None));
    registration.remove();

    // A listener the rules reject fails instead of hanging: the backend removes the target and
    // reports the cause, which the SDK surfaces on the callback.
    let denied = live
        .firestore
        .collection("forbidden_for_rust_sdk_tests")
        .expect("collection")
        .query();
    let errors: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let error_recorder = Arc::clone(&errors);
    let _denied_registration = client
        .on_snapshot(&denied, move |result| {
            if let Err(error) = result {
                error_recorder
                    .lock()
                    .expect("lock")
                    .push(error.code.as_str().to_string());
            }
        })
        .expect("attach denied listener");
    wait_for(
        || !errors.lock().expect("lock").is_empty(),
        "a listener the rules deny must report an error",
    )
    .await;
    assert_eq!(
        errors.lock().expect("lock").first().map(String::as_str),
        Some("firestore/permission-denied")
    );

    live.teardown().await;
}

// ---------------------------------------------------------------------------------------------
// Realtime Database
// ---------------------------------------------------------------------------------------------

/// Skips unless the Realtime Database emulator is configured; the online project has no RTDB
/// instance provisioned.
fn require_database(test: &str, config: &LiveConfig) -> bool {
    if config.emulators.database.is_none() {
        skip(
            test,
            "needs the Realtime Database emulator (scripts/emulator_test.sh); the project has no \
             database instance",
            "n/a",
        );
        return false;
    }
    true
}

/// Signs in anonymously (the emulator rules require `auth != null`) and returns the database.
async fn database_test_setup(
    config: &LiveConfig,
    label: &str,
) -> (
    FirebaseApp,
    Arc<firebase_rs_sdk::auth::Auth>,
    Arc<firebase_rs_sdk::database::Database>,
) {
    let app = live_app(config, label).await;
    let auth = auth_for(config, &app);
    auth.sign_in_anonymously().await.expect("anonymous sign-in");
    let database = database_for(config, &app).await;
    (app, auth, database)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn database_writes_reads_queries_and_server_values() {
    let test = "database_writes_reads_queries_and_server_values";
    let Some(config) = require_config(test) else {
        return;
    };
    if !require_database(test, &config) {
        return;
    }
    let (app, auth, database) = database_test_setup(&config, "database-rest").await;

    let root_path = format!("rust_sdk_live_tests/{}", nonce());
    let base = database.reference(&root_path).expect("reference");

    // --- set / get / update / remove ---------------------------------------------------------
    let profile = base.child("profile").expect("child");
    profile.set(json!({"name": "Ada", "score": 10})).await.expect("set");
    assert_eq!(profile.get().await.expect("get"), json!({"name": "Ada", "score": 10}));

    let mut updates = serde_json::Map::new();
    updates.insert("score".to_string(), json!(11));
    updates.insert("nested/flag".to_string(), json!(true));
    profile.update(updates).await.expect("update");
    assert_eq!(
        profile.get().await.expect("get"),
        json!({"name": "Ada", "score": 11, "nested": {"flag": true}})
    );

    profile.child("nested").expect("child").remove().await.expect("remove");
    assert_eq!(profile.get().await.expect("get"), json!({"name": "Ada", "score": 11}));

    // `push` mints ordered keys, mirroring the JS SDK's push IDs.
    let messages = base.child("messages").expect("child");
    let first = messages.push_with_value(json!("first")).await.expect("push");
    let second = messages.push_with_value(json!("second")).await.expect("push");
    let first_key = first.key().expect("key").to_string();
    let second_key = second.key().expect("key").to_string();
    assert!(first_key < second_key, "push IDs must sort chronologically");
    assert_eq!(
        messages.get().await.expect("get"),
        json!({ first_key.clone(): "first", second_key: "second" })
    );

    // --- server values --------------------------------------------------------------------
    let stamped = base.child("stamped").expect("child");
    stamped
        .set(json!({"at": firebase_rs_sdk::database::server_timestamp()}))
        .await
        .expect("set with server timestamp");
    let stored = stamped.get().await.expect("get");
    let at = stored
        .get("at")
        .and_then(|value| value.as_u64())
        .expect("server timestamp must resolve to a number");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    assert!(
        at.abs_diff(now) < 5 * 60 * 1000,
        "server timestamp {at} is not close to now ({now})"
    );

    // Increments are applied by the server, so concurrent bumps cannot lose updates the way a
    // read-modify-write on the client would.
    let counter = base.child("counter").expect("child");
    counter.set(json!(10)).await.expect("seed counter");
    let bumps = (0..5).map(|_| {
        let counter = counter.clone();
        async move {
            counter
                .set(firebase_rs_sdk::database::increment(1.0))
                .await
                .expect("increment");
        }
    });
    futures::future::join_all(bumps).await;
    assert_eq!(
        counter.get().await.expect("get"),
        json!(15.0),
        "five concurrent increments must all land"
    );

    // --- queries --------------------------------------------------------------------------
    // `players` is the one path the emulator rules index (see firebase-emulator/database.rules.json).
    let players = base.child("players").expect("child");
    players
        .set(json!({
            "ada": {"name": "Ada", "score": 30},
            "bob": {"name": "Bob", "score": 10},
            "cy": {"name": "Cy", "score": 20}
        }))
        .await
        .expect("seed players");

    let top_two = players
        .order_by_child("score")
        .expect("order_by_child")
        .limit_to_last(2)
        .expect("limit_to_last")
        .get()
        .await
        .expect("query");
    assert_eq!(
        top_two.as_object().map(|map| map.len()),
        Some(2),
        "limit_to_last must trim the result: {top_two}"
    );
    assert!(top_two.get("ada").is_some() && top_two.get("cy").is_some(), "got {top_two}");

    let above_fifteen = players
        .order_by_child("score")
        .expect("order_by_child")
        .start_at(json!(15))
        .expect("start_at")
        .get()
        .await
        .expect("query");
    assert!(
        above_fifteen.get("bob").is_none() && above_fifteen.get("ada").is_some(),
        "start_at must drop lower scores: {above_fifteen}"
    );

    let exactly_bob = players
        .order_by_child("name")
        .expect("order_by_child")
        .equal_to(json!("Bob"))
        .expect("equal_to")
        .get()
        .await
        .expect("query");
    assert_eq!(exactly_bob.as_object().map(|map| map.len()), Some(1), "got {exactly_bob}");

    let first_key_only = players
        .order_by_key()
        .expect("order_by_key")
        .limit_to_first(1)
        .expect("limit_to_first")
        .get()
        .await
        .expect("query");
    assert!(first_key_only.get("ada").is_some(), "got {first_key_only}");

    // Ordering by an unindexed child is a server-side error, not silently unordered data.
    let err = players
        .order_by_child("rank")
        .expect("order_by_child")
        .get()
        .await
        .expect_err("unindexed queries must fail");
    assert!(
        err.to_string().contains("indexOn"),
        "the error should name the missing index: {err}"
    );

    base.remove().await.expect("cleanup");
    cleanup_auth(&auth).await;
    delete_app(&app).await.expect("delete_app");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn database_listeners_receive_remote_writes() {
    let test = "database_listeners_receive_remote_writes";
    let Some(config) = require_config(test) else {
        return;
    };
    if !require_database(test, &config) {
        return;
    }
    let (app, auth, database) = database_test_setup(&config, "database-listen").await;
    // A second client so the updates genuinely travel through the server.
    let (writer_app, writer_auth, writer_database) = database_test_setup(&config, "database-writer").await;

    let root_path = format!("rust_sdk_live_tests/{}", nonce());
    let watched = database.reference(&root_path).expect("reference");
    let remote = writer_database.reference(&root_path).expect("reference");

    let values: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&values);
    let registration = watched
        .on_value(move |event| {
            if let Ok(snapshot) = event {
                recorder.lock().expect("lock").push(snapshot.value().clone());
            }
        })
        .await
        .expect("on_value");

    let children: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let child_recorder = Arc::clone(&children);
    let child_registration = watched
        .on_child_added(move |event| {
            if let Ok(event) = event {
                child_recorder.lock().expect("lock").push((
                    event.snapshot.key().unwrap_or_default().to_string(),
                    event.snapshot.value().clone(),
                ));
            }
        })
        .await
        .expect("on_child_added");

    // The initial event reports "no data yet".
    assert_eq!(values.lock().expect("lock").as_slice(), &[Value::Null]);

    remote
        .child("a")
        .expect("child")
        .set(json!(1))
        .await
        .expect("remote set");
    wait_for(
        || values.lock().expect("lock").len() >= 2,
        "value listener must see the remote write",
    )
    .await;
    assert_eq!(values.lock().expect("lock").last().cloned(), Some(json!({"a": 1})));

    remote
        .child("b")
        .expect("child")
        .set(json!(2))
        .await
        .expect("remote set");
    wait_for(
        || children.lock().expect("lock").len() >= 2,
        "child_added must fire for the second child",
    )
    .await;
    assert_eq!(
        children.lock().expect("lock").as_slice(),
        &[("a".to_string(), json!(1)), ("b".to_string(), json!(2))]
    );

    // A location kept in sync by a listener reads back from that live view.
    assert_eq!(watched.get().await.expect("get"), json!({"a": 1, "b": 2}));

    // Detaching stops the events.
    registration.detach();
    child_registration.detach();
    let seen_before = values.lock().expect("lock").len();
    remote
        .child("c")
        .expect("child")
        .set(json!(3))
        .await
        .expect("remote set");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        values.lock().expect("lock").len(),
        seen_before,
        "a detached listener must not receive events"
    );
    // ... and reads go back to the server.
    assert_eq!(watched.get().await.expect("get"), json!({"a": 1, "b": 2, "c": 3}));

    remote.remove().await.expect("cleanup");
    cleanup_auth(&auth).await;
    cleanup_auth(&writer_auth).await;
    delete_app(&app).await.expect("delete_app");
    delete_app(&writer_app).await.expect("delete_app");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn database_transactions_use_compare_and_set() {
    let test = "database_transactions_use_compare_and_set";
    let Some(config) = require_config(test) else {
        return;
    };
    if !require_database(test, &config) {
        return;
    }
    let (app, auth, database) = database_test_setup(&config, "database-txn").await;
    let (other_app, other_auth, other_database) = database_test_setup(&config, "database-txn-other").await;

    let root_path = format!("rust_sdk_live_tests/{}", nonce());
    let counter = database.reference(&format!("{root_path}/counter")).expect("reference");
    let other_counter = other_database
        .reference(&format!("{root_path}/counter"))
        .expect("reference");

    // A transaction on a location that does not exist yet sees `null`.
    let result = counter
        .run_transaction(|current| {
            assert_eq!(current, Value::Null, "a missing node must be reported as null");
            Some(json!(1))
        })
        .await
        .expect("transaction");
    assert!(result.committed);
    assert_eq!(result.snapshot.value(), &json!(1));

    // Two clients incrementing at once must both land: the loser retries against fresh data.
    let mine = counter.run_transaction(|current| Some(json!(current.as_i64().unwrap_or(0) + 1)));
    let theirs = other_counter.run_transaction(|current| Some(json!(current.as_i64().unwrap_or(0) + 1)));
    let (mine, theirs) = futures::future::join(mine, theirs).await;
    assert!(mine.expect("transaction").committed);
    assert!(theirs.expect("transaction").committed);
    assert_eq!(
        counter.get().await.expect("get"),
        json!(3),
        "concurrent transactions must not lose an update"
    );

    // Returning `None` aborts without writing.
    let aborted = counter.run_transaction(|_| None).await.expect("transaction");
    assert!(!aborted.committed);
    assert_eq!(aborted.snapshot.value(), &json!(3));
    assert_eq!(counter.get().await.expect("get"), json!(3));

    database
        .reference(&root_path)
        .expect("reference")
        .remove()
        .await
        .expect("cleanup");
    cleanup_auth(&auth).await;
    cleanup_auth(&other_auth).await;
    delete_app(&app).await.expect("delete_app");
    delete_app(&other_app).await.expect("delete_app");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn database_rules_report_permission_denied() {
    let test = "database_rules_report_permission_denied";
    let Some(config) = require_config(test) else {
        return;
    };
    if !require_database(test, &config) {
        return;
    }
    let (app, auth, database) = database_test_setup(&config, "database-rules").await;

    // Nothing outside `rust_sdk_live_tests/` is readable or writable.
    let forbidden = database.reference("forbidden/area").expect("reference");
    let err = forbidden.set(json!(1)).await.expect_err("rules must deny the write");
    assert_eq!(err.code, DatabaseErrorCode::PermissionDenied, "got {err}");
    let err = forbidden.get().await.expect_err("rules must deny the read");
    assert_eq!(err.code, DatabaseErrorCode::PermissionDenied, "got {err}");

    // Attaching a listener to a location the rules hide fails immediately instead of hanging on a
    // listen the server will never answer.
    let err = forbidden
        .on_value(|_| {})
        .await
        .expect_err("rules must deny the listen");
    assert_eq!(err.code, DatabaseErrorCode::PermissionDenied, "got {err}");

    // Reads and writes inside the test area still work for the signed-in user.
    let allowed = database
        .reference(&format!("rust_sdk_live_tests/{}/ok", nonce()))
        .expect("reference");
    allowed.set(json!("visible")).await.expect("write inside the rules");
    assert_eq!(allowed.get().await.expect("get"), json!("visible"));
    allowed.remove().await.expect("cleanup");

    cleanup_auth(&auth).await;
    delete_app(&app).await.expect("delete_app");
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

/// Drives the resumable upload protocol end to end: chunked progress notifications, pausing and
/// resuming a half-finished upload, and cancelling one so no object is ever created.
#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn storage_resumable_upload_progress_pause_and_cancel() {
    let test = "storage_resumable_upload_progress_pause_and_cancel";
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
    let app = live_app(&config, "storage-resumable").await;
    let auth = auth_for(&config, &app);
    auth.sign_in_anonymously().await.expect("anonymous sign-in");
    let storage = storage_for(&config, &app).await;
    let root = storage.root_reference().expect("root reference");

    let prefix = format!("rust_sdk_live_tests/resumable-{}", nonce());
    // 1 MiB is comfortably above the 256 KiB threshold that switches to the resumable protocol,
    // so the upload spans several chunks and progress is observable.
    let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
    let total = payload.len() as u64;

    // --- progress notifications ------------------------------------------------------------
    let progress_path = format!("{prefix}/progress.bin");
    let progress_ref = root.child(&progress_path);
    let task = progress_ref
        .upload_bytes_resumable(payload.clone(), None)
        .expect("resumable task");
    assert!(task.is_resumable(), "1 MiB must use the resumable protocol");
    assert_eq!(task.state(), UploadTaskState::Pending);

    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&observed);
    let unsubscribe = task.on_state_changed(move |snapshot| {
        recorder
            .lock()
            .expect("observer lock")
            .push((snapshot.state, snapshot.bytes_transferred));
    });
    let handle = task.handle();
    let metadata = task.run_to_completion().await.expect("resumable upload");
    unsubscribe();

    assert_eq!(metadata.size, Some(total));
    assert_eq!(metadata.name.as_deref(), Some(progress_path.as_str()));
    assert_eq!(handle.state(), UploadTaskState::Completed);
    assert_eq!(handle.bytes_transferred(), total);

    let events = observed.lock().expect("observer lock").clone();
    assert!(events.len() >= 2, "expected several chunks, saw {events:?}");
    let mut previous = 0;
    for (_, bytes) in &events {
        assert!(*bytes >= previous, "progress went backwards: {events:?}");
        previous = *bytes;
    }
    assert!(
        events.iter().any(|(_, bytes)| *bytes > 0 && *bytes < total),
        "expected a partial chunk event, saw {events:?}"
    );
    assert_eq!(
        events.last().map(|(state, bytes)| (*state, *bytes)),
        Some((UploadTaskState::Completed, total)),
        "the final event must report success"
    );

    let downloaded = progress_ref.get_bytes(None).await.expect("get_bytes");
    assert_eq!(downloaded, payload, "uploaded bytes must round-trip");

    // --- pause, ask the server where it got to, resume -------------------------------------
    let paused_path = format!("{prefix}/paused.bin");
    let paused_ref = root.child(&paused_path);
    let mut task = paused_ref
        .upload_bytes_resumable(payload.clone(), None)
        .expect("resumable task");
    let handle = task.handle();

    assert!(
        task.upload_next().await.expect("first chunk").is_none(),
        "1 MiB cannot finish in a single chunk"
    );
    let after_first_chunk = handle.bytes_transferred();
    assert!(
        after_first_chunk > 0 && after_first_chunk < total,
        "unexpected offset after the first chunk: {after_first_chunk}"
    );

    assert!(handle.pause(), "a running task can be paused");
    assert_eq!(handle.state(), UploadTaskState::Paused);
    assert!(task.upload_next().await.expect("paused task").is_none());
    assert_eq!(handle.bytes_transferred(), after_first_chunk, "a paused task must not upload");

    let server_offset = task.refresh_status().await.expect("resumable session status");
    assert_eq!(server_offset, after_first_chunk, "the server must agree with the local offset");

    assert!(handle.resume(), "a paused task can be resumed");
    let metadata = task.run_to_completion().await.expect("resumed upload");
    assert_eq!(metadata.size, Some(total));
    assert_eq!(handle.state(), UploadTaskState::Completed);
    let downloaded = paused_ref.get_bytes(None).await.expect("get_bytes");
    assert_eq!(downloaded.len(), payload.len());

    // --- cancel mid-flight ------------------------------------------------------------------
    let canceled_path = format!("{prefix}/canceled.bin");
    let canceled_ref = root.child(&canceled_path);
    let task = canceled_ref
        .upload_bytes_resumable(payload.clone(), None)
        .expect("resumable task");
    let handle = task.handle();
    let canceller = handle.clone();
    let unsubscribe = task.on_state_changed(move |snapshot| {
        if snapshot.bytes_transferred > 0 && !snapshot.state.is_terminal() {
            canceller.cancel();
        }
    });
    let err = task.run_to_completion().await.expect_err("a canceled upload must fail");
    unsubscribe();

    assert_eq!(err.code, StorageErrorCode::Canceled, "got {err}");
    assert_eq!(err.code_str(), "storage/canceled");
    assert_eq!(handle.state(), UploadTaskState::Canceled);
    assert!(handle.bytes_transferred() < total, "the upload must stop before the last chunk");
    let err = canceled_ref
        .get_metadata()
        .await
        .expect_err("a canceled session must not produce an object");
    assert_eq!(err.code, StorageErrorCode::ObjectNotFound, "got {err}");

    // Cancelling also tears down the session server-side: querying it afterwards reports
    // `storage/canceled` instead of a byte offset.
    let discarded_path = format!("{prefix}/discarded.bin");
    let discarded_ref = root.child(&discarded_path);
    let mut task = discarded_ref
        .upload_bytes_resumable(payload.clone(), None)
        .expect("resumable task");
    let handle = task.handle();
    assert!(task.upload_next().await.expect("first chunk").is_none());
    assert!(handle.upload_session_url().is_some(), "a session must be open");
    assert!(handle.cancel());
    let err = task.upload_next().await.expect_err("a canceled task must fail");
    assert_eq!(err.code, StorageErrorCode::Canceled, "got {err}");
    let err = task
        .refresh_status()
        .await
        .expect_err("the canceled session must be gone");
    assert_eq!(err.code, StorageErrorCode::Canceled, "got {err}");
    let err = discarded_ref
        .get_metadata()
        .await
        .expect_err("a canceled session must not produce an object");
    assert_eq!(err.code, StorageErrorCode::ObjectNotFound, "got {err}");

    progress_ref.delete_object().await.expect("delete progress.bin");
    paused_ref.delete_object().await.expect("delete paused.bin");
    cleanup_auth(&auth).await;
    delete_app(&app).await.expect("delete_app");
}

/// Covers `list` pagination (page sizes, page tokens, prefixes vs items) and metadata updates.
#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn storage_list_pagination_and_metadata_updates() {
    let test = "storage_list_pagination_and_metadata_updates";
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
    let app = live_app(&config, "storage-list").await;
    let auth = auth_for(&config, &app);
    auth.sign_in_anonymously().await.expect("anonymous sign-in");
    let storage = storage_for(&config, &app).await;
    let root = storage.root_reference().expect("root reference");

    let prefix = format!("rust_sdk_live_tests/list-{}", nonce());
    for name in ["a.txt", "b.txt", "c.txt"] {
        root.child(&format!("{prefix}/{name}"))
            .upload_string(name, StringFormat::Raw, None)
            .await
            .expect("upload");
    }
    let nested = root.child(&format!("{prefix}/sub/d.txt"));
    nested
        .upload_string("d", StringFormat::Raw, None)
        .await
        .expect("upload");

    let folder = root.child(&prefix);

    // Page sizes are validated client-side, exactly like the Web SDK.
    let err = folder
        .list(Some(ListOptions {
            max_results: Some(0),
            page_token: None,
        }))
        .await
        .expect_err("0 is not a valid page size");
    assert_eq!(err.code_str(), "storage/invalid-argument", "got {err}");

    let first = folder
        .list(Some(ListOptions {
            max_results: Some(2),
            page_token: None,
        }))
        .await
        .expect("first page");
    assert_eq!(
        first.items.iter().map(|item| item.name()).collect::<Vec<_>>(),
        vec!["a.txt".to_string(), "b.txt".to_string()],
        "a page holds at most max_results items"
    );
    assert!(
        first.prefixes.iter().any(|p| p.full_path() == format!("{prefix}/sub")),
        "nested objects surface as prefixes, got {:?}",
        first.prefixes
    );
    let token = first
        .next_page_token
        .clone()
        .expect("a truncated listing carries a page token");

    let second = folder
        .list(Some(ListOptions {
            max_results: Some(2),
            page_token: Some(token),
        }))
        .await
        .expect("second page");
    assert_eq!(
        second.items.iter().map(|item| item.name()).collect::<Vec<_>>(),
        vec!["c.txt".to_string()],
        "the page token must resume where the first page stopped"
    );
    assert!(second.next_page_token.is_none(), "the last page must not carry a token");

    let all = folder.list_all().await.expect("list_all");
    assert_eq!(
        all.items.iter().map(|item| item.name()).collect::<Vec<_>>(),
        vec!["a.txt".to_string(), "b.txt".to_string(), "c.txt".to_string()],
        "list_all walks every page"
    );
    assert!(all.prefixes.iter().any(|p| p.full_path() == format!("{prefix}/sub")));

    // --- metadata updates -------------------------------------------------------------------
    let target = root.child(&format!("{prefix}/a.txt"));
    let mut update = SettableMetadata::new()
        .with_content_type("text/markdown")
        .with_cache_control("max-age=42");
    update.insert_custom_metadata("purpose", "live-test");
    let updated = target.update_metadata(update).await.expect("update_metadata");
    assert_eq!(updated.content_type.as_deref(), Some("text/markdown"));
    assert_eq!(updated.cache_control.as_deref(), Some("max-age=42"));
    assert_eq!(
        updated
            .custom_metadata
            .as_ref()
            .and_then(|custom| custom.get("purpose"))
            .map(String::as_str),
        Some("live-test")
    );

    let fetched = target.get_metadata().await.expect("get_metadata");
    assert_eq!(fetched.content_type.as_deref(), Some("text/markdown"));
    assert_eq!(fetched.cache_control.as_deref(), Some("max-age=42"));
    assert_eq!(
        fetched
            .custom_metadata
            .as_ref()
            .and_then(|custom| custom.get("purpose"))
            .map(String::as_str),
        Some("live-test"),
        "custom metadata must survive a round-trip"
    );

    for reference in all.items.iter().chain(std::iter::once(&nested)) {
        reference.delete_object().await.expect("delete_object");
    }
    let empty = folder.list_all().await.expect("list_all after cleanup");
    assert!(empty.items.is_empty(), "cleanup must remove every object");

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

        // Streaming callable: chunks arrive as server-sent events, then the final result.
        let streaming = functions
            .https_callable::<serde_json::Value, serde_json::Value>("streamNumbers")
            .expect("callable reference");
        let mut stream = streaming
            .stream_async(&serde_json::json!({ "count": 4 }))
            .await
            .expect("stream_async");
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next_message().await.expect("next_message") {
            chunks.push(chunk["n"].as_i64().expect("n"));
        }
        assert_eq!(chunks, vec![1, 2, 3, 4]);
        let result = stream.result().await.expect("stream result");
        assert_eq!(result["total"], 10);
        assert_eq!(result["streamed"], true, "server must see Accept: text/event-stream");
        // The same function answers a plain call without streaming.
        let plain = streaming
            .call_async(&serde_json::json!({ "count": 2 }))
            .await
            .expect("plain call to a streaming function");
        assert_eq!(plain["total"], 3);
        assert_eq!(plain["streamed"], false);

        // An error raised after the first chunk surfaces as a typed error from the stream.
        let failing_stream = functions
            .https_callable::<serde_json::Value, serde_json::Value>("streamThenFail")
            .expect("callable reference");
        let mut stream = failing_stream.stream_async(&serde_json::json!({})).await.expect("open");
        let first = stream.next_message().await.expect("first chunk").expect("one chunk");
        assert_eq!(first["n"], 1);
        let err = stream.next_message().await.expect_err("error after the chunk");
        assert_eq!(err.code, FunctionsErrorCode::ResourceExhausted, "got {err}");
        assert_eq!(err.message(), "stream failed midway");
        assert_eq!(err.details().and_then(|d| d["after"].as_i64()), Some(1));

        // HttpsCallableOptions.timeout maps to deadline-exceeded.
        let slow = functions
            .https_callable_with_options::<serde_json::Value, serde_json::Value>(
                "slowEcho",
                firebase_rs_sdk::functions::HttpsCallableOptions {
                    timeout: Duration::from_millis(400),
                    ..Default::default()
                },
            )
            .expect("callable reference");
        let err = slow
            .call_async(&serde_json::json!({ "delayMs": 3000 }))
            .await
            .expect_err("must time out");
        assert_eq!(err.code, FunctionsErrorCode::DeadlineExceeded, "got {err}");

        // httpsCallableFromURL: the same helloWorld through its absolute emulator URL.
        let host = config.emulators.functions.as_deref().unwrap();
        let by_url = functions
            .https_callable_from_url::<serde_json::Value, serde_json::Value>(&format!(
                "http://{host}/{}/us-central1/helloWorld",
                config.project_id
            ))
            .expect("callable from url");
        let response = by_url
            .call_async(&serde_json::json!({ "message": "by-url" }))
            .await
            .expect("helloWorld by URL");
        assert_eq!(response["message"], "Hello, by-url!");
        assert_eq!(response["uid"], uid, "auth headers are attached to URL callables too");

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

// ---------------------------------------------------------------------------------------------
// Authentication flows that need the Auth emulator's out-of-band code, SMS code, custom token
// and fake IdP support. All of these skip without the emulator.
// ---------------------------------------------------------------------------------------------

/// Client for the Auth emulator's administrative REST endpoints
/// (`/emulator/v1/projects/{project}/...`), which expose what production would send by email
/// or SMS.
struct EmulatorAuthAdmin {
    base: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct OobCode {
    email: String,
    #[serde(rename = "oobCode")]
    oob_code: String,
    #[serde(rename = "oobLink")]
    oob_link: String,
    #[serde(rename = "requestType")]
    request_type: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SmsCode {
    code: String,
    #[serde(rename = "phoneNumber")]
    phone_number: String,
}

impl EmulatorAuthAdmin {
    /// Deletes an account the way an administrator would, without touching the SDK's state.
    async fn delete_account(&self, uid: &str) {
        let url = format!(
            "{}/accounts:delete",
            self.base
                .replace("/emulator/v1/projects/", "/identitytoolkit.googleapis.com/v1/projects/")
        );
        let response = self
            .client
            .post(&url)
            .header("Authorization", "Bearer owner")
            .json(&serde_json::json!({ "localId": uid }))
            .send()
            .await
            .expect("delete account");
        assert!(response.status().is_success(), "failed to delete {uid}: {}", response.status());
    }

    fn new(config: &LiveConfig) -> Option<Self> {
        let host = config.emulators.auth.as_ref()?;
        Some(Self {
            base: format!("http://{host}/emulator/v1/projects/{}", config.project_id),
            client: reqwest::Client::new(),
        })
    }

    async fn oob_codes(&self) -> Vec<OobCode> {
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(rename = "oobCodes", default)]
            oob_codes: Vec<OobCode>,
        }
        let body: Body = self
            .client
            .get(format!("{}/oobCodes", self.base))
            .send()
            .await
            .expect("emulator oobCodes")
            .json()
            .await
            .expect("oobCodes json");
        body.oob_codes
    }

    /// The most recent code of `request_type` issued for `email`.
    async fn latest_oob(&self, email: &str, request_type: &str) -> OobCode {
        self.oob_codes()
            .await
            .into_iter()
            .filter(|code| code.email.eq_ignore_ascii_case(email) && code.request_type == request_type)
            .last()
            .unwrap_or_else(|| panic!("no {request_type} code for {email} in the emulator"))
    }

    async fn latest_sms_code(&self, phone_number: &str) -> String {
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(rename = "verificationCodes", default)]
            codes: Vec<SmsCode>,
        }
        let body: Body = self
            .client
            .get(format!("{}/verificationCodes", self.base))
            .send()
            .await
            .expect("emulator verificationCodes")
            .json()
            .await
            .expect("verificationCodes json");
        body.codes
            .into_iter()
            .filter(|c| c.phone_number == phone_number)
            .last()
            .map(|c| c.code)
            .unwrap_or_else(|| panic!("no SMS code for {phone_number} in the emulator"))
    }
}

/// The emulator ignores reCAPTCHA, so any token satisfies the phone endpoints.
struct EmulatorVerifier;

impl firebase_rs_sdk::auth::ApplicationVerifier for EmulatorVerifier {
    fn verify(&self) -> firebase_rs_sdk::auth::AuthResult<String> {
        Ok("emulator-recaptcha-token".to_string())
    }

    fn verifier_type(&self) -> &str {
        "recaptcha"
    }
}

/// Builds an unsigned custom token (`alg: none`), which the emulator accepts in place of one
/// minted by the Admin SDK.
fn unsigned_custom_token(uid: &str, claims: serde_json::Value) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = serde_json::json!({
        "iss": "firebase-adminsdk@demo.iam.gserviceaccount.com",
        "sub": "firebase-adminsdk@demo.iam.gserviceaccount.com",
        "aud": "https://identitytoolkit.googleapis.com/google.identity.identitytoolkit.v1.IdentityToolkit",
        "iat": now,
        "exp": now + 3600,
        "uid": uid,
        "claims": claims,
    });
    let payload = URL_SAFE_NO_PAD.encode(payload.to_string());
    format!("{header}.{payload}.")
}

/// Fake Google credential for the emulator: the "ID token" is a JSON profile.
fn emulator_google_credential(sub: &str, email: &str, name: &str) -> firebase_rs_sdk::auth::AuthCredential {
    let profile = serde_json::json!({ "sub": sub, "email": email, "email_verified": true, "name": name });
    firebase_rs_sdk::auth::GoogleAuthProvider::credential(Some(&profile.to_string()), None)
}

/// Common setup for the emulator-only auth tests. Returns `None` (after printing a skip) when
/// the Auth emulator is not running.
/// A signed-in session written to disk must come back after the process that created it is gone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_session_survives_a_restart() {
    let test = "auth_emulator_session_survives_a_restart";
    let Some(config) = require_config(test) else {
        return;
    };
    if config.emulators.auth.is_none() {
        skip(test, "needs the Auth emulator (scripts/emulator_test.sh)", "n/a");
        return;
    }

    let store = temp_auth_store("restart");
    let email = format!("persist-{}@example.com", nonce());
    let password = "correct-horse-battery";

    // --- first run: sign in and let persistence record the session ---------------------------
    let first_app = live_app(&config, "auth-persist-first").await;
    let first_auth = initialize_auth(first_app.clone(), Arc::new(FilePersistence::new(&store)))
        .await
        .expect("initialize auth");
    first_auth.connect_emulator(&format!("http://{}", config.emulators.auth.clone().unwrap()));
    let credential = first_auth
        .create_user_with_email_and_password(&email, password)
        .await
        .expect("create user");
    first_auth
        .update_profile(Some("Ada Lovelace"), None)
        .await
        .expect("update profile");
    let uid = credential.user.uid().to_string();
    assert!(store.exists(), "signing in must write the session to {store:?}");
    delete_app(&first_app).await.expect("delete_app");

    // --- second run: a brand new app reading the same store ------------------------------------
    let second_app = live_app(&config, "auth-persist-second").await;
    let restored_events: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let second_auth = initialize_auth(second_app.clone(), Arc::new(FilePersistence::new(&store)))
        .await
        .expect("initialize auth");

    let user = second_auth.current_user().expect("the session must be restored");
    assert_eq!(user.uid(), uid, "the same account must come back");
    assert_eq!(
        user.info().email.as_deref(),
        Some(email.as_str()),
        "the restored profile is the real one, not a stub"
    );
    assert_eq!(user.info().display_name.as_deref(), Some("Ada Lovelace"));
    assert!(!user.is_anonymous());

    // The restored session is usable: a cached token is available and can be refreshed.
    let token = user.get_id_token(false).await.expect("cached id token");
    assert!(!token.is_empty());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let refreshed = user.get_id_token(true).await.expect("forced refresh");
    assert!(!refreshed.is_empty());

    // Listeners attached after the restore see the signed-in user straight away.
    let recorder = Arc::clone(&restored_events);
    let unsubscribe = second_auth.on_auth_state_changed(move |user: &Option<Arc<User>>| {
        recorder
            .lock()
            .expect("lock")
            .push(user.as_ref().map(|user| user.uid().to_string()));
    });
    assert_eq!(
        restored_events.lock().expect("lock").first().cloned(),
        Some(Some(uid.clone())),
        "a new listener is told about the restored user"
    );
    unsubscribe();

    // --- signing out clears the store, so the next start is signed out -------------------------
    second_auth.delete_user().await.expect("delete user");
    delete_app(&second_app).await.expect("delete_app");

    let third_app = live_app(&config, "auth-persist-third").await;
    let third_auth = initialize_auth(third_app.clone(), Arc::new(FilePersistence::new(&store)))
        .await
        .expect("initialize auth");
    assert!(
        third_auth.current_user().is_none(),
        "after signing out there is nothing to restore"
    );
    delete_app(&third_app).await.expect("delete_app");
    let _ = std::fs::remove_file(&store);
}

/// A stored session whose account is gone must not come back as a signed-in user.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_restored_session_is_dropped_when_the_account_is_gone() {
    let test = "auth_emulator_restored_session_is_dropped_when_the_account_is_gone";
    let Some(config) = require_config(test) else {
        return;
    };
    let Some(admin) = EmulatorAuthAdmin::new(&config) else {
        skip(test, "needs the Auth emulator (scripts/emulator_test.sh)", "n/a");
        return;
    };

    let store = temp_auth_store("revoked");
    let email = format!("revoked-{}@example.com", nonce());

    let first_app = live_app(&config, "auth-revoked-first").await;
    let first_auth = initialize_auth(first_app.clone(), Arc::new(FilePersistence::new(&store)))
        .await
        .expect("initialize auth");
    first_auth.connect_emulator(&format!("http://{}", config.emulators.auth.clone().unwrap()));
    let credential = first_auth
        .create_user_with_email_and_password(&email, "correct-horse-battery")
        .await
        .expect("create user");
    let uid = credential.user.uid().to_string();
    assert!(store.exists());
    delete_app(&first_app).await.expect("delete_app");

    // Delete the account behind the SDK's back, the way an administrator would.
    admin.delete_account(&uid).await;

    let second_app = live_app(&config, "auth-revoked-second").await;
    let second_auth = initialize_auth(second_app.clone(), Arc::new(FilePersistence::new(&store)))
        .await
        .expect("initialize auth");
    assert!(
        second_auth.current_user().is_none(),
        "a session the backend no longer honours must not be restored"
    );
    assert!(
        FilePersistence::new(&store).get().expect("read store").is_none(),
        "the dead session must be cleared from storage as well"
    );

    delete_app(&second_app).await.expect("delete_app");
    let _ = std::fs::remove_file(&store);
}

/// `set_persistence` moves a live session to another store.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_set_persistence_moves_the_session() {
    let test = "auth_emulator_set_persistence_moves_the_session";
    let Some(config) = require_config(test) else {
        return;
    };
    if config.emulators.auth.is_none() {
        skip(test, "needs the Auth emulator (scripts/emulator_test.sh)", "n/a");
        return;
    }

    let store = temp_auth_store("moved");
    let app = live_app(&config, "auth-move-store").await;
    let auth = auth_for(&config, &app);
    auth.sign_in_anonymously().await.expect("anonymous sign-in");
    let uid = auth.current_user().expect("user").uid().to_string();
    assert!(!store.exists(), "the default backend keeps the session in memory only");

    auth.set_persistence(Arc::new(FilePersistence::new(&store)))
        .expect("set_persistence");
    let stored = FilePersistence::new(&store)
        .get()
        .expect("read store")
        .expect("the current session moves to the new store");
    assert_eq!(stored.user_id, uid);
    assert!(stored.is_anonymous, "anonymous sessions restore as anonymous");

    // And a fresh app started against that store picks the same user back up.
    let next_app = live_app(&config, "auth-move-store-next").await;
    let next_auth = initialize_auth(next_app.clone(), Arc::new(FilePersistence::new(&store)))
        .await
        .expect("initialize auth");
    let restored = next_auth.current_user().expect("restored user");
    assert_eq!(restored.uid(), uid);
    assert!(restored.is_anonymous());

    next_auth.delete_user().await.expect("delete user");
    delete_app(&next_app).await.expect("delete_app");
    delete_app(&app).await.expect("delete_app");
    let _ = std::fs::remove_file(&store);
}

/// A scratch file for one persistence test, in the system temp directory.
fn temp_auth_store(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("firebase-rs-sdk-auth-{label}-{}.json", nonce()));
    path
}

async fn emulator_auth(
    test: &str,
) -> Option<(
    LiveConfig,
    FirebaseApp,
    Arc<User>,
    Arc<firebase_rs_sdk::auth::Auth>,
    EmulatorAuthAdmin,
)> {
    let config = require_config(test)?;
    let Some(admin) = EmulatorAuthAdmin::new(&config) else {
        skip(test, "needs the Auth emulator (scripts/emulator_test.sh)", "n/a");
        return None;
    };
    let app = live_app(&config, "auth-emu").await;
    let auth = auth_for(&config, &app);
    // Seed an email/password user so every flow starts from a real account.
    let email = format!("emu-{}@example.com", nonce());
    let credential = auth
        .create_user_with_email_and_password(&email, "correct-horse-battery")
        .await
        .expect("create user");
    Some((config, app, credential.user.clone(), auth, admin))
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_password_reset_round_trip() {
    let test = "auth_emulator_password_reset_round_trip";
    let Some((_config, app, user, auth, admin)) = emulator_auth(test).await else {
        return;
    };
    let email = user.info().email.clone().unwrap();
    auth.sign_out();

    auth.send_password_reset_email(&email)
        .await
        .expect("send_password_reset_email");
    let oob = admin.latest_oob(&email, "PASSWORD_RESET").await;
    assert!(oob.oob_link.contains("mode=resetPassword"), "link: {}", oob.oob_link);

    let verified_email = auth
        .verify_password_reset_code(&oob.oob_code)
        .await
        .expect("verify code");
    assert_eq!(verified_email.to_lowercase(), email.to_lowercase());
    let info = auth.check_action_code(&oob.oob_code).await.expect("check_action_code");
    assert_eq!(info.operation, firebase_rs_sdk::auth::ActionCodeOperation::PasswordReset);

    auth.confirm_password_reset(&oob.oob_code, "new-password-42")
        .await
        .expect("confirm_password_reset");

    let stale = auth
        .sign_in_with_email_and_password(&email, "correct-horse-battery")
        .await
        .expect_err("old password must be rejected");
    assert!(
        matches!(
            stale.code(),
            Some(AuthErrorCode::WrongPassword | AuthErrorCode::InvalidCredential)
        ),
        "got {stale}"
    );
    let fresh = auth
        .sign_in_with_email_and_password(&email, "new-password-42")
        .await
        .expect("new password works");
    assert_eq!(fresh.user.uid(), user.uid());

    // A used code is rejected.
    let reused = auth
        .verify_password_reset_code(&oob.oob_code)
        .await
        .expect_err("used code");
    assert!(
        matches!(
            reused.code(),
            Some(AuthErrorCode::InvalidActionCode | AuthErrorCode::ExpiredActionCode)
        ),
        "got {reused}"
    );

    assert_eq!(auth.fetch_sign_in_methods_for_email(&email).await.unwrap(), vec!["password"]);
    assert!(auth
        .fetch_sign_in_methods_for_email("nobody-here@example.com")
        .await
        .unwrap()
        .is_empty());

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_email_verification_and_reload() {
    let test = "auth_emulator_email_verification_and_reload";
    let Some((_config, app, user, auth, admin)) = emulator_auth(test).await else {
        return;
    };
    let email = user.info().email.clone().unwrap();
    assert!(!user.email_verified());

    auth.send_email_verification().await.expect("send_email_verification");
    let oob = admin.latest_oob(&email, "VERIFY_EMAIL").await;
    let info = auth.check_action_code(&oob.oob_code).await.expect("check_action_code");
    assert_eq!(info.operation, firebase_rs_sdk::auth::ActionCodeOperation::VerifyEmail);
    auth.apply_action_code(&oob.oob_code).await.expect("apply_action_code");

    // The cached user is stale until reloaded.
    assert!(!auth.current_user().unwrap().email_verified());
    let reloaded = auth.reload().await.expect("reload");
    assert!(reloaded.email_verified(), "reload must pick up the verified flag");
    assert_eq!(reloaded.uid(), user.uid());
    assert!(reloaded.metadata().creation_time.is_some(), "metadata.creation_time");
    assert!(reloaded.metadata().last_sign_in_time.is_some(), "metadata.last_sign_in_time");
    let providers: Vec<&str> = reloaded
        .provider_data()
        .iter()
        .map(|p| p.provider_id.as_str())
        .collect();
    assert_eq!(providers, vec!["password"]);
    assert_eq!(reloaded.provider_data()[0].email.as_deref(), Some(email.as_str()));
    assert!(Arc::ptr_eq(&reloaded, &auth.current_user().unwrap()));
    // Tokens survive the reload.
    assert!(auth.get_token(false).await.unwrap().is_some());

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_email_link_sign_in() {
    let test = "auth_emulator_email_link_sign_in";
    let Some((_config, app, _user, auth, admin)) = emulator_auth(test).await else {
        return;
    };
    auth.sign_out();
    let email = format!("link-{}@example.com", nonce());
    let settings = firebase_rs_sdk::auth::ActionCodeSettings {
        url: "http://localhost/finish-sign-in".to_string(),
        handle_code_in_app: true,
        ..Default::default()
    };
    auth.send_sign_in_link_to_email(&email, &settings)
        .await
        .expect("send_sign_in_link_to_email");
    let oob = admin.latest_oob(&email, "EMAIL_SIGNIN").await;
    assert!(auth.is_sign_in_with_email_link(&oob.oob_link), "link: {}", oob.oob_link);
    assert!(!auth.is_sign_in_with_email_link("http://localhost/not-a-link"));

    let credential = auth
        .sign_in_with_email_link(&email, &oob.oob_link)
        .await
        .expect("sign_in_with_email_link");
    assert_eq!(
        credential.user.info().email.as_deref().map(str::to_lowercase),
        Some(email.to_lowercase())
    );
    let reloaded = auth.reload().await.unwrap();
    assert!(reloaded.email_verified(), "email-link users are verified");
    let methods = auth.fetch_sign_in_methods_for_email(&email).await.unwrap();
    assert!(methods.contains(&"emailLink".to_string()), "methods: {methods:?}");

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_custom_token_claims_and_id_token_result() {
    let test = "auth_emulator_custom_token_claims_and_id_token_result";
    let Some((_config, app, _user, auth, _admin)) = emulator_auth(test).await else {
        return;
    };
    auth.delete_user().await.expect("drop seed user");
    let uid = format!("custom-{}", nonce());
    let token = unsigned_custom_token(&uid, serde_json::json!({ "role": "admin", "level": 7 }));
    let credential = auth
        .sign_in_with_custom_token(&token)
        .await
        .expect("custom token sign-in");
    assert_eq!(credential.user.uid(), uid);

    let result = auth.get_id_token_result(false).await.expect("get_id_token_result");
    assert_eq!(result.claims["role"], "admin");
    assert_eq!(result.claims["level"], 7);
    assert_eq!(result.claims["user_id"], uid);
    assert_eq!(result.sign_in_provider.as_deref(), Some("custom"));
    assert!(result.expiration_time.is_some() && result.issued_at_time.is_some() && result.auth_time.is_some());
    assert_eq!(result.token, auth.get_token(false).await.unwrap().unwrap());

    let refreshed = auth.get_id_token_result(true).await.expect("forced refresh");
    assert_eq!(refreshed.claims["role"], "admin", "custom claims survive a refresh");

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_profile_updates_reauth_and_token_listeners() {
    let test = "auth_emulator_profile_updates_reauth_and_token_listeners";
    let Some((_config, app, user, auth, admin)) = emulator_auth(test).await else {
        return;
    };
    let email = user.info().email.clone().unwrap();

    let state_events = Arc::new(Mutex::new(0usize));
    let token_events = Arc::new(Mutex::new(0usize));
    let (s, t) = (Arc::clone(&state_events), Arc::clone(&token_events));
    let _unsub_state = auth.on_auth_state_changed(move |_: &Option<Arc<User>>| *s.lock().unwrap() += 1);
    let _unsub_token = auth.on_id_token_changed(move |_: &Option<Arc<User>>| *t.lock().unwrap() += 1);
    assert_eq!((*state_events.lock().unwrap(), *token_events.lock().unwrap()), (1, 1), "primed");

    // A token refresh fires only the ID-token listener.
    auth.get_token(true).await.expect("refresh");
    assert_eq!(*state_events.lock().unwrap(), 1);
    assert_eq!(*token_events.lock().unwrap(), 2);

    let updated = auth
        .update_profile(Some("Ada Lovelace"), Some("https://example.com/ada.png"))
        .await
        .expect("update_profile");
    assert_eq!(updated.info().display_name.as_deref(), Some("Ada Lovelace"));
    let reloaded = auth.reload().await.unwrap();
    assert_eq!(reloaded.info().display_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(reloaded.info().photo_url.as_deref(), Some("https://example.com/ada.png"));

    // Reauthenticate, then change the password and the email.
    auth.reauthenticate_with_password(&email, "correct-horse-battery")
        .await
        .expect("reauthenticate");
    auth.update_password("even-better-password")
        .await
        .expect("update_password");
    let new_email = format!("renamed-{}@example.com", nonce());
    auth.update_email(&new_email).await.expect("update_email");
    auth.sign_out();
    let back = auth
        .sign_in_with_email_and_password(&new_email, "even-better-password")
        .await
        .expect("sign in with the new email and password");
    assert_eq!(back.user.uid(), user.uid());

    // verifyBeforeUpdateEmail: the new address only applies after the code is used.
    let pending_email = format!("pending-{}@example.com", nonce());
    auth.verify_before_update_email(&pending_email, None)
        .await
        .expect("verify_before_update_email");
    assert_eq!(auth.reload().await.unwrap().info().email.as_deref(), Some(new_email.as_str()));
    let oob = // The emulator files this code under the account's current address, not the pending one.
    admin.latest_oob(&new_email, "VERIFY_AND_CHANGE_EMAIL").await;
    assert!(oob.oob_link.contains("mode=verifyAndChangeEmail"), "link: {}", oob.oob_link);
    auth.apply_action_code(&oob.oob_code)
        .await
        .expect("apply change-email code");
    let renamed = auth.reload().await.unwrap();
    assert_eq!(renamed.info().email.as_deref(), Some(pending_email.as_str()));
    assert!(renamed.email_verified());

    assert!(
        *token_events.lock().unwrap() > *state_events.lock().unwrap(),
        "token listener must fire more often than the auth-state listener"
    );

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_idp_credential_sign_in_link_and_unlink() {
    let test = "auth_emulator_idp_credential_sign_in_link_and_unlink";
    let Some((_config, app, seed, auth, _admin)) = emulator_auth(test).await else {
        return;
    };
    let seed_email = seed.info().email.clone().unwrap();
    auth.sign_out();

    // Sign in with a (fake) Google credential.
    let google_email = format!("google-{}@example.com", nonce());
    let credential = emulator_google_credential(&format!("g-{}", nonce()), &google_email, "Google User");
    let signed_in = auth
        .sign_in_with_oauth_credential(credential.clone())
        .await
        .expect("sign_in_with_oauth_credential");
    assert_eq!(signed_in.provider_id.as_deref(), Some("google.com"));
    assert_eq!(signed_in.user.info().email.as_deref(), Some(google_email.as_str()));
    let reloaded = auth.reload().await.unwrap();
    assert_eq!(
        reloaded
            .provider_data()
            .iter()
            .map(|p| p.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["google.com"]
    );
    assert_eq!(
        auth.fetch_sign_in_methods_for_email(&google_email).await.unwrap(),
        vec!["google.com"]
    );

    // Reauthenticate with the same credential, then delete.
    auth.reauthenticate_with_oauth_credential(credential)
        .await
        .expect("reauthenticate");
    auth.delete_user().await.expect("delete google user");

    // Link Google to the seeded password user, then unlink it again.
    auth.sign_in_with_email_and_password(&seed_email, "correct-horse-battery")
        .await
        .expect("seed sign-in");
    let link_credential =
        emulator_google_credential(&format!("g-{}", nonce()), &format!("linked-{}@example.com", nonce()), "Linked");
    let linked = auth
        .link_with_oauth_credential(link_credential)
        .await
        .expect("link_with_oauth_credential");
    assert_eq!(linked.user.uid(), seed.uid(), "linking keeps the uid");
    let mut providers: Vec<String> = auth
        .reload()
        .await
        .unwrap()
        .provider_data()
        .iter()
        .map(|p| p.provider_id.clone())
        .collect();
    providers.sort();
    assert_eq!(providers, vec!["google.com", "password"]);

    let unlinked = auth.unlink_providers(&["google.com"]).await.expect("unlink");
    assert_eq!(unlinked.uid(), seed.uid());
    let providers: Vec<String> = auth
        .reload()
        .await
        .unwrap()
        .provider_data()
        .iter()
        .map(|p| p.provider_id.clone())
        .collect();
    assert_eq!(providers, vec!["password"]);

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_anonymous_upgrade_to_email_password() {
    let test = "auth_emulator_anonymous_upgrade_to_email_password";
    let Some((_config, app, _seed, auth, _admin)) = emulator_auth(test).await else {
        return;
    };
    auth.delete_user().await.expect("drop seed user");

    let anonymous = auth.sign_in_anonymously().await.expect("anonymous");
    assert!(anonymous.user.is_anonymous());
    let uid = anonymous.user.uid().to_string();

    let email = format!("upgraded-{}@example.com", nonce());
    let upgraded = auth
        .link_with_email_and_password(&email, "upgrade-password-1")
        .await
        .expect("link_with_email_and_password");
    assert_eq!(upgraded.user.uid(), uid, "upgrade keeps the anonymous uid");
    assert!(!upgraded.user.is_anonymous());
    assert_eq!(upgraded.user.info().email.as_deref(), Some(email.as_str()));
    assert_eq!(upgraded.operation_type.as_deref(), Some("link"));

    auth.sign_out();
    let back = auth
        .sign_in_with_email_and_password(&email, "upgrade-password-1")
        .await
        .expect("password sign-in after upgrade");
    assert_eq!(back.user.uid(), uid);
    assert!(!back.user.is_anonymous());
    assert_eq!(auth.fetch_sign_in_methods_for_email(&email).await.unwrap(), vec!["password"]);

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_phone_sign_in_and_link() {
    let test = "auth_emulator_phone_sign_in_and_link";
    let Some((_config, app, seed, auth, admin)) = emulator_auth(test).await else {
        return;
    };
    let seed_email = seed.info().email.clone().unwrap();
    auth.sign_out();
    let verifier = Arc::new(EmulatorVerifier);

    // Phone sign-in: SMS code comes from the emulator instead of a handset.
    let phone = format!("+1555555{:04}", nonce().len() * 37 % 10000);
    let confirmation = auth
        .sign_in_with_phone_number(&phone, verifier.clone())
        .await
        .expect("sign_in_with_phone_number");
    let code = admin.latest_sms_code(&phone).await;
    let signed_in = confirmation.confirm(&code).await.expect("confirm");
    assert_eq!(signed_in.user.info().phone_number.as_deref(), Some(phone.as_str()));
    assert_eq!(signed_in.provider_id.as_deref(), Some("phone"));
    let phone_uid = signed_in.user.uid().to_string();
    let wrong = auth
        .sign_in_with_phone_number(&phone, verifier.clone())
        .await
        .unwrap()
        .confirm("000000")
        .await
        .expect_err("wrong code");
    assert_eq!(wrong.code(), Some(&AuthErrorCode::InvalidVerificationCode), "got {wrong}");
    // Signing in again with the right code returns the same account.
    let again = auth.sign_in_with_phone_number(&phone, verifier.clone()).await.unwrap();
    let code = admin.latest_sms_code(&phone).await;
    assert_eq!(again.confirm(&code).await.unwrap().user.uid(), phone_uid);
    auth.delete_user().await.expect("delete phone user");

    // Link a phone number to the password user.
    auth.sign_in_with_email_and_password(&seed_email, "correct-horse-battery")
        .await
        .unwrap();
    let link_phone = format!("+1555555{:04}", (nonce().len() * 53 + 1) % 10000);
    let confirmation = auth
        .link_with_phone_number(&link_phone, verifier.clone())
        .await
        .expect("link_with_phone_number");
    let code = admin.latest_sms_code(&link_phone).await;
    let linked = confirmation.confirm(&code).await.expect("confirm link");
    assert_eq!(linked.user.uid(), seed.uid());
    assert_eq!(linked.user.info().phone_number.as_deref(), Some(link_phone.as_str()));
    let mut providers: Vec<String> = auth
        .reload()
        .await
        .unwrap()
        .provider_data()
        .iter()
        .map(|p| p.provider_id.clone())
        .collect();
    providers.sort();
    assert_eq!(providers, vec!["password", "phone"]);

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}

#[tokio::test]
#[ignore = "requires live Firebase credentials"]
async fn auth_emulator_phone_multi_factor_enrollment_and_challenge() {
    let test = "auth_emulator_phone_multi_factor_enrollment_and_challenge";
    let Some((_config, app, seed, auth, admin)) = emulator_auth(test).await else {
        return;
    };
    let email = seed.info().email.clone().unwrap();
    let verifier = Arc::new(EmulatorVerifier);

    // MFA enrolment requires a verified email.
    auth.send_email_verification().await.unwrap();
    let oob = admin.latest_oob(&email, "VERIFY_EMAIL").await;
    auth.apply_action_code(&oob.oob_code).await.unwrap();
    auth.reload().await.unwrap();

    let phone = format!("+1555555{:04}", (nonce().len() * 71 + 2) % 10000);
    let multi_factor = auth.multi_factor();
    let confirmation = multi_factor
        .enroll_phone_number(&phone, verifier.clone(), Some("work phone"))
        .await
        .expect("enroll_phone_number");
    let code = admin.latest_sms_code(&phone).await;
    let enrolled = confirmation.confirm(&code).await.expect("finalize enrolment");
    assert_eq!(enrolled.user.uid(), seed.uid());
    let factors = multi_factor.enrolled_factors().await.expect("enrolled_factors");
    assert_eq!(factors.len(), 1, "factors: {factors:?}");
    assert_eq!(factors[0].display_name.as_deref(), Some("work phone"));
    let factor_uid = factors[0].uid.clone();

    // A fresh password sign-in now requires the second factor.
    auth.sign_out();
    let err = auth
        .sign_in_with_email_and_password(&email, "correct-horse-battery")
        .await
        .expect_err("MFA must be required");
    assert!(matches!(err, AuthError::MultiFactorRequired(_)), "got {err}");
    let resolver = auth.multi_factor_resolver(&err).expect("resolver");
    assert_eq!(resolver.hints().len(), 1);
    let hint = resolver.hints()[0].clone();
    assert_eq!(hint.display_name.as_deref(), Some("work phone"));

    let verification_id = resolver
        .send_phone_sign_in_code(&hint, verifier.clone())
        .await
        .expect("send_phone_sign_in_code");
    let code = admin.latest_sms_code(&phone).await;
    let assertion = firebase_rs_sdk::auth::PhoneMultiFactorGenerator::assertion(
        firebase_rs_sdk::auth::PhoneAuthCredential::new(verification_id, code),
    );
    let resolved = resolver.resolve_sign_in(assertion).await.expect("resolve_sign_in");
    assert_eq!(resolved.user.uid(), seed.uid());
    assert!(auth.current_user().is_some());
    let result = auth.get_id_token_result(false).await.unwrap();
    assert_eq!(
        result.sign_in_second_factor.as_deref(),
        Some("phone"),
        "claims: {}",
        result.claims
    );

    // Unenrol and confirm a plain password sign-in works again.
    auth.multi_factor().unenroll(&factor_uid).await.expect("unenroll");
    assert!(auth.multi_factor().enrolled_factors().await.unwrap().is_empty());
    auth.sign_out();
    auth.sign_in_with_email_and_password(&email, "correct-horse-battery")
        .await
        .expect("no second factor after unenrol");

    auth.delete_user().await.expect("cleanup");
    delete_app(&app).await.ok();
}
