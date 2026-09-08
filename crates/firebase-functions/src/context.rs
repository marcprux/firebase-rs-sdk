use std::fmt::{Debug, Formatter};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::sync::Mutex;

#[cfg(not(all(feature = "wasm-web", target_arch = "wasm32")))]
use std::time::{SystemTime, UNIX_EPOCH};

use firebase_core::app::FirebaseApp;
#[cfg(not(target_arch = "wasm32"))]
use firebase_core::component::ServiceProvider;
use firebase_core::platform::credentials::{AppCredentials, CredentialRequest};
#[cfg(not(target_arch = "wasm32"))]
use firebase_messaging::Messaging;

/// Metadata that may be attached to callable Function requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallContext {
    pub auth_token: Option<String>,
    pub messaging_token: Option<String>,
    pub app_check_token: Option<String>,
    pub app_check_heartbeat: Option<String>,
}

pub struct ContextProvider {
    credentials: AppCredentials,
    #[cfg(not(target_arch = "wasm32"))]
    messaging_provider: ServiceProvider<Messaging>,
    #[cfg(not(target_arch = "wasm32"))]
    cached_messaging: Mutex<Option<Arc<Messaging>>>,
    overrides: Mutex<Option<CallContext>>,
}

impl Debug for ContextProvider {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        #[allow(unused_variables)]
        let messaging_cached = {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.cached_messaging.lock().unwrap().is_some()
            }
            #[cfg(target_arch = "wasm32")]
            {
                false
            }
        };
        f.debug_struct("ContextProvider")
            .field("messaging_cached", &messaging_cached)
            .finish()
    }
}

impl ContextProvider {
    pub fn new(app: FirebaseApp) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let container = app.container();

        Self {
            credentials: AppCredentials::for_app(&app),
            overrides: Mutex::new(None),
            #[cfg(not(target_arch = "wasm32"))]
            messaging_provider: container.service::<Messaging>(),
            #[cfg(not(target_arch = "wasm32"))]
            cached_messaging: Mutex::new(None),
        }
    }

    pub async fn get_context_async(&self, limited_use_app_check_tokens: bool) -> CallContext {
        if let Some(overrides) = self.overrides.lock().unwrap().clone() {
            return overrides;
        }

        // A credential that cannot be minted is left out of the call rather than failing it,
        // which is what the JS SDK does: the backend decides whether an anonymous call is allowed.
        let credentials = self
            .credentials
            .headers_or_empty(CredentialRequest {
                limited_use_app_check_token: limited_use_app_check_tokens,
                include_heartbeat: true,
            })
            .await;

        CallContext {
            auth_token: credentials.auth_token,
            messaging_token: self.fetch_messaging_token().await,
            app_check_token: credentials.app_check_token,
            app_check_heartbeat: credentials.heartbeat,
        }
    }

    async fn fetch_messaging_token(&self) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = self;
            None
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            use firebase_messaging::read_token;

            const MESSAGING_TOKEN_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

            let messaging = self.ensure_messaging()?;
            let store_key = messaging.app().name().to_string();
            if let Ok(Some(record)) = read_token(&store_key) {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or(0);
                if !record.is_expired(now_ms, MESSAGING_TOKEN_TTL_MS) {
                    return Some(record.token);
                }
            }

            match messaging.get_token(None).await {
                Ok(token) if !token.is_empty() => Some(token),
                _ => None,
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn ensure_messaging(&self) -> Option<Arc<Messaging>> {
        if let Some(cached) = self.cached_messaging.lock().unwrap().clone() {
            return Some(cached);
        }

        if let Some(messaging) = self.messaging_provider.get() {
            *self.cached_messaging.lock().unwrap() = Some(messaging.clone());
            Some(messaging)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub fn set_overrides(&self, overrides: CallContext) {
        *self.overrides.lock().unwrap() = Some(overrides);
    }
}
