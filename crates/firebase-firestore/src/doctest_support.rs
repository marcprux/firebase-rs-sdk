//! Helpers that let documentation examples compile in isolation.

use std::sync::Arc;

use firebase_core::app::FirebaseApp;
use firebase_core::doctest_support::get_mock_app;

use crate::{get_firestore, register_firestore_component, Firestore, FirestoreClient};

/// Resolves Firestore for a throwaway app.
pub async fn get_mock_firestore(app: Option<FirebaseApp>) -> Arc<Firestore> {
    let app = match app {
        Some(app) => app,
        None => get_mock_app().await,
    };

    register_firestore_component();
    get_firestore(Some(app))
        .await
        .expect("failed to resolve Firestore component")
}

/// A client for a throwaway app, for examples that show a call without performing it.
pub async fn get_mock_client(app: Option<FirebaseApp>) -> FirestoreClient {
    let firestore = get_mock_firestore(app).await;
    FirestoreClient::with_http_datastore(Firestore::from_arc(firestore)).expect("failed to create FirestoreClient")
}
