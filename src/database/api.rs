use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use serde_json::{Map, Value};

use crate::app;
use crate::app::FirebaseApp;
use crate::component::types::{ComponentError, DynService, InstanceFactoryOptions, InstantiationMode};
use crate::component::{Component, ComponentType};
use crate::database::backend::{select_backend, CasOutcome, DatabaseBackend};
use crate::database::constants::DATABASE_COMPONENT_NAME;
use crate::database::error::{internal_error, invalid_argument, permission_denied, DatabaseError, DatabaseResult};
use crate::database::on_disconnect::OnDisconnect;
use crate::database::push_id::next_push_id;
use crate::database::query::{QueryBound, QueryIndex, QueryLimit, QueryParams};
use crate::database::realtime::{ListenSpec, Repo};
use crate::database::server_value::{current_time_millis, extract_data_ref};
use crate::logger::Logger;
use crate::platform::runtime;

/// Attempts a transaction makes before giving up, matching the Web SDK's retry budget.
const MAX_TRANSACTION_ATTEMPTS: usize = 25;

static REALTIME_LOGGER: LazyLock<Logger> = LazyLock::new(|| Logger::new("@firebase/database/realtime"));

#[derive(Clone, Debug)]
pub struct Database {
    inner: Arc<DatabaseInner>,
}

struct DatabaseInner {
    app: FirebaseApp,
    /// Swappable so `connect_database_emulator` can re-point a database that has already been
    /// created, the way `connectDatabaseEmulator` does in the JS SDK.
    backend: RwLock<Arc<dyn DatabaseBackend>>,
    repo: RwLock<Arc<Repo>>,
    listeners: Mutex<HashMap<u64, Listener>>,
    next_listener_id: AtomicU64,
    /// Partial mirror of the server's tree, holding only the data this client has read or
    /// received for its listeners. The Web SDK keeps the same kind of local view in its
    /// `SyncTree`; nothing here is ever populated by reading the whole database.
    mirror: Mutex<Value>,
}

impl fmt::Debug for DatabaseInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseInner")
            .field("app", &self.app.name())
            .field("backend", &"dynamic")
            .field("repo", &"realtime")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseReference {
    database: Database,
    path: Vec<String>,
}

/// Represents a composable database query, analogous to the JS `QueryImpl`
/// (`packages/database/src/api/Reference_impl.ts`).
#[derive(Clone, Debug)]
pub struct DatabaseQuery {
    reference: DatabaseReference,
    params: QueryParams,
}

/// Represents a single constraint produced by helpers such as `order_by_child()`
/// (`packages/database/src/api/Reference_impl.ts`).
#[derive(Clone, Debug)]
pub struct QueryConstraint {
    kind: QueryConstraintKind,
}

#[derive(Clone, Debug)]
enum QueryConstraintKind {
    OrderByChild(String),
    OrderByKey,
    OrderByValue,
    OrderByPriority,
    Start {
        value: Value,
        name: Option<String>,
        inclusive: bool,
    },
    End {
        value: Value,
        name: Option<String>,
        inclusive: bool,
    },
    LimitFirst(u32),
    LimitLast(u32),
    EqualTo {
        value: Value,
        name: Option<String>,
    },
}

type ValueListenerCallback = Arc<dyn Fn(Result<DataSnapshot, DatabaseError>) + Send + Sync>;
type ChildListenerCallback = Arc<dyn Fn(Result<ChildEvent, DatabaseError>) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildEventType {
    Added,
    Changed,
    Removed,
}

#[derive(Clone)]
pub struct ChildEvent {
    pub event: ChildEventType,
    pub snapshot: DataSnapshot,
    pub previous_name: Option<String>,
}

#[derive(Clone)]
enum ListenerKind {
    Value(ValueListenerCallback),
    Child {
        event: ChildEventType,
        callback: ChildListenerCallback,
    },
}

#[derive(Clone)]
struct Listener {
    target: ListenerTarget,
    kind: ListenerKind,
    spec: ListenSpec,
}

#[derive(Clone)]
enum ListenerTarget {
    Reference(Vec<String>),
    Query { path: Vec<String>, params: QueryParams },
}

impl ListenerTarget {
    fn matches(&self, changed_path: &[String]) -> bool {
        match self {
            ListenerTarget::Reference(path) => paths_related(path, changed_path),
            ListenerTarget::Query { path, .. } => paths_related(path, changed_path),
        }
    }
}

/// Represents a data snapshot returned to listeners, analogous to the JS
/// `DataSnapshot` type.
#[derive(Clone, Debug)]
pub struct DataSnapshot {
    reference: DatabaseReference,
    value: Value,
}

impl DataSnapshot {
    pub fn reference(&self) -> &DatabaseReference {
        &self.reference
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn exists(&self) -> bool {
        !self.value.is_null()
    }

    pub fn key(&self) -> Option<&str> {
        self.reference.key()
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    /// Returns a snapshot for the provided relative path, mirroring
    /// `DataSnapshot.child(path)` in `Reference_impl.ts`.
    pub fn child(&self, relative_path: &str) -> DatabaseResult<DataSnapshot> {
        let segments = normalize_path(relative_path)?;
        let child_reference = self.reference.child(relative_path)?;
        let value = get_value_at_path(&self.value, &segments).unwrap_or(Value::Null);
        Ok(DataSnapshot {
            reference: child_reference,
            value,
        })
    }

    /// Returns whether data exists at the provided child path, mirroring the JS
    /// `DataSnapshot.hasChild()` helper.
    pub fn has_child(&self, relative_path: &str) -> DatabaseResult<bool> {
        let segments = normalize_path(relative_path)?;
        Ok(get_value_at_path(&self.value, &segments)
            .map(|value| !value.is_null())
            .unwrap_or(false))
    }

    /// Returns whether the snapshot has any direct child properties, mirroring
    /// `DataSnapshot.hasChildren()`.
    pub fn has_children(&self) -> bool {
        match extract_data_ref(&self.value) {
            Value::Object(map) => !map.is_empty(),
            Value::Array(array) => !array.is_empty(),
            _ => false,
        }
    }

    /// Returns the number of direct child properties, mirroring the JS `size` getter.
    pub fn size(&self) -> usize {
        match extract_data_ref(&self.value) {
            Value::Object(map) => map.len(),
            Value::Array(array) => array.len(),
            _ => 0,
        }
    }

    /// Returns the JSON representation (including priority metadata) of this snapshot.
    pub fn to_json(&self) -> Value {
        self.value.clone()
    }
}

/// RAII-style listener registration; dropping the handle detaches the
/// underlying listener.
impl fmt::Debug for ListenerRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListenerRegistration").field("id", &self.id).finish()
    }
}

pub struct ListenerRegistration {
    database: Database,
    id: Option<u64>,
}

impl ListenerRegistration {
    fn new(database: Database, id: u64) -> Self {
        Self { database, id: Some(id) }
    }

    pub fn detach(mut self) {
        if let Some(id) = self.id.take() {
            self.database.remove_listener(id);
        }
    }
}

/// Result returned by `run_transaction`, mirroring the JS SDK contract.
#[derive(Clone, Debug)]
pub struct TransactionResult {
    /// Indicates whether the transaction committed (i.e. the update function
    /// returned `Some` and the write succeeded).
    pub committed: bool,
    /// Snapshot reflecting the data at the location after the transaction.
    pub snapshot: DataSnapshot,
}

impl Drop for ListenerRegistration {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.database.remove_listener(id);
        }
    }
}

impl QueryConstraint {
    fn new(kind: QueryConstraintKind) -> Self {
        Self { kind }
    }

    fn apply(self, query: DatabaseQuery) -> DatabaseResult<DatabaseQuery> {
        match self.kind {
            QueryConstraintKind::OrderByChild(path) => query.order_by_child(&path),
            QueryConstraintKind::OrderByKey => query.order_by_key(),
            QueryConstraintKind::OrderByValue => query.order_by_value(),
            QueryConstraintKind::OrderByPriority => query.order_by_priority(),
            QueryConstraintKind::Start { value, name, inclusive } => {
                if inclusive {
                    query.start_at_with_key(value, name)
                } else {
                    query.start_after_with_key(value, name)
                }
            }
            QueryConstraintKind::End { value, name, inclusive } => {
                if inclusive {
                    query.end_at_with_key(value, name)
                } else {
                    query.end_before_with_key(value, name)
                }
            }
            QueryConstraintKind::LimitFirst(limit) => query.limit_to_first(limit),
            QueryConstraintKind::LimitLast(limit) => query.limit_to_last(limit),
            QueryConstraintKind::EqualTo { value, name } => query.equal_to_with_key(value, name),
        }
    }
}

/// Creates a derived query by applying the provided constraints, following the
/// semantics of `query()` in `packages/database/src/api/Reference_impl.ts`.
pub fn query(
    reference: DatabaseReference,
    constraints: impl IntoIterator<Item = QueryConstraint>,
) -> DatabaseResult<DatabaseQuery> {
    let mut current = reference.query();
    for constraint in constraints {
        current = constraint.apply(current)?;
    }
    Ok(current)
}

/// Produces a constraint that orders the results by the specified child path.
/// Mirrors `orderByChild()` from the JS SDK.
pub fn order_by_child(path: impl Into<String>) -> QueryConstraint {
    QueryConstraint::new(QueryConstraintKind::OrderByChild(path.into()))
}

/// Produces a constraint that orders nodes by key (`orderByKey()` in JS).
pub fn order_by_key() -> QueryConstraint {
    QueryConstraint::new(QueryConstraintKind::OrderByKey)
}

/// Produces a constraint that orders nodes by priority (`orderByPriority()` in JS).
pub fn order_by_priority() -> QueryConstraint {
    QueryConstraint::new(QueryConstraintKind::OrderByPriority)
}

/// Produces a constraint that orders nodes by value (`orderByValue()` in JS).
pub fn order_by_value() -> QueryConstraint {
    QueryConstraint::new(QueryConstraintKind::OrderByValue)
}

/// Mirrors the JS `startAt()` constraint (`Reference_impl.ts`).
pub fn start_at<V>(value: V) -> QueryConstraint
where
    V: Into<Value>,
{
    QueryConstraint::new(QueryConstraintKind::Start {
        value: value.into(),
        name: None,
        inclusive: true,
    })
}

/// Mirrors the JS `startAt(value, name)` overload (`Reference_impl.ts`).
pub fn start_at_with_key<V, S>(value: V, name: S) -> QueryConstraint
where
    V: Into<Value>,
    S: Into<String>,
{
    QueryConstraint::new(QueryConstraintKind::Start {
        value: value.into(),
        name: Some(name.into()),
        inclusive: true,
    })
}

/// Mirrors the JS `startAfter()` constraint (`Reference_impl.ts`).
pub fn start_after<V>(value: V) -> QueryConstraint
where
    V: Into<Value>,
{
    QueryConstraint::new(QueryConstraintKind::Start {
        value: value.into(),
        name: None,
        inclusive: false,
    })
}

/// Mirrors the JS `startAfter(value, name)` overload (`Reference_impl.ts`).
pub fn start_after_with_key<V, S>(value: V, name: S) -> QueryConstraint
where
    V: Into<Value>,
    S: Into<String>,
{
    QueryConstraint::new(QueryConstraintKind::Start {
        value: value.into(),
        name: Some(name.into()),
        inclusive: false,
    })
}

/// Mirrors the JS `endAt()` constraint (`Reference_impl.ts`).
pub fn end_at<V>(value: V) -> QueryConstraint
where
    V: Into<Value>,
{
    QueryConstraint::new(QueryConstraintKind::End {
        value: value.into(),
        name: None,
        inclusive: true,
    })
}

/// Mirrors the JS `endAt(value, name)` overload (`Reference_impl.ts`).
pub fn end_at_with_key<V, S>(value: V, name: S) -> QueryConstraint
where
    V: Into<Value>,
    S: Into<String>,
{
    QueryConstraint::new(QueryConstraintKind::End {
        value: value.into(),
        name: Some(name.into()),
        inclusive: true,
    })
}

/// Mirrors the JS `endBefore()` constraint (`Reference_impl.ts`).
pub fn end_before<V>(value: V) -> QueryConstraint
where
    V: Into<Value>,
{
    QueryConstraint::new(QueryConstraintKind::End {
        value: value.into(),
        name: None,
        inclusive: false,
    })
}

/// Mirrors the JS `endBefore(value, name)` overload (`Reference_impl.ts`).
pub fn end_before_with_key<V, S>(value: V, name: S) -> QueryConstraint
where
    V: Into<Value>,
    S: Into<String>,
{
    QueryConstraint::new(QueryConstraintKind::End {
        value: value.into(),
        name: Some(name.into()),
        inclusive: false,
    })
}

/// Mirrors the JS `limitToFirst()` constraint (`Reference_impl.ts`).
pub fn limit_to_first(limit: u32) -> QueryConstraint {
    QueryConstraint::new(QueryConstraintKind::LimitFirst(limit))
}

/// Mirrors the JS `limitToLast()` constraint (`Reference_impl.ts`).
pub fn limit_to_last(limit: u32) -> QueryConstraint {
    QueryConstraint::new(QueryConstraintKind::LimitLast(limit))
}

/// Mirrors the JS `equalTo()` constraint (`Reference_impl.ts`).
pub fn equal_to<V>(value: V) -> QueryConstraint
where
    V: Into<Value>,
{
    QueryConstraint::new(QueryConstraintKind::EqualTo {
        value: value.into(),
        name: None,
    })
}

/// Mirrors the JS `equalTo(value, name)` overload (`Reference_impl.ts`).
pub fn equal_to_with_key<V, S>(value: V, name: S) -> QueryConstraint
where
    V: Into<Value>,
    S: Into<String>,
{
    QueryConstraint::new(QueryConstraintKind::EqualTo {
        value: value.into(),
        name: Some(name.into()),
    })
}

/// Generates a child location with an auto-generated push ID.
///
/// Mirrors the modular `push()` helper from the JS SDK
/// (`packages/database/src/api/Reference_impl.ts`).
pub async fn push(reference: &DatabaseReference) -> DatabaseResult<DatabaseReference> {
    reference.push().await
}

/// Generates a child location with an auto-generated push ID and writes the provided value.
///
/// Mirrors the modular `push(ref, value)` overload from the JS SDK
/// (`packages/database/src/api/Reference_impl.ts`).
pub async fn push_with_value<V>(reference: &DatabaseReference, value: V) -> DatabaseResult<DatabaseReference>
where
    V: Into<Value>,
{
    reference.push_with_value(value).await
}

/// Registers a `value` listener for the provided reference.
#[allow(dead_code)]
pub async fn on_value<F>(reference: &DatabaseReference, callback: F) -> DatabaseResult<ListenerRegistration>
where
    F: Fn(Result<DataSnapshot, DatabaseError>) + Send + Sync + 'static,
{
    reference.on_value(callback).await
}

/// Registers a `child_added` listener for the provided reference.
pub async fn on_child_added<F>(reference: &DatabaseReference, callback: F) -> DatabaseResult<ListenerRegistration>
where
    F: Fn(Result<ChildEvent, DatabaseError>) + Send + Sync + 'static,
{
    reference.on_child_added(callback).await
}

/// Registers a `child_changed` listener for the provided reference.
pub async fn on_child_changed<F>(reference: &DatabaseReference, callback: F) -> DatabaseResult<ListenerRegistration>
where
    F: Fn(Result<ChildEvent, DatabaseError>) + Send + Sync + 'static,
{
    reference.on_child_changed(callback).await
}

/// Registers a `child_removed` listener for the provided reference.
pub async fn on_child_removed<F>(reference: &DatabaseReference, callback: F) -> DatabaseResult<ListenerRegistration>
where
    F: Fn(Result<ChildEvent, DatabaseError>) + Send + Sync + 'static,
{
    reference.on_child_removed(callback).await
}

/// Runs a transaction at the provided reference, mirroring the JS SDK.
///
/// The update closure receives the current value and can return `Some(new_value)`
/// to commit the write or `None` to abort. The operation currently uses a
/// best-effort optimistic strategy when hitting the REST backend; concurrent
/// writers may lead to retries from user-space code.
pub async fn run_transaction<F>(reference: &DatabaseReference, mut update: F) -> DatabaseResult<TransactionResult>
where
    F: FnMut(Value) -> Option<Value>,
{
    reference.run_transaction(|value| update(value)).await
}

/// Writes a value together with a priority, mirroring the modular `setWithPriority()` helper
/// (`packages/database/src/api/Reference_impl.ts`).
pub async fn set_with_priority<V, P>(reference: &DatabaseReference, value: V, priority: P) -> DatabaseResult<()>
where
    V: Into<Value>,
    P: Into<Value>,
{
    reference.set_with_priority(value, priority).await
}

/// Updates the priority for the current location, mirroring the modular `setPriority()` helper
/// (`packages/database/src/api/Reference_impl.ts`).
pub async fn set_priority<P>(reference: &DatabaseReference, priority: P) -> DatabaseResult<()>
where
    P: Into<Value>,
{
    reference.set_priority(priority).await
}

impl Database {
    fn new(app: FirebaseApp) -> Self {
        let repo = Repo::new_for_app(&app);
        let inner = Arc::new(DatabaseInner {
            backend: RwLock::new(select_backend(&app)),
            repo: RwLock::new(repo.clone()),
            app,
            listeners: Mutex::new(HashMap::new()),
            next_listener_id: AtomicU64::new(1),
            mirror: Mutex::new(Value::Object(Map::new())),
        });
        let database = Self { inner };
        let handler_db = database.clone();
        repo.set_event_handler(Arc::new(move |action, body| {
            let database = handler_db.clone();
            Box::pin(async move { database.handle_realtime_action(&action, &body).await })
        }));
        database
    }

    /// True when a value listener keeps `path` (or one of its ancestors) in sync, so the mirror
    /// holds a complete view of it.
    fn is_synced(&self, path: &[String]) -> bool {
        let listeners = self.inner.listeners.lock().unwrap();
        listeners.values().any(|listener| match &listener.target {
            ListenerTarget::Reference(listen_path) => {
                listen_path.len() <= path.len() && path[..listen_path.len()] == listen_path[..]
            }
            ListenerTarget::Query { .. } => false,
        })
    }

    /// Returns the current local mirror. This never touches the network: data lands here when a
    /// listener is attached, when the realtime connection pushes an update, and after local writes.
    fn mirror_snapshot(&self) -> Value {
        self.inner.mirror.lock().unwrap().clone()
    }

    /// Applies a write to the local mirror and notifies listeners about what changed.
    async fn apply_local_write(&self, path: &[String], value: Value) -> DatabaseResult<()> {
        let (old_root, new_root) = {
            let mut mirror = self.inner.mirror.lock().unwrap();
            let old_root = mirror.clone();
            apply_realtime_value(&mut mirror, path, value);
            (old_root, mirror.clone())
        };
        self.dispatch_listeners(path, &old_root, &new_root).await
    }

    /// Seeds the mirror for a newly attached listener and returns the mirror.
    async fn seed_listener_path(&self, target: &ListenerTarget) -> DatabaseResult<Value> {
        match target {
            ListenerTarget::Reference(path) => self.seed_mirror(path).await,
            // Query listeners re-read their window from the server on every event, so there is
            // nothing to seed.
            ListenerTarget::Query { .. } => Ok(self.mirror_snapshot()),
        }
    }

    /// Reads `path` from the server and seeds the mirror with it, so a freshly attached listener
    /// can report an initial value without reading the whole database.
    async fn seed_mirror(&self, path: &[String]) -> DatabaseResult<Value> {
        let value = self.backend().get(path, &[]).await?;
        let mut mirror = self.inner.mirror.lock().unwrap();
        apply_realtime_value(&mut mirror, path, value);
        Ok(mirror.clone())
    }

    pub(crate) fn repo(&self) -> Arc<Repo> {
        self.inner.repo.read().unwrap().clone()
    }

    fn backend(&self) -> Arc<dyn DatabaseBackend> {
        self.inner.backend.read().unwrap().clone()
    }

    /// The Realtime Database namespace this instance talks to (`<project>-default-rtdb` unless the
    /// configured `databaseURL` names another one).
    fn namespace(&self) -> String {
        let configured = self.inner.app.options().database_url.and_then(|url| {
            let parsed = url::Url::parse(&url).ok()?;
            parsed
                .query_pairs()
                .find(|(key, _)| key == "ns")
                .map(|(_, value)| value.into_owned())
                .or_else(|| {
                    parsed
                        .host_str()
                        .and_then(|host| host.split('.').next().map(|label| label.to_string()))
                })
        });
        configured.unwrap_or_else(|| {
            let project = self.inner.app.options().project_id.unwrap_or_default();
            format!("{project}-default-rtdb")
        })
    }

    /// Routes this database at the Realtime Database emulator, mirroring `connectDatabaseEmulator`.
    ///
    /// Both the REST channel and the realtime connection move, so reads, writes and listeners all
    /// talk to the emulator. Call it before attaching listeners; the JS SDK likewise refuses once
    /// the instance is in use.
    pub fn connect_emulator(&self, host: &str, port: u16) -> DatabaseResult<()> {
        if !self.inner.listeners.lock().unwrap().is_empty() {
            return Err(invalid_argument(
                "connect_database_emulator must be called before attaching listeners",
            ));
        }

        let url = format!("http://{host}:{port}/?ns={}", self.namespace());
        crate::database::backend::set_database_url_override(self.inner.app.name(), url);

        *self.inner.backend.write().unwrap() = select_backend(&self.inner.app);

        let repo = Repo::new_for_app(&self.inner.app);
        let handler_db = self.clone();
        repo.set_event_handler(Arc::new(move |action, body| {
            let database = handler_db.clone();
            Box::pin(async move { database.handle_realtime_action(&action, &body).await })
        }));
        *self.inner.repo.write().unwrap() = repo;
        *self.inner.mirror.lock().unwrap() = Value::Object(Map::new());
        Ok(())
    }

    #[allow(dead_code)]
    #[cfg(test)]
    fn clear_mirror_for_test(&self) {
        *self.inner.mirror.lock().unwrap() = Value::Object(Map::new());
    }

    async fn handle_realtime_action(&self, action: &str, body: &serde_json::Value) -> DatabaseResult<()> {
        match action {
            "d" | "m" => self.handle_realtime_data(action, body).await,
            "c" => {
                REALTIME_LOGGER.warn("listener revoked by server".to_string());
                self.revoke_listener(body).await;
                Ok(())
            }
            "ac" | "apc" => {
                REALTIME_LOGGER.warn(format!("credential revoked by server ({action})"));
                self.fail_listeners(internal_error(format!("realtime credential revoked: {action}")));
                Ok(())
            }
            "error" => {
                let message = body.as_str().unwrap_or("realtime connection error");
                self.fail_listeners(internal_error(message.to_string()));
                Ok(())
            }
            "sd" => Ok(()),
            other => Err(internal_error(format!("unhandled realtime action '{other}'"))),
        }
    }

    async fn handle_realtime_data(&self, action: &str, body: &serde_json::Value) -> DatabaseResult<()> {
        let Some(path) = body.get("p").and_then(|value| value.as_str()) else {
            return Ok(());
        };
        let data = body.get("d").cloned().unwrap_or(serde_json::Value::Null);

        let segments = normalize_path(path)?;
        let old_root = self.mirror_snapshot();
        let mut new_root = old_root.clone();

        match action {
            "d" => apply_realtime_value(&mut new_root, &segments, data.clone()),
            "m" => {
                let Value::Object(map) = &data else {
                    return Err(invalid_argument("Realtime merge payload must be a JSON object"));
                };
                for (key, value) in map.iter() {
                    let mut child_path = segments.clone();
                    child_path.extend(normalize_path(key)?);
                    apply_realtime_value(&mut new_root, &child_path, value.clone());
                }
            }
            _ => {}
        }

        REALTIME_LOGGER.debug(format!("realtime payload action={action} path={path} data={data:?}"));

        *self.inner.mirror.lock().unwrap() = new_root.clone();
        self.dispatch_listeners(&segments, &old_root, &new_root).await?;
        Ok(())
    }

    async fn revoke_listener(&self, body: &serde_json::Value) {
        let Some(path) = body.get("p").and_then(|value| value.as_str()) else {
            return;
        };
        let segments = match normalize_path(path) {
            Ok(segments) => segments,
            Err(err) => {
                REALTIME_LOGGER.warn(format!("failed to normalise revoked path: {err}"));
                return;
            }
        };

        let (removed, should_disconnect) = {
            let mut listeners = self.inner.listeners.lock().unwrap();
            let cancelled: Vec<u64> = listeners
                .iter()
                .filter(|(_, listener)| match &listener.target {
                    ListenerTarget::Reference(path) => path == &segments,
                    ListenerTarget::Query { path, .. } => path == &segments,
                })
                .map(|(id, _)| *id)
                .collect();

            let removed = cancelled
                .into_iter()
                .filter_map(|id| listeners.remove(&id))
                .collect::<Vec<_>>();
            let should_disconnect = listeners.is_empty();
            (removed, should_disconnect)
        };

        if removed.is_empty() {
            return;
        }

        for listener in &removed {
            if let Err(err) = self.repo().unlisten(listener.spec.clone()).await {
                REALTIME_LOGGER.warn(format!("failed to detach revoked realtime listener: {err}"));
            }
        }

        // Preserve what the server said: a listen rejected by the rules must surface as
        // `permission_denied`, matching the JS SDK's cancel callback.
        let status = body.get("s").and_then(|value| value.as_str()).unwrap_or_default();
        let detail = body
            .get("d")
            .and_then(|value| value.as_str())
            .unwrap_or("listener revoked by server")
            .to_string();
        let error = if status == "permission_denied" {
            permission_denied(detail)
        } else {
            internal_error(detail)
        };
        for listener in removed {
            match listener.kind {
                ListenerKind::Value(callback) => {
                    callback(Err(error.clone()));
                }
                ListenerKind::Child { callback, .. } => {
                    callback(Err(error.clone()));
                }
            }
        }

        if should_disconnect {
            if let Err(err) = self.go_offline().await {
                REALTIME_LOGGER.warn(format!("failed to go offline after listener revocation: {err}"));
            }
        }
    }

    fn fail_listeners(&self, err: DatabaseError) {
        let mut guard = self.inner.listeners.lock().unwrap();
        let listeners = guard.values().cloned().collect::<Vec<_>>();
        guard.clear();
        drop(guard);
        for listener in listeners {
            REALTIME_LOGGER.warn(format!("listener cancelled due to error: {err}"));
            match listener.kind {
                ListenerKind::Value(callback) => {
                    callback(Err(err.clone()));
                }
                ListenerKind::Child { callback, .. } => {
                    callback(Err(err.clone()));
                }
            }
        }
    }

    pub async fn go_online(&self) -> DatabaseResult<()> {
        self.repo().go_online().await
    }

    pub async fn go_offline(&self) -> DatabaseResult<()> {
        self.repo().go_offline().await
    }

    pub fn app(&self) -> &FirebaseApp {
        &self.inner.app
    }

    pub fn reference(&self, path: &str) -> DatabaseResult<DatabaseReference> {
        let segments = normalize_path(path)?;
        Ok(DatabaseReference {
            database: self.clone(),
            path: segments,
        })
    }

    fn reference_from_segments(&self, segments: Vec<String>) -> DatabaseReference {
        DatabaseReference {
            database: self.clone(),
            path: segments,
        }
    }

    /// Every listener subscribes to a plain path on the realtime connection.
    ///
    /// The wire protocol only accepts a filtered listen when it carries a tag that routes the
    /// server's windowed pushes (an untagged query listen makes the server drop the whole
    /// connection). Until tagged views are ported, a query listener subscribes to its path and
    /// re-runs the query against the server whenever that path changes; sharing one spec across
    /// every query on a path also lets the repo reference-count them.
    fn listen_spec_for_target(&self, target: &ListenerTarget) -> DatabaseResult<ListenSpec> {
        let path = match target {
            ListenerTarget::Reference(path) => path.clone(),
            ListenerTarget::Query { path, .. } => path.clone(),
        };
        Ok(ListenSpec::new(path, Vec::new()))
    }

    async fn register_listener(
        &self,
        target: ListenerTarget,
        kind: ListenerKind,
    ) -> DatabaseResult<ListenerRegistration> {
        let spec = self.listen_spec_for_target(&target)?;
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::SeqCst);

        let first_listener = {
            let mut listeners = self.inner.listeners.lock().unwrap();
            let was_empty = listeners.is_empty();
            listeners.insert(
                id,
                Listener {
                    target: target.clone(),
                    kind: kind.clone(),
                    spec: spec.clone(),
                },
            );
            was_empty
        };

        if first_listener {
            if let Err(err) = self.go_online().await {
                let mut listeners = self.inner.listeners.lock().unwrap();
                listeners.remove(&id);
                return Err(err);
            }
        }

        if let Err(err) = self.repo().listen(spec.clone()).await {
            self.inner.listeners.lock().unwrap().remove(&id);
            if first_listener {
                let _ = self.go_offline().await;
            }
            return Err(err);
        }

        let current_root = match self.seed_listener_path(&target).await {
            Ok(root) => root,
            Err(err) => {
                self.remove_listener(id);
                return Err(err);
            }
        };
        match kind {
            ListenerKind::Value(callback) => {
                let snapshot = self.snapshot_from_root(&target, &current_root).await?;
                callback(Ok(snapshot));
            }
            ListenerKind::Child { event, callback } => {
                if let Err(err) = self.fire_initial_child_events(&target, event, &callback, &current_root) {
                    self.remove_listener(id);
                    return Err(err);
                }
            }
        }

        Ok(ListenerRegistration::new(self.clone(), id))
    }

    fn remove_listener(&self, id: u64) {
        let (listener, should_disconnect) = {
            let mut listeners = self.inner.listeners.lock().unwrap();
            let removed = listeners.remove(&id);
            let should_disconnect = listeners.is_empty();
            (removed, should_disconnect)
        };

        if let Some(listener) = listener {
            let repo = self.repo();
            let spec = listener.spec.clone();
            runtime::spawn_detached(async move {
                if let Err(err) = repo.unlisten(spec).await {
                    REALTIME_LOGGER.warn(format!("failed to detach realtime listener during cleanup: {err}"));
                }
            });
        }

        if should_disconnect {
            let database = self.clone();
            runtime::spawn_detached(async move {
                if let Err(err) = database.go_offline().await {
                    REALTIME_LOGGER.warn(format!("failed to go offline after removing last listener: {err}"));
                }
            });
        }
    }

    async fn dispatch_listeners(
        &self,
        changed_path: &[String],
        old_root: &Value,
        new_root: &Value,
    ) -> DatabaseResult<()> {
        let listeners: Vec<Listener> = {
            let listeners = self.inner.listeners.lock().unwrap();
            listeners
                .values()
                .filter(|listener| listener.target.matches(changed_path))
                .cloned()
                .collect()
        };

        for listener in listeners {
            match &listener.kind {
                ListenerKind::Value(callback) => {
                    // The Web SDK only raises a value event when the view actually changed; without
                    // this the server's echo of a local write would fire a duplicate event.
                    if let ListenerTarget::Reference(path) = &listener.target {
                        if value_at_path(old_root, path) == value_at_path(new_root, path) {
                            continue;
                        }
                    }
                    let snapshot = self.snapshot_from_root(&listener.target, new_root).await?;
                    callback(Ok(snapshot));
                }
                ListenerKind::Child { event, callback } => {
                    self.invoke_child_listener(&listener, *event, callback, old_root, new_root)?;
                }
            }
        }
        Ok(())
    }

    async fn snapshot_from_root(&self, target: &ListenerTarget, root: &Value) -> DatabaseResult<DataSnapshot> {
        match target {
            ListenerTarget::Reference(path) => {
                let value = value_at_path(root, path);
                let reference = self.reference_from_segments(path.clone());
                Ok(DataSnapshot { reference, value })
            }
            ListenerTarget::Query { .. } => self.snapshot_for_target(target).await,
        }
    }

    fn fire_initial_child_events(
        &self,
        target: &ListenerTarget,
        event: ChildEventType,
        callback: &ChildListenerCallback,
        root: &Value,
    ) -> DatabaseResult<()> {
        if event != ChildEventType::Added {
            return Ok(());
        }

        if let ListenerTarget::Reference(path) = target {
            let new_value = value_at_path(root, path);
            let empty = Value::Null;
            self.emit_child_events(path, event, callback, &empty, &new_value)?;
        }
        Ok(())
    }

    fn invoke_child_listener(
        &self,
        listener: &Listener,
        event: ChildEventType,
        callback: &ChildListenerCallback,
        old_root: &Value,
        new_root: &Value,
    ) -> DatabaseResult<()> {
        let ListenerTarget::Reference(path) = &listener.target else {
            return Ok(());
        };
        let old_value = value_at_path(old_root, path);
        let new_value = value_at_path(new_root, path);
        self.emit_child_events(path, event, callback, &old_value, &new_value)
    }

    fn emit_child_events(
        &self,
        parent_path: &[String],
        event: ChildEventType,
        callback: &ChildListenerCallback,
        old_value: &Value,
        new_value: &Value,
    ) -> DatabaseResult<()> {
        let old_children = children_map(old_value);
        let new_children = children_map(new_value);

        match event {
            ChildEventType::Added => {
                let new_keys: Vec<String> = new_children.keys().cloned().collect();
                for key in new_keys.iter() {
                    if !old_children.contains_key(key) {
                        let value = new_children.get(key).cloned().unwrap_or(Value::Null);
                        let prev_name = previous_key(&new_keys, key);
                        let snapshot = self.child_snapshot(parent_path, key, value.clone());
                        callback(Ok(ChildEvent {
                            event,
                            snapshot,
                            previous_name: prev_name,
                        }));
                    }
                }
            }
            ChildEventType::Changed => {
                let new_keys: Vec<String> = new_children.keys().cloned().collect();
                for key in new_keys.iter() {
                    if let Some(old_value_child) = old_children.get(key) {
                        let new_child = new_children.get(key).expect("child exists in map");
                        if old_value_child != new_child {
                            let value = new_child.clone();
                            let prev_name = previous_key(&new_keys, key);
                            let snapshot = self.child_snapshot(parent_path, key, value);
                            callback(Ok(ChildEvent {
                                event,
                                snapshot,
                                previous_name: prev_name,
                            }));
                        }
                    }
                }
            }
            ChildEventType::Removed => {
                let old_keys: Vec<String> = old_children.keys().cloned().collect();
                for key in old_keys.iter() {
                    if !new_children.contains_key(key) {
                        let value = old_children.get(key).cloned().unwrap_or(Value::Null);
                        let prev_name = previous_key(&old_keys, key);
                        let snapshot = self.child_snapshot(parent_path, key, value);
                        callback(Ok(ChildEvent {
                            event,
                            snapshot,
                            previous_name: prev_name,
                        }));
                    }
                }
            }
        }
        Ok(())
    }

    fn child_snapshot(&self, parent_path: &[String], child_key: &str, value: Value) -> DataSnapshot {
        let mut segments = parent_path.to_vec();
        segments.push(child_key.to_string());
        let reference = self.reference_from_segments(segments);
        DataSnapshot { reference, value }
    }

    async fn snapshot_for_target(&self, target: &ListenerTarget) -> DatabaseResult<DataSnapshot> {
        match target {
            ListenerTarget::Reference(path) => {
                let value = self.backend().get(path, &[]).await?;
                let reference = self.reference_from_segments(path.clone());
                Ok(DataSnapshot { reference, value })
            }
            ListenerTarget::Query { path, params } => {
                let rest_params = params.to_rest_params()?;
                let value = self.backend().get(path, rest_params.as_slice()).await?;
                let reference = self.reference_from_segments(path.clone());
                Ok(DataSnapshot { reference, value })
            }
        }
    }
}

impl DatabaseReference {
    pub fn child(&self, relative: &str) -> DatabaseResult<DatabaseReference> {
        let mut segments = self.path.clone();
        segments.extend(normalize_path(relative)?);
        Ok(DatabaseReference {
            database: self.database.clone(),
            path: segments,
        })
    }

    /// Returns the parent of this reference, mirroring `ref.parent` in the JS SDK.
    pub fn parent(&self) -> Option<DatabaseReference> {
        if self.path.is_empty() {
            None
        } else {
            let mut parent = self.path.clone();
            parent.pop();
            Some(DatabaseReference {
                database: self.database.clone(),
                path: parent,
            })
        }
    }

    /// Returns the root of the database, mirroring `ref.root` in the JS SDK.
    pub fn root(&self) -> DatabaseReference {
        DatabaseReference {
            database: self.database.clone(),
            path: Vec::new(),
        }
    }

    /// Writes `value` at this location, mirroring `set()` in the JS SDK.
    ///
    /// `ServerValue` placeholders ([`server_timestamp`](crate::database::server_timestamp),
    /// [`increment`](crate::database::increment)) are resolved by the server, so an `increment` is
    /// atomic even when several clients write at once.
    pub async fn set(&self, value: Value) -> DatabaseResult<()> {
        let stored = self.database.backend().set(&self.path, value).await?;
        self.database.apply_local_write(&self.path, stored).await
    }

    /// Creates a query anchored at this reference, mirroring the JS `query()` helper.
    pub fn query(&self) -> DatabaseQuery {
        DatabaseQuery {
            reference: self.clone(),
            params: QueryParams::default(),
        }
    }

    /// Returns a query ordered by the provided child path, mirroring `orderByChild()`.
    pub fn order_by_child(&self, path: &str) -> DatabaseResult<DatabaseQuery> {
        self.query().order_by_child(path)
    }

    /// Returns a query ordered by key, mirroring `orderByKey()`.
    pub fn order_by_key(&self) -> DatabaseResult<DatabaseQuery> {
        self.query().order_by_key()
    }

    /// Returns a query ordered by value, mirroring `orderByValue()`.
    pub fn order_by_value(&self) -> DatabaseResult<DatabaseQuery> {
        self.query().order_by_value()
    }

    /// Returns a query ordered by priority, mirroring `orderByPriority()`.
    pub fn order_by_priority(&self) -> DatabaseResult<DatabaseQuery> {
        self.query().order_by_priority()
    }

    /// Registers a value listener for this reference, mirroring `onValue()`.
    pub async fn on_value<F>(&self, callback: F) -> DatabaseResult<ListenerRegistration>
    where
        F: Fn(Result<DataSnapshot, DatabaseError>) + Send + Sync + 'static,
    {
        let user_fn: ValueListenerCallback = Arc::new(callback);
        self.database
            .register_listener(ListenerTarget::Reference(self.path.clone()), ListenerKind::Value(user_fn))
            .await
    }

    /// Registers an `onChildAdded` listener, mirroring the JS SDK.
    pub async fn on_child_added<F>(&self, callback: F) -> DatabaseResult<ListenerRegistration>
    where
        F: Fn(Result<ChildEvent, DatabaseError>) + Send + Sync + 'static,
    {
        let cb: ChildListenerCallback = Arc::new(callback);
        self.database
            .register_listener(
                ListenerTarget::Reference(self.path.clone()),
                ListenerKind::Child {
                    event: ChildEventType::Added,
                    callback: cb,
                },
            )
            .await
    }

    /// Registers an `onChildChanged` listener, mirroring the JS SDK.
    pub async fn on_child_changed<F>(&self, callback: F) -> DatabaseResult<ListenerRegistration>
    where
        F: Fn(Result<ChildEvent, DatabaseError>) + Send + Sync + 'static,
    {
        let cb: ChildListenerCallback = Arc::new(callback);
        self.database
            .register_listener(
                ListenerTarget::Reference(self.path.clone()),
                ListenerKind::Child {
                    event: ChildEventType::Changed,
                    callback: cb,
                },
            )
            .await
    }

    /// Registers an `onChildRemoved` listener, mirroring the JS SDK.
    pub async fn on_child_removed<F>(&self, callback: F) -> DatabaseResult<ListenerRegistration>
    where
        F: Fn(Result<ChildEvent, DatabaseError>) + Send + Sync + 'static,
    {
        let cb: ChildListenerCallback = Arc::new(callback);
        self.database
            .register_listener(
                ListenerTarget::Reference(self.path.clone()),
                ListenerKind::Child {
                    event: ChildEventType::Removed,
                    callback: cb,
                },
            )
            .await
    }

    /// Returns a handle for configuring operations to run when the client disconnects.
    pub fn on_disconnect(&self) -> OnDisconnect {
        OnDisconnect::new(self.clone())
    }

    pub(crate) fn database(&self) -> &Database {
        &self.database
    }

    pub(crate) fn path_segments(&self) -> Vec<String> {
        self.path.clone()
    }

    /// Runs a transaction on this reference. The closure receives the current value and may
    /// return `Some(next)` to commit or `None` to abort, mirroring the JS SDK contract.
    pub async fn run_transaction<F>(&self, mut update: F) -> DatabaseResult<TransactionResult>
    where
        F: FnMut(Value) -> Option<Value>,
    {
        let mut current = self.database.backend().read_for_update(&self.path).await?;

        for _ in 0..MAX_TRANSACTION_ATTEMPTS {
            let input = extract_data_ref(&current.value).clone();
            let Some(new_value) = update(input) else {
                return Ok(TransactionResult {
                    committed: false,
                    snapshot: DataSnapshot {
                        reference: self.clone(),
                        value: current.value,
                    },
                });
            };

            match self
                .database
                .backend()
                .compare_and_set(&self.path, new_value, current.version.clone())
                .await?
            {
                CasOutcome::Committed(stored) => {
                    self.database.apply_local_write(&self.path, stored.clone()).await?;
                    return Ok(TransactionResult {
                        committed: true,
                        snapshot: DataSnapshot {
                            reference: self.clone(),
                            value: stored,
                        },
                    });
                }
                // Somebody wrote first: run the closure again against the data as it is now.
                CasOutcome::Conflict(latest) => current = latest,
            }
        }

        Err(internal_error(format!(
            "Transaction failed after {MAX_TRANSACTION_ATTEMPTS} attempts"
        )))
    }

    /// Applies the provided partial updates to the current location using a single
    /// REST `PATCH` call when available.
    ///
    /// Each key represents a relative child path (e.g. `"profile/name"`).
    /// The method rejects empty keys to mirror the JS SDK behaviour.
    pub async fn update(&self, updates: serde_json::Map<String, Value>) -> DatabaseResult<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut operations = Vec::with_capacity(updates.len());
        for (key, value) in updates {
            if key.trim().is_empty() {
                return Err(invalid_argument("Database update path cannot be empty"));
            }
            let mut segments = self.path.clone();
            let relative = normalize_path(&key)?;
            if relative.is_empty() {
                return Err(invalid_argument("Database update path cannot reference the current location"));
            }
            segments.extend(relative);
            operations.push((segments, value));
        }

        let stored = self.database.backend().update(&self.path, operations).await?;
        for (absolute, value) in stored {
            self.database.apply_local_write(&absolute, value).await?;
        }
        Ok(())
    }

    /// Reads the value at this location, mirroring `get()` in the JS SDK.
    ///
    /// When a listener keeps this location in sync the value comes from that live view (like
    /// `repoGetValue` consulting the sync tree); otherwise the read goes to the server. It is never
    /// served from a stale snapshot of some ancestor that nobody is listening to.
    pub async fn get(&self) -> DatabaseResult<Value> {
        if self.database.is_synced(&self.path) {
            return Ok(value_at_path(&self.database.mirror_snapshot(), &self.path));
        }
        self.database.backend().get(&self.path, &[]).await
    }

    /// Deletes the value at this location using the backend's `DELETE` support.
    pub async fn remove(&self) -> DatabaseResult<()> {
        self.database.backend().delete(&self.path).await?;
        self.database.apply_local_write(&self.path, Value::Null).await
    }

    /// Writes the provided value together with its priority, mirroring
    /// `setWithPriority()` in `packages/database/src/api/Reference_impl.ts`.
    pub async fn set_with_priority<V, P>(&self, value: V, priority: P) -> DatabaseResult<()>
    where
        V: Into<Value>,
        P: Into<Value>,
    {
        let priority = priority.into();
        validate_priority_value(&priority)?;
        if matches!(self.key(), Some(".length" | ".keys")) {
            return Err(invalid_argument("set_with_priority failed: read-only child key"));
        }

        let payload = pack_with_priority(value.into(), priority);
        let stored = self.database.backend().set(&self.path, payload).await?;
        self.database.apply_local_write(&self.path, stored).await
    }

    /// Updates the priority for this location, mirroring `setPriority()` in the JS SDK.
    pub async fn set_priority<P>(&self, priority: P) -> DatabaseResult<()>
    where
        P: Into<Value>,
    {
        let priority = priority.into();
        validate_priority_value(&priority)?;

        let current = self.database.backend().get(&self.path, &[]).await?;
        let value = extract_data_owned(&current);
        let payload = pack_with_priority(value, priority);
        let stored = self.database.backend().set(&self.path, payload).await?;
        self.database.apply_local_write(&self.path, stored).await
    }

    /// Creates a child location with an auto-generated key, mirroring `push()` from the JS SDK.
    ///
    /// Port of `push()` in `packages/database/src/api/Reference_impl.ts`.
    ///
    /// # Examples
    /// ```rust,ignore
    /// # use firebase_rs_sdk::database::{DatabaseReference, DatabaseResult};
    /// # use serde_json::json;
    /// # fn demo(messages: &DatabaseReference) -> DatabaseResult<()> {
    /// let new_message = messages.push_with_value(json!({ "text": "hi" })).await.expected("Failed to push message");
    /// assert!(new_message.key().is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn push(&self) -> DatabaseResult<DatabaseReference> {
        self.push_internal(None).await
    }

    /// Creates a child location with an auto-generated key and writes the provided value.
    ///
    /// Mirrors the `push(ref, value)` overload from `packages/database/src/api/Reference_impl.ts`.
    pub async fn push_with_value<V>(&self, value: V) -> DatabaseResult<DatabaseReference>
    where
        V: Into<Value>,
    {
        self.push_internal(Some(value.into())).await
    }

    async fn push_internal(&self, value: Option<Value>) -> DatabaseResult<DatabaseReference> {
        let timestamp = current_time_millis()?;
        let key = next_push_id(timestamp);
        let child = self.child(&key)?;
        if let Some(value) = value {
            child.set(value).await?;
        }
        Ok(child)
    }

    /// Returns the last path segment (the key) for this reference, mirroring
    /// `ref.key` in the JS SDK.
    pub fn key(&self) -> Option<&str> {
        self.path.last().map(|segment| segment.as_str())
    }

    pub fn path(&self) -> String {
        if self.path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}/", self.path.join("/"))
        }
    }
}

impl DatabaseQuery {
    /// Exposes the underlying reference backing this query.
    pub fn reference(&self) -> &DatabaseReference {
        &self.reference
    }

    /// Orders children by the given path, mirroring `orderByChild()`.
    pub fn order_by_child(mut self, path: &str) -> DatabaseResult<Self> {
        validate_order_by_child_target(path)?;
        let segments = normalize_path(path)?;
        if segments.is_empty() {
            return Err(invalid_argument("orderByChild path cannot be empty"));
        }
        let joined = segments.join("/");
        self.params.set_index(QueryIndex::Child(joined))?;
        Ok(self)
    }

    /// Orders children by key, mirroring `orderByKey()`.
    pub fn order_by_key(mut self) -> DatabaseResult<Self> {
        self.params.set_index(QueryIndex::Key)?;
        Ok(self)
    }

    /// Orders children by value, mirroring `orderByValue()`.
    pub fn order_by_value(mut self) -> DatabaseResult<Self> {
        self.params.set_index(QueryIndex::Value)?;
        Ok(self)
    }

    /// Orders children by priority, mirroring `orderByPriority()`.
    pub fn order_by_priority(mut self) -> DatabaseResult<Self> {
        self.params.set_index(QueryIndex::Priority)?;
        Ok(self)
    }

    /// Applies a `startAt()` constraint to the query.
    pub fn start_at(self, value: Value) -> DatabaseResult<Self> {
        self.start_at_with_key(value, None)
    }

    /// Applies a keyed `startAt(value, name)` constraint to the query.
    pub fn start_at_with_key(mut self, value: Value, name: Option<String>) -> DatabaseResult<Self> {
        let bound = QueryBound {
            value,
            name,
            inclusive: true,
        };
        self.params.set_start(bound)?;
        Ok(self)
    }

    /// Applies a `startAfter()` constraint to the query.
    pub fn start_after(self, value: Value) -> DatabaseResult<Self> {
        self.start_after_with_key(value, None)
    }

    /// Applies a keyed `startAfter(value, name)` constraint to the query.
    pub fn start_after_with_key(mut self, value: Value, name: Option<String>) -> DatabaseResult<Self> {
        let bound = QueryBound {
            value,
            name,
            inclusive: false,
        };
        self.params.set_start(bound)?;
        Ok(self)
    }

    /// Applies an `endAt()` constraint to the query.
    pub fn end_at(self, value: Value) -> DatabaseResult<Self> {
        self.end_at_with_key(value, None)
    }

    /// Applies a keyed `endAt(value, name)` constraint to the query.
    pub fn end_at_with_key(mut self, value: Value, name: Option<String>) -> DatabaseResult<Self> {
        let bound = QueryBound {
            value,
            name,
            inclusive: true,
        };
        self.params.set_end(bound)?;
        Ok(self)
    }

    /// Applies an `endBefore()` constraint to the query.
    pub fn end_before(self, value: Value) -> DatabaseResult<Self> {
        self.end_before_with_key(value, None)
    }

    /// Applies a keyed `endBefore(value, name)` constraint to the query.
    pub fn end_before_with_key(mut self, value: Value, name: Option<String>) -> DatabaseResult<Self> {
        let bound = QueryBound {
            value,
            name,
            inclusive: false,
        };
        self.params.set_end(bound)?;
        Ok(self)
    }

    /// Applies `limitToFirst()` to the query.
    pub fn limit_to_first(mut self, limit: u32) -> DatabaseResult<Self> {
        if limit == 0 {
            return Err(invalid_argument("limitToFirst must be greater than zero"));
        }
        self.params.set_limit(QueryLimit::First(limit))?;
        Ok(self)
    }

    /// Applies `limitToLast()` to the query.
    pub fn limit_to_last(mut self, limit: u32) -> DatabaseResult<Self> {
        if limit == 0 {
            return Err(invalid_argument("limitToLast must be greater than zero"));
        }
        self.params.set_limit(QueryLimit::Last(limit))?;
        Ok(self)
    }

    /// Applies `equalTo()` to the query.
    pub fn equal_to(self, value: Value) -> DatabaseResult<Self> {
        self.equal_to_with_key(value, None)
    }

    /// Applies a keyed `equalTo(value, name)` constraint to the query.
    pub fn equal_to_with_key(mut self, value: Value, name: Option<String>) -> DatabaseResult<Self> {
        let start_bound = QueryBound {
            value: value.clone(),
            name: name.clone(),
            inclusive: true,
        };
        let end_bound = QueryBound {
            value,
            name,
            inclusive: true,
        };
        self.params.set_start(start_bound)?;
        self.params.set_end(end_bound)?;
        Ok(self)
    }

    /// Executes the query and returns the JSON payload, mirroring JS `get()`.
    pub async fn get(&self) -> DatabaseResult<Value> {
        let params = self.params.to_rest_params()?;
        self.reference
            .database
            .backend()
            .get(&self.reference.path, params.as_slice())
            .await
    }

    /// Registers a value listener for this query, mirroring `onValue(query, cb)`.
    pub async fn on_value<F>(&self, callback: F) -> DatabaseResult<ListenerRegistration>
    where
        F: Fn(Result<DataSnapshot, DatabaseError>) + Send + Sync + 'static,
    {
        let user_fn: ValueListenerCallback = Arc::new(callback);
        self.reference
            .database
            .register_listener(
                ListenerTarget::Query {
                    path: self.reference.path.clone(),
                    params: self.params.clone(),
                },
                ListenerKind::Value(user_fn),
            )
            .await
    }
}

pub(crate) fn normalize_path(path: &str) -> DatabaseResult<Vec<String>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err(invalid_argument("Database path cannot contain empty segments"));
        }
        segments.push(segment.to_string());
    }
    Ok(segments)
}

fn validate_order_by_child_target(path: &str) -> DatabaseResult<()> {
    match path {
        "$key" => Err(invalid_argument(
            "order_by_child(\"$key\") is invalid; call order_by_key() instead",
        )),
        "$priority" => Err(invalid_argument(
            "order_by_child(\"$priority\") is invalid; call order_by_priority() instead",
        )),
        "$value" => Err(invalid_argument(
            "order_by_child(\"$value\") is invalid; call order_by_value() instead",
        )),
        _ => Ok(()),
    }
}

fn paths_related(a: &[String], b: &[String]) -> bool {
    is_prefix(a, b) || is_prefix(b, a)
}

fn is_prefix(prefix: &[String], path: &[String]) -> bool {
    if prefix.len() > path.len() {
        return false;
    }
    prefix.iter().zip(path.iter()).all(|(left, right)| left == right)
}

fn apply_realtime_value(root: &mut Value, path: &[String], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }

    let mut current = root;
    for segment in &path[..path.len() - 1] {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        let obj = current.as_object_mut().expect("object ensured");
        current = obj.entry(segment.clone()).or_insert(Value::Object(Map::new()));
    }

    if value.is_null() {
        if let Some(obj) = current.as_object_mut() {
            obj.remove(path.last().expect("path non-empty"));
        }
    } else {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        current
            .as_object_mut()
            .expect("object ensured")
            .insert(path.last().expect("path non-empty").clone(), value);
    }
}

pub(crate) fn validate_priority_value(priority: &Value) -> DatabaseResult<()> {
    match priority {
        Value::Null | Value::Number(_) | Value::String(_) => Ok(()),
        _ => Err(invalid_argument("Priority must be a string, number, or null")),
    }
}

pub(crate) fn pack_with_priority(value: Value, priority: Value) -> Value {
    let mut map = Map::with_capacity(2);
    map.insert(".value".to_string(), value);
    map.insert(".priority".to_string(), priority);
    Value::Object(map)
}

fn extract_data_owned(value: &Value) -> Value {
    extract_data_ref(value).clone()
}

fn value_at_path(root: &Value, path: &[String]) -> Value {
    if path.is_empty() {
        return extract_data_ref(root).clone();
    }
    get_value_at_path(root, path).unwrap_or(Value::Null)
}

fn children_map(value: &Value) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    match extract_data_ref(value) {
        Value::Object(obj) => {
            for (key, child) in obj.iter() {
                map.insert(key.clone(), child.clone());
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                map.insert(index.to_string(), child.clone());
            }
        }
        _ => {}
    }
    map
}

fn previous_key(keys: &[String], key: &str) -> Option<String> {
    let mut previous: Option<String> = None;
    for current in keys {
        if current == key {
            return previous;
        }
        previous = Some(current.clone());
    }
    None
}

fn get_value_at_path(root: &Value, segments: &[String]) -> Option<Value> {
    if segments.is_empty() {
        return Some(extract_data_ref(root).clone());
    }

    let mut current = root;
    for segment in segments {
        match current {
            Value::Object(map) => {
                if let Some(child) = map.get(segment) {
                    current = child;
                    continue;
                }

                if let Some(value_field) = map.get(".value") {
                    current = match value_field {
                        Value::Object(obj) => obj.get(segment)?,
                        Value::Array(items) => {
                            let index = segment.parse::<usize>().ok()?;
                            items.get(index)?
                        }
                        _ => return None,
                    };
                    continue;
                }

                return None;
            }
            Value::Array(array) => {
                let index = segment.parse::<usize>().ok()?;
                current = array.get(index)?;
            }
            _ => return None,
        }
    }

    Some(current.clone())
}

static DATABASE_COMPONENT: LazyLock<Component> = LazyLock::new(|| {
    Component::new(DATABASE_COMPONENT_NAME, Arc::new(database_factory), ComponentType::Public)
        .with_instantiation_mode(InstantiationMode::Lazy)
});

fn database_factory(
    container: &crate::component::ComponentContainer,
    _options: InstanceFactoryOptions,
) -> Result<DynService, ComponentError> {
    let app = container
        .root_service::<FirebaseApp>()
        .ok_or_else(|| ComponentError::InitializationFailed {
            name: DATABASE_COMPONENT_NAME.to_string(),
            reason: "Firebase app not attached to component container".to_string(),
        })?;

    let database = Database::new((*app).clone());
    Ok(Arc::new(database) as DynService)
}

fn ensure_registered() {
    let component = LazyLock::force(&DATABASE_COMPONENT).clone();
    let _ = app::register_component(component);
}

fn ensure_component_attached(app: &FirebaseApp) {
    let provider = app.container().get_provider(DATABASE_COMPONENT_NAME);
    if provider.is_component_set() {
        return;
    }
    let component = LazyLock::force(&DATABASE_COMPONENT).clone();
    app::add_component(app, &component);
}

pub fn register_database_component() {
    ensure_registered();
}

/// Routes `database` at the Realtime Database emulator, mirroring `connectDatabaseEmulator(db, host, port)`.
///
/// ```no_run
/// # use firebase_rs_sdk::database::{connect_database_emulator, get_database};
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let database = get_database(None).await?;
/// connect_database_emulator(&database, "127.0.0.1", 9000)?;
/// # Ok(())
/// # }
/// ```
pub fn connect_database_emulator(database: &Database, host: &str, port: u16) -> DatabaseResult<()> {
    database.connect_emulator(host, port)
}

pub async fn get_database(app: Option<FirebaseApp>) -> DatabaseResult<Arc<Database>> {
    ensure_registered();
    let app = match app {
        Some(app) => app,
        None => {
            #[cfg(all(feature = "wasm-web", target_arch = "wasm32"))]
            {
                return Err(internal_error(
                    "get_database(None) is not supported on wasm; pass a FirebaseApp",
                ));
            }
            #[cfg(not(all(feature = "wasm-web", target_arch = "wasm32")))]
            {
                crate::app::get_app(None)
                    .await
                    .map_err(|err| internal_error(err.to_string()))?
            }
        }
    };

    ensure_component_attached(&app);

    let provider = app::get_provider(&app, DATABASE_COMPONENT_NAME);
    if let Some(database) = provider.get_immediate::<Database>() {
        return Ok(database);
    }

    match provider.initialize::<Database>(Value::Null, None) {
        Ok(service) => Ok(service),
        Err(crate::component::types::ComponentError::InstanceUnavailable { .. }) => provider
            .get_immediate::<Database>()
            .ok_or_else(|| internal_error("Database component not available")),
        Err(err) => Err(internal_error(err.to_string())),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::app::initialize_app;
    use crate::app::{FirebaseApp, FirebaseAppConfig, FirebaseAppSettings, FirebaseOptions};
    use crate::component::ComponentContainer;
    use crate::database::{
        equal_to_with_key, increment, limit_to_first, limit_to_last, order_by_child, order_by_key,
        query as compose_query, server_timestamp, start_at,
    };
    use httpmock::prelude::*;
    use httpmock::Method::{DELETE, GET, PATCH, PUT};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    fn unique_settings() -> FirebaseAppSettings {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("database-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_database_attaches_component_for_untracked_app() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let config = FirebaseAppConfig::new("detached-db", false);
        let container = ComponentContainer::new(config.name.as_ref());
        let app = FirebaseApp::new(options, config, container);

        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("messages").unwrap();
        reference.set(json!({ "hello": "world" })).await.expect("in-memory set");
        let value = reference.get().await.expect("in-memory get");
        assert_eq!(value, json!({ "hello": "world" }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_and_get_value() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let ref_root = database.reference("/messages").unwrap();
        ref_root.set(json!({ "greeting": "hello" })).await.expect("set");
        let value = ref_root.get().await.unwrap();
        assert_eq!(value, json!({ "greeting": "hello" }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_generates_monotonic_keys() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let queue = database.reference("queue").unwrap();

        let mut keys = Vec::new();
        for _ in 0..5 {
            let key = queue.push().await.unwrap().key().unwrap().to_string();
            keys.push(key);
        }

        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_with_value_persists_data() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let messages = database.reference("messages").unwrap();

        let payload = json!({ "text": "hello" });
        let child = messages
            .push_with_value(payload.clone())
            .await
            .expect("push with value");

        let stored = child.get().await.unwrap();
        assert_eq!(stored, payload);

        let parent = messages.get().await.unwrap();
        let key = child.key().unwrap();
        assert_eq!(parent.get(key), Some(&payload));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn child_updates_merge() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let root = database.reference("items").unwrap();
        root.set(json!({ "a": { "count": 1 } })).await.unwrap();
        root.child("a/count").unwrap().set(json!(2)).await.unwrap();
        let value = root.get().await.unwrap();
        assert_eq!(value, json!({ "a": { "count": 2 } }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_with_priority_wraps_value() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let item = database.reference("items/main").unwrap();

        item.set_with_priority(json!({ "count": 1 }), json!(5)).await.unwrap();

        let stored = item.get().await.unwrap();
        assert_eq!(
            stored,
            json!({
                ".value": { "count": 1 },
                ".priority": 5
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_priority_updates_existing_value() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let item = database.reference("items/main").unwrap();

        item.set(json!({ "count": 4 })).await.unwrap();
        item.set_priority(json!(10)).await.unwrap();

        let stored = item.get().await.unwrap();
        assert_eq!(
            stored,
            json!({
                ".value": { "count": 4 },
                ".priority": 10
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_timestamp_is_resolved_on_set() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let created_at = database.reference("meta/created_at").unwrap();

        created_at.set(server_timestamp()).await.unwrap();

        let value = created_at.get().await.unwrap();
        let ts = value.as_u64().expect("timestamp as u64");
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(now >= ts);
        assert!(now - ts < 5_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_increment_updates_value() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let counter = database.reference("counters/main").unwrap();

        counter.set(json!(1)).await.unwrap();
        counter.set(increment(2.0)).await.unwrap();

        let value = counter.get().await.unwrap();
        assert_eq!(value.as_f64().unwrap(), 3.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_supports_server_increment() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let scores = database.reference("scores").unwrap();

        scores.set(json!({ "alice": 4 })).await.unwrap();
        let mut delta = serde_json::Map::new();
        delta.insert("alice".to_string(), increment(3.0));
        scores.update(delta).await.unwrap();

        let stored = scores.get().await.unwrap();
        assert_eq!(stored.get("alice").unwrap().as_f64().unwrap(), 7.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_rejects_empty_key() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("items").unwrap();

        let mut updates = serde_json::Map::new();
        updates.insert("".to_string(), json!(1));

        let err = reference.update(updates).await.unwrap_err();
        assert_eq!(err.code_str(), "database/invalid-argument");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_transaction_commits_update() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let counter = database.reference("counters/main").unwrap();
        counter.set(json!(1)).await.unwrap();

        let result = counter
            .run_transaction(|current| {
                let next = current.as_i64().unwrap_or(0) + 1;
                Some(json!(next))
            })
            .await
            .unwrap();

        assert!(result.committed);
        assert_eq!(result.snapshot.into_value(), json!(2));
        let stored = counter.get().await.unwrap();
        assert_eq!(stored, json!(2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_transaction_abort_preserves_value() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let flag = database.reference("flags/feature").unwrap();
        flag.set(json!(true)).await.unwrap();

        let result = flag
            .run_transaction(|current| {
                if current == Value::Bool(true) {
                    None
                } else {
                    Some(Value::Bool(true))
                }
            })
            .await
            .unwrap();

        assert!(!result.committed);
        assert_eq!(result.snapshot.into_value(), json!(true));
        let stored = flag.get().await.unwrap();
        assert_eq!(stored, json!(true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_performs_http_requests() {
        let server = MockServer::start();

        let set_mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/messages.json")
                .query_param("print", "silent")
                .json_body(json!({ "greeting": "hello" }));
            then.status(200).header("content-type", "application/json").body("null");
        });

        let get_mock = server.mock(|when, then| {
            when.method(GET).path("/messages.json").query_param("format", "export");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"greeting":"hello"}"#);
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("/messages").unwrap();

        reference
            .set(json!({ "greeting": "hello" }))
            .await
            .expect("set over REST");
        database.clear_mirror_for_test();
        let value = reference.get().await.expect("get over REST");

        assert_eq!(value, json!({ "greeting": "hello" }));
        set_mock.assert();
        get_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reference_parent_and_root() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();

        let nested = database.reference("users/alice/profile").unwrap();
        let parent = nested.parent().expect("parent reference");
        assert_eq!(parent.path(), "/users/alice/");
        assert_eq!(parent.parent().unwrap().path(), "/users/");

        let root = nested.root();
        assert_eq!(root.path(), "/");
        assert!(root.parent().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn realtime_set_updates_cache_and_listeners() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();

        let reference = database.reference("items/foo").unwrap();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let _registration = reference
            .on_value(move |result| {
                if let Ok(snapshot) = result {
                    events_clone.lock().unwrap().push(snapshot.value().clone());
                }
            })
            .await
            .unwrap();

        database
            .handle_realtime_action(
                "d",
                &json!({
                    "p": "items/foo",
                    "d": { "count": 5 }
                }),
            )
            .await
            .unwrap();

        let values = events.lock().unwrap();
        assert_eq!(values.last().unwrap(), &json!({ "count": 5 }));
        drop(values);

        assert_eq!(reference.get().await.unwrap(), json!({ "count": 5 }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn realtime_merge_updates_cache() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();

        // The mirror only tracks locations somebody listens to, so attach a listener first.
        let _registration = database.reference("items").unwrap().on_value(|_| {}).await.unwrap();

        database
            .handle_realtime_action(
                "d",
                &json!({
                    "p": "items",
                    "d": {"foo": {"count": 1}}
                }),
            )
            .await
            .unwrap();

        database
            .handle_realtime_action(
                "m",
                &json!({
                    "p": "items",
                    "d": {
                        "foo/count": 2,
                        "bar": {"value": true}
                    }
                }),
            )
            .await
            .unwrap();

        let foo = database.reference("items/foo").unwrap().get().await.unwrap();
        assert_eq!(foo, json!({"count": 2}));

        let bar = database.reference("items/bar").unwrap().get().await.unwrap();
        assert_eq!(bar, json!({"value": true}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn datasnapshot_child_and_metadata_helpers() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let profiles = database.reference("profiles").unwrap();

        profiles
            .set(json!({
                "alice": { "age": 31, "city": "Rome" },
                "bob": { "age": 29 }
            }))
            .await
            .unwrap();

        let captured = Arc::new(Mutex::new(None));
        let holder = captured.clone();
        profiles
            .on_value(move |result| {
                if let Ok(snapshot) = result {
                    *holder.lock().unwrap() = Some(snapshot);
                }
            })
            .await
            .unwrap();

        let snapshot = captured.lock().unwrap().clone().expect("initial snapshot");
        assert!(snapshot.exists());
        assert!(snapshot.has_children());
        assert_eq!(snapshot.size(), 2);

        let alice = snapshot.child("alice").unwrap();
        assert_eq!(alice.key(), Some("alice"));
        assert_eq!(alice.size(), 2);
        assert!(alice.has_children());
        assert_eq!(alice.child("age").unwrap().value(), &json!(31));
        assert!(snapshot.has_child("bob").unwrap());
        assert!(!snapshot.has_child("carol").unwrap());

        let json_output = snapshot.to_json();
        assert_eq!(
            json_output,
            json!({
                "alice": { "age": 31, "city": "Rome" },
                "bob": { "age": 29 }
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn child_event_listeners_receive_updates() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let items = database.reference("items").unwrap();

        items
            .set(json!({
                "a": { "count": 1 },
                "b": { "count": 2 }
            }))
            .await
            .unwrap();

        let added_events = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let capture = added_events.clone();
        let registration = items
            .on_child_added(move |result| {
                if let Ok(event) = result {
                    capture
                        .lock()
                        .unwrap()
                        .push((event.snapshot.key().unwrap().to_string(), event.previous_name.clone()));
                }
            })
            .await
            .unwrap();

        {
            let events = added_events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].0, "a");
            assert_eq!(events[1].0, "b");
        }

        items.child("c").unwrap().set(json!({ "count": 3 })).await.unwrap();

        {
            let events = added_events.lock().unwrap();
            assert_eq!(events.len(), 3);
            assert_eq!(events[2].0, "c");
        }

        registration.detach();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_set_with_priority_includes_metadata() {
        let server = MockServer::start();

        let put_mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/items.json")
                .query_param("print", "silent")
                .json_body(json!({
                    ".value": { "count": 1 },
                    ".priority": 3
                }));
            then.status(200).header("content-type", "application/json").body("null");
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("items").unwrap();

        reference
            .set_with_priority(json!({ "count": 1 }), json!(3))
            .await
            .unwrap();

        put_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_with_value_rest_backend_performs_put() {
        let server = MockServer::start();

        let push_mock = server.mock(|when, then| {
            when.method(PUT)
                .path_contains("/messages/")
                .query_param("print", "silent")
                .json_body(json!({ "text": "hello" }));
            then.status(200).header("content-type", "application/json").body("null");
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let messages = database.reference("messages").unwrap();

        let child = messages
            .push_with_value(json!({ "text": "hello" }))
            .await
            .expect("push with value rest");

        assert_eq!(child.key().unwrap().len(), 20);
        push_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_uses_patch_for_updates() {
        let server = MockServer::start();

        let patch_mock = server.mock(|when, then| {
            when.method(PATCH)
                .path("/items.json")
                .query_param("print", "silent")
                .json_body(json!({ "a/count": 2, "b": { "flag": true } }));
            then.status(200).header("content-type", "application/json").body("null");
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("items").unwrap();

        let mut updates = serde_json::Map::new();
        updates.insert("a/count".to_string(), json!(2));
        updates.insert("b".to_string(), json!({ "flag": true }));
        reference.update(updates).await.expect("patch update");

        patch_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_delete_supports_remove() {
        let server = MockServer::start();

        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path("/items.json").query_param("print", "silent");
            then.status(200).body("null");
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("items").unwrap();

        reference.remove().await.expect("delete request");
        delete_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_backend_preserves_namespace_query_parameter() {
        let server = MockServer::start();

        let set_mock = server.mock(|when, then| {
            when.method(PUT)
                .path("/messages.json")
                .query_param("ns", "demo-ns")
                .query_param("print", "silent")
                .json_body(json!({ "value": 1 }));
            then.status(200).body("null");
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(format!("{}?ns=demo-ns", server.url("/"))),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("messages").unwrap();

        reference.set(json!({ "value": 1 })).await.unwrap();
        set_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_query_order_by_child_and_limit() {
        let server = MockServer::start();

        let get_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/items.json")
                .query_param("orderBy", "\"score\"")
                .query_param("startAt", "100")
                .query_param("limitToFirst", "5")
                .query_param("format", "export");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"a":{"score":120}}"#);
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("items").unwrap();
        let filtered = compose_query(reference, vec![order_by_child("score"), start_at(100), limit_to_first(5)])
            .expect("compose query with constraints");

        let value = filtered.get().await.unwrap();
        assert_eq!(value, json!({ "a": { "score": 120 } }));
        get_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rest_query_equal_to_with_key() {
        let server = MockServer::start();

        let get_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/items.json")
                .query_param("orderBy", "\"$key\"")
                .query_param("startAt", "\"item-1\",\"item-1\"")
                .query_param("endAt", "\"item-1\",\"item-1\"")
                .query_param("format", "export");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"item-1":{"value":true}}"#);
        });

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            database_url: Some(server.url("/")),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let filtered = compose_query(
            database.reference("items").unwrap(),
            vec![order_by_key(), equal_to_with_key("item-1", "item-1")],
        )
        .expect("compose equal_to query");

        let value = filtered.get().await.unwrap();
        assert_eq!(value, json!({ "item-1": { "value": true } }));
        get_mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limit_to_first_rejects_zero() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();

        let err = database
            .reference("items")
            .unwrap()
            .query()
            .limit_to_first(0)
            .unwrap_err();

        assert_eq!(err.code_str(), "database/invalid-argument");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn order_by_child_rejects_empty_path() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();

        let err = database.reference("items").unwrap().order_by_child("").unwrap_err();

        assert_eq!(err.code_str(), "database/invalid-argument");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_rejects_multiple_order_by_constraints() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("items").unwrap();

        let err = compose_query(reference, vec![order_by_key(), order_by_child("score")]).unwrap_err();

        assert_eq!(err.code_str(), "database/invalid-argument");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn on_value_listener_receives_updates() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let reference = database.reference("counters/main").unwrap();

        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();

        let registration = reference
            .on_value(move |result| {
                if let Ok(snapshot) = result {
                    captured.lock().unwrap().push(snapshot.value().clone());
                }
            })
            .await
            .expect("on_value registration");

        reference.set(json!(1)).await.unwrap();
        reference.set(json!(2)).await.unwrap();

        {
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 3);
            assert_eq!(events[0], Value::Null);
            assert_eq!(events[1], json!(1));
            assert_eq!(events[2], json!(2));
        }

        registration.detach();
        reference.set(json!(3)).await.unwrap();

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_on_value_reacts_to_changes() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let scores = database.reference("scores").unwrap();

        scores
            .set(json!({
                "a": { "score": 10 },
                "b": { "score": 20 },
                "c": { "score": 30 }
            }))
            .await
            .unwrap();

        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();

        let _registration = compose_query(scores.clone(), vec![order_by_child("score"), limit_to_last(1)])
            .unwrap()
            .on_value(move |result| {
                if let Ok(snapshot) = result {
                    captured.lock().unwrap().push(snapshot.value().clone());
                }
            })
            .await
            .unwrap();

        {
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0],
                json!({
                    "a": { "score": 10 },
                    "b": { "score": 20 },
                    "c": { "score": 30 }
                })
            );
        }

        scores.child("d").unwrap().set(json!({ "score": 50 })).await.unwrap();

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            json!({
                "a": { "score": 10 },
                "b": { "score": 20 },
                "c": { "score": 30 },
                "d": { "score": 50 }
            })
        );
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_emulator_points_every_channel_at_the_emulator() {
        let options = FirebaseOptions {
            project_id: Some("demo-project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app.clone())).await.unwrap();

        database.connect_emulator("127.0.0.1", 9000).unwrap();

        assert_eq!(
            crate::database::backend::database_url_for(&app).as_deref(),
            Some("http://127.0.0.1:9000/?ns=demo-project-default-rtdb"),
            "the namespace defaults to <project>-default-rtdb, like the JS SDK"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_emulator_is_refused_once_listeners_exist() {
        let options = FirebaseOptions {
            project_id: Some("demo-project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let database = get_database(Some(app)).await.unwrap();
        let _registration = database.reference("items").unwrap().on_value(|_| {}).await.unwrap();

        let err = database
            .connect_emulator("127.0.0.1", 9000)
            .expect_err("re-pointing a database in use must fail");
        assert_eq!(err.code, crate::database::error::DatabaseErrorCode::InvalidArgument);
    }
}
