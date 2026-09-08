use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::app::component::{self, Component, Provider};
use crate::app::heartbeat::HeartbeatServiceImpl;
use crate::app::logger::LOGGER;
use crate::app::types::{FirebaseApp, FirebaseServerApp, HeartbeatService};
use crate::component::constants::DEFAULT_ENTRY_NAME;
use crate::component::types::{ComponentError, InstanceFactoryOptions};
use crate::component::{ComponentContainer, Service, ServiceProvider};
use crate::platform::runtime;

pub static APPS: LazyLock<Mutex<HashMap<String, FirebaseApp>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub static SERVER_APPS: LazyLock<Mutex<HashMap<String, FirebaseServerApp>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Serialises tests that reset the global app/component registries. Every test module that
/// calls a `reset()` on these globals must lock this, otherwise modules race each other.
#[cfg(test)]
pub(crate) static TEST_REGISTRY_SERIAL: LazyLock<async_lock::Mutex<()>> = LazyLock::new(|| async_lock::Mutex::new(()));

pub(crate) fn apps_guard() -> MutexGuard<'static, HashMap<String, FirebaseApp>> {
    APPS.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn server_apps_guard() -> MutexGuard<'static, HashMap<String, FirebaseServerApp>> {
    SERVER_APPS.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn registered_components_guard() -> MutexGuard<'static, HashMap<Arc<str>, Component>> {
    component::global_components()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Attaches a component to the given app, logging failures for debugging.
/// Mirrors the JS `_addComponent` helper.
pub fn add_component(app: &FirebaseApp, component: &Component) {
    if app.container().add_component(component.clone()).is_err() {
        LOGGER.debug(format!(
            "Component {} failed to register with FirebaseApp {}",
            component.name(),
            app.name()
        ));
    }
}

/// Replaces any existing component with the same name on the given app.
/// Mirrors the JS `_addOrOverwriteComponent` helper.
#[allow(dead_code)]
pub fn add_or_overwrite_component(app: &FirebaseApp, component: Component) {
    app.container().add_or_overwrite_component(component);
}

/// Clears globally registered components
/// Mirrors the JS `_clearComponents` helper.
#[allow(dead_code)]
pub fn clear_components() {
    registered_components_guard().clear();
}

/// Registers a global component and propagates it to already-initialized apps.
///
/// The global component map is held for the whole operation, and the app map is taken inside
/// it. `initialize_app` acquires the same two locks in the same order to snapshot the
/// components and publish the new app atomically, so a component can never be missed by an app
/// that is being created concurrently. Keep that lock order (components, then apps) everywhere.
pub fn register_component(component: Component) -> bool {
    let mut global = registered_components_guard();
    let newly_registered = if global.contains_key(component.name()) {
        false
    } else {
        global.insert(Arc::from(component.name().to_owned()), component.clone());
        true
    };
    // Reuse the stored version so an already-registered component is still propagated to any
    // apps that may have been initialized without it.
    let component = global.get(component.name()).cloned().unwrap_or(component);

    {
        let apps = apps_guard();
        for app in apps.values() {
            add_component(app, &component);
        }
    }

    {
        let server_apps = server_apps_guard();
        for server_app in server_apps.values() {
            add_component(server_app.base(), &component);
        }
    }

    drop(global);
    newly_registered
}

/// Registers a product's service, so every app — existing and future — can resolve it.
///
/// The name, instantiation mode and multiplicity come from the [`Service`] impl, and the factory
/// returns the service's own type, so a registration cannot disagree with the lookups.
pub fn register_service<S, F>(factory: F) -> bool
where
    S: Service,
    F: Fn(&ComponentContainer, InstanceFactoryOptions) -> Result<Arc<S>, ComponentError> + Send + Sync + 'static,
{
    register_component(Component::for_service::<S, _>(factory))
}

/// Attaches the registered component for `S` to one app.
///
/// Global registration reaches every app the registry knows about, but an app built directly (as
/// tests do) or created before the product was first used has to be given the component; every
/// product's accessor does this before resolving. Returns false when nothing is registered for
/// `S` yet.
pub fn attach_service<S: Service>(app: &FirebaseApp) -> bool {
    let component = registered_components_guard().get(S::NAME).cloned();
    match component {
        Some(component) => {
            add_component(app, &component);
            true
        }
        None => false,
    }
}

/// The app's handle on a service, triggering the heartbeat the way `_getProvider` does in the JS
/// SDK.
pub fn service_provider<S: Service>(app: &FirebaseApp) -> ServiceProvider<S> {
    ServiceProvider::new(get_provider(app, S::NAME))
}

/// The service itself, when this app has it.
pub fn service<S: Service>(app: &FirebaseApp) -> Option<Arc<S>> {
    service_provider::<S>(app).get()
}

/// Fetches the provider for the named component, triggering heartbeat side-effects.
/// Mirrors the JS `_getProvider` helper.
pub fn get_provider(app: &FirebaseApp, name: &str) -> Provider {
    let container = app.container();
    if let Some(service) = container
        .get_provider("heartbeat")
        .get_immediate::<HeartbeatServiceImpl>()
    {
        let app_name = app.name().to_string();
        let service_clone = service.clone();
        runtime::spawn_detached(async move {
            if let Err(err) = service_clone.trigger_heartbeat().await {
                LOGGER.debug(format!("Failed to trigger heartbeat for app {}: {}", app_name, err));
            }
        });
    }
    container.get_provider(name)
}

/// Removes a cached service instance from the given app by provider name.
/// Mirrors the JS `_removeServiceInstance` helper.
#[allow(dead_code)]
pub fn remove_service_instance(app: &FirebaseApp, name: &str, instance_identifier: Option<&str>) {
    let instance_identifier = instance_identifier.unwrap_or(DEFAULT_ENTRY_NAME);
    get_provider(app, name).clear_instance(instance_identifier);
}

/// Returns true when the supplied app corresponds to a server-side Firebase app instance.
#[allow(dead_code)]
pub fn is_firebase_server_app(app: &FirebaseApp) -> bool {
    server_apps_guard().contains_key(app.name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::api;
    use crate::app::heartbeat::clear_heartbeat_store_for_tests;
    use crate::app::types::{FirebaseAppSettings, FirebaseOptions, FirebaseServerAppSettings};
    use crate::component::types::{ComponentType, InstantiationMode};
    use crate::component::Component;
    use crate::platform::runtime;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn reset() {
        {
            let mut apps = apps_guard();
            for app in apps.values() {
                app.set_is_deleted(true);
            }
            apps.clear();
        }
        server_apps_guard().clear();
        registered_components_guard().clear();
        clear_heartbeat_store_for_tests();
        crate::component::global_components()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    async fn with_serialized_test<F, Fut>(f: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future,
    {
        let _guard = TEST_REGISTRY_SERIAL.lock().await;
        reset();
        f().await
    }

    fn test_options() -> FirebaseOptions {
        FirebaseOptions {
            api_key: Some("internal-test-key".into()),
            app_id: Some("1:987:web:test".into()),
            project_id: Some("internal-test".into()),
            ..Default::default()
        }
    }

    fn make_component<S, F>(name: &str, factory: F) -> Component
    where
        S: std::any::Any + Send + Sync + 'static,
        F: Fn(
                &crate::component::ComponentContainer,
                crate::component::InstanceFactoryOptions,
            ) -> Result<Arc<S>, crate::component::ComponentError>
            + Send
            + Sync
            + 'static,
    {
        Component::typed::<S, _>(name.to_string(), factory, ComponentType::Public)
            .with_instantiation_mode(InstantiationMode::Lazy)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_component_attaches_to_app() {
        with_serialized_test(|| async {
            let app = api::initialize_app(test_options(), None).await.expect("app init");
            let c = make_component("internal-comp", |_, _| Ok(Arc::new(())));
            add_component(&app, &c);

            assert!(app.container().get_provider("internal-comp").is_component_set());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_or_overwrite_component_replaces_existing_instance() {
        with_serialized_test(|| async {
            let app = api::initialize_app(test_options(), None).await.expect("app init");

            let counter = Arc::new(AtomicUsize::new(0));
            let base_counter = counter.clone();
            add_component(
                &app,
                &make_component("overwrite", move |_, _| {
                    Ok(Arc::new(base_counter.fetch_add(1, Ordering::SeqCst) + 1))
                }),
            );

            let first_provider = app.container().get_provider("overwrite");
            let first = first_provider
                .get_immediate::<usize>()
                .expect("first instance")
                .as_ref()
                .clone();
            assert_eq!(first, 1);

            let counter_two = counter.clone();
            counter_two.store(40, Ordering::SeqCst);
            add_or_overwrite_component(
                &app,
                make_component("overwrite", move |_, _| {
                    Ok(Arc::new(counter_two.fetch_add(1, Ordering::SeqCst) + 1))
                }),
            );

            remove_service_instance(&app, "overwrite", None);
            let provider_after = app.container().get_provider("overwrite");
            let second = provider_after
                .get_immediate::<usize>()
                .expect("second instance")
                .as_ref()
                .clone();
            assert!(second > first);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_components_drops_registry_entries() {
        with_serialized_test(|| async {
            let app = api::initialize_app(test_options(), None).await.expect("app init");
            register_component(make_component("clearable", |_, _| Ok(Arc::new(()))));

            // The registry is global: clearing it and putting it back happens under one guard so
            // no other test can observe the gap. Holding the lock means calling `clear_components`
            // itself would deadlock, so this runs the operation it performs.
            {
                let mut global = registered_components_guard();
                assert!(global.keys().any(|name| name.as_ref() == "clearable"));

                let saved: Vec<Component> = global.values().cloned().collect();
                global.clear();
                assert!(global.is_empty(), "clearing drops every registered component");

                for component in saved {
                    if component.name() != "clearable" {
                        global.insert(Arc::from(component.name().to_owned()), component);
                    }
                }
            }

            // Apps that already have the component keep it; clearing only affects new apps.
            assert!(app.container().get_provider("clearable").is_component_set());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_component_propagates_to_existing_apps() {
        with_serialized_test(|| async {
            let app = api::initialize_app(test_options(), None).await.expect("app init");
            register_component(make_component("late", |_, _| Ok(Arc::new("shared"))));

            let provider = app.container().get_provider("late");
            assert!(provider.is_component_set());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_component_attaches_when_already_registered() {
        with_serialized_test(|| async {
            let app = api::initialize_app(test_options(), None).await.expect("app init");
            let component = make_component("late", |_, _| Ok(Arc::new("shared")));

            // Simulate a pre-registered component that was not propagated to this app yet.
            assert!(component::register_component(component.clone()));
            assert!(!app.container().get_provider("late").is_component_set());

            let newly_registered = register_component(component);
            assert!(!newly_registered);

            let provider = app.container().get_provider("late");
            assert!(provider.is_component_set());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_provider_and_remove_service_instance_reset_cached_instance() {
        with_serialized_test(|| async {
            let app = api::initialize_app(test_options(), None).await.expect("app init");
            let counter = Arc::new(AtomicUsize::new(0));
            let counter_clone = counter.clone();
            add_component(
                &app,
                &make_component("provider", move |_, _| {
                    Ok(Arc::new(counter_clone.fetch_add(1, Ordering::SeqCst) + 1))
                }),
            );

            let provider = get_provider(&app, "provider");
            let first = provider.get_immediate::<usize>().expect("first").as_ref().clone();
            assert_eq!(first, 1);

            remove_service_instance(&app, "provider", None);
            let second = provider.get_immediate::<usize>().expect("second").as_ref().clone();
            assert_eq!(second, 2);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn is_firebase_server_app_detects_server_instances() {
        with_serialized_test(|| async {
            let server_settings = FirebaseServerAppSettings {
                automatic_data_collection_enabled: None,
                auth_id_token: None,
                app_check_token: None,
                release_on_deref: Some(true),
            };
            let server_app = api::initialize_server_app(Some(test_options()), Some(server_settings))
                .await
                .expect("server app");
            assert!(is_firebase_server_app(server_app.base()));

            drop(server_app);
            runtime::sleep(Duration::from_millis(25)).await;

            let app = api::initialize_app(
                test_options(),
                Some(FirebaseAppSettings {
                    name: Some("regular".into()),
                    automatic_data_collection_enabled: None,
                }),
            )
            .await
            .expect("regular app");
            assert!(!is_firebase_server_app(&app));
        })
        .await;
    }
}
