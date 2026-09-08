use std::error::Error;
use std::fmt;

/// Error type returned by async token providers when token acquisition fails.
#[derive(Debug, Clone)]
pub struct TokenError {
    message: String,
}

impl TokenError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn from_error(err: impl Error) -> Self {
        Self::new(err.to_string())
    }
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for TokenError {}

/// A source of credentials for outgoing requests, resolved once per request.
///
/// Auth and App Check implement this; every product that talks to a Firebase backend consumes it.
/// It lives here, in the platform layer, so the credential producers do not have to depend on one
/// of their consumers — Firestore used to own this trait, which made `auth` and `app_check` depend
/// on `firestore`.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait TokenProvider: Send + Sync + 'static {
    /// The current token, or `None` when there is nothing to send (no user, no App Check).
    async fn get_token(&self) -> Result<Option<String>, TokenError>;

    /// Marks the cached token as stale, so the next call fetches a fresh one. Consumers call this
    /// after a backend rejects a request as unauthenticated.
    fn invalidate_token(&self);

    /// The `X-Firebase-Client` heartbeat header, when the provider tracks one.
    async fn heartbeat_header(&self) -> Result<Option<String>, TokenError> {
        Ok(None)
    }
}

pub type TokenProviderArc = std::sync::Arc<dyn TokenProvider>;

/// Sends no credentials at all.
#[derive(Default, Clone)]
pub struct NoopTokenProvider;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TokenProvider for NoopTokenProvider {
    async fn get_token(&self) -> Result<Option<String>, TokenError> {
        Ok(None)
    }

    fn invalidate_token(&self) {}
}
