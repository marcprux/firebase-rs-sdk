#![doc = include_str!("README.md")]
mod api;
mod client;
mod errors;
mod interop;
mod logger;
#[cfg(feature = "wasm-web")]
mod persistence;
mod providers;
mod recaptcha;
mod refresher;
mod state;
//#[cfg(feature = "firestore")]
mod token_provider;
mod types;
mod util;

//pub(super) fn on_token_stored
#[doc(inline)]
pub use api::{
    add_token_listener, custom_provider, debug_token_provider, get_limited_use_token, get_token, initialize_app_check,
    recaptcha_enterprise_provider, recaptcha_v3_provider, remove_token_listener, set_token_auto_refresh_enabled,
    token_with_ttl,
};

// Test helpers other crates use to serialise around App Check's process-global state.
#[cfg(any(test, feature = "test-support"))]
pub use api::{clear_registry, clear_state_for_tests, test_guard};

#[doc(inline)]
pub use client::{
    exchange_token, get_exchange_debug_token_request, get_exchange_recaptcha_enterprise_request,
    get_exchange_recaptcha_v3_request, ExchangeRequest,
};

#[cfg(any(test, feature = "test-support"))]
#[allow(unused_imports)]
pub use client::{clear_exchange_override, set_exchange_override};

// Lets a test point the exchange at a local server instead of the production endpoint.
#[cfg(all(any(test, feature = "test-support"), not(target_arch = "wasm32")))]
pub use client::set_base_endpoint_for_test;

#[doc(inline)]
pub use errors::{AppCheckError, AppCheckResult};

#[doc(inline)]
pub use interop::FirebaseAppCheckInternal;

#[doc(inline)]
pub use providers::{
    CustomProvider, CustomProviderOptions, DebugTokenProvider, ReCaptchaEnterpriseProvider, ReCaptchaV3Provider,
};

//#[cfg(feature = "firestore")]
#[doc(inline)]
pub use token_provider::{app_check_token_provider_arc, AppCheckTokenProvider};

#[allow(unused_imports)]
pub(crate) use types::{AppCheckState, TokenListenerEntry};

// Implementors of `AppCheckProvider` outside this crate need it to box their futures.
#[doc(inline)]
pub use types::box_app_check_future;

#[doc(inline)]
pub use types::{
    AppCheck, AppCheckInternalListener, AppCheckOptions, AppCheckProvider, AppCheckProviderFuture, AppCheckToken,
    AppCheckTokenError, AppCheckTokenErrorListener, AppCheckTokenListener, AppCheckTokenResult, ListenerHandle,
    ListenerType, TokenErrorKind, APP_CHECK_COMPONENT_NAME, APP_CHECK_INTERNAL_COMPONENT_NAME,
};
