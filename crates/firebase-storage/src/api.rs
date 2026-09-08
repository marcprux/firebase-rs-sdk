use std::sync::Arc;

use crate::constants::STORAGE_TYPE;
use crate::error::{internal_error, StorageResult};
use crate::reference::StorageReference;
use crate::service::FirebaseStorageImpl;
use crate::util::is_url;
use firebase_core::app::FirebaseApp;
use firebase_core::app::{get_app, SDK_VERSION};
use firebase_core::component::types::{ComponentError, InstanceFactoryOptions};
use firebase_core::component::{ComponentContainer, Service};

/// One instance per bucket: `getStorage(app, "gs://other-bucket")` is a second service on the
/// same app, keyed by the bucket URL.
impl Service for FirebaseStorageImpl {
    const NAME: &'static str = STORAGE_TYPE;
    const MULTIPLE_INSTANCES: bool = true;
}

fn storage_factory(
    container: &ComponentContainer,
    options: InstanceFactoryOptions,
) -> Result<Arc<FirebaseStorageImpl>, ComponentError> {
    let app = container.app().ok_or_else(|| ComponentError::InitializationFailed {
        name: STORAGE_TYPE.to_string(),
        reason: "Firebase app not attached to component container".to_string(),
    })?;

    let storage = FirebaseStorageImpl::new(
        (*app).clone(),
        options.instance_identifier.clone(),
        Some(SDK_VERSION.to_string()),
    )
    .map_err(|err| ComponentError::InitializationFailed {
        name: STORAGE_TYPE.to_string(),
        reason: err.to_string(),
    })?;

    Ok(Arc::new(storage))
}

fn ensure_registered() {
    firebase_core::app::register_service::<FirebaseStorageImpl, _>(storage_factory);
}

pub fn register_storage_component() {
    ensure_registered();
}

pub async fn get_storage_for_app(
    app: Option<FirebaseApp>,
    bucket_url: Option<&str>,
) -> StorageResult<Arc<FirebaseStorageImpl>> {
    ensure_registered();
    let app = match app {
        Some(app) => app,
        None => get_app(None).await.map_err(|err| internal_error(err.to_string()))?,
    };

    let storage = firebase_core::app::service_provider::<FirebaseStorageImpl>(&app)
        .try_get(bucket_url)
        .map_err(|err| internal_error(err.to_string()))?
        .ok_or_else(|| internal_error("Storage component did not return an instance"))?;

    Ok(storage)
}

pub fn storage_ref_from_storage(
    storage: &FirebaseStorageImpl,
    path_or_url: Option<&str>,
) -> StorageResult<StorageReference> {
    storage.reference_from_path(path_or_url)
}

pub fn storage_ref_from_reference(reference: &StorageReference, path: Option<&str>) -> StorageResult<StorageReference> {
    match path {
        Some(segment) if is_url(segment) => {
            // Mirrors JS behaviour: URLs must be paired with a Storage instance, not a reference.
            Err(internal_error("Use storage_ref_from_storage for URL-based references"))
        }
        Some(segment) => Ok(reference.child(segment)),
        None => Ok(reference.clone()),
    }
}

pub fn connect_storage_emulator(
    storage: &FirebaseStorageImpl,
    host: &str,
    port: u16,
    mock_user_token: Option<String>,
) -> StorageResult<()> {
    storage.connect_emulator(host, port, mock_user_token)
}

pub fn delete_storage_instance(storage: &FirebaseStorageImpl) {
    let bucket = storage.bucket();
    if bucket.is_some() {
        // Components currently lack explicit cleanup hooks; placeholder for parity.
    }
}
