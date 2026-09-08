use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::component::service::Service;
use crate::component::types::{
    ComponentError, ComponentType, DynService, InstanceFactory, InstanceFactoryOptions, InstantiationMode,
    OnInstanceCreatedCallback,
};
use crate::component::ComponentContainer;

/// One product's registration with the container: a name, how to build the service, and — since
/// the container stores services erased — which Rust type that name holds.
#[derive(Clone)]
pub struct Component {
    name: Arc<str>,
    pub(crate) instance_factory: InstanceFactory,
    pub(crate) service_type: TypeId,
    pub(crate) service_type_name: &'static str,
    pub(crate) ty: ComponentType,
    pub(crate) instantiation_mode: InstantiationMode,
    pub(crate) multiple_instances: bool,
    pub(crate) service_props: Map<String, Value>,
    pub(crate) on_instance_created: Option<OnInstanceCreatedCallback>,
}

impl Component {
    /// The component for a [`Service`], taking its name, mode and multiplicity from the trait.
    ///
    /// This is how a product registers: see [`crate::app::register_service`].
    pub fn for_service<S, F>(factory: F) -> Self
    where
        S: Service,
        F: Fn(&ComponentContainer, InstanceFactoryOptions) -> Result<Arc<S>, ComponentError> + Send + Sync + 'static,
    {
        Self::typed::<S, F>(S::NAME, factory, S::COMPONENT_TYPE)
            .with_instantiation_mode(S::INSTANTIATION_MODE)
            .with_multiple_instances(S::MULTIPLE_INSTANCES)
    }

    /// A component whose name is not known until runtime — the per-library version services are
    /// the only ones — but whose service type still is.
    pub fn typed<S, F>(name: impl Into<String>, factory: F, ty: ComponentType) -> Self
    where
        S: Any + Send + Sync + 'static,
        F: Fn(&ComponentContainer, InstanceFactoryOptions) -> Result<Arc<S>, ComponentError> + Send + Sync + 'static,
    {
        let erased: InstanceFactory =
            Arc::new(move |container, options| factory(container, options).map(|service| service as DynService));
        Self {
            name: Arc::from(name.into()),
            instance_factory: erased,
            service_type: TypeId::of::<S>(),
            service_type_name: std::any::type_name::<S>(),
            ty,
            instantiation_mode: InstantiationMode::Lazy,
            multiple_instances: false,
            service_props: Map::new(),
            on_instance_created: None,
        }
    }

    /// The Rust type this component's service is, and its name for diagnostics.
    pub fn service_type(&self) -> (TypeId, &'static str) {
        (self.service_type, self.service_type_name)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn component_type(&self) -> ComponentType {
        self.ty
    }

    pub fn instantiation_mode(&self) -> InstantiationMode {
        self.instantiation_mode
    }

    pub fn multiple_instances(&self) -> bool {
        self.multiple_instances
    }

    pub fn service_props(&self) -> &Map<String, Value> {
        &self.service_props
    }

    pub fn on_instance_created(&self) -> Option<&OnInstanceCreatedCallback> {
        self.on_instance_created.as_ref()
    }

    pub fn with_instantiation_mode(mut self, mode: InstantiationMode) -> Self {
        self.instantiation_mode = mode;
        self
    }

    pub fn with_multiple_instances(mut self, multiple: bool) -> Self {
        self.multiple_instances = multiple;
        self
    }

    pub fn with_service_props(mut self, props: HashMap<String, Value>) -> Self {
        self.service_props = props.into_iter().collect();
        self
    }

    pub fn with_instance_created_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(&crate::component::container::ComponentContainer, &str, &crate::component::types::DynService)
            + Send
            + Sync
            + 'static,
    {
        self.on_instance_created = Some(Arc::new(callback));
        self
    }
}
