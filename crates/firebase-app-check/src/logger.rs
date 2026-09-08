use std::sync::LazyLock;

use firebase_core::logger::Logger;

pub static LOGGER: LazyLock<Logger> = LazyLock::new(|| Logger::new("@firebase/app-check"));
