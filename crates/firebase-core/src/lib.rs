//! Shared foundation for the Firebase Rust SDK.
//!
//! Everything every product needs and nothing specific to one of them: the app lifecycle and its
//! component container, the platform primitives (runtime, storage, tokens), logging and small
//! utilities. Products depend on this crate; this crate depends on no product, which is what keeps
//! the dependency graph acyclic — Auth and App Check produce credentials through
//! [`platform::token::TokenProvider`], and the products that talk to a backend consume them.
pub mod app;
pub mod component;
pub mod logger;
pub mod platform;
pub mod util;

#[doc(hidden)]
pub mod doctest_support;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
