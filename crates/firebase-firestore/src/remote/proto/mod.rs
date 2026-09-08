//! Generated protobuf/gRPC bindings for `google.firestore.v1`.
//!
//! The `.proto` sources are vendored under `proto/` (from
//! <https://github.com/googleapis/googleapis>, Apache-2.0) and the Rust code here is generated
//! from them, so building this crate needs no `protoc`. Regenerate with
//! `scripts/generate_firestore_protos.sh` after updating the vendored protos.
//!
//! Only the `Listen` streaming RPC is used today (see
//! [`crate::remote::listen`]); one-shot reads and writes go over REST.
#![allow(clippy::all)]
#![allow(rustdoc::all)]
#![allow(dead_code)]

pub mod google {
    pub mod firestore {
        pub mod v1 {
            include!("firestore_v1.rs");
        }
    }

    pub mod rpc {
        include!("rpc.rs");
    }

    pub mod r#type {
        include!("type_latlng.rs");
    }
}

#[allow(unused_imports)]
pub(crate) use google::firestore::v1;
