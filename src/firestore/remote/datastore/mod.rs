use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::firestore::api::snapshot::DocumentSnapshot;
use crate::firestore::error::FirestoreResult;
use crate::firestore::model::{DocumentKey, FieldPath, Timestamp};
use crate::firestore::value::{FirestoreValue, MapValue};
use crate::firestore::AggregateDefinition;
use crate::firestore::FieldTransform;
use crate::firestore::QueryDefinition;

mod http;
mod in_memory;

// Re-export public API
pub use http::{HttpDatastore, HttpDatastoreBuilder, RetrySettings};
pub use in_memory::InMemoryDatastore;

#[derive(Clone, Debug)]
pub enum WriteOperation {
    Set {
        key: DocumentKey,
        data: MapValue,
        mask: Option<Vec<FieldPath>>,
        transforms: Vec<FieldTransform>,
    },
    Update {
        key: DocumentKey,
        data: MapValue,
        field_paths: Vec<FieldPath>,
        transforms: Vec<FieldTransform>,
    },
    Delete {
        key: DocumentKey,
    },
}

impl WriteOperation {
    /// Returns the document key targeted by this write.
    pub fn key(&self) -> &DocumentKey {
        match self {
            WriteOperation::Set { key, .. } | WriteOperation::Update { key, .. } | WriteOperation::Delete { key } => {
                key
            }
        }
    }
}

/// Outcome of a single write inside a commit, mirroring `google.firestore.v1.WriteResult`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WriteResultInfo {
    /// The time the document was last updated after this write. `None` when the write was a
    /// no-op (for example deleting a missing document) or the backend omitted it.
    pub update_time: Option<Timestamp>,
}

/// Outcome of a `documents:commit` call.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CommitResult {
    /// One entry per write, in request order.
    pub write_results: Vec<WriteResultInfo>,
    /// The time at which the commit occurred, when reported by the backend.
    pub commit_time: Option<Timestamp>,
}

/// Condition a write must satisfy for the whole commit to be applied.
///
/// Mirrors `google.firestore.v1.Precondition`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Precondition {
    /// No condition.
    #[default]
    None,
    /// The document must (`true`) or must not (`false`) exist.
    Exists(bool),
    /// The document's current `updateTime` must equal this value.
    UpdateTime(Timestamp),
}

/// A write with an attached precondition, or a pure verification, as sent by transactions.
#[derive(Clone, Debug)]
pub enum ConditionalWrite {
    /// Apply `operation` if `precondition` holds.
    Write {
        operation: WriteOperation,
        precondition: Precondition,
    },
    /// Apply nothing, but fail the commit unless `precondition` holds for `key`
    /// (`google.firestore.v1.Write.verify`).
    Verify {
        key: DocumentKey,
        precondition: Precondition,
    },
}

impl ConditionalWrite {
    /// The document targeted by this entry.
    pub fn key(&self) -> &DocumentKey {
        match self {
            ConditionalWrite::Write { operation, .. } => operation.key(),
            ConditionalWrite::Verify { key, .. } => key,
        }
    }

    /// The precondition attached to this entry.
    pub fn precondition(&self) -> &Precondition {
        match self {
            ConditionalWrite::Write { precondition, .. } | ConditionalWrite::Verify { precondition, .. } => {
                precondition
            }
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Datastore: Send + Sync + 'static {
    async fn get_document(&self, key: &DocumentKey) -> FirestoreResult<DocumentSnapshot>;
    async fn set_document(
        &self,
        key: &DocumentKey,
        data: MapValue,
        mask: Option<Vec<FieldPath>>,
        transforms: Vec<FieldTransform>,
    ) -> FirestoreResult<()>;
    async fn run_query(&self, query: &QueryDefinition) -> FirestoreResult<Vec<DocumentSnapshot>>;
    async fn update_document(
        &self,
        key: &DocumentKey,
        data: MapValue,
        field_paths: Vec<FieldPath>,
        transforms: Vec<FieldTransform>,
    ) -> FirestoreResult<()>;
    async fn delete_document(&self, key: &DocumentKey) -> FirestoreResult<()>;
    async fn commit(&self, writes: Vec<WriteOperation>) -> FirestoreResult<()>;
    async fn run_aggregate(
        &self,
        query: &QueryDefinition,
        aggregations: &[AggregateDefinition],
    ) -> FirestoreResult<BTreeMap<String, FirestoreValue>>;

    /// Commits `writes` and returns the per-write results. The default delegates to
    /// [`commit`](Self::commit) and reports no update times.
    async fn commit_with_results(&self, writes: Vec<WriteOperation>) -> FirestoreResult<CommitResult> {
        let count = writes.len();
        self.commit(writes).await?;
        Ok(CommitResult {
            write_results: vec![WriteResultInfo::default(); count],
            commit_time: None,
        })
    }

    /// Reads several documents in one round trip (`documents:batchGet`), returning one snapshot
    /// per key in request order with `update_time` populated for existing documents. The default
    /// performs individual reads.
    async fn batch_get_documents(&self, keys: &[DocumentKey]) -> FirestoreResult<Vec<DocumentSnapshot>> {
        let mut snapshots = Vec::with_capacity(keys.len());
        for key in keys {
            snapshots.push(self.get_document(key).await?);
        }
        Ok(snapshots)
    }

    /// Atomically commits writes that carry preconditions, as produced by transactions. The
    /// commit must fail as a whole with `failed-precondition` (or `already-exists` /
    /// `not-found` for existence checks) when any precondition does not hold.
    async fn commit_conditional(&self, writes: Vec<ConditionalWrite>) -> FirestoreResult<CommitResult> {
        let _ = writes;
        Err(crate::firestore::error::internal_error(
            "this datastore does not support conditional writes",
        ))
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait TokenProvider: Send + Sync + 'static {
    async fn get_token(&self) -> FirestoreResult<Option<String>>;
    fn invalidate_token(&self);
    async fn heartbeat_header(&self) -> FirestoreResult<Option<String>> {
        Ok(None)
    }
}

#[derive(Default, Clone)]
pub struct NoopTokenProvider;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl TokenProvider for NoopTokenProvider {
    async fn get_token(&self) -> FirestoreResult<Option<String>> {
        Ok(None)
    }

    fn invalidate_token(&self) {}
}

pub type TokenProviderArc = Arc<dyn TokenProvider>;
