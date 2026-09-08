#[cfg(test)]
mod tests {
    use crate::component::types::{DynService, InstanceFactory};
    use crate::component::{Component, ComponentContainer, ComponentError, ComponentType, InstantiationMode};
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn build_component(name: &str) -> Component {
        let factory: InstanceFactory = Arc::new(|_container, _options| Ok(Arc::new(()) as DynService));
        Component::new(name.to_string(), factory, ComponentType::Public)
    }

    #[test]
    fn set_component_rejects_mismatched_name() {
        let container = ComponentContainer::new("test");
        let provider = container.get_provider("foo");
        let component = build_component("bar");
        assert!(matches!(
            provider.set_component(component),
            Err(ComponentError::MismatchingComponent { .. })
        ));
    }

    #[test]
    fn eager_component_initializes_immediately() {
        let container = ComponentContainer::new("test");
        let provider = container.get_provider("foo");
        let factory: InstanceFactory = Arc::new(|_container, _options| Ok(Arc::new(42u32) as DynService));
        let component =
            Component::new("foo", factory, ComponentType::Public).with_instantiation_mode(InstantiationMode::Eager);
        provider.set_component(component).unwrap();
        let value = provider.get_immediate::<u32>();
        assert_eq!(value.map(|arc| *arc), Some(42));
    }

    #[test]
    fn initialize_with_options_stores_options() {
        let container = ComponentContainer::new("test");
        let provider = container.get_provider("foo");
        let factory: InstanceFactory = Arc::new(|_container, options| Ok(Arc::new(options.options) as DynService));
        let component =
            Component::new("foo", factory, ComponentType::Public).with_instantiation_mode(InstantiationMode::Explicit);
        provider.set_component(component).unwrap();
        let options = json!({"value": true});
        let result = provider.initialize::<Value>(options.clone(), None).unwrap();
        assert_eq!(*result, options);
    }
    /// Two threads asking the same container for a provider must get the same one.
    ///
    /// This is the shape of what happens at startup: `initialize_app` fills a container while
    /// `register_component` propagates a newly registered component into the very same container.
    /// When the two race, a provider that already holds a component must not be replaced by an
    /// empty one, or every later lookup reports the service as unavailable.
    #[test]
    fn concurrent_lookups_never_discard_a_configured_provider() {
        use std::sync::Barrier;

        for _ in 0..500 {
            let container = ComponentContainer::new("race");
            let barrier = Arc::new(Barrier::new(2));

            let adder = {
                let container = container.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let _ = container.add_component(build_component("thing"));
                })
            };
            let looker = {
                let container = container.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    container.get_provider("thing");
                })
            };

            adder.join().unwrap();
            looker.join().unwrap();

            assert!(
                container.get_provider("thing").is_component_set(),
                "the component was lost when a concurrent lookup replaced its provider"
            );
        }
    }
}
