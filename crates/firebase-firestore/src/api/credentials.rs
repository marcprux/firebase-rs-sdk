//! Resolves the caller's credentials from the app, the way every other service does.
//!
//! The resolution itself lives in `firebase-core`
//! ([`AppCredentials`](firebase_core::platform::credentials::AppCredentials)); Firestore only
//! picks the two providers out of it, because its datastore attaches the ID token and the App
//! Check token at different layers (REST headers and gRPC metadata).
//!
//! Firestore used to require the providers to be passed in by hand, which meant the documented
//! constructor produced anonymous requests and App Check never protected Firestore at all.

use firebase_core::app::FirebaseApp;
use firebase_core::platform::credentials::AppCredentials;
use firebase_core::platform::token::TokenProviderArc;

/// The app's Auth token provider, resolved when the first request needs it.
pub(crate) fn auth_provider_for_app(app: &FirebaseApp) -> TokenProviderArc {
    AppCredentials::for_app(app).auth().clone()
}

/// The app's App Check token provider, resolved when the first request needs it.
pub(crate) fn app_check_provider_for_app(app: &FirebaseApp) -> TokenProviderArc {
    AppCredentials::for_app(app).app_check().clone()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::remote::connection::ConnectionBuilder;
    use crate::remote::datastore::HttpDatastore;
    use crate::{get_firestore, register_firestore_component, Firestore, FirestoreClient};
    use firebase_app_check::{custom_provider, initialize_app_check, token_with_ttl, AppCheckOptions};
    use firebase_core::app::{initialize_app, FirebaseAppSettings, FirebaseOptions};
    use httpmock::prelude::*;

    fn unique_settings(label: &str) -> FirebaseAppSettings {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("{label}-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    async fn app_for(label: &str) -> firebase_core::app::FirebaseApp {
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
        let _guard = firebase_app_check::test_guard();
        firebase_app_check::clear_state_for_tests();
        firebase_app_check::clear_registry();

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
