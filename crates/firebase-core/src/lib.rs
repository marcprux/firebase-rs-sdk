//! Shared foundation for the Firebase Rust SDK.
//!
//! Everything every product needs and nothing specific to one of them: the app lifecycle and its
//! component container, the platform primitives (runtime, storage, credentials, HTTP), logging and
//! small utilities. Products depend on this crate; this crate depends on no product, which is what
//! keeps the dependency graph acyclic.
//!
//! A product attaches itself to an app through [`component::Service`]: it declares the service's
//! component name and lifetime once, registers a factory with
//! [`app::register_service`](crate::app::register_service), and resolves it by type. The container
//! stores services erased, but the type is recorded with the registration, so a lookup with the
//! wrong type is reported rather than silently answered with `None`.
//!
//! Three more primitives are what a product talks to a Firebase backend with:
//!
//! - [`platform::credentials`] — the user's ID token and the App Check token, resolved from the
//!   app per request. Auth and App Check publish a [`platform::token::TokenProvider`] into the
//!   component container; every consumer reads it from here, so no product depends on a credential
//!   producer.
//! - [`platform::http`] — one HTTP client for native and the browser, with a retry policy.
//! - [`util::status`] — the canonical Google API status codes and the error envelope the backends
//!   answer a rejected request with.
pub mod app;
pub mod component;
pub mod logger;
pub mod platform;
pub mod util;

#[doc(hidden)]
pub mod doctest_support;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
