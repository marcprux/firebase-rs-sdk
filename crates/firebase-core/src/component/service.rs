//! The typed face of the component container.
//!
//! The container itself stores services as `Arc<dyn Any>`, because a Firebase app holds services
//! of every product at once and looks them up by the name the JS SDK uses. What that cost, until
//! this layer existed, was the type: registration and lookup each named a Rust type independently,
//! a mismatch between them produced `None` rather than an error, and every call site spelled a
//! magic string.
//!
//! A product now declares its service once, by implementing [`Service`]:
//!
//! ```
//! use firebase_core::component::{ComponentContainer, InstanceFactoryOptions, Service};
//! use firebase_core::component::types::ComponentError;
//! use std::sync::Arc;
//!
//! struct Widgets;
//!
//! impl Service for Widgets {
//!     const NAME: &'static str = "widgets";
//! }
//!
//! fn widgets_factory(_: &ComponentContainer, _: InstanceFactoryOptions) -> Result<Arc<Widgets>, ComponentError> {
//!     Ok(Arc::new(Widgets))
//! }
//!
//! # fn main() {
//! firebase_core::app::register_service::<Widgets, _>(widgets_factory);
//! # }
//! ```
//!
//! and reads it back with [`ComponentContainer::service`] or
//! [`firebase_core::app::service_provider`](crate::app::service_provider), where the name and the
//! type both come from the `Service` impl and cannot drift apart.

use std::any::TypeId;
use std::marker::PhantomData;
use std::sync::Arc;

use serde_json::Value;

use crate::component::container::ComponentContainer;
use crate::component::provider::Provider;
use crate::component::types::{ComponentError, ComponentType, InstantiationMode};

/// A service a product publishes into a Firebase app.
///
/// The constants are the component's registration: its name (the same one the JS SDK uses, since
/// apps are shared between SDKs through it), when it is created, and whether one app can hold
/// several instances of it — a Storage bucket or a Firestore database, say.
pub trait Service: Send + Sync + 'static {
    /// The component name. Unique across the SDK: two services cannot share one.
    const NAME: &'static str;

    /// When the container creates the service. [`InstantiationMode::Lazy`] — on first use — suits
    /// almost everything; App Check is [`InstantiationMode::Explicit`] because it exists only once
    /// the application has configured a provider for it.
    const INSTANTIATION_MODE: InstantiationMode = InstantiationMode::Lazy;

    /// Whether one app can hold several instances, keyed by an identifier.
    const MULTIPLE_INSTANCES: bool = false;

    /// Public services are the ones an application resolves; private ones are wiring between
    /// products (the credential sources), and the platform logger reports the version ones.
    const COMPONENT_TYPE: ComponentType = ComponentType::Public;
}

/// A container's handle on one service.
///
/// Created by [`ComponentContainer::service`] or
/// [`firebase_core::app::service_provider`](crate::app::service_provider). Holding one does not
/// create the service: the lookup happens per call, so a provider taken before the application
/// signed in, or before App Check was initialised, still sees the service once it exists.
pub struct ServiceProvider<S: Service> {
    provider: Provider,
    marker: PhantomData<fn() -> Arc<S>>,
}

impl<S: Service> Clone for ServiceProvider<S> {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            marker: PhantomData,
        }
    }
}

impl<S: Service> std::fmt::Debug for ServiceProvider<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceProvider").field("name", &S::NAME).finish()
    }
}

impl<S: Service> ServiceProvider<S> {
    pub(crate) fn new(provider: Provider) -> Self {
        Self {
            provider,
            marker: PhantomData,
        }
    }

    /// The service, created if the app has the component and its mode allows it, and `None` when
    /// the product was never registered (its feature is off) or the service is explicit and has
    /// not been initialised yet.
    pub fn get(&self) -> Option<Arc<S>> {
        self.get_instance(None)
    }

    /// One named instance of a multi-instance service.
    pub fn get_instance(&self, identifier: Option<&str>) -> Option<Arc<S>> {
        match self.try_get(identifier) {
            Ok(service) => service,
            Err(err) => {
                // A type mismatch means two crates disagree about what `S::NAME` holds, which no
                // caller can recover from; saying so beats handing back `None`.
                crate::app::LOGGER.error(err.to_string());
                None
            }
        }
    }

    /// Like [`get_instance`](Self::get_instance), but reports why the service is unavailable.
    pub fn try_get(&self, identifier: Option<&str>) -> Result<Option<Arc<S>>, ComponentError> {
        self.check_type()?;
        self.provider.get_immediate_with_options::<S>(identifier, true)
    }

    /// Creates the service explicitly, with options the factory reads.
    ///
    /// This is how a service whose mode is [`InstantiationMode::Explicit`] comes into being, and
    /// how a multi-instance service is created for one identifier.
    pub fn initialize(&self, options: Value, identifier: Option<&str>) -> Result<Arc<S>, ComponentError> {
        self.check_type()?;
        self.provider.initialize::<S>(options, identifier)
    }

    /// Whether the app has this service already.
    pub fn is_initialized(&self, identifier: Option<&str>) -> bool {
        self.provider.is_initialized(identifier)
    }

    /// Whether the product registered its component with this app at all.
    pub fn is_registered(&self) -> bool {
        self.provider.is_component_set()
    }

    /// The options an instance was created with.
    pub fn options(&self, identifier: Option<&str>) -> Value {
        self.provider.get_options(identifier)
    }

    /// Drops one instance, so the next lookup builds it again.
    pub fn clear_instance(&self, identifier: &str) {
        self.provider.clear_instance(identifier);
    }

    /// Guards against `S::NAME` being registered as some other type.
    fn check_type(&self) -> Result<(), ComponentError> {
        match self.provider.service_type() {
            Some((type_id, type_name)) if type_id != TypeId::of::<S>() => Err(ComponentError::MismatchingServiceType {
                name: S::NAME.to_string(),
                expected: std::any::type_name::<S>().to_string(),
                found: type_name.to_string(),
            }),
            _ => Ok(()),
        }
    }
}

impl ComponentContainer {
    /// This container's handle on a service.
    pub fn service<S: Service>(&self) -> ServiceProvider<S> {
        ServiceProvider::new(self.get_provider(S::NAME))
    }

    /// The service itself, when the app has it.
    pub fn get<S: Service>(&self) -> Option<Arc<S>> {
        self.service::<S>().get()
    }
}
