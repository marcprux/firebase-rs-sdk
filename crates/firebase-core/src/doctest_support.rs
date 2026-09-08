//! Helpers that let documentation examples compile in isolation.
//!
//! Hidden from the rendered documentation: examples use these to obtain a throwaway app without
//! turning every snippet into a page of setup.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::app::{initialize_app, FirebaseApp, FirebaseAppSettings, FirebaseOptions};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A uniquely named app with placeholder credentials, for examples that never reach a backend.
pub async fn get_mock_app() -> FirebaseApp {
    let app_name = format!("doc-app-{}", COUNTER.fetch_add(1, Ordering::SeqCst));
    let options = FirebaseOptions {
        api_key: Some("DOCTEST_API_KEY".into()),
        project_id: Some("doctest-project".into()),
        auth_domain: Some("doctest.firebaseapp.com".into()),
        ..Default::default()
    };
    let settings = FirebaseAppSettings {
        name: Some(app_name),
        ..Default::default()
    };

    initialize_app(options, Some(settings))
        .await
        .expect("failed to initialize doctest Firebase app")
}
