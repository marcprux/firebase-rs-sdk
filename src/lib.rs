#![doc = include_str!("RUSTDOC.md")]

// The foundation every product shares lives in `firebase-core`; it is re-exported here so the
// familiar `firebase_rs_sdk::app` paths keep working. Each product is a crate of its own, pulled in
// by the feature of the same name — depend with `default-features = false` to build only what you
// use.
pub use firebase_core::{app, component, logger, platform, util};

#[cfg(feature = "ai")]
pub use firebase_ai as ai;
#[cfg(feature = "analytics")]
pub use firebase_analytics as analytics;
#[cfg(feature = "app-check")]
pub use firebase_app_check as app_check;
#[cfg(feature = "auth")]
pub use firebase_auth as auth;
#[cfg(feature = "data-connect")]
pub use firebase_data_connect as data_connect;
#[cfg(feature = "database")]
pub use firebase_database as database;
#[cfg(feature = "firestore")]
pub use firebase_firestore as firestore;
#[cfg(feature = "functions")]
pub use firebase_functions as functions;
#[cfg(feature = "installations")]
pub use firebase_installations as installations;
#[cfg(feature = "messaging")]
pub use firebase_messaging as messaging;
#[cfg(feature = "performance")]
pub use firebase_performance as performance;
#[cfg(feature = "remote-config")]
pub use firebase_remote_config as remote_config;
#[cfg(feature = "storage")]
pub use firebase_storage as storage;

#[cfg(test)]
pub use firebase_core::test_support;

mod namespace;
pub use namespace::FirebaseNamespace;

pub mod doctest_support;
