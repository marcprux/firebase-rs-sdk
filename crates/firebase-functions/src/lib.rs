#![doc = include_str!("README.md")]
mod api;
mod constants;
mod context;
pub mod error;
mod transport;

#[cfg(not(target_arch = "wasm32"))]
pub use api::CallableStream;
pub use api::{
    connect_functions_emulator, get_functions, register_functions_component, CallableFunction, Functions,
    HttpsCallableOptions,
};
pub use error::{FunctionsError, FunctionsErrorCode, FunctionsResult};
