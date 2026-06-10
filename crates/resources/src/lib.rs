//! Pure resource data model for machined-rs: metadata and the closed
//! `Resource` enum. No I/O, no async.

pub mod metadata;
pub mod resource;

pub use metadata::{Key, Metadata, Phase, ResourceType};
pub use resource::{MachineConfigSpec, Resource, ResourceObject, ServiceState, ServiceStatusSpec};
