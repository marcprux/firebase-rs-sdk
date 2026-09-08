//! The compat-style namespace helper, kept in the façade because it reaches across products.

use firebase_core::app::{
    get_app, get_apps, initialize_app, on_log, register_version, set_log_level, AppResult, FirebaseApp,
    FirebaseAppSettings, FirebaseOptions, SDK_VERSION,
};
use firebase_core::logger::{LogCallback, LogLevel, LogOptions};

pub struct FirebaseNamespace;

impl FirebaseNamespace {
    /// Public entry point mirroring the JS `initializeApp` helper.
    pub async fn initialize_app(
        options: FirebaseOptions,
        settings: Option<FirebaseAppSettings>,
    ) -> AppResult<FirebaseApp> {
        initialize_app(options, settings).await
    }

    /// Returns an initialized `FirebaseApp` by name or the default app when `None` is provided.
    pub async fn app(name: Option<&str>) -> AppResult<FirebaseApp> {
        get_app(name).await
    }

    /// Lists all apps that have been initialized in the current process.
    pub async fn apps() -> Vec<FirebaseApp> {
        get_apps().await
    }

    /// Registers an additional library version for platform logging.
    pub async fn register_version(library: &str, version: &str, variant: Option<&str>) {
        register_version(library, version, variant)
    }

    /// Updates the global log verbosity for Firebase.
    pub fn set_log_level(level: LogLevel) {
        set_log_level(level)
    }

    /// Installs or clears a user-provided log callback.
    pub fn on_log(callback: Option<LogCallback>, options: Option<LogOptions>) -> AppResult<()> {
        on_log(callback, options)
    }

    /// Exposes the Firebase SDK version bundled in this crate.
    pub fn sdk_version() -> &'static str {
        SDK_VERSION
    }

    /// Returns the Auth service for the given app, mirroring the JS namespace helper.
    ///
    /// Lives here rather than in `firebase-core` because it reaches into a product: an app crate
    /// that depended on Auth would put a cycle back into the dependency graph.
    #[cfg(feature = "auth")]
    pub async fn auth(app: Option<FirebaseApp>) -> firebase_auth::AuthResult<std::sync::Arc<firebase_auth::Auth>> {
        firebase_auth::register_auth_component();
        let app = match app {
            Some(app) => app,
            None => get_app(None).await.map_err(firebase_auth::AuthError::from)?,
        };
        firebase_auth::auth_for_app(app)
    }
}
