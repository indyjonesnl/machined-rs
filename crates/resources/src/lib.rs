//! Pure resource data model for machined-rs: metadata and the closed
//! `Resource` enum. No I/O, no async.

pub mod block;
pub mod metadata;
pub mod network;
pub mod resource;

pub use block::{DiscoveredVolume, DiskStatus};
pub use metadata::{Key, Metadata, Phase, ResourceType};
pub use network::{
    AddrCidr, AddrCidrParseError, AddressSpec, AddressStatus, HostnameSpec, LinkSpec, LinkStatus,
    ResolverSpec, RouteSpec, RouteStatus,
};
pub use resource::{MachineConfigSpec, Resource, ResourceObject, ServiceState, ServiceStatusSpec};
