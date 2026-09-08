//! Helpers used by documentation examples, re-exported from the product crates.
//!
//! Each crate owns the helpers its own examples need; this module keeps the familiar
//! `firebase_rs_sdk::doctest_support::...` paths working for examples written against the façade.

pub use firebase_core::doctest_support::get_mock_app;

#[cfg(feature = "auth")]
pub use firebase_auth::doctest_support::{auth, get_mock_auth};

#[cfg(feature = "firestore")]
pub mod firestore {
    pub use firebase_firestore::doctest_support::{get_mock_client, get_mock_firestore};
}
