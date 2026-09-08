use std::sync::atomic::{AtomicBool, Ordering};

use crate::errors::AppCheckError;
use crate::FirebaseAppCheckInternal;
use firebase_core::platform::token::{TokenError, TokenProvider, TokenProviderArc};

/// Exposes App Check as a [`TokenProvider`], so every product can attach the token the same way.
pub struct AppCheckTokenProvider {
    app_check: FirebaseAppCheckInternal,
    force_refresh: AtomicBool,
}

impl AppCheckTokenProvider {
    /// Creates a new provider backed by the given App Check instance.
    pub fn new(app_check: FirebaseAppCheckInternal) -> Self {
        Self {
            app_check,
            force_refresh: AtomicBool::new(false),
        }
    }

    /// Converts the provider into a reference-counted [`TokenProviderArc`].
    pub fn into_arc(self) -> TokenProviderArc {
        std::sync::Arc::new(self)
    }
}

/// Convenience helper to expose an App Check instance as a [`TokenProviderArc`].
pub fn app_check_token_provider_arc(app_check: FirebaseAppCheckInternal) -> TokenProviderArc {
    AppCheckTokenProvider::new(app_check).into_arc()
}

impl Clone for AppCheckTokenProvider {
    fn clone(&self) -> Self {
        Self {
            app_check: self.app_check.clone(),
            force_refresh: AtomicBool::new(self.force_refresh.load(Ordering::SeqCst)),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TokenProvider for AppCheckTokenProvider {
    async fn get_token(&self) -> Result<Option<String>, TokenError> {
        let force_refresh = self.force_refresh.swap(false, Ordering::SeqCst);
        match self.app_check.get_token(force_refresh).await {
            Ok(result) => {
                if result.token.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(result.token))
                }
            }
            Err(err) => {
                if let Some(cached) = err.cached_token() {
                    if cached.token.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(cached.token.clone()))
                    }
                } else {
                    Err(map_app_check_error(err.cause))
                }
            }
        }
    }

    fn invalidate_token(&self) {
        self.force_refresh.store(true, Ordering::SeqCst);
    }

    async fn heartbeat_header(&self) -> Result<Option<String>, TokenError> {
        self.app_check.heartbeat_header().await.map_err(map_app_check_error)
    }
}

fn map_app_check_error(error: AppCheckError) -> TokenError {
    // The consumer decides what a credential failure means for its own error type; here it is just
    // a message, so no product's error taxonomy leaks into App Check.
    TokenError::new(error.to_string())
}
