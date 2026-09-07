//! Read-write transactions.
//!
//! Mirrors `runTransaction` / `Transaction` in `packages/firestore/src/lite-api/transaction.ts`
//! and `packages/firestore/src/core/transaction.ts`, including the retry policy of
//! `packages/firestore/src/core/transaction_runner.ts`.
//!
//! Like the JS SDK, transactions are optimistic: no server-side transaction is opened (the
//! `beginTransaction` RPC is not available to end-user credentials). Instead, every document
//! read through the [`Transaction`] records its `updateTime`, staged writes carry that time as a
//! `currentDocument` precondition, and documents that were only read are sent as `verify`
//! entries. If any of them changed in the meantime the commit fails with
//! `failed-precondition` and the update closure is run again from scratch.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::firestore::api::converter::FirestoreDataConverter;
use crate::firestore::api::database::Firestore;
use crate::firestore::api::operations::{self, SetOptions};
use crate::firestore::api::reference::{ConvertedDocumentReference, DocumentReference};
use crate::firestore::api::snapshot::{DocumentSnapshot, TypedDocumentSnapshot};
use crate::firestore::error::{
    aborted, failed_precondition, invalid_argument, FirestoreError, FirestoreErrorCode, FirestoreResult,
};
use crate::firestore::model::{DocumentKey, Timestamp};
use crate::firestore::remote::datastore::{ConditionalWrite, Datastore, Precondition, WriteOperation};
use crate::firestore::value::FirestoreValue;
use crate::platform::runtime::sleep as runtime_sleep;

/// Tuning knobs for [`FirestoreClient::run_transaction`](crate::firestore::FirestoreClient::run_transaction).
#[derive(Clone, Debug)]
pub struct TransactionOptions {
    /// Maximum number of attempts before giving up on a contended transaction. The JS SDK
    /// default is 5.
    pub max_attempts: usize,
    /// Delay before the first retry; subsequent retries back off exponentially (factor 1.5)
    /// up to `max_backoff`.
    pub initial_backoff: Duration,
    /// Upper bound for the delay between retries.
    pub max_backoff: Duration,
}

impl Default for TransactionOptions {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        }
    }
}

impl TransactionOptions {
    fn backoff_for(&self, retry_index: usize) -> Duration {
        let delay = self.initial_backoff.mul_f64(1.5f64.powi(retry_index as i32));
        delay.min(self.max_backoff)
    }
}

/// Version of a document as observed by a transaction read. `None` means "did not exist".
type ReadVersion = Option<Timestamp>;

/// A handle used inside a transaction closure to read documents and stage writes.
///
/// All reads must happen before the first write, as in the JS SDK. Writes are buffered and sent
/// in a single commit when the closure returns successfully, guarded by preconditions derived
/// from the reads (see the module documentation).
pub struct Transaction {
    firestore: Firestore,
    datastore: Arc<dyn Datastore>,
    read_versions: Mutex<BTreeMap<String, ReadVersion>>,
    written: Mutex<BTreeSet<String>>,
    writes: Mutex<Vec<ConditionalWrite>>,
    finished: AtomicBool,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("reads", &self.read_versions.lock().unwrap().len())
            .field("pending_writes", &self.pending_writes())
            .field("finished", &self.finished.load(Ordering::SeqCst))
            .finish()
    }
}

impl Transaction {
    pub(crate) fn new(firestore: Firestore, datastore: Arc<dyn Datastore>) -> Self {
        Self {
            firestore,
            datastore,
            read_versions: Mutex::new(BTreeMap::new()),
            written: Mutex::new(BTreeSet::new()),
            writes: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        }
    }

    /// Number of writes staged so far.
    pub fn pending_writes(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    /// Reads a document inside the transaction.
    pub async fn get(&self, reference: &DocumentReference) -> FirestoreResult<DocumentSnapshot> {
        self.ensure_same_firestore(reference.firestore())?;
        let key = DocumentKey::from_path(reference.path().clone())?;
        self.get_key(key).await
    }

    /// Reads the document at `path` (e.g. `cities/sf`) inside the transaction.
    pub async fn get_path(&self, path: &str) -> FirestoreResult<DocumentSnapshot> {
        let key = operations::validate_document_path(path)?;
        self.get_key(key).await
    }

    /// Reads several documents in one round trip, returning snapshots in the same order.
    pub async fn get_all(&self, references: &[DocumentReference]) -> FirestoreResult<Vec<DocumentSnapshot>> {
        self.ensure_reads_allowed()?;
        let mut keys = Vec::with_capacity(references.len());
        for reference in references {
            self.ensure_same_firestore(reference.firestore())?;
            keys.push(DocumentKey::from_path(reference.path().clone())?);
        }
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let snapshots = self.datastore.batch_get_documents(&keys).await?;
        for snapshot in &snapshots {
            self.record_version(snapshot)?;
        }
        Ok(snapshots)
    }

    /// Reads a document through a data converter.
    pub async fn get_with_converter<C>(
        &self,
        reference: &ConvertedDocumentReference<C>,
    ) -> FirestoreResult<TypedDocumentSnapshot<C>>
    where
        C: FirestoreDataConverter,
    {
        let snapshot = self.get(reference.raw()).await?;
        Ok(snapshot.into_typed(reference.converter()))
    }

    /// Reads a document inside the transaction and deserializes it into `T`; `Ok(None)` when it
    /// does not exist.
    pub async fn get_as<T: serde::de::DeserializeOwned>(
        &self,
        reference: &DocumentReference,
    ) -> FirestoreResult<Option<T>> {
        self.get(reference).await?.data_as()
    }

    /// Stages a `set` of a serde-serializable value for `reference`.
    pub fn set_as<T: serde::Serialize + ?Sized>(
        &self,
        reference: &DocumentReference,
        value: &T,
        options: Option<SetOptions>,
    ) -> FirestoreResult<&Self> {
        let data = crate::firestore::value::to_document(value)?;
        self.set(reference, data, options)
    }

    /// Stages an `update` with the fields of a serde-serializable value for `reference`.
    pub fn update_as<T: serde::Serialize + ?Sized>(
        &self,
        reference: &DocumentReference,
        value: &T,
    ) -> FirestoreResult<&Self> {
        let data = crate::firestore::value::to_document(value)?;
        self.update(reference, data)
    }

    /// Stages a `set` (optionally merging) for `reference`.
    pub fn set(
        &self,
        reference: &DocumentReference,
        data: BTreeMap<String, FirestoreValue>,
        options: Option<SetOptions>,
    ) -> FirestoreResult<&Self> {
        self.ensure_same_firestore(reference.firestore())?;
        let key = DocumentKey::from_path(reference.path().clone())?;
        self.set_key(key, data, options)
    }

    /// Stages a `set` for the document at `path`.
    pub fn set_path(
        &self,
        path: &str,
        data: BTreeMap<String, FirestoreValue>,
        options: Option<SetOptions>,
    ) -> FirestoreResult<&Self> {
        let key = operations::validate_document_path(path)?;
        self.set_key(key, data, options)
    }

    /// Stages a `set` through a data converter.
    pub fn set_with_converter<C>(
        &self,
        reference: &ConvertedDocumentReference<C>,
        model: C::Model,
        options: Option<SetOptions>,
    ) -> FirestoreResult<&Self>
    where
        C: FirestoreDataConverter,
    {
        let map = reference.converter().to_map(&model)?;
        self.set(reference.raw(), map, options)
    }

    /// Stages an `update` for `reference`. Fails immediately if the document was read inside
    /// this transaction and did not exist, and at commit time if it does not exist otherwise.
    pub fn update(
        &self,
        reference: &DocumentReference,
        data: BTreeMap<String, FirestoreValue>,
    ) -> FirestoreResult<&Self> {
        self.ensure_same_firestore(reference.firestore())?;
        let key = DocumentKey::from_path(reference.path().clone())?;
        self.update_key(key, data)
    }

    /// Stages an `update` for the document at `path`.
    pub fn update_path(&self, path: &str, data: BTreeMap<String, FirestoreValue>) -> FirestoreResult<&Self> {
        let key = operations::validate_document_path(path)?;
        self.update_key(key, data)
    }

    /// Stages an `update` through a data converter's reference.
    pub fn update_with_converter<C>(
        &self,
        reference: &ConvertedDocumentReference<C>,
        data: BTreeMap<String, FirestoreValue>,
    ) -> FirestoreResult<&Self>
    where
        C: FirestoreDataConverter,
    {
        self.update(reference.raw(), data)
    }

    /// Stages a delete for `reference`.
    pub fn delete(&self, reference: &DocumentReference) -> FirestoreResult<&Self> {
        self.ensure_same_firestore(reference.firestore())?;
        let key = DocumentKey::from_path(reference.path().clone())?;
        self.delete_key(key)
    }

    /// Stages a delete for the document at `path`.
    pub fn delete_path(&self, path: &str) -> FirestoreResult<&Self> {
        let key = operations::validate_document_path(path)?;
        self.delete_key(key)
    }

    /// Stages a delete through a data converter's reference.
    pub fn delete_with_converter<C>(&self, reference: &ConvertedDocumentReference<C>) -> FirestoreResult<&Self>
    where
        C: FirestoreDataConverter,
    {
        self.delete(reference.raw())
    }

    async fn get_key(&self, key: DocumentKey) -> FirestoreResult<DocumentSnapshot> {
        self.ensure_reads_allowed()?;
        let mut snapshots = self.datastore.batch_get_documents(&[key]).await?;
        let snapshot = snapshots
            .pop()
            .ok_or_else(|| failed_precondition("Firestore returned no result for the requested document"))?;
        self.record_version(&snapshot)?;
        Ok(snapshot)
    }

    /// Remembers the version a document had when it was read (JS: `recordVersion`).
    fn record_version(&self, snapshot: &DocumentSnapshot) -> FirestoreResult<()> {
        let canonical = snapshot.key().path().canonical_string();
        let version: ReadVersion = if snapshot.exists() {
            Some(snapshot.update_time().ok_or_else(|| {
                failed_precondition(format!(
                    "Firestore returned document {canonical} without an updateTime; transactions need it"
                ))
            })?)
        } else {
            None
        };
        let mut versions = self.read_versions.lock().unwrap();
        match versions.get(&canonical) {
            Some(existing) if *existing != version => Err(aborted("Document version changed between two reads.")),
            Some(_) => Ok(()),
            None => {
                versions.insert(canonical, version);
                Ok(())
            }
        }
    }

    /// Precondition for a `set` or `delete` (JS: `precondition`).
    fn precondition_for(&self, canonical: &str) -> Precondition {
        if self.written.lock().unwrap().contains(canonical) {
            return Precondition::None;
        }
        match self.read_versions.lock().unwrap().get(canonical) {
            Some(Some(version)) => Precondition::UpdateTime(*version),
            Some(None) => Precondition::Exists(false),
            None => Precondition::None,
        }
    }

    /// Precondition for an `update` (JS: `preconditionForUpdate`).
    fn precondition_for_update(&self, canonical: &str) -> FirestoreResult<Precondition> {
        if self.written.lock().unwrap().contains(canonical) {
            return Ok(Precondition::Exists(true));
        }
        match self.read_versions.lock().unwrap().get(canonical) {
            Some(Some(version)) => Ok(Precondition::UpdateTime(*version)),
            Some(None) => Err(invalid_argument("Can't update a document that doesn't exist.")),
            None => Ok(Precondition::Exists(true)),
        }
    }

    fn stage(&self, operation: WriteOperation, precondition: Precondition) {
        let canonical = operation.key().path().canonical_string();
        self.writes.lock().unwrap().push(ConditionalWrite::Write {
            operation,
            precondition,
        });
        self.written.lock().unwrap().insert(canonical);
    }

    fn set_key(
        &self,
        key: DocumentKey,
        data: BTreeMap<String, FirestoreValue>,
        options: Option<SetOptions>,
    ) -> FirestoreResult<&Self> {
        self.ensure_not_finished()?;
        let encoded = operations::encode_set_data(data, &options.unwrap_or_default())?;
        let precondition = self.precondition_for(&key.path().canonical_string());
        self.stage(
            WriteOperation::Set {
                key,
                data: encoded.map,
                mask: encoded.mask,
                transforms: encoded.transforms,
            },
            precondition,
        );
        Ok(self)
    }

    fn update_key(&self, key: DocumentKey, data: BTreeMap<String, FirestoreValue>) -> FirestoreResult<&Self> {
        self.ensure_not_finished()?;
        let encoded = operations::encode_update_document_data(data)?;
        let precondition = self.precondition_for_update(&key.path().canonical_string())?;
        self.stage(
            WriteOperation::Update {
                key,
                data: encoded.map,
                field_paths: encoded.field_paths,
                transforms: encoded.transforms,
            },
            precondition,
        );
        Ok(self)
    }

    fn delete_key(&self, key: DocumentKey) -> FirestoreResult<&Self> {
        self.ensure_not_finished()?;
        let precondition = self.precondition_for(&key.path().canonical_string());
        self.stage(WriteOperation::Delete { key }, precondition);
        Ok(self)
    }

    fn ensure_reads_allowed(&self) -> FirestoreResult<()> {
        self.ensure_not_finished()?;
        if !self.writes.lock().unwrap().is_empty() {
            return Err(invalid_argument(
                "Firestore transactions require all reads to be executed before all writes.",
            ));
        }
        Ok(())
    }

    fn ensure_not_finished(&self) -> FirestoreResult<()> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(failed_precondition(
                "A transaction object cannot be used after its update function has completed.",
            ));
        }
        Ok(())
    }

    fn ensure_same_firestore(&self, other: &Firestore) -> FirestoreResult<()> {
        if self.firestore.database_id() != other.database_id() {
            return Err(invalid_argument(
                "All transaction operations must target the same Firestore instance",
            ));
        }
        Ok(())
    }

    /// Seals the transaction and returns the writes to commit: the staged writes followed by a
    /// `verify` entry for every document that was read but not written (JS: `commit`).
    fn finish(&self) -> FirestoreResult<Vec<ConditionalWrite>> {
        self.finished.store(true, Ordering::SeqCst);
        let mut writes = std::mem::take(&mut *self.writes.lock().unwrap());
        let written = self.written.lock().unwrap();
        for (canonical, version) in self.read_versions.lock().unwrap().iter() {
            if written.contains(canonical) {
                continue;
            }
            let key = DocumentKey::from_string(canonical)?;
            let precondition = match version {
                Some(version) => Precondition::UpdateTime(*version),
                None => Precondition::Exists(false),
            };
            writes.push(ConditionalWrite::Verify { key, precondition });
        }
        Ok(writes)
    }
}

/// Whether a transaction that failed with `error` should be attempted again.
///
/// Mirrors `isRetryableTransactionError` in the JS SDK: `aborted`, `failed-precondition` and
/// `already-exists` are retried (the read set changed under us), and so is every
/// non-permanent status such as `unavailable` or `deadline-exceeded`.
pub fn is_retryable_transaction_error(error: &FirestoreError) -> bool {
    !matches!(
        error.code,
        FirestoreErrorCode::InvalidArgument
            | FirestoreErrorCode::MissingProjectId
            | FirestoreErrorCode::NotFound
            | FirestoreErrorCode::PermissionDenied
    )
}

/// Runs `update` inside a transaction, retrying on contention. See
/// [`FirestoreClient::run_transaction`](crate::firestore::FirestoreClient::run_transaction).
pub(crate) async fn run_transaction<F, Fut, T>(
    firestore: Firestore,
    datastore: Arc<dyn Datastore>,
    options: TransactionOptions,
    update: F,
) -> FirestoreResult<T>
where
    F: Fn(Arc<Transaction>) -> Fut,
    Fut: Future<Output = FirestoreResult<T>>,
{
    let max_attempts = options.max_attempts.max(1);
    let mut attempt = 0usize;
    loop {
        let transaction = Arc::new(Transaction::new(firestore.clone(), Arc::clone(&datastore)));
        let outcome = update(Arc::clone(&transaction)).await;

        let error = match outcome.and_then(|value| transaction.finish().map(|writes| (value, writes))) {
            Ok((value, writes)) => match datastore.commit_conditional(writes).await {
                Ok(_) => return Ok(value),
                Err(err) => err,
            },
            Err(err) => {
                transaction.finished.store(true, Ordering::SeqCst);
                err
            }
        };

        attempt += 1;
        if attempt >= max_attempts || !is_retryable_transaction_error(&error) {
            return Err(error);
        }
        runtime_sleep(options.backoff_for(attempt - 1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{initialize_app, FirebaseAppSettings, FirebaseOptions};
    use crate::firestore::api::database::get_firestore;
    use crate::firestore::api::document::FirestoreClient;
    use crate::firestore::error::{not_found, unavailable};
    use crate::firestore::model::FieldPath;
    use crate::firestore::remote::datastore::{CommitResult, InMemoryDatastore};
    use crate::firestore::value::{MapValue, ValueKind};
    use crate::firestore::{AggregateDefinition, FieldTransform, QueryDefinition};
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;

    fn unique_settings() -> FirebaseAppSettings {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("firestore-txn-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    /// Delegates to an in-memory datastore, fails the first `commit_failures` conditional commits
    /// with the configured error, and records every conditional commit body.
    struct FlakyDatastore {
        inner: InMemoryDatastore,
        commit_failures: AtomicUsize,
        failure: FirestoreError,
        commits: AtomicUsize,
        last_commit: Mutex<Vec<ConditionalWrite>>,
    }

    impl FlakyDatastore {
        fn new(commit_failures: usize, failure: FirestoreError) -> Arc<Self> {
            Arc::new(Self {
                inner: InMemoryDatastore::new(),
                commit_failures: AtomicUsize::new(commit_failures),
                failure,
                commits: AtomicUsize::new(0),
                last_commit: Mutex::new(Vec::new()),
            })
        }
    }

    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl Datastore for FlakyDatastore {
        async fn get_document(&self, key: &DocumentKey) -> FirestoreResult<DocumentSnapshot> {
            self.inner.get_document(key).await
        }
        async fn set_document(
            &self,
            key: &DocumentKey,
            data: MapValue,
            mask: Option<Vec<FieldPath>>,
            transforms: Vec<FieldTransform>,
        ) -> FirestoreResult<()> {
            self.inner.set_document(key, data, mask, transforms).await
        }
        async fn run_query(&self, query: &QueryDefinition) -> FirestoreResult<Vec<DocumentSnapshot>> {
            self.inner.run_query(query).await
        }
        async fn update_document(
            &self,
            key: &DocumentKey,
            data: MapValue,
            field_paths: Vec<FieldPath>,
            transforms: Vec<FieldTransform>,
        ) -> FirestoreResult<()> {
            self.inner.update_document(key, data, field_paths, transforms).await
        }
        async fn delete_document(&self, key: &DocumentKey) -> FirestoreResult<()> {
            self.inner.delete_document(key).await
        }
        async fn commit(&self, writes: Vec<WriteOperation>) -> FirestoreResult<()> {
            self.inner.commit(writes).await
        }
        async fn run_aggregate(
            &self,
            query: &QueryDefinition,
            aggregations: &[AggregateDefinition],
        ) -> FirestoreResult<BTreeMap<String, FirestoreValue>> {
            self.inner.run_aggregate(query, aggregations).await
        }
        async fn batch_get_documents(&self, keys: &[DocumentKey]) -> FirestoreResult<Vec<DocumentSnapshot>> {
            self.inner.batch_get_documents(keys).await
        }
        async fn commit_conditional(&self, writes: Vec<ConditionalWrite>) -> FirestoreResult<CommitResult> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            *self.last_commit.lock().unwrap() = writes.clone();
            let remaining = self.commit_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                self.commit_failures.store(remaining - 1, Ordering::SeqCst);
                return Err(self.failure.clone());
            }
            self.inner.commit_conditional(writes).await
        }
    }

    async fn client_with(datastore: Arc<dyn Datastore>) -> (FirestoreClient, Firestore) {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let firestore = Firestore::from_arc(get_firestore(Some(app)).await.unwrap());
        (FirestoreClient::new(firestore.clone(), datastore), firestore)
    }

    fn fast_options(max_attempts: usize) -> TransactionOptions {
        TransactionOptions {
            max_attempts,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        }
    }

    fn integer(snapshot: &DocumentSnapshot, field: &str) -> Option<i64> {
        snapshot.data().and_then(|d| d.get(field)).and_then(|v| match v.kind() {
            ValueKind::Integer(i) => Some(*i),
            _ => None,
        })
    }

    fn seed(value: i64) -> BTreeMap<String, FirestoreValue> {
        let mut data = BTreeMap::new();
        data.insert("total".to_string(), FirestoreValue::from_integer(value));
        data
    }

    #[tokio::test]
    async fn transaction_reads_then_writes_and_returns_closure_value() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        let counter = firestore.doc("counters/visits").unwrap();
        client.set_doc("counters/visits", seed(41), None).await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let result = client
            .run_transaction(|txn| {
                let counter = counter.clone();
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let snapshot = txn.get(&counter).await?;
                    assert!(snapshot.update_time().is_some(), "reads must carry a version");
                    let next = integer(&snapshot, "total").unwrap_or(0) + 1;
                    txn.set(&counter, seed(next), None)?;
                    assert_eq!(txn.pending_writes(), 1);
                    Ok(next)
                }
            })
            .await
            .expect("transaction");

        assert_eq!(result, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.commits.load(Ordering::SeqCst), 1);
        let stored = client.get_doc("counters/visits").await.unwrap();
        assert_eq!(integer(&stored, "total"), Some(42));

        // The write carried the version that was read as its precondition.
        let commit = store.last_commit.lock().unwrap();
        assert_eq!(commit.len(), 1);
        assert!(matches!(commit[0].precondition(), Precondition::UpdateTime(_)));
    }

    #[tokio::test]
    async fn read_only_documents_are_verified_at_commit() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        client.set_doc("accounts/a", seed(100), None).await.unwrap();
        client.set_doc("accounts/b", seed(50), None).await.unwrap();
        let a = firestore.doc("accounts/a").unwrap();
        let b = firestore.doc("accounts/b").unwrap();
        let missing = firestore.doc("accounts/missing").unwrap();

        client
            .run_transaction(|txn| {
                let (a, b, missing) = (a.clone(), b.clone(), missing.clone());
                async move {
                    let docs = txn.get_all(&[a.clone(), b.clone(), missing.clone()]).await?;
                    assert_eq!(docs.len(), 3);
                    assert!(!docs[2].exists());
                    // Only `a` is written; `b` and `missing` must be verified.
                    txn.update(&a, seed(integer(&docs[0], "total").unwrap() - 10))?;
                    Ok(())
                }
            })
            .await
            .unwrap();

        let commit = store.last_commit.lock().unwrap();
        assert_eq!(commit.len(), 3);
        assert!(matches!(
            &commit[0],
            ConditionalWrite::Write {
                precondition: Precondition::UpdateTime(_),
                ..
            }
        ));
        let verifies: Vec<(String, Precondition)> = commit[1..]
            .iter()
            .map(|w| (w.key().path().canonical_string(), w.precondition().clone()))
            .collect();
        assert!(matches!(verifies[0], (ref path, Precondition::UpdateTime(_)) if path == "accounts/b"));
        assert_eq!(verifies[1], ("accounts/missing".to_string(), Precondition::Exists(false)));
    }

    #[tokio::test]
    async fn concurrent_modification_of_a_read_document_triggers_retry() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        client.set_doc("counters/hits", seed(0), None).await.unwrap();
        let counter = firestore.doc("counters/hits").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let interfering = client.clone();

        let result = client
            .run_transaction_with_options(fast_options(5), |txn| {
                let counter = counter.clone();
                let calls = Arc::clone(&calls);
                let interfering = interfering.clone();
                async move {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    let current = integer(&txn.get(&counter).await?, "total").unwrap();
                    if attempt == 0 {
                        // Another client bumps the counter between our read and our commit.
                        interfering.set_doc("counters/hits", seed(current + 100), None).await?;
                    }
                    txn.set(&counter, seed(current + 1), None)?;
                    Ok(current + 1)
                }
            })
            .await
            .expect("second attempt commits");

        assert_eq!(result, 101, "the retry observed the interfering write");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.commits.load(Ordering::SeqCst), 2);
        assert_eq!(integer(&client.get_doc("counters/hits").await.unwrap(), "total"), Some(101));
    }

    #[tokio::test]
    async fn set_after_reading_missing_document_requires_absence() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/new").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let interfering = client.clone();

        client
            .run_transaction_with_options(fast_options(3), |txn| {
                let doc = doc.clone();
                let calls = Arc::clone(&calls);
                let interfering = interfering.clone();
                async move {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    let snapshot = txn.get(&doc).await?;
                    if attempt == 0 {
                        assert!(!snapshot.exists());
                        // Someone creates the document before we commit: `already-exists`, retry.
                        interfering.set_doc("items/new", seed(7), None).await?;
                    } else {
                        assert!(snapshot.exists(), "retry must see the concurrently created document");
                    }
                    txn.set(&doc, seed(integer(&snapshot, "total").unwrap_or(0) + 1), None)?;
                    Ok(())
                }
            })
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(integer(&client.get_doc("items/new").await.unwrap(), "total"), Some(8));
    }

    #[tokio::test]
    async fn update_of_document_read_as_missing_fails_immediately() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/ghost").unwrap();

        let err = client
            .run_transaction_with_options(fast_options(3), |txn| {
                let doc = doc.clone();
                async move {
                    let _ = txn.get(&doc).await?;
                    txn.update(&doc, seed(1))?;
                    Ok(())
                }
            })
            .await
            .expect_err("update on a missing document");
        assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);
        assert!(err.to_string().contains("Can't update a document that doesn't exist"));
        assert_eq!(store.commits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn transaction_retries_on_failed_precondition_from_backend() {
        let store = FlakyDatastore::new(2, failed_precondition("the stored version does not match"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/a").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));

        client
            .run_transaction_with_options(fast_options(5), |txn| {
                let doc = doc.clone();
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let _ = txn.get(&doc).await?;
                    txn.set(&doc, seed(1), None)?;
                    Ok(())
                }
            })
            .await
            .expect("third attempt succeeds");

        assert_eq!(calls.load(Ordering::SeqCst), 3, "closure runs once per attempt");
        assert_eq!(store.commits.load(Ordering::SeqCst), 3);
        assert!(client.get_doc("items/a").await.unwrap().exists());
    }

    #[tokio::test]
    async fn transaction_gives_up_after_max_attempts() {
        let store = FlakyDatastore::new(usize::MAX, unavailable("backend down"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/b").unwrap();

        let err = client
            .run_transaction_with_options(fast_options(3), |txn| {
                let doc = doc.clone();
                async move {
                    txn.set(&doc, BTreeMap::new(), None)?;
                    Ok(())
                }
            })
            .await
            .expect_err("must fail");
        assert_eq!(err.code, FirestoreErrorCode::Unavailable);
        assert_eq!(store.commits.load(Ordering::SeqCst), 3);
        assert!(!client.get_doc("items/b").await.unwrap().exists());
    }

    #[tokio::test]
    async fn closure_error_discards_writes_without_retry() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/c").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));

        let err = client
            .run_transaction_with_options(fast_options(5), |txn| {
                let doc = doc.clone();
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    txn.set(&doc, BTreeMap::new(), None)?;
                    Err::<(), _>(not_found("business rule failed"))
                }
            })
            .await
            .expect_err("closure error propagates");

        assert_eq!(err.code, FirestoreErrorCode::NotFound);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "not-found is not retryable");
        assert_eq!(store.commits.load(Ordering::SeqCst), 0);
        assert!(!client.get_doc("items/c").await.unwrap().exists());
    }

    #[tokio::test]
    async fn retryable_closure_error_is_retried() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, _firestore) = client_with(store.clone()).await;
        let calls = Arc::new(AtomicUsize::new(0));

        let result = client
            .run_transaction_with_options(fast_options(4), |_txn| {
                let calls = Arc::clone(&calls);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(aborted("simulated read conflict"))
                    } else {
                        Ok("done")
                    }
                }
            })
            .await
            .expect("succeeds on third attempt");

        assert_eq!(result, "done");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(store.commits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reads_after_writes_are_rejected() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/d").unwrap();

        let err = client
            .run_transaction_with_options(fast_options(1), |txn| {
                let doc = doc.clone();
                async move {
                    txn.set(&doc, BTreeMap::new(), None)?;
                    txn.get(&doc).await.map(|_| ())
                }
            })
            .await
            .expect_err("read after write must fail");
        assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);
        assert!(err.to_string().contains("reads to be executed before all writes"));
    }

    #[tokio::test]
    async fn transaction_handle_is_unusable_after_completion() {
        let store = FlakyDatastore::new(0, aborted("unused"));
        let (client, firestore) = client_with(store.clone()).await;
        let doc = firestore.doc("items/e").unwrap();
        let leaked: Arc<Mutex<Option<Arc<Transaction>>>> = Arc::new(Mutex::new(None));

        client
            .run_transaction_with_options(fast_options(1), |txn| {
                let leaked = Arc::clone(&leaked);
                async move {
                    *leaked.lock().unwrap() = Some(txn);
                    Ok(())
                }
            })
            .await
            .unwrap();

        let txn = leaked.lock().unwrap().take().unwrap();
        let err = txn.set(&doc, BTreeMap::new(), None).expect_err("finished transaction");
        assert_eq!(err.code, FirestoreErrorCode::FailedPrecondition);
    }

    #[test]
    fn retryable_error_classification_matches_js() {
        assert!(is_retryable_transaction_error(&aborted("x")));
        assert!(is_retryable_transaction_error(&failed_precondition("x")));
        assert!(is_retryable_transaction_error(&crate::firestore::error::already_exists("x")));
        assert!(is_retryable_transaction_error(&unavailable("x")));
        assert!(is_retryable_transaction_error(&crate::firestore::error::internal_error("x")));
        assert!(!is_retryable_transaction_error(&invalid_argument("x")));
        assert!(!is_retryable_transaction_error(&not_found("x")));
        assert!(!is_retryable_transaction_error(&crate::firestore::error::permission_denied(
            "x"
        )));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let options = TransactionOptions {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(180),
        };
        assert_eq!(options.backoff_for(0), Duration::from_millis(100));
        assert_eq!(options.backoff_for(1), Duration::from_millis(150));
        assert_eq!(options.backoff_for(2), Duration::from_millis(180));
    }
}
