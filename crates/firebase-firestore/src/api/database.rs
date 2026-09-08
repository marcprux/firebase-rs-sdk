use std::sync::Arc;

use crate::constants::FIRESTORE_COMPONENT_NAME;
use crate::error::{internal_error, invalid_argument, missing_project_id, FirestoreResult};
use crate::model::{DatabaseId, ResourcePath};
use firebase_core::app;
use firebase_core::app::FirebaseApp;
use firebase_core::app::SDK_VERSION;
use firebase_core::app::{get_app, register_version};
use firebase_core::component::types::{ComponentError, InstanceFactoryOptions};
use firebase_core::component::{ComponentContainer, Service};

use super::query::Query;
use super::reference::{CollectionReference, DocumentReference};

#[derive(Clone, Debug)]
pub struct Firestore {
    inner: Arc<FirestoreInner>,
}

#[derive(Debug)]
struct FirestoreInner {
    app: FirebaseApp,
    database_id: DatabaseId,
}

impl Firestore {
    pub(crate) fn new(app: FirebaseApp, database_id: DatabaseId) -> Self {
        let inner = FirestoreInner { app, database_id };
        Self { inner: Arc::new(inner) }
    }

    /// Returns the `FirebaseApp` this Firestore instance is scoped to.
    pub fn app(&self) -> &FirebaseApp {
        &self.inner.app
    }

    /// The fully qualified database identifier (project + database name).
    pub fn database_id(&self) -> &DatabaseId {
        &self.inner.database_id
    }

    /// Creates a `CollectionReference` pointing at `path`.
    ///
    /// The path is interpreted relative to the Firestore root using forward
    /// slashes to separate segments (e.g. `"users/alovelace/repos"`).
    pub fn collection(&self, path: &str) -> FirestoreResult<CollectionReference> {
        let resource = ResourcePath::from_string(path)?;
        CollectionReference::new(self.clone(), resource)
    }

    /// Creates a `DocumentReference` pointing at `path`.
    ///
    /// The path must contain an even number of segments (collection/doc pairs).
    pub fn doc(&self, path: &str) -> FirestoreResult<DocumentReference> {
        let resource = ResourcePath::from_string(path)?;
        DocumentReference::new(self.clone(), resource)
    }

    /// Creates a query that targets every collection with the provided identifier, regardless of its parent path.
    ///
    /// Mirrors the modular JS `collectionGroup` API from
    /// `packages/firestore/src/lite-api/reference.ts`.
    pub fn collection_group(&self, collection_id: &str) -> FirestoreResult<Query> {
        Query::new_collection_group(self.clone(), collection_id.to_string())
    }

    /// Clones a Firestore handle that has been wrapped in an `Arc`.
    pub fn from_arc(arc: Arc<Self>) -> Self {
        arc.as_ref().clone()
    }

    /// Returns the project identifier backing this database.
    pub fn project_id(&self) -> &str {
        self.inner.database_id.project_id()
    }

    /// Returns the logical database name (usually `"(default)"`).
    pub fn database(&self) -> &str {
        self.inner.database_id.database()
    }
}

/// One instance per database: an app can address several Firestore databases, keyed by database
/// id.
impl Service for Firestore {
    const NAME: &'static str = FIRESTORE_COMPONENT_NAME;
    const MULTIPLE_INSTANCES: bool = true;
}

fn firestore_factory(
    container: &ComponentContainer,
    options: InstanceFactoryOptions,
) -> Result<Arc<Firestore>, ComponentError> {
    let app = container.app().ok_or_else(|| ComponentError::InitializationFailed {
        name: FIRESTORE_COMPONENT_NAME.to_string(),
        reason: "Firebase app not attached to component container".to_string(),
    })?;

    let database_id = match options.instance_identifier.as_deref() {
        Some(identifier) if !identifier.is_empty() => {
            parse_database_identifier(&app, identifier).map_err(|err| ComponentError::InitializationFailed {
                name: FIRESTORE_COMPONENT_NAME.to_string(),
                reason: err.to_string(),
            })?
        }
        _ => DatabaseId::from_app(&app).map_err(|err| ComponentError::InitializationFailed {
            name: FIRESTORE_COMPONENT_NAME.to_string(),
            reason: err.to_string(),
        })?,
    };

    let firestore = Firestore::new((*app).clone(), database_id);

    register_version("@firebase/firestore", SDK_VERSION, None);

    Ok(Arc::new(firestore))
}

fn parse_database_identifier(app: &FirebaseApp, identifier: &str) -> FirestoreResult<DatabaseId> {
    let options = app.options();
    let project_id = options.project_id.clone().ok_or_else(missing_project_id)?;

    if identifier.starts_with("projects/") {
        let segments: Vec<_> = identifier.split('/').collect();
        if segments.len() == 4 && segments[0] == "projects" && segments[2] == "databases" {
            return Ok(DatabaseId::new(segments[1], segments[3]));
        }
        return Err(invalid_argument(
            "Database identifier must follow projects/{project}/databases/{database}",
        ));
    }

    Ok(DatabaseId::new(project_id, identifier))
}

fn ensure_registered() {
    app::register_service::<Firestore, _>(firestore_factory);
}

/// Guarantees `app` can resolve Firestore.
///
/// Registration is global and propagates to the apps the registry knows about, which leaves out
/// apps built directly (as tests do) or removed from the registry, so the component is attached to
/// this app's container as well when it is missing.
fn ensure_registered_for(app: &FirebaseApp) {
    ensure_registered();
    if !app.container().service::<Firestore>().is_registered() {
        app::attach_service::<Firestore>(app);
    }
}

pub fn register_firestore_component() {
    ensure_registered();
}

/// Resolves (or lazily instantiates) the Firestore service for the provided app.
///
/// When `app` is `None` the default Firebase app is used. Multiple calls with
/// the same app yield the same shared `Arc<Firestore>` handle.
pub async fn get_firestore(app: Option<FirebaseApp>) -> FirestoreResult<Arc<Firestore>> {
    ensure_registered();
    let app = match app {
        Some(app) => app,
        None => get_app(None).await.map_err(|err| internal_error(err.to_string()))?,
    };

    ensure_registered_for(&app);
    app::service_provider::<Firestore>(&app)
        .try_get(None)
        .map_err(|err| internal_error(err.to_string()))?
        .ok_or_else(|| internal_error("Failed to obtain Firestore instance"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use firebase_core::app::initialize_app;
    use firebase_core::app::{FirebaseAppSettings, FirebaseOptions};

    fn unique_settings() -> FirebaseAppSettings {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("firestore-api-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_app_outside_the_registry_still_resolves_firestore() {
        // Apps built directly never enter the global app map, so a component registered later
        // cannot be propagated to them. The accessor attaches it to the container instead.
        use firebase_core::app::{FirebaseAppConfig, FirebaseOptions};
        use firebase_core::component::ComponentContainer;

        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let config = FirebaseAppConfig::new("unregistered-app", false);
        let container = ComponentContainer::new("unregistered-app");
        let app = FirebaseApp::new(options, config, container);

        let firestore = get_firestore(Some(app)).await.expect("firestore for a detached app");
        assert_eq!(firestore.project_id(), "project");
    }

    #[tokio::test]
    async fn get_firestore_registers_component() {
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let firestore = get_firestore(Some(app)).await.unwrap();
        assert_eq!(firestore.project_id(), "project");
        assert_eq!(firestore.database(), "(default)");
    }

    #[tokio::test]
    async fn custom_database_identifier() {
        register_firestore_component();
        let options = FirebaseOptions {
            project_id: Some("project".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let instance = app::service_provider::<Firestore>(&app)
            .initialize(serde_json::Value::Null, Some("projects/project/databases/custom"))
            .unwrap();
        assert_eq!(instance.database(), "custom");
    }
}
