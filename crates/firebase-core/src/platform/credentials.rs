//! Turning an app into the credentials its outgoing requests must carry.
//!
//! Every product that talks to a Firebase backend needs the same two things: the signed-in user's
//! ID token and the App Check token. Each of them used to resolve those itself, which meant six
//! product crates depended on `firebase-auth` and `firebase-app-check` just to read a string, and
//! each one spelled the header names, the `Bearer`/`Firebase` scheme and the empty-token rules
//! slightly differently. The producers publish a [`TokenSource`] into the app's component
//! container; this module is the only consumer of it, and every product reads credentials from
//! here.
//!
//! Resolution is lazy on purpose: holding the component provider and looking the service up per
//! request means creating a client neither forces Auth to initialise nor pins the signed-out state
//! of an app that signs in later.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::app::FirebaseApp;
use crate::component::{ComponentType, InstantiationMode, Service, ServiceProvider};
use crate::platform::token::{TokenError, TokenProvider, TokenProviderArc};

/// The header carrying the user's ID token.
pub const AUTHORIZATION_HEADER: &str = "Authorization";
/// The header carrying the App Check token.
pub const APP_CHECK_HEADER: &str = "X-Firebase-AppCheck";
/// The header carrying the heartbeat (`X-Firebase-Client`) payload.
pub const CLIENT_HEADER: &str = "X-Firebase-Client";

/// A credential source published into the component container.
///
/// The container cannot hold a trait object, so the credential producers register one of these
/// concrete services instead of their own type. That is what lets a product read a token without
/// linking against the crate that produced it.
pub trait TokenSource: Service {
    /// The provider this service publishes.
    fn provider(&self) -> TokenProviderArc;
}

macro_rules! token_source {
    ($(#[$meta:meta])* $name:ident, $component:literal, $mode:expr) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub struct $name {
            provider: TokenProviderArc,
        }

        impl $name {
            pub fn new(provider: TokenProviderArc) -> Self {
                Self { provider }
            }
        }

        impl Service for $name {
            const NAME: &'static str = $component;
            const INSTANTIATION_MODE: InstantiationMode = $mode;
            const COMPONENT_TYPE: ComponentType = ComponentType::Private;
        }

        impl TokenSource for $name {
            fn provider(&self) -> TokenProviderArc {
                self.provider.clone()
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    };
}

token_source!(
    /// Firebase Auth as a credential source. Created on first use, like Auth itself.
    AuthTokenSource,
    "auth-token",
    InstantiationMode::Lazy
);

token_source!(
    /// App Check as a credential source. Explicit, because App Check exists only once the
    /// application has configured a provider for it.
    AppCheckTokenSource,
    "app-check-token",
    InstantiationMode::Explicit
);

/// How a product spells the user's token in the `Authorization` header.
///
/// The Firebase backends are not consistent about this: the Google APIs (Firestore, Functions,
/// Data Connect) take `Bearer`, while Storage and the AI backends take `Firebase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthHeaderScheme {
    Bearer,
    Firebase,
}

impl AuthHeaderScheme {
    /// The full header value for a token, e.g. `Bearer ya29…`.
    pub fn header_value(&self, token: &str) -> String {
        match self {
            AuthHeaderScheme::Bearer => format!("Bearer {token}"),
            AuthHeaderScheme::Firebase => format!("Firebase {token}"),
        }
    }
}

/// What a caller wants resolved for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredentialRequest {
    /// Ask App Check for a single-use token instead of the shared cached one.
    pub limited_use_app_check_token: bool,
    /// Resolve the heartbeat header alongside the tokens.
    pub include_heartbeat: bool,
}

impl Default for CredentialRequest {
    fn default() -> Self {
        Self {
            limited_use_app_check_token: false,
            include_heartbeat: true,
        }
    }
}

impl CredentialRequest {
    /// A request for a limited-use App Check token, as callable Functions ask for.
    pub fn limited_use() -> Self {
        Self {
            limited_use_app_check_token: true,
            ..Self::default()
        }
    }
}

/// The credentials resolved for one request. Absent and empty tokens are both `None`: a product
/// must never send `Authorization: Bearer ` with nothing after it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CredentialHeaders {
    pub auth_token: Option<String>,
    pub app_check_token: Option<String>,
    pub heartbeat: Option<String>,
}

impl CredentialHeaders {
    /// True when there is nothing to attach.
    pub fn is_empty(&self) -> bool {
        self.auth_token.is_none() && self.app_check_token.is_none() && self.heartbeat.is_none()
    }

    /// The headers as name/value pairs, in the order the JS SDK sends them.
    pub fn pairs(&self, scheme: AuthHeaderScheme) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::with_capacity(3);
        if let Some(token) = &self.auth_token {
            pairs.push((AUTHORIZATION_HEADER, scheme.header_value(token)));
        }
        if let Some(token) = &self.app_check_token {
            pairs.push((APP_CHECK_HEADER, token.clone()));
        }
        if let Some(heartbeat) = &self.heartbeat {
            pairs.push((CLIENT_HEADER, heartbeat.clone()));
        }
        pairs
    }

    /// Writes the headers into a map, replacing whatever was there.
    pub fn apply(&self, scheme: AuthHeaderScheme, headers: &mut HashMap<String, String>) {
        for (name, value) in self.pairs(scheme) {
            headers.insert(name.to_string(), value);
        }
    }
}

/// The app's credentials, resolved per request.
///
/// Build one with [`AppCredentials::for_app`] and keep it for the lifetime of the client: it holds
/// component providers, not services, so it stays correct across sign-in, sign-out and a late
/// `initializeAppCheck`.
#[derive(Clone)]
pub struct AppCredentials {
    auth: TokenProviderArc,
    app_check: TokenProviderArc,
}

impl AppCredentials {
    /// Reads both credential sources out of the app's component container.
    pub fn for_app(app: &FirebaseApp) -> Self {
        let container = app.container();
        Self {
            auth: lazy_token_provider(container.service::<AuthTokenSource>()),
            app_check: lazy_token_provider(container.service::<AppCheckTokenSource>()),
        }
    }

    /// Uses providers the caller already has, for tests and for the emulator paths that supply a
    /// fixed token.
    pub fn from_providers(auth: TokenProviderArc, app_check: TokenProviderArc) -> Self {
        Self { auth, app_check }
    }

    pub fn auth(&self) -> &TokenProviderArc {
        &self.auth
    }

    pub fn app_check(&self) -> &TokenProviderArc {
        &self.app_check
    }

    /// The user's ID token, or `None` when nobody is signed in.
    pub async fn auth_token(&self) -> Result<Option<String>, TokenError> {
        Ok(non_empty(self.auth.get_token().await?))
    }

    /// The App Check token, or `None` when App Check is not installed.
    pub async fn app_check_token(&self) -> Result<Option<String>, TokenError> {
        Ok(non_empty(self.app_check.get_token().await?))
    }

    /// A single-use App Check token; falls back to the shared one for providers that have no
    /// separate limited-use path.
    pub async fn limited_use_app_check_token(&self) -> Result<Option<String>, TokenError> {
        Ok(non_empty(self.app_check.get_limited_use_token().await?))
    }

    /// Tells Auth the token it handed out was rejected, so the next request refreshes it.
    pub fn invalidate_auth_token(&self) {
        self.auth.invalidate_token();
    }

    /// Tells App Check its token was rejected.
    pub fn invalidate_app_check_token(&self) {
        self.app_check.invalidate_token();
    }

    /// Everything one request needs, with the default policy.
    pub async fn headers(&self) -> Result<CredentialHeaders, TokenError> {
        self.headers_for(CredentialRequest::default()).await
    }

    /// Everything one request needs.
    pub async fn headers_for(&self, request: CredentialRequest) -> Result<CredentialHeaders, TokenError> {
        let app_check_token = if request.limited_use_app_check_token {
            self.limited_use_app_check_token().await?
        } else {
            self.app_check_token().await?
        };

        let heartbeat = if request.include_heartbeat {
            non_empty(self.app_check.heartbeat_header().await?)
        } else {
            None
        };

        Ok(CredentialHeaders {
            auth_token: self.auth_token().await?,
            app_check_token,
            heartbeat,
        })
    }

    /// The headers a request should carry, with a failure to mint either token treated as "send
    /// nothing" rather than as a failed request.
    ///
    /// This is what the JS SDK does for App Check everywhere and for Auth in the products that
    /// tolerate an anonymous request; products that must fail loudly call [`Self::headers_for`].
    pub async fn headers_or_empty(&self, request: CredentialRequest) -> CredentialHeaders {
        let app_check_token = if request.limited_use_app_check_token {
            self.limited_use_app_check_token().await.ok().flatten()
        } else {
            self.app_check_token().await.ok().flatten()
        };

        CredentialHeaders {
            auth_token: self.auth_token().await.ok().flatten(),
            app_check_token,
            heartbeat: if request.include_heartbeat {
                non_empty(self.app_check.heartbeat_header().await.ok().flatten())
            } else {
                None
            },
        }
    }
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials").finish_non_exhaustive()
    }
}

fn non_empty(token: Option<String>) -> Option<String> {
    token.filter(|value| !value.is_empty())
}

/// A token provider that resolves the underlying [`TokenSource`] from the container on first use,
/// and reports "no credentials" while the service is absent.
struct LazyComponentTokenProvider<S: TokenSource> {
    provider: ServiceProvider<S>,
    resolved: Mutex<Option<TokenProviderArc>>,
    invalidated: AtomicBool,
    marker: PhantomData<fn() -> S>,
}

impl<S: TokenSource> LazyComponentTokenProvider<S> {
    fn resolved(&self) -> Option<TokenProviderArc> {
        if let Some(existing) = self.resolved.lock().unwrap().clone() {
            return Some(existing);
        }

        let resolved = self.provider.get()?.provider();
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
impl<S: TokenSource> TokenProvider for LazyComponentTokenProvider<S> {
    async fn get_token(&self) -> Result<Option<String>, TokenError> {
        match self.resolved() {
            Some(provider) => provider.get_token().await,
            None => Ok(None),
        }
    }

    async fn get_limited_use_token(&self) -> Result<Option<String>, TokenError> {
        match self.resolved() {
            Some(provider) => provider.get_limited_use_token().await,
            None => Ok(None),
        }
    }

    fn invalidate_token(&self) {
        match self.resolved() {
            Some(provider) => provider.invalidate_token(),
            None => self.invalidated.store(true, Ordering::SeqCst),
        }
    }

    async fn heartbeat_header(&self) -> Result<Option<String>, TokenError> {
        match self.resolved() {
            Some(provider) => provider.heartbeat_header().await,
            None => Ok(None),
        }
    }
}

/// Wraps a service provider as a [`TokenProvider`] that resolves its [`TokenSource`] on demand.
pub fn lazy_token_provider<S: TokenSource>(provider: ServiceProvider<S>) -> TokenProviderArc {
    Arc::new(LazyComponentTokenProvider {
        provider,
        resolved: Mutex::new(None),
        invalidated: AtomicBool::new(false),
        marker: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{Component, ComponentContainer};
    use crate::test_support::test_firebase_app_with_api_key;

    /// A stand-in for Auth or App Check: hands out a fixed token and records invalidation.
    struct FakeProvider {
        token: Option<String>,
        limited_use_token: Option<String>,
        heartbeat: Option<String>,
        fails: bool,
        invalidations: Arc<Mutex<usize>>,
    }

    impl FakeProvider {
        fn with_token(token: &str) -> Self {
            Self {
                token: Some(token.to_string()),
                limited_use_token: None,
                heartbeat: None,
                fails: false,
                invalidations: Arc::new(Mutex::new(0)),
            }
        }

        fn failing() -> Self {
            Self {
                token: None,
                limited_use_token: None,
                heartbeat: None,
                fails: true,
                invalidations: Arc::new(Mutex::new(0)),
            }
        }
    }

    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl TokenProvider for FakeProvider {
        async fn get_token(&self) -> Result<Option<String>, TokenError> {
            if self.fails {
                return Err(TokenError::new("no credentials"));
            }
            Ok(self.token.clone())
        }

        async fn get_limited_use_token(&self) -> Result<Option<String>, TokenError> {
            match &self.limited_use_token {
                Some(token) => Ok(Some(token.clone())),
                None => self.get_token().await,
            }
        }

        fn invalidate_token(&self) {
            *self.invalidations.lock().unwrap() += 1;
        }

        async fn heartbeat_header(&self) -> Result<Option<String>, TokenError> {
            Ok(self.heartbeat.clone())
        }
    }

    /// Publishes a credential source the way Auth and App Check do.
    fn publish<S: TokenSource + Clone>(container: &ComponentContainer, source: S) {
        container
            .add_component(Component::for_service::<S, _>(move |_, _| Ok(Arc::new(source.clone()))))
            .expect("component");
        // App Check's source is explicit, so it has to be asked for by name once, as
        // `initialize_app_check` does.
        if S::INSTANTIATION_MODE == InstantiationMode::Explicit {
            container
                .service::<S>()
                .initialize(serde_json::Value::Null, None)
                .expect("initialize");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_app_without_credentials_sends_nothing() {
        let app = test_firebase_app_with_api_key("key");
        let credentials = AppCredentials::for_app(&app);

        let headers = credentials.headers().await.expect("headers");

        assert!(headers.is_empty());
        assert!(headers.pairs(AuthHeaderScheme::Bearer).is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolves_both_tokens_from_the_container() {
        let app = test_firebase_app_with_api_key("key");
        publish(
            &app.container(),
            AuthTokenSource::new(Arc::new(FakeProvider::with_token("id-token"))),
        );
        let mut app_check = FakeProvider::with_token("app-check-token");
        app_check.heartbeat = Some("heartbeat".into());
        publish(&app.container(), AppCheckTokenSource::new(Arc::new(app_check)));

        let credentials = AppCredentials::for_app(&app);
        let headers = credentials.headers().await.expect("headers");

        assert_eq!(
            headers.pairs(AuthHeaderScheme::Bearer),
            vec![
                (AUTHORIZATION_HEADER, "Bearer id-token".to_string()),
                (APP_CHECK_HEADER, "app-check-token".to_string()),
                (CLIENT_HEADER, "heartbeat".to_string()),
            ]
        );
        assert_eq!(headers.pairs(AuthHeaderScheme::Firebase)[0].1, "Firebase id-token".to_string());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_empty_token_is_no_token() {
        let app = test_firebase_app_with_api_key("key");
        publish(&app.container(), AuthTokenSource::new(Arc::new(FakeProvider::with_token(""))));

        let credentials = AppCredentials::for_app(&app);

        assert_eq!(credentials.auth_token().await.expect("token"), None);
        assert!(credentials.headers().await.expect("headers").is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn asks_app_check_for_a_limited_use_token() {
        let app = test_firebase_app_with_api_key("key");
        let mut app_check = FakeProvider::with_token("shared");
        app_check.limited_use_token = Some("single-use".into());
        publish(&app.container(), AppCheckTokenSource::new(Arc::new(app_check)));

        let credentials = AppCredentials::for_app(&app);

        assert_eq!(
            credentials.headers_for(CredentialRequest::limited_use()).await.unwrap(),
            CredentialHeaders {
                auth_token: None,
                app_check_token: Some("single-use".into()),
                heartbeat: None,
            }
        );
        assert_eq!(credentials.headers().await.unwrap().app_check_token, Some("shared".into()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_invalidation_reaches_a_service_that_did_not_exist_yet() {
        let app = test_firebase_app_with_api_key("key");
        let provider = FakeProvider::with_token("id-token");
        let invalidations = provider.invalidations.clone();

        let credentials = AppCredentials::for_app(&app);
        // Nothing is registered yet: the request has to be remembered.
        credentials.invalidate_auth_token();
        assert_eq!(*invalidations.lock().unwrap(), 0);

        publish(&app.container(), AuthTokenSource::new(Arc::new(provider)));
        assert_eq!(credentials.auth_token().await.unwrap(), Some("id-token".into()));
        assert_eq!(*invalidations.lock().unwrap(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_credential_failure_is_fatal_only_when_the_caller_asks() {
        let app = test_firebase_app_with_api_key("key");
        publish(&app.container(), AuthTokenSource::new(Arc::new(FakeProvider::failing())));

        let credentials = AppCredentials::for_app(&app);

        assert!(credentials.headers().await.is_err());
        assert!(credentials
            .headers_or_empty(CredentialRequest::default())
            .await
            .is_empty());
    }
}
