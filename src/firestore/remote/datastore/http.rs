use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;

use async_trait::async_trait;

use crate::firestore::api::snapshot::{DocumentSnapshot, SnapshotMetadata};
use crate::firestore::error::{internal_error, invalid_argument, FirestoreError, FirestoreErrorCode, FirestoreResult};
use crate::firestore::model::{DatabaseId, DocumentKey, FieldPath};
use crate::firestore::remote::connection::{Connection, ConnectionBuilder, RequestContext};
use crate::firestore::remote::serializer::JsonProtoSerializer;
use crate::firestore::remote::structured_query::{encode_aggregation_body, encode_structured_query};
use crate::firestore::value::{FirestoreValue, MapValue};
use crate::firestore::AggregateDefinition;
use crate::firestore::FieldTransform;
use crate::firestore::QueryDefinition;
use serde_json::{json, Value as JsonValue};

use crate::platform::runtime::sleep as runtime_sleep;

use super::{
    CommitResult, ConditionalWrite, Datastore, NoopTokenProvider, TokenProviderArc, WriteOperation, WriteResultInfo,
};

#[derive(Clone)]
pub struct HttpDatastore {
    connection: Connection,
    serializer: JsonProtoSerializer,
    auth_provider: TokenProviderArc,
    app_check_provider: TokenProviderArc,
    retry: RetrySettings,
}

#[derive(Clone)]
pub struct HttpDatastoreBuilder {
    database_id: DatabaseId,
    connection_builder: ConnectionBuilder,
    auth_provider: TokenProviderArc,
    app_check_provider: TokenProviderArc,
    retry: RetrySettings,
}

#[derive(Clone, Debug)]
pub struct RetrySettings {
    pub max_attempts: usize,
    pub initial_delay: Duration,
    pub multiplier: f64,
    pub max_delay: Duration,
    pub request_timeout: Duration,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(100),
            multiplier: 1.5,
            max_delay: Duration::from_secs(5),
            request_timeout: Duration::from_secs(20),
        }
    }
}

impl RetrySettings {
    /// Returns retry settings suited for long-lived streaming connections.
    ///
    /// These defaults mirror the Firestore SDK's exponential backoff for listen/write RPCs:
    /// - Infinite retries (`max_attempts = 0`).
    /// - Initial delay of 1s, backing off up to 60s with a 1.5x multiplier.
    /// - A generous 60s request timeout to accommodate stream keep-alives.
    pub fn streaming_defaults() -> Self {
        Self {
            max_attempts: 0,
            initial_delay: Duration::from_secs(1),
            multiplier: 1.5,
            max_delay: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
        }
    }
}

impl HttpDatastore {
    pub fn builder(database_id: DatabaseId) -> HttpDatastoreBuilder {
        HttpDatastoreBuilder::new(database_id)
    }

    pub fn from_database_id(database_id: DatabaseId) -> FirestoreResult<Self> {
        Self::builder(database_id).build()
    }

    fn new(
        connection: Connection,
        serializer: JsonProtoSerializer,
        auth_provider: TokenProviderArc,
        app_check_provider: TokenProviderArc,
        retry: RetrySettings,
    ) -> Self {
        Self {
            connection,
            serializer,
            auth_provider,
            app_check_provider,
            retry,
        }
    }

    async fn execute_with_retry<F, Fut, T>(&self, operation: F) -> FirestoreResult<T>
    where
        F: FnMut(&RequestContext) -> Fut,
        Fut: Future<Output = FirestoreResult<T>>,
    {
        self.execute_with_retry_policy(operation, true).await
    }

    /// Like [`execute_with_retry`](Self::execute_with_retry) but for requests that must not be
    /// replayed blindly (commits): only an expired credential is retried, after refreshing it.
    async fn execute_non_idempotent<F, Fut, T>(&self, operation: F) -> FirestoreResult<T>
    where
        F: FnMut(&RequestContext) -> Fut,
        Fut: Future<Output = FirestoreResult<T>>,
    {
        self.execute_with_retry_policy(operation, false).await
    }

    async fn execute_with_retry_policy<F, Fut, T>(&self, mut operation: F, idempotent: bool) -> FirestoreResult<T>
    where
        F: FnMut(&RequestContext) -> Fut,
        Fut: Future<Output = FirestoreResult<T>>,
    {
        let mut attempt = 0usize;
        loop {
            let context = self.build_request_context().await?;
            match operation(&context).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    let retryable = if idempotent {
                        self.retry.should_retry(attempt, &err)
                    } else {
                        self.retry.should_retry_non_idempotent(attempt, &err)
                    };
                    if !retryable {
                        return Err(err);
                    }

                    if err.code == FirestoreErrorCode::Unauthenticated {
                        self.auth_provider.invalidate_token();
                        self.app_check_provider.invalidate_token();
                    }

                    let delay = self.retry.backoff_delay(attempt);
                    runtime_sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn build_request_context(&self) -> FirestoreResult<RequestContext> {
        let auth_token = self.auth_provider.get_token().await?;
        let app_check_token = self.app_check_provider.get_token().await?;
        let heartbeat_header = self.app_check_provider.heartbeat_header().await?;
        Ok(RequestContext {
            auth_token,
            app_check_token,
            heartbeat_header,
            request_timeout: Some(self.retry.request_timeout),
        })
    }

    fn encode_commit_body(&self, writes: &[WriteOperation]) -> JsonValue {
        let encoded: Vec<JsonValue> = writes
            .iter()
            .map(|write| self.serializer.encode_write_operation(write))
            .collect();
        json!({ "writes": encoded })
    }

    fn encode_conditional_commit_body(&self, writes: &[ConditionalWrite]) -> JsonValue {
        let encoded: Vec<JsonValue> = writes
            .iter()
            .map(|write| self.serializer.encode_conditional_write(write))
            .collect();
        json!({ "writes": encoded })
    }

    fn decode_commit_response(&self, response: &JsonValue, expected: usize) -> FirestoreResult<CommitResult> {
        // The commit has already been applied at this point; an unparseable timestamp in the
        // metadata must not turn a successful write into an error, so it decodes to `None`.
        let decode_time = |value: Option<&JsonValue>| {
            value
                .and_then(JsonValue::as_str)
                .and_then(|text| self.serializer.decode_timestamp_string(text).ok())
        };
        let commit_time = decode_time(response.get("commitTime"));
        let mut write_results = Vec::with_capacity(expected);
        if let Some(entries) = response.get("writeResults").and_then(JsonValue::as_array) {
            for entry in entries {
                write_results.push(WriteResultInfo {
                    update_time: decode_time(entry.get("updateTime")),
                });
            }
        }
        // The backend returns one result per write; pad defensively so callers can index by
        // write position even if a result is missing.
        while write_results.len() < expected {
            write_results.push(WriteResultInfo::default());
        }
        Ok(CommitResult {
            write_results,
            commit_time,
        })
    }

    async fn commit_body(&self, commit_body: JsonValue, expected: usize) -> FirestoreResult<CommitResult> {
        let response = self
            .execute_non_idempotent(|context| {
                let context = context.clone();
                let body = commit_body.clone();
                async move {
                    self.connection
                        .invoke_json(Method::POST, "documents:commit", Some(body.clone()), &context)
                        .await
                }
            })
            .await?;
        self.decode_commit_response(&response, expected)
    }

    /// Decodes a `batchGet` / `runQuery` / `get` document payload into a snapshot, including the
    /// backend's create and update times when present.
    fn decode_document(&self, document: &JsonValue) -> FirestoreResult<DocumentSnapshot> {
        let name = document
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| internal_error("Firestore document payload missing 'name' field"))?;
        let key = self.serializer.document_key_from_name(name)?;
        self.decode_document_with_key(key, document)
    }

    fn decode_document_with_key(&self, key: DocumentKey, document: &JsonValue) -> FirestoreResult<DocumentSnapshot> {
        let map_value = self
            .serializer
            .decode_document_fields(document)?
            .unwrap_or_else(|| MapValue::new(BTreeMap::new()));
        let time = |field: &str| {
            document
                .get(field)
                .and_then(JsonValue::as_str)
                .and_then(|text| self.serializer.decode_timestamp_string(text).ok())
        };
        Ok(DocumentSnapshot::new(key, Some(map_value), SnapshotMetadata::new(false, false))
            .with_times(time("createTime"), time("updateTime")))
    }

    fn decode_query_response(&self, response: &JsonValue) -> FirestoreResult<Vec<DocumentSnapshot>> {
        let results = response
            .as_array()
            .ok_or_else(|| internal_error("Firestore runQuery response must be an array"))?;

        let mut snapshots = Vec::new();
        for entry in results {
            if let Some(document) = entry.get("document") {
                snapshots.push(self.decode_document(document)?);
            }
        }
        Ok(snapshots)
    }

    fn query_request(&self, query: &QueryDefinition) -> FirestoreResult<(String, JsonValue)> {
        let request_path = if query.parent_path().is_empty() {
            "documents:runQuery".to_string()
        } else {
            format!("documents/{}:runQuery", query.parent_path().canonical_string())
        };
        let structured_query = encode_structured_query(&self.serializer, query)?;
        Ok((request_path, json!({ "structuredQuery": structured_query })))
    }

    async fn run_query_internal(&self, query: &QueryDefinition) -> FirestoreResult<Vec<DocumentSnapshot>> {
        let (request_path, body) = self.query_request(query)?;
        let response = self
            .execute_with_retry(|context| {
                let context = context.clone();
                let request_path = request_path.clone();
                let body = body.clone();
                async move {
                    self.connection
                        .invoke_json(Method::POST, &request_path, Some(body.clone()), &context)
                        .await
                }
            })
            .await?;
        self.decode_query_response(&response)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Datastore for HttpDatastore {
    async fn get_document(&self, key: &DocumentKey) -> FirestoreResult<DocumentSnapshot> {
        let doc_path = format!("documents/{}", key.path().canonical_string());
        let snapshot = self
            .execute_with_retry(|context| {
                let context = context.clone();
                let doc_path = doc_path.clone();
                async move {
                    self.connection
                        .invoke_json_optional(Method::GET, &doc_path, None, &context)
                        .await
                }
            })
            .await?;

        if let Some(json) = snapshot {
            self.decode_document_with_key(key.clone(), &json)
        } else {
            Ok(DocumentSnapshot::new(key.clone(), None, SnapshotMetadata::new(false, false)))
        }
    }

    async fn set_document(
        &self,
        key: &DocumentKey,
        data: MapValue,
        mask: Option<Vec<FieldPath>>,
        transforms: Vec<FieldTransform>,
    ) -> FirestoreResult<()> {
        self.commit(vec![WriteOperation::Set {
            key: key.clone(),
            data,
            mask,
            transforms,
        }])
        .await
    }

    async fn run_query(&self, query: &QueryDefinition) -> FirestoreResult<Vec<DocumentSnapshot>> {
        self.run_query_internal(query).await
    }

    async fn update_document(
        &self,
        key: &DocumentKey,
        data: MapValue,
        field_paths: Vec<FieldPath>,
        transforms: Vec<FieldTransform>,
    ) -> FirestoreResult<()> {
        if field_paths.is_empty() && transforms.is_empty() {
            return Err(invalid_argument("update_document requires at least one field path"));
        }

        self.commit(vec![WriteOperation::Update {
            key: key.clone(),
            data,
            field_paths,
            transforms,
        }])
        .await
    }

    async fn delete_document(&self, key: &DocumentKey) -> FirestoreResult<()> {
        self.commit(vec![WriteOperation::Delete { key: key.clone() }]).await
    }

    async fn commit(&self, writes: Vec<WriteOperation>) -> FirestoreResult<()> {
        self.commit_with_results(writes).await.map(|_| ())
    }

    async fn commit_with_results(&self, writes: Vec<WriteOperation>) -> FirestoreResult<CommitResult> {
        if writes.is_empty() {
            return Ok(CommitResult::default());
        }
        let expected = writes.len();
        let body = self.encode_commit_body(&writes);
        self.commit_body(body, expected).await
    }

    async fn batch_get_documents(&self, keys: &[DocumentKey]) -> FirestoreResult<Vec<DocumentSnapshot>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let names: Vec<String> = keys.iter().map(|key| self.serializer.document_name(key)).collect();
        let body = json!({ "documents": names });
        let response = self
            .execute_with_retry(|context| {
                let context = context.clone();
                let body = body.clone();
                async move {
                    self.connection
                        .invoke_json(Method::POST, "documents:batchGet", Some(body.clone()), &context)
                        .await
                }
            })
            .await?;

        let entries = response
            .as_array()
            .ok_or_else(|| internal_error("Firestore batchGet response must be an array"))?;
        let mut by_name: BTreeMap<String, DocumentSnapshot> = BTreeMap::new();
        for entry in entries {
            // `BatchGetDocumentsResponse` reports hits under `found` (not `document` as runQuery does).
            if let Some(document) = entry.get("found").or_else(|| entry.get("document")) {
                let snapshot = self.decode_document(document)?;
                by_name.insert(self.serializer.document_name(snapshot.key()), snapshot);
            } else if let Some(missing) = entry.get("missing").and_then(JsonValue::as_str) {
                let key = self.serializer.document_key_from_name(missing)?;
                by_name.insert(
                    missing.to_string(),
                    DocumentSnapshot::new(key, None, SnapshotMetadata::new(false, false)),
                );
            }
        }
        // batchGet may answer in any order; return snapshots in request order.
        Ok(names
            .iter()
            .zip(keys)
            .map(|(name, key)| {
                by_name
                    .remove(name)
                    .unwrap_or_else(|| DocumentSnapshot::new(key.clone(), None, SnapshotMetadata::new(false, false)))
            })
            .collect())
    }

    async fn commit_conditional(&self, writes: Vec<ConditionalWrite>) -> FirestoreResult<CommitResult> {
        if writes.is_empty() {
            return Ok(CommitResult::default());
        }
        let expected = writes.len();
        let body = self.encode_conditional_commit_body(&writes);
        self.commit_body(body, expected).await
    }

    async fn run_aggregate(
        &self,
        query: &QueryDefinition,
        aggregations: &[AggregateDefinition],
    ) -> FirestoreResult<BTreeMap<String, FirestoreValue>> {
        if aggregations.is_empty() {
            return Ok(BTreeMap::new());
        }

        let request_path = if query.parent_path().is_empty() {
            "documents:runAggregationQuery".to_string()
        } else {
            format!("documents/{}:runAggregationQuery", query.parent_path().canonical_string())
        };

        let body = encode_aggregation_body(&self.serializer, query, aggregations)?;
        let serializer = self.serializer.clone();

        let response = self
            .execute_with_retry(|context| {
                let context = context.clone();
                let request_path = request_path.clone();
                let body = body.clone();
                async move {
                    self.connection
                        .invoke_json(Method::POST, &request_path, Some(body.clone()), &context)
                        .await
                }
            })
            .await?;

        let entries = response
            .as_array()
            .ok_or_else(|| internal_error("Firestore runAggregationQuery response must be an array"))?;

        let mut aggregates = BTreeMap::new();
        for entry in entries {
            let result = match entry.get("result") {
                Some(result) => result,
                None => continue,
            };
            let fields = result
                .get("aggregateFields")
                .and_then(JsonValue::as_object)
                .ok_or_else(|| internal_error("Firestore runAggregationQuery response missing aggregateFields"))?;
            for (alias, value_json) in fields {
                let decoded = serializer.decode_value_json(value_json)?;
                aggregates.insert(alias.clone(), decoded);
            }
        }

        if aggregates.is_empty() {
            return Err(internal_error(
                "Firestore runAggregationQuery response contained no aggregation results",
            ));
        }

        Ok(aggregates)
    }
}

impl HttpDatastore {}

impl HttpDatastoreBuilder {
    fn new(database_id: DatabaseId) -> Self {
        let auth_provider: TokenProviderArc = Arc::new(NoopTokenProvider);
        let app_check_provider: TokenProviderArc = Arc::new(NoopTokenProvider);
        let connection_builder = Connection::builder(database_id.clone());
        Self {
            database_id,
            connection_builder,
            auth_provider,
            app_check_provider,
            retry: RetrySettings::default(),
        }
    }

    pub fn with_auth_provider(mut self, provider: TokenProviderArc) -> Self {
        self.auth_provider = provider;
        self
    }

    pub fn with_app_check_provider(mut self, provider: TokenProviderArc) -> Self {
        self.app_check_provider = provider;
        self
    }

    pub fn with_retry_settings(mut self, settings: RetrySettings) -> Self {
        self.retry = settings;
        self
    }

    pub fn with_connection_builder(mut self, builder: ConnectionBuilder) -> Self {
        self.connection_builder = builder;
        self
    }

    pub fn build(self) -> FirestoreResult<HttpDatastore> {
        let connection = self.connection_builder.build()?;
        let serializer = JsonProtoSerializer::new(self.database_id.clone());
        Ok(HttpDatastore::new(
            connection,
            serializer,
            self.auth_provider,
            self.app_check_provider,
            self.retry,
        ))
    }
}

impl RetrySettings {
    fn should_retry(&self, attempt: usize, error: &FirestoreError) -> bool {
        if attempt + 1 >= self.max_attempts {
            return false;
        }

        matches!(
            error.code,
            FirestoreErrorCode::Internal
                | FirestoreErrorCode::Unavailable
                | FirestoreErrorCode::DeadlineExceeded
                | FirestoreErrorCode::ResourceExhausted
                | FirestoreErrorCode::Unauthenticated
        )
    }

    /// Commits are not safe to replay: a request that was applied but whose response was lost
    /// would double-apply `increment` and `arrayUnion`. Only an expired credential is retried.
    fn should_retry_non_idempotent(&self, attempt: usize, error: &FirestoreError) -> bool {
        if attempt + 1 >= self.max_attempts {
            return false;
        }
        error.code == FirestoreErrorCode::Unauthenticated
    }

    fn backoff_delay(&self, attempt: usize) -> Duration {
        let factor = self.multiplier.powi(attempt as i32);
        let delay = self.initial_delay.mul_f64(factor);
        if delay > self.max_delay {
            self.max_delay
        } else {
            delay
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::app::{FirebaseApp, FirebaseAppConfig, FirebaseOptions};
    use crate::component::ComponentContainer;
    use crate::firestore::api::database::Firestore;
    use crate::firestore::error::{internal_error, unauthenticated};
    use crate::firestore::model::DatabaseId;
    use crate::firestore::value::ValueKind;
    use crate::firestore::FirestoreValue;
    use crate::firestore::{AggregateField, AggregateSpec};
    use crate::test_support::start_mock_server;
    use httpmock::prelude::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::panic;

    #[test]
    fn retries_unauthenticated_errors() {
        let settings = RetrySettings {
            max_attempts: 3,
            ..Default::default()
        };
        let error = unauthenticated("expired");
        assert!(settings.should_retry(0, &error));
        assert!(settings.should_retry(1, &error));
        assert!(!settings.should_retry(2, &error));
    }

    #[test]
    fn stops_retrying_after_max_attempts() {
        let settings = RetrySettings {
            max_attempts: 1,
            ..Default::default()
        };
        let error = internal_error("boom");
        assert!(!settings.should_retry(0, &error));
    }

    fn mock_datastore(server: &MockServer, database_id: &DatabaseId, retry: RetrySettings) -> HttpDatastore {
        let connection_builder =
            ConnectionBuilder::new(database_id.clone()).with_emulator_host(server.address().to_string());
        HttpDatastore::builder(database_id.clone())
            .with_connection_builder(connection_builder)
            .with_retry_settings(retry)
            .build()
            .expect("datastore")
    }

    fn fast_retry(max_attempts: usize) -> RetrySettings {
        RetrySettings {
            max_attempts,
            initial_delay: Duration::from_millis(1),
            multiplier: 1.0,
            max_delay: Duration::from_millis(1),
            request_timeout: Duration::from_secs(5),
        }
    }

    fn try_server(name: &str) -> Option<MockServer> {
        match panic::catch_unwind(start_mock_server) {
            Ok(server) => Some(server),
            Err(_) => {
                eprintln!("Skipping {name}: unable to bind httpmock server in this environment.");
                None
            }
        }
    }

    #[tokio::test]
    async fn batch_get_and_conditional_commit_use_the_rest_wire_format() {
        let Some(server) = try_server("batch_get_and_conditional_commit_use_the_rest_wire_format") else {
            return;
        };
        let database_id = DatabaseId::new("demo-project", "(default)");
        let base = "/v1/projects/demo-project/databases/(default)";
        let doc_a = "projects/demo-project/databases/(default)/documents/cities/A".to_string();
        let doc_b = "projects/demo-project/databases/(default)/documents/cities/B".to_string();
        let doc_c = "projects/demo-project/databases/(default)/documents/cities/C".to_string();

        // batchGet answers out of order and reports B as missing.
        let batch_get = server.mock(|when, then| {
            when.method(POST)
                .path(format!("{base}/documents:batchGet"))
                .json_body(json!({ "documents": [doc_a, doc_b] }));
            then.status(200).json_body(json!([
                { "missing": doc_b, "readTime": "2026-09-07T00:00:00Z" },
                { "found": {
                    "name": doc_a,
                    "fields": { "name": { "stringValue": "Amsterdam" } },
                    "createTime": "2026-09-01T00:00:00Z",
                    "updateTime": "2026-09-07T12:34:56.789Z"
                } }
            ]));
        });
        let commit = server.mock(|when, then| {
            when.method(POST)
                .path(format!("{base}/documents:commit"))
                .json_body(json!({ "writes": [
                    { "update": { "name": doc_a, "fields": {} },
                      "currentDocument": { "updateTime": "2026-09-07T12:34:56.789000000Z" } },
                    { "delete": doc_c, "currentDocument": { "exists": true } },
                    { "verify": doc_b, "currentDocument": { "exists": false } }
                ] }));
            then.status(200).json_body(json!({
                "writeResults": [ { "updateTime": "2026-09-07T12:35:00Z" }, {}, {} ],
                "commitTime": "2026-09-07T12:35:00Z"
            }));
        });

        let datastore = mock_datastore(&server, &database_id, fast_retry(1));
        let keys = [
            DocumentKey::from_string("cities/A").unwrap(),
            DocumentKey::from_string("cities/B").unwrap(),
        ];
        let snapshots = datastore.batch_get_documents(&keys).await.expect("batchGet");
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].id(), "A");
        assert!(snapshots[0].exists());
        let version = snapshots[0].update_time().expect("updateTime decoded");
        assert!(snapshots[0].create_time().is_some());
        assert_eq!(snapshots[1].id(), "B");
        assert!(!snapshots[1].exists());
        assert!(snapshots[1].update_time().is_none());

        let writes = vec![
            super::super::ConditionalWrite::Write {
                operation: WriteOperation::Set {
                    key: keys[0].clone(),
                    data: MapValue::new(BTreeMap::new()),
                    mask: None,
                    transforms: Vec::new(),
                },
                precondition: super::super::Precondition::UpdateTime(version),
            },
            super::super::ConditionalWrite::Write {
                operation: WriteOperation::Delete {
                    key: DocumentKey::from_string("cities/C").unwrap(),
                },
                precondition: super::super::Precondition::Exists(true),
            },
            super::super::ConditionalWrite::Verify {
                key: keys[1].clone(),
                precondition: super::super::Precondition::Exists(false),
            },
        ];
        let result = datastore.commit_conditional(writes).await.expect("commit");
        assert_eq!(result.write_results.len(), 3);
        assert!(result.write_results[0].update_time.is_some());
        assert!(result.write_results[1].update_time.is_none());
        assert_eq!(result.commit_time, result.write_results[0].update_time);

        batch_get.assert();
        commit.assert();
    }

    #[tokio::test]
    async fn commit_is_not_replayed_after_a_transport_style_failure() {
        let Some(server) = try_server("commit_is_not_replayed_after_a_transport_style_failure") else {
            return;
        };
        let database_id = DatabaseId::new("demo-project", "(default)");
        let base = "/v1/projects/demo-project/databases/(default)";
        let commit = server.mock(|when, then| {
            when.method(POST).path(format!("{base}/documents:commit"));
            then.status(503)
                .json_body(json!({ "error": { "status": "UNAVAILABLE", "message": "try later" } }));
        });
        let query = server.mock(|when, then| {
            when.method(POST).path(format!("{base}/documents:runQuery"));
            then.status(503)
                .json_body(json!({ "error": { "status": "UNAVAILABLE", "message": "try later" } }));
        });

        let datastore = mock_datastore(&server, &database_id, fast_retry(3));
        let key = DocumentKey::from_string("cities/A").unwrap();
        let err = datastore
            .commit(vec![WriteOperation::Delete { key }])
            .await
            .expect_err("commit fails");
        assert_eq!(err.code, FirestoreErrorCode::Unavailable);
        assert_eq!(
            commit.hits(),
            1,
            "a commit must not be replayed: it may already have been applied"
        );

        // Reads are idempotent and keep the retry policy.
        let options = FirebaseOptions {
            project_id: Some(database_id.project_id().to_string()),
            ..Default::default()
        };
        let app = FirebaseApp::new(
            options,
            FirebaseAppConfig::new("commit-retry-test", false),
            ComponentContainer::new("commit-retry-test"),
        );
        let firestore = Firestore::new(app, database_id.clone());
        let firestore_query = firestore.collection("cities").unwrap().query().definition();
        let err = datastore.run_query(&firestore_query).await.expect_err("query fails");
        assert_eq!(err.code, FirestoreErrorCode::Unavailable);
        assert_eq!(query.hits(), 3);
    }

    #[tokio::test]
    async fn run_query_fetches_documents() {
        let server = match panic::catch_unwind(|| start_mock_server()) {
            Ok(server) => server,
            Err(_) => {
                eprintln!("Skipping run_query_fetches_documents: unable to bind httpmock server in this environment.");
                return;
            }
        };
        let database_id = DatabaseId::new("demo-project", "(default)");

        let response_body = json!([
            {
                "document": {
                    "name": format!(
                        "projects/{}/databases/{}/documents/cities/LA",
                        database_id.project_id(),
                        database_id.database()
                    ),
                    "fields": {
                        "name": { "stringValue": "Los Angeles" }
                    }
                }
            },
            {
                "document": {
                    "name": format!(
                        "projects/{}/databases/{}/documents/cities/SF",
                        database_id.project_id(),
                        database_id.database()
                    ),
                    "fields": {
                        "name": { "stringValue": "San Francisco" }
                    }
                }
            }
        ]);

        let expected_body = json!({
            "structuredQuery": {
                "from": [
                    {
                        "collectionId": "cities",
                        "allDescendants": false
                    }
                ],
                "orderBy": [
                    {
                        "field": { "fieldPath": "__name__" },
                        "direction": "ASCENDING"
                    }
                ]
            }
        });

        let expected_path = format!(
            "/v1/projects/{}/databases/{}/documents:runQuery",
            database_id.project_id(),
            database_id.database()
        );

        let run_query_path = expected_path.clone();
        let expected_body_clone = expected_body.clone();
        let response_clone = response_body.clone();

        let _mock = server.mock(move |when, then| {
            when.method(POST)
                .path(run_query_path.as_str())
                .json_body(expected_body_clone.clone());
            then.status(200).json_body(response_clone.clone());
        });

        let client = reqwest::Client::builder().build().expect("reqwest client");

        let connection_builder = Connection::builder(database_id.clone())
            .with_client(client)
            .with_emulator_host(server.address().to_string());

        let datastore = HttpDatastore::builder(database_id.clone())
            .with_connection_builder(connection_builder)
            .build()
            .expect("datastore");

        let options = FirebaseOptions {
            project_id: Some(database_id.project_id().to_string()),
            ..Default::default()
        };
        let app = FirebaseApp::new(
            options,
            FirebaseAppConfig::new("query-test", false),
            ComponentContainer::new("query-test"),
        );

        let firestore = Firestore::new(app, database_id.clone());
        let query = firestore.collection("cities").unwrap().query();
        let definition = query.definition();

        let snapshots = datastore.run_query(&definition).await.expect("query");
        assert_eq!(snapshots.len(), 2);
        let names: Vec<_> = snapshots.iter().map(|snap| snap.id().to_string()).collect();
        assert_eq!(names, vec!["LA", "SF"]);
    }

    #[tokio::test]
    async fn run_query_collection_group_sets_all_descendants() {
        let server = match panic::catch_unwind(|| start_mock_server()) {
            Ok(server) => server,
            Err(_) => {
                eprintln!(
                    "Skipping run_query_collection_group_sets_all_descendants: unable to bind httpmock server in this environment."
                );
                return;
            }
        };
        let database_id = DatabaseId::new("demo-project", "(default)");

        let response_body = json!([
            {
                "document": {
                    "name": format!(
                        "projects/{}/databases/{}/documents/cities/SF/landmarks/golden_gate",
                        database_id.project_id(),
                        database_id.database()
                    ),
                    "fields": {
                        "name": { "stringValue": "Golden Gate" }
                    }
                }
            }
        ]);

        let expected_body = json!({
            "structuredQuery": {
                "from": [
                    {
                        "collectionId": "landmarks",
                        "allDescendants": true
                    }
                ],
                "orderBy": [
                    {
                        "field": { "fieldPath": "__name__" },
                        "direction": "ASCENDING"
                    }
                ]
            }
        });

        let expected_path = format!(
            "/v1/projects/{}/databases/{}/documents:runQuery",
            database_id.project_id(),
            database_id.database()
        );

        let run_query_path = expected_path.clone();
        let expected_body_clone = expected_body.clone();
        let response_clone = response_body.clone();

        let _mock = server.mock(move |when, then| {
            when.method(POST)
                .path(run_query_path.as_str())
                .json_body(expected_body_clone.clone());
            then.status(200).json_body(response_clone.clone());
        });

        let client = reqwest::Client::builder().build().expect("reqwest client");

        let connection_builder = Connection::builder(database_id.clone())
            .with_client(client)
            .with_emulator_host(server.address().to_string());

        let datastore = HttpDatastore::builder(database_id.clone())
            .with_connection_builder(connection_builder)
            .build()
            .expect("datastore");

        let options = FirebaseOptions {
            project_id: Some(database_id.project_id().to_string()),
            ..Default::default()
        };
        let app = FirebaseApp::new(
            options,
            FirebaseAppConfig::new("query-test", false),
            ComponentContainer::new("query-test"),
        );

        let firestore = Firestore::new(app, database_id.clone());
        let query = firestore.collection_group("landmarks").unwrap();
        let definition = query.definition();

        let snapshots = datastore.run_query(&definition).await.expect("collection group query");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id(), "golden_gate");
    }

    #[tokio::test]
    async fn run_aggregate_posts_structured_query() {
        let server = match panic::catch_unwind(|| start_mock_server()) {
            Ok(server) => server,
            Err(_) => {
                eprintln!(
                    "Skipping run_aggregate_posts_structured_query: unable to bind httpmock server in this environment."
                );
                return;
            }
        };
        let database_id = DatabaseId::new("demo-project", "(default)");

        let response_body = json!([
            {
                "result": {
                    "aggregateFields": {
                        "count": { "integerValue": "2" },
                        "total_population": { "integerValue": "150" }
                    }
                }
            }
        ]);

        let expected_body = json!({
            "structuredAggregationQuery": {
                "structuredQuery": {
                    "from": [
                        {
                            "collectionId": "cities",
                            "allDescendants": false
                        }
                    ],
                    "orderBy": [
                        {
                            "field": { "fieldPath": "__name__" },
                            "direction": "ASCENDING"
                        }
                    ]
                },
                "aggregations": [
                    { "alias": "count", "count": {} },
                    {
                        "alias": "total_population",
                        "sum": { "field": { "fieldPath": "population" } }
                    }
                ]
            }
        });

        let expected_path = format!(
            "/v1/projects/{}/databases/{}/documents:runAggregationQuery",
            database_id.project_id(),
            database_id.database()
        );

        let run_query_path = expected_path.clone();
        let expected_body_clone = expected_body.clone();
        let response_clone = response_body.clone();

        let _mock = server.mock(move |when, then| {
            when.method(POST)
                .path(run_query_path.as_str())
                .json_body(expected_body_clone.clone());
            then.status(200).json_body(response_clone.clone());
        });

        let client = reqwest::Client::builder().build().expect("reqwest client");

        let connection_builder = Connection::builder(database_id.clone())
            .with_client(client)
            .with_emulator_host(server.address().to_string());

        let datastore = HttpDatastore::builder(database_id.clone())
            .with_connection_builder(connection_builder)
            .build()
            .expect("datastore");

        let options = FirebaseOptions {
            project_id: Some(database_id.project_id().to_string()),
            ..Default::default()
        };
        let app = FirebaseApp::new(
            options,
            FirebaseAppConfig::new("aggregate-test", false),
            ComponentContainer::new("aggregate-test"),
        );

        let firestore = Firestore::new(app, database_id.clone());
        let query = firestore.collection("cities").unwrap().query();
        let definition = query.definition();

        let mut spec = AggregateSpec::new();
        spec.insert("count", AggregateField::count()).unwrap();
        spec.insert("total_population", AggregateField::sum("population").unwrap())
            .unwrap();
        let aggregates = spec.definitions();

        let results = datastore
            .run_aggregate(&definition, &aggregates)
            .await
            .expect("aggregate");

        let count_value = results.get("count").expect("count present");
        match count_value.kind() {
            ValueKind::Integer(i) => assert_eq!(*i, 2),
            other => panic!("expected integer count, got {other:?}"),
        }

        let total_value = results.get("total_population").expect("sum present");
        match total_value.kind() {
            ValueKind::Integer(i) => assert_eq!(*i, 150),
            other => panic!("expected integer total, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_document_merge_sends_update_mask() {
        let server = match panic::catch_unwind(|| start_mock_server()) {
            Ok(server) => server,
            Err(_) => {
                eprintln!(
                    "Skipping set_document_merge_sends_update_mask: unable to bind httpmock server in this environment."
                );
                return;
            }
        };

        let database_id = DatabaseId::new("demo-project", "(default)");
        let expected_path = format!(
            "/v1/projects/{}/databases/{}/documents:commit",
            database_id.project_id(),
            database_id.database()
        );

        let expected_body = json!({
            "writes": [
                {
                    "update": {
                        "name": format!(
                            "projects/{}/databases/{}/documents/cities/SF",
                            database_id.project_id(),
                            database_id.database()
                        ),
                        "fields": {
                            "stats": {
                                "mapValue": {
                                    "fields": {
                                        "population": { "integerValue": "200" }
                                    }
                                }
                            }
                        }
                    },
                    "updateMask": {
                        "fieldPaths": ["stats.population"]
                    }
                }
            ]
        });

        let run_path = expected_path.clone();
        let expected_body_clone = expected_body.clone();
        let _mock = server.mock(move |when, then| {
            when.method(POST)
                .path(run_path.as_str())
                .json_body(expected_body_clone.clone());
            then.status(200).json_body(json!({ "commitTime": "" }));
        });

        let client = reqwest::Client::builder().build().expect("reqwest client");
        let connection_builder = Connection::builder(database_id.clone())
            .with_client(client)
            .with_emulator_host(server.address().to_string());
        let datastore = HttpDatastore::builder(database_id.clone())
            .with_connection_builder(connection_builder)
            .build()
            .expect("datastore");

        let mut stats = BTreeMap::new();
        stats.insert("population".to_string(), FirestoreValue::from_integer(200));
        let data = MapValue::new(BTreeMap::from([("stats".to_string(), FirestoreValue::from_map(stats))]));

        let key = DocumentKey::from_string("cities/SF").unwrap();
        let mask = vec![FieldPath::from_dot_separated("stats.population").unwrap()];
        datastore
            .set_document(&key, data, Some(mask), Vec::new())
            .await
            .expect("merge commit");
    }
}
