use crate::app::types::{PlatformLoggerService, VersionService};
use crate::component::types::ComponentType;
use crate::component::ComponentContainer;

/// Built eagerly with the app: it reports every registered library's version, and does so by
/// walking the container, so it has to exist before anyone asks.
impl crate::component::Service for PlatformLoggerServiceImpl {
    const NAME: &'static str = "platform-logger";
    const INSTANTIATION_MODE: crate::component::InstantiationMode = crate::component::InstantiationMode::Eager;
    const COMPONENT_TYPE: crate::component::ComponentType = crate::component::ComponentType::Private;
}

pub struct PlatformLoggerServiceImpl {
    container: ComponentContainer,
}

impl PlatformLoggerServiceImpl {
    /// Creates the platform logger service using the component container from an app.
    pub fn new(container: ComponentContainer) -> Self {
        Self { container }
    }
}

impl PlatformLoggerService for PlatformLoggerServiceImpl {
    fn platform_info_string(&self) -> String {
        let providers = self.container.get_providers();
        let mut entries = Vec::new();
        for provider in providers {
            if provider.component_type() == Some(ComponentType::Version) {
                if let Some(service) = provider.get_immediate::<VersionService>() {
                    entries.push(format!("{}/{}", service.library, service.version));
                }
            }
        }
        entries.join(" ")
    }
}
