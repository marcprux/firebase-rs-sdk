use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::component::provider::Provider;
use crate::component::types::{ComponentError, DynService};
use crate::component::Component;

#[derive(Clone)]
pub struct ComponentContainer {
    pub(crate) inner: Arc<ComponentContainerInner>,
}

pub(crate) struct ComponentContainerInner {
    pub name: Arc<str>,
    pub providers: Mutex<HashMap<Arc<str>, Provider>>, // Provider holds Arc to inner state
    pub root_service: Mutex<Option<DynService>>,
}

impl ComponentContainer {
    pub fn new(name: impl Into<String>) -> Self {
        let name: Arc<str> = Arc::from(name.into());
        Self {
            inner: Arc::new(ComponentContainerInner {
                name,
                providers: Mutex::new(HashMap::new()),
                root_service: Mutex::new(None),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn add_component(&self, component: Component) -> Result<(), ComponentError> {
        let provider = self.get_provider(component.name());
        provider.set_component(component)
    }

    /// Replaces whatever provider the container has for this component, dropping its instances.
    ///
    /// The old provider is removed and the new one installed under a single lock so a concurrent
    /// [`get_provider`](Self::get_provider) cannot hand out the provider that is on its way out.
    pub fn add_or_overwrite_component(&self, component: Component) {
        let provider = {
            let mut providers = self.inner.providers.lock().unwrap();
            providers.remove(component.name());
            let provider = Provider::new(component.name(), self.clone());
            providers.insert(Arc::from(component.name().to_owned()), provider.clone());
            provider
        };
        let _ = provider.set_component(component);
    }

    /// Returns the container's provider for `name`, creating it the first time it is asked for.
    ///
    /// The lookup and the insert happen under one lock: dropping it in between let two threads
    /// each create a provider, and the second one would overwrite the first — discarding the
    /// component that had just been set on it, so every later lookup found a provider with no
    /// component and reported the service as unavailable.
    pub fn get_provider(&self, name: &str) -> Provider {
        let mut providers = self.inner.providers.lock().unwrap();
        if let Some(provider) = providers.get(name) {
            return provider.clone();
        }

        let provider = Provider::new(name, self.clone());
        providers.insert(Arc::from(name.to_owned()), provider.clone());
        provider
    }

    pub fn get_providers(&self) -> Vec<Provider> {
        self.inner.providers.lock().unwrap().values().cloned().collect()
    }

    pub fn attach_root_service(&self, service: DynService) {
        *self.inner.root_service.lock().unwrap() = Some(service);
    }

    pub fn root_service<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        self.inner
            .root_service
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|svc| Arc::clone(svc).downcast::<T>().ok())
    }
}
