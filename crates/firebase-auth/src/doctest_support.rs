//! Helpers that let documentation examples compile in isolation.

use std::sync::Arc;

use firebase_core::app::FirebaseApp;
use firebase_core::doctest_support::get_mock_app;

use crate::{auth_for_app, register_auth_component, Auth};

/// Resolves Auth for a throwaway app, so examples can start from a signed-out instance.
pub async fn get_mock_auth(app: Option<FirebaseApp>) -> Arc<Auth> {
    let app = match app {
        Some(app) => app,
        None => get_mock_app().await,
    };
    register_auth_component();
    auth_for_app(app).expect("failed to resolve Auth component")
}

pub mod auth {
    use crate::{ApplicationVerifier, AuthResult};

    /// Stands in for reCAPTCHA in phone-auth examples.
    pub struct MockVerifier {
        pub token: &'static str,
        pub kind: &'static str,
    }

    impl ApplicationVerifier for MockVerifier {
        fn verify(&self) -> AuthResult<String> {
            Ok(self.token.to_string())
        }

        fn verifier_type(&self) -> &str {
            self.kind
        }
    }
}
