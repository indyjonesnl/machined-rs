//! Network controllers: config -> desired specs -> applied kernel state.

// TEMPORARY (Task 1 interim): helpers below are unused until Tasks 2-3 add the
// spec controllers. Removed in Task 2 once those controllers use the helpers.
#![allow(dead_code)]

// pub mod address;
pub mod config_controller;
// pub mod hostname;
// pub mod link;
// pub mod resolver;
// pub mod route;

// pub use address::AddressController;
pub use config_controller::NetworkConfigController;
// pub use hostname::HostnameController;
// pub use link::LinkController;
// pub use resolver::ResolverController;
// pub use route::RouteController;

use std::fmt::Display;

use machined_resources::{Key, Resource, ResourceObject, ResourceType};
use machined_runtime_core::{Error, State};

/// Namespace all network resources live in.
pub const NS: &str = "network";

/// Map any backend error into a runtime-core controller error.
pub(crate) fn ctl<E: Display>(e: E) -> Error {
    Error::Controller(e.to_string())
}

/// Create-or-update a status resource (`spec`) at `(NS, id)`.
pub(crate) fn publish_status(state: &State, id: &str, spec: Resource) {
    let key = Key::new(NS, spec.resource_type(), id);
    match state.get(&key) {
        Ok(existing) => {
            let _ = state.update(&key, existing.metadata.version, spec);
        }
        Err(_) => {
            let _ = state.create(ResourceObject::new(NS, id, spec));
        }
    }
}

/// Destroy a status resource at `(NS, typ, id)` if present.
pub(crate) fn destroy_status(state: &State, typ: ResourceType, id: &str) {
    let key = Key::new(NS, typ, id);
    if let Ok(obj) = state.get(&key) {
        let _ = state.destroy(&key, obj.metadata.version);
    }
}
