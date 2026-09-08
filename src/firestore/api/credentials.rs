//! Resolves the caller's credentials from the app, the way every other service does.
//!
//! Storage, Functions and the Realtime Database read `auth-internal` and `app-check-internal` out
//! of the app's component container, so signing in is enough for their requests to carry a token.
//! Firestore used to require the providers to be passed in by hand, which meant the documented
//! constructor produced anonymous requests and App Check never protected Firestore at all.
//!
//! Resolution is lazy: holding the component provider and looking the service up per request means
//! creating a Firestore client neither forces Auth to initialise nor pins the signed-out state of
//! an app that signs in later.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::app::FirebaseApp;
use crate::app_check::{app_check_token_provider_arc, FirebaseAppCheckInternal};
use crate::auth::{auth_token_provider_arc, Auth};
use crate::component::Provider;
use crate::firestore::error::FirestoreResult;
use crate::firestore::remote::datastore::{TokenProvider, TokenProviderArc};

/// Component names shared with the other services (and with the JS SDK).
const AUTH_INTERNAL_COMPONENT_NAME: &str = "auth-internal";
const APP_CHECK_INTERNAL_COMPONENT_NAME: &str = "app-check-internal";

/// A token provider that resolves the underlying service from a component provider on first use.
///
/// Reports `Ok(None)` while the service is absent, which is what an app without Auth (or without
/// App Check) should send: nothing.
struct LazyComponentTokenProvider {
    provider: Provider,
    resolve: fn(&Provider) -> Option<TokenProviderArc>,
    resolved: Mutex<Option<TokenProviderArc>>,
    invalidated: AtomicBool,
}

impl LazyComponentTokenProvider {
    fn new(provider: Provider, resolve: fn(&Provider) -> Option<TokenProviderArc>) -> Self {
        Self {
            provider,
            resolve,
            resolved: Mutex::new(None),
            invalidated: AtomicBool::new(false),
        }
    }

    fn resolved(&self) -> Option<TokenProviderArc> {
        if let Some(existing) = self.resolved.lock().unwrap().clone() {
            return Some(existing);
        }

        let resolved = (self.resolve)(&self.provider)?;
        // An invalidation that arrived before the service existed still has to reach it.
        if self.invalidated.swap(false, Ordering::SeqCst) {
            resolved.invalidate_token();
        }
        *self.resolved.lock().unwrap() = Some(resolved.clone());
        Some(resolved)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TokenProvider for LazyComponentTokenProvider {
    async fn get_token(&self) -> FirestoreResult<Option<String>> {
        match self.resolved() {
            Some(provider) => provider.get_token().await,
            None => Ok(None),
        }
    }

    fn invalidate_token(&self) {
        match self.resolved() {
            Some(provider) => provider.invalidate_token(),
            None => self.invalidated.store(true, Ordering::SeqCst),
        }
    }

    async fn heartbeat_header(&self) -> FirestoreResult<Option<String>> {
        match self.resolved() {
            Some(provider) => provider.heartbeat_header().await,
            None => Ok(None),
        }
    }
}

/// The app's Auth token provider, resolved when the first request needs it.
pub(crate) fn auth_provider_for_app(app: &FirebaseApp) -> TokenProviderArc {
    Arc::new(LazyComponentTokenProvider::new(
        app.container().get_provider(AUTH_INTERNAL_COMPONENT_NAME),
        |provider| {
            provider
                .get_immediate::<Auth>()
                .map(|auth| auth_token_provider_arc(auth))
        },
    ))
}

/// The app's App Check token provider, resolved when the first request needs it.
pub(crate) fn app_check_provider_for_app(app: &FirebaseApp) -> TokenProviderArc {
    Arc::new(LazyComponentTokenProvider::new(
        app.container().get_provider(APP_CHECK_INTERNAL_COMPONENT_NAME),
        |provider| {
            provider
                .get_immediate::<FirebaseAppCheckInternal>()
                .map(|app_check| app_check_token_provider_arc((*app_check).clone()))
        },
    ))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::app::{initialize_app, FirebaseAppSettings, FirebaseOptions};
    use crate::app_check::{custom_provider, initialize_app_check, token_with_ttl, AppCheckOptions};
    use crate::firestore::remote::connection::ConnectionBuilder;
    use crate::firestore::remote::datastore::HttpDatastore;
    use crate::firestore::{get_firestore, register_firestore_component, Firestore, FirestoreClient};
    use httpmock::prelude::*;

    fn unique_settings(label: &str) -> FirebaseAppSettings {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("{label}-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    async fn app_for(label: &str) -> crate::app::FirebaseApp {
        let options = FirebaseOptions {
            api_key: Some("api-key".into()),
            app_id: Some("1:1:web:1".into()),
            project_id: Some("demo-project".into()),
            ..Default::default()
        };
        initialize_app(options, Some(unique_settings(label)))
            .await
            .expect("app")
    }

    /// Builds a client whose REST calls land on `server`.
    fn client_against(server: &MockServer, firestore: &Firestore) -> FirestoreClient {
        let app = firestore.app().clone();
        let connection_builder =
            ConnectionBuilder::new(firestore.database_id().clone()).with_emulator_host(server.address().to_string());
        let datastore = HttpDatastore::builder(firestore.database_id().clone())
            .with_connection_builder(connection_builder)
            .with_auth_provider(auth_provider_for_app(&app))
            .with_app_check_provider(app_check_provider_for_app(&app))
            .build()
            .expect("datastore");
        FirestoreClient::new(firestore.clone(), std::sync::Arc::new(datastore))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn app_check_tokens_travel_with_firestore_requests() {
        let _guard = crate::app_check::test_guard();
        crate::app_check::clear_state_for_tests();
        crate::app_check::clear_registry();

        register_firestore_component();
        let app = app_for("firestore-credentials-app-check").await;
        let firestore = Firestore::from_arc(get_firestore(Some(app.clone())).await.expect("firestore"));

        // App Check is initialised after the client exists, which is the ordinary case: the
        // provider is resolved per request, not captured at construction.
        let server = MockServer::start();
        let client = client_against(&server, &firestore);

        initialize_app_check(
            Some(app.clone()),
            AppCheckOptions::new(custom_provider(|| {
                token_with_ttl("app-check-for-firestore", Duration::from_secs(600))
            })),
        )
        .await
        .expect("initialize app check");

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path_contains("/documents/cities/LA")
                .header("X-Firebase-AppCheck", "app-check-for-firestore");
            then.status(200).json_body(serde_json::json!({
                "name": "projects/demo-project/databases/(default)/documents/cities/LA",
                "fields": {},
            }));
        });

        client.get_doc("cities/LA").await.expect("get_doc");
        mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_is_sent_when_the_app_has_no_credentials() {
        register_firestore_component();
        let app = app_for("firestore-credentials-anonymous").await;
        let firestore = Firestore::from_arc(get_firestore(Some(app)).await.expect("firestore"));

        let server = MockServer::start();
        let client = client_against(&server, &firestore);

        // No user is signed in and App Check was never initialised, so the request carries neither
        // header rather than an empty one.
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path_contains("/documents/cities/LA")
                .matches(|request| {
                    let names: Vec<String> = request
                        .headers
                        .as_ref()
                        .map(|headers| headers.iter().map(|(name, _)| name.to_lowercase()).collect())
                        .unwrap_or_default();
                    !names
                        .iter()
                        .any(|name| name == "authorization" || name == "x-firebase-appcheck")
                });
            then.status(200).json_body(serde_json::json!({
                "name": "projects/demo-project/databases/(default)/documents/cities/LA",
                "fields": {},
            }));
        });

        client.get_doc("cities/LA").await.expect("get_doc");
        mock.assert();
    }
}
