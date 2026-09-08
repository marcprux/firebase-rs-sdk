#![doc = include_str!("README.md")]
mod api;
mod constants;
mod error;
mod model;
mod query_evaluator;
mod remote;
mod value;

pub(crate) use api::aggregate::AggregateOperation;

#[doc(inline)]
pub use api::aggregate::{AggregateDefinition, AggregateField, AggregateQuerySnapshot, AggregateSpec};

#[doc(inline)]
pub use value::{
    from_document, from_firestore_value, to_document, to_firestore_value, ValueDeserializer, ValueSerializer,
};

#[doc(inline)]
pub use api::converter::{FirestoreDataConverter, PassthroughConverter, SerdeConverter};

#[doc(inline)]
pub use api::database::{get_firestore, register_firestore_component, Firestore};

#[doc(inline)]
pub use api::document::FirestoreClient;

#[cfg(not(target_arch = "wasm32"))]
#[doc(inline)]
pub use api::listener::ListenerRegistration;

#[doc(inline)]
pub use api::operations::{
    encode_document_data, encode_set_data, encode_update_document_data, validate_document_path, EncodedSetData,
    EncodedUpdateData, FieldTransform, SetOptions, TransformOperation,
};

pub(crate) use api::operations::{remove_value_at_field_path, set_value_at_field_path, value_for_field_path};

#[doc(inline)]
pub use api::query::{
    and, or, where_filter, CompositeOperator, ConvertedQuery, DocumentChangeType, FieldFilter, Filter, FilterOperator,
    LimitType, OrderDirection, Query, QueryDocumentChange, QuerySnapshot, QuerySnapshotMetadata,
    TypedQueryDocumentChange, TypedQuerySnapshot,
};

#[allow(unused_imports)]
pub(crate) use api::query::{compute_doc_changes, Bound, OrderBy, QueryDefinition};

#[doc(inline)]
pub use api::reference::{
    CollectionReference, ConvertedCollectionReference, ConvertedDocumentReference, DocumentReference,
};

#[doc(inline)]
pub use api::snapshot::{DocumentSnapshot, SnapshotMetadata, TypedDocumentSnapshot};

#[doc(inline)]
pub use api::transaction::{is_retryable_transaction_error, Transaction, TransactionOptions};

#[doc(inline)]
pub use api::write_batch::WriteBatch;

#[doc(inline)]
pub use constants::{DEFAULT_DATABASE_ID, FIRESTORE_COMPONENT_NAME};

#[doc(inline)]
pub use error::{
    aborted, already_exists, deadline_exceeded, failed_precondition, internal_error, invalid_argument,
    missing_project_id, not_found, permission_denied, resource_exhausted, unauthenticated, unavailable, FirestoreError,
    FirestoreErrorCode, FirestoreResult,
};

#[doc(inline)]
pub use model::{DatabaseId, DocumentKey, FieldPath, GeoPoint, IntoFieldPath, ResourcePath, Timestamp};

#[allow(unused_imports)]
pub(crate) use query_evaluator::apply_query_to_documents;

#[doc(inline)]
pub use remote::connection::{Connection, ConnectionBuilder, RequestContext};

#[doc(inline)]
pub use remote::datastore::{CommitResult, ConditionalWrite, Precondition, WriteResultInfo};
#[doc(inline)]
pub use remote::datastore::{
    Datastore, HttpDatastore, HttpDatastoreBuilder, InMemoryDatastore, NoopTokenProvider, RetrySettings, TokenProvider,
    TokenProviderArc, WriteOperation,
};

#[doc(inline)]
pub use remote::remote_event::{RemoteEvent, TargetChange};

#[doc(inline)]
pub use remote::rpc_error::map_http_error;

#[doc(inline)]
pub use remote::serializer::JsonProtoSerializer;

#[allow(unused_imports)]
pub(crate) use remote::structured_query::{encode_aggregation_body, encode_structured_query};

#[doc(inline)]
pub use remote::watch_change::{
    decode_watch_change, DocumentChange, DocumentDelete, DocumentRemove, ExistenceFilterChange, TargetChangeState,
    WatchChange, WatchDocument, WatchTargetChange,
};

#[doc(inline)]
pub use remote::watch_change_aggregator::{TargetMetadataProvider, WatchChangeAggregator};

#[doc(inline)]
pub use value::{ArrayValue, BytesValue, FirestoreValue, MapValue, SentinelValue, ValueKind};

#[doc(hidden)]
pub mod doctest_support;
