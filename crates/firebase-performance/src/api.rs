use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_lock::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::constants::{
    MAX_ATTRIBUTE_NAME_LENGTH, MAX_ATTRIBUTE_VALUE_LENGTH, MAX_METRIC_NAME_LENGTH, OOB_TRACE_PAGE_LOAD_PREFIX,
    PERFORMANCE_COMPONENT_NAME, RESERVED_ATTRIBUTE_PREFIXES, RESERVED_METRIC_PREFIX,
};
use crate::error::{internal_error, invalid_argument, PerformanceResult};
use crate::instrumentation;
use crate::storage::{create_trace_store, TraceEnvelope, TraceStoreHandle};
use crate::transport::{TransportController, TransportOptions};
use firebase_core::app;
use firebase_core::app::FirebaseApp;
use firebase_core::component::types::{ComponentError, InstanceFactoryOptions};
use firebase_core::component::{ComponentContainer, Service};
use firebase_core::platform::credentials::AppCredentials;
#[cfg(all(target_arch = "wasm32", feature = "wasm-web"))]
use firebase_core::platform::environment;
use firebase_core::platform::runtime;
use firebase_core::platform::token::TokenProviderArc;
use firebase_installations::get_installations;
use log::debug;

/// User provided configuration toggles mirroring the JavaScript SDK's
/// [`PerformanceSettings`](https://github.com/firebase/firebase-js-sdk/blob/master/packages/performance/src/public_types.ts).
///
/// Leave fields as `None` to keep the app-level defaults derived from
/// [`FirebaseAppSettings::automatic_data_collection_enabled`](firebase_core::app::FirebaseAppSettings).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PerformanceSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instrumentation_enabled: Option<bool>,
}

/// Resolved runtime settings that reflect the effective data collection and
/// instrumentation flags for a [`Performance`] instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerformanceRuntimeSettings {
    data_collection_enabled: bool,
    instrumentation_enabled: bool,
}

impl PerformanceRuntimeSettings {
    fn resolve(app: &FirebaseApp, overrides: Option<&PerformanceSettings>) -> Self {
        let mut resolved = Self {
            data_collection_enabled: app.automatic_data_collection_enabled(),
            instrumentation_enabled: app.automatic_data_collection_enabled(),
        };

        if let Some(settings) = overrides {
            if let Some(value) = settings.data_collection_enabled {
                resolved.data_collection_enabled = value;
            }
            if let Some(value) = settings.instrumentation_enabled {
                resolved.instrumentation_enabled = value;
            }
        }

        resolved
    }

    pub fn data_collection_enabled(&self) -> bool {
        self.data_collection_enabled
    }

    pub fn instrumentation_enabled(&self) -> bool {
        self.instrumentation_enabled
    }
}

impl Default for PerformanceRuntimeSettings {
    fn default() -> Self {
        Self {
            data_collection_enabled: true,
            instrumentation_enabled: true,
        }
    }
}

/// Firebase Performance Monitoring entry point tied to a [`FirebaseApp`].
///
/// This struct intentionally mirrors the behaviour of the JS SDK's
/// `FirebasePerformance` controller: it holds runtime toggles, orchestrates
/// trace/HTTP instrumentation, and can be retrieved via [`get_performance`].
#[derive(Clone)]
pub struct Performance {
    inner: Arc<PerformanceInner>,
}

struct PerformanceInner {
    app: FirebaseApp,
    traces: Mutex<HashMap<String, PerformanceTrace>>,
    network_requests: Mutex<Vec<NetworkRequestRecord>>,
    settings: RwLock<PerformanceRuntimeSettings>,
    credentials: AppCredentials,
    app_check_override: StdMutex<Option<TokenProviderArc>>,
    auth: StdMutex<Option<AuthContext>>,
    trace_store: TraceStoreHandle,
    transport: StdMutex<Option<Arc<TransportController>>>,
    transport_options: Arc<RwLock<TransportOptions>>,
    installation_id: Mutex<Option<String>>,
}

/// Builder for manual traces (`Trace` in the JS SDK).
#[derive(Clone)]
pub struct TraceHandle {
    performance: Performance,
    name: Arc<str>,
    state: TraceLifecycle,
    metrics: HashMap<String, i64>,
    attributes: HashMap<String, String>,
    is_auto: bool,
}

/// Builder for manual network request instrumentation. Mirrors the JS
/// `NetworkRequestTrace` helper and records timing plus payload metadata.
#[derive(Clone)]
pub struct NetworkTraceHandle {
    performance: Performance,
    url: String,
    method: HttpMethod,
    state: NetworkLifecycle,
    request_payload_bytes: Option<u64>,
    response_payload_bytes: Option<u64>,
    response_code: Option<u16>,
    response_content_type: Option<String>,
}

/// HTTP method enum reused by [`NetworkTraceHandle`].
/// Represents the HTTP verb associated with a [`NetworkTraceHandle`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Put,
    Post,
    Delete,
    Head,
    Patch,
    Options,
    Trace,
    Connect,
    Custom(String),
}

impl HttpMethod {
    /// Returns the canonical uppercase representation used when logging.
    pub fn as_str(&self) -> &str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Put => "PUT",
            HttpMethod::Post => "POST",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Trace => "TRACE",
            HttpMethod::Connect => "CONNECT",
            HttpMethod::Custom(value) => value.as_str(),
        }
    }
}

/// Immutable snapshot of a recorded trace with timing, metrics, and attributes.
#[derive(Clone, Debug, PartialEq)]
pub struct PerformanceTrace {
    pub name: Arc<str>,
    pub start_time_us: u128,
    pub duration: Duration,
    pub metrics: HashMap<String, i64>,
    pub attributes: HashMap<String, String>,
    pub is_auto: bool,
    pub auth_uid: Option<String>,
}

/// Optional bundle of metrics and attributes passed into
/// [`TraceHandle::record`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceRecordOptions {
    pub metrics: HashMap<String, i64>,
    pub attributes: HashMap<String, String>,
}

/// Captured HTTP request metadata that mirrors the payload sent by the JS SDK
/// when batching network traces to the backend.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkRequestRecord {
    pub url: String,
    pub http_method: HttpMethod,
    pub start_time_us: u128,
    pub time_to_request_completed_us: u128,
    pub time_to_response_initiated_us: Option<u128>,
    pub time_to_response_completed_us: Option<u128>,
    pub request_payload_bytes: Option<u64>,
    pub response_payload_bytes: Option<u64>,
    pub response_code: Option<u16>,
    pub response_content_type: Option<String>,
    pub app_check_token: Option<String>,
}

#[derive(Clone, Debug)]
enum TraceLifecycle {
    Idle,
    Running { started_at: Instant, started_micros: u128 },
    Completed,
}

#[derive(Clone, Debug)]
enum NetworkLifecycle {
    Idle,
    Running {
        started_at: Instant,
        started_micros: u128,
        response_initiated: Option<Duration>,
    },
    Completed,
}

/// Reads the signed-in user's id when a trace is recorded.
///
/// Performance does not need Firebase Auth, it needs a user id, so it takes the id as a callback:
/// `perf.attach_user_id_source(Arc::new(move || auth.current_user().map(|u| u.uid().to_string())))`.
pub type UserIdSource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[derive(Clone)]
enum AuthContext {
    Source(UserIdSource),
    Static(String),
}

impl AuthContext {
    fn current_uid(&self) -> Option<String> {
        match self {
            AuthContext::Source(source) => source().filter(|uid| !uid.is_empty()),
            AuthContext::Static(uid) if uid.is_empty() => None,
            AuthContext::Static(uid) => Some(uid.clone()),
        }
    }
}

impl Performance {
    fn new(app: FirebaseApp, settings: Option<PerformanceSettings>) -> Self {
        let resolved = PerformanceRuntimeSettings::resolve(&app, settings.as_ref());
        let trace_store = create_trace_store(&app);
        let transport_options = Arc::new(RwLock::new(TransportOptions::default()));
        let credentials = AppCredentials::for_app(&app);
        let inner = PerformanceInner {
            app,
            traces: Mutex::new(HashMap::new()),
            network_requests: Mutex::new(Vec::new()),
            settings: RwLock::new(resolved),
            credentials,
            app_check_override: StdMutex::new(None),
            auth: StdMutex::new(None),
            trace_store,
            transport: StdMutex::new(None),
            transport_options,
            installation_id: Mutex::new(None),
        };
        let performance = Self { inner: Arc::new(inner) };
        performance.initialize_background_tasks();
        performance
    }

    fn initialize_background_tasks(&self) {
        instrumentation::initialize(self);
        let controller = TransportController::new(
            self.clone(),
            self.inner.trace_store.clone(),
            self.inner.transport_options.clone(),
        );
        if let Ok(mut guard) = self.inner.transport.lock() {
            *guard = Some(controller);
        }
    }

    /// Returns the [`FirebaseApp`] that owns this performance instance.
    pub fn app(&self) -> &FirebaseApp {
        &self.inner.app
    }

    /// Resolves the currently effective runtime settings.
    pub fn settings(&self) -> PerformanceRuntimeSettings {
        self.inner.settings.read().expect("settings lock poisoned").clone()
    }

    /// Applies the provided settings, overwriting only the `Some` fields.
    pub fn apply_settings(&self, settings: PerformanceSettings) {
        let mut guard = self.inner.settings.write().expect("settings lock poisoned");
        if let Some(value) = settings.data_collection_enabled {
            guard.data_collection_enabled = value;
        }
        if let Some(value) = settings.instrumentation_enabled {
            guard.instrumentation_enabled = value;
        }
    }

    /// Enables or disables manual/custom trace collection.
    pub fn set_data_collection_enabled(&self, enabled: bool) {
        self.apply_settings(PerformanceSettings {
            data_collection_enabled: Some(enabled),
            ..Default::default()
        });
    }

    /// Enables or disables automatic instrumentation (network/OOB traces).
    pub fn set_instrumentation_enabled(&self, enabled: bool) {
        self.apply_settings(PerformanceSettings {
            instrumentation_enabled: Some(enabled),
            ..Default::default()
        });
    }

    /// Returns whether manual/custom traces are currently recorded.
    pub fn data_collection_enabled(&self) -> bool {
        self.settings().data_collection_enabled()
    }

    /// Returns whether automatic instrumentation (network/OOB) is enabled.
    pub fn instrumentation_enabled(&self) -> bool {
        self.settings().instrumentation_enabled()
    }

    /// Overrides where the App Check token attached to network traces comes from.
    ///
    /// Initialising App Check is enough on its own — the token is read from the app's component
    /// container — so this is only for wiring a source of your own.
    pub fn attach_app_check(&self, provider: TokenProviderArc) {
        let mut guard = self.inner.app_check_override.lock().expect("app_check lock");
        *guard = Some(provider);
    }

    /// Drops an override installed by [`attach_app_check`](Self::attach_app_check), returning to
    /// the app's own App Check token.
    pub fn clear_app_check(&self) {
        let mut guard = self.inner.app_check_override.lock().expect("app_check lock");
        guard.take();
    }

    /// Supplies the signed-in user's id, so recorded traces can carry it (the JS SDK's `setUserId`).
    pub fn attach_user_id_source(&self, source: UserIdSource) {
        let mut guard = self.inner.auth.lock().expect("auth lock");
        *guard = Some(AuthContext::Source(source));
    }

    /// Manually overrides the authenticated user ID attribute that will be
    /// stored with subsequent traces.
    pub fn set_authenticated_user_id(&self, uid: Option<&str>) {
        let mut guard = self.inner.auth.lock().expect("auth lock");
        match uid {
            Some(value) => *guard = Some(AuthContext::Static(value.to_string())),
            None => {
                guard.take();
            }
        }
    }

    /// Clears any stored authentication context.
    pub fn clear_auth(&self) {
        let mut guard = self.inner.auth.lock().expect("auth lock");
        guard.take();
    }

    /// Creates a new manual trace. Call [`TraceHandle::start`] /
    /// [`stop`](TraceHandle::stop) to record the timing metrics.
    ///
    /// # Examples
    /// ```rust,no_run
    /// # use firebase_performance::{get_performance, PerformanceResult};
    /// # async fn demo(app: firebase_core::app::FirebaseApp) -> PerformanceResult<()> {
    /// let perf = get_performance(Some(app)).await?;
    /// let mut trace = perf.new_trace("warmup")?;
    /// trace.start()?;
    /// let _ = trace.stop().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_trace(&self, name: &str) -> PerformanceResult<TraceHandle> {
        validate_trace_name(name)?;
        Ok(TraceHandle {
            performance: self.clone(),
            name: Arc::from(name.to_string()),
            state: TraceLifecycle::Idle,
            metrics: HashMap::new(),
            attributes: HashMap::new(),
            is_auto: false,
        })
    }

    /// Creates a manual network trace, mirroring the JS SDK's
    /// `performance.traceNetworkRequest` helper.
    ///
    /// # Examples
    /// ```rust,no_run
    /// # use firebase_performance::{get_performance, HttpMethod, PerformanceResult};
    /// # async fn demo(app: firebase_core::app::FirebaseApp) -> PerformanceResult<()> {
    /// let perf = get_performance(Some(app)).await?;
    /// let mut req = perf.new_network_request("https://example.com", HttpMethod::Get)?;
    /// req.start()?;
    /// let _record = req.stop().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_network_request(&self, url: &str, method: HttpMethod) -> PerformanceResult<NetworkTraceHandle> {
        if url.trim().is_empty() {
            return Err(invalid_argument("Request URL must not be empty"));
        }
        Ok(NetworkTraceHandle {
            performance: self.clone(),
            url: url.to_string(),
            method,
            state: NetworkLifecycle::Idle,
            request_payload_bytes: None,
            response_payload_bytes: None,
            response_code: None,
            response_content_type: None,
        })
    }

    /// Returns the most recently recorded trace with the provided name.
    pub async fn recorded_trace(&self, name: &str) -> Option<PerformanceTrace> {
        self.inner.traces.lock().await.get(name).cloned()
    }

    /// Returns the latest network trace captured for the given URL (without
    /// query parameters), if any.
    pub async fn recorded_network_request(&self, url: &str) -> Option<NetworkRequestRecord> {
        let traces = self.inner.network_requests.lock().await;
        traces.iter().rev().find(|record| record.url == url).cloned()
    }

    /// Overrides the transport configuration used for batching uploads.
    pub fn configure_transport(&self, options: TransportOptions) {
        if let Ok(mut guard) = self.inner.transport_options.write() {
            *guard = options;
        }
        if let Some(controller) = self.transport_controller() {
            controller.trigger_flush();
        }
    }

    /// Forces an immediate transport flush.
    pub async fn flush_transport(&self) -> PerformanceResult<()> {
        if let Some(controller) = self.transport_controller() {
            controller.flush_once().await
        } else {
            Ok(())
        }
    }

    #[cfg_attr(not(all(feature = "wasm-web", target_arch = "wasm32")), allow(dead_code))]
    pub(crate) async fn record_auto_trace(
        self,
        name: &str,
        start_time_us: u128,
        duration: Duration,
        metrics: HashMap<String, i64>,
        attributes: HashMap<String, String>,
    ) -> PerformanceResult<()> {
        let trace = PerformanceTrace {
            name: Arc::from(name.to_string()),
            start_time_us,
            duration,
            metrics,
            attributes,
            is_auto: true,
            auth_uid: self.auth_uid(),
        };
        self.store_trace(trace).await
    }

    #[cfg_attr(not(all(feature = "wasm-web", target_arch = "wasm32")), allow(dead_code))]
    pub(crate) async fn record_auto_network(self, record: NetworkRequestRecord) -> PerformanceResult<()> {
        self.store_network_request(record).await
    }

    pub(crate) async fn installation_id(&self) -> Option<String> {
        if let Some(cached) = self.inner.installation_id.lock().await.clone() {
            return Some(cached);
        }
        let installations = get_installations(Some(self.app().clone())).ok()?;
        let fid = installations.get_id().await.ok()?;
        let mut guard = self.inner.installation_id.lock().await;
        *guard = Some(fid.clone());
        Some(fid)
    }

    async fn store_trace(&self, trace: PerformanceTrace) -> PerformanceResult<()> {
        if !self.data_collection_enabled() {
            return Ok(());
        }
        let name = trace.name.to_string();
        self.inner.traces.lock().await.insert(name.clone(), trace.clone());
        if let Err(err) = self.inner.trace_store.push(TraceEnvelope::Trace(trace)).await {
            debug!("failed to persist trace {name}: {}", err);
        }
        if let Some(controller) = self.transport_controller() {
            controller.trigger_flush();
        }
        Ok(())
    }

    async fn store_network_request(&self, record: NetworkRequestRecord) -> PerformanceResult<()> {
        if !self.instrumentation_enabled() {
            return Ok(());
        }
        const MAX_HISTORY: usize = 50;
        let mut traces = self.inner.network_requests.lock().await;
        if traces.len() == MAX_HISTORY {
            traces.remove(0);
        }
        traces.push(record.clone());
        if let Err(err) = self.inner.trace_store.push(TraceEnvelope::Network(record)).await {
            debug!("failed to persist network trace: {}", err);
        }
        if let Some(controller) = self.transport_controller() {
            controller.trigger_flush();
        }
        Ok(())
    }

    fn auth_uid(&self) -> Option<String> {
        let ctx = self.inner.auth.lock().ok().and_then(|guard| guard.clone());
        ctx.and_then(|ctx| ctx.current_uid())
    }

    fn transport_controller(&self) -> Option<Arc<TransportController>> {
        self.inner
            .transport
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    async fn app_check_token(&self) -> Option<String> {
        let provider = self
            .inner
            .app_check_override
            .lock()
            .ok()
            .and_then(|guard| guard.clone());

        match provider {
            Some(provider) => provider.get_token().await.ok().flatten().filter(|t| !t.is_empty()),
            None => self.inner.credentials.app_check_token().await.ok().flatten(),
        }
    }
}

impl fmt::Debug for Performance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Performance").field("app", self.app()).finish()
    }
}

impl TraceHandle {
    /// Starts the trace timing measurement.
    pub fn start(&mut self) -> PerformanceResult<()> {
        match self.state {
            TraceLifecycle::Idle => {
                self.state = TraceLifecycle::Running {
                    started_at: Instant::now(),
                    started_micros: now_micros(),
                };
                Ok(())
            }
            TraceLifecycle::Running { .. } => Err(invalid_argument("Trace has already been started")),
            TraceLifecycle::Completed => Err(invalid_argument("Trace has already completed")),
        }
    }

    /// Sets (or replaces) a custom metric value.
    pub fn put_metric(&mut self, name: &str, value: i64) -> PerformanceResult<()> {
        validate_metric_name(name, &self.name)?;
        self.metrics.insert(name.to_string(), value);
        Ok(())
    }

    /// Increments a custom metric by `delta` (defaults to `1` when omitted in JS).
    pub fn increment_metric(&mut self, name: &str, delta: i64) -> PerformanceResult<()> {
        validate_metric_name(name, &self.name)?;
        let entry = self.metrics.entry(name.to_string()).or_insert(0);
        *entry = entry.saturating_add(delta);
        Ok(())
    }

    /// Returns the current value for the provided metric (zero when unset).
    pub fn get_metric(&self, name: &str) -> i64 {
        *self.metrics.get(name).unwrap_or(&0)
    }

    /// Stores (or replaces) a custom attribute on the trace.
    pub fn put_attribute(&mut self, name: &str, value: &str) -> PerformanceResult<()> {
        validate_attribute_name(name)?;
        validate_attribute_value(value)?;
        self.attributes.insert(name.to_string(), value.to_string());
        Ok(())
    }

    /// Removes a stored attribute, if present.
    pub fn remove_attribute(&mut self, name: &str) {
        self.attributes.remove(name);
    }

    /// Reads an attribute value by name.
    pub fn get_attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(|value| value.as_str())
    }

    /// Returns the full attribute map for inspection.
    pub fn attributes(&self) -> &HashMap<String, String> {
        &self.attributes
    }

    /// Records a trace with externally captured timestamps, mirroring the JS
    /// SDK's `trace.record(startTime, duration, options)` API.
    ///
    /// # Examples
    /// ```rust,no_run
    /// # use firebase_performance::{get_performance, PerformanceResult};
    /// # use std::time::Duration;
    /// # async fn demo(app: firebase_core::app::FirebaseApp) -> PerformanceResult<()> {
    /// let perf = get_performance(Some(app)).await?;
    /// let trace = perf.new_trace("cached").unwrap();
    /// let start = std::time::SystemTime::now();
    /// trace.record(start, Duration::from_millis(5), None).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn record(
        &self,
        start_time: SystemTime,
        duration: Duration,
        options: Option<TraceRecordOptions>,
    ) -> PerformanceResult<PerformanceTrace> {
        if duration.is_zero() {
            return Err(invalid_argument("Trace duration must be positive"));
        }
        let mut metrics = HashMap::new();
        let mut attributes = HashMap::new();
        if let Some(opts) = options {
            metrics = opts.metrics;
            attributes = opts.attributes;
        }
        let trace = PerformanceTrace {
            name: self.name.clone(),
            start_time_us: timestamp_micros(start_time),
            duration,
            metrics,
            attributes,
            is_auto: self.is_auto,
            auth_uid: self.performance.auth_uid(),
        };
        self.performance.store_trace(trace.clone()).await?;
        Ok(trace)
    }

    /// Stops the trace, finalising the timing measurement.
    pub async fn stop(mut self) -> PerformanceResult<PerformanceTrace> {
        let (started_at, started_micros) = match self.state {
            TraceLifecycle::Running {
                started_at,
                started_micros,
            } => (started_at, started_micros),
            TraceLifecycle::Idle => return Err(invalid_argument("Trace must be started before stopping")),
            TraceLifecycle::Completed => return Err(invalid_argument("Trace already completed")),
        };
        self.state = TraceLifecycle::Completed;
        let trace = PerformanceTrace {
            name: self.name.clone(),
            start_time_us: started_micros,
            duration: started_at.elapsed(),
            metrics: self.metrics.clone(),
            attributes: self.attributes.clone(),
            is_auto: self.is_auto,
            auth_uid: self.performance.auth_uid(),
        };
        self.performance.store_trace(trace.clone()).await?;
        Ok(trace)
    }
}

impl NetworkTraceHandle {
    /// Marks the beginning of the manual network request measurement.
    pub fn start(&mut self) -> PerformanceResult<()> {
        match self.state {
            NetworkLifecycle::Idle => {
                self.state = NetworkLifecycle::Running {
                    started_at: Instant::now(),
                    started_micros: now_micros(),
                    response_initiated: None,
                };
                Ok(())
            }
            NetworkLifecycle::Running { .. } => Err(invalid_argument("Network trace already started")),
            NetworkLifecycle::Completed => Err(invalid_argument("Network trace already completed")),
        }
    }

    /// Records when the response headers have been received.
    pub fn mark_response_initiated(&mut self) -> PerformanceResult<()> {
        match &mut self.state {
            NetworkLifecycle::Running {
                response_initiated,
                started_at,
                ..
            } => {
                if response_initiated.is_some() {
                    Err(invalid_argument("Response already marked as initiated"))
                } else {
                    *response_initiated = Some(started_at.elapsed());
                    Ok(())
                }
            }
            _ => Err(invalid_argument("Response can only be marked after start")),
        }
    }

    /// Annotates the request payload size.
    pub fn set_request_payload_bytes(&mut self, bytes: u64) {
        self.request_payload_bytes = Some(bytes);
    }

    /// Annotates the response payload size.
    pub fn set_response_payload_bytes(&mut self, bytes: u64) {
        self.response_payload_bytes = Some(bytes);
    }

    /// Stores the final HTTP response code.
    pub fn set_response_code(&mut self, code: u16) -> PerformanceResult<()> {
        if (100..=599).contains(&code) {
            self.response_code = Some(code);
            Ok(())
        } else {
            Err(invalid_argument("HTTP status code must be between 100 and 599"))
        }
    }

    /// Stores the response `Content-Type` header (if known).
    pub fn set_response_content_type(&mut self, content_type: impl Into<String>) {
        self.response_content_type = Some(content_type.into());
    }

    /// Completes the network trace, returning the recorded timings.
    pub async fn stop(mut self) -> PerformanceResult<NetworkRequestRecord> {
        let (started_at, started_micros, response_initiated) = match &self.state {
            NetworkLifecycle::Running {
                started_at,
                started_micros,
                response_initiated,
            } => (*started_at, *started_micros, *response_initiated),
            NetworkLifecycle::Idle => return Err(invalid_argument("Network trace must be started before stopping")),
            NetworkLifecycle::Completed => return Err(invalid_argument("Network trace already completed")),
        };
        self.state = NetworkLifecycle::Completed;
        let duration = started_at.elapsed();
        let response_initiated_us = response_initiated.map(|value| value.as_micros());
        let record = NetworkRequestRecord {
            url: self.url.clone(),
            http_method: self.method.clone(),
            start_time_us: started_micros,
            time_to_request_completed_us: duration.as_micros(),
            time_to_response_initiated_us: response_initiated_us,
            time_to_response_completed_us: Some(duration.as_micros()),
            request_payload_bytes: self.request_payload_bytes,
            response_payload_bytes: self.response_payload_bytes,
            response_code: self.response_code,
            response_content_type: self.response_content_type.clone(),
            app_check_token: self.performance.app_check_token().await,
        };
        self.performance.store_network_request(record.clone()).await?;
        Ok(record)
    }
}

fn validate_trace_name(name: &str) -> PerformanceResult<()> {
    if name.trim().is_empty() {
        Err(invalid_argument("Trace name must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_metric_name(name: &str, trace_name: &str) -> PerformanceResult<()> {
    if name.is_empty() || name.len() > MAX_METRIC_NAME_LENGTH {
        return Err(invalid_argument("Metric name must be 1-100 characters"));
    }
    if name.starts_with(RESERVED_METRIC_PREFIX) && !trace_name.starts_with(OOB_TRACE_PAGE_LOAD_PREFIX) {
        return Err(invalid_argument("Metric names starting with '_' are reserved for auto traces"));
    }
    Ok(())
}

fn validate_attribute_name(name: &str) -> PerformanceResult<()> {
    if name.is_empty() || name.len() > MAX_ATTRIBUTE_NAME_LENGTH {
        return Err(invalid_argument("Attribute name must be 1-40 characters"));
    }
    if !name.chars().next().map(|ch| ch.is_ascii_alphabetic()).unwrap_or(false) {
        return Err(invalid_argument("Attribute names must start with an ASCII letter"));
    }
    if !name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(invalid_argument("Attribute names may only contain letters, numbers, and '_'"));
    }
    if RESERVED_ATTRIBUTE_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Err(invalid_argument("Attribute prefix is reserved"));
    }
    Ok(())
}

fn validate_attribute_value(value: &str) -> PerformanceResult<()> {
    if value.is_empty() || value.len() > MAX_ATTRIBUTE_VALUE_LENGTH {
        Err(invalid_argument("Attribute value must be 1-100 characters"))
    } else {
        Ok(())
    }
}

fn timestamp_micros(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or(0)
}

fn now_micros() -> u128 {
    timestamp_micros(runtime::now())
}

/// Returns `true` when performance monitoring is expected to work on the
/// current platform (matches the checks performed in the JS SDK).
pub fn is_supported() -> bool {
    #[cfg(all(target_arch = "wasm32", feature = "wasm-web"))]
    {
        environment::is_browser()
    }

    #[cfg(not(all(target_arch = "wasm32", feature = "wasm-web")))]
    {
        true
    }
}

impl Service for Performance {
    const NAME: &'static str = PERFORMANCE_COMPONENT_NAME;
}

fn performance_factory(
    container: &ComponentContainer,
    options: InstanceFactoryOptions,
) -> Result<Arc<Performance>, ComponentError> {
    let app = container.app().ok_or_else(|| ComponentError::InitializationFailed {
        name: PERFORMANCE_COMPONENT_NAME.to_string(),
        reason: "Firebase app not attached to component container".to_string(),
    })?;

    let settings = match options.options {
        Value::Null => None,
        value => Some(
            serde_json::from_value(value).map_err(|err| ComponentError::InitializationFailed {
                name: PERFORMANCE_COMPONENT_NAME.to_string(),
                reason: format!("invalid settings: {err}"),
            })?,
        ),
    };

    let performance = Performance::new((*app).clone(), settings);
    Ok(Arc::new(performance))
}

fn ensure_registered() {
    app::register_service::<Performance, _>(performance_factory);
}

/// Registers the performance component in the shared container (normally invoked automatically).
pub fn register_performance_component() {
    ensure_registered();
}

/// Initializes the performance component with explicit settings (akin to the
/// JS SDK's `initializePerformance`). Subsequent calls with a different
/// configuration will fail.
pub async fn initialize_performance(
    app: FirebaseApp,
    settings: Option<PerformanceSettings>,
) -> PerformanceResult<Arc<Performance>> {
    ensure_registered();
    let provider = app::service_provider::<Performance>(&app);
    let options_value = match settings {
        Some(settings) => serde_json::to_value(&settings)
            .map_err(|err| internal_error(format!("failed to serialize settings: {err}")))?,
        None => Value::Null,
    };
    provider.initialize(options_value, None).map_err(|err| match err {
        ComponentError::InstanceAlreadyInitialized { .. } => {
            invalid_argument("Performance has already been initialized for this app")
        }
        other => internal_error(other.to_string()),
    })
}

/// Resolves (or lazily creates) the [`Performance`] instance for the provided
/// app. When `app` is `None`, the default app is used.
pub async fn get_performance(app: Option<FirebaseApp>) -> PerformanceResult<Arc<Performance>> {
    ensure_registered();
    let app = match app {
        Some(app) => app,
        None => firebase_core::app::get_app(None)
            .await
            .map_err(|err| internal_error(err.to_string()))?,
    };

    let provider = app::service_provider::<Performance>(&app);
    if let Some(perf) = provider.get() {
        return Ok(perf);
    }

    match provider.initialize(Value::Null, None) {
        Ok(perf) => Ok(perf),
        Err(ComponentError::InstanceUnavailable { .. }) => provider
            .get()
            .ok_or_else(|| internal_error("Performance component not available")),
        Err(err) => Err(internal_error(err.to_string())),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::TransportOptions;
    use firebase_app_check::{
        box_app_check_future, initialize_app_check, AppCheckOptions, AppCheckProvider, AppCheckProviderFuture,
        AppCheckResult, AppCheckToken,
    };
    use firebase_core::app::initialize_app;
    use firebase_core::app::{FirebaseAppSettings, FirebaseOptions};
    use httpmock::prelude::*;
    use std::sync::Arc;
    use tokio::time::sleep;

    /// `FIREBASE_PERF_DISABLE_TRANSPORT` is process-global: one test turning it off while another
    /// expects it on is what made these tests flaky. Every test that reads or writes it takes this
    /// guard first.
    static TRANSPORT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn transport_guard() -> std::sync::MutexGuard<'static, ()> {
        TRANSPORT_ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn disable_transport() {
        // Callers hold `transport_guard()` already; taking it here would deadlock.
        std::env::set_var("FIREBASE_PERF_DISABLE_TRANSPORT", "1");
    }

    async fn test_app(name: &str) -> FirebaseApp {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        initialize_app(
            options,
            Some(FirebaseAppSettings {
                name: Some(name.to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("create app")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_records_metrics_and_attributes() {
        let _transport_guard = transport_guard();
        disable_transport();
        let app = test_app("perf-trace").await;
        let performance = get_performance(Some(app)).await.unwrap();
        let mut trace = performance.new_trace("load").unwrap();
        trace.put_metric("items", 3).unwrap();
        trace.increment_metric("items", 2).unwrap();
        trace.put_attribute("locale", "en-US").unwrap();
        trace.start().unwrap();
        sleep(Duration::from_millis(5)).await;
        let result = trace.stop().await.unwrap();
        assert_eq!(result.metrics.get("items"), Some(&5));
        assert_eq!(result.attributes.get("locale"), Some(&"en-US".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn record_api_stores_trace_once() {
        let _transport_guard = transport_guard();
        disable_transport();
        let app = test_app("perf-record").await;
        let performance = get_performance(Some(app)).await.unwrap();
        let trace = performance.new_trace("bootstrap").unwrap();
        let start = runtime::now();
        let trace = trace.record(start, Duration::from_millis(10), None).await.unwrap();
        assert_eq!(trace.duration.as_millis(), 10);
        let stored = performance.recorded_trace("bootstrap").await.unwrap();
        assert_eq!(stored.duration.as_millis(), 10);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn network_request_collects_payload_and_status() {
        let _transport_guard = transport_guard();
        disable_transport();
        let app = test_app("perf-network").await;
        let performance = get_performance(Some(app)).await.unwrap();
        let mut request = performance
            .new_network_request("https://example.com", HttpMethod::Post)
            .unwrap();
        request.start().unwrap();
        sleep(Duration::from_millis(5)).await;
        request.mark_response_initiated().unwrap();
        request.set_request_payload_bytes(512);
        request.set_response_payload_bytes(1024);
        request.set_response_code(200).unwrap();
        request.set_response_content_type("application/json");
        let record = request.stop().await.unwrap();
        assert_eq!(record.response_code, Some(200));
        assert_eq!(record.request_payload_bytes, Some(512));
        assert!(record.time_to_request_completed_us >= 5_000);
    }

    #[derive(Clone)]
    struct StaticAppCheckProvider;

    impl AppCheckProvider for StaticAppCheckProvider {
        fn get_token(&self) -> AppCheckProviderFuture<'_, AppCheckResult<AppCheckToken>> {
            box_app_check_future(async move { AppCheckToken::with_ttl("app-check-token", Duration::from_secs(60)) })
        }
    }

    /// Initialising App Check is all the wiring there is: Performance reads the token from the
    /// app's component container.
    async fn attach_app_check(_performance: &Performance, app: &FirebaseApp) {
        let options = AppCheckOptions::new(Arc::new(StaticAppCheckProvider));
        initialize_app_check(Some(app.clone()), options)
            .await
            .expect("initialize app check");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn network_request_includes_app_check_token() {
        let _transport_guard = transport_guard();
        disable_transport();
        let app = test_app("perf-app-check").await;
        let performance = get_performance(Some(app.clone())).await.unwrap();
        attach_app_check(&performance, &app).await;
        let mut request = performance
            .new_network_request("https://example.com", HttpMethod::Get)
            .unwrap();
        request.start().unwrap();
        let record = request.stop().await.unwrap();
        assert_eq!(record.app_check_token.as_deref(), Some("app-check-token"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_performance_respects_settings() {
        let _transport_guard = transport_guard();
        disable_transport();
        let app = test_app("perf-init").await;
        let settings = PerformanceSettings {
            data_collection_enabled: Some(false),
            instrumentation_enabled: Some(false),
        };
        let perf = initialize_performance(app.clone(), Some(settings.clone()))
            .await
            .unwrap();
        assert!(!perf.data_collection_enabled());
        assert!(!perf.instrumentation_enabled());

        let err = initialize_performance(app, Some(settings)).await.unwrap_err();
        assert_eq!(err.code_str(), "performance/invalid-argument");
    }

    #[cfg_attr(target_os = "linux", ignore = "localhost sockets disabled in sandbox")]
    #[tokio::test(flavor = "current_thread")]
    async fn transport_flushes_to_custom_endpoint() {
        let _transport_guard = transport_guard();
        std::env::remove_var("FIREBASE_PERF_DISABLE_TRANSPORT");
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/upload");
            then.status(200);
        });

        let app = test_app("perf-transport").await;
        let performance = get_performance(Some(app)).await.unwrap();
        performance.configure_transport(TransportOptions {
            endpoint: Some(server.url("/upload")),
            api_key: None,
            flush_interval: Some(Duration::from_millis(10)),
            max_batch_size: Some(1),
        });

        let mut trace = performance.new_trace("upload").unwrap();
        trace.start().unwrap();
        trace.stop().await.unwrap();
        performance.flush_transport().await.unwrap();
        // The upload happens on the transport's own task, so wait for it rather than assuming it
        // beats a fixed sleep — that assumption is what made this test flaky.
        for _ in 0..100 {
            if mock.hits() >= 1 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        mock.assert_hits(1);
        disable_transport();
    }
}
