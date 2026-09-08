#[cfg(test)]
mod tests {
    use crate::component::{Component, ComponentContainer, ComponentError, ComponentType, InstantiationMode, Service};
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn build_component(name: &str) -> Component {
        Component::typed::<(), _>(name.to_string(), |_container, _options| Ok(Arc::new(())), ComponentType::Public)
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
        let component =
            Component::typed::<u32, _>("foo", |_container, _options| Ok(Arc::new(42u32)), ComponentType::Public)
                .with_instantiation_mode(InstantiationMode::Eager);
        provider.set_component(component).unwrap();
        let value = provider.get_immediate::<u32>();
        assert_eq!(value.map(|arc| *arc), Some(42));
    }

    #[test]
    fn initialize_with_options_stores_options() {
        let container = ComponentContainer::new("test");
        let provider = container.get_provider("foo");
        let component = Component::typed::<Value, _>(
            "foo",
            |_container, options| Ok(Arc::new(options.options)),
            ComponentType::Public,
        )
        .with_instantiation_mode(InstantiationMode::Explicit);
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

    struct Widgets(u32);

    impl Service for Widgets {
        const NAME: &'static str = "widgets";
    }

    /// A service that names the same component as `Widgets` but holds something else: the mistake
    /// the typed layer exists to catch.
    #[derive(Debug)]
    struct Gadgets;

    impl Service for Gadgets {
        const NAME: &'static str = "widgets";
    }

    fn widget_container() -> ComponentContainer {
        let container = ComponentContainer::new("typed");
        container
            .add_component(Component::for_service::<Widgets, _>(|_, _| Ok(Arc::new(Widgets(7)))))
            .expect("component");
        container
    }

    #[test]
    fn a_service_is_looked_up_by_its_type() {
        let container = widget_container();

        assert_eq!(container.get::<Widgets>().map(|widgets| widgets.0), Some(7));
        assert!(container.service::<Widgets>().is_registered());
        assert!(container.service::<Widgets>().is_initialized(None));
    }

    #[test]
    fn an_unregistered_service_is_absent_rather_than_an_error() {
        let container = ComponentContainer::new("typed");

        assert!(container.get::<Widgets>().is_none());
        assert!(!container.service::<Widgets>().is_registered());
        assert!(container
            .service::<Widgets>()
            .try_get(None)
            .expect("no error")
            .is_none());
    }

    #[test]
    fn looking_a_service_up_as_the_wrong_type_is_an_error() {
        let container = widget_container();

        // The untyped container would have answered `None` here, which reads as "the product is
        // not installed" and hides the bug.
        let error = container
            .service::<Gadgets>()
            .try_get(None)
            .expect_err("a type mismatch must be reported");

        assert!(matches!(error, ComponentError::MismatchingServiceType { .. }));
        assert!(container.get::<Gadgets>().is_none());
    }
}
